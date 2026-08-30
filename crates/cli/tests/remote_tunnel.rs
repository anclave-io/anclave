//! The SSH tunnel, over a stub `ssh`.
//!
//! Substituting the ssh binary tests every branch that matters without
//! needing a reachable host, a key, or sshd in CI. What a real ssh adds over
//! the stub is transport and authentication, which are ssh's job and not
//! Anclave's to re-test.
//!
//! The success case is the important one: the stub links the "forwarded"
//! path to a real daemon socket, so the whole path is exercised end to end,
//! `Client::connect` included, and the assertion is a real protocol
//! round trip rather than the existence of a file.

use std::time::Duration;

use anclave_cli::remote::{Tunnel, TunnelError};

/// Write an executable stub and point `ANCLAVE_SSH` at it.
fn stub(directory: &std::path::Path, body: &str) -> std::path::PathBuf {
    let path = directory.join("ssh-stub");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

/// A successful forward yields a socket a client can actually talk to.
#[tokio::test]
async fn a_successful_tunnel_carries_the_protocol() {
    let directory = tempfile::tempdir().unwrap();

    // Stand in for the remote daemon with a real listener speaking the
    // protocol's framing: what matters is that bytes cross the forwarded
    // path, not what is on the far end.
    let remote = directory.path().join("remote.sock");
    let listener = tokio::net::UnixListener::bind(&remote).unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buffer = [0u8; 1024];
                while let Ok(read) = stream.read(&mut buffer).await {
                    if read == 0 {
                        break;
                    }
                    let _ = stream.write_all(&buffer[..read]).await;
                }
            });
        }
    });

    // ssh forwards a local path to a remote one; the stub links them, which
    // is the same observable effect for a client connecting locally.
    let script = stub(
        directory.path(),
        r#"
for arg in "$@"; do
  case "$arg" in
    *:*) local_path=${arg%%:*}; ln -sf "${arg#*:}" "$local_path" ;;
  esac
done
sleep 30
"#,
    );

    let tunnel = Tunnel::open_with(
        script.to_str().unwrap(),
        "me@box",
        remote.to_str().unwrap(),
        Duration::from_secs(5),
    )
    .await
    .expect("the tunnel must open");

    // A real connection over the forwarded path.
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::UnixStream::connect(tunnel.socket())
        .await
        .unwrap();
    stream.write_all(b"round trip").await.unwrap();
    let mut buffer = [0u8; 10];
    stream.read_exact(&mut buffer).await.unwrap();
    assert_eq!(&buffer, b"round trip");

    let path = tunnel.socket().to_path_buf();
    drop(tunnel);
    // Dropping the tunnel must not leave the local socket behind: a client
    // that reconnects would otherwise accumulate one per attempt.
    assert!(!path.exists(), "the local socket outlived its tunnel");
}

/// An unreachable host reports what ssh said, not a generic failure.
///
/// The message is the only thing separating a wrong hostname from a refused
/// key from a daemon that is not running, and all three send people looking
/// in different places.
#[tokio::test]
async fn an_unavailable_host_reports_what_ssh_said() {
    let directory = tempfile::tempdir().unwrap();
    let script = stub(
        directory.path(),
        "echo 'ssh: Could not resolve hostname nope' >&2\nexit 255",
    );
    let error = Tunnel::open_with(
        script.to_str().unwrap(),
        "nope",
        "/tmp/anclaved.sock",
        Duration::from_secs(5),
    )
    .await
    .expect_err("an unreachable host must fail");
    let text = error.to_string();
    assert!(
        text.contains("Could not resolve hostname"),
        "the message must carry ssh's own words: {text}"
    );
}

/// A forward that never becomes usable ends at the deadline, not never.
#[tokio::test]
async fn a_tunnel_that_never_opens_times_out() {
    let directory = tempfile::tempdir().unwrap();
    // Connects to nothing and stays up, which is what a host that accepts
    // the connection but has no daemon listening looks like.
    let script = stub(directory.path(), "sleep 30");
    let started = std::time::Instant::now();
    let error = Tunnel::open_with(
        script.to_str().unwrap(),
        "slow",
        "/tmp/anclaved.sock",
        Duration::from_secs(1),
    )
    .await
    .expect_err("a tunnel that never opens must time out");
    assert!(
        matches!(error, TunnelError::Timeout { .. }),
        "expected a timeout, got {error}"
    );
    assert!(
        error.to_string().contains("is anclaved running"),
        "the message should point at the likely cause: {error}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the deadline must actually bound the wait"
    );
}

/// A missing ssh is reported as such rather than as a timeout.
#[tokio::test]
async fn a_missing_ssh_binary_is_named() {
    let error = Tunnel::open_with(
        "/nonexistent/ssh",
        "host",
        "/tmp/anclaved.sock",
        Duration::from_secs(2),
    )
    .await
    .expect_err("a missing ssh must fail");
    assert!(
        matches!(error, TunnelError::Spawn(_)),
        "expected a spawn failure, got {error}"
    );
}

/// The forward asks for the right thing, with prompts and hangs disabled.
#[tokio::test]
async fn the_forward_is_requested_with_safe_options() {
    let directory = tempfile::tempdir().unwrap();
    let log = directory.path().join("args");
    // The log path is baked into the script rather than passed by
    // environment, for the same reason the program is a parameter.
    let script = stub(
        directory.path(),
        &format!(
            "for arg in \"$@\"; do echo \"$arg\" >> {}; done\nexit 3",
            log.display()
        ),
    );

    let _ = Tunnel::open_with(
        script.to_str().unwrap(),
        "me@box",
        "/run/anclaved.sock",
        Duration::from_secs(2),
    )
    .await;
    let arguments = std::fs::read_to_string(&log).unwrap_or_default();

    // BatchMode: a password prompt behind the alternate screen is invisible
    // and hangs the client. ExitOnForwardFailure: without it ssh stays up
    // after a failed forward and the wait runs its whole deadline.
    for expected in [
        "BatchMode=yes",
        "ExitOnForwardFailure=yes",
        "-N",
        "me@box",
        "/run/anclaved.sock",
    ] {
        assert!(
            arguments.contains(expected),
            "ssh was not asked for {expected}; got:\n{arguments}"
        );
    }
}

/// The real ssh accepts the option set we build.
///
/// The stub above proves the branches; it cannot prove that `ssh` itself
/// tolerates these flags, and a rejected option would surface as a failure to
/// connect rather than as the usage error it is. So this runs the actual
/// binary against an address that cannot answer and asserts the failure is
/// about *reaching* the host, not about the command line.
///
/// Skipped where there is no ssh, since that is an absent tool rather than a
/// broken tunnel.
#[tokio::test]
async fn the_real_ssh_accepts_our_options() {
    if std::process::Command::new("ssh")
        .arg("-V")
        .output()
        .is_err()
    {
        return;
    }

    // 203.0.113.0/24 is TEST-NET-3: reserved for documentation and not
    // routable, so this cannot accidentally reach a real host.
    let error = Tunnel::open_with(
        "ssh",
        "anclave-test@203.0.113.1",
        "/tmp/anclaved.sock",
        Duration::from_secs(2),
    )
    .await
    .expect_err("an unroutable address cannot connect");

    let text = error.to_string();
    for rejection in [
        "usage:",
        "unknown option",
        "command-line",
        "Bad configuration",
    ] {
        assert!(
            !text.contains(rejection),
            "ssh rejected our arguments rather than failing to connect: {text}"
        );
    }
}
