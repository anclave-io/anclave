use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anclave_protocol::{Envelope, Event, Request};
use anclaved::{
    backend::LocalTmuxBackend,
    events::EventBus,
    runtime::{handle_envelope, Runtime},
    storage::Storage,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast;

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
    let events = EventBus::new();
    loop {
        let (stream, _) = listener.accept().await?;
        let client_storage = Arc::clone(&storage);
        let client_backend = Arc::clone(&backend);
        let client_events = events.clone();
        tokio::spawn(async move {
            if let Err(error) =
                handle_client(stream, client_storage, client_backend, client_events).await
            {
                eprintln!("anclaved client error: {error}");
            }
        });
    }
}

async fn handle_client(
    mut stream: UnixStream,
    storage: Arc<Mutex<Storage>>,
    backend: Arc<LocalTmuxBackend>,
    events: EventBus,
) -> io::Result<()> {
    let runtime = Runtime::new(storage, backend);
    let mut subscription: Option<broadcast::Receiver<Event>> = None;
    loop {
        let mut prefix = [0; 4];
        if stream.read_exact(&mut prefix).await.is_err() {
            return Ok(());
        }
        let length = u32::from_be_bytes(prefix) as usize;
        if length > anclave_protocol::MAX_FRAME_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "frame too large",
            ));
        }
        let mut payload = vec![0; length];
        stream.read_exact(&mut payload).await?;

        let request: Envelope<Request> = anclave_protocol::decode(&payload)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if matches!(request.payload, Request::SubscribeEvents) {
            subscription = Some(events.subscribe());
        }
        let response = handle_envelope(&runtime, request);
        let bytes = anclave_protocol::encode(&response)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        write_frame_async(&mut stream, &bytes).await?;

        if let Some(receiver) = subscription.as_mut() {
            while let Ok(event) = receiver.try_recv() {
                let event = Envelope::new(None, event);
                let bytes = anclave_protocol::encode(&event)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                write_frame_async(&mut stream, &bytes).await?;
            }
        }
    }
}

async fn write_frame_async(stream: &mut UnixStream, payload: &[u8]) -> io::Result<()> {
    let length = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "frame too large"))?;
    stream.write_all(&length.to_be_bytes()).await?;
    stream.write_all(payload).await?;
    stream.flush().await
}
