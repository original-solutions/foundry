//! One link to a peer: hello, catch-up, then live sends and heartbeats until it ends.
//!
//! The dialing end says hello first. The accepting end waits for that hello and answers in
//! the lower of the two protocol numbers, and from then on each end sends in that protocol, so
//! a newer machine speaks the older form to an older peer. With one protocol so far, the forms
//! differ only in the number; a later protocol that changes a message's shape converts it to
//! the older shape where [`records_message`] builds it.
//!
//! Past the hellos, sending and reading run side by side, so a send that waits on a slow peer
//! never stops this end hearing it.

use foundry_proto::ids::MachineId;
use foundry_proto::messages::{PROTOCOL, PeerMessage};
use foundry_proto::record::AnyRecord;
use serde::Serialize;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::time::{Instant, timeout, timeout_at};

use std::time::Duration;

use super::{BATCH, Conn, Inner, OFFLINE_AFTER, RETRY_REFUSED, TICK, on_store};
use crate::store::Applied;

/// How often a link repeats the warning that this machine's records cannot be read.
const WARN_EVERY: Duration = Duration::from_secs(60);

/// Runs a link until it closes, the peer is silent for [`OFFLINE_AFTER`], the peer is not (or
/// is no longer) in the machine list, or the peer takes nothing for [`OFFLINE_AFTER`].
/// `dialed` is the machine this end dialed, or `None` for a link a peer opened. `first` is the
/// peer's first message when the caller has already read and parsed it, so the hello is
/// neither read nor parsed again.
pub(super) async fn run(
    inner: &Inner,
    conn: Conn,
    dialed: Option<MachineId>,
    first: Option<PeerMessage>,
) {
    let Conn { send, mut recv } = conn;
    let Some(open) = hello(inner, &send, &mut recv, dialed, first).await else {
        return;
    };
    tokio::select! {
        () = sending(inner, &send, &open) => {}
        () = reading(inner, &mut recv, open.peer) => {}
    }
}

/// Sends this machine's new records every [`TICK`] until the peer leaves the machine list or
/// stops taking them.
async fn sending(inner: &Inner, send: &Sender<String>, open: &Open) {
    let mut sent = open.sent;
    let mut warned: Option<Instant> = None;
    let mut every = tokio::time::interval(TICK);
    loop {
        every.tick().await;
        if !listed(inner, open.peer).await {
            tracing::info!("sync: {} left the machine list; link closed", open.peer);
            return;
        }
        if !send_own(inner, send, open, &mut sent, &mut warned).await {
            return;
        }
    }
}

/// Takes the peer's messages until the link closes or the peer is silent for
/// [`OFFLINE_AFTER`]. Each message taken counts as hearing from the peer.
///
/// After a message this machine cannot take (one it cannot read, or a record the store
/// refuses), the link takes nothing more from the peer and no longer counts it as heard, so
/// the peer shows offline and its sessions unreachable within [`OFFLINE_AFTER`]. Records
/// arrive in version order, so the store keeps a prefix and loses nothing. The link stays open
/// for [`RETRY_REFUSED`], then ends, so the next link's hello asks again for everything past
/// that prefix: a passing store error clears then, and a record this build can never take is
/// resent once per link rather than once a second.
async fn reading(inner: &Inner, recv: &mut Receiver<String>, peer: MachineId) {
    let mut refused: Option<Instant> = None;
    loop {
        let text = match timeout(OFFLINE_AFTER, recv.recv()).await {
            Ok(Some(text)) => text,
            Ok(None) => return,
            Err(_) => {
                tracing::debug!("sync: {peer} has been silent for {OFFLINE_AFTER:?}");
                return;
            }
        };
        if let Some(at) = refused {
            if at.elapsed() >= RETRY_REFUSED {
                tracing::info!("sync: link to {peer} closed to ask again for what it refused");
                return;
            }
            continue;
        }
        match receive(inner, peer, &text).await {
            Ok(()) => inner.heard_from(peer),
            Err(error) => {
                tracing::warn!(
                    "sync: {error}; nothing more is taken from {peer} on this link, which \
                     closes in {RETRY_REFUSED:?} to ask again, and it shows offline until a link \
                     takes its records"
                );
                refused = Some(Instant::now());
            }
        }
    }
}

/// Sends this machine's records above `sent`, reading and sending one message of at most
/// [`BATCH`] at a time and moving `sent` past each one sent, and says whether the link is
/// still open. When nothing was sent, it sends one empty message: the heartbeat goes out
/// either way, so a read error never makes this machine look offline.
///
/// A record of this machine's own that cannot be read (one a newer build wrote before a
/// rollback) stops the send at the batch that holds it: the batches below it go, and nothing
/// from that batch on goes to this peer, which then shows this machine online with the records
/// it had before. The warning says so once a minute until the row is fixed; reading around
/// such a row belongs with rollback (task 18).
async fn send_own(
    inner: &Inner,
    send: &Sender<String>,
    open: &Open,
    sent: &mut u64,
    warned: &mut Option<Instant>,
) -> bool {
    let mut sent_any = false;
    loop {
        let (me, above) = (inner.me, *sent);
        let batch = match on_store(&inner.store, move |store| {
            store.since_limited(me, above, BATCH)
        })
        .await
        {
            Ok(batch) => batch,
            Err(error) => {
                if warned.is_none_or(|at| at.elapsed() >= WARN_EVERY) {
                    tracing::warn!(
                        "sync: this machine's records above version {above} cannot be read, \
                         so {} is not sent them: {error}",
                        open.peer
                    );
                    *warned = Some(Instant::now());
                }
                break;
            }
        };
        let Some(newest) = batch.last().map(AnyRecord::version) else {
            break;
        };
        let full = batch.len() == BATCH;
        if !send_message(send, &records_message(open, batch)).await {
            return false;
        }
        *sent = newest;
        sent_any = true;
        if !full {
            break;
        }
    }
    sent_any || send_message(send, &records_message(open, Vec::new())).await
}

fn records_message(open: &Open, records: Vec<AnyRecord>) -> PeerMessage {
    PeerMessage::Records {
        protocol: open.speak,
        records,
    }
}

/// A link past its hellos.
struct Open {
    peer: MachineId,
    /// The protocol both ends speak: the lower of the two.
    speak: u16,
    /// The highest version of this machine's records the peer holds.
    sent: u64,
}

/// Exchanges hellos, and refuses a peer that is not in the machine list, is this machine, or
/// is not the machine that was dialed. A hello does not count as hearing from the peer: that
/// waits for a message this machine takes.
async fn hello(
    inner: &Inner,
    send: &Sender<String>,
    recv: &mut Receiver<String>,
    dialed: Option<MachineId>,
    first: Option<PeerMessage>,
) -> Option<Open> {
    if dialed.is_some() && !send_message(send, &own_hello(inner, PROTOCOL).await?).await {
        return None;
    }
    let first = match first {
        Some(message) => Some(message),
        None => {
            let text = timeout_at(Instant::now() + OFFLINE_AFTER, recv.recv())
                .await
                .ok()??;
            serde_json::from_str(&text).ok()
        }
    };
    let Some(PeerMessage::Hello {
        protocol,
        machine,
        versions,
    }) = first
    else {
        tracing::warn!("sync: a link opened without a hello");
        return None;
    };
    let allowed = match dialed {
        Some(expected) => machine == expected,
        None => machine != inner.me && listed(inner, machine).await,
    };
    if !allowed {
        tracing::warn!("sync: refused a link from {machine}: not a peer of this machine");
        return None;
    }
    let speak = protocol.min(PROTOCOL);
    if dialed.is_none() && !send_message(send, &own_hello(inner, speak).await?).await {
        return None;
    }
    Some(Open {
        peer: machine,
        speak,
        sent: versions.get(&inner.me).copied().unwrap_or(0),
    })
}

async fn own_hello(inner: &Inner, protocol: u16) -> Option<PeerMessage> {
    match on_store(&inner.store, |store| store.versions()).await {
        Ok(versions) => Some(PeerMessage::Hello {
            protocol,
            machine: inner.me,
            versions,
        }),
        Err(error) => {
            tracing::warn!("sync: cannot read the versions held: {error}");
            None
        }
    }
}

/// Whether `machine` is in the machine list: the one rule for who is a peer, for record links
/// and relays alike.
pub(super) async fn listed(inner: &Inner, machine: MachineId) -> bool {
    match on_store(&inner.store, |store| store.settings()).await {
        Ok(settings) => settings.machines.iter().any(|m| m.id == machine),
        Err(error) => {
            tracing::warn!("sync: cannot read the machine list: {error}");
            false
        }
    }
}

/// Sends `message`, and says whether the link is still open. A message that cannot be
/// written as JSON is skipped. A peer that takes nothing for [`OFFLINE_AFTER`] ends the link.
pub(super) async fn send_message(send: &Sender<String>, message: &impl Serialize) -> bool {
    let text = match serde_json::to_string(message) {
        Ok(text) => text,
        Err(error) => {
            tracing::warn!("sync: a message could not be written: {error}");
            return true;
        }
    };
    matches!(timeout(OFFLINE_AFTER, send.send(text)).await, Ok(Ok(())))
}

/// Takes the records in a message from `peer`, each only if `peer` owns it and it is newer.
/// A message that cannot be read, or a record that cannot be stored, is an error, and nothing
/// after it is taken: the store then holds a prefix of the peer's records.
async fn receive(inner: &Inner, peer: MachineId, text: &str) -> Result<(), String> {
    let records = match serde_json::from_str::<PeerMessage>(text) {
        Ok(PeerMessage::Records { records, .. }) => records,
        // Shared settings move only through the coordinator, which owns how they are sent. A
        // relay opens a link of its own and means nothing on a record link.
        Ok(PeerMessage::Settings { .. })
        | Ok(PeerMessage::Hello { .. })
        | Ok(PeerMessage::Relay { .. }) => return Ok(()),
        Err(error) => return Err(format!("a message from {peer} could not be read: {error}")),
    };
    on_store(&inner.store, move |store| {
        for record in &records {
            match store.apply(record, peer) {
                Ok(Applied::NotOwner) => tracing::warn!(
                    "sync: {peer} sent a record owned by {}; refused",
                    record.owner()
                ),
                Ok(Applied::Stored | Applied::Stale) => {}
                Err(error) => return Err(format!("a record from {peer} not stored: {error}")),
            }
        }
        Ok(())
    })
    .await
}
