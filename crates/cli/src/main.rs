use std::env;

use anclave_cli::{default_socket, Client};
use anclave_protocol::{
    AgentId, BackendId, CreateSession, MemberAccess, Request, Response, WorkspaceId,
    WorkspaceMember, WorkspaceSpec,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    let command = arguments.next().unwrap_or_else(|| "help".to_owned());
    let socket = env::var("ANCLAVE_SOCKET").unwrap_or_else(|_| default_socket().to_owned());
    // A missing daemon is the most common failure and deserves a sentence
    // rather than a Debug-formatted io::Error. Exit 2 separates "could not
    // reach the daemon" from "the daemon refused the request" (exit 1).
    let mut client = match Client::connect(&socket).await {
        Ok(client) => client,
        Err(error) => {
            eprintln!("cannot reach the anclave daemon at {socket}: {error}");
            eprintln!("start one with `anclaved --socket {socket}`, or set ANCLAVE_SOCKET.");
            std::process::exit(2);
        }
    };

    let request = match command.as_str() {
        "ping" => Request::Ping,
        "version" => Request::GetVersion,
        "daemon" => daemon_request(&mut arguments)?,
        "session" => session_request(&mut arguments)?,
        _ => {
            print_help();
            return Ok(());
        }
    };

    match client.request(request).await? {
        Response::Error { code, message } => {
            eprintln!("{code:?}: {message}");
            std::process::exit(1);
        }
        response => println!("{}", serde_json::to_string_pretty(&response)?),
    }
    Ok(())
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
            let agent_name = arguments.next().unwrap_or_else(|| "default".to_owned());
            let workspace = parse_workspace(&name, arguments)?;
            Ok(Request::CreateSession(CreateSession {
                name,
                agent: AgentId::new(agent_name)?,
                backend: BackendId::new("local")?,
                workspace,
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
) -> Result<Option<WorkspaceSpec>, Box<dyn std::error::Error>> {
    let mut branch: Option<String> = None;
    let mut members = Vec::new();
    let mut pending_worktrees = Vec::new();

    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--branch" => branch = Some(arguments.next().ok_or("--branch requires a name")?),
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
        return Ok(None);
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

    Ok(Some(WorkspaceSpec {
        id: WorkspaceId::new(format!("ws-{session_name}"))?,
        members: all,
    }))
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
        r"anclave-cli COMMAND

  daemon status                   is a daemon reachable, and which version
  ping | version
  session list
  session get ID
  session delete ID
  session restart ID
  session capture ID
  session send ID TEXT
  session create NAME [AGENT] [workspace options]

Workspace options for `session create`:
  --branch NAME    branch every --repo member gets its own worktree on
  --repo PATH      repository with its own worktree (repeatable)
  --dir PATH       repository attached as it is (repeatable)

With one member the agent runs in that repository; with several it runs in a
directory gathering them, each under its own name."
    );
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
