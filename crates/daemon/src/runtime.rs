use std::sync::{Arc, Mutex};

use anclave_protocol::{
    CreateSession, Envelope, ErrorCode, Request, Response, SessionState, SessionSummary,
};

use crate::backend::{BackendError, CreateRequest, SharedBackend};
use crate::storage::Storage;

#[derive(Clone)]
pub struct Runtime {
    storage: Arc<Mutex<Storage>>,
    backend: SharedBackend,
}

impl Runtime {
    pub fn new(storage: Arc<Mutex<Storage>>, backend: SharedBackend) -> Self {
        Self { storage, backend }
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
            Request::DeleteSession { id } => self.delete(&id),
            Request::ResizeSession { id, size } => match self.backend.resize(&id, size) {
                Ok(()) => Response::Accepted,
                Err(error) => backend_error(error),
            },
            _ => response_error(ErrorCode::InvalidRequest, "request is not implemented yet"),
        }
    }

    fn create(&self, request: CreateSession) -> Response {
        if request.name.trim().is_empty() {
            return response_error(ErrorCode::InvalidRequest, "session name cannot be empty");
        }

        let storage = self.storage.lock().expect("storage mutex is not poisoned");
        let id = match storage.next_session_id() {
            Ok(id) => id,
            Err(error) => return storage_error(error),
        };
        let summary = SessionSummary {
            id: id.clone(),
            name: request.name,
            state: SessionState::Starting,
        };
        let backend_request = CreateRequest {
            session_id: id,
            name: summary.name.clone(),
            size: anclave_protocol::Size {
                columns: 80,
                rows: 24,
            },
        };
        if let Err(error) = self.backend.create(backend_request) {
            return backend_error(error);
        }
        match storage.insert_session(&summary) {
            Ok(()) => Response::Session(summary),
            Err(storage_failure) => {
                let _ = self.backend.kill(&summary.id);
                if is_constraint_error(&storage_failure) {
                    response_error(ErrorCode::InvalidRequest, "session name already exists")
                } else {
                    storage_error(storage_failure)
                }
            }
        }
    }

    fn delete(&self, id: &anclave_protocol::SessionId) -> Response {
        let deleted = self
            .storage
            .lock()
            .expect("storage mutex is not poisoned")
            .delete_session(id);
        match deleted {
            Ok(true) => match self.backend.kill(id) {
                Ok(()) | Err(BackendError::NotFound) => Response::Accepted,
                Err(error) => backend_error(error),
            },
            Ok(false) => response_error(ErrorCode::NotFound, "session not found"),
            Err(error) => storage_error(error),
        }
    }
}

fn is_constraint_error(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(inner, _)
            if inner.code == rusqlite::ErrorCode::ConstraintViolation
    )
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
        BackendError::Failed(message) => response_error(ErrorCode::BackendFailure, &message),
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

    fn create(name: &str) -> Request {
        Request::CreateSession(CreateSession {
            name: name.to_owned(),
            agent: AgentId::new("mock").unwrap(),
            backend: BackendId::new("local").unwrap(),
            workspace: None,
        })
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
        assert_eq!(session.state, SessionState::Starting);
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
                    rows: 30,
                },
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
    fn envelope_preserves_request_id() {
        let runtime = runtime();
        let request = Envelope::new(Some(RequestId::new("req-1").unwrap()), Request::Ping);
        let response = handle_envelope(&runtime, request);
        assert_eq!(response.request_id.unwrap().as_str(), "req-1");
        assert_eq!(response.payload, Response::Pong);
    }
}
