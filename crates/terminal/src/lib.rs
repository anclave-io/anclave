use anclave_protocol::{Color, Cursor, ScreenSnapshot, Size, Span, Style};

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

    /// Resize in place, keeping what is on screen.
    ///
    /// Replacing the parser: which is what this used to do: discards the
    /// screen, so resizing a window blanked the agent's output. vt100 can
    /// reflow, so the content survives.
    pub fn resize(&mut self, size: Size) -> Result<(), TerminalError> {
        size.validate().map_err(|_| TerminalError::InvalidSize)?;
        self.parser.screen_mut().set_size(size.rows, size.columns);
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
        let (rows, columns) = (self.size.rows, self.size.columns);
        let (cursor_row, cursor_column) = screen.cursor_position();

        ScreenSnapshot {
            size: self.size,
            rows: (0..rows)
                .map(|row| row_spans(screen, row, columns))
                .collect(),
            cursor: Cursor {
                row: cursor_row,
                column: cursor_column,
                visible: !screen.hide_cursor(),
            },
            alternate_screen: screen.alternate_screen(),
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
mod fidelity_tests {
    use super::*;

    fn surface(bytes: &[u8]) -> ScreenSnapshot {
        let mut s = TerminalSurface::new(Size {
            columns: 20,
            rows: 4,
        })
        .unwrap();
        s.write_output(bytes);
        s.screen()
    }

    /// The point of the whole change: color survives to the client.
    #[test]
    fn color_reaches_the_snapshot() {
        // "red" in SGR 31, then default.
        let screen = surface(b"\x1b[31mred\x1b[0m plain");
        let spans = &screen.rows[0];
        let colored = spans
            .iter()
            .find(|s| s.text.starts_with("red"))
            .expect("the colored run is its own span");
        assert_eq!(colored.style.foreground, Color::Indexed(1));
        assert!(spans.iter().any(|s| s.style.is_plain()));
    }

    #[test]
    fn attributes_survive() {
        let screen = surface(b"\x1b[1mbold\x1b[0m\x1b[4munder\x1b[0m");
        let spans = &screen.rows[0];
        assert!(spans
            .iter()
            .any(|s| s.text.starts_with("bold") && s.style.bold));
        assert!(spans
            .iter()
            .any(|s| s.text.starts_with("under") && s.style.underline));
    }

    /// A plain row must not cost one span per column.
    #[test]
    fn an_unstyled_row_is_a_single_span() {
        let screen = surface(b"hello");
        assert_eq!(screen.rows[0].len(), 1);
        assert_eq!(screen.rows[0][0].text.trim_end(), "hello");
    }

    #[test]
    fn the_grid_keeps_its_shape() {
        let screen = surface(b"a\r\nb\r\nc");
        assert_eq!(screen.rows.len(), 4, "one entry per row, always");
        assert_eq!(screen.rows[0][0].text.trim_end(), "a");
        assert_eq!(screen.rows[1][0].text.trim_end(), "b");
        assert_eq!(screen.rows[2][0].text.trim_end(), "c");
    }

    #[test]
    fn the_cursor_is_reported_and_can_be_hidden() {
        let screen = surface(b"abc");
        assert_eq!(screen.cursor.row, 0);
        assert_eq!(screen.cursor.column, 3);
        assert!(screen.cursor.visible);

        let hidden = surface(b"abc\x1b[?25l");
        assert!(!hidden.cursor.visible, "a hidden cursor must not be drawn");
    }

    /// Restoring a session needs to know a full-screen program is running.
    #[test]
    fn the_alternate_screen_flag_is_carried() {
        assert!(!surface(b"plain").alternate_screen);
        assert!(surface(b"\x1b[?1049h").alternate_screen);
    }

    /// A wide glyph occupies two columns; emitting the continuation cell
    /// would duplicate it and shift the rest of the row.
    #[test]
    fn a_wide_character_is_not_duplicated() {
        let screen = surface("日本".as_bytes());
        let line: String = screen.rows[0].iter().map(|s| s.text.as_str()).collect();
        assert!(line.starts_with("日本"), "got {line:?}");
        assert_eq!(line.matches('日').count(), 1);
    }

    /// Resizing used to rebuild the parser, which blanked the screen.
    #[test]
    fn resizing_keeps_what_is_on_screen() {
        let mut s = TerminalSurface::new(Size {
            columns: 20,
            rows: 4,
        })
        .unwrap();
        s.write_output(b"persistent");
        s.resize(Size {
            columns: 40,
            rows: 8,
        })
        .unwrap();

        let screen = s.screen();
        assert_eq!(screen.size.columns, 40);
        assert_eq!(screen.rows.len(), 8);
        assert!(
            screen.to_text().contains("persistent"),
            "resize lost the screen: {:?}",
            screen.to_text()
        );
    }

    #[test]
    fn to_text_is_still_available_for_logs_and_capture() {
        let screen = surface(b"\x1b[31mred\x1b[0m");
        assert_eq!(screen.to_text().lines().next(), Some("red"));
    }
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
        assert!(screen.to_text().starts_with("hello"));
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
        assert!(terminal.screen().to_text().contains("  ok"));
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
        assert!(!terminal.screen().to_text().is_empty());
    }
}

/// Collapse one row of cells into runs of identical style.
///
/// A plain row becomes a single span, which is what keeps a full screen from
/// costing one JSON object per character.
fn row_spans(screen: &vt100::Screen, row: u16, columns: u16) -> Vec<Span> {
    let mut spans: Vec<Span> = Vec::new();

    for column in 0..columns {
        let Some(cell) = screen.cell(row, column) else {
            continue;
        };
        // A wide character occupies two columns; the second reports itself as
        // a continuation and carries no text. Emitting it would duplicate the
        // glyph and push the rest of the row right.
        if cell.is_wide_continuation() {
            continue;
        }
        let style = cell_style(cell);
        let text = if cell.has_contents() {
            cell.contents().to_owned()
        } else {
            " ".to_owned()
        };

        match spans.last_mut() {
            Some(last) if last.style == style => last.text.push_str(&text),
            _ => spans.push(Span { text, style }),
        }
    }

    spans
}

fn cell_style(cell: &vt100::Cell) -> Style {
    Style {
        foreground: convert_color(cell.fgcolor()),
        background: convert_color(cell.bgcolor()),
        bold: cell.bold(),
        italic: cell.italic(),
        underline: cell.underline(),
        inverse: cell.inverse(),
    }
}

fn convert_color(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Default,
        vt100::Color::Idx(index) => Color::Indexed(index),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}
