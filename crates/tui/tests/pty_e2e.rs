//! The TUI driven through a real pty, with its output parsed by a real VT
//! parser.
//!
//! "Test every terminal emulator" is not automatable: emulators are GUI
//! programs and CI has no display. What *is* testable is the half that
//! actually differs between them. An emulator is a pty on one side and a VT
//! parser on the other, so putting the TUI between those two and asserting
//! the resulting screen exercises exactly what any emulator would do with it.
//!
//! What that catches: escape sequences the TUI emits, whether the alternate
//! screen is entered and left, cursor placement, wide-character handling,
//! behavior at small sizes, and whether the terminal is restored on exit. A
//! program that leaves the alternate screen on, or the cursor hidden, breaks
//! the user's shell after it quits, and no unit test sees that.
//!
//! What it does not catch: font rendering, GPU quirks, and how a specific
//! emulator encodes a specific key chord. Those need a human with that
//! emulator.

#![cfg(unix)]

use std::io::{Read, Write};
use std::os::unix::io::FromRawFd;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// A pty with a child running on the far side.
struct Pty {
    master: std::fs::File,
    child: std::process::Child,
    parser: vt100::Parser,
    /// Every byte the program wrote, kept so tests can assert on the escape
    /// sequences themselves. The parsed screen cannot show whether the
    /// alternate screen was left on exit; only the raw stream can.
    raw: Vec<u8>,
}

impl Pty {
    /// Spawn `command` attached to a new pty of the given size.
    fn spawn(mut command: Command, rows: u16, columns: u16) -> Self {
        let mut master_fd = 0;
        let mut slave_fd = 0;
        let size = libc::winsize {
            ws_row: rows,
            ws_col: columns,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };

        // SAFETY: both fds are written by openpty and checked below; the
        // winsize is fully initialised above.
        let rc = unsafe {
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &size as *const libc::winsize as *mut libc::winsize,
            )
        };
        assert_eq!(rc, 0, "openpty failed");

        // SAFETY: openpty returned success, so both descriptors are valid and
        // owned by us.
        let master = unsafe { std::fs::File::from_raw_fd(master_fd) };
        let slave_in = unsafe { std::fs::File::from_raw_fd(slave_fd) };
        let slave_out = slave_in.try_clone().unwrap();
        let slave_err = slave_in.try_clone().unwrap();

        let child = command
            .stdin(Stdio::from(slave_in))
            .stdout(Stdio::from(slave_out))
            .stderr(Stdio::from(slave_err))
            .spawn()
            .expect("spawn under pty");

        Self {
            master,
            child,
            parser: vt100::Parser::new(rows, columns, 0),
            raw: Vec::new(),
        }
    }

    /// Read whatever is available, feeding it to the parser.
    fn pump(&mut self, duration: Duration) {
        let deadline = Instant::now() + duration;
        set_nonblocking(&self.master);
        let mut buffer = [0u8; 8192];
        while Instant::now() < deadline {
            match self.master.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    self.raw.extend_from_slice(&buffer[..n]);
                    self.parser.process(&buffer[..n]);
                }
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    }

    /// Everything the program has written so far, as bytes.
    fn raw_output(&self) -> &[u8] {
        &self.raw
    }

    fn contains_sequence(&self, needle: &[u8]) -> bool {
        self.raw.windows(needle.len()).any(|w| w == needle)
    }

    /// Wait for the child to exit, pumping so the final bytes are captured.
    fn wait_for_exit(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            self.pump(Duration::from_millis(100));
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                // Drain whatever the exit path wrote on the way out.
                self.pump(Duration::from_millis(300));
                return;
            }
        }
        panic!("the program did not exit");
    }

    fn send(&mut self, bytes: &[u8]) {
        let _ = self.master.write_all(bytes);
        let _ = self.master.flush();
    }

    /// The screen as the emulator would show it.
    fn screen_text(&self) -> String {
        let screen = self.parser.screen();
        let (rows, columns) = screen.size();
        (0..rows)
            .map(|row| {
                screen
                    .contents_between(row, 0, row, columns)
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Wait until the screen satisfies `predicate`, or give up.
    fn wait_for(&mut self, what: &str, predicate: impl Fn(&str) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            self.pump(Duration::from_millis(200));
            if predicate(&self.screen_text()) {
                return;
            }
        }
        panic!(
            "timed out waiting for {what}. screen was:\n{}",
            self.screen_text()
        );
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn set_nonblocking(file: &std::fs::File) {
    use std::os::unix::io::AsRawFd;
    // SAFETY: the descriptor is owned by `file` and valid for this call.
    unsafe {
        let fd = file.as_raw_fd();
        let flags = libc::fcntl(fd, libc::F_GETFL, 0);
        libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
}

// ---------------------------------------------------------------------------
// A daemon to talk to
// ---------------------------------------------------------------------------

struct Daemon {
    child: std::process::Child,
    socket: PathBuf,
    root: PathBuf,
}

impl Daemon {
    fn start(label: &str) -> Self {
        // Unique per call. `Instant::now().elapsed()` is ~0, so an earlier
        // version handed every daemon in this process the same root and
        // socket: tests running in parallel then talked to each other's
        // daemons, and one without an agents file runs a blank shell.
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "anclave-pty-{label}-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let socket = root.join("d.sock");

        // An agent that keeps writing, so there is something to render. The
        // built-in `sh` leaves a blank pane, which would make a rendering
        // test pass for the wrong reason.
        let agents = root.join("agents.toml");
        std::fs::write(
            &agents,
            // `echo` rather than `printf`: the format string would have to
            // survive Rust escaping and then TOML escaping, and a mangled one
            // fails as a silently blank pane.
            "[[agents]]\nname = \"default\"\ncommand = \"sh\"\n\
             args = [\"-c\", \"while true; do echo tick; sleep 1; done\"]\n",
        )
        .unwrap();
        let child = Command::new(binary("anclaved"))
            .arg(format!("--socket={}", socket.display()))
            .env("ANCLAVE_AGENTS_FILE", &agents)
            .env_remove("TMUX")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start anclaved");

        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && !socket.exists() {
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(socket.exists(), "the daemon never bound its socket");

        Self {
            child,
            socket,
            root,
        }
    }

    /// What the daemon thinks that session's screen holds, via the CLI.
    fn capture(&self, id: &str) -> String {
        let out = Command::new(binary("anclave-cli"))
            .args(["session", "capture", id])
            .env("ANCLAVE_SOCKET", &self.socket)
            .output()
            .expect("run anclave-cli capture");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn create_session(&self, name: &str) {
        let status = Command::new(binary("anclave-cli"))
            .args(["session", "create", name, "default"])
            .env("ANCLAVE_SOCKET", &self.socket)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("run anclave-cli");
        assert!(status.success(), "creating a session failed");
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Kill the multiplexer too. Its socket lives in /tmp, derived from
        // the daemon's socket path, so it is outside the root removed below
        // and survived every run: each test left a server behind running the
        // tick agent forever. Sessions outliving their daemon is the point of
        // the architecture, which is exactly why a test has to clean up after
        // itself rather than assume killing the daemon is enough.
        let _ = std::process::Command::new("tmux")
            .args([
                "-S",
                tmux_socket_for(&self.socket).to_string_lossy().as_ref(),
                "kill-server",
            ])
            .output();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Where the daemon puts its multiplexer socket for a given daemon socket.
///
/// Mirrors `tmux_socket_for` in the daemon binary: the test cannot call it
/// (the tui crate must not depend on anclaved) so it repeats the derivation.
fn tmux_socket_for(socket: &std::path::Path) -> PathBuf {
    let digest = socket
        .to_string_lossy()
        .bytes()
        .fold(0xcbf29ce484222325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
        });
    PathBuf::from(format!("/tmp/anclave-tmux-{digest:016x}"))
}

/// Locate a workspace binary.
///
/// `CARGO_BIN_EXE_*` is only set for the crate that defines the binary, and
/// these live elsewhere, so fall back to the shared target directory.
fn binary(name: &str) -> PathBuf {
    if let Ok(path) = std::env::var(format!("CARGO_BIN_EXE_{name}")) {
        return PathBuf::from(path);
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let candidate = manifest.join("../../target/debug").join(name);
    let path = candidate
        .canonicalize()
        .unwrap_or_else(|_| panic!("build {name} before running the pty tests"));
    // Check each binary against the crates it is actually built from. A
    // single union list compared the *daemon* binary against *tui* sources,
    // so editing the client failed these tests with a message about a stale
    // daemon that was not stale. A guard that cries wolf is one people learn
    // to ignore.
    let sources: &[&str] = match name {
        "anclaved" => &[
            "daemon",
            "protocol",
            "security",
            "workspace",
            "audit",
            "terminal",
        ],
        "anclave" => &["tui", "protocol", "plugin", "cli"],
        "anclave-cli" => &["cli", "protocol"],
        _ => &[],
    };
    assert_fresh(&path, sources);
    path
}

/// Refuse a binary older than the sources it was built from.
///
/// These tests spawn a binary by path rather than through
/// `CARGO_BIN_EXE_*`, which is only set for the crate that defines it and
/// which the architecture rules forbid depending on. The cost is that
/// `cargo test -p <this crate>` does not rebuild it: the suite then exercises
/// whatever was built last and reports on code that is not the code under
/// test. That is a green run for the wrong reason, which is worse than a red
/// one. `cargo test --workspace` and CI build everything, so this only fires
/// on a targeted local run.
fn assert_fresh(binary: &std::path::Path, crates: &[&str]) {
    let built = match binary.metadata().and_then(|m| m.modified()) {
        Ok(time) => time,
        Err(_) => return,
    };
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for name in crates {
        let mut stack = vec![root.join("crates").join(name).join("src")];
        while let Some(directory) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&directory) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().is_some_and(|e| e == "rs") {
                    if let Ok(time) = entry.metadata().and_then(|m| m.modified()) {
                        if newest.as_ref().is_none_or(|(t, _)| time > *t) {
                            newest = Some((time, path));
                        }
                    }
                }
            }
        }
    }
    if let Some((time, path)) = newest {
        assert!(
            built >= time,
            "{} is older than {}: rebuild it, or run `cargo test --workspace`.\n\
             Running against a stale binary reports on code that is not the code under test.",
            binary.display(),
            path.display()
        );
    }
}

fn tui(daemon: &Daemon, term: &str) -> Command {
    let mut command = Command::new(binary("anclave"));
    command
        .env("ANCLAVE_SOCKET", &daemon.socket)
        .env("TERM", term)
        .env_remove("TMUX");
    command
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The one a unit test can never catch: a TUI that does not put the terminal
/// back leaves the user's shell in the alternate screen with no cursor.
#[test]
fn the_terminal_is_restored_on_quit() {
    let daemon = Daemon::start("restore");
    let mut pty = Pty::spawn(tui(&daemon, "xterm-256color"), 24, 80);

    pty.wait_for("the session list", |screen| screen.contains("Sessions"));
    assert!(
        pty.contains_sequence(b"\x1b[?1049h"),
        "the TUI never entered the alternate screen"
    );

    pty.send(b"q");
    pty.wait_for_exit();

    assert!(
        pty.contains_sequence(b"\x1b[?1049l"),
        "the alternate screen was never left: the shell is left scrolled away.\nraw tail: {:?}",
        String::from_utf8_lossy(&pty.raw_output()[pty.raw_output().len().saturating_sub(60)..])
    );
    assert!(
        pty.contains_sequence(b"\x1b[?25h"),
        "the cursor was never shown again: the shell is left with an invisible cursor"
    );
}

/// The TUI must draw something recognisable, through a real terminal.
#[test]
fn the_session_list_renders_through_a_pty() {
    let daemon = Daemon::start("render");
    daemon.create_session("alpha");

    let mut pty = Pty::spawn(tui(&daemon, "xterm-256color"), 24, 80);
    pty.wait_for("the created session", |screen| screen.contains("alpha"));

    let screen = pty.screen_text();
    assert!(screen.contains("Sessions"), "got:\n{screen}");
    assert!(screen.contains("Terminal"), "got:\n{screen}");
}

/// Pressing enter attaches, and the agent's output appears.
#[test]
fn attaching_shows_the_agent_output() {
    let daemon = Daemon::start("attach");
    daemon.create_session("alpha");

    // Confirm the daemon has output before blaming the client for not
    // showing any.
    std::thread::sleep(Duration::from_secs(3));
    let from_daemon = daemon.capture("session-0");
    assert!(
        from_daemon.contains("tick"),
        "the daemon itself has no agent output, so this is not a TUI problem:\n{from_daemon}"
    );

    let mut pty = Pty::spawn(tui(&daemon, "xterm-256color"), 24, 80);
    pty.wait_for("the session list", |screen| screen.contains("alpha"));
    pty.send(b"\r");
    pty.wait_for("the agent's output", |screen| screen.contains("tick"));
}

/// A cramped terminal must not panic. Layout arithmetic that underflows is a
/// crash, and a one-column terminal is the cheapest way to find it.
#[test]
fn a_tiny_terminal_does_not_crash_the_tui() {
    let daemon = Daemon::start("tiny");
    let mut pty = Pty::spawn(tui(&daemon, "xterm-256color"), 1, 1);

    pty.pump(Duration::from_secs(3));
    assert!(
        matches!(pty.child.try_wait(), Ok(None)),
        "the TUI died on a 1x1 terminal"
    );

    pty.send(b"q");
    pty.wait_for_exit();
}

/// Terminals disagree about what they support. A TUI that assumes 256 colors
/// or a specific TERM breaks on the ones that do not, and `dumb` is the
/// harshest of the realistic cases.
#[test]
fn it_starts_under_every_term_we_claim_to_support() {
    for term in ["xterm-256color", "xterm", "screen", "vt100", "dumb"] {
        let daemon = Daemon::start("term");
        let mut pty = Pty::spawn(tui(&daemon, term), 24, 80);

        pty.pump(Duration::from_secs(3));
        assert!(
            matches!(pty.child.try_wait(), Ok(None)),
            "the TUI exited immediately under TERM={term}"
        );

        pty.send(b"q");
        pty.wait_for_exit();
    }
}

/// A daemon that is not there must be explained, not merely survived.
///
/// Plan commit 31: "a broken extensible UI cannot brick the application."
/// The client has to start, say why it is unusable, offer a way to look
/// closer, and quit cleanly.
#[test]
fn a_missing_daemon_is_explained_and_the_client_still_quits() {
    let mut command = Command::new(binary("anclave"));
    command
        .env("ANCLAVE_SOCKET", "/tmp/anclave-does-not-exist-at-all.sock")
        .env("TERM", "xterm-256color")
        .env_remove("TMUX");
    let mut pty = Pty::spawn(command, 24, 80);

    // The status row is 80 columns and a real reason does not fit: it is
    // truncated mid-word here, which is exactly why the overlay exists.
    pty.wait_for("the failure to be reported", |screen| {
        screen.contains("Disconnected") && screen.contains("d for details")
    });

    // The overlay is where the untruncated reason lives, along with the facts
    // needed to tell one failure from another.
    pty.send(b"d");
    pty.wait_for("the diagnostics overlay", |screen| {
        screen.contains("Diagnostics")
            && screen.contains("client protocol")
            && screen.contains("cannot reach the daemon")
    });

    // Esc closes the overlay rather than quitting: the key that closes things
    // must close the top thing.
    pty.send(&[0x1b]);
    pty.wait_for("the overlay to close", |screen| {
        !screen.contains("Diagnostics")
    });

    pty.send(b"q");
    pty.wait_for_exit();
}

/// A daemon speaking a different protocol is named as such.
///
/// Reconnecting cannot fix a mismatch, so telling someone to retry is worse
/// than useless. Before this the client never asked: the first incompatible
/// response surfaced as a decode error with no hint that the versions
/// differed.
#[test]
fn a_protocol_mismatch_is_named_rather_than_retried() {
    use std::io::{BufReader, BufWriter};
    use std::os::unix::net::UnixListener;

    let socket = std::env::temp_dir().join(format!(
        "anclave-mismatch-{}-{}.sock",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).expect("bind the fake daemon socket");

    // A daemon from the future: same framing, different protocol number.
    let server_socket = socket.clone();
    let server = std::thread::spawn(move || {
        while let Ok((stream, _)) = listener.accept() {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = BufWriter::new(stream);
            while let Ok(envelope) =
                anclave_protocol::ipc::read_envelope::<_, anclave_protocol::Request>(&mut reader)
            {
                let response =
                    anclave_protocol::Message::Response(anclave_protocol::Response::Version {
                        protocol: anclave_protocol::PROTOCOL_VERSION + 41,
                        version: "99.0.0".to_owned(),
                    });
                let reply = anclave_protocol::Envelope::new(envelope.request_id, response);
                if anclave_protocol::ipc::write_envelope(&mut writer, &reply).is_err() {
                    break;
                }
            }
            if !server_socket.exists() {
                break;
            }
        }
    });

    let mut command = Command::new(binary("anclave"));
    command
        .env("ANCLAVE_SOCKET", &socket)
        .env("TERM", "xterm-256color")
        .env_remove("TMUX");
    let mut pty = Pty::spawn(command, 24, 80);

    pty.wait_for("the mismatch to be named", |screen| {
        screen.contains("protocol mismatch")
    });

    pty.send(b"q");
    pty.wait_for_exit();
    let _ = std::fs::remove_file(&socket);
    drop(server);
}
