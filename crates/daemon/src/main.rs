use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anclave_protocol::{Envelope, Event, Request};
use anclaved::listen::{clear_stale_socket, parse_args, restrict_socket};
use anclaved::{backend::LocalTmuxBackend, runtime::Runtime, storage::Storage};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, watch};
use tokio::time::{interval, Duration};

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
    let options = parse_args(std::env::args().skip(1))
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    if options.help {
        println!("{}", anclaved::listen::USAGE);
        return Ok(());
    }
    if options.version {
        println!("anclaved {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let socket = options.socket;

    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent)?;
    }
    clear_stale_socket(&socket)?;
    let listener = UnixListener::bind(&socket)?;
    restrict_socket(&socket)?;
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
    if let Ok(path) = std::env::var("ANCLAVE_SECURITY_FILE") {
        match std::fs::read_to_string(&path)
            .map_err(|error| error.to_string())
            .and_then(|text| {
                anclave_security::SecurityConfig::parse(&text).map_err(|error| error.to_string())
            }) {
            Ok(security) => runtime.set_security(security),
            // A security file that does not parse must stop the daemon, not
            // start it with the permissive defaults the operator was trying
            // to replace.
            Err(error) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("security config {path}: {error}"),
                ))
            }
        }
    }
    if let Ok(path) = std::env::var("ANCLAVE_WORKSPACE_ROOT") {
        runtime.set_workspace_root(path);
    }
    // Probe once at startup so the create path never pays for it, and so a
    // host with no containment available is discoverable before someone
    // creates a session that needs it.
    runtime.detect_sandbox_runtime();
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

    // SIGTERM/SIGINT must run the same shutdown path a `Shutdown` request
    // takes. The default action runs no cleanup, which left the socket file
    // behind and made the next start look like a live daemon.
    let signal_shutdown = shutdown_sender.clone();
    tokio::spawn(async move {
        if wait_for_terminate().await.is_ok() {
            let _ = signal_shutdown.send(true);
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
                let response = Envelope::new(
                    response.request_id,
                    anclave_protocol::Message::Response(response.payload),
                );
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
                        let bytes = anclave_protocol::encode(&Envelope::new(
                            None,
                            anclave_protocol::Message::Event(event),
                        ))
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

/// Resolve when the process is asked to stop.
async fn wait_for_terminate() -> io::Result<()> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    tokio::select! {
        _ = terminate.recv() => Ok(()),
        _ = interrupt.recv() => Ok(()),
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
