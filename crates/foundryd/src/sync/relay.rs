//! A terminal of another machine, carried over a link of its own.
//!
//! Machine A opens a terminal that machine B owns with [`Peers::open_terminal`]. It dials B with
//! the [`super::Dialer`] the record links use, so the link proves both machines' keys as every
//! peer link does, and opens it with a [`PeerMessage::Relay`] in place of a hello. B's
//! [`Peers::accept`] hands that link to [`serve`], which attaches to the terminal and carries it
//! until either end goes. Terminal bytes never ride the record link, where they would queue
//! behind a catch-up and could starve its heartbeats.
//!
//! A link carries text only, so each message is a JSON [`Frame`] with bytes as hex, in pieces
//! of at most [`CHUNK`] so no frame comes near a WebSocket's frame limit. B sends the snapshot
//! first, then output and refusals of input, and last a close with the code A's terminal socket
//! closes with; it then waits for A to close the link, so that close is never lost to a link
//! torn down under it. A sends input and resizes. Both ends send a beat every [`TICK`], and an
//! end that hears nothing for [`OFFLINE_AFTER`] takes the link as down, so a link that dies
//! without closing still ends.

use std::io;

use bytes::Bytes;
use foundry_proto::ids::{MachineId, TerminalId};
use foundry_proto::messages::{PROTOCOL, PeerMessage};
use futures::StreamExt as _;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::time::{Instant, timeout, timeout_at};

use super::link::{listed, send_message};
use super::{Conn, DIAL_TIMEOUT, Inner, OFFLINE_AFTER, Peers, TICK, Task, lock, on_store};
use crate::store::StoreError;
use crate::terminals::{Snapshot, TerminalError};

/// The close codes of the terminal socket (see [`crate::api::terminals`]): the child exited,
/// attach again, and any other end.
pub const EXITED: u16 = 1000;
pub const AGAIN: u16 = 1013;
pub const FAILED: u16 = 1011;

/// The most bytes one frame carries, before hex doubles them. A snapshot or input larger than
/// this goes in several frames, the last one marked.
const CHUNK: usize = 1024 * 1024;

/// The most bytes one snapshot or one input is put back together from: the holder's own frame
/// limit, which nothing larger could pass.
const MOST: usize = foundry_holder::protocol::MAX_FRAME;

/// One message on a relay link.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Frame {
    /// Owner to opener, first: a piece of the kept screen and scrollback.
    Snapshot {
        start: u64,
        bytes: String,
        last: bool,
    },
    /// Owner to opener: live output.
    Output { bytes: String },
    /// Owner to opener: input the terminal refused. The relay stays open.
    Refused { reason: String },
    /// Owner to opener, last: the code and reason the terminal socket closes with.
    Closed { code: u16, reason: String },
    /// Opener to owner: a piece of one typed input.
    Input { bytes: String, last: bool },
    /// Opener to owner.
    Resize { cols: u16, rows: u16 },
    /// Either way, every [`TICK`].
    Beat,
}

/// Why a relay did not open.
#[derive(Debug, thiserror::Error)]
pub enum RelayError {
    /// The owner, named here, could not be reached: try again once the link is back.
    #[error("the link to {0} is down")]
    Down(String),
    /// The owner refused the relay, or this machine cannot open one; the reason says why.
    #[error("{0}")]
    Refused(String),
}

/// How a terminal socket ends: its close code and reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Closed {
    pub code: u16,
    pub reason: String,
}

impl Closed {
    /// The child exited and every byte was sent.
    pub fn exited() -> Self {
        Self {
            code: EXITED,
            reason: "exited".to_owned(),
        }
    }

    /// How a terminal error ends the socket: attach again when the holder cut the client off,
    /// failed otherwise.
    pub fn of(error: &TerminalError) -> Self {
        let code = match error {
            TerminalError::Detached(_) => AGAIN,
            _ => FAILED,
        };
        Self {
            code,
            reason: error.to_string(),
        }
    }
}

/// What a relayed terminal says next.
#[derive(Debug)]
pub enum Event {
    Output(Bytes),
    /// Input the terminal refused; the relay stays open.
    Refused(String),
    /// The relay ended. A link that went down ends with 1013, so the client attaches again.
    Closed(Closed),
}

/// A terminal of another machine, open through this one.
pub struct Remote {
    send: Sender<String>,
    recv: Receiver<String>,
    /// When the owner was last heard from.
    heard: Instant,
    /// How the relay ends when the link goes down.
    down: Closed,
    _beat: Task,
}

impl Peers {
    /// The other machine that owns terminal `id`: the owner of its record, when that record is
    /// held, is not deleted and names a machine other than this one.
    pub async fn owner_of(&self, id: TerminalId) -> Result<Option<MachineId>, StoreError> {
        let me = self.inner.me;
        on_store(&self.inner.store, move |store| {
            Ok(store
                .terminal(id)?
                .filter(|record| !record.deleted && record.owner != me)
                .map(|record| record.owner))
        })
        .await
    }

    /// Opens terminal `id` of machine `owner` over a relay link: its snapshot, then the
    /// [`Remote`] that carries it. The link is dialed as the record links are, so a machine cut
    /// off from this one, or one silent past [`OFFLINE_AFTER`], is [`RelayError::Down`], as is
    /// a link that closes once the owner has started answering. An owner this machine hears
    /// from on its record link that closes the link without answering, as a build older than
    /// relays does, has refused it.
    pub async fn open_terminal(
        &self,
        owner: MachineId,
        id: TerminalId,
    ) -> Result<(Snapshot, Remote), RelayError> {
        let settings = on_store(&self.inner.store, |store| store.settings())
            .await
            .map_err(|e| RelayError::Refused(format!("cannot read the machine list: {e}")))?;
        let Some(peer) = settings.machines.into_iter().find(|m| m.id == owner) else {
            return Err(RelayError::Refused(format!(
                "machine {owner} is not in the machine list"
            )));
        };
        let down = || RelayError::Down(peer.name.clone());
        let Conn { send, mut recv } =
            match timeout(DIAL_TIMEOUT, self.inner.dialer.dial(&peer)).await {
                Ok(Ok(conn)) => conn,
                // The owner refused this machine's key.
                Ok(Err(e)) if e.kind() == io::ErrorKind::PermissionDenied => {
                    return Err(RelayError::Refused(e.to_string()));
                }
                Ok(Err(_)) | Err(_) => return Err(down()),
            };
        let open = PeerMessage::Relay {
            protocol: PROTOCOL,
            machine: self.inner.me,
            terminal: id,
        };
        if !send_message(&send, &open).await {
            return Err(down());
        }
        // Beat from now on: the owner ends a relay it hears nothing on, and a large snapshot
        // over a slow link takes longer than that.
        let beating = beat(send.clone());
        // The owner's attach answers within its own limit, which is shorter than this, and each
        // piece of the snapshot gives the next as long again: the limit is on silence, not on
        // the whole transfer.
        let mut deadline = Instant::now() + OFFLINE_AFTER;
        let mut kept = Vec::new();
        let mut answered = false;
        let snapshot = loop {
            let text = match timeout_at(deadline, recv.recv()).await {
                Ok(Some(text)) => text,
                Ok(None) if !answered && self.online().contains(&owner) => {
                    return Err(RelayError::Refused(format!(
                        "{} closed the relay without answering; it may run an older build",
                        peer.name
                    )));
                }
                Ok(None) | Err(_) => return Err(down()),
            };
            answered = true;
            deadline = Instant::now() + OFFLINE_AFTER;
            match serde_json::from_str(&text) {
                Ok(Frame::Snapshot { start, bytes, last }) => {
                    let Some(bytes) = unhex(&bytes).filter(|b| kept.len() + b.len() <= MOST) else {
                        return Err(RelayError::Refused(format!(
                            "{} sent a snapshot that cannot be read",
                            peer.name
                        )));
                    };
                    kept.extend_from_slice(&bytes);
                    if last {
                        break Snapshot {
                            start,
                            bytes: Bytes::from(kept),
                        };
                    }
                }
                Ok(Frame::Closed { reason, .. }) => return Err(RelayError::Refused(reason)),
                _ => {}
            }
        };
        let remote = Remote {
            _beat: beating,
            send,
            recv,
            heard: Instant::now(),
            down: Closed {
                code: AGAIN,
                reason: down().to_string(),
            },
        };
        Ok((snapshot, remote))
    }
}

impl Remote {
    /// What the terminal says next. Safe to cancel: nothing is lost when a `select!` drops it.
    pub async fn next(&mut self) -> Event {
        loop {
            let Ok(Some(text)) = timeout_at(self.heard + OFFLINE_AFTER, self.recv.recv()).await
            else {
                return Event::Closed(self.down.clone());
            };
            self.heard = Instant::now();
            match serde_json::from_str(&text) {
                Ok(Frame::Output { bytes }) => {
                    return match unhex(&bytes) {
                        Some(bytes) => Event::Output(bytes),
                        None => Event::Closed(Closed {
                            code: FAILED,
                            reason: "the terminal's owner sent output that cannot be read".into(),
                        }),
                    };
                }
                Ok(Frame::Refused { reason }) => return Event::Refused(reason),
                Ok(Frame::Closed { code, reason }) => {
                    // Only the codes the terminal socket gives pass through.
                    let code = if [EXITED, AGAIN].contains(&code) {
                        code
                    } else {
                        FAILED
                    };
                    return Event::Closed(Closed { code, reason });
                }
                // A beat, or a frame this build does not know.
                _ => {}
            }
        }
    }

    /// Sends typed input, in pieces the owner puts back together. An error is how the relay
    /// ends: the link is down.
    pub async fn input(&self, bytes: Bytes) -> Result<(), Closed> {
        for (piece, last) in pieces(&bytes) {
            self.send(&Frame::Input {
                bytes: hex(piece),
                last,
            })
            .await?;
        }
        Ok(())
    }

    pub async fn resize(&self, cols: u16, rows: u16) -> Result<(), Closed> {
        self.send(&Frame::Resize { cols, rows }).await
    }

    async fn send(&self, frame: &Frame) -> Result<(), Closed> {
        if send_message(&self.send, frame).await {
            Ok(())
        } else {
            Err(self.down.clone())
        }
    }
}

/// Serves a relay that `machine` opened for `terminal`, until either end goes. A machine that
/// is not a peer, a `Peers` serving no terminals, and a terminal this machine does not hold
/// are each answered with a close that says why. The machine list is read again every
/// [`TICK`], as the record link does, and a relay whose machine has left it closes the same way.
pub(super) async fn serve(inner: &Inner, machine: MachineId, terminal: TerminalId, conn: Conn) {
    let Conn { send, mut recv } = conn;
    let failed = |reason: String| Closed {
        code: FAILED,
        reason,
    };
    if machine == inner.me || !listed(inner, machine).await {
        tracing::warn!("sync: refused a relay from {machine}: not a peer of this machine");
        let reason = format!("machine {machine} is not a peer of this machine");
        return finish(&send, &mut recv, failed(reason)).await;
    }
    let Some(terminals) = lock(&inner.terminals).clone() else {
        let reason = "this machine serves no terminals".to_owned();
        return finish(&send, &mut recv, failed(reason)).await;
    };
    let (snapshot, mut output) = match terminals.attach(terminal).await {
        Ok(attached) => attached,
        Err(error) => return finish(&send, &mut recv, Closed::of(&error)).await,
    };
    for (piece, last) in pieces(&snapshot.bytes) {
        let frame = Frame::Snapshot {
            start: snapshot.start,
            bytes: hex(piece),
            last,
        };
        if !send_message(&send, &frame).await {
            return;
        }
    }
    let _beat = beat(send.clone());
    let mut heard = Instant::now();
    let mut typed = Vec::new();
    let mut every = tokio::time::interval(TICK);
    loop {
        tokio::select! {
            _ = every.tick() => {
                if !listed(inner, machine).await {
                    tracing::info!("sync: {machine} left the machine list; relay closed");
                    let reason = format!("machine {machine} is no longer a peer of this machine");
                    return finish(&send, &mut recv, failed(reason)).await;
                }
            }
            item = output.next() => {
                let bytes = match item {
                    Some(Ok(bytes)) => bytes,
                    None => return finish(&send, &mut recv, Closed::exited()).await,
                    Some(Err(error)) => return finish(&send, &mut recv, Closed::of(&error)).await,
                };
                for (piece, _) in pieces(&bytes) {
                    if !send_message(&send, &Frame::Output { bytes: hex(piece) }).await {
                        return;
                    }
                }
            }
            text = timeout_at(heard + OFFLINE_AFTER, recv.recv()) => {
                let Ok(Some(text)) = text else {
                    tracing::debug!("sync: the relay of terminal {terminal} to {machine} ended");
                    return;
                };
                heard = Instant::now();
                let done = match serde_json::from_str(&text) {
                    Ok(Frame::Input { bytes, last }) => {
                        let Some(bytes) = unhex(&bytes).filter(|b| typed.len() + b.len() <= MOST)
                        else {
                            let reason = format!("{machine} sent input that cannot be read");
                            return finish(&send, &mut recv, failed(reason)).await;
                        };
                        typed.extend_from_slice(&bytes);
                        if !last {
                            continue;
                        }
                        terminals.input(terminal, Bytes::from(std::mem::take(&mut typed))).await
                    }
                    Ok(Frame::Resize { cols, rows }) => terminals.resize(terminal, cols, rows).await,
                    // A beat, or a frame this build does not know.
                    _ => Ok(()),
                };
                match done {
                    Ok(()) => {}
                    Err(TerminalError::Refused { reason, .. }) => {
                        if !send_message(&send, &Frame::Refused { reason }).await {
                            return;
                        }
                    }
                    Err(error) => return finish(&send, &mut recv, Closed::of(&error)).await,
                }
            }
        }
    }
}

/// Sends the close that ends a relay, then waits, at most [`OFFLINE_AFTER`], for the opener to
/// close the link. Returning at once would let a keyed link's forwarder, or the WebSocket under
/// it, be dropped with the close still queued, and the opener would read a link gone down.
async fn finish(send: &Sender<String>, recv: &mut Receiver<String>, closed: Closed) {
    let frame = Frame::Closed {
        code: closed.code,
        reason: closed.reason,
    };
    if send_message(send, &frame).await {
        let _ = timeout(OFFLINE_AFTER, async {
            while recv.recv().await.is_some() {}
        })
        .await;
    }
}

/// Sends a beat every [`TICK`] on `send` until the link closes or the task is dropped.
pub(super) fn beat(send: Sender<String>) -> Task {
    Task(tokio::spawn(async move {
        let mut every = tokio::time::interval(TICK);
        loop {
            every.tick().await;
            if !send_message(&send, &Frame::Beat).await {
                return;
            }
        }
    }))
}

/// `bytes` in pieces of at most [`CHUNK`], each with whether it is the last. Empty bytes are
/// one empty last piece.
fn pieces(bytes: &[u8]) -> impl Iterator<Item = (&[u8], bool)> {
    let count = bytes.len().div_ceil(CHUNK).max(1);
    (0..count).map(move |i| {
        let end = bytes.len().min((i + 1) * CHUNK);
        (&bytes[i * CHUNK..end], i + 1 == count)
    })
}

const DIGITS: &[u8; 16] = b"0123456789abcdef";

pub(super) fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(char::from(DIGITS[usize::from(byte >> 4)]));
        text.push(char::from(DIGITS[usize::from(byte & 0xf)]));
    }
    text
}

pub(super) fn unhex(text: &str) -> Option<Bytes> {
    let digit = |c: u8| {
        char::from(c)
            .to_digit(16)
            .and_then(|d| u8::try_from(d).ok())
    };
    let text = text.as_bytes();
    if !text.len().is_multiple_of(2) {
        return None;
    }
    text.chunks(2)
        .map(|pair| Some(digit(pair[0])? << 4 | digit(pair[1])?))
        .collect::<Option<Vec<u8>>>()
        .map(Bytes::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_survive_hex_both_ways() {
        let every: Vec<u8> = (0..=255).collect();
        assert_eq!(unhex(&hex(&every)).as_deref(), Some(&every[..]));
        assert_eq!(hex(b"\x00\x1b[0m"), "001b5b306d");
    }

    #[test]
    fn hex_that_is_odd_or_not_hex_is_refused() {
        assert_eq!(unhex("abc"), None);
        assert_eq!(unhex("zz"), None);
    }

    /// The holder keeps up to 8 MiB of screen, which as one hex frame would pass the 16 MiB a
    /// WebSocket frame may hold.
    #[test]
    fn a_full_snapshot_goes_in_frames_well_under_a_websockets_limit_and_comes_back_whole() {
        let screen: Vec<u8> = (0..8 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
        let mut back = Vec::new();
        let mut lasts = Vec::new();
        for (piece, last) in pieces(&screen) {
            let frame = Frame::Snapshot {
                start: 7,
                bytes: hex(piece),
                last,
            };
            let text = serde_json::to_string(&frame).unwrap();
            assert!(
                text.len() < 4 * 1024 * 1024,
                "a frame of {} bytes",
                text.len()
            );
            back.extend_from_slice(&unhex(&hex(piece)).unwrap());
            lasts.push(last);
        }
        assert_eq!(back, screen);
        assert_eq!(lasts.iter().filter(|last| **last).count(), 1);
        assert_eq!(lasts.last(), Some(&true));
    }

    #[test]
    fn empty_bytes_are_one_empty_last_piece() {
        assert_eq!(pieces(b"").collect::<Vec<_>>(), vec![(&b""[..], true)]);
        assert_eq!(
            pieces(&[0; CHUNK])
                .map(|(p, l)| (p.len(), l))
                .collect::<Vec<_>>(),
            vec![(CHUNK, true)]
        );
    }
}
