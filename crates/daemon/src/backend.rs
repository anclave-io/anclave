use std::collections::BTreeSet;
use std::process::Command;
use std::sync::{Arc, Mutex};

use anclave_protocol::{BackendId, SessionId, Size};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateRequest {
    pub session_id: SessionId,
    pub name: String,
    pub size: Size,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendSession {
    pub session_id: SessionId,
    pub backend: BackendId,
}

pub trait SessionBackend: Send + Sync {
    fn create(&self, request: CreateRequest) -> Result<BackendSession, BackendError>;
    fn kill(&self, id: &SessionId) -> Result<(), BackendError>;
    fn resize(&self, id: &SessionId, size: Size) -> Result<(), BackendError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendError {
    AlreadyExists,
    NotFound,
    InvalidSize,
    Failed(String),
}

#[derive(Debug, Default)]
pub struct FakeBackend {
    sessions: Mutex<BTreeSet<String>>,
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
}

impl SessionBackend for FakeBackend {
    fn create(&self, request: CreateRequest) -> Result<BackendSession, BackendError> {
        request
            .size
            .validate()
            .map_err(|_| BackendError::InvalidSize)?;
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
}

#[derive(Debug, Clone)]
pub struct LocalTmuxBackend {
    socket: String,
    session: String,
    command: String,
}

impl LocalTmuxBackend {
    pub fn new(socket: impl Into<String>, session: impl Into<String>) -> Self {
        Self {
            socket: socket.into(),
            session: session.into(),
            command: "sh".to_owned(),
        }
    }

    pub fn with_command(mut self, command: impl Into<String>) -> Self {
        self.command = command.into();
        self
    }

    fn tmux(&self, args: &[&str]) -> Result<std::process::Output, BackendError> {
        Command::new("tmux")
            .arg("-S")
            .arg(&self.socket)
            .args(args)
            .output()
            .map_err(|error| BackendError::Failed(format!("start tmux: {error}")))
    }

    fn target(&self, id: &SessionId) -> String {
        format!("{}:{}", self.session, id)
    }

    fn check(output: std::process::Output) -> Result<std::process::Output, BackendError> {
        if output.status.success() {
            Ok(output)
        } else {
            let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            Err(BackendError::Failed(if message.is_empty() {
                "tmux command failed".to_owned()
            } else {
                message
            }))
        }
    }
}

impl SessionBackend for LocalTmuxBackend {
    fn create(&self, request: CreateRequest) -> Result<BackendSession, BackendError> {
        request
            .size
            .validate()
            .map_err(|_| BackendError::InvalidSize)?;
        let window = request.session_id.as_str();
        let size_x = request.size.columns.to_string();
        let size_y = request.size.rows.to_string();
        let output = self.tmux(&[
            "new-session",
            "-d",
            "-s",
            &self.session,
            "-n",
            window,
            "-x",
            &size_x,
            "-y",
            &size_y,
            &self.command,
        ])?;
        let output = Self::check(output).map_err(|error| match error {
            BackendError::Failed(message) if message.contains("duplicate") => {
                BackendError::AlreadyExists
            }
            other => other,
        })?;
        if !output.status.success() {
            return Err(BackendError::Failed(
                "tmux session creation failed".to_owned(),
            ));
        }
        Ok(BackendSession {
            session_id: request.session_id,
            backend: BackendId::new("local-tmux").expect("static backend ID is valid"),
        })
    }

    fn kill(&self, id: &SessionId) -> Result<(), BackendError> {
        let output = self.tmux(&["kill-window", "-t", &self.target(id)])?;
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

    fn resize(&self, id: &SessionId, size: Size) -> Result<(), BackendError> {
        size.validate().map_err(|_| BackendError::InvalidSize)?;
        let columns = size.columns.to_string();
        let rows = size.rows.to_string();
        let output = self.tmux(&[
            "resize-window",
            "-t",
            &self.target(id),
            "-x",
            &columns,
            "-y",
            &rows,
        ])?;
        Self::check(output).map(|_| ())
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
        }
    }

    #[test]
    fn fake_backend_tracks_lifecycle() {
        let backend = FakeBackend::new();
        let value = request("session-1");
        backend.create(value.clone()).unwrap();
        assert!(backend.contains(&value.session_id));
        backend
            .resize(
                &value.session_id,
                Size {
                    columns: 100,
                    rows: 30,
                },
            )
            .unwrap();
        backend.kill(&value.session_id).unwrap();
        assert!(!backend.contains(&value.session_id));
    }

    #[test]
    fn fake_backend_rejects_duplicate_and_invalid_requests() {
        let backend = FakeBackend::new();
        let value = request("session-1");
        backend.create(value.clone()).unwrap();
        assert_eq!(backend.create(value), Err(BackendError::AlreadyExists));
        assert_eq!(
            backend.create(CreateRequest {
                session_id: SessionId::new("session-2").unwrap(),
                name: "demo".to_owned(),
                size: Size {
                    columns: 0,
                    rows: 1
                },
            }),
            Err(BackendError::InvalidSize)
        );
    }
}
