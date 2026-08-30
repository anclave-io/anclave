use std::collections::BTreeSet;
use std::process::Command;
use std::sync::{Arc, Mutex};

use anclave_protocol::{BackendId, SessionId, Size};

use crate::agent::LaunchSpec;

pub const MAX_INPUT_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateRequest {
    pub session_id: SessionId,
    pub name: String,
    pub size: Size,
    pub launch: LaunchSpec,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendSession {
    pub session_id: SessionId,
    pub backend: BackendId,
}

pub trait SessionBackend: Send + Sync {
    fn create(&self, request: CreateRequest) -> Result<BackendSession, BackendError>;
    fn adopt(&self, id: &SessionId) -> Result<BackendSession, BackendError>;
    fn restart(&self, request: CreateRequest) -> Result<BackendSession, BackendError> {
        let _ = self.kill(&request.session_id);
        self.create(request)
    }
    fn kill(&self, id: &SessionId) -> Result<(), BackendError>;
    fn resize(&self, id: &SessionId, size: Size) -> Result<(), BackendError>;
    fn send_input(&self, id: &SessionId, bytes: &[u8]) -> Result<(), BackendError>;
    fn capture(&self, id: &SessionId) -> Result<String, BackendError>;
    fn sessions(&self) -> Result<Vec<SessionId>, BackendError>;
}

/// Turn tmux's line-oriented capture into something a VT parser can read.
///
/// `capture-pane` separates rows with a bare `\n`. To a terminal parser LF
/// means "down one row, same column": the carriage return that real
/// terminals see is added by the tty driver's ONLCR, which is not involved
/// here. Feeding the capture through unchanged draws every line one step
/// further right than the last, a staircase instead of a screen.
pub fn to_terminal_bytes(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 40);
    let mut previous = '\0';
    for ch in text.chars() {
        if ch == '\n' && previous != '\r' {
            out.push('\r');
        }
        out.push(ch);
        previous = ch;
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendError {
    AlreadyExists,
    NotFound,
    InvalidSize,
    InputTooLarge,
    Failed(String),
}

#[derive(Debug, Default)]
pub struct FakeBackend {
    sessions: Mutex<BTreeSet<String>>,
    inputs: Mutex<Vec<(String, Vec<u8>)>>,
    launches: Mutex<Vec<CreateRequest>>,
}

impl FakeBackend {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn contains(&self, id: &SessionId) -> bool {
        self.sessions
            .lock()
            .expect("fake backend mutex is not poisoned")
            .contains(id.as_str())
    }

    pub fn launches(&self) -> Vec<CreateRequest> {
        self.launches
            .lock()
            .expect("fake backend launch mutex is not poisoned")
            .clone()
    }

    pub fn inputs(&self) -> Vec<(String, Vec<u8>)> {
        self.inputs
            .lock()
            .expect("fake backend input mutex is not poisoned")
            .clone()
    }
}

impl SessionBackend for FakeBackend {
    fn create(&self, request: CreateRequest) -> Result<BackendSession, BackendError> {
        request
            .size
            .validate()
            .map_err(|_| BackendError::InvalidSize)?;
        self.launches
            .lock()
            .expect("fake backend launch mutex is not poisoned")
            .push(request.clone());
        let mut sessions = self
            .sessions
            .lock()
            .expect("fake backend mutex is not poisoned");
        if !sessions.insert(request.session_id.to_string()) {
            return Err(BackendError::AlreadyExists);
        }
        Ok(BackendSession {
            session_id: request.session_id,
            backend: BackendId::new("fake").expect("static backend ID is valid"),
        })
    }

    fn adopt(&self, id: &SessionId) -> Result<BackendSession, BackendError> {
        if self.contains(id) {
            Ok(BackendSession {
                session_id: id.clone(),
                backend: BackendId::new("fake").expect("static backend ID is valid"),
            })
        } else {
            Err(BackendError::NotFound)
        }
    }

    fn kill(&self, id: &SessionId) -> Result<(), BackendError> {
        let removed = self
            .sessions
            .lock()
            .expect("fake backend mutex is not poisoned")
            .remove(id.as_str());
        if removed {
            Ok(())
        } else {
            Err(BackendError::NotFound)
        }
    }

    fn resize(&self, id: &SessionId, size: Size) -> Result<(), BackendError> {
        size.validate().map_err(|_| BackendError::InvalidSize)?;
        if self.contains(id) {
            Ok(())
        } else {
            Err(BackendError::NotFound)
        }
    }

    fn send_input(&self, id: &SessionId, bytes: &[u8]) -> Result<(), BackendError> {
        if bytes.len() > MAX_INPUT_BYTES {
            return Err(BackendError::InputTooLarge);
        }
        if !self.contains(id) {
            return Err(BackendError::NotFound);
        }
        self.inputs
            .lock()
            .expect("fake backend input mutex is not poisoned")
            .push((id.to_string(), bytes.to_vec()));
        Ok(())
    }

    fn capture(&self, id: &SessionId) -> Result<String, BackendError> {
        if self.contains(id) {
            Ok(String::new())
        } else {
            Err(BackendError::NotFound)
        }
    }

    fn sessions(&self) -> Result<Vec<SessionId>, BackendError> {
        self.sessions
            .lock()
            .expect("fake backend mutex is not poisoned")
            .iter()
            .map(|id| {
                SessionId::new(id.clone())
                    .map_err(|_| BackendError::Failed("invalid session ID".to_owned()))
            })
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct LocalTmuxBackend {
    socket: String,
    session: String,
    default_command: LaunchSpec,
}

impl LocalTmuxBackend {
    pub fn new(socket: impl Into<String>, session: impl Into<String>) -> Self {
        Self {
            socket: socket.into(),
            session: session.into(),
            default_command: LaunchSpec {
                program: "sh".to_owned(),
                args: Vec::new(),
                environment: None,
            },
        }
    }

    pub fn with_command(mut self, command: LaunchSpec) -> Self {
        self.default_command = command;
        self
    }

    fn tmux(&self, args: &[String]) -> Result<std::process::Output, BackendError> {
        Command::new("tmux")
            .arg("-S")
            .arg(&self.socket)
            .args(args)
            .output()
            .map_err(|error| BackendError::Failed(format!("start tmux: {error}")))
    }

    /// Whether this backend's tmux session already exists.
    fn session_exists(&self) -> bool {
        self.tmux(&[
            "has-session".to_owned(),
            "-t".to_owned(),
            self.session.clone(),
        ])
        .map(|output| output.status.success())
        .unwrap_or(false)
    }

    fn target(&self, id: &SessionId) -> String {
        format!("{}:{}", self.session, id)
    }

    fn check(output: std::process::Output) -> Result<std::process::Output, BackendError> {
        if output.status.success() {
            return Ok(output);
        }
        let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        Err(BackendError::Failed(if message.is_empty() {
            "tmux command failed".to_owned()
        } else {
            message
        }))
    }
}

impl SessionBackend for LocalTmuxBackend {
    fn create(&self, request: CreateRequest) -> Result<BackendSession, BackendError> {
        request
            .size
            .validate()
            .map_err(|_| BackendError::InvalidSize)?;
        let launch = if request.launch.program.is_empty() {
            &self.default_command
        } else {
            &request.launch
        };
        // The first session creates the tmux session; every one after it adds
        // a window to the existing one. Using `new-session` unconditionally
        // meant the second session a daemon ever created failed with
        // "duplicate session": the daemon could host exactly one, which for
        // a multi-session orchestrator is the whole product.
        let mut args = if self.session_exists() {
            vec![
                "new-window".to_owned(),
                "-d".to_owned(),
                "-t".to_owned(),
                self.session.clone(),
                "-n".to_owned(),
                request.session_id.to_string(),
            ]
        } else {
            vec![
                "new-session".to_owned(),
                "-d".to_owned(),
                "-s".to_owned(),
                self.session.clone(),
                "-n".to_owned(),
                request.session_id.to_string(),
                // `new-window` takes no -x/-y; the window is sized after it
                // exists instead.
                "-x".to_owned(),
                request.size.columns.to_string(),
                "-y".to_owned(),
                request.size.rows.to_string(),
            ]
        };

        // A constructed environment is delivered by launching through
        // `env -i`, not by tmux's own `-e`. `-e` *adds* variables to whatever
        // the tmux server already has, so the inherited credentials would
        // survive alongside ours: the policy would appear applied and change
        // nothing. `env -i` starts from empty, which is the only form that
        // matches what `build_environment` promises.
        match &launch.environment {
            Some(environment) => {
                args.push("env".to_owned());
                args.push("-i".to_owned());
                for (name, value) in environment {
                    args.push(format!("{name}={value}"));
                }
                args.push(launch.program.clone());
            }
            None => args.push(launch.program.clone()),
        }
        args.extend(launch.args.iter().cloned());
        Self::check(self.tmux(&args)?).map_err(|error| match error {
            BackendError::Failed(message) if message.contains("duplicate") => {
                BackendError::AlreadyExists
            }
            other => other,
        })?;
        // `new-window` inherits the session's size, so a window created after
        // the first would silently take the first one's dimensions.
        let _ = self.tmux(&[
            "resize-window".to_owned(),
            "-t".to_owned(),
            self.target(&request.session_id),
            "-x".to_owned(),
            request.size.columns.to_string(),
            "-y".to_owned(),
            request.size.rows.to_string(),
        ]);
        Ok(BackendSession {
            session_id: request.session_id,
            backend: BackendId::new("local-tmux").expect("static backend ID is valid"),
        })
    }

    fn adopt(&self, id: &SessionId) -> Result<BackendSession, BackendError> {
        let output = Self::check(self.tmux(&[
            "has-session".to_owned(),
            "-t".to_owned(),
            self.target(id),
        ])?);
        match output {
            Ok(_) => Ok(BackendSession {
                session_id: id.clone(),
                backend: BackendId::new("local-tmux").expect("static backend ID is valid"),
            }),
            Err(BackendError::Failed(message))
                if message.contains("can't find") || message.contains("no server running") =>
            {
                Err(BackendError::NotFound)
            }
            Err(error) => Err(error),
        }
    }

    fn kill(&self, id: &SessionId) -> Result<(), BackendError> {
        match Self::check(self.tmux(&[
            "kill-window".to_owned(),
            "-t".to_owned(),
            self.target(id),
        ])?) {
            Ok(_) => Ok(()),
            Err(BackendError::Failed(message))
                if message.contains("can't find") || message.contains("no server running") =>
            {
                Err(BackendError::NotFound)
            }
            Err(error) => Err(error),
        }
    }

    fn resize(&self, id: &SessionId, size: Size) -> Result<(), BackendError> {
        size.validate().map_err(|_| BackendError::InvalidSize)?;
        Self::check(self.tmux(&[
            "resize-window".to_owned(),
            "-t".to_owned(),
            self.target(id),
            "-x".to_owned(),
            size.columns.to_string(),
            "-y".to_owned(),
            size.rows.to_string(),
        ])?)
        .map(|_| ())
    }

    fn send_input(&self, id: &SessionId, bytes: &[u8]) -> Result<(), BackendError> {
        if bytes.len() > MAX_INPUT_BYTES {
            return Err(BackendError::InputTooLarge);
        }
        if bytes.is_empty() {
            return Ok(());
        }
        let encoded = bytes
            .iter()
            .map(|byte| format!("{:02x}", byte))
            .collect::<Vec<_>>()
            .join(" ");
        let output = self.tmux(&[
            "send-keys".to_owned(),
            "-H".to_owned(),
            "-t".to_owned(),
            self.target(id),
            encoded,
        ])?;
        match Self::check(output) {
            Ok(_) => Ok(()),
            Err(BackendError::Failed(message))
                if message.contains("can't find") || message.contains("no server running") =>
            {
                Err(BackendError::NotFound)
            }
            Err(error) => Err(error),
        }
    }

    fn capture(&self, id: &SessionId) -> Result<String, BackendError> {
        let output = Self::check(self.tmux(&[
            "capture-pane".to_owned(),
            "-p".to_owned(),
            "-J".to_owned(),
            // Keep the agent's colors. Without this tmux hands back plain
            // text and every screen the daemon publishes is monochrome no
            // matter what the agent drew.
            "-e".to_owned(),
            "-t".to_owned(),
            self.target(id),
        ])?)?;
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        Ok(to_terminal_bytes(&text))
    }

    fn sessions(&self) -> Result<Vec<SessionId>, BackendError> {
        let output = match Self::check(self.tmux(&[
            "list-windows".to_owned(),
            "-t".to_owned(),
            self.session.clone(),
            "-F".to_owned(),
            "#{window_name}".to_owned(),
        ])?) {
            Ok(output) => output,
            Err(BackendError::Failed(message)) if message.contains("no server running") => {
                return Ok(Vec::new())
            }
            Err(error) => return Err(error),
        };
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| {
                SessionId::new(line)
                    .map_err(|_| BackendError::Failed("invalid session ID".to_owned()))
            })
            .collect()
    }
}

pub type SharedBackend = Arc<dyn SessionBackend>;

#[cfg(test)]
mod tests {
    use super::*;

    fn request(id: &str) -> CreateRequest {
        CreateRequest {
            session_id: SessionId::new(id).unwrap(),
            name: "demo".to_owned(),
            size: Size {
                columns: 80,
                rows: 24,
            },
            launch: LaunchSpec {
                program: "sh".to_owned(),
                args: Vec::new(),
                environment: None,
            },
        }
    }

    /// tmux hands back rows joined by a bare LF. A VT parser reads that as
    /// "down one row, same column", so an unconverted capture renders as a
    /// staircase rather than a screen.
    #[test]
    fn line_feeds_from_tmux_become_carriage_return_line_feeds() {
        assert_eq!(to_terminal_bytes("a\nb\nc"), "a\r\nb\r\nc");
    }

    #[test]
    fn an_existing_carriage_return_is_not_doubled() {
        assert_eq!(to_terminal_bytes("a\r\nb"), "a\r\nb");
    }

    #[test]
    fn text_without_line_feeds_is_unchanged() {
        assert_eq!(to_terminal_bytes("plain"), "plain");
        assert_eq!(to_terminal_bytes(""), "");
    }

    /// The escapes `capture-pane -e` emits must survive untouched, or the
    /// colors they carry are lost on the way to the parser.
    #[test]
    fn escape_sequences_pass_through() {
        let colored = "\x1b[31mred\x1b[0m\ntail";
        assert_eq!(to_terminal_bytes(colored), "\x1b[31mred\x1b[0m\r\ntail");
    }

    #[test]
    fn fake_backend_tracks_lifecycle_and_input() {
        let backend = FakeBackend::new();
        let value = request("session-1");
        backend.create(value.clone()).unwrap();
        backend.send_input(&value.session_id, b"hello").unwrap();
        assert_eq!(
            backend.inputs(),
            vec![("session-1".to_owned(), b"hello".to_vec())]
        );
        backend
            .resize(
                &value.session_id,
                Size {
                    columns: 100,
                    rows: 30,
                },
            )
            .unwrap();
        assert_eq!(backend.sessions().unwrap().len(), 1);
        assert!(backend.adopt(&value.session_id).is_ok());
        backend.kill(&value.session_id).unwrap();
        assert!(!backend.contains(&value.session_id));
    }

    #[test]
    fn fake_backend_rejects_duplicate_invalid_and_oversized_input() {
        let backend = FakeBackend::new();
        let value = request("session-1");
        backend.create(value.clone()).unwrap();
        assert_eq!(backend.create(value), Err(BackendError::AlreadyExists));
        assert_eq!(
            backend.send_input(&SessionId::new("missing").unwrap(), b"x"),
            Err(BackendError::NotFound)
        );
        assert_eq!(
            backend.send_input(
                &SessionId::new("session-1").unwrap(),
                &vec![0; MAX_INPUT_BYTES + 1]
            ),
            Err(BackendError::InputTooLarge)
        );
    }
}
