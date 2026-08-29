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
            id: anclave_protocol::SessionId::new(arguments.next().ok_or("missing session ID")?)?,
        }),
        "delete" => Ok(Request::DeleteSession {
            id: anclave_protocol::SessionId::new(arguments.next().ok_or("missing session ID")?)?,
        }),
        "create" => Ok(Request::CreateSession(CreateSession {
            name: arguments.next().ok_or("missing session name")?,
            agent: AgentId::new("default")?,
            backend: BackendId::new("local")?,
            workspace: None,
        })),
        _ => Err(format!("unknown session action: {action}").into()),
    }
}

fn print_help() {
    println!(
        "anclave-cli ping|version|session list|session get ID|session create NAME|session delete ID"
    );
}
