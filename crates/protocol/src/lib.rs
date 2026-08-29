//! Versioned IPC contract for the daemon-backed Anclave rewrite.
//!
//! This crate deliberately contains data and validation only. It must remain
//! usable by the daemon, CLI, and TUI without depending on any runtime or
//! platform-specific implementation.

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub mod ipc;

/// The first wire protocol version.
pub const PROTOCOL_VERSION: u16 = 1;

/// Maximum encoded protocol frame accepted by clients and the daemon.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, ProtocolError> {
                let value = value.into();
                if value.is_empty() || value.len() > 128 {
                    return Err(ProtocolError::InvalidIdentifier);
                }
                if value.chars().any(|c| c.is_control()) {
                    return Err(ProtocolError::InvalidIdentifier);
                }
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

id_type!(SessionId);
id_type!(AgentId);
id_type!(BackendId);
id_type!(WorkspaceId);
id_type!(SandboxId);
id_type!(RequestId);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Size {
    pub columns: u16,
    pub rows: u16,
}

impl Size {
    pub fn validate(self) -> Result<Self, ProtocolError> {
        if self.columns == 0 || self.rows == 0 {
            return Err(ProtocolError::InvalidSize);
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateSession {
    pub name: String,
    pub agent: AgentId,
    pub backend: BackendId,
    pub workspace: Option<WorkspaceId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    Ping,
    GetVersion,
    ListSessions,
    GetSession { id: SessionId },
    CreateSession(CreateSession),
    DeleteSession { id: SessionId },
    RestartSession { id: SessionId },
    AttachSession { id: SessionId },
    DetachSession { id: SessionId },
    SendInput { id: SessionId, bytes: Vec<u8> },
    ResizeSession { id: SessionId, size: Size },
    CaptureScreen { id: SessionId },
    SubscribeEvents,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    Pong,
    Version { protocol: u16, version: String },
    Sessions(Vec<SessionSummary>),
    Session(SessionSummary),
    Accepted,
    Screen(ScreenSnapshot),
    Subscribed,
    Error { code: ErrorCode, message: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: SessionId,
    pub name: String,
    pub state: SessionState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionState {
    Creating,
    Starting,
    Running,
    Detached,
    Unreachable,
    Exited,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenSnapshot {
    pub size: Size,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    SessionCreated { session: SessionSummary },
    SessionStateChanged { session: SessionSummary },
    OutputChanged { id: SessionId },
    ScreenChanged { id: SessionId },
    SessionExited { id: SessionId, code: Option<i32> },
    BackendError { backend: BackendId, message: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope<T> {
    pub protocol: u16,
    pub request_id: Option<RequestId>,
    pub payload: T,
}

impl<T> Envelope<T> {
    pub fn new(request_id: Option<RequestId>, payload: T) -> Self {
        Self {
            protocol: PROTOCOL_VERSION,
            request_id,
            payload,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    InvalidRequest,
    InvalidIdentifier,
    InvalidSize,
    UnsupportedProtocol,
    NotFound,
    PermissionDenied,
    BackendFailure,
    Internal,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProtocolError {
    #[error("invalid identifier")]
    InvalidIdentifier,
    #[error("invalid terminal size")]
    InvalidSize,
    #[error("unsupported protocol version")]
    UnsupportedProtocol,
    #[error("frame exceeds the maximum size")]
    FrameTooLarge,
    #[error("invalid JSON payload: {0}")]
    InvalidJson(String),
}

pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, ProtocolError> {
    let bytes =
        serde_json::to_vec(value).map_err(|error| ProtocolError::InvalidJson(error.to_string()))?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge);
    }
    Ok(bytes)
}

pub fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T, ProtocolError> {
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge);
    }
    serde_json::from_slice(bytes).map_err(|error| ProtocolError::InvalidJson(error.to_string()))
}

pub fn validate_protocol(version: u16) -> Result<(), ProtocolError> {
    if version != PROTOCOL_VERSION {
        return Err(ProtocolError::UnsupportedProtocol);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id<T>(value: &str, make: impl FnOnce(String) -> Result<T, ProtocolError>) -> T {
        make(value.to_owned()).expect("valid identifier")
    }

    #[test]
    fn identifiers_round_trip() {
        let session = id("session-1", SessionId::new);
        let bytes = encode(&session).unwrap();
        assert_eq!(decode::<SessionId>(&bytes).unwrap(), session);
        assert_eq!(session.as_str(), "session-1");
    }

    #[test]
    fn identifiers_reject_empty_and_control_values() {
        assert_eq!(SessionId::new(""), Err(ProtocolError::InvalidIdentifier));
        assert_eq!(
            SessionId::new("bad\nvalue"),
            Err(ProtocolError::InvalidIdentifier)
        );
    }

    #[test]
    fn envelope_round_trips_requests() {
        let request = Envelope::new(Some(RequestId::new("request-1").unwrap()), Request::Ping);
        let bytes = encode(&request).unwrap();
        let decoded: Envelope<Request> = decode(&bytes).unwrap();
        assert_eq!(decoded, request);
        validate_protocol(decoded.protocol).unwrap();
    }

    #[test]
    fn invalid_protocol_versions_are_rejected() {
        assert_eq!(
            validate_protocol(PROTOCOL_VERSION + 1),
            Err(ProtocolError::UnsupportedProtocol)
        );
    }

    #[test]
    fn terminal_sizes_must_be_nonzero() {
        assert_eq!(
            Size {
                columns: 0,
                rows: 24
            }
            .validate(),
            Err(ProtocolError::InvalidSize)
        );
    }

    #[test]
    fn frames_have_a_size_limit() {
        let payload = "x".repeat(MAX_FRAME_BYTES);
        assert_eq!(encode(&payload), Err(ProtocolError::FrameTooLarge));
    }
}
