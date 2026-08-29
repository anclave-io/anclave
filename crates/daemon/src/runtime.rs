use std::collections::BTreeMap;

use anclave_protocol::{
    CreateSession, Envelope, ErrorCode, Request, Response, SessionId, SessionState, SessionSummary,
};

#[derive(Debug, Default)]
pub struct Runtime {
    sessions: BTreeMap<String, SessionSummary>,
}

impl Runtime {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn handle(&mut self, request: Request) -> Response {
        match request {
            Request::Ping => Response::Pong,
            Request::GetVersion => Response::Version {
                protocol: anclave_protocol::PROTOCOL_VERSION,
                version: env!("CARGO_PKG_VERSION").to_owned(),
            },
            Request::ListSessions => Response::Sessions(self.sessions.values().cloned().collect()),
            Request::GetSession { id } => self
                .sessions
                .get(id.as_str())
                .cloned()
                .map(Response::Session)
                .unwrap_or_else(|| error(ErrorCode::NotFound, "session not found")),
            Request::CreateSession(request) => self.create(request),
            Request::DeleteSession { id } => {
                if self.sessions.remove(id.as_str()).is_some() {
                    Response::Accepted
                } else {
                    error(ErrorCode::NotFound, "session not found")
                }
            }
            _ => error(ErrorCode::InvalidRequest, "request is not implemented yet"),
        }
    }

    fn create(&mut self, request: CreateSession) -> Response {
        if request.name.trim().is_empty() {
            return error(ErrorCode::InvalidRequest, "session name cannot be empty");
        }
        if self
            .sessions
            .values()
            .any(|session| session.name == request.name)
        {
            return error(ErrorCode::InvalidRequest, "session name already exists");
        }

        let id = SessionId::new(format!("session-{}", self.sessions.len() + 1))
            .expect("generated session ID is valid");
        let summary = SessionSummary {
            id: id.clone(),
            name: request.name,
            state: SessionState::Creating,
        };
        self.sessions.insert(id.to_string(), summary.clone());
        Response::Session(summary)
    }
}

fn error(code: ErrorCode, message: &str) -> Response {
    Response::Error {
        code,
        message: message.to_owned(),
    }
}

pub fn handle_envelope(runtime: &mut Runtime, request: Envelope<Request>) -> Envelope<Response> {
    Envelope::new(request.request_id, runtime.handle(request.payload))
}

#[cfg(test)]
mod tests {
    use super::*;
    use anclave_protocol::{AgentId, BackendId, RequestId};

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
        let mut runtime = Runtime::new();
        assert_eq!(
            runtime.handle(Request::ListSessions),
            Response::Sessions(vec![])
        );
    }

    #[test]
    fn create_list_get_and_delete_session() {
        let mut runtime = Runtime::new();
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
        let mut runtime = Runtime::new();
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
        let mut runtime = Runtime::new();
        let request = Envelope::new(Some(RequestId::new("req-1").unwrap()), Request::Ping);
        let response = handle_envelope(&mut runtime, request);
        assert_eq!(response.request_id.unwrap().as_str(), "req-1");
        assert_eq!(response.payload, Response::Pong);
    }
}
