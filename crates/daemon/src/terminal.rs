use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anclave_protocol::{ScreenSnapshot, SessionId, Size};
use anclave_terminal::TerminalSurface;

pub const DEFAULT_SIZE: Size = Size {
    columns: 80,
    rows: 24,
};

/// One session's terminal, plus the last capture it was built from.
struct Entry {
    surface: TerminalSurface,
    /// The raw text of the previous capture, kept only to answer "did
    /// anything change". Without it the daemon cannot tell a quiet session
    /// from a busy one and publishes an event on every poll.
    last_capture: String,
    /// Cursor and alternate-screen state as the multiplexer reports it.
    ///
    /// These cannot be recovered from the captured text: it holds rendered
    /// characters, not the mode escapes that produced them, so a parser fed
    /// that text puts the cursor wherever writing happened to end and never
    /// sees the alternate screen at all.
    pane_state: Option<crate::backend::PaneState>,
}

#[derive(Clone, Default)]
pub struct TerminalStore {
    surfaces: Arc<Mutex<HashMap<String, Entry>>>,
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
            .insert(
                id.to_string(),
                Entry {
                    surface,
                    last_capture: String::new(),
                    pane_state: None,
                },
            );
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
        let entry = surfaces
            .get_mut(id.as_str())
            .ok_or(TerminalError::NotFound)?;
        entry.surface.write_output(bytes);
        Ok(())
    }

    pub fn resize(&self, id: &SessionId, size: Size) -> Result<(), TerminalError> {
        let mut surfaces = self
            .surfaces
            .lock()
            .expect("terminal store mutex is not poisoned");
        let entry = surfaces
            .get_mut(id.as_str())
            .ok_or(TerminalError::NotFound)?;
        entry
            .surface
            .resize(size)
            .map_err(|_| TerminalError::InvalidSize)
    }

    pub fn capture(&self, id: &SessionId) -> Result<ScreenSnapshot, TerminalError> {
        let surfaces = self
            .surfaces
            .lock()
            .expect("terminal store mutex is not poisoned");
        let entry = surfaces.get(id.as_str()).ok_or(TerminalError::NotFound)?;
        let mut screen = entry.surface.screen();
        // Overlay what only the multiplexer knows. Without this the cursor
        // sits wherever the captured text ended and `alternate_screen` is
        // always false, so a client cannot tell a full-screen program from a
        // scrolling one.
        if let Some(state) = entry.pane_state {
            screen.cursor = anclave_protocol::Cursor {
                row: state.cursor_row,
                column: state.cursor_column,
                visible: state.cursor_visible,
            };
            screen.alternate_screen = state.alternate_screen;
        }
        Ok(screen)
    }

    /// Record the terminal state the multiplexer reports.
    pub fn set_pane_state(
        &self,
        id: &SessionId,
        state: crate::backend::PaneState,
    ) -> Result<(), TerminalError> {
        let mut surfaces = self
            .surfaces
            .lock()
            .expect("terminal store mutex is not poisoned");
        let entry = surfaces
            .get_mut(id.as_str())
            .ok_or(TerminalError::NotFound)?;
        entry.pane_state = Some(state);
        Ok(())
    }

    /// Apply a full-screen capture, reporting whether anything changed.
    ///
    /// The backend hands back the whole rendered pane each poll, so this
    /// *replaces* rather than appends. Appending grew the parser by a screen
    /// every poll and hit the surface's byte ceiling after a few minutes,
    /// at which point the session's terminal stopped updating for good.
    ///
    /// Returning `false` for an unchanged capture is what lets the daemon
    /// stay quiet: a session nobody is typing into publishes no events.
    pub fn apply_capture(&self, id: &SessionId, text: &str) -> Result<bool, TerminalError> {
        let mut surfaces = self
            .surfaces
            .lock()
            .expect("terminal store mutex is not poisoned");
        let entry = surfaces
            .get_mut(id.as_str())
            .ok_or(TerminalError::NotFound)?;

        if entry.last_capture == text {
            return Ok(false);
        }

        entry.surface.reset();
        entry.surface.write_output(text.as_bytes());
        entry.last_capture = text.to_owned();
        Ok(true)
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

    /// The daemon must stay quiet for a session nobody is touching. An
    /// unchanged capture is not a screen change.
    #[test]
    fn an_unchanged_capture_reports_no_change() {
        let store = TerminalStore::new();
        store.insert(&id(), DEFAULT_SIZE).unwrap();

        assert!(store.apply_capture(&id(), "hello").unwrap());
        assert!(!store.apply_capture(&id(), "hello").unwrap());
        assert!(!store.apply_capture(&id(), "hello").unwrap());
        assert!(store.apply_capture(&id(), "hello there").unwrap());
    }

    /// The backend reports the whole pane each poll. Appending it grew the
    /// parser without bound and, once the surface hit its byte ceiling, the
    /// session's screen stopped updating permanently.
    #[test]
    fn repeated_captures_do_not_exhaust_the_byte_budget() {
        let store = TerminalStore::new();
        store.insert(&id(), DEFAULT_SIZE).unwrap();

        // Far more bytes in total than a surface will accept in one lifetime.
        let screen = "x".repeat(2000);
        for n in 0..5000 {
            let text = format!("{screen}{n}");
            store.apply_capture(&id(), &text).unwrap();
        }

        // Still rendering, rather than stuck behind a truncation flag.
        let shown = store.capture(&id()).unwrap().to_text();
        assert!(shown.contains("4999"), "screen stopped updating: {shown:?}");
    }

    #[test]
    fn a_capture_replaces_rather_than_accumulates() {
        let store = TerminalStore::new();
        store.insert(&id(), DEFAULT_SIZE).unwrap();

        store.apply_capture(&id(), "first").unwrap();
        store.apply_capture(&id(), "second").unwrap();

        let shown = store.capture(&id()).unwrap().to_text();
        assert!(shown.contains("second"));
        assert!(
            !shown.contains("first"),
            "the previous screen bled through: {shown:?}"
        );
    }

    /// An exited session still has a screen worth showing: its last output.
    #[test]
    fn the_final_screen_survives_after_output_stops() {
        let store = TerminalStore::new();
        store.insert(&id(), DEFAULT_SIZE).unwrap();
        store.apply_capture(&id(), "goodbye").unwrap();

        for _ in 0..10 {
            assert!(!store.apply_capture(&id(), "goodbye").unwrap());
        }
        assert!(store.capture(&id()).unwrap().to_text().contains("goodbye"));
    }

    #[test]
    fn applying_a_capture_to_an_unknown_session_is_an_error() {
        let store = TerminalStore::new();
        assert_eq!(
            store.apply_capture(&id(), "x"),
            Err(TerminalError::NotFound)
        );
    }

    #[test]
    fn store_tracks_output_and_capture() {
        let store = TerminalStore::new();
        store.insert(&id(), DEFAULT_SIZE).unwrap();
        store.write_output(&id(), b"hello").unwrap();
        assert!(store.capture(&id()).unwrap().to_text().starts_with("hello"));
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
