use std::process::Command;

use anclave_protocol::{SessionId, Size};
use anclaved::agent::LaunchSpec;
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
            environment: None,
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
                environment: None,
            },
        })
        .unwrap();
    assert!(backend.adopt(&id).is_ok());
    assert!(!backend.capture(&id).unwrap().is_empty());
    backend.send_input(&id, b"echo routed-input\n").unwrap();
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

/// A daemon must be able to host more than one session.
///
/// It could not: `create` always ran `new-session`, so the second session
/// ever created failed with "duplicate session". No unit test caught it
/// because the fake backend has no concept of a shared tmux session: this
/// needs real tmux.
#[test]
fn a_second_session_gets_its_own_window_rather_than_failing() {
    if !tmux_available() {
        return;
    }

    let socket = std::env::temp_dir().join(format!(
        "anclave-multi-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    let backend = LocalTmuxBackend::new(socket.to_string_lossy().into_owned(), "anclave-test");

    let first = SessionId::new("session-first").unwrap();
    let second = SessionId::new("session-second").unwrap();
    let size = Size {
        columns: 80,
        rows: 24,
    };

    backend
        .create(CreateRequest {
            session_id: first.clone(),
            name: "first".to_owned(),
            size,
            launch: LaunchSpec {
                program: "sh".to_owned(),
                args: vec!["-c".to_owned(), "sleep 30".to_owned()],
                environment: None,
            },
        })
        .expect("the first session must be created");

    backend
        .create(CreateRequest {
            session_id: second.clone(),
            name: "second".to_owned(),
            size,
            launch: LaunchSpec {
                program: "sh".to_owned(),
                args: vec!["-c".to_owned(), "sleep 30".to_owned()],
                environment: None,
            },
        })
        .expect("the second session must not collide with the first");

    let live = backend.sessions().expect("sessions are listable");
    assert!(live.contains(&first), "first session missing: {live:?}");
    assert!(live.contains(&second), "second session missing: {live:?}");

    let _ = backend.kill(&first);
    let _ = backend.kill(&second);
    let _ = Command::new("tmux")
        .args(["-S", socket.to_str().unwrap(), "kill-server"])
        .output();
}
