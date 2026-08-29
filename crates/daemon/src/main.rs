use std::io;
use std::path::PathBuf;

use anclave_protocol::{Envelope, Request};
use anclaved::{
    runtime::{handle_envelope, Runtime},
    storage::Storage,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

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
    let result = run(listener, storage).await;
    let _ = std::fs::remove_file(&socket);
    result
}

async fn run(listener: UnixListener, storage: Storage) -> io::Result<()> {
    let storage = std::sync::Arc::new(std::sync::Mutex::new(storage));
    loop {
        let (stream, _) = listener.accept().await?;
        let client_storage = std::sync::Arc::clone(&storage);
        tokio::spawn(async move {
            if let Err(error) = handle_client(stream, client_storage).await {
                eprintln!("anclaved client error: {error}");
            }
        });
    }
}

async fn handle_client(
    mut stream: UnixStream,
    storage: std::sync::Arc<std::sync::Mutex<Storage>>,
) -> io::Result<()> {
    let mut runtime = Runtime::new(storage);
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
        let response = handle_envelope(&mut runtime, request);
        let bytes = anclave_protocol::encode(&response)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        write_frame_async(&mut stream, &bytes).await?;
    }
}

async fn write_frame_async(stream: &mut UnixStream, payload: &[u8]) -> io::Result<()> {
    let length = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "frame too large"))?;
    stream.write_all(&length.to_be_bytes()).await?;
    stream.write_all(payload).await?;
    stream.flush().await
}
