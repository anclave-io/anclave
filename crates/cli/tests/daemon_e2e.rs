use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anclave_cli::Client;
use anclave_protocol::{Request, Response};
use tokio::process::{Child, Command};
use tokio::time::{sleep, timeout};

struct Daemon {
    child: Child,
    socket: PathBuf,
    database: PathBuf,
}

impl Daemon {
    async fn start() -> Self {
        let root = std::env::temp_dir().join(format!(
            "anclave-e2e-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let socket = root.join("anclaved.sock");
        let database = root.join("anclaved.db");
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
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();

        let daemon = Self {
            child,
            socket,
            database,
        };
        daemon.wait_until_ready().await;
        daemon
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
        if let Some(parent) = self.socket.parent() {
            let _ = std::fs::remove_dir(parent);
        }
    }
}

fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock is after UNIX epoch")
        .as_nanos()
}

#[tokio::test]
async fn cli_client_exercises_real_daemon_and_persistent_sessions() {
    let daemon = Daemon::start().await;
    let mut client = Client::connect(&daemon.socket).await.unwrap();

    assert!(matches!(
        client.request(Request::GetVersion).await.unwrap(),
        Response::Version { protocol: 1, .. }
    ));
    assert_eq!(
        client.request(Request::ListSessions).await.unwrap(),
        Response::Sessions(vec![])
    );

    let created = client
        .request(Request::CreateSession(anclave_protocol::CreateSession {
            name: "integration".to_owned(),
            agent: anclave_protocol::AgentId::new("mock").unwrap(),
            backend: anclave_protocol::BackendId::new("local").unwrap(),
            workspace: None,
        }))
        .await
        .unwrap();
    let Response::Session(session) = created else {
        panic!("expected a created session")
    };

    let mut second_client = Client::connect(&daemon.socket).await.unwrap();
    assert!(matches!(
        second_client
            .request(Request::GetSession {
                id: session.id.clone()
            })
            .await
            .unwrap(),
        Response::Session(_)
    ));
    assert!(matches!(
        second_client
            .request(Request::DeleteSession {
                id: session.id.clone()
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
