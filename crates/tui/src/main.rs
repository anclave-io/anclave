use std::env;
use std::io::{self, stdout};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use anclave_cli::{Client, ClientError};
use anclave_protocol::{
    Event as DaemonEvent, Request, Response, ScreenSnapshot, SessionId, SessionState,
    SessionSummary, Size,
};
use tokio::time::sleep;

const DEFAULT_SOCKET: &str = "/tmp/anclaved.sock";

#[derive(Debug)]
struct App {
    sessions: Vec<SessionSummary>,
    selected: usize,
    screen: Option<ScreenSnapshot>,
    screen_session: Option<SessionId>,
    status: String,
    should_quit: bool,
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
    let socket = env::var("ANCLAVE_SOCKET").unwrap_or_else(|_| DEFAULT_SOCKET.to_owned());
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
                handle_key(&mut app, &mut client, key.code).await;
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
                app.status = format!("Disconnected: {error} — reconnecting…");
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

async fn handle_key(app: &mut App, client: &mut Option<Client>, code: KeyCode) {
    match code {
        KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
        KeyCode::Down | KeyCode::Char('j') => app.move_selection(1),
        KeyCode::Up | KeyCode::Char('k') => app.move_selection(-1),
        KeyCode::Enter => {
            if let (Some(active_client), Some(id)) = (client.as_mut(), app.selected_id()) {
                match active_client
                    .request(Request::CaptureScreen { id: id.clone() })
                    .await
                {
                    Ok(Response::Screen(screen)) => {
                        app.screen = Some(screen);
                        app.screen_session = Some(id);
                        app.status = "Connected".to_owned();
                    }
                    Ok(Response::Error { message, .. }) => app.status = message,
                    Ok(_) => app.status = "Unexpected capture response".to_owned(),
                    Err(error) => {
                        app.status = format!("Disconnected: {error}");
                        *client = None;
                    }
                }
            }
        }
        KeyCode::Char('r') => {
            *client = None;
        }
        KeyCode::Char(character) => {
            if let (Some(active_client), Some(id)) = (client.as_mut(), app.selected_id()) {
                if let Err(error) = active_client
                    .request(Request::SendInput {
                        id,
                        bytes: character.to_string().into_bytes(),
                    })
                    .await
                {
                    app.status = format!("Disconnected: {error}");
                    *client = None;
                }
            }
        }
        _ => {}
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
                    app.status = format!("Disconnected: {error} — press r to reconnect");
                    None
                }
            },
            Ok(_) => None,
            Err(error) => {
                app.status = format!("Disconnected: {error} — press r to reconnect");
                None
            }
        },
        Err(error) => {
            app.status = format!("Disconnected: {error} — press r to reconnect");
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
    let layout = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
        .split(frame.area());

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

    let content = app
        .screen
        .as_ref()
        .map(|screen| screen.content.as_str())
        .unwrap_or("Select a session and press Enter to capture its terminal.");
    let terminal = Paragraph::new(content)
        .block(Block::default().title(" Terminal ").borders(Borders::ALL))
        .wrap(Wrap { trim: false });
    frame.render_widget(terminal, layout[1]);

    let status = Paragraph::new(app.status.as_str())
        .block(Block::default().title(" Status ").borders(Borders::ALL));
    let status_area = ratatui::layout::Rect {
        x: 0,
        y: frame.area().height.saturating_sub(1),
        width: frame.area().width,
        height: 1.min(frame.area().height),
    };
    frame.render_widget(status, status_area);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_stays_in_bounds_when_sessions_change() {
        let mut app = App::new();
        app.sessions = vec![SessionSummary {
            id: SessionId::new("session-1").unwrap(),
            name: "demo".to_owned(),
            state: SessionState::Running,
            agent: anclave_protocol::AgentId::new("default").unwrap(),
            workspace: None,
        }];
        app.selected = 1;
        app.update_sessions(Vec::new());
        assert_eq!(app.selected, 0);
        assert!(app.selected_id().is_none());
    }
}
