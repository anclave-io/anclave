use anclave_protocol::{ScreenSnapshot, Size};

pub const MAX_OUTPUT_BYTES: usize = 4 * 1024 * 1024;

pub struct TerminalSurface {
    parser: vt100::Parser,
    size: Size,
    output_bytes: usize,
    truncated: bool,
}

impl TerminalSurface {
    pub fn new(size: Size) -> Result<Self, TerminalError> {
        size.validate().map_err(|_| TerminalError::InvalidSize)?;
        Ok(Self {
            parser: vt100::Parser::new(size.rows, size.columns, 0),
            size,
            output_bytes: 0,
            truncated: false,
        })
    }

    pub fn resize(&mut self, size: Size) -> Result<(), TerminalError> {
        size.validate().map_err(|_| TerminalError::InvalidSize)?;
        self.parser = vt100::Parser::new(size.rows, size.columns, 0);
        self.size = size;
        Ok(())
    }

    pub fn write_output(&mut self, bytes: &[u8]) {
        if self.output_bytes >= MAX_OUTPUT_BYTES {
            self.truncated = true;
            return;
        }
        let remaining = MAX_OUTPUT_BYTES - self.output_bytes;
        let accepted = bytes.len().min(remaining);
        self.parser.process(&bytes[..accepted]);
        self.output_bytes += accepted;
        if accepted < bytes.len() {
            self.truncated = true;
        }
    }

    pub fn screen(&self) -> ScreenSnapshot {
        let screen = self.parser.screen();
        let content = (0..self.size.rows)
            .map(|row| screen.contents_between(row, 0, row, self.size.columns))
            .collect::<Vec<_>>()
            .join("\n");
        ScreenSnapshot {
            size: self.size,
            content,
        }
    }

    pub fn cursor_position(&self) -> (u16, u16) {
        let (row, column) = self.parser.screen().cursor_position();
        (row, column)
    }

    pub fn output_bytes(&self) -> usize {
        self.output_bytes
    }

    pub fn is_truncated(&self) -> bool {
        self.truncated
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalError {
    InvalidSize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_becomes_a_screen_snapshot() {
        let mut terminal = TerminalSurface::new(Size {
            columns: 10,
            rows: 2,
        })
        .unwrap();
        terminal.write_output(b"hello");
        let screen = terminal.screen();
        assert_eq!(screen.size.columns, 10);
        assert!(screen.content.starts_with("hello"));
        assert_eq!(terminal.cursor_position(), (0, 5));
    }

    #[test]
    fn ansi_sequences_are_interpreted_by_vt100() {
        let mut terminal = TerminalSurface::new(Size {
            columns: 10,
            rows: 2,
        })
        .unwrap();
        terminal.write_output(b"\x1b[2;3Hok");
        assert_eq!(terminal.cursor_position(), (1, 4));
        assert!(terminal.screen().content.contains("  ok"));
    }

    #[test]
    fn resize_updates_parser_and_snapshot_dimensions() {
        let mut terminal = TerminalSurface::new(Size {
            columns: 10,
            rows: 2,
        })
        .unwrap();
        terminal
            .resize(Size {
                columns: 20,
                rows: 4,
            })
            .unwrap();
        assert_eq!(
            terminal.screen().size,
            Size {
                columns: 20,
                rows: 4
            }
        );
    }

    #[test]
    fn zero_dimensions_are_rejected() {
        assert_eq!(
            TerminalSurface::new(Size {
                columns: 0,
                rows: 2,
            })
            .err(),
            Some(TerminalError::InvalidSize)
        );
    }

    #[test]
    fn output_is_bounded() {
        let mut terminal = TerminalSurface::new(Size {
            columns: 10,
            rows: 2,
        })
        .unwrap();
        terminal.write_output(&vec![b'x'; MAX_OUTPUT_BYTES + 1]);
        assert_eq!(terminal.output_bytes(), MAX_OUTPUT_BYTES);
        assert!(terminal.is_truncated());
    }

    #[test]
    fn double_width_text_does_not_panic() {
        let mut terminal = TerminalSurface::new(Size {
            columns: 4,
            rows: 2,
        })
        .unwrap();
        terminal.write_output("界".as_bytes());
        assert!(!terminal.screen().content.is_empty());
    }
}
