use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anclave_protocol::{Envelope, Event, Request};
use anclaved::{backend::LocalTmuxBackend, runtime::Runtime, storage::Storage};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast;
use tokio::time::{interval, Duration};

const DEFAULT_SOCKET: &str = "/tmp/anclaved.sock";

#[tokio::main]
async fn main() -> io::Result<()> {
    let socket = std::env::args()
        .skip(1)
        .find_map(|argument| argument.strip_prefix("--socket=").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET));

    if socket.exists() {
        std::fs::remove_file(&socket)?;
    }
    let listener = UnixListener::bind(&socket)?;
    let storage_path = socket.with_extension("db");
    let storage = Storage::open(&storage_path)
        .map_err(|error| io::Error::other(format!("open storage: {error}")))?;
    let tmux_socket = format!("{}-tmux", socket.display());
    let result = run(listener, storage, tmux_socket).await;
    let _ = std::fs::remove_file(&socket);
    result
}

async fn run(listener: UnixListener, storage: Storage, tmux_socket: String) -> io::Result<()> {
    let storage = Arc::new(Mutex::new(storage));
    let backend = Arc::new(LocalTmuxBackend::new(tmux_socket, "anclave"));
    let runtime = Runtime::new(storage, backend);
    let events = runtime.events();
    let polling_runtime = runtime.clone();
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_millis(100));
        loop {
            ticker.tick().await;
            polling_runtime.poll_backend();
        }
    });

    loop {
        let (stream, _) = listener.accept().await?;
        let client_runtime = runtime.clone();
        let client_events = events.clone();
        tokio::spawn(async move {
            if let Err(error) = handle_client(stream, client_runtime, client_events).await {
                eprintln!("anclaved client error: {error}");
            }
        });
    }
}

async fn handle_client(
    mut stream: UnixStream,
    runtime: Runtime,
    events: anclaved::events::EventBus,
) -> io::Result<()> {
    let mut subscription: Option<broadcast::Receiver<Event>> = None;
    loop {
        tokio::select! {
            result = read_frame(&mut stream) => {
                let payload = match result {
                    Ok(payload) => payload,
                    Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
                    Err(error) => return Err(error),
                };
                let request: Envelope<Request> = anclave_protocol::decode(&payload)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                if matches!(request.payload, Request::SubscribeEvents) {
                    subscription = Some(events.subscribe());
                }
                let response = anclaved::runtime::handle_envelope(&runtime, request);
                let bytes = anclave_protocol::encode(&response)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                write_frame(&mut stream, &bytes).await?;
            }
            event = async {
                match subscription.as_mut() {
                    Some(receiver) => receiver.recv().await.map(Some),
                    None => std::future::pending().await,
                }
            } => {
                match event {
                    Ok(Some(event)) => {
                        let bytes = anclave_protocol::encode(&Envelope::new(None, event))
                            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                        write_frame(&mut stream, &bytes).await?;
                    }
                    Ok(None) | Err(broadcast::error::RecvError::Closed) => return Ok(()),
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }
        }
    }
}

async fn read_frame(stream: &mut UnixStream) -> io::Result<Vec<u8>> {
    let mut prefix = [0; 4];
    stream.read_exact(&mut prefix).await?;
    let length = u32::from_be_bytes(prefix) as usize;
    if length > anclave_protocol::MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut payload = vec![0; length];
    stream.read_exact(&mut payload).await?;
    Ok(payload)
}

async fn write_frame(stream: &mut UnixStream, payload: &[u8]) -> io::Result<()> {
    let length = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "frame too large"))?;
    stream.write_all(&length.to_be_bytes()).await?;
    stream.write_all(payload).await?;
    stream.flush().await
}
