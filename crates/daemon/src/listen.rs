//! Daemon startup concerns: how the socket path is chosen, how a socket left
//! by a dead daemon is cleared, and how the listening socket is restricted.
//!
//! These are separated from `main` so the decisions can be unit-tested without
//! binding a real socket or starting a runtime.

use std::io;
use std::path::{Path, PathBuf};

pub const DEFAULT_SOCKET: &str = "/tmp/anclaved.sock";

/// Everything the daemon takes from its command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub socket: PathBuf,
    /// Print usage and exit without binding anything.
    pub help: bool,
    /// Print the version and exit.
    pub version: bool,
    /// Accepted and always true: the daemon never detaches on its own. The
    /// flag exists so a supervisor that passes it does not fail to start.
    pub foreground: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            socket: PathBuf::from(DEFAULT_SOCKET),
            help: false,
            version: false,
            foreground: true,
        }
    }
}

/// Parse `--socket PATH`, `--socket=PATH`, and `--foreground`.
///
/// Both socket spellings are supported deliberately: the plan documents the
/// space-separated form, and the test harness uses the `=` form.
pub fn parse_args<I>(arguments: I) -> Result<Options, String>
where
    I: IntoIterator<Item = String>,
{
    let mut options = Options::default();
    let mut arguments = arguments.into_iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--foreground" => options.foreground = true,
            // `--help` on a daemon has to work without a socket, a database,
            // or a writable /tmp. It is the first thing anyone types.
            "--help" | "-h" => options.help = true,
            "--version" | "-V" => options.version = true,
            "--socket" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| "--socket requires a path".to_owned())?;
                options.socket = PathBuf::from(value);
            }
            other => match other.strip_prefix("--socket=") {
                Some(value) if !value.is_empty() => options.socket = PathBuf::from(value),
                Some(_) => return Err("--socket requires a path".to_owned()),
                None => return Err(format!("unknown argument: {other}")),
            },
        }
    }
    Ok(options)
}

/// Remove a leftover socket file, but only when nothing is listening on it.
///
/// The unconditional `remove_file` this replaces would delete the socket of a
/// *live* daemon and then bind a second one, leaving the first running and
/// unreachable. Connecting is the only reliable liveness test: a bound,
/// listening socket accepts, and a stale inode refuses.
pub fn clear_stale_socket(path: &Path) -> io::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    match std::os::unix::net::UnixStream::connect(path) {
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("a daemon is already listening on {}", path.display()),
        )),
        Err(_) => std::fs::remove_file(path),
    }
}

/// Restrict the bound socket to its owner.
///
/// The socket is the daemon's entire authority: anything that can connect can
/// create sessions and send input to a running agent. The default umask leaves
/// it group- and world-readable on a shared host.
/// Usage, printed for `--help`.
pub const USAGE: &str = r"anclaved — the anclave session daemon

USAGE
  anclaved [OPTIONS]

OPTIONS
  --socket PATH     socket to listen on (default /tmp/anclaved.sock)
  --foreground      stay in the foreground (the default; anclaved never
                    detaches on its own — use your service manager)
  --help, -h        print this and exit
  --version, -V     print the version and exit

The socket is created with mode 0600: anything that can connect to it can
create sessions and type into a running agent.

The database lives beside the socket, at the same path with a .db extension.

ENVIRONMENT
  ANCLAVE_AGENTS_FILE     agent definitions (TOML)
  ANCLAVE_SECURITY_FILE   security profiles (TOML); a file that does not
                          parse stops the daemon rather than falling back
  ANCLAVE_WORKSPACE_ROOT  where session workspaces are built";

pub fn restrict_socket(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn defaults_to_the_shared_socket_in_the_foreground() {
        let options = parse_args(Vec::new()).unwrap();
        assert_eq!(options.socket, PathBuf::from(DEFAULT_SOCKET));
        assert!(options.foreground);
    }

    #[test]
    fn both_socket_spellings_are_accepted() {
        assert_eq!(
            parse_args(args(&["--socket", "/tmp/a.sock"]))
                .unwrap()
                .socket,
            PathBuf::from("/tmp/a.sock")
        );
        assert_eq!(
            parse_args(args(&["--socket=/tmp/b.sock"])).unwrap().socket,
            PathBuf::from("/tmp/b.sock")
        );
    }

    #[test]
    fn help_and_version_are_parsed_without_needing_anything_else() {
        assert!(parse_args(args(&["--help"])).unwrap().help);
        assert!(parse_args(args(&["-h"])).unwrap().help);
        assert!(parse_args(args(&["--version"])).unwrap().version);
        assert!(parse_args(args(&["-V"])).unwrap().version);
        assert!(!parse_args(Vec::new()).unwrap().help);
    }

    #[test]
    fn malformed_and_unknown_arguments_are_rejected() {
        assert!(parse_args(args(&["--socket"])).is_err());
        assert!(parse_args(args(&["--socket="])).is_err());
        assert!(parse_args(args(&["--nope"])).is_err());
    }

    #[test]
    fn a_missing_socket_is_not_an_error() {
        let path = std::env::temp_dir().join("anclave-absent-socket.sock");
        let _ = std::fs::remove_file(&path);
        assert!(clear_stale_socket(&path).is_ok());
    }

    #[test]
    fn a_stale_socket_is_removed_but_a_live_one_is_refused() {
        let path = std::env::temp_dir().join(format!("anclave-stale-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);

        // A plain file at the path is not a listener: stale, so it goes.
        std::fs::write(&path, b"").unwrap();
        clear_stale_socket(&path).unwrap();
        assert!(!path.exists());

        // A bound listener is live: refuse rather than unlink it.
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let error = clear_stale_socket(&path).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        assert!(path.exists());
        drop(listener);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_bound_socket_is_restricted_to_its_owner() {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!("anclave-mode-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        restrict_socket(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        drop(listener);
        let _ = std::fs::remove_file(&path);
    }
}
