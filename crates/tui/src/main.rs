use std::env;
use std::io::{self, stdout};
use std::time::Duration;

use anclave_cli::{Client, ClientError};
use anclave_protocol::{
    Event as DaemonEvent, Request, Response, ScreenSnapshot, SessionId, SessionState,
    SessionSummary, Size,
};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Margin;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color as RColor, Modifier, Style as RStyle};
use ratatui::text::{Line, Span as RSpan};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use tokio::time::sleep;

const DEFAULT_SOCKET: &str = "/tmp/anclaved.sock";

const USAGE: &str = r"anclave: terminal client for the anclave daemon

USAGE
  anclave [OPTIONS]

OPTIONS
  --socket PATH     daemon socket (default /tmp/anclaved.sock)
  --help, -h        print this and exit
  --version, -V     print the version and exit

KEYS
  j / k, up / down  move between sessions
  enter             show the selected session's screen
  r                 reconnect to the daemon
  q / esc           quit

Quitting leaves every session running: the daemon owns them, not this client.

ENVIRONMENT
  ANCLAVE_SOCKET    daemon socket, overridden by --socket";

// ---------------------------------------------------------------------------
// Key encoding
// ---------------------------------------------------------------------------

/// Where keystrokes go.
///
/// Without this split there is one namespace for navigating and for typing,
/// so `q` quits instead of typing `q` and Enter re-captures instead of running
/// a command. That made the client able to watch an agent but not drive one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Keys drive the client: move between sessions, focus one, quit.
    Navigate,
    /// Keys go to the agent. Everything except the one way out.
    Terminal,
}

/// The byte that leaves Terminal mode: `Ctrl+]`, the classic telnet escape.
///
/// Matched as a *byte* rather than as a key, because terminals report the
/// chord inconsistently: crossterm calls it `Ctrl+5` on this machine, since
/// 0x1d has both spellings. Comparing the encoded byte recognises every
/// spelling of the same chord.
///
/// `Esc` would be the wrong choice: agents use it constantly, to leave insert
/// mode or interrupt a turn, so taking it would make the client unable to
/// drive the programs it exists to drive.
const ESCAPE_BYTE: u8 = 0x1d;
const ESCAPE_HINT: &str = "Ctrl+]";

/// Translate a key press into the bytes a terminal would send.
///
/// Returns `None` for a key with no representation, which is left unsent
/// rather than guessed at: inventing bytes for an unknown key puts noise into
/// the agent's input that nobody typed.
fn encode_key(code: KeyCode, modifiers: KeyModifiers) -> Option<Vec<u8>> {
    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    let alt = modifiers.contains(KeyModifiers::ALT);

    let base: Vec<u8> = match code {
        KeyCode::Char(c) if ctrl => {
            // Control codes: Ctrl+A is 0x01 through Ctrl+Z at 0x1a, then the
            // handful above it that terminals also send.
            let byte = match c.to_ascii_lowercase() {
                c @ 'a'..='z' => (c as u8) - b'a' + 1,
                '@' | ' ' | '2' => 0,
                // Each of these control codes has two historical spellings,
                // and terminals disagree about which they report. Ctrl+] and
                // Ctrl+5 are the same byte; a client that recognised only one
                // would miss the chord on half of them.
                '[' | '3' => 0x1b,
                '\\' | '4' => 0x1c,
                ']' | '5' => 0x1d,
                '^' | '6' => 0x1e,
                '_' | '?' | '7' => 0x1f,
                _ => return None,
            };
            vec![byte]
        }
        KeyCode::Char(c) => c.to_string().into_bytes(),
        // A terminal sends CR for Enter, not LF. Sending LF leaves many
        // programs waiting for a line that never ends.
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        // DEL, not BS: this is what terminals actually send for backspace,
        // and the difference is why a misconfigured one erases nothing.
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::F(n @ 1..=4) => vec![0x1b, b'O', b'P' + (n - 1)],
        KeyCode::F(n @ 5..=12) => {
            // The historical numbering has gaps; these are the codes xterm
            // sends rather than a tidy sequence.
            let code = match n {
                5 => 15,
                6..=10 => n + 11,
                11 => 23,
                _ => 24,
            };
            format!("\x1b[{code}~").into_bytes()
        }
        _ => return None,
    };

    // Alt is an ESC prefix, which is how terminals have always sent it.
    if alt {
        let mut out = vec![0x1b];
        out.extend(base);
        return Some(out);
    }
    Some(base)
}

#[derive(Debug)]
struct App {
    sessions: Vec<SessionSummary>,
    selected: usize,
    screen: Option<ScreenSnapshot>,
    screen_session: Option<SessionId>,
    status: String,
    should_quit: bool,
    mode: Mode,
}

impl App {
    fn new() -> Self {
        Self {
            sessions: Vec::new(),
            selected: 0,
            screen: None,
            screen_session: None,
            status: "Connecting to anclaved…".to_owned(),
            should_quit: false,
            mode: Mode::Navigate,
        }
    }

    fn selected_id(&self) -> Option<SessionId> {
        self.sessions
            .get(self.selected)
            .map(|session| session.id.clone())
    }

    fn update_sessions(&mut self, sessions: Vec<SessionSummary>) {
        let selected_id = self.selected_id();
        self.sessions = sessions;
        self.selected = selected_id
            .and_then(|id| self.sessions.iter().position(|session| session.id == id))
            .unwrap_or_else(|| self.selected.min(self.sessions.len().saturating_sub(1)));
        if self.sessions.is_empty() {
            self.screen = None;
            self.screen_session = None;
        }
    }

    fn move_selection(&mut self, offset: isize) {
        if self.sessions.is_empty() {
            return;
        }
        let last = self.sessions.len() - 1;
        self.selected = self.selected.saturating_add_signed(offset).min(last);
        if self.screen_session != self.selected_id() {
            self.screen = None;
        }
    }

    fn apply_event(&mut self, event: DaemonEvent) {
        match event {
            DaemonEvent::ScreenChanged { id } if self.screen_session.as_ref() == Some(&id) => {
                self.status = "Connected".to_owned();
            }
            DaemonEvent::SessionStateChanged { session } => {
                if let Some(existing) = self
                    .sessions
                    .iter_mut()
                    .find(|existing| existing.id == session.id)
                {
                    *existing = session;
                }
            }
            DaemonEvent::SessionCreated { session } => self.sessions.push(session),
            DaemonEvent::SessionExited { id, .. } => {
                if let Some(session) = self.sessions.iter_mut().find(|session| session.id == id) {
                    session.state = SessionState::Exited;
                }
            }
            _ => {}
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Argument handling comes before the terminal is taken. `anclave --help`
    // used to enter the alternate screen, draw nothing and leave, which reads
    // as the program being broken.
    let mut socket = env::var("ANCLAVE_SOCKET").unwrap_or_else(|_| DEFAULT_SOCKET.to_owned());
    let mut arguments = env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--help" | "-h" => {
                println!("{USAGE}");
                return Ok(());
            }
            "--version" | "-V" => {
                println!("anclave {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--socket" => {
                socket = arguments.next().ok_or("--socket requires a path")?;
            }
            other => match other.strip_prefix("--socket=") {
                Some(value) if !value.is_empty() => socket = value.to_owned(),
                _ => {
                    eprintln!("anclave: unknown argument: {other}");
                    eprintln!("run `anclave --help` for usage.");
                    std::process::exit(2);
                }
            },
        }
    }

    let mut terminal = setup_terminal()?;
    let result = run(&mut terminal, socket).await;
    restore_terminal(&mut terminal)?;
    result
}

async fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    socket: String,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut app = App::new();
    let mut client = connect(&socket, &mut app).await;
    let mut last_size = terminal.size()?;

    while !app.should_quit {
        terminal.draw(|frame| draw(frame, &app))?;
        if let Event::Key(key) = next_input()? {
            if key.kind == KeyEventKind::Press {
                handle_key(&mut app, &mut client, key.code, key.modifiers).await;
            }
        }
        let size = terminal.size()?;
        if size != last_size {
            last_size = size;
            if let (Some(active_client), Some(id)) = (client.as_mut(), app.selected_id()) {
                let _ = active_client
                    .request(Request::ResizeSession {
                        id,
                        size: Size {
                            columns: size.width,
                            rows: size.height.saturating_sub(1),
                        },
                    })
                    .await;
            }
        }
        if let Some(active_client) = client.as_mut() {
            if let Err(error) = drain_live_client(active_client, &mut app).await {
                app.status = format!("Disconnected: {error}: reconnecting…");
                client = None;
            }
        } else {
            client = connect(&socket, &mut app).await;
            sleep(Duration::from_millis(250)).await;
        }
    }
    Ok(())
}

fn next_input() -> io::Result<Event> {
    if event::poll(Duration::from_millis(100))? {
        event::read()
    } else {
        Ok(Event::Resize(0, 0))
    }
}

async fn handle_key(
    app: &mut App,
    client: &mut Option<Client>,
    code: KeyCode,
    modifiers: KeyModifiers,
) {
    match app.mode {
        Mode::Terminal => handle_terminal_key(app, client, code, modifiers).await,
        Mode::Navigate => handle_navigate_key(app, client, code).await,
    }
}

/// In Terminal mode every key belongs to the agent except the one way out.
///
/// No other chord is reserved. A client that kept `q` or Enter for itself
/// could not drive the programs it exists to drive.
async fn handle_terminal_key(
    app: &mut App,
    client: &mut Option<Client>,
    code: KeyCode,
    modifiers: KeyModifiers,
) {
    if encode_key(code, modifiers).as_deref() == Some([ESCAPE_BYTE].as_slice()) {
        app.mode = Mode::Navigate;
        app.status = "Navigate: j/k move, enter focuses, q quits".to_owned();
        return;
    }

    let Some(bytes) = encode_key(code, modifiers) else {
        return;
    };
    let (Some(active_client), Some(id)) = (client.as_mut(), app.screen_session.clone()) else {
        return;
    };
    if let Err(error) = active_client
        .request(Request::SendInput { id, bytes })
        .await
    {
        app.status = format!("Disconnected: {error}");
        *client = None;
    }
}

async fn handle_navigate_key(app: &mut App, client: &mut Option<Client>, code: KeyCode) {
    match code {
        KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
        KeyCode::Down | KeyCode::Char('j') => app.move_selection(1),
        KeyCode::Up | KeyCode::Char('k') => app.move_selection(-1),
        KeyCode::Char('r') => *client = None,
        KeyCode::Enter => focus_selected(app, client).await,
        _ => {}
    }
}

/// Attach to the selected session and hand it the keyboard.
async fn focus_selected(app: &mut App, client: &mut Option<Client>) {
    let (Some(active_client), Some(id)) = (client.as_mut(), app.selected_id()) else {
        return;
    };
    match active_client
        .request(Request::AttachSession { id: id.clone() })
        .await
    {
        Ok(Response::Screen(screen)) => {
            app.screen = Some(screen);
            app.screen_session = Some(id);
            app.mode = Mode::Terminal;
            app.status = format!("Terminal: keys go to the agent, {ESCAPE_HINT} to leave");
        }
        Ok(Response::Error { message, .. }) => app.status = message,
        Ok(_) => app.status = "Unexpected response".to_owned(),
        Err(error) => {
            app.status = format!("Disconnected: {error}");
            *client = None;
        }
    }
}

async fn connect(socket: &str, app: &mut App) -> Option<Client> {
    match Client::connect(socket).await {
        Ok(mut client) => match client.subscribe().await {
            Ok(Response::Subscribed) => match client.request(Request::ListSessions).await {
                Ok(Response::Sessions(sessions)) => {
                    app.update_sessions(sessions);
                    app.status = "Connected".to_owned();
                    Some(client)
                }
                Ok(_) => None,
                Err(error) => {
                    app.status = format!("Disconnected: {error}: press r to reconnect");
                    None
                }
            },
            Ok(_) => None,
            Err(error) => {
                app.status = format!("Disconnected: {error}: press r to reconnect");
                None
            }
        },
        Err(error) => {
            app.status = format!("Disconnected: {error}: press r to reconnect");
            None
        }
    }
}

async fn drain_live_client(client: &mut Client, app: &mut App) -> Result<(), ClientError> {
    while let Ok(event_result) =
        tokio::time::timeout(Duration::from_millis(1), client.next_event()).await
    {
        let event = event_result?;
        app.apply_event(event.clone());
        if let DaemonEvent::ScreenChanged { id } = event {
            if app.screen_session.as_ref() == Some(&id) {
                if let Ok(Response::Screen(screen)) = client
                    .request(Request::CaptureScreen { id: id.clone() })
                    .await
                {
                    app.screen = Some(screen);
                }
            }
        }
    }
    Ok(())
}

fn draw(frame: &mut Frame<'_>, app: &App) {
    // Reserve the bottom row for the status bar rather than drawing the panes
    // full height and painting over them.
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(frame.area());
    let layout = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
        .split(outer[0]);

    let items = app.sessions.iter().map(|session| {
        let marker = match session.state {
            SessionState::Running => "●",
            SessionState::Exited => "×",
            SessionState::Unreachable => "!",
            _ => "…",
        };
        ListItem::new(format!("{marker} {}", session.name))
    });
    let list = List::new(items)
        .block(Block::default().title(" Sessions ").borders(Borders::ALL))
        .highlight_symbol("▸ ");
    let mut state = ratatui::widgets::ListState::default();
    if !app.sessions.is_empty() {
        state.select(Some(app.selected));
    }
    frame.render_stateful_widget(list, layout[0], &mut state);

    let block = Block::default().title(" Terminal ").borders(Borders::ALL);
    match app.screen.as_ref() {
        // A terminal grid is rendered row for row with wrapping *off*. The
        // screen already has a width; letting the widget re-wrap it is what
        // turned a full-screen agent into scrambled text.
        Some(screen) => {
            let lines: Vec<Line> = screen
                .rows
                .iter()
                .map(|row| {
                    Line::from(
                        row.iter()
                            .map(|span| {
                                RSpan::styled(span.text.clone(), convert_style(&span.style))
                            })
                            .collect::<Vec<_>>(),
                    )
                })
                .collect();
            frame.render_widget(Paragraph::new(lines).block(block), layout[1]);

            // Put the real cursor where the agent put it, unless the program
            // hid it.
            if screen.cursor.visible {
                let inner = layout[1].inner(Margin {
                    horizontal: 1,
                    vertical: 1,
                });
                let x = inner.x + screen.cursor.column.min(inner.width.saturating_sub(1));
                let y = inner.y + screen.cursor.row.min(inner.height.saturating_sub(1));
                frame.set_cursor_position((x, y));
            }
        }
        None => frame.render_widget(
            Paragraph::new("Select a session and press Enter to type into it.")
                .block(block)
                .wrap(Wrap { trim: false }),
            layout[1],
        ),
    }

    // No border. The status area is one row tall, and a bordered block in one
    // row draws only its top edge, so the text was never visible: the
    // connection state the user most needs when something is wrong was the
    // one thing the UI could not show.
    // The mode is the one thing that changes what every other key does, so it
    // is shown rather than inferred: a user who cannot tell which mode they
    // are in cannot predict what typing will do.
    let (tag, tag_style) = match app.mode {
        Mode::Terminal => (
            " TERMINAL ",
            RStyle::default().fg(RColor::Black).bg(RColor::Green),
        ),
        Mode::Navigate => (
            " NAVIGATE ",
            RStyle::default().fg(RColor::Black).bg(RColor::Cyan),
        ),
    };
    let status = Paragraph::new(Line::from(vec![
        RSpan::styled(tag, tag_style),
        RSpan::raw(" "),
        RSpan::raw(app.status.as_str()),
    ]))
    .style(RStyle::default().fg(RColor::Black).bg(RColor::Gray));
    frame.render_widget(status, outer[1]);
}

fn setup_terminal() -> io::Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    let mut output = stdout();
    execute!(output, EnterAlternateScreen)?;
    Terminal::new(CrosstermBackend::new(output))
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()
}

/// Translate a protocol style into ratatui's.
///
/// Palette indices stay indices: resolving them to RGB here would override
/// the viewer's own terminal theme, so "red" would stop meaning whatever
/// their terminal calls red.
fn convert_style(style: &anclave_protocol::Style) -> RStyle {
    let mut out = RStyle::default();
    if let Some(color) = convert_color(style.foreground) {
        out = out.fg(color);
    }
    if let Some(color) = convert_color(style.background) {
        out = out.bg(color);
    }
    let mut modifiers = Modifier::empty();
    if style.bold {
        modifiers |= Modifier::BOLD;
    }
    if style.italic {
        modifiers |= Modifier::ITALIC;
    }
    if style.underline {
        modifiers |= Modifier::UNDERLINED;
    }
    if style.inverse {
        modifiers |= Modifier::REVERSED;
    }
    out.add_modifier(modifiers)
}

fn convert_color(color: anclave_protocol::Color) -> Option<RColor> {
    match color {
        anclave_protocol::Color::Default => None,
        anclave_protocol::Color::Indexed(index) => Some(RColor::Indexed(index)),
        anclave_protocol::Color::Rgb(r, g, b) => Some(RColor::Rgb(r, g, b)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(code: KeyCode) -> Option<Vec<u8>> {
        encode_key(code, KeyModifiers::NONE)
    }

    /// Enter must be CR. Sending LF leaves many programs waiting for a line
    /// that never ends, which looks like the agent ignoring you.
    #[test]
    fn enter_is_a_carriage_return() {
        assert_eq!(plain(KeyCode::Enter), Some(vec![b'\r']));
    }

    /// DEL, not BS. The difference is why a misconfigured terminal erases
    /// nothing when you press backspace.
    #[test]
    fn backspace_is_delete() {
        assert_eq!(plain(KeyCode::Backspace), Some(vec![0x7f]));
    }

    #[test]
    fn arrows_and_navigation_use_their_escape_sequences() {
        assert_eq!(plain(KeyCode::Up), Some(b"\x1b[A".to_vec()));
        assert_eq!(plain(KeyCode::Down), Some(b"\x1b[B".to_vec()));
        assert_eq!(plain(KeyCode::Right), Some(b"\x1b[C".to_vec()));
        assert_eq!(plain(KeyCode::Left), Some(b"\x1b[D".to_vec()));
        assert_eq!(plain(KeyCode::PageUp), Some(b"\x1b[5~".to_vec()));
        assert_eq!(plain(KeyCode::Delete), Some(b"\x1b[3~".to_vec()));
    }

    /// Ctrl+C is how you interrupt an agent, so it has to reach it.
    #[test]
    fn control_chords_become_control_codes() {
        let ctrl = KeyModifiers::CONTROL;
        assert_eq!(encode_key(KeyCode::Char('c'), ctrl), Some(vec![0x03]));
        assert_eq!(encode_key(KeyCode::Char('d'), ctrl), Some(vec![0x04]));
        assert_eq!(encode_key(KeyCode::Char('a'), ctrl), Some(vec![0x01]));
        assert_eq!(encode_key(KeyCode::Char('z'), ctrl), Some(vec![0x1a]));
        // Case must not matter: a shifted chord is the same control code.
        assert_eq!(encode_key(KeyCode::Char('C'), ctrl), Some(vec![0x03]));
    }

    /// The escape chord has two historical spellings and terminals disagree
    /// about which they report: crossterm calls it Ctrl+5 here. Both must
    /// produce the same byte, or the way out of Terminal mode works on some
    /// machines and not others.
    #[test]
    fn both_spellings_of_the_escape_chord_are_the_same_byte() {
        let ctrl = KeyModifiers::CONTROL;
        assert_eq!(encode_key(KeyCode::Char(']'), ctrl), Some(vec![0x1d]));
        assert_eq!(encode_key(KeyCode::Char('5'), ctrl), Some(vec![0x1d]));
        assert_eq!(encode_key(KeyCode::Char('['), ctrl), Some(vec![0x1b]));
        assert_eq!(encode_key(KeyCode::Char('3'), ctrl), Some(vec![0x1b]));
    }

    #[test]
    fn alt_is_an_escape_prefix() {
        assert_eq!(
            encode_key(KeyCode::Char('b'), KeyModifiers::ALT),
            Some(vec![0x1b, b'b'])
        );
    }

    /// Agents use Esc constantly, to leave insert mode or interrupt a turn.
    /// It must reach them rather than being eaten by the client.
    #[test]
    fn escape_reaches_the_agent() {
        assert_eq!(plain(KeyCode::Esc), Some(vec![0x1b]));
    }

    /// Ordinary characters, including the ones the old keymap had stolen for
    /// navigation. Not being able to type "quirk" is not a usable client.
    #[test]
    fn letters_the_navigation_keys_used_to_steal_are_typable() {
        for c in ['q', 'j', 'k', 'r'] {
            assert_eq!(plain(KeyCode::Char(c)), Some(vec![c as u8]));
        }
    }

    #[test]
    fn multibyte_characters_survive() {
        assert_eq!(plain(KeyCode::Char('é')), Some("é".as_bytes().to_vec()));
    }

    /// A key with no representation is left unsent. Inventing bytes for it
    /// would put input into the agent that nobody typed.
    #[test]
    fn an_unrepresentable_key_sends_nothing() {
        assert_eq!(plain(KeyCode::CapsLock), None);
        assert_eq!(encode_key(KeyCode::Char('€'), KeyModifiers::CONTROL), None);
    }

    #[test]
    fn function_keys_use_their_historical_codes() {
        assert_eq!(plain(KeyCode::F(1)), Some(b"\x1bOP".to_vec()));
        assert_eq!(plain(KeyCode::F(4)), Some(b"\x1bOS".to_vec()));
        assert_eq!(plain(KeyCode::F(5)), Some(b"\x1b[15~".to_vec()));
        assert_eq!(plain(KeyCode::F(12)), Some(b"\x1b[24~".to_vec()));
    }

    #[test]
    fn selection_stays_in_bounds_when_sessions_change() {
        let mut app = App::new();
        app.sessions = vec![SessionSummary {
            id: SessionId::new("session-1").unwrap(),
            name: "demo".to_owned(),
            state: SessionState::Running,
            agent: anclave_protocol::AgentId::new("default").unwrap(),
            workspace: None,
            security: Default::default(),
        }];
        app.selected = 1;
        app.update_sessions(Vec::new());
        assert_eq!(app.selected, 0);
        assert!(app.selected_id().is_none());
    }
}
