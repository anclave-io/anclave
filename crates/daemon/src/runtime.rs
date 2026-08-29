use std::sync::{Arc, Mutex};

use anclave_protocol::{
    CreateSession, Envelope, ErrorCode, Event, Request, Response, SessionState, SessionSummary,
};

use crate::backend::{BackendError, CreateRequest, SharedBackend};
use crate::events::EventBus;
use crate::storage::Storage;
use crate::terminal::{TerminalError, TerminalStore, DEFAULT_SIZE};

#[derive(Clone)]
pub struct Runtime {
    storage: Arc<Mutex<Storage>>,
    backend: SharedBackend,
    terminals: TerminalStore,
    events: EventBus,
    agents: Arc<crate::agent::AgentRegistry>,
}

impl Runtime {
    pub fn new(storage: Arc<Mutex<Storage>>, backend: SharedBackend) -> Self {
        Self {
            storage,
            backend,
            terminals: TerminalStore::new(),
            events: EventBus::new(),
            agents: Arc::new(crate::agent::AgentRegistry::builtins()),
        }
    }

    pub fn set_agents(&mut self, agents: crate::agent::AgentRegistry) {
        self.agents = Arc::new(agents);
    }

    pub fn events(&self) -> EventBus {
        self.events.clone()
    }

    pub fn recover_sessions(&self) {
        let sessions = self
            .storage
            .lock()
            .expect("storage mutex is not poisoned")
            .list_sessions()
            .unwrap_or_default();
        for session in sessions {
            let state = match self.backend.adopt(&session.id) {
                Ok(_) => {
                    let _ = self.terminals.insert(&session.id, DEFAULT_SIZE);
                    SessionState::Running
                }
                Err(BackendError::NotFound) => SessionState::Exited,
                Err(_) => SessionState::Unreachable,
            };
            let _ = self
                .storage
                .lock()
                .expect("storage mutex is not poisoned")
                .set_session_state(&session.id, state);
        }
    }

    pub fn poll_backend(&self) {
        let Ok(ids) = self.backend.sessions() else {
            return;
        };
        for id in ids {
            let Ok(output) = self.backend.capture(&id) else {
                continue;
            };
            if output.is_empty() || self.terminals.write_output(&id, output.as_bytes()).is_err() {
                continue;
            }
            self.events.publish_screen_changed(id);
        }
    }

    pub fn handle(&self, request: Request) -> Response {
        match request {
            Request::Ping => Response::Pong,
            Request::GetVersion => Response::Version {
                protocol: anclave_protocol::PROTOCOL_VERSION,
                version: env!("CARGO_PKG_VERSION").to_owned(),
            },
            Request::ListSessions => self
                .storage
                .lock()
                .expect("storage mutex is not poisoned")
                .list_sessions()
                .map(Response::Sessions)
                .unwrap_or_else(storage_error),
            Request::GetSession { id } => {
                match self
                    .storage
                    .lock()
                    .expect("storage mutex is not poisoned")
                    .get_session(&id)
                {
                    Ok(Some(session)) => Response::Session(session),
                    Ok(None) => response_error(ErrorCode::NotFound, "session not found"),
                    Err(error) => storage_error(error),
                }
            }
            Request::CreateSession(request) => self.create(request),
            Request::RestartSession { id } => self.restart(&id),
            Request::DeleteSession { id } => self.delete(&id),
            Request::SubscribeEvents => Response::Subscribed,
            Request::Shutdown => Response::Accepted,
            Request::CaptureScreen { id } => match self.capture(&id) {
                Ok(screen) => Response::Screen(screen),
                Err(error) => terminal_error(error),
            },
            Request::SendInput { id, bytes } => match self.backend.send_input(&id, &bytes) {
                Ok(()) => Response::Accepted,
                Err(error) => backend_error(error),
            },
            Request::ResizeSession { id, size } => match self.backend.resize(&id, size) {
                Ok(()) => match self.terminals.resize(&id, size) {
                    Ok(()) => Response::Accepted,
                    Err(error) => terminal_error(error),
                },
                Err(error) => backend_error(error),
            },
            _ => response_error(ErrorCode::InvalidRequest, "request is not implemented yet"),
        }
    }

    fn capture(
        &self,
        id: &anclave_protocol::SessionId,
    ) -> Result<anclave_protocol::ScreenSnapshot, TerminalError> {
        let backend_output = self.backend.capture(id).map_err(|error| match error {
            BackendError::NotFound => TerminalError::NotFound,
            _ => TerminalError::NotFound,
        })?;
        if !backend_output.is_empty() {
            self.terminals.write_output(id, backend_output.as_bytes())?;
            self.events.publish_screen_changed(id.clone());
        }
        self.terminals.capture(id)
    }

    fn create(&self, request: CreateSession) -> Response {
        if request.name.trim().is_empty() {
            return response_error(ErrorCode::InvalidRequest, "session name cannot be empty");
        }

        let id = match self
            .storage
            .lock()
            .expect("storage mutex is not poisoned")
            .next_session_id()
        {
            Ok(id) => id,
            Err(error) => return storage_error(error),
        };
        let Some(agent) = self.agents.get(&request.agent) else {
            return response_error(
                ErrorCode::UnknownAgent,
                &format!("unknown agent: {}", request.agent),
            );
        };

        let mut summary = SessionSummary {
            id: id.clone(),
            name: request.name,
            state: SessionState::Creating,
            agent: request.agent,
            workspace: request.workspace,
        };

        if let Err(error) = self
            .storage
            .lock()
            .expect("storage mutex is not poisoned")
            .insert_session(&summary)
        {
            return if is_constraint_error(&error) {
                response_error(ErrorCode::InvalidRequest, "session name already exists")
            } else {
                storage_error(error)
            };
        }
        self.events.publish(Event::SessionCreated {
            session: summary.clone(),
        });

        summary.state = SessionState::Starting;
        if let Err(error) = self
            .storage
            .lock()
            .expect("storage mutex is not poisoned")
            .update_session(&summary)
        {
            return self.rollback_create(&summary.id, storage_error(error));
        }
        self.events.publish(Event::SessionStateChanged {
            session: summary.clone(),
        });

        let backend_request = CreateRequest {
            session_id: summary.id.clone(),
            name: summary.name.clone(),
            size: DEFAULT_SIZE,
            launch: agent.launch(&summary.id),
        };
        if let Err(error) = self.backend.create(backend_request) {
            return self.rollback_create(&summary.id, backend_error(error));
        }
        if let Err(error) = self.terminals.insert(&summary.id, DEFAULT_SIZE) {
            let _ = self.backend.kill(&summary.id);
            return self.rollback_create(&summary.id, terminal_error(error));
        }

        summary.state = SessionState::Running;
        if let Err(error) = self
            .storage
            .lock()
            .expect("storage mutex is not poisoned")
            .update_session(&summary)
        {
            let response = storage_error(error);
            let _ = self.backend.kill(&summary.id);
            self.terminals.remove(&summary.id);
            return self.rollback_create(&summary.id, response);
        }
        self.events.publish(Event::SessionStateChanged {
            session: summary.clone(),
        });
        Response::Session(summary)
    }

    fn rollback_create(&self, id: &anclave_protocol::SessionId, response: Response) -> Response {
        self.terminals.remove(id);
        let _ = self
            .storage
            .lock()
            .expect("storage mutex is not poisoned")
            .remove_session(id);
        response
    }

    fn restart(&self, id: &anclave_protocol::SessionId) -> Response {
        let Some(existing) = self
            .storage
            .lock()
            .expect("storage mutex is not poisoned")
            .get_session(id)
            .ok()
            .flatten()
        else {
            return response_error(ErrorCode::NotFound, "session not found");
        };

        let Some(agent) = self.agents.get(&existing.agent) else {
            return response_error(
                ErrorCode::UnknownAgent,
                &format!("configured agent is unavailable: {}", existing.agent),
            );
        };
        let request = CreateRequest {
            session_id: id.clone(),
            name: existing.name.clone(),
            size: DEFAULT_SIZE,
            launch: agent.launch(id),
        };
        if let Err(error) = self.backend.restart(request) {
            return backend_error(error);
        }
        let _ = self
            .storage
            .lock()
            .expect("storage mutex is not poisoned")
            .set_session_state(id, SessionState::Running);
        let _ = self.terminals.insert(id, DEFAULT_SIZE);
        Response::Session(SessionSummary {
            state: SessionState::Running,
            ..existing
        })
    }

    fn delete(&self, id: &anclave_protocol::SessionId) -> Response {
        let deleted = self
            .storage
            .lock()
            .expect("storage mutex is not poisoned")
            .delete_session(id);
        match deleted {
            Ok(true) => {
                self.terminals.remove(id);
                match self.backend.kill(id) {
                    Ok(()) | Err(BackendError::NotFound) => Response::Accepted,
                    Err(error) => backend_error(error),
                }
            }
            Ok(false) => response_error(ErrorCode::NotFound, "session not found"),
            Err(error) => storage_error(error),
        }
    }
}

fn is_constraint_error(error: &rusqlite::Error) -> bool {
    matches!(error, rusqlite::Error::SqliteFailure(inner, _) if inner.code == rusqlite::ErrorCode::ConstraintViolation)
}

fn backend_error(error: BackendError) -> Response {
    match error {
        BackendError::NotFound => response_error(ErrorCode::NotFound, "backend session not found"),
        BackendError::AlreadyExists => {
            response_error(ErrorCode::BackendFailure, "backend session already exists")
        }
        BackendError::InvalidSize => {
            response_error(ErrorCode::InvalidSize, "invalid terminal size")
        }
        BackendError::InputTooLarge => {
            response_error(ErrorCode::InvalidRequest, "input exceeds the maximum size")
        }
        BackendError::Failed(message) => response_error(ErrorCode::BackendFailure, &message),
    }
}

fn terminal_error(error: TerminalError) -> Response {
    match error {
        TerminalError::InvalidSize => {
            response_error(ErrorCode::InvalidSize, "invalid terminal size")
        }
        TerminalError::NotFound => {
            response_error(ErrorCode::NotFound, "terminal session not found")
        }
    }
}

fn storage_error(storage: rusqlite::Error) -> Response {
    response_error(ErrorCode::Internal, &format!("storage error: {storage}"))
}

fn response_error(code: ErrorCode, message: &str) -> Response {
    Response::Error {
        code,
        message: message.to_owned(),
    }
}

pub fn handle_envelope(runtime: &Runtime, request: Envelope<Request>) -> Envelope<Response> {
    Envelope::new(request.request_id, runtime.handle(request.payload))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::FakeBackend;
    use anclave_protocol::{AgentId, BackendId, RequestId};

    fn runtime() -> Runtime {
        Runtime::new(
            Arc::new(Mutex::new(Storage::open_in_memory().unwrap())),
            Arc::new(FakeBackend::new()),
        )
    }

    fn custom_runtime(backend: Arc<FakeBackend>) -> Runtime {
        let mut runtime = Runtime::new(
            Arc::new(Mutex::new(Storage::open_in_memory().unwrap())),
            backend,
        );
        let path =
            std::env::temp_dir().join(format!("anclave-runtime-agent-{}.toml", std::process::id()));
        std::fs::write(
            &path,
            "[[agents]]\nname = 'mock'\ncommand = 'mock-agent'\nargs = ['--session', '{id}', '--mode', 'test']\n",
        )
        .unwrap();
        runtime.set_agents(crate::agent::AgentRegistry::load(&path).unwrap());
        let _ = std::fs::remove_file(path);
        runtime
    }

    fn create(name: &str) -> Request {
        Request::CreateSession(CreateSession {
            name: name.to_owned(),
            agent: AgentId::new("default").unwrap(),
            backend: BackendId::new("local").unwrap(),
            workspace: None,
        })
    }

    #[test]
    fn unknown_agents_are_rejected_before_persisting_a_session() {
        let runtime = runtime();
        let response = runtime.handle(Request::CreateSession(CreateSession {
            name: "demo".to_owned(),
            agent: AgentId::new("missing").unwrap(),
            backend: BackendId::new("local").unwrap(),
            workspace: None,
        }));
        assert!(matches!(
            response,
            Response::Error {
                code: ErrorCode::UnknownAgent,
                ..
            }
        ));
        assert_eq!(
            runtime.handle(Request::ListSessions),
            Response::Sessions(vec![])
        );
    }

    #[test]
    fn custom_agent_launch_spec_reaches_the_backend() {
        let backend = Arc::new(FakeBackend::new());
        let runtime = custom_runtime(backend.clone());
        let response = runtime.handle(Request::CreateSession(CreateSession {
            name: "custom".to_owned(),
            agent: AgentId::new("mock").unwrap(),
            backend: BackendId::new("local").unwrap(),
            workspace: None,
        }));
        let Response::Session(session) = response else {
            panic!("expected created session")
        };
        let launches = backend.launches();
        assert_eq!(launches.len(), 1);
        assert_eq!(launches[0].session_id, session.id);
        assert_eq!(launches[0].launch.program, "mock-agent");
        assert_eq!(
            launches[0].launch.args,
            vec!["--session", "session-0", "--mode", "test"]
        );
    }

    #[test]
    fn runtime_starts_empty() {
        let runtime = runtime();
        assert_eq!(
            runtime.handle(Request::ListSessions),
            Response::Sessions(vec![])
        );
    }

    #[test]
    fn create_list_get_resize_and_delete_session() {
        let runtime = runtime();
        let Response::Session(session) = runtime.handle(create("demo")) else {
            panic!("expected created session")
        };
        assert_eq!(session.state, SessionState::Running);
        assert!(matches!(
            runtime.handle(Request::GetSession {
                id: session.id.clone()
            }),
            Response::Session(_)
        ));
        assert!(matches!(
            runtime.handle(Request::ResizeSession {
                id: session.id.clone(),
                size: anclave_protocol::Size {
                    columns: 100,
                    rows: 30
                }
            }),
            Response::Accepted
        ));
        assert!(matches!(
            runtime.handle(Request::DeleteSession { id: session.id }),
            Response::Accepted
        ));
        assert_eq!(
            runtime.handle(Request::ListSessions),
            Response::Sessions(vec![])
        );
    }

    #[test]
    fn duplicate_names_are_rejected() {
        let runtime = runtime();
        assert!(matches!(
            runtime.handle(create("demo")),
            Response::Session(_)
        ));
        assert!(matches!(
            runtime.handle(create("demo")),
            Response::Error { .. }
        ));
    }

    #[test]
    fn recovery_adopts_existing_backend_sessions() {
        let backend = Arc::new(FakeBackend::new());
        let storage = Arc::new(Mutex::new(Storage::open_in_memory().unwrap()));
        let first = Runtime::new(storage.clone(), backend.clone());
        let Response::Session(session) = first.handle(create("demo")) else {
            panic!("expected created session")
        };
        let recovered = Runtime::new(storage, backend);
        recovered.recover_sessions();
        assert!(matches!(
            recovered.handle(Request::CaptureScreen {
                id: session.id.clone()
            }),
            Response::Screen(_)
        ));
        let Response::Session(recovered_session) =
            recovered.handle(Request::GetSession { id: session.id })
        else {
            panic!("expected recovered session")
        };
        assert_eq!(recovered_session.state, SessionState::Running);
    }

    #[test]
    fn recovery_marks_missing_backend_sessions_exited() {
        let storage = Arc::new(Mutex::new(Storage::open_in_memory().unwrap()));
        let backend = Arc::new(FakeBackend::new());
        let id = anclave_protocol::SessionId::new("session-1").unwrap();
        storage
            .lock()
            .unwrap()
            .insert_session(&SessionSummary {
                id: id.clone(),
                name: "demo".to_owned(),
                state: SessionState::Starting,
                agent: AgentId::new("default").unwrap(),
                workspace: None,
            })
            .unwrap();
        let recovered = Runtime::new(storage, backend);
        recovered.recover_sessions();
        let Response::Session(recovered_session) = recovered.handle(Request::GetSession { id })
        else {
            panic!("expected recovered session")
        };
        assert_eq!(recovered_session.state, SessionState::Exited);
    }

    #[test]
    fn create_publishes_lifecycle_events() {
        let runtime = runtime();
        let mut events = runtime.events().subscribe();
        let Response::Session(session) = runtime.handle(create("demo")) else {
            panic!("expected created session")
        };
        assert!(matches!(
            events.try_recv().unwrap(),
            Event::SessionCreated { .. }
        ));
        assert!(matches!(
            events.try_recv().unwrap(),
            Event::SessionStateChanged { session: value }
                if value.state == SessionState::Starting
        ));
        assert!(matches!(
            events.try_recv().unwrap(),
            Event::SessionStateChanged { session: value }
                if value.id == session.id && value.state == SessionState::Running
        ));
    }

    #[test]
    fn failed_create_rolls_back_persisted_metadata() {
        let runtime = runtime();
        let response = runtime.handle(create("demo"));
        assert!(matches!(response, Response::Session(_)));
        let response = runtime.handle(create("demo"));
        assert!(matches!(response, Response::Error { .. }));
        assert_eq!(
            runtime.handle(Request::ListSessions),
            Response::Sessions(vec![SessionSummary {
                id: anclave_protocol::SessionId::new("session-0").unwrap(),
                name: "demo".to_owned(),
                state: SessionState::Running,
                agent: AgentId::new("default").unwrap(),
                workspace: None,
            }])
        );
    }

    #[test]
    fn restart_recreates_the_backend_session() {
        let runtime = runtime();
        let Response::Session(session) = runtime.handle(create("demo")) else {
            panic!("expected created session")
        };
        assert!(matches!(
            runtime.handle(Request::RestartSession {
                id: session.id.clone()
            }),
            Response::Session(_)
        ));
    }

    #[test]
    fn restart_missing_session_returns_not_found() {
        let runtime = runtime();
        let response = runtime.handle(Request::RestartSession {
            id: anclave_protocol::SessionId::new("missing").unwrap(),
        });
        assert!(matches!(
            response,
            Response::Error {
                code: ErrorCode::NotFound,
                ..
            }
        ));
    }

    #[test]
    fn envelope_preserves_request_id() {
        let runtime = runtime();
        let request = Envelope::new(Some(RequestId::new("req-1").unwrap()), Request::Ping);
        let response = handle_envelope(&runtime, request);
        assert_eq!(response.request_id.unwrap().as_str(), "req-1");
        assert_eq!(response.payload, Response::Pong);
    }
}
