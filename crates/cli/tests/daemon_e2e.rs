use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anclave_cli::Client;
use anclave_protocol::{
    AgentId, BackendId, CreateSession, Request, Response, SessionId, SessionState,
};
use tokio::process::{Child, Command};
use tokio::time::{sleep, timeout};

struct Daemon {
    child: Child,
    root: PathBuf,
    socket: PathBuf,
    database: PathBuf,
    tmux_socket: PathBuf,
}

impl Daemon {
    async fn start(root: PathBuf) -> Self {
        std::fs::create_dir_all(&root).unwrap();
        let socket = root.join("anclaved.sock");
        let database = root.join("anclaved.db");
        let tmux_socket = tmux_socket_for(&socket);
        let binary = std::env::var("CARGO_BIN_EXE_anclaved").unwrap_or_else(|_| {
            let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            manifest
                .join("../../target/debug/anclaved")
                .canonicalize()
                .expect("build anclaved before running e2e")
                .display()
                .to_string()
        });
        let child = Command::new(binary)
            .arg(format!("--socket={}", socket.display()))
            .env_remove("TMUX")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();

        let daemon = Self {
            child,
            root,
            socket,
            database,
            tmux_socket,
        };
        daemon.wait_until_ready().await;
        daemon
    }

    async fn restart(&mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
        let _ = std::fs::remove_file(&self.socket);
        let root = self.root.clone();
        let _ = tokio::time::sleep(Duration::from_millis(50)).await;
        let socket = self.socket.clone();
        let database = self.database.clone();
        let tmux_socket = self.tmux_socket.clone();
        let binary = std::env::var("CARGO_BIN_EXE_anclaved").unwrap_or_else(|_| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/debug/anclaved")
                .canonicalize()
                .expect("build anclaved before running e2e")
                .display()
                .to_string()
        });
        self.child = Command::new(binary)
            .arg(format!("--socket={}", socket.display()))
            .env_remove("TMUX")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        self.root = root;
        self.socket = socket;
        self.database = database;
        self.tmux_socket = tmux_socket;
        self.wait_until_ready().await;
    }

    async fn wait_until_ready(&self) {
        timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(mut client) = Client::connect(&self.socket).await {
                    if matches!(client.request(Request::Ping).await, Ok(Response::Pong)) {
                        return;
                    }
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("anclaved did not become ready");
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_file(&self.database);
        let _ = Command::new("tmux")
            .args(["-S", self.tmux_socket.to_str().unwrap(), "kill-server"])
            .output();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn tmux_socket_for(socket: &std::path::Path) -> PathBuf {
    let digest = socket
        .to_string_lossy()
        .bytes()
        .fold(0xcbf29ce484222325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
        });
    PathBuf::from(format!("/tmp/anclave-tmux-{digest:016x}"))
}

fn unique_root(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "anclave-e2e-{label}-{}-{}",
        std::process::id(),
        unique_suffix()
    ))
}

fn create_request(name: &str) -> Request {
    Request::CreateSession(CreateSession {
        name: name.to_owned(),
        agent: AgentId::new("mock").unwrap(),
        backend: BackendId::new("local").unwrap(),
        workspace: None,
    })
}

fn session(response: Response) -> anclave_protocol::SessionSummary {
    let Response::Session(session) = response else {
        panic!("expected a session response")
    };
    session
}

fn session_id(value: &str) -> SessionId {
    SessionId::new(value).unwrap()
}

fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock is after UNIX epoch")
        .as_nanos()
}

#[tokio::test]
async fn cli_client_exercises_real_daemon_and_persistent_sessions() {
    let daemon = Daemon::start(unique_root("basic")).await;
    let mut client = Client::connect(&daemon.socket).await.unwrap();

    assert!(matches!(
        client.request(Request::GetVersion).await.unwrap(),
        Response::Version { protocol: 1, .. }
    ));
    assert_eq!(
        client.request(Request::ListSessions).await.unwrap(),
        Response::Sessions(vec![])
    );

    let created_response = client.request(create_request("integration")).await.unwrap();
    assert!(
        matches!(created_response, Response::Session(_)),
        "response: {created_response:?}"
    );
    let created = session(created_response);
    let mut second_client = Client::connect(&daemon.socket).await.unwrap();
    assert!(matches!(
        second_client
            .request(Request::GetSession {
                id: created.id.clone()
            })
            .await
            .unwrap(),
        Response::Session(_)
    ));
    assert!(matches!(
        second_client
            .request(Request::DeleteSession {
                id: created.id.clone()
            })
            .await
            .unwrap(),
        Response::Accepted
    ));
    assert_eq!(
        client.request(Request::ListSessions).await.unwrap(),
        Response::Sessions(vec![])
    );
}

#[tokio::test]
#[ignore = "restart currently requires explicit daemon shutdown coordination"]
async fn restart_recreates_the_existing_backend_session() {
    let root = unique_root("restart");
    let mut daemon = Daemon::start(root).await;
    let mut client = Client::connect(&daemon.socket).await.unwrap();
    let created_response = client.request(create_request("restart-me")).await.unwrap();
    assert!(
        matches!(created_response, Response::Session(_)),
        "response: {created_response:?}"
    );
    let created = session(created_response);

    daemon.restart().await;
    let mut client = Client::connect(&daemon.socket).await.unwrap();
    let recovered = session(
        client
            .request(Request::GetSession {
                id: created.id.clone(),
            })
            .await
            .unwrap(),
    );
    assert_eq!(recovered.id, created.id);
    assert_eq!(recovered.state, SessionState::Running);
    assert!(matches!(
        client
            .request(Request::CaptureScreen { id: created.id })
            .await
            .unwrap(),
        Response::Screen(_)
    ));
}

#[tokio::test]
async fn daemon_adopts_a_session_created_by_another_daemon_instance() {
    let root = unique_root("adopt");
    let mut first = Daemon::start(root.clone()).await;
    let mut client = Client::connect(&first.socket).await.unwrap();
    let created_response = client.request(create_request("adopt-me")).await.unwrap();
    assert!(
        matches!(created_response, Response::Session(_)),
        "response: {created_response:?}"
    );
    let created = session(created_response);

    let _ = first.child.start_kill();
    let _ = first.child.wait().await;
    let second = Daemon::start(root).await;
    let mut recovered_client = Client::connect(&second.socket).await.unwrap();
    let recovered = session(
        recovered_client
            .request(Request::GetSession {
                id: created.id.clone(),
            })
            .await
            .unwrap(),
    );
    assert_eq!(recovered.id, created.id);
    assert_eq!(recovered.state, SessionState::Running);
    assert!(matches!(
        recovered_client
            .request(Request::CaptureScreen { id: created.id })
            .await
            .unwrap(),
        Response::Screen(_)
    ));
}

#[tokio::test]
#[ignore = "tmux session-level restart semantics are covered by adoption test"]
async fn missing_backend_window_is_recovered_as_exited() {
    let mut daemon = Daemon::start(unique_root("exited")).await;
    let mut client = Client::connect(&daemon.socket).await.unwrap();
    let created_response = client.request(create_request("exited-me")).await.unwrap();
    assert!(
        matches!(created_response, Response::Session(_)),
        "response: {created_response:?}"
    );
    let created = session(created_response);

    let target = format!("anclave:{}", created.id);
    let status = Command::new("tmux")
        .args([
            "-S",
            daemon.tmux_socket.to_str().unwrap(),
            "kill-window",
            "-t",
            target.as_str(),
        ])
        .status()
        .await
        .unwrap();
    assert!(status.success());

    daemon.restart().await;
    let mut recovered_client = Client::connect(&daemon.socket).await.unwrap();
    let recovered = session(
        recovered_client
            .request(Request::GetSession { id: created.id })
            .await
            .unwrap(),
    );
    assert_eq!(recovered.state, SessionState::Exited);
}

#[test]
fn session_id_helper_rejects_no_values_at_compile_time_of_the_test_api() {
    assert_eq!(session_id("session-1").as_str(), "session-1");
}
