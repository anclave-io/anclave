use std::collections::BTreeSet;
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
