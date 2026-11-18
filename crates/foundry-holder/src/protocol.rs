//! The length-prefixed socket protocol: attach, input, resize, status, close.
//!
//! Every frame is a big-endian `u32` length, then that many bytes: a version byte, a kind byte and
//! the body. A connection carries any number of requests. After `Attach` it also carries the
//! snapshot, the live output and, once the child exits, its status.
//!
//! A running holder cannot be updated without ending its terminal, so this protocol changes
//! rarely, and every frame names the version it speaks.

use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The protocol version this build speaks.
pub const VERSION: u8 = 1;

/// The largest frame either side accepts, version and kind bytes included.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

const ATTACH: u8 = 1;
const INPUT: u8 = 2;
const RESIZE: u8 = 3;
const STATUS: u8 = 4;
const CLOSE: u8 = 5;
const SNAPSHOT: u8 = 0x81;
const OUTPUT: u8 = 0x82;
const STATUS_REPLY: u8 = 0x84;

/// What a client asks of the holder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Send the snapshot, then live output, on this connection.
    Attach,
    /// Bytes typed into the terminal.
    Input(Bytes),
    Resize {
        cols: u16,
        rows: u16,
    },
    /// Reply with the child's status.
    Status,
    /// End the terminal: hang up the child, remove the socket and exit.
    Close,
}

/// What the holder sends a client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// The kept screen and scrollback, the first reply to `Attach`.
    Snapshot(Bytes),
    /// Live output, after the snapshot.
    Output(Bytes),
    Status(ChildStatus),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildStatus {
    Running,
    /// The exit code, or 128 plus the signal number when a signal ended the child.
    Exited(i32),
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("holder protocol version {0} is not supported; this side speaks version {VERSION}")]
    Version(u8),
    #[error("unknown holder message kind {0}")]
    Kind(u8),
    #[error("malformed holder {0}")]
    Malformed(&'static str),
    #[error("holder frame of {0} bytes is over the {MAX_FRAME} byte limit")]
    TooLarge(usize),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl Request {
    /// The whole frame, length prefix included.
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Request::Attach => frame(ATTACH, &[]),
            Request::Input(bytes) => frame(INPUT, bytes),
            Request::Resize { cols, rows } => {
                frame(RESIZE, &[cols.to_be_bytes(), rows.to_be_bytes()].concat())
            }
            Request::Status => frame(STATUS, &[]),
            Request::Close => frame(CLOSE, &[]),
        }
    }

    /// Decodes a frame's payload: everything after the length prefix.
    pub fn decode(payload: &[u8]) -> Result<Self, Error> {
        let (kind, body) = open(payload)?;
        match kind {
            ATTACH => Ok(Request::Attach),
            INPUT => Ok(Request::Input(Bytes::copy_from_slice(body))),
            RESIZE => match body {
                [c0, c1, r0, r1] => Ok(Request::Resize {
                    cols: u16::from_be_bytes([*c0, *c1]),
                    rows: u16::from_be_bytes([*r0, *r1]),
                }),
                _ => Err(Error::Malformed("resize")),
            },
            STATUS => Ok(Request::Status),
            CLOSE => Ok(Request::Close),
            other => Err(Error::Kind(other)),
        }
    }
}

impl Reply {
    /// The whole frame, length prefix included.
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Reply::Snapshot(bytes) => frame(SNAPSHOT, bytes),
            Reply::Output(bytes) => frame(OUTPUT, bytes),
            Reply::Status(ChildStatus::Running) => frame(STATUS_REPLY, &[0]),
            Reply::Status(ChildStatus::Exited(code)) => {
                frame(STATUS_REPLY, &[&[1][..], &code.to_be_bytes()].concat())
            }
        }
    }

    /// Decodes a frame's payload: everything after the length prefix.
    pub fn decode(payload: &[u8]) -> Result<Self, Error> {
        let (kind, body) = open(payload)?;
        match kind {
            SNAPSHOT => Ok(Reply::Snapshot(Bytes::copy_from_slice(body))),
            OUTPUT => Ok(Reply::Output(Bytes::copy_from_slice(body))),
            STATUS_REPLY => match body {
                [0] => Ok(Reply::Status(ChildStatus::Running)),
                [1, a, b, c, d] => Ok(Reply::Status(ChildStatus::Exited(i32::from_be_bytes([
                    *a, *b, *c, *d,
                ])))),
                _ => Err(Error::Malformed("status")),
            },
            other => Err(Error::Kind(other)),
        }
    }
}

/// Reads one request. `None` when the peer closed the connection between frames.
pub async fn read_request<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<Request>, Error> {
    read_payload(r)
        .await?
        .map(|payload| Request::decode(&payload))
        .transpose()
}

/// Reads one reply. `None` when the holder closed the connection between frames.
pub async fn read_reply<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<Reply>, Error> {
    read_payload(r)
        .await?
        .map(|payload| Reply::decode(&payload))
        .transpose()
}

pub async fn write_request<W: AsyncWrite + Unpin>(
    w: &mut W,
    request: &Request,
) -> Result<(), Error> {
    w.write_all(&request.encode()).await?;
    Ok(w.flush().await?)
}

pub async fn write_reply<W: AsyncWrite + Unpin>(w: &mut W, reply: &Reply) -> Result<(), Error> {
    w.write_all(&reply.encode()).await?;
    Ok(w.flush().await?)
}

fn frame(kind: u8, body: &[u8]) -> Vec<u8> {
    let len = u32::try_from(body.len() + 2).expect("a frame body fits in u32");
    let mut frame = Vec::with_capacity(body.len() + 6);
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(&[VERSION, kind]);
    frame.extend_from_slice(body);
    frame
}

fn open(payload: &[u8]) -> Result<(u8, &[u8]), Error> {
    match payload {
        [version, ..] if *version != VERSION => Err(Error::Version(*version)),
        [_, kind, body @ ..] => Ok((*kind, body)),
        _ => Err(Error::Malformed("frame")),
    }
}

async fn read_payload<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<Vec<u8>>, Error> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(Error::TooLarge(len));
    }
    let mut payload = vec![0; len];
    r.read_exact(&mut payload).await?;
    Ok(Some(payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_request_round_trips() {
        for request in [
            Request::Attach,
            Request::Input(Bytes::from_static(b"ls -la\r\0\xff")),
            Request::Resize {
                cols: 132,
                rows: 43,
            },
            Request::Status,
            Request::Close,
        ] {
            let frame = request.encode();
            let len = u32::from_be_bytes(frame[..4].try_into().unwrap()) as usize;
            assert_eq!(len, frame.len() - 4);
            assert_eq!(frame[4], VERSION);
            assert_eq!(Request::decode(&frame[4..]).unwrap(), request);
        }
    }

    #[test]
    fn every_reply_round_trips() {
        for reply in [
            Reply::Snapshot(Bytes::from_static(b"screen\r\n")),
            Reply::Output(Bytes::from_static(b"\x1b[31mred\x1b[0m")),
            Reply::Status(ChildStatus::Running),
            Reply::Status(ChildStatus::Exited(0)),
            Reply::Status(ChildStatus::Exited(-1)),
            Reply::Status(ChildStatus::Exited(137)),
        ] {
            let frame = reply.encode();
            assert_eq!(Reply::decode(&frame[4..]).unwrap(), reply);
        }
    }

    #[test]
    fn another_version_is_refused_by_number() {
        let mut frame = Request::Status.encode();
        frame[4] = VERSION + 1;
        let err = Request::decode(&frame[4..]).unwrap_err();
        assert!(matches!(err, Error::Version(v) if v == VERSION + 1));
        assert!(err.to_string().contains(&(VERSION + 1).to_string()));
    }

    #[test]
    fn an_unknown_kind_or_a_short_body_is_an_error() {
        assert!(matches!(
            Request::decode(&[VERSION, 99]),
            Err(Error::Kind(99))
        ));
        assert!(matches!(
            Request::decode(&[VERSION, 3, 0, 80]),
            Err(Error::Malformed(_))
        ));
        assert!(matches!(
            Reply::decode(&[VERSION]),
            Err(Error::Malformed(_))
        ));
    }

    #[tokio::test]
    async fn frames_are_read_in_order_and_eof_ends_the_stream() {
        let (mut client, mut server) = tokio::io::duplex(1024);
        write_request(&mut client, &Request::Input(Bytes::from_static(b"a")))
            .await
            .unwrap();
        write_request(&mut client, &Request::Close).await.unwrap();
        drop(client);
        assert_eq!(
            read_request(&mut server).await.unwrap(),
            Some(Request::Input(Bytes::from_static(b"a")))
        );
        assert_eq!(
            read_request(&mut server).await.unwrap(),
            Some(Request::Close)
        );
        assert_eq!(read_request(&mut server).await.unwrap(), None);
    }

    #[tokio::test]
    async fn an_oversized_frame_is_refused_before_it_is_read() {
        let (mut client, mut server) = tokio::io::duplex(64);
        let len = u32::try_from(MAX_FRAME + 1).unwrap();
        tokio::io::AsyncWriteExt::write_all(&mut client, &len.to_be_bytes())
            .await
            .unwrap();
        assert!(matches!(
            read_reply(&mut server).await,
            Err(Error::TooLarge(n)) if n == MAX_FRAME + 1
        ));
    }
}
