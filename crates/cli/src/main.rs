use std::env;

use anclave_cli::{default_socket, Client};
use anclave_protocol::{AgentId, BackendId, CreateSession, Request, Response};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    let command = arguments.next().unwrap_or_else(|| "help".to_owned());
    let socket = env::var("ANCLAVE_SOCKET").unwrap_or_else(|_| default_socket().to_owned());
    let mut client = Client::connect(socket).await?;

    let request = match command.as_str() {
        "ping" => Request::Ping,
        "version" => Request::GetVersion,
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
        "create" => Ok(Request::CreateSession(CreateSession {
            name: arguments.next().ok_or("missing session name")?,
            agent: AgentId::new("default")?,
            backend: BackendId::new("local")?,
            workspace: None,
        })),
        _ => Err(format!("unknown session action: {action}").into()),
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
        "anclave-cli ping|version|session list|session get ID|session create NAME|session capture ID|session send ID TEXT|session delete ID"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(args: &[&str]) -> Request {
        session_request(&mut args.iter().map(|arg| (*arg).to_owned())).unwrap()
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
