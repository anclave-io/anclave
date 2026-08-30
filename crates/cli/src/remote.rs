//! Reaching a daemon on another host.
//!
//! The shape is the one the plan prefers, and it is worth being explicit
//! about why:
//!
//! ```text
//! local client → SSH tunnel → remote anclaved → remote backend
//! ```
//!
//! The remote daemon owns remote sessions exactly as the local one owns local
//! sessions. Nothing here knows what a session is. It forwards a Unix socket
//! and hands back a path, so `Client::connect` is unchanged, the protocol is
//! unchanged, and "remote sessions use the same lifecycle as local sessions"
//! is true by construction rather than by two implementations kept in step.
//!
//! The alternative, running `ssh host tmux …` from the local process, puts
//! the remote lifecycle in the client: every operation grows a local and a
//! remote form, and they drift.
//!
//! **Authentication and transport security are SSH's.** Anclave adds no
//! second credential and no key material: if you can reach the remote socket
//! over SSH, you can drive that daemon, and the remote socket's own 0600
//! permissions decide who that is on the far side.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::{Child, Command};

/// Where a remote daemon listens, unless told otherwise.
pub const DEFAULT_REMOTE_SOCKET: &str = "/tmp/anclaved.sock";

/// How long to wait for the forwarded socket to answer.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug)]
pub enum TunnelError {
    /// The ssh client could not be started at all.
    Spawn(std::io::Error),
    /// ssh exited before the forward was usable. Carries what it said, which
    /// is the only thing that distinguishes a wrong host from a refused key
    /// from a daemon that is not running.
    Exited { status: String, stderr: String },
    /// The forward never became usable within the deadline.
    Timeout { seconds: u64 },
}

impl std::fmt::Display for TunnelError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(error) => write!(formatter, "cannot start ssh: {error}"),
            Self::Exited { status, stderr } => {
                let detail = stderr.trim();
                if detail.is_empty() {
                    write!(formatter, "ssh exited ({status})")
                } else {
                    write!(formatter, "ssh exited ({status}): {detail}")
                }
            }
            Self::Timeout { seconds } => write!(
                formatter,
                "the remote socket did not answer within {seconds}s: \
                 is anclaved running on that host?"
            ),
        }
    }
}

impl std::error::Error for TunnelError {}

/// A forwarded Unix socket, alive for as long as this value is.
///
/// Dropping it kills ssh and removes the local socket, so a client that exits
/// or reconnects does not leave a forward behind.
#[derive(Debug)]
pub struct Tunnel {
    /// Held to be dropped, not to be read.
    ///
    /// `kill_on_drop` means this field *is* the tunnel's lifetime: letting it
    /// fall out of scope is what closes ssh. Naming that here because a
    /// "never read" field looks removable and removing it would leave a
    /// forward running after the client that opened it had gone.
    #[allow(dead_code)]
    child: Child,
    local: PathBuf,
}

impl Tunnel {
    /// The local path to connect to, exactly as `Client::connect` wants it.
    pub fn socket(&self) -> &Path {
        &self.local
    }

    /// Open a forward to `remote_socket` on `destination`.
    ///
    /// `destination` is whatever ssh accepts: `user@host`, or a `~/.ssh/config`
    /// alias, which is deliberately not parsed here. Anclave should not
    /// reimplement a config format ssh already reads.
    pub async fn open(
        destination: &str,
        remote_socket: &str,
        timeout: Duration,
    ) -> Result<Self, TunnelError> {
        // Read here rather than in `open_with`: an ambient program name is
        // process-global, which is fine for a real run and wrong for tests,
        // where it made parallel cases substitute each other's stubs.
        let program = std::env::var("ANCLAVE_SSH").unwrap_or_else(|_| "ssh".to_owned());
        Self::open_with(&program, destination, remote_socket, timeout).await
    }

    /// Open a forward using a named ssh program.
    ///
    /// The program is a parameter rather than read from the environment
    /// inside here: an ambient one is process-global, which made parallel
    /// tests substitute each other's stubs and fail in ways that looked like
    /// bugs in the tunnel.
    pub async fn open_with(
        program: &str,
        destination: &str,
        remote_socket: &str,
        timeout: Duration,
    ) -> Result<Self, TunnelError> {
        let local = local_socket_path();
        let _ = std::fs::remove_file(&local);

        let mut command = Command::new(program);
        command
            .arg("-N")
            // Fail rather than prompt. A password prompt from a process the
            // TUI spawned would be invisible behind the alternate screen and
            // would hang the client until it timed out.
            .arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg(format!("ConnectTimeout={}", timeout.as_secs().max(1)))
            // Without this ssh stays up after a failed forward, and the wait
            // below would run its full deadline against a tunnel that was
            // never going to work.
            .arg("-o")
            .arg("ExitOnForwardFailure=yes")
            .arg("-o")
            .arg("ServerAliveInterval=15")
            .arg("-L")
            .arg(format!("{}:{}", local.display(), remote_socket))
            .arg(destination)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = command.spawn().map_err(TunnelError::Spawn)?;
        let stderr = child.stderr.take();

        let deadline = std::time::Instant::now() + timeout;
        loop {
            // Connectable, not merely present: ssh creates the socket before
            // the far side is ready, so an existence check hands back a path
            // that refuses the first connection.
            if tokio::net::UnixStream::connect(&local).await.is_ok() {
                return Ok(Self { child, local });
            }
            if let Ok(Some(status)) = child.try_wait() {
                let message = drain(stderr).await;
                let _ = std::fs::remove_file(&local);
                return Err(TunnelError::Exited {
                    status: status.to_string(),
                    stderr: message,
                });
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill().await;
                let message = drain(stderr).await;
                let _ = std::fs::remove_file(&local);
                if !message.trim().is_empty() {
                    return Err(TunnelError::Exited {
                        status: "timed out".to_owned(),
                        stderr: message,
                    });
                }
                return Err(TunnelError::Timeout {
                    seconds: timeout.as_secs(),
                });
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        // `kill_on_drop` handles the child; the socket is ours to remove.
        let _ = std::fs::remove_file(&self.local);
    }
}

/// Read what ssh said, without waiting on anything that outlives it.
///
/// Bounded because reading to EOF waits for *every* holder of the pipe:
/// killing ssh leaves a grandchild holding stderr open, and an unbounded read
/// then blocks for as long as that process lives. The diagnostic is worth a
/// moment, never worth hanging the client that is trying to report it.
async fn drain(stderr: Option<tokio::process::ChildStderr>) -> String {
    use tokio::io::AsyncReadExt;
    let Some(mut stderr) = stderr else {
        return String::new();
    };
    let mut buffer = String::new();
    let _ = tokio::time::timeout(
        Duration::from_millis(500),
        stderr.read_to_string(&mut buffer),
    )
    .await;
    buffer
}

/// A short, unique path for the local end of the forward.
///
/// Short on purpose: a Unix socket path is capped near 104 bytes, and the
/// system temp directory on macOS is already about 50 of them.
fn local_socket_path() -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    PathBuf::from(format!("/tmp/anclave-r{}-{}.sock", std::process::id(), n))
}
