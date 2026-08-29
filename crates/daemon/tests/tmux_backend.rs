use std::process::Command;

use anclave_protocol::{SessionId, Size};
use anclaved::backend::{CreateRequest, LocalTmuxBackend, SessionBackend};

fn tmux_available() -> bool {
    Command::new("tmux").arg("-V").output().is_ok()
}

#[test]
fn local_tmux_backend_creates_resizes_and_kills_window() {
    if !tmux_available() {
        return;
    }

    let root = std::env::temp_dir().join(format!(
        "anclave-tmux-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let socket = root.join("tmux.sock");
    let backend = LocalTmuxBackend::new(socket.to_string_lossy(), "anclave-test").with_command(
        anclaved::agent::LaunchSpec {
            program: "sh".to_owned(),
            args: Vec::new(),
        },
    );
    let id = SessionId::new("session-1").unwrap();

    backend
        .create(CreateRequest {
            session_id: id.clone(),
            name: "demo".to_owned(),
            size: Size {
                columns: 80,
                rows: 24,
            },
            launch: anclaved::agent::LaunchSpec {
                program: "sh".to_owned(),
                args: Vec::new(),
            },
        })
        .unwrap();
    assert!(!backend.capture(&id).unwrap().is_empty());
    backend
        .resize(
            &id,
            Size {
                columns: 100,
                rows: 30,
            },
        )
        .unwrap();
    backend.kill(&id).unwrap();
    assert_eq!(
        backend.kill(&id),
        Err(anclaved::backend::BackendError::NotFound)
    );

    let _ = Command::new("tmux")
        .args(["-S", socket.to_str().unwrap(), "kill-server"])
        .output();
    let _ = std::fs::remove_dir_all(root);
}

fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock is after UNIX epoch")
        .as_nanos()
}
