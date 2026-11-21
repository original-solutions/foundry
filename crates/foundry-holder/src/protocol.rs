//! The length-prefixed socket protocol: attach, input, resize, status, close.
//!
//! Every frame is a big-endian `u32` length, then that many bytes: a version byte, a kind byte and
//! the body. A connection carries any number of requests. After `Attach` it also carries the
//! snapshot, the live output and, once the child exits, its status.
//!
//! The holder answers a frame it cannot read, another version included, with `Refused` and closes
//! the connection. That frame carries the holder's own version byte, so a newer client learns which
//! version to speak. A connection the holder closes with no `Refused` belonged to a client too slow
//! to keep up with the output, or one that hung up. The holder stops reading such a connection when
//! it closes it, so requests the client sent just before may not have been applied, and the close
//! can land part way through a frame. [`read_reply`] reports any end of the connection as `None`.
//! A client that was too slow can attach again.
//!
//! A running holder cannot be updated without ending its terminal, so this protocol changes
//! rarely, and every frame names the version it speaks.

use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The protocol version this build speaks.
pub const VERSION: u8 = 1;

/// The largest frame either side accepts, version and kind bytes included.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

/// Typed input the holder keeps while the child is not reading it. An `Input` that would take it
/// over this is refused and its connection closed, so input is never dropped from the middle of a
/// stream.
pub const MAX_PENDING_INPUT: usize = 4 * 1024 * 1024;

/// How long the holder lets a connection it is closing, after a `Refused` or after the client hung
/// up, take what is still queued for it before it cuts the connection off.
pub const CLOSE_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// The code `Exited` carries when the holder could not learn the child's exit status.
pub const UNKNOWN_EXIT: i32 = -1;

// A full scrollback fits in one Snapshot frame (kind, version and start offset included), and
// the largest input the holder keeps fits in one Input frame.
const _: () = assert!(crate::scrollback::MAX_BYTES + 10 <= MAX_FRAME);
const _: () = assert!(MAX_PENDING_INPUT + 2 <= MAX_FRAME);

const ATTACH: u8 = 1;
const INPUT: u8 = 2;
const RESIZE: u8 = 3;
const STATUS: u8 = 4;
const CLOSE: u8 = 5;
const SNAPSHOT: u8 = 0x81;
const OUTPUT: u8 = 0x82;
const STATUS_REPLY: u8 = 0x84;
const REFUSED: u8 = 0x85;

/// What a client asks of the holder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Send the snapshot, then live output, on this connection.
    Attach,
    /// Bytes typed into the terminal. See [`MAX_PENDING_INPUT`].
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
    /// The kept screen and scrollback, the first reply to `Attach`. `start` is the offset of its
    /// first byte in everything the child has written, so the live output that follows starts at
    /// `start + bytes.len()`. A client that reattaches skips what it already has, or sees a gap.
    Snapshot {
        start: u64,
        bytes: Bytes,
    },
    /// Live output, after the snapshot.
    Output(Bytes),
    Status(ChildStatus),
    /// Why the holder is closing this connection.
    Refused(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildStatus {
    Running,
    /// The exit code, 128 plus the signal number when a signal ended the child, or
    /// [`UNKNOWN_EXIT`].
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
                let [c0, c1] = cols.to_be_bytes();
                let [r0, r1] = rows.to_be_bytes();
                frame(RESIZE, &[c0, c1, r0, r1])
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
        let (kind, head, head_len, body) = self.parts();
        let head = &head[..head_len];
        let mut frame = Vec::with_capacity(6 + head.len() + body.len());
        frame.extend_from_slice(&header(kind, head.len() + body.len()));
        frame.extend_from_slice(head);
        frame.extend_from_slice(body);
        frame
    }

    /// Encodes an `Output` frame for `data` into `out`, reusing its capacity.
    pub fn encode_output_into(data: &[u8], out: &mut Vec<u8>) {
        out.clear();
        out.reserve(6 + data.len());
        out.extend_from_slice(&header(OUTPUT, data.len()));
        out.extend_from_slice(data);
    }

    /// Encodes a `Snapshot` frame of `len` bytes at `start` into `out`, writing the payload
    /// directly via `write_bytes` without intermediate copies.
    pub fn encode_snapshot_from(
        start: u64,
        len: usize,
        write_bytes: impl FnOnce(&mut Vec<u8>),
        out: &mut Vec<u8>,
    ) {
        out.clear();
        out.reserve(14 + len);
        out.extend_from_slice(&header(SNAPSHOT, 8 + len));
        out.extend_from_slice(&start.to_be_bytes());
        write_bytes(out);
    }

    /// Decodes a frame's payload: everything after the length prefix.
    pub fn decode(payload: &[u8]) -> Result<Self, Error> {
        let (kind, body) = open(payload)?;
        match kind {
            SNAPSHOT => match body.split_first_chunk::<8>() {
                Some((start, bytes)) => Ok(Reply::Snapshot {
                    start: u64::from_be_bytes(*start),
                    bytes: Bytes::copy_from_slice(bytes),
                }),
                None => Err(Error::Malformed("snapshot")),
            },
            OUTPUT => Ok(Reply::Output(Bytes::copy_from_slice(body))),
            STATUS_REPLY => match body {
                [0] => Ok(Reply::Status(ChildStatus::Running)),
                [1, a, b, c, d] => Ok(Reply::Status(ChildStatus::Exited(i32::from_be_bytes([
                    *a, *b, *c, *d,
                ])))),
                _ => Err(Error::Malformed("status")),
            },
            REFUSED => String::from_utf8(body.to_vec())
                .map(Reply::Refused)
                .map_err(|_| Error::Malformed("refusal")),
            other => Err(Error::Kind(other)),
        }
    }

    /// The kind, a short inline head and the body, so a large body is copied once into its frame.
    fn parts(&self) -> (u8, [u8; 8], usize, &[u8]) {
        match self {
            Reply::Snapshot { start, bytes } => (SNAPSHOT, start.to_be_bytes(), 8, bytes),
            Reply::Output(bytes) => (OUTPUT, [0; 8], 0, bytes),
            Reply::Status(ChildStatus::Running) => (STATUS_REPLY, [0; 8], 1, &[]),
            Reply::Status(ChildStatus::Exited(code)) => {
                let [a, b, c, d] = code.to_be_bytes();
                (STATUS_REPLY, [1, a, b, c, d, 0, 0, 0], 5, &[])
            }
            Reply::Refused(reason) => (REFUSED, [0; 8], 0, reason.as_bytes()),
        }
    }
}

/// Reads one request. `None` when the connection ended, a request cut off by the end included.
pub async fn read_request<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<Request>, Error> {
    read_payload(r)
        .await?
        .map(|payload| Request::decode(&payload))
        .transpose()
}

/// Reads one reply. `None` when the connection ended, a reply cut off by the end included.
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

fn frame(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(body.len() + 6);
    frame.extend_from_slice(&header(kind, body.len()));
    frame.extend_from_slice(body);
    frame
}

/// The length prefix, version and kind of a frame with a body of `body_len` bytes.
fn header(kind: u8, body_len: usize) -> [u8; 6] {
    let len = u32::try_from(body_len + 2).expect("a frame body fits in u32");
    let [a, b, c, d] = len.to_be_bytes();
    [a, b, c, d, VERSION, kind]
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
    if !read_or_end(r, &mut len).await? {
        return Ok(None);
    }
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(Error::TooLarge(len));
    }
    let mut payload = vec![0; len];
    if !read_or_end(r, &mut payload).await? {
        return Ok(None);
    }
    Ok(Some(payload))
}

/// Fills `buf`. False when the connection ended first, by a close or a reset.
async fn read_or_end<R: AsyncRead + Unpin>(r: &mut R, buf: &mut [u8]) -> Result<bool, Error> {
    match r.read_exact(buf).await {
        Ok(_) => Ok(true),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset
            ) =>
        {
            Ok(false)
        }
        Err(e) => Err(e.into()),
    }
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
            Reply::Snapshot {
                start: 0,
                bytes: Bytes::new(),
            },
            Reply::Snapshot {
                start: u64::MAX - 1,
                bytes: Bytes::from_static(b"screen\r\n"),
            },
            Reply::Refused("holder protocol version 9 is not supported".into()),
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

    /// The wire format a running holder speaks for its whole life. These bytes never change
    /// within a version.
    #[test]
    fn version_1_frames_are_pinned_byte_for_byte() {
        let requests: [(Request, &[u8]); 5] = [
            (Request::Attach, &[0, 0, 0, 2, 1, 1]),
            (
                Request::Input(Bytes::from_static(b"ab")),
                &[0, 0, 0, 4, 1, 2, b'a', b'b'],
            ),
            (
                Request::Resize {
                    cols: 132,
                    rows: 43,
                },
                &[0, 0, 0, 6, 1, 3, 0, 132, 0, 43],
            ),
            (Request::Status, &[0, 0, 0, 2, 1, 4]),
            (Request::Close, &[0, 0, 0, 2, 1, 5]),
        ];
        for (request, bytes) in requests {
            assert_eq!(request.encode(), bytes, "{request:?}");
            assert_eq!(Request::decode(&bytes[4..]).unwrap(), request);
        }
        let replies: [(Reply, &[u8]); 5] = [
            (
                Reply::Snapshot {
                    start: 258,
                    bytes: Bytes::from_static(b"hi"),
                },
                &[0, 0, 0, 12, 1, 0x81, 0, 0, 0, 0, 0, 0, 1, 2, b'h', b'i'],
            ),
            (
                Reply::Output(Bytes::from_static(b"x")),
                &[0, 0, 0, 3, 1, 0x82, b'x'],
            ),
            (
                Reply::Status(ChildStatus::Running),
                &[0, 0, 0, 3, 1, 0x84, 0],
            ),
            (
                Reply::Status(ChildStatus::Exited(7)),
                &[0, 0, 0, 7, 1, 0x84, 1, 0, 0, 0, 7],
            ),
            (
                Reply::Refused("no".into()),
                &[0, 0, 0, 4, 1, 0x85, b'n', b'o'],
            ),
        ];
        for (reply, bytes) in replies {
            assert_eq!(reply.encode(), bytes, "{reply:?}");
            assert_eq!(Reply::decode(&bytes[4..]).unwrap(), reply);
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
        assert!(matches!(
            Reply::decode(&[VERSION, 0x81, 0, 0, 0]),
            Err(Error::Malformed(_))
        ));
        assert!(matches!(
            Reply::decode(&[VERSION, 0x85, 0xff]),
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

    /// A reader whose connection was reset after `before` bytes.
    struct Reset {
        before: Vec<u8>,
    }

    impl AsyncRead for Reset {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            if self.before.is_empty() {
                return std::task::Poll::Ready(Err(std::io::ErrorKind::ConnectionReset.into()));
            }
            let n = self.before.len().min(buf.remaining());
            buf.put_slice(&self.before[..n]);
            self.before.drain(..n);
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn a_reset_connection_has_ended() {
        let frame = Request::Input(Bytes::from_static(b"abc")).encode();
        for before in [&frame[..0], &frame[..2], &frame[..7]] {
            let mut reply_side = Reset {
                before: before.to_vec(),
            };
            assert_eq!(read_reply(&mut reply_side).await.unwrap(), None);
            let mut request_side = Reset {
                before: before.to_vec(),
            };
            assert_eq!(read_request(&mut request_side).await.unwrap(), None);
        }
    }

    #[tokio::test]
    async fn a_connection_that_ends_part_way_through_a_frame_has_ended() {
        let (mut client, mut server) = tokio::io::duplex(64);
        let mut frame = Reply::Output(Bytes::from_static(b"cut off")).encode();
        frame.truncate(frame.len() - 3);
        tokio::io::AsyncWriteExt::write_all(&mut server, &frame)
            .await
            .unwrap();
        drop(server);
        assert_eq!(read_reply(&mut client).await.unwrap(), None);
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

    #[test]
    fn direct_encoders_match_reply_encode_byte_for_byte() {
        let mut out = Vec::new();
        Reply::encode_output_into(b"hello world\r\n", &mut out);
        assert_eq!(
            out,
            Reply::Output(Bytes::from_static(b"hello world\r\n")).encode()
        );

        let mut snap = Vec::new();
        Reply::encode_snapshot_from(258, 2, |dst| dst.extend_from_slice(b"hi"), &mut snap);
        assert_eq!(
            snap,
            Reply::Snapshot {
                start: 258,
                bytes: Bytes::from_static(b"hi"),
            }
            .encode()
        );
    }
}
