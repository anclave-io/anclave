pub mod remote;

use std::fmt;
use std::io;
use std::path::Path;

use anclave_protocol::{Envelope, Event, Message, ProtocolError, Request, RequestId, Response};
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
    /// Events that arrived while waiting for a response.
    ///
    /// Both share the connection, so a request can and does read an event
    /// first. Dropping it there is how screen updates went missing; holding
    /// it here means `next_event` still sees it.
    pending_events: std::collections::VecDeque<Event>,
}

impl Client {
    pub async fn connect(path: impl AsRef<Path>) -> Result<Self, ClientError> {
        Ok(Self {
            stream: UnixStream::connect(path).await.map_err(ClientError::Io)?,
            next_request: 1,
            pending_events: std::collections::VecDeque::new(),
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
            let frame: Envelope<Message> = read_frame(&mut self.stream).await?;
            if frame.protocol != anclave_protocol::PROTOCOL_VERSION {
                return Err(ClientError::Protocol(ProtocolError::UnsupportedProtocol));
            }
            match frame.payload {
                Message::Response(response) => return Ok(response),
                // Keep it: the caller asked for a response, but something is
                // still listening for this.
                Message::Event(event) => self.pending_events.push_back(event),
            }
        }
    }

    pub async fn next_event(&mut self) -> Result<Event, ClientError> {
        if let Some(event) = self.pending_events.pop_front() {
            return Ok(event);
        }
        loop {
            let frame: Envelope<Message> = read_frame(&mut self.stream).await?;
            if frame.protocol != anclave_protocol::PROTOCOL_VERSION {
                return Err(ClientError::Protocol(ProtocolError::UnsupportedProtocol));
            }
            match frame.payload {
                Message::Event(event) => return Ok(event),
                // A response nobody is waiting for: the request that wanted
                // it has gone. Discarding is correct; queuing would hand it
                // to the next unrelated request.
                Message::Response(_) => continue,
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

    /// An event arriving mid-request used to be decoded as a response,
    /// which failed and dropped the connection. It must be held instead, and
    /// still be delivered to `next_event`.
    #[tokio::test]
    async fn an_event_during_a_request_is_kept_not_dropped() {
        use anclave_protocol::{Event, SessionId};

        let path = std::env::temp_dir().join(format!("anclave-mux-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request: Envelope<Request> = read_frame(&mut stream).await.unwrap();
            // The event goes first, exactly as a busy daemon would send it.
            let event = Envelope::new(
                None,
                Message::Event(Event::ScreenChanged {
                    id: SessionId::new("session-0").unwrap(),
                }),
            );
            let bytes = anclave_protocol::encode(&event).unwrap();
            write_frame(&mut stream, &bytes).await.unwrap();

            let response = Envelope::new(request.request_id, Message::Response(Response::Pong));
            let bytes = anclave_protocol::encode(&response).unwrap();
            write_frame(&mut stream, &bytes).await.unwrap();
        });

        let mut client = Client::connect(&path).await.unwrap();
        assert_eq!(client.request(Request::Ping).await.unwrap(), Response::Pong);
        // The event survived the request.
        assert!(matches!(
            client.next_event().await.unwrap(),
            Event::ScreenChanged { .. }
        ));
        server.await.unwrap();
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn client_round_trips_requests() {
        let path = std::env::temp_dir().join(format!("anclave-cli-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request: Envelope<Request> = read_frame(&mut stream).await.unwrap();
            assert_eq!(request.payload, Request::Ping);
            let response = Envelope::new(request.request_id, Message::Response(Response::Pong));
            let bytes = anclave_protocol::encode(&response).unwrap();
            write_frame(&mut stream, &bytes).await.unwrap();
        });

        let mut client = Client::connect(&path).await.unwrap();
        assert_eq!(client.request(Request::Ping).await.unwrap(), Response::Pong);
        server.await.unwrap();
        let _ = std::fs::remove_file(path);
    }
}
