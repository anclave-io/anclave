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
    /// Name of the security profile to run under. `None` takes the
    /// configured default, which is the uncontained compatibility profile
    /// unless the operator changed it.
    #[serde(default)]
    pub security: Option<String>,
}

/// A session's workspace: one or more repositories gathered into a single
/// directory the agent runs in.
///
/// **This is workspace isolation, not agent containment.** A workspace reduces
/// merge conflicts between concurrent agents by giving each its own checkout.
/// It confers no process authority whatsoever: an agent in a workspace can
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
    /// A one-repository workspace on its own branch: the common case.
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
    GetSession {
        id: SessionId,
    },
    CreateSession(CreateSession),
    DeleteSession {
        id: SessionId,
    },
    RestartSession {
        id: SessionId,
    },
    AttachSession {
        id: SessionId,
    },
    DetachSession {
        id: SessionId,
    },
    SendInput {
        id: SessionId,
        bytes: Vec<u8>,
    },
    ResizeSession {
        id: SessionId,
        size: Size,
    },
    CaptureScreen {
        id: SessionId,
    },
    /// What containment this host can provide. Asked of the daemon rather
    /// than probed locally: the agent runs on the daemon's machine, which is
    /// not necessarily the client's.
    GetSandboxReport,
    /// Everything waiting on a decision.
    ListApprovals,
    /// Allow a pending action. The id names which one: two prompts must not
    /// be answerable by accident.
    ApproveAction {
        id: String,
    },
    DenyAction {
        id: String,
    },
    SubscribeEvents,
    /// Report what a migration from `source` would do. Reads only.
    InspectMigration {
        source: String,
    },
    /// Perform that migration. `apply = false` is a dry run and writes
    /// nothing, so the safe form is the one you get by forgetting a flag.
    ImportMigration {
        source: String,
        apply: bool,
    },
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
    Sandboxes(SandboxReport),
    Approvals(Vec<ApprovalRequest>),
    Subscribed,
    Migration(MigrationReport),
    Error { code: ErrorCode, message: String },
}

/// What a migration would do, or did.
///
/// The same shape answers `inspect`, a dry run and an applied import, so what
/// you reviewed is literally what ran. A separate "preview" type is how a
/// preview drifts from the thing it previews.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MigrationReport {
    pub source: String,
    /// False for `inspect` and for a dry run: nothing was written.
    pub applied: bool,
    /// Where the state needed to undo this was written, when it was applied.
    pub rollback: Option<String>,
    pub items: Vec<MigrationItem>,
}

impl MigrationReport {
    pub fn count(&self, action: MigrationAction) -> usize {
        self.items
            .iter()
            .filter(|item| item.action == action)
            .count()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MigrationItem {
    /// What kind of thing this is: an agent, a session, a preference.
    pub kind: String,
    pub name: String,
    pub action: MigrationAction,
    /// Why, for anything not imported. Always present for a skip: a refusal
    /// without a reason is not reviewable, and reviewable is the whole point.
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum MigrationAction {
    /// Would be, or was, imported.
    Import,
    /// Already present in the destination; left alone.
    AlreadyPresent,
    /// Deliberately not imported. `detail` says why.
    Skip,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: SessionId,
    pub name: String,
    pub state: SessionState,
    pub agent: AgentId,
    pub workspace: Option<WorkspaceSpec>,
    /// The session's security posture, resolved when it was created.
    ///
    /// Always present, and always reported to clients: a posture nobody can
    /// see is one nobody checks, and "which of these agents is uncontained"
    /// has to be answerable without reading a config file.
    #[serde(default)]
    pub security: SecurityPosture,
}

/// What a client is told about a session's security, without depending on the
/// security crate to say it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecurityPosture {
    /// The profile's name, as configured.
    pub profile: String,
    /// Whether the agent is actually confined.
    pub contained: bool,
    /// One line a person can read.
    pub summary: String,
    /// What this posture does *not* enforce. Empty when it enforces
    /// everything it declares.
    #[serde(default)]
    pub caveats: Vec<String>,
}

impl Default for SecurityPosture {
    /// The posture of a session created before postures existed, or by a
    /// client that did not ask: uncontained, and saying so.
    fn default() -> Self {
        Self {
            profile: "default".to_owned(),
            contained: false,
            summary: "sandbox=host (ambient trust: no containment)".to_owned(),
            caveats: vec!["runs on the host with your full authority".to_owned()],
        }
    }
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
/// A terminal screen as the daemon sees it.
///
/// Carries the *grid*, not flattened text. A screen is fixed rows by fixed
/// columns; joining it into one string throws away that structure, and a
/// client then re-wraps it at whatever width it happens to have, which
/// scrambles any full-screen program. Color, the cursor and the
/// alternate-screen flag go the same way: and an agent's TUI is exactly the
/// thing that needs all four.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenSnapshot {
    pub size: Size,
    /// One entry per row, top to bottom. Always `size.rows` long.
    pub rows: Vec<Vec<Span>>,
    pub cursor: Cursor,
    /// True while a full-screen program holds the alternate screen. A client
    /// restoring a session needs this to know whether to expect scrollback.
    pub alternate_screen: bool,
}

impl ScreenSnapshot {
    /// The screen as plain text, one line per row, trailing blanks trimmed.
    ///
    /// For logs, tests and `capture`: never for rendering, which is what the
    /// spans are for.
    pub fn to_text(&self) -> String {
        self.rows
            .iter()
            .map(|row| {
                let line: String = row.iter().map(|span| span.text.as_str()).collect();
                line.trim_end().to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// A run of characters sharing one style.
///
/// Runs rather than cells: an ordinary row is a single span, so a mostly
/// plain screen costs about what the old string did, while a colored one
/// keeps its color.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub text: String,
    #[serde(default, skip_serializing_if = "Style::is_plain")]
    pub style: Style,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Cursor {
    pub row: u16,
    pub column: u16,
    /// False while the program has hidden it. Drawing a cursor a program
    /// deliberately hid is a visible artifact, so clients need to be told.
    pub visible: bool,
}

/// Terminal color, in the three forms a terminal actually uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Color {
    #[default]
    Default,
    /// A palette index. Kept as an index rather than resolved to RGB so the
    /// viewer's own theme decides what "red" looks like.
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Style {
    pub foreground: Color,
    pub background: Color,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
}

impl Style {
    /// Whether this style says nothing, so it can be omitted on the wire.
    pub fn is_plain(&self) -> bool {
        *self == Self::default()
    }
}
/// An action the daemon is holding until somebody decides.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub id: String,
    pub session: SessionId,
    /// What is being asked, in words a person can act on.
    pub action: String,
    /// Whether refusing is the safer default, so a client can present it
    /// accordingly.
    pub destructive: bool,
    /// Seconds remaining before this expires unapproved.
    pub expires_in_secs: u64,
}

/// What the daemon's host can contain an agent with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxReport {
    pub platform: String,
    /// Strongest first, available or not: an operator needs to see what was
    /// looked for, not only what was found.
    pub candidates: Vec<SandboxCandidate>,
    /// `None` means this host cannot contain an agent at all today.
    pub recommended: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxCandidate {
    pub name: String,
    pub available: bool,
    /// "separate kernel in a VM" / "OS-level, shares the host kernel".
    pub isolation: String,
    /// Version string when found, reason when not.
    pub detail: String,
    pub caveat: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    /// The daemon will not proceed until somebody decides.
    ApprovalRequested {
        approval: ApprovalRequest,
    },
    /// Nobody decided in time. Reported separately from a denial so a client
    /// can retry a timeout without retrying a refusal.
    ApprovalExpired {
        id: String,
    },
    /// Somebody decided; other clients showing the prompt should drop it.
    ApprovalResolved {
        id: String,
        approved: bool,
    },
    SessionCreated {
        session: SessionSummary,
    },
    SessionStateChanged {
        session: SessionSummary,
    },
    OutputChanged {
        id: SessionId,
    },
    ScreenChanged {
        id: SessionId,
    },
    SessionExited {
        id: SessionId,
        code: Option<i32>,
    },
    BackendError {
        backend: BackendId,
        message: String,
    },
}
/// Anything the daemon sends back on the connection.
///
/// Responses and events share one socket, so they must share one frame type.
/// Decoding each frame as whichever the reader happened to expect meant the
/// first event to arrive during a request was parsed as a response, failed,
/// and killed the connection. A single enum makes the demultiplexing a match
/// rather than a guess.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Message {
    Response(Response),
    Event(Event),
}

impl From<Response> for Message {
    fn from(response: Response) -> Self {
        Self::Response(response)
    }
}

impl From<Event> for Message {
    fn from(event: Event) -> Self {
        Self::Event(event)
    }
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
