use std::io::{self, Read, Write};

use serde::{de::DeserializeOwned, Serialize};

use crate::{decode, encode, Envelope, ProtocolError, MAX_FRAME_BYTES};

/// Four-byte big-endian length prefix followed by one encoded payload.
pub fn write_frame<W, T>(writer: &mut W, value: &T) -> io::Result<()>
where
    W: Write,
    T: Serialize,
{
    let payload = encode(value).map_err(protocol_io_error)?;
    let length = u32::try_from(payload.len())
        .map_err(|_| protocol_io_error(ProtocolError::FrameTooLarge))?;
    writer.write_all(&length.to_be_bytes())?;
    writer.write_all(&payload)?;
    writer.flush()
}

/// Read exactly one bounded length-delimited payload.
pub fn read_frame<R, T>(reader: &mut R) -> Result<T, IpcError>
where
    R: Read,
    T: DeserializeOwned,
{
    let mut prefix = [0; 4];
    reader.read_exact(&mut prefix).map_err(IpcError::Io)?;
    let length = u32::from_be_bytes(prefix) as usize;
    if length > MAX_FRAME_BYTES {
        return Err(IpcError::Protocol(ProtocolError::FrameTooLarge));
    }

    let mut payload = vec![0; length];
    reader.read_exact(&mut payload).map_err(IpcError::Io)?;
    decode(&payload).map_err(IpcError::Protocol)
}

pub fn write_envelope<W, T>(writer: &mut W, envelope: &Envelope<T>) -> io::Result<()>
where
    W: Write,
    T: Serialize,
{
    write_frame(writer, envelope)
}

pub fn read_envelope<R, T>(reader: &mut R) -> Result<Envelope<T>, IpcError>
where
    R: Read,
    T: DeserializeOwned,
{
    read_frame(reader)
}

#[derive(Debug)]
pub enum IpcError {
    Io(io::Error),
    Protocol(ProtocolError),
}

fn protocol_io_error(error: ProtocolError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Request, RequestId, Response};

    #[test]
    fn frames_round_trip_multiple_messages() {
        let first = Envelope::new(Some(RequestId::new("one").unwrap()), Request::Ping);
        let second = Envelope::new(Some(RequestId::new("two").unwrap()), Request::GetVersion);
        let mut bytes = Vec::new();
        write_envelope(&mut bytes, &first).unwrap();
        write_envelope(&mut bytes, &second).unwrap();

        let mut reader = bytes.as_slice();
        assert_eq!(read_envelope::<_, Request>(&mut reader).unwrap(), first);
        assert_eq!(read_envelope::<_, Request>(&mut reader).unwrap(), second);
        assert!(reader.is_empty());
    }

    #[test]
    fn truncated_frame_is_an_io_error() {
        let bytes = [0, 0, 0, 10, b'{'];
        assert!(matches!(
            read_frame::<_, Response>(&mut &bytes[..]),
            Err(IpcError::Io(_))
        ));
    }

    #[test]
    fn oversized_frame_is_rejected_before_allocation() {
        let length = (MAX_FRAME_BYTES as u32).saturating_add(1).to_be_bytes();
        assert!(matches!(
            read_frame::<_, Response>(&mut length.as_slice()),
            Err(IpcError::Protocol(ProtocolError::FrameTooLarge))
        ));
    }
}
