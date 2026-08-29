use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anclave_protocol::{Envelope, Event, Request};
use anclaved::{backend::LocalTmuxBackend, runtime::Runtime, storage::Storage};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, watch};
use tokio::time::{interval, Duration};

const DEFAULT_SOCKET: &str = "/tmp/anclaved.sock";
const TMUX_SOCKET_PREFIX: &str = "anclave-tmux-";

fn tmux_socket_for(daemon_socket: &Path) -> String {
    let identity = daemon_socket.to_string_lossy();
    let digest = identity.bytes().fold(0xcbf29ce484222325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    });
    format!("/tmp/{TMUX_SOCKET_PREFIX}{digest:016x}")
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let socket = std::env::args()
        .skip(1)
        .find_map(|argument| argument.strip_prefix("--socket=").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET));

    if socket.exists() {
        std::fs::remove_file(&socket)?;
    }
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let listener = UnixListener::bind(&socket)?;
    let storage_path = socket.with_extension("db");
    let storage = Storage::open(&storage_path)
        .map_err(|error| io::Error::other(format!("open storage: {error}")))?;
    let tmux_socket = tmux_socket_for(&socket);
    let result = run(listener, storage, tmux_socket).await;
    let _ = std::fs::remove_file(&socket);
    result
}

async fn run(listener: UnixListener, storage: Storage, tmux_socket: String) -> io::Result<()> {
    let storage = Arc::new(Mutex::new(storage));
    let backend = Arc::new(LocalTmuxBackend::new(tmux_socket, "anclave"));
    let mut runtime = Runtime::new(storage, backend);
    if let Ok(path) = std::env::var("ANCLAVE_AGENTS_FILE") {
        if let Ok(agents) = anclaved::agent::AgentRegistry::load(path) {
            runtime.set_agents(agents);
        }
    }
    if let Ok(path) = std::env::var("ANCLAVE_WORKSPACE_ROOT") {
        runtime.set_workspace_root(path);
    }
    runtime.recover_sessions();
    let events = runtime.events();
    let (shutdown_sender, shutdown_receiver) = watch::channel(false);
    let polling_runtime = runtime.clone();
    let mut polling_shutdown = shutdown_receiver.clone();
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_millis(100));
        loop {
            tokio::select! {
                _ = ticker.tick() => polling_runtime.poll_backend(),
                result = polling_shutdown.changed() => {
                    if result.is_err() || *polling_shutdown.borrow() {
                        break;
                    }
                }
            }
        }
    });

    let listener = Arc::new(listener);
    loop {
        tokio::select! {
            result = listener.accept() => {
                let (stream, _) = result?;
                let client_runtime = runtime.clone();
                let client_events = events.clone();
                let client_shutdown = shutdown_receiver.clone();
                let client_shutdown_sender = shutdown_sender.clone();
                tokio::spawn(async move {
                    if let Err(error) = handle_client(
                        stream,
                        client_runtime,
                        client_events,
                        client_shutdown,
                        client_shutdown_sender,
                    ).await {
                        eprintln!("anclaved client error: {error}");
                    }
                });
            }
            result = wait_for_shutdown(shutdown_receiver.clone()) => {
                result?;
                return Ok(());
            }
        }
    }
}

async fn handle_client(
    mut stream: UnixStream,
    runtime: Runtime,
    events: anclaved::events::EventBus,
    mut shutdown: watch::Receiver<bool>,
    shutdown_sender: watch::Sender<bool>,
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
                let should_shutdown = matches!(request.payload, Request::Shutdown);
                if should_shutdown {
                    let _ = shutdown_sender.send(true);
                }
                let response = anclaved::runtime::handle_envelope(&runtime, request);
                let bytes = anclave_protocol::encode(&response)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                write_frame(&mut stream, &bytes).await?;
                if should_shutdown {
                    return Ok(());
                }
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return Ok(());
                }
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

async fn wait_for_shutdown(mut shutdown: watch::Receiver<bool>) -> io::Result<()> {
    if *shutdown.borrow() {
        return Ok(());
    }
    shutdown
        .changed()
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "shutdown channel closed"))?;
    Ok(())
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
