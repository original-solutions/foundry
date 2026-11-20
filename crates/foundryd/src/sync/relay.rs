//! A terminal of another machine, carried over a link of its own.
//!
//! Machine A opens a terminal that machine B owns with [`Peers::open_terminal`]. It dials B with
//! the [`super::Dialer`] the record links use, so the link proves both machines' keys as every
//! peer link does, and opens it with a [`PeerMessage::Relay`] in place of a hello. B's
//! [`Peers::accept`] hands that link to [`serve`], which attaches to the terminal and carries it
//! until either end goes. Terminal bytes never ride the record link, where they would queue
//! behind a catch-up and could starve its heartbeats.
//!
//! A link carries text only, so each message is a JSON [`Frame`] with bytes as hex. B sends
//! the snapshot first, then output and refusals of input, and last a close with the code A's
//! terminal socket closes with. A sends input and resizes. Both ends send a beat every
//! [`TICK`], and an end that hears nothing for [`OFFLINE_AFTER`] takes the link as down, so a
//! link that dies without closing still ends.

use std::io;

use bytes::Bytes;
use foundry_proto::ids::{MachineId, TerminalId};
use foundry_proto::messages::{PROTOCOL, PeerMessage};
use foundry_proto::record::AnyRecord;
use futures::StreamExt as _;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::time::{Instant, timeout, timeout_at};

use super::{Conn, DIAL_TIMEOUT, Inner, OFFLINE_AFTER, Peers, TICK, Task, lock, on_store};
use crate::store::StoreError;
use crate::terminals::{Snapshot, TerminalError};

/// The close codes of the terminal socket (see [`crate::api::terminals`]): the child exited,
/// attach again, and any other end.
const EXITED: u16 = 1000;
const AGAIN: u16 = 1013;
const FAILED: u16 = 1011;

/// One message on a relay link.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Frame {
    /// Owner to opener, first: the kept screen and scrollback.
    Snapshot { start: u64, bytes: String },
    /// Owner to opener: live output.
    Output { bytes: String },
    /// Owner to opener: input the terminal refused. The relay stays open.
    Refused { reason: String },
    /// Owner to opener, last: the code and reason the terminal socket closes with.
    Closed { code: u16, reason: String },
    /// Opener to owner: typed input.
    Input { bytes: String },
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

/// How a relay ended: the code and reason the terminal socket closes with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Closed {
    pub code: u16,
    pub reason: String,
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
            for owner in store.versions()?.into_keys().filter(|owner| *owner != me) {
                let held = match store.since(owner, 0) {
                    Ok(held) => held,
                    Err(error) => {
                        tracing::warn!("sync: records of {owner} not searched: {error}");
                        continue;
                    }
                };
                if held.iter().any(
                    |record| matches!(record, AnyRecord::Terminal(t) if t.body.id == id && !t.deleted),
                ) {
                    return Ok(Some(owner));
                }
            }
            Ok(None)
        })
        .await
    }

    /// Opens terminal `id` of machine `owner` over a relay link: its snapshot, then the
    /// [`Remote`] that carries it. The link is dialed as the record links are, so a machine cut
    /// off from this one is [`RelayError::Down`].
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
        if !send_text(&send, &open).await {
            return Err(down());
        }
        // The owner's attach answers within its own limit, which is shorter than this.
        let deadline = Instant::now() + OFFLINE_AFTER;
        let snapshot = loop {
            let Ok(Some(text)) = timeout_at(deadline, recv.recv()).await else {
                return Err(down());
            };
            match serde_json::from_str(&text) {
                Ok(Frame::Snapshot { start, bytes }) => match unhex(&bytes) {
                    Some(bytes) => break Snapshot { start, bytes },
                    None => {
                        return Err(RelayError::Refused(format!(
                            "{} sent a snapshot that cannot be read",
                            peer.name
                        )));
                    }
                },
                Ok(Frame::Closed { reason, .. }) => return Err(RelayError::Refused(reason)),
                _ => {}
            }
        };
        let remote = Remote {
            _beat: beat(send.clone()),
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

    /// Sends typed input. An error is how the relay ends: the link is down.
    pub async fn input(&self, bytes: Bytes) -> Result<(), Closed> {
        self.send(&Frame::Input { bytes: hex(&bytes) }).await
    }

    pub async fn resize(&self, cols: u16, rows: u16) -> Result<(), Closed> {
        self.send(&Frame::Resize { cols, rows }).await
    }

    async fn send(&self, frame: &Frame) -> Result<(), Closed> {
        if send_text(&self.send, frame).await {
            Ok(())
        } else {
            Err(self.down.clone())
        }
    }
}

/// Serves a relay that `machine` opened for `terminal`, until either end goes. A machine that
/// is not a peer, a `Peers` serving no terminals, and a terminal this machine does not hold
/// are each answered with a close that says why.
pub(super) async fn serve(inner: &Inner, machine: MachineId, terminal: TerminalId, conn: Conn) {
    let Conn { send, mut recv } = conn;
    let closed = |code, reason: String| Frame::Closed { code, reason };
    if machine == inner.me || !listed(inner, machine).await {
        tracing::warn!("sync: refused a relay from {machine}: not a peer of this machine");
        let reason = format!("machine {machine} is not a peer of this machine");
        send_text(&send, &closed(FAILED, reason)).await;
        return;
    }
    let Some(terminals) = lock(&inner.terminals).clone() else {
        let reason = "this machine serves no terminals".to_owned();
        send_text(&send, &closed(FAILED, reason)).await;
        return;
    };
    let (snapshot, mut output) = match terminals.attach(terminal).await {
        Ok(attached) => attached,
        Err(error) => {
            send_text(&send, &closed(code(&error), error.to_string())).await;
            return;
        }
    };
    let first = Frame::Snapshot {
        start: snapshot.start,
        bytes: hex(&snapshot.bytes),
    };
    if !send_text(&send, &first).await {
        return;
    }
    let _beat = beat(send.clone());
    let mut heard = Instant::now();
    loop {
        tokio::select! {
            item = output.next() => {
                let frame = match item {
                    Some(Ok(bytes)) => Frame::Output { bytes: hex(&bytes) },
                    None => {
                        send_text(&send, &closed(EXITED, "exited".to_owned())).await;
                        return;
                    }
                    Some(Err(error)) => {
                        send_text(&send, &closed(code(&error), error.to_string())).await;
                        return;
                    }
                };
                if !send_text(&send, &frame).await {
                    return;
                }
            }
            text = timeout_at(heard + OFFLINE_AFTER, recv.recv()) => {
                let Ok(Some(text)) = text else {
                    tracing::debug!("sync: the relay of terminal {terminal} to {machine} ended");
                    return;
                };
                heard = Instant::now();
                let done = match serde_json::from_str(&text) {
                    Ok(Frame::Input { bytes }) => match unhex(&bytes) {
                        Some(bytes) => terminals.input(terminal, bytes).await,
                        None => {
                            let reason = format!("{machine} sent input that cannot be read");
                            send_text(&send, &closed(FAILED, reason)).await;
                            return;
                        }
                    },
                    Ok(Frame::Resize { cols, rows }) => terminals.resize(terminal, cols, rows).await,
                    // A beat, or a frame this build does not know.
                    _ => Ok(()),
                };
                match done {
                    Ok(()) => {}
                    Err(TerminalError::Refused { reason, .. }) => {
                        if !send_text(&send, &Frame::Refused { reason }).await {
                            return;
                        }
                    }
                    Err(error) => {
                        send_text(&send, &closed(FAILED, error.to_string())).await;
                        return;
                    }
                }
            }
        }
    }
}

/// The close code for a terminal error: attach again after the holder cut the client off.
fn code(error: &TerminalError) -> u16 {
    match error {
        TerminalError::Detached(_) => AGAIN,
        _ => FAILED,
    }
}

async fn listed(inner: &Inner, machine: MachineId) -> bool {
    match on_store(&inner.store, |store| store.settings()).await {
        Ok(settings) => settings.machines.iter().any(|m| m.id == machine),
        Err(error) => {
            tracing::warn!("sync: cannot read the machine list: {error}");
            false
        }
    }
}

/// Sends a beat every [`TICK`] on `send` until the link closes or the task is dropped.
fn beat(send: Sender<String>) -> Task {
    Task(tokio::spawn(async move {
        let mut every = tokio::time::interval(TICK);
        loop {
            every.tick().await;
            if !send_text(&send, &Frame::Beat).await {
                return;
            }
        }
    }))
}

/// Sends `message` as JSON, and says whether the link is still open. A peer that takes
/// nothing for [`OFFLINE_AFTER`] ends the link.
async fn send_text(send: &Sender<String>, message: &impl Serialize) -> bool {
    let Ok(text) = serde_json::to_string(message) else {
        return true;
    };
    matches!(timeout(OFFLINE_AFTER, send.send(text)).await, Ok(Ok(())))
}

const DIGITS: &[u8; 16] = b"0123456789abcdef";

fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(char::from(DIGITS[usize::from(byte >> 4)]));
        text.push(char::from(DIGITS[usize::from(byte & 0xf)]));
    }
    text
}

fn unhex(text: &str) -> Option<Bytes> {
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
}
