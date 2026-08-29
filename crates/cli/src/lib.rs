use std::fmt;
use std::io;
use std::path::Path;

use anclave_protocol::{Envelope, Event, ProtocolError, Request, RequestId, Response};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

#[derive(Debug)]
pub enum ClientError {
    Io(io::Error),
    Protocol(ProtocolError),
    UnexpectedResponse,
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "I/O error: {error}"),
            Self::Protocol(error) => write!(f, "protocol error: {error}"),
            Self::UnexpectedResponse => f.write_str("unexpected response"),
        }
    }
}

impl std::error::Error for ClientError {}

pub struct Client {
    stream: UnixStream,
    next_request: u64,
}

impl Client {
    pub async fn connect(path: impl AsRef<Path>) -> Result<Self, ClientError> {
        Ok(Self {
            stream: UnixStream::connect(path).await.map_err(ClientError::Io)?,
            next_request: 1,
        })
    }

    pub async fn shutdown(&mut self) -> Result<Response, ClientError> {
        self.request(Request::Shutdown).await
    }

    pub async fn subscribe(&mut self) -> Result<Response, ClientError> {
        self.request(Request::SubscribeEvents).await
    }

    pub async fn request(&mut self, request: Request) -> Result<Response, ClientError> {
        let request_id =
            RequestId::new(format!("cli-{}", self.next_request)).map_err(ClientError::Protocol)?;
        self.next_request += 1;
        let envelope = Envelope::new(Some(request_id), request);
        let bytes = anclave_protocol::encode(&envelope).map_err(ClientError::Protocol)?;
        write_frame(&mut self.stream, &bytes)
            .await
            .map_err(ClientError::Io)?;
        loop {
            let response: Envelope<Response> = read_frame(&mut self.stream).await?;
            if response.protocol != anclave_protocol::PROTOCOL_VERSION {
                return Err(ClientError::Protocol(ProtocolError::UnsupportedProtocol));
            }
            if response.request_id.is_some() {
                return Ok(response.payload);
            }
        }
    }

    pub async fn next_event(&mut self) -> Result<Event, ClientError> {
        loop {
            let event: Envelope<Event> = read_frame(&mut self.stream).await?;
            if event.protocol != anclave_protocol::PROTOCOL_VERSION {
                return Err(ClientError::Protocol(ProtocolError::UnsupportedProtocol));
            }
            if event.request_id.is_none() {
                return Ok(event.payload);
            }
        }
    }
}

async fn write_frame(stream: &mut UnixStream, payload: &[u8]) -> io::Result<()> {
    let length = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "frame too large"))?;
    stream.write_all(&length.to_be_bytes()).await?;
    stream.write_all(payload).await?;
    stream.flush().await
}

async fn read_frame<T: serde::de::DeserializeOwned>(
    stream: &mut UnixStream,
) -> Result<T, ClientError> {
    let mut prefix = [0; 4];
    stream
        .read_exact(&mut prefix)
        .await
        .map_err(ClientError::Io)?;
    let length = u32::from_be_bytes(prefix) as usize;
    if length > anclave_protocol::MAX_FRAME_BYTES {
        return Err(ClientError::Protocol(ProtocolError::FrameTooLarge));
    }
    let mut payload = vec![0; length];
    stream
        .read_exact(&mut payload)
        .await
        .map_err(ClientError::Io)?;
    anclave_protocol::decode(&payload).map_err(ClientError::Protocol)
}

pub fn default_socket() -> &'static str {
    "/tmp/anclaved.sock"
}

#[cfg(test)]
mod tests {
    use super::*;
    use anclave_protocol::{Request, Response};
    use tokio::net::UnixListener;

    #[tokio::test]
    async fn client_round_trips_requests() {
        let path = std::env::temp_dir().join(format!("anclave-cli-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request: Envelope<Request> = read_frame(&mut stream).await.unwrap();
            assert_eq!(request.payload, Request::Ping);
            let response = Envelope::new(request.request_id, Response::Pong);
            let bytes = anclave_protocol::encode(&response).unwrap();
            write_frame(&mut stream, &bytes).await.unwrap();
        });

        let mut client = Client::connect(&path).await.unwrap();
        assert_eq!(client.request(Request::Ping).await.unwrap(), Response::Pong);
        server.await.unwrap();
        let _ = std::fs::remove_file(path);
    }
}
