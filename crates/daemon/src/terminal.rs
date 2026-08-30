use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anclave_protocol::{ScreenSnapshot, SessionId, Size};
use anclave_terminal::TerminalSurface;

pub const DEFAULT_SIZE: Size = Size {
    columns: 80,
    rows: 24,
};

#[derive(Clone, Default)]
pub struct TerminalStore {
    surfaces: Arc<Mutex<HashMap<String, TerminalSurface>>>,
}

impl TerminalStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, id: &SessionId, size: Size) -> Result<(), TerminalError> {
        let surface = TerminalSurface::new(size).map_err(|_| TerminalError::InvalidSize)?;
        self.surfaces
            .lock()
            .expect("terminal store mutex is not poisoned")
            .insert(id.to_string(), surface);
        Ok(())
    }

    pub fn remove(&self, id: &SessionId) {
        self.surfaces
            .lock()
            .expect("terminal store mutex is not poisoned")
            .remove(id.as_str());
    }

    pub fn write_output(&self, id: &SessionId, bytes: &[u8]) -> Result<(), TerminalError> {
        let mut surfaces = self
            .surfaces
            .lock()
            .expect("terminal store mutex is not poisoned");
        let surface = surfaces
            .get_mut(id.as_str())
            .ok_or(TerminalError::NotFound)?;
        surface.write_output(bytes);
        Ok(())
    }

    pub fn resize(&self, id: &SessionId, size: Size) -> Result<(), TerminalError> {
        let mut surfaces = self
            .surfaces
            .lock()
            .expect("terminal store mutex is not poisoned");
        let surface = surfaces
            .get_mut(id.as_str())
            .ok_or(TerminalError::NotFound)?;
        surface.resize(size).map_err(|_| TerminalError::InvalidSize)
    }

    pub fn capture(&self, id: &SessionId) -> Result<ScreenSnapshot, TerminalError> {
        let surfaces = self
            .surfaces
            .lock()
            .expect("terminal store mutex is not poisoned");
        surfaces
            .get(id.as_str())
            .map(TerminalSurface::screen)
            .ok_or(TerminalError::NotFound)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalError {
    InvalidSize,
    NotFound,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> SessionId {
        SessionId::new("session-1").unwrap()
    }

    #[test]
    fn store_tracks_output_and_capture() {
        let store = TerminalStore::new();
        store.insert(&id(), DEFAULT_SIZE).unwrap();
        store.write_output(&id(), b"hello").unwrap();
        assert!(store.capture(&id()).unwrap().content.starts_with("hello"));
        store.remove(&id());
        assert_eq!(store.capture(&id()), Err(TerminalError::NotFound));
    }

    #[test]
    fn store_validates_sizes_and_missing_sessions() {
        let store = TerminalStore::new();
        assert_eq!(
            store.insert(
                &id(),
                Size {
                    columns: 0,
                    rows: 1
                }
            ),
            Err(TerminalError::InvalidSize)
        );
        assert_eq!(
            store.write_output(&id(), b"x"),
            Err(TerminalError::NotFound)
        );
        assert_eq!(
            store.resize(&id(), DEFAULT_SIZE),
            Err(TerminalError::NotFound)
        );
    }
}
