use std::env;

use anclave_cli::{default_socket, Client};
use anclave_protocol::{
    AgentId, BackendId, CreateSession, MemberAccess, Request, Response, WorkspaceId,
    WorkspaceMember, WorkspaceSpec,
};

/// Exit codes, so a script can tell the cases apart.
const EXIT_DAEMON_ERROR: i32 = 1;
const EXIT_USAGE: i32 = 2;
const EXIT_UNREACHABLE: i32 = 3;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    let command = arguments.next().unwrap_or_else(|| "help".to_owned());

    // Answer everything that does not need a daemon *before* connecting.
    // `--help` that only works when a daemon is already running is not help,
    // and it is the first thing anyone types.
    // Verifying the audit chain reads a file; it must not need a daemon,
    // since the case you most want it in is one where you do not trust what
    // the daemon is telling you.
    if command == "audit" {
        return audit_command(&mut arguments);
    }
    if command == "plugin" {
        return plugin_command(&mut arguments);
    }

    match command.as_str() {
        "help" | "--help" | "-h" => {
            print_help();
            return Ok(());
        }
        "--version" | "-V" => {
            println!("anclave-cli {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        _ => {}
    }

    // Parse before connecting too, so a typo is reported as a typo rather than
    // as whatever the network did.
    let request = match build_request(&command, &mut arguments) {
        Ok(request) => request,
        Err(error) => {
            eprintln!("anclave-cli: {error}");
            eprintln!("run `anclave-cli --help` for usage.");
            std::process::exit(EXIT_USAGE);
        }
    };

    // A remote host is reached by forwarding its daemon's socket here, so
    // everything below this point is identical for local and remote. The
    // tunnel is held for the life of the process: dropping it closes ssh.
    let host = env::var("ANCLAVE_HOST").ok().filter(|h| !h.is_empty());
    let _tunnel = match host {
        None => None,
        Some(ref destination) => {
            let remote = env::var("ANCLAVE_REMOTE_SOCKET")
                .unwrap_or_else(|_| anclave_cli::remote::DEFAULT_REMOTE_SOCKET.to_owned());
            match anclave_cli::remote::Tunnel::open(
                destination,
                &remote,
                anclave_cli::remote::DEFAULT_TIMEOUT,
            )
            .await
            {
                Ok(tunnel) => Some(tunnel),
                Err(error) => {
                    eprintln!("cannot reach {destination}: {error}");
                    std::process::exit(EXIT_UNREACHABLE);
                }
            }
        }
    };

    let socket = match _tunnel {
        Some(ref tunnel) => tunnel.socket().to_string_lossy().into_owned(),
        None => env::var("ANCLAVE_SOCKET").unwrap_or_else(|_| default_socket().to_owned()),
    };
    let mut client = match Client::connect(&socket).await {
        Ok(client) => client,
        Err(error) => {
            eprintln!("cannot reach the anclave daemon at {socket}: {error}");
            eprintln!("start one with `anclaved --socket {socket}`, or set ANCLAVE_SOCKET.");
            std::process::exit(EXIT_UNREACHABLE);
        }
    };

    match client.request(request).await? {
        Response::Error { code, message } => {
            eprintln!("{code:?}: {message}");
            std::process::exit(EXIT_DAEMON_ERROR);
        }
        Response::Migration(report) => print_migration(&report),
        response => println!("{}", serde_json::to_string_pretty(&response)?),
    }
    Ok(())
}

/// `migrate inspect|import SOURCE [--apply]`.
///
/// `import` without `--apply` is a dry run. The safe form is the one you get
/// by forgetting a flag, rather than the one you get by remembering
/// `--dry-run`.
fn migrate_request(
    arguments: &mut impl Iterator<Item = String>,
) -> Result<Request, Box<dyn std::error::Error>> {
    let action = arguments.next().ok_or("missing migrate action")?;
    let source = arguments.next().ok_or("missing source directory")?;
    let mut apply = false;
    for argument in arguments {
        match argument.as_str() {
            "--apply" => apply = true,
            "--dry-run" => apply = false,
            other => return Err(format!("unknown migrate option: {other}").into()),
        }
    }
    match action.as_str() {
        "inspect" => Ok(Request::InspectMigration { source }),
        "import" => Ok(Request::ImportMigration { source, apply }),
        other => Err(format!("unknown migrate action: {other}").into()),
    }
}

/// `plugin list|trust|revoke`: the explicit grant surface for UI plugins.
///
/// Granting is deliberately a separate, typed act rather than a prompt in the
/// client: a person approving a capability should be doing it on purpose,
/// with the path in front of them, not dismissing a dialog that stands
/// between them and the pane they wanted to see.
///
/// **This governs UI plugins only.** It grants a pane the ability to ask the
/// client to do something the client already does. It does not widen what any
/// coding agent may do: that is decided by the agent's security profile, and
/// nothing here touches it.
fn plugin_command(
    arguments: &mut impl Iterator<Item = String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let action = arguments.next().unwrap_or_else(|| "list".to_owned());
    let directory = std::env::var("ANCLAVE_PLUGIN_DIR")
        .map_err(|_| "set ANCLAVE_PLUGIN_DIR to the directory holding your plugins".to_owned())?;
    let directory = std::path::PathBuf::from(directory);
    let store_path = anclave_plugin::trust_store_path(&directory);

    match action.as_str() {
        "list" => {
            let mut store = anclave_plugin::TrustStore::load(&store_path);
            let _ = &mut store;
            let (mut host, errors) = anclave_plugin::PluginHost::load_directory(&directory);
            host.set_trust(anclave_plugin::TrustStore::load(&store_path));
            host.reload();
            for error in &errors {
                println!("failed  {error}");
            }
            for plugin in host.plugins() {
                let declared: Vec<&str> = plugin
                    .declared
                    .iter()
                    .map(|capability| capability.name())
                    .collect();
                println!(
                    "{:<10} {:<12} {} [{}]",
                    plugin.id,
                    format!("{:?}", plugin.trust).to_lowercase(),
                    plugin.path.display(),
                    declared.join(",")
                );
            }
            Ok(())
        }
        "trust" | "revoke" => {
            let path = arguments.next().ok_or("missing plugin path")?;
            let path = std::path::PathBuf::from(path);
            let mut store = anclave_plugin::TrustStore::load(&store_path);
            if action == "trust" {
                let source = std::fs::read_to_string(&path)?;
                store.trust(&path, &source);
                println!("trusted {}", path.display());
                println!("capabilities it declares are granted until the file changes.");
            } else {
                store.revoke(&path);
                println!("revoked {}", path.display());
            }
            store.save(&store_path)?;
            Ok(())
        }
        other => {
            eprintln!("anclave-cli: unknown plugin action: {other}");
            std::process::exit(EXIT_USAGE);
        }
    }
}

/// `audit verify PATH`: check the hash chain of a log on disk.
fn audit_command(
    arguments: &mut impl Iterator<Item = String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let action = arguments.next().unwrap_or_else(|| "verify".to_owned());
    if action != "verify" {
        eprintln!("anclave-cli: unknown audit action: {action}");
        std::process::exit(EXIT_USAGE);
    }
    let path = arguments.next().ok_or("missing audit log path")?;
    let log = anclave_audit::AuditLog::new(&path);

    match log.verify()? {
        anclave_audit::Integrity::Intact { entries } => {
            println!("intact: {entries} entries, chain verified");
            Ok(())
        }
        anclave_audit::Integrity::Altered { sequence } => {
            eprintln!("TAMPERED: entry {sequence} does not match its own hash");
            std::process::exit(EXIT_DAEMON_ERROR);
        }
        anclave_audit::Integrity::BrokenChain { sequence } => {
            eprintln!(
                "TAMPERED: the chain breaks at entry {sequence}: an entry was removed or reordered"
            );
            std::process::exit(EXIT_DAEMON_ERROR);
        }
    }
}

fn build_request(
    command: &str,
    arguments: &mut impl Iterator<Item = String>,
) -> Result<Request, Box<dyn std::error::Error>> {
    match command {
        "ping" => Ok(Request::Ping),
        "version" => Ok(Request::GetVersion),
        "daemon" => daemon_request(arguments),
        "migrate" => migrate_request(arguments),
        "session" => session_request(arguments),
        "approval" | "approvals" => approval_request(arguments),
        other => Err(format!("unknown command: {other}").into()),
    }
}

fn session_request(
    arguments: &mut impl Iterator<Item = String>,
) -> Result<Request, Box<dyn std::error::Error>> {
    let action = arguments.next().unwrap_or_else(|| "list".to_owned());
    match action.as_str() {
        "list" => Ok(Request::ListSessions),
        "restart" => Ok(Request::RestartSession {
            id: session_id(arguments, "session ID")?,
        }),
        "get" => Ok(Request::GetSession {
            id: session_id(arguments, "session ID")?,
        }),
        "delete" => Ok(Request::DeleteSession {
            id: session_id(arguments, "session ID")?,
        }),
        "capture" => Ok(Request::CaptureScreen {
            id: session_id(arguments, "session ID")?,
        }),
        "send" => {
            let id = session_id(arguments, "session ID")?;
            let text = arguments.next().ok_or("missing input text")?;
            Ok(Request::SendInput {
                id,
                bytes: text.into_bytes(),
            })
        }
        "create" => {
            let name = arguments.next().ok_or("missing session name")?;
            // AGENT is optional, so a leading `--` means it was left out
            // rather than that someone named an agent `--repo`. Taking the
            // next argument unconditionally swallowed the first flag and then
            // reported that flag's *value* as an unknown option, which read
            // as "--repo is not supported" for a `--repo` that is.
            let rest: Vec<String> = arguments.collect();
            let (agent_name, rest) = match rest.split_first() {
                Some((first, tail)) if !first.starts_with("--") => (first.clone(), tail.to_vec()),
                _ => ("default".to_owned(), rest),
            };
            let (workspace, security) = parse_workspace(&name, &mut rest.into_iter())?;
            Ok(Request::CreateSession(CreateSession {
                name,
                agent: AgentId::new(agent_name)?,
                backend: BackendId::new("local")?,
                workspace,
                security,
            }))
        }
        _ => Err(format!("unknown session action: {action}").into()),
    }
}

/// `daemon status` answers "is a daemon reachable, and which version".
/// Reaching this point already proves the socket connected, so the request is
/// the version handshake rather than a separate liveness probe.
fn daemon_request(
    arguments: &mut impl Iterator<Item = String>,
) -> Result<Request, Box<dyn std::error::Error>> {
    match arguments
        .next()
        .unwrap_or_else(|| "status".to_owned())
        .as_str()
    {
        "status" => Ok(Request::GetVersion),
        "sandbox" | "sandboxes" => Ok(Request::GetSandboxReport),
        other => Err(format!("unknown daemon action: {other}").into()),
    }
}

/// Build a workspace from repeatable trailing flags.
///
/// `--repo PATH` gets its own worktree on `--branch`; `--dir PATH` is attached
/// as it is, sharing whatever branch it already has. No flags means no
/// workspace, and the agent runs wherever the daemon puts it.
fn parse_workspace(
    session_name: &str,
    arguments: &mut impl Iterator<Item = String>,
) -> Result<(Option<WorkspaceSpec>, Option<String>), Box<dyn std::error::Error>> {
    let mut branch: Option<String> = None;
    let mut security: Option<String> = None;
    let mut members = Vec::new();
    let mut pending_worktrees = Vec::new();

    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--branch" => branch = Some(arguments.next().ok_or("--branch requires a name")?),
            "--security" => {
                security = Some(
                    arguments
                        .next()
                        .ok_or("--security requires a profile name")?,
                )
            }
            "--repo" => pending_worktrees.push(arguments.next().ok_or("--repo requires a path")?),
            "--dir" => members.push(WorkspaceMember {
                repository: arguments.next().ok_or("--dir requires a path")?,
                branch: None,
                base: None,
                access: MemberAccess::ReadWrite,
            }),
            other => return Err(format!("unknown create option: {other}").into()),
        }
    }

    if pending_worktrees.is_empty() && members.is_empty() {
        return Ok((None, security));
    }
    // Every worktree member shares one branch, which is what makes a
    // multi-repository change reviewable as one branch name across repos.
    let branch = branch.ok_or("--repo requires --branch")?;
    let worktrees = pending_worktrees
        .into_iter()
        .map(|repository| WorkspaceMember {
            repository,
            branch: Some(branch.clone()),
            base: None,
            access: MemberAccess::ReadWrite,
        });
    let mut all: Vec<WorkspaceMember> = worktrees.collect();
    all.extend(members);

    Ok((
        Some(WorkspaceSpec {
            id: WorkspaceId::new(format!("ws-{session_name}"))?,
            members: all,
        }),
        security,
    ))
}

/// `approval list|approve ID|deny ID`.
fn approval_request(
    arguments: &mut impl Iterator<Item = String>,
) -> Result<Request, Box<dyn std::error::Error>> {
    match arguments
        .next()
        .unwrap_or_else(|| "list".to_owned())
        .as_str()
    {
        "list" => Ok(Request::ListApprovals),
        "approve" => Ok(Request::ApproveAction {
            id: arguments.next().ok_or("missing approval ID")?,
        }),
        "deny" => Ok(Request::DenyAction {
            id: arguments.next().ok_or("missing approval ID")?,
        }),
        other => Err(format!("unknown approval action: {other}").into()),
    }
}

fn session_id(
    arguments: &mut impl Iterator<Item = String>,
    label: &str,
) -> Result<anclave_protocol::SessionId, Box<dyn std::error::Error>> {
    Ok(anclave_protocol::SessionId::new(
        arguments.next().ok_or_else(|| format!("missing {label}"))?,
    )?)
}

fn print_help() {
    println!(
        r"anclave-cli: headless client for the anclave daemon

USAGE
  anclave-cli COMMAND [ARGS]

COMMANDS
  daemon status                    is a daemon reachable, and which version
  daemon sandbox                   what containment this host can provide
  ping                             round-trip the daemon
  version                          the daemon's protocol and build version
  session list                     every session the daemon knows
  session get ID
  session create NAME [AGENT] [workspace options]
  session restart ID               resume the agent where it can
  session delete ID
  session capture ID               the session's current screen
  session send ID TEXT             type TEXT into the session
  approval list                    actions waiting on a decision
  approval approve ID              allow one
  approval deny ID                 refuse one
  migrate inspect SOURCE           what importing SOURCE would do
  migrate import SOURCE [--apply]  import it; a dry run without --apply
  plugin list                      UI plugins, their trust and capabilities
  plugin trust PATH                grant a UI plugin its declared capabilities
  plugin revoke PATH               withdraw that grant
  audit verify PATH                check an audit log's hash chain
                                   (reads a file; needs no daemon)

WORKSPACE OPTIONS (session create)
  --branch NAME                    branch each --repo member is worktreed on
  --repo PATH                      repository with its own worktree (repeatable)
  --dir PATH                       directory attached as it is (repeatable)
  --security PROFILE               security profile to run under
                                   (default: the daemon's configured default,
                                   which is uncontained unless changed)

  With one member the agent runs in that repository; with several it runs in a
  directory gathering them, each under its own name.

ENVIRONMENT
  ANCLAVE_SOCKET                   daemon socket (default /tmp/anclaved.sock)
  ANCLAVE_HOST                     ssh destination of a remote daemon; its
                                   socket is forwarded and used instead
  ANCLAVE_REMOTE_SOCKET            the remote daemon's socket
                                   (default /tmp/anclaved.sock)

EXIT CODES
  0  success
  1  the daemon refused the request
  2  usage error
  3  the daemon could not be reached"
    );
}

/// Print a migration report as something reviewable.
///
/// A wall of JSON is not a review. The counts come last because they are the
/// summary, and the skips carry their reasons because a refusal without one
/// cannot be acted on.
fn print_migration(report: &anclave_protocol::MigrationReport) {
    use anclave_protocol::MigrationAction;

    println!("source: {}", report.source);
    println!(
        "{}",
        if report.applied {
            "applied"
        } else {
            "dry run: nothing was written"
        }
    );
    println!();

    for item in &report.items {
        let mark = match item.action {
            MigrationAction::Import => "import",
            MigrationAction::AlreadyPresent => "present",
            MigrationAction::Skip => "SKIP",
        };
        match &item.detail {
            Some(detail) if !detail.is_empty() => {
                println!("  {mark:<8} {:<10} {:<20} {detail}", item.kind, item.name)
            }
            _ => println!("  {mark:<8} {:<10} {}", item.kind, item.name),
        }
    }

    println!();
    println!(
        "{} to import, {} already present, {} skipped",
        report.count(MigrationAction::Import),
        report.count(MigrationAction::AlreadyPresent),
        report.count(MigrationAction::Skip)
    );
    if let Some(rollback) = &report.rollback {
        println!("rollback record: {rollback}");
    }
    if !report.applied {
        println!("re-run with --apply to perform it");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(args: &[&str]) -> Request {
        session_request(&mut args.iter().map(|arg| (*arg).to_owned())).unwrap()
    }

    #[test]
    fn create_without_workspace_flags_has_no_workspace() {
        let Request::CreateSession(request) = request(&["create", "demo"]) else {
            panic!("expected create request")
        };
        assert!(request.workspace.is_none());
    }

    #[test]
    fn create_gathers_repos_and_dirs_into_one_workspace() {
        let Request::CreateSession(request) = request(&[
            "create",
            "demo",
            "claude",
            "--branch",
            "feat/x",
            "--repo",
            "/a",
            "--repo",
            "/b",
            "--dir",
            "/reference",
        ]) else {
            panic!("expected create request")
        };
        let workspace = request.workspace.expect("workspace");
        assert_eq!(workspace.members.len(), 3);
        // Worktree members share the branch; the attached dir keeps its own.
        assert_eq!(workspace.members[0].branch.as_deref(), Some("feat/x"));
        assert_eq!(workspace.members[1].branch.as_deref(), Some("feat/x"));
        assert_eq!(workspace.members[2].repository, "/reference");
        assert!(workspace.members[2].branch.is_none());
    }

    /// `AGENT` is optional, so flags may follow the name directly.
    ///
    /// This consumed `--repo` as the agent name and then failed with
    /// "unknown create option: /a", naming the path rather than the flag, so
    /// the usage documented in `--help` did not work.
    #[test]
    fn the_agent_may_be_omitted_before_workspace_flags() {
        let Request::CreateSession(request) =
            request(&["create", "demo", "--repo", "/a", "--branch", "feat/x"])
        else {
            panic!("expected create request")
        };
        assert_eq!(request.agent.as_str(), "default");
        let workspace = request.workspace.expect("workspace");
        assert_eq!(workspace.members.len(), 1);
        assert_eq!(workspace.members[0].repository, "/a");
        assert_eq!(workspace.members[0].branch.as_deref(), Some("feat/x"));
    }

    /// Naming the agent still works, and still selects that agent.
    #[test]
    fn a_named_agent_before_workspace_flags_is_kept() {
        let Request::CreateSession(request) = request(&[
            "create", "demo", "claude", "--repo", "/a", "--branch", "feat/x",
        ]) else {
            panic!("expected create request")
        };
        assert_eq!(request.agent.as_str(), "claude");
        assert_eq!(request.workspace.expect("workspace").members.len(), 1);
    }

    #[test]
    fn a_worktree_member_without_a_branch_is_rejected() {
        let mut args = ["create", "demo", "claude", "--repo", "/a"]
            .iter()
            .map(|s| (*s).to_owned());
        assert!(session_request(&mut args).is_err());
    }

    #[test]
    fn parses_restart() {
        let Request::RestartSession { id } = request(&["restart", "session-1"]) else {
            panic!("expected restart request")
        };
        assert_eq!(id.as_str(), "session-1");
    }

    #[test]
    fn daemon_status_is_the_default_daemon_action() {
        let mut empty = std::iter::empty::<String>();
        assert_eq!(daemon_request(&mut empty).unwrap(), Request::GetVersion);
        let mut named = ["status"].iter().map(|s| (*s).to_owned());
        assert_eq!(daemon_request(&mut named).unwrap(), Request::GetVersion);
        let mut bad = ["nope"].iter().map(|s| (*s).to_owned());
        assert!(daemon_request(&mut bad).is_err());
    }

    #[test]
    fn parses_capture() {
        let Request::CaptureScreen { id } = request(&["capture", "session-1"]) else {
            panic!("expected capture request")
        };
        assert_eq!(id.as_str(), "session-1");
    }

    #[test]
    fn parses_send_as_bytes() {
        let Request::SendInput { id, bytes } = request(&["send", "session-1", "hello"]) else {
            panic!("expected send request")
        };
        assert_eq!(id.as_str(), "session-1");
        assert_eq!(bytes, b"hello");
    }

    #[test]
    fn rejects_missing_send_text() {
        assert!(
            session_request(&mut ["send", "session-1"].iter().map(|s| (*s).to_owned())).is_err()
        );
    }
}
