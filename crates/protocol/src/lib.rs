//! Versioned IPC contract for the daemon-backed Anclave rewrite.
use serde::{Deserialize, Serialize};
use thiserror::Error;
pub mod ipc;
pub const PROTOCOL_VERSION: u16 = 1;
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;
macro_rules! id_type {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);
        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, ProtocolError> {
                let value = value.into();
                if value.is_empty() || value.len() > 128 || value.chars().any(|c| c.is_control()) {
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
            Err(ProtocolError::InvalidSize)
        } else {
            Ok(self)
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateSession {
    pub name: String,
    pub agent: AgentId,
    pub backend: BackendId,
    pub workspace: Option<WorkspaceSpec>,
}

/// A session's workspace: one or more repositories gathered into a single
/// directory the agent runs in.
///
/// **This is workspace isolation, not agent containment.** A workspace reduces
/// merge conflicts between concurrent agents by giving each its own checkout.
/// It confers no process authority whatsoever — an agent in a workspace can
/// read and write anything the user can. Containment is a `SecurityProfile`
/// concern and is enforced somewhere else entirely.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceSpec {
    pub id: WorkspaceId,
    /// Ordered; the first member is the primary one, and a single-member
    /// workspace launches the agent directly in it rather than in a wrapper
    /// directory.
    pub members: Vec<WorkspaceMember>,
}

impl WorkspaceSpec {
    /// A one-repository workspace on its own branch — the common case.
    pub fn single(
        id: WorkspaceId,
        repository: impl Into<String>,
        branch: impl Into<String>,
    ) -> Self {
        Self {
            id,
            members: vec![WorkspaceMember {
                repository: repository.into(),
                branch: Some(branch.into()),
                base: None,
                access: MemberAccess::ReadWrite,
            }],
        }
    }

    pub fn primary(&self) -> Option<&WorkspaceMember> {
        self.members.first()
    }
}

/// One repository inside a workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceMember {
    pub repository: String,
    /// `Some` gets the member its own worktree on that branch. `None` attaches
    /// the directory as it is, sharing whatever branch it already has.
    pub branch: Option<String>,
    /// The revision a new worktree branches from. Ignored when `branch` is
    /// `None`.
    pub base: Option<String>,
    pub access: MemberAccess,
}

/// A member's intended access.
///
/// **Declared, not enforced here.** The workspace layer builds directories and
/// symlinks; it cannot stop a process from writing to one. This is carried so
/// a sandbox can mount the member accordingly, and it means nothing until a
/// `SecurityProfile` acts on it. Treating it as a control is exactly the
/// mistake this codebase is built to avoid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum MemberAccess {
    #[default]
    ReadWrite,
    ReadOnly,
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
    Shutdown,
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
    pub agent: AgentId,
    pub workspace: Option<WorkspaceSpec>,
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
    UnknownAgent,
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
    let bytes = serde_json::to_vec(value).map_err(|e| ProtocolError::InvalidJson(e.to_string()))?;
    if bytes.len() > MAX_FRAME_BYTES {
        Err(ProtocolError::FrameTooLarge)
    } else {
        Ok(bytes)
    }
}
pub fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T, ProtocolError> {
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge);
    }
    serde_json::from_slice(bytes).map_err(|e| ProtocolError::InvalidJson(e.to_string()))
}
pub fn validate_protocol(version: u16) -> Result<(), ProtocolError> {
    if version != PROTOCOL_VERSION {
        Err(ProtocolError::UnsupportedProtocol)
    } else {
        Ok(())
    }
}
