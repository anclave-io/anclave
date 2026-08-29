use std::sync::{Arc, Mutex};

use anclave_protocol::{
    CreateSession, Envelope, ErrorCode, Request, Response, SessionState, SessionSummary,
};

use crate::storage::Storage;

#[derive(Debug, Clone)]
pub struct Runtime {
    storage: Arc<Mutex<Storage>>,
}

impl Runtime {
    pub fn new(storage: Arc<Mutex<Storage>>) -> Self {
        Self { storage }
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
            Request::DeleteSession { id } => {
                match self
                    .storage
                    .lock()
                    .expect("storage mutex is not poisoned")
                    .delete_session(&id)
                {
                    Ok(true) => Response::Accepted,
                    Ok(false) => response_error(ErrorCode::NotFound, "session not found"),
                    Err(error) => storage_error(error),
                }
            }
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
            id,
            name: request.name,
            state: SessionState::Creating,
        };
        match storage.insert_session(&summary) {
            Ok(()) => Response::Session(summary),
            Err(storage) if is_constraint_error(&storage) => {
                response_error(ErrorCode::InvalidRequest, "session name already exists")
            }
            Err(storage) => storage_error(storage),
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
    use anclave_protocol::{AgentId, BackendId, RequestId};

    fn runtime() -> Runtime {
        Runtime::new(Arc::new(Mutex::new(Storage::open_in_memory().unwrap())))
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
    fn create_list_get_and_delete_session() {
        let runtime = runtime();
        let Response::Session(session) = runtime.handle(create("demo")) else {
            panic!("expected created session")
        };
        assert_eq!(session.state, SessionState::Creating);
        assert!(matches!(
            runtime.handle(Request::GetSession {
                id: session.id.clone()
            }),
            Response::Session(_)
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
