//! One link to a peer: hello, catch-up, then live sends and heartbeats until it ends.
//!
//! The dialing end says hello first. The accepting end waits for that hello and answers in
//! the lower of the two protocol numbers, and from then on each end sends in that protocol, so
//! a newer machine speaks the older form to an older peer. With one protocol so far, the forms
//! differ only in the number; a later protocol that changes a message's shape converts it to
//! the older shape where [`records_message`] builds it.

use foundry_proto::ids::MachineId;
use foundry_proto::messages::{PROTOCOL, PeerMessage};
use foundry_proto::record::AnyRecord;
use tokio::time::{Instant, sleep_until, timeout, timeout_at};

use super::{BATCH, Conn, Inner, OFFLINE_AFTER, TICK, lock};
use crate::store::Applied;

/// Runs a link until it closes, the peer is silent for [`OFFLINE_AFTER`], the peer is not (or
/// is no longer) in the machine list, or the peer sends a message this machine cannot take.
/// `dialed` is the machine this end dialed, or `None` for a link a peer opened.
pub(super) async fn run(inner: &Inner, mut conn: Conn, dialed: Option<MachineId>) {
    let Some(open) = hello(inner, &mut conn, dialed).await else {
        return;
    };
    let mut sent = open.sent;
    let mut warned = false;
    let mut last_heard = Instant::now();
    let mut every = tokio::time::interval(TICK);
    loop {
        tokio::select! {
            _ = every.tick() => {
                if !listed(inner, open.peer) {
                    tracing::info!("sync: {} left the machine list; link closed", open.peer);
                    return;
                }
                if !send_own(inner, &conn, &open, &mut sent, &mut warned).await {
                    return;
                }
            }
            text = conn.recv.recv() => {
                let Some(text) = text else {
                    return;
                };
                last_heard = Instant::now();
                inner.heard_from(open.peer);
                if !receive(inner, open.peer, &text) {
                    return;
                }
            }
            _ = sleep_until(last_heard + OFFLINE_AFTER) => {
                tracing::debug!("sync: {} has been silent for {OFFLINE_AFTER:?}", open.peer);
                return;
            }
        }
    }
}

/// Sends this machine's records above `sent` in messages of at most [`BATCH`], moving `sent`
/// past each one sent, and says whether the link is still open. With nothing new, or when the
/// records cannot be read, it sends one empty message: the heartbeat goes out either way, so a
/// read error never makes this machine look offline. The first read error on a link is a
/// warning; later ones are not repeated.
async fn send_own(
    inner: &Inner,
    conn: &Conn,
    open: &Open,
    sent: &mut u64,
    warned: &mut bool,
) -> bool {
    let records = match lock(&inner.store).since(inner.me, *sent) {
        Ok(records) => records,
        Err(error) => {
            if !*warned {
                tracing::warn!("sync: cannot read this machine's records: {error}");
                *warned = true;
            }
            Vec::new()
        }
    };
    if records.is_empty() {
        return send(conn, &records_message(open, Vec::new())).await;
    }
    for batch in records.chunks(BATCH) {
        let newest = batch.last().map(AnyRecord::version);
        if !send(conn, &records_message(open, batch.to_vec())).await {
            return false;
        }
        if let Some(version) = newest {
            *sent = version;
        }
    }
    true
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
/// is not the machine that was dialed.
async fn hello(inner: &Inner, conn: &mut Conn, dialed: Option<MachineId>) -> Option<Open> {
    if dialed.is_some() && !send(conn, &own_hello(inner, PROTOCOL)?).await {
        return None;
    }
    let text = timeout_at(Instant::now() + OFFLINE_AFTER, conn.recv.recv())
        .await
        .ok()??;
    let Ok(PeerMessage::Hello {
        protocol,
        machine,
        versions,
    }) = serde_json::from_str(&text)
    else {
        tracing::warn!("sync: a link opened without a hello");
        return None;
    };
    let allowed = match dialed {
        Some(expected) => machine == expected,
        None => machine != inner.me && listed(inner, machine),
    };
    if !allowed {
        tracing::warn!("sync: refused a link from {machine}: not a peer of this machine");
        return None;
    }
    let speak = protocol.min(PROTOCOL);
    if dialed.is_none() && !send(conn, &own_hello(inner, speak)?).await {
        return None;
    }
    inner.heard_from(machine);
    Some(Open {
        peer: machine,
        speak,
        sent: versions.get(&inner.me).copied().unwrap_or(0),
    })
}

fn own_hello(inner: &Inner, protocol: u16) -> Option<PeerMessage> {
    match lock(&inner.store).versions() {
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

fn listed(inner: &Inner, machine: MachineId) -> bool {
    match lock(&inner.store).settings() {
        Ok(settings) => settings.machines.iter().any(|m| m.id == machine),
        Err(error) => {
            tracing::warn!("sync: cannot read the machine list: {error}");
            false
        }
    }
}

/// Sends `message`, and says whether the link is still open. A message that cannot be
/// written as JSON is skipped. A peer that takes nothing for [`OFFLINE_AFTER`] ends the link.
async fn send(conn: &Conn, message: &PeerMessage) -> bool {
    let text = match serde_json::to_string(message) {
        Ok(text) => text,
        Err(error) => {
            tracing::warn!("sync: a message could not be written: {error}");
            return true;
        }
    };
    matches!(
        timeout(OFFLINE_AFTER, conn.send.send(text)).await,
        Ok(Ok(()))
    )
}

/// Takes the records in a message from `peer`, each only if `peer` owns it and it is newer,
/// and says whether to keep the link. A message that cannot be read, or a record that cannot
/// be stored, ends the link and nothing after it is taken: records arrive in version order, so
/// the store then holds a prefix, and the next hello asks for everything past it again.
fn receive(inner: &Inner, peer: MachineId, text: &str) -> bool {
    match serde_json::from_str::<PeerMessage>(text) {
        Ok(PeerMessage::Records { records, .. }) => {
            let mut store = lock(&inner.store);
            for record in &records {
                match store.apply(record, peer) {
                    Ok(Applied::NotOwner) => tracing::warn!(
                        "sync: {peer} sent a record owned by {}; refused",
                        record.owner()
                    ),
                    Ok(Applied::Stored | Applied::Stale) => {}
                    Err(error) => {
                        tracing::warn!("sync: a record from {peer} not stored: {error}");
                        return false;
                    }
                }
            }
            true
        }
        // Shared settings move only through the coordinator, which owns how they are sent.
        Ok(PeerMessage::Settings { .. }) | Ok(PeerMessage::Hello { .. }) => true,
        Err(error) => {
            tracing::warn!("sync: a message from {peer} could not be read: {error}");
            false
        }
    }
}
