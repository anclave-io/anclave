use std::process::Command;

use anclave_protocol::{SessionId, Size};
use anclaved::agent::LaunchSpec;
use anclaved::backend::{CreateRequest, LocalTmuxBackend, SessionBackend};

fn tmux_available() -> bool {
    Command::new("tmux").arg("-V").output().is_ok()
}

/// Kills a test's tmux server when the test ends, however it ends.
///
/// Teardown used to be trailing statements at the bottom of each test, which
/// a failed assertion skips: the panic unwinds past them and leaves a server
/// running its agent forever. Every failing run leaked one, and a machine
/// that runs this suite while developing accumulates them silently. A guard
/// runs during unwinding, so the cleanup is tied to the value's lifetime
/// rather than to reaching the end of the happy path.
struct TmuxServer {
    socket: std::path::PathBuf,
    root: Option<std::path::PathBuf>,
}

impl Drop for TmuxServer {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .args(["-S", self.socket.to_string_lossy().as_ref(), "kill-server"])
            .output();
        if let Some(ref root) = self.root {
            let _ = std::fs::remove_dir_all(root);
        }
    }
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
    let _server = TmuxServer {
        socket: socket.clone(),
        root: Some(root.clone()),
    };
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
    // Wait for the shell to be reading before typing at it. Bytes sent while
    // it is still starting can be flushed by its own terminal setup, which
    // made this assertion fail intermittently under a loaded test run.
    let mut ready = false;
    for _ in 0..100 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if backend.capture(&id).unwrap().contains('$') {
            ready = true;
            break;
        }
    }
    assert!(ready, "the shell never produced a prompt");

    // Assert the keystrokes *arrived*. `send-keys` exits 0 for a malformed
    // argument shape, so an unchecked call passed for a version of this
    // backend that delivered nothing at all.
    backend.send_input(&id, b"echo routed-input\n").unwrap();
    let mut seen = false;
    for _ in 0..100 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if backend.capture(&id).unwrap().contains("routed-input") {
            seen = true;
            break;
        }
    }
    assert!(seen, "input sent to the pane never reached the agent");
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
    let _server = TmuxServer {
        socket: socket.clone(),
        root: None,
    };
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
}

/// Plan commit 16 requires cursor and alternate-screen restoration. Neither
/// can be recovered from captured text, so this asserts the daemon reads them
/// from tmux itself: a full-screen program must be reported as such.
#[test]
fn the_alternate_screen_and_cursor_are_read_from_tmux() {
    if !tmux_available() {
        return;
    }

    let socket = std::env::temp_dir().join(format!(
        "anclave-panestate-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    let _server = TmuxServer {
        socket: socket.clone(),
        root: None,
    };
    let backend = LocalTmuxBackend::new(socket.to_string_lossy().into_owned(), "anclave-test");
    let size = Size {
        columns: 80,
        rows: 24,
    };

    // A program that enters the alternate screen and hides the cursor, the
    // way a full-screen agent does.
    let alt = SessionId::new("session-alt").unwrap();
    backend
        .create(CreateRequest {
            session_id: alt.clone(),
            name: "alt".to_owned(),
            size,
            launch: LaunchSpec {
                program: "sh".to_owned(),
                args: vec![
                    "-c".to_owned(),
                    "printf '\\033[?1049h\\033[?25l'; sleep 30".to_owned(),
                ],
                environment: None,
            },
        })
        .expect("create the alternate-screen session");

    // A plain one, as the control.
    let plain = SessionId::new("session-plain").unwrap();
    backend
        .create(CreateRequest {
            session_id: plain.clone(),
            name: "plain".to_owned(),
            size,
            launch: LaunchSpec {
                program: "sh".to_owned(),
                args: vec!["-c".to_owned(), "sleep 30".to_owned()],
                environment: None,
            },
        })
        .expect("create the plain session");

    std::thread::sleep(std::time::Duration::from_millis(700));

    let alt_state = backend.pane_state(&alt).expect("read alt pane state");
    let plain_state = backend.pane_state(&plain).expect("read plain pane state");

    assert!(
        alt_state.alternate_screen,
        "a full-screen program was not reported as holding the alternate screen"
    );
    assert!(
        !alt_state.cursor_visible,
        "a hidden cursor was reported as visible"
    );
    assert!(
        !plain_state.alternate_screen,
        "an ordinary program was reported as holding the alternate screen"
    );
    assert!(plain_state.cursor_visible);

    let _ = backend.kill(&alt);
    let _ = backend.kill(&plain);
}
