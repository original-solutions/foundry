//! One link to a peer: hello, catch-up, then live sends and heartbeats until it ends.
//!
//! The dialing end says hello first. The accepting end waits for that hello and answers in
//! the lower of the two protocol numbers, and from then on each end sends in that protocol, so
//! a newer machine speaks the older form to an older peer. Protocol 1 has no shared settings
//! or edits on a link (see [`EDITS_SINCE`]), so a link in it sends neither; otherwise the
//! forms differ only in the number. A later protocol that changes a message's shape converts
//! it to the older shape where [`records_message`] builds it.
//!
//! Past the hellos, sending and reading run side by side, so a send that waits on a slow peer
//! never stops this end hearing it.
//!
//! Once a coordinator is set ([`super::Peers::coordinate`]), each end of a link that speaks
//! [`EDITS_SINCE`] or later sends the shared settings it holds first after the hellos, before
//! any record, and again whenever their `seq` changes; a machine is therefore never heard from
//! on such a link before its settings are taken. Settings and edits a link receives go to the
//! coordinator, which merges them or numbers the edit, and an answer to an edit this machine
//! sent goes to the coordinator that waits for it; with none set they are ignored. A link in
//! protocol 4 or later ([`foundry_proto::messages::ANSWERS_SINCE`]) carries an edit's id and its
//! answer; an older one carries neither.
//!
//! A link in protocol 5 or later ([`foundry_proto::messages::FACTORY_SINCE`]) also carries
//! factory records ([`super::factory`]): each hello names the factory versions it holds, and
//! each round sends this machine's new factory records after its P1 records, on a cursor of
//! their own. An older link carries none, and its P1 catch-up is as it was.

use foundry_proto::ids::MachineId;
use foundry_proto::messages::{EDITS_SINCE, PROTOCOL, PeerMessage};
use foundry_proto::record::AnyRecord;
use serde::Serialize;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::time::{Instant, timeout, timeout_at};

use std::time::Duration;

use super::factory::{self, Cursor, Sent};
use super::{BATCH, Conn, Inner, OFFLINE_AFTER, RETRY_REFUSED, TICK, WARN_EVERY, on_store};

/// Runs a link until it closes, the peer is silent for [`OFFLINE_AFTER`], the peer is not (or
/// is no longer) one this machine links with ([`links_with`]: it is listed, or this machine's
/// list holds no machine but itself), or the peer takes nothing for [`OFFLINE_AFTER`].
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
    let _opened = inner.opened(open.peer, open.speak, send.clone());
    tokio::select! {
        () = sending(inner, &send, &open) => {}
        () = reading(inner, &mut recv, open.peer, open.speak) => {}
    }
}

/// Sends this machine's new records every [`TICK`] until the peer is no longer one this machine
/// links with ([`links_with`]: it is listed, or this machine's list holds no machine but
/// itself) or stops taking them. Changed settings go first in each round, so a peer just removed
/// from the list is sent the copy that removed it before its link closes, and holds its own
/// removal. New factory records go last, on a link that carries them, one message a round and
/// only into an empty queue; while more of them wait, the next round starts as soon as the queue
/// is empty.
async fn sending(inner: &Inner, send: &Sender<String>, open: &Open) {
    let mut sent = open.sent;
    let mut factory_sent = Cursor::new(open.factory_sent);
    let mut settings_sent: Option<u64> = None;
    let mut warned: Option<Instant> = None;
    let mut every = tokio::time::interval(TICK);
    loop {
        every.tick().await;
        let room = send.capacity() == send.max_capacity();
        if !send_settings(inner, send, open, &mut settings_sent).await {
            return;
        }
        if !links_with(inner, open.peer).await {
            tracing::info!("sync: {} left the machine list; link closed", open.peer);
            return;
        }
        if !send_own(inner, send, open, &mut sent, &mut warned).await {
            return;
        }
        match factory::send_own(inner, send, open.peer, open.speak, &mut factory_sent, room).await {
            Sent::Closed => return,
            Sent::More if drained(send).await => every.reset_immediately(),
            Sent::More | Sent::Done => {}
        }
    }
}

/// Waits until the peer's end has taken everything queued on `send`, for at most a [`TICK`],
/// and says whether it did.
async fn drained(send: &Sender<String>) -> bool {
    let start = Instant::now();
    loop {
        if send.is_closed() {
            return false;
        }
        if send.capacity() == send.max_capacity() {
            return true;
        }
        if start.elapsed() >= TICK {
            return false;
        }
        tokio::time::sleep(DRAIN_POLL).await;
    }
}

/// How often [`drained`] looks at a link's queue.
const DRAIN_POLL: Duration = Duration::from_millis(10);

/// Sends the shared settings this machine holds when a coordinator is set, the link speaks
/// [`EDITS_SINCE`] or later, and their `seq` is not the one last sent on this link (`last`),
/// and says whether the link is still open.
async fn send_settings(
    inner: &Inner,
    send: &Sender<String>,
    open: &Open,
    last: &mut Option<u64>,
) -> bool {
    if inner.coordinator().is_none() || open.speak < EDITS_SINCE {
        return true;
    }
    let prev = *last;
    let settings = match on_store(&inner.store, move |store| store.settings_since(prev)).await {
        Ok(Some(settings)) => settings,
        Ok(None) => return true,
        Err(error) => {
            tracing::warn!("sync: cannot read the shared settings to send: {error}");
            return true;
        }
    };
    let seq = settings.seq;
    let message = PeerMessage::Settings {
        protocol: open.speak,
        settings,
    };
    if !send_message(send, &message).await {
        return false;
    }
    *last = Some(seq);
    true
}

/// Takes the peer's messages until the link closes or the peer is silent for
/// [`OFFLINE_AFTER`]. Each message taken counts as hearing from the peer.
///
/// After a message this machine cannot take (one it cannot read, or a record the store
/// refuses), the link takes no more records from the peer and no longer counts it as heard, so
/// the peer shows offline and its sessions unreachable within [`OFFLINE_AFTER`]. Records
/// arrive in version order, so the store keeps a prefix and loses nothing. The link stays open
/// for [`RETRY_REFUSED`], then ends, so the next link's hello asks again for everything past
/// that prefix: a passing store error clears then, and a record this build can never take is
/// resent once per link rather than once a second. Shared settings, edits and answers do not
/// depend on records, so a refused link still takes them: an edit sent on it is still numbered,
/// and the answer to one still arrives.
async fn reading(inner: &Inner, recv: &mut Receiver<String>, peer: MachineId, speak: u16) {
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
            // Taken before the link closes, so the message that closes it is not lost.
            if let Ok(
                message @ (PeerMessage::Settings { .. }
                | PeerMessage::Edit { .. }
                | PeerMessage::Answer { .. }),
            ) = serde_json::from_str::<PeerMessage>(&text)
                && let Err(error) = take(inner, peer, speak, message).await
            {
                tracing::warn!("sync: {error}");
            }
            if at.elapsed() >= RETRY_REFUSED {
                tracing::info!("sync: link to {peer} closed to ask again for what it refused");
                return;
            }
            continue;
        }
        match receive(inner, peer, speak, &text).await {
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
    /// The highest version of this machine's factory records the peer holds: none on a link
    /// that carries no factory record.
    factory_sent: u64,
}

/// Exchanges hellos, and refuses a peer that is not in the machine list ([`links_with`]), is
/// this machine, or is not the machine that was dialed. A hello does not count as hearing from
/// the peer: that waits for a message this machine takes.
///
/// A machine whose list holds no machine but itself takes a record link from any machine. A
/// link reaches `hello` only after `Keys::accept` has proved a key this machine holds, so
/// after `foundryd join` the only machine it lets in is the one that admitted it. The joined
/// machine then takes that machine's settings through the normal merge, and the current rule,
/// the machine list, applies from then on. A machine whose list is later cut down to itself
/// takes a record link again from any machine whose key it still holds.
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
        factory_versions,
    }) = first
    else {
        tracing::warn!("sync: a link opened without a hello");
        return None;
    };
    let allowed = match dialed {
        Some(expected) => machine == expected,
        None => machine != inner.me && links_with(inner, machine).await,
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
        factory_sent: if factory::carried(speak) {
            factory_versions.get(&inner.me).copied().unwrap_or(0)
        } else {
            0
        },
    })
}

/// This machine's hello in `protocol`: the P1 versions it holds, and the factory versions when
/// the protocol carries factory records. A dialing end says hello in this build's protocol
/// before it knows the peer's, so an older peer is sent factory versions it does not read.
async fn own_hello(inner: &Inner, protocol: u16) -> Option<PeerMessage> {
    match on_store(&inner.store, |store| store.versions()).await {
        Ok(versions) => Some(PeerMessage::Hello {
            protocol,
            machine: inner.me,
            versions,
            factory_versions: factory::versions(inner, protocol).await,
        }),
        Err(error) => {
            tracing::warn!("sync: cannot read the versions held: {error}");
            None
        }
    }
}

type ListedCache = (u64, std::collections::BTreeSet<MachineId>);

/// The machine ids in `store`'s shared settings, reusing `cached` while the settings are
/// unchanged, and the ids to cache when it read them again.
fn ids_in_store(
    store: &crate::store::Store,
    cached: Option<&ListedCache>,
) -> Result<(std::collections::BTreeSet<MachineId>, Option<ListedCache>), crate::store::StoreError>
{
    Ok(match store.settings_since(cached.map(|(seq, _)| *seq))? {
        None => (cached.map(|(_, ids)| ids.clone()).unwrap_or_default(), None),
        Some(settings) => {
            let ids: std::collections::BTreeSet<MachineId> =
                settings.machines.into_iter().map(|m| m.id).collect();
            (ids.clone(), Some((settings.seq, ids)))
        }
    })
}

/// The machine ids in the machine list, through the cache, or `None` when it cannot be read.
async fn listed_ids(inner: &Inner) -> Option<std::collections::BTreeSet<MachineId>> {
    let cached = super::lock(&inner.listed_cache).clone();
    let result = on_store(&inner.store, move |store| {
        ids_in_store(store, cached.as_ref())
    })
    .await;
    match result {
        Ok((ids, updated)) => {
            if let Some(cache) = updated {
                *super::lock(&inner.listed_cache) = Some(cache);
            }
            Some(ids)
        }
        Err(error) => {
            tracing::warn!("sync: cannot read the machine list: {error}");
            None
        }
    }
}

/// Whether `machine` is in the machine list: the rule for who is a peer for calls and relays,
/// and for record links once the list holds another machine ([`links_with`]).
pub(super) async fn listed(inner: &Inner, machine: MachineId) -> bool {
    listed_ids(inner)
        .await
        .is_some_and(|ids| ids.contains(&machine))
}

/// Whether a record link with `machine` is allowed: it is in the machine list, or the list
/// holds no machine but this one (or is empty), so a machine just joined takes a link from the
/// machine that admitted it before it has that machine's settings. That holds whenever the
/// list is cut down to this machine, for any machine whose key it holds. A list that cannot be
/// read allows nothing.
async fn links_with(inner: &Inner, machine: MachineId) -> bool {
    listed_ids(inner)
        .await
        .is_some_and(|ids| ids.contains(&machine) || ids.iter().all(|id| *id == inner.me))
}

/// Sends `message`, and says whether the link is still open. A message that cannot be
/// written as JSON is skipped. A peer that takes nothing for [`OFFLINE_AFTER`] ends the link.
pub(super) async fn send_message(send: &Sender<String>, message: &impl Serialize) -> bool {
    match serde_json::to_string(message) {
        Ok(text) => send_text(send, text).await,
        Err(error) => {
            tracing::warn!("sync: a message could not be written: {error}");
            true
        }
    }
}

/// Sends a message already written as JSON, and says whether the link is still open, as
/// [`send_message`] does.
pub(super) async fn send_text(send: &Sender<String>, text: String) -> bool {
    matches!(timeout(OFFLINE_AFTER, send.send(text)).await, Ok(Ok(())))
}

/// Takes the records in a message from `peer`, each only if `peer` owns it and it is newer.
/// A message that cannot be read, or a record that cannot be stored, is an error, and nothing
/// after it is taken: the store then holds a prefix of the peer's records.
///
/// Shared settings are merged by the coordinator, an edit is handed to it to number or pass
/// on, and an answer to the edit it waits on; with no coordinator set all three are ignored.
async fn receive(inner: &Inner, peer: MachineId, speak: u16, text: &str) -> Result<(), String> {
    match serde_json::from_str::<PeerMessage>(text) {
        Ok(message) => take(inner, peer, speak, message).await,
        Err(error) => Err(format!("a message from {peer} could not be read: {error}")),
    }
}

/// Takes one message from `peer`, which sent it on a link in protocol `speak`, as [`receive`]
/// says.
async fn take(
    inner: &Inner,
    peer: MachineId,
    speak: u16,
    message: PeerMessage,
) -> Result<(), String> {
    let records = match message {
        PeerMessage::Records { records, .. } => records,
        PeerMessage::Settings { settings, .. } => {
            if let Some(coordinator) = inner.coordinator() {
                coordinator
                    .take_settings(peer, speak, settings)
                    .await
                    .map_err(|error| format!("settings from {peer} not stored: {error}"))?;
            }
            return Ok(());
        }
        PeerMessage::Edit { id, edit, .. } => {
            if let Some(coordinator) = inner.coordinator() {
                coordinator.take_edit(peer, id, edit);
            }
            return Ok(());
        }
        PeerMessage::Answer { id, outcome, .. } => {
            if let Some(coordinator) = inner.coordinator() {
                coordinator.take_answer(id, outcome);
            }
            return Ok(());
        }
        // A relay or a call opens a link of its own and means nothing on a record link.
        PeerMessage::Hello { .. } | PeerMessage::Relay { .. } | PeerMessage::Call { .. } => {
            return Ok(());
        }
        PeerMessage::FactoryRecords { records, .. } => {
            return factory::take(inner, peer, speak, records).await;
        }
    };
    on_store(&inner.store, move |store| {
        store
            .apply_batch(&records, peer, |record| {
                tracing::warn!(
                    "sync: {peer} sent a record owned by {}; refused",
                    record.owner()
                );
            })
            .map_err(|error| format!("a record from {peer} not stored: {error}"))
    })
    .await
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use foundry_proto::settings::Machine;
    use futures::future::BoxFuture;

    use super::*;
    use crate::store::Store;
    use crate::sync::{Conn, Dialer, Peers};

    struct NoDial;

    impl Dialer for NoDial {
        fn dial(&self, _: &Machine) -> BoxFuture<'static, io::Result<Conn>> {
            Box::pin(async { Err(io::Error::other("no peers in these tests")) })
        }
    }

    const WAIT: Duration = Duration::from_secs(5);

    /// A machine whose list holds `listed`, a peer dialing it, and the peer's end of the link.
    struct Dialed {
        _data: tempfile::TempDir,
        theirs: Conn,
        accepting: tokio::task::JoinHandle<()>,
    }

    impl Dialed {
        /// This machine lists `listed` (by id) and accepts a link `peer` opens with a hello.
        async fn new(listed: impl FnOnce(MachineId) -> Vec<MachineId>, peer: MachineId) -> Self {
            Self::open(listed, peer, false).await
        }

        /// As [`Dialed::new`]; when `unreadable`, the stored settings are then damaged through
        /// a second connection to the database file, so the machine list cannot be read.
        async fn open(
            listed: impl FnOnce(MachineId) -> Vec<MachineId>,
            peer: MachineId,
            unreadable: bool,
        ) -> Self {
            let data = tempfile::tempdir().unwrap();
            let me = MachineId::random();
            let mut store = Store::open(data.path().join("foundry.db")).unwrap();
            let mut held = store.settings().unwrap();
            held.seq += 1;
            held.machines = listed(me)
                .into_iter()
                .enumerate()
                .map(|(i, id)| Machine {
                    id,
                    name: format!("machine-{i}"),
                    address: format!("100.64.0.{}", i + 1),
                    key: String::new(),
                })
                .collect();
            store.apply_settings(held).unwrap();
            if unreadable {
                rusqlite::Connection::open(data.path().join("foundry.db"))
                    .unwrap()
                    .execute("UPDATE settings SET settings = 'not settings'", [])
                    .unwrap();
            }
            let peers = Peers::new(me, Arc::new(Mutex::new(store)), Arc::new(NoDial));
            let (ours, theirs) = Conn::pair();
            let accepting = tokio::spawn(async move { peers.accept(ours).await });
            let hello = PeerMessage::Hello {
                protocol: PROTOCOL,
                machine: peer,
                versions: BTreeMap::new(),
                factory_versions: BTreeMap::new(),
            };
            theirs
                .send
                .send(serde_json::to_string(&hello).unwrap())
                .await
                .unwrap();
            Self {
                _data: data,
                theirs,
                accepting,
            }
        }

        /// The next message on the link, or `None` when it closed or sent nothing in time.
        async fn next(&mut self) -> Option<PeerMessage> {
            let text = timeout(WAIT, self.theirs.recv.recv()).await.ok()??;
            Some(serde_json::from_str(&text).unwrap())
        }

        /// Whether the link was accepted: a hello came back.
        async fn accepted(&mut self) -> bool {
            matches!(self.next().await, Some(PeerMessage::Hello { .. }))
        }
    }

    #[tokio::test]
    async fn a_machine_whose_list_holds_only_itself_opens_a_link_a_peer_dialed() {
        let mut link = Dialed::new(|me| vec![me], MachineId::random()).await;

        assert!(link.accepted().await, "the link was refused");
        // The link stays open past its first tick: a heartbeat arrives, not a close.
        assert!(
            matches!(link.next().await, Some(PeerMessage::Records { .. })),
            "the link closed instead of sending"
        );
        assert!(!link.accepting.is_finished());
    }

    #[tokio::test]
    async fn a_machine_whose_list_is_empty_opens_a_link_a_peer_dialed() {
        let mut link = Dialed::new(|_| Vec::new(), MachineId::random()).await;

        assert!(link.accepted().await, "the link was refused");
    }

    #[tokio::test]
    async fn a_machine_whose_list_holds_another_machine_refuses_a_link_from_one_not_in_it() {
        let mut link = Dialed::new(|me| vec![me, MachineId::random()], MachineId::random()).await;

        assert!(!link.accepted().await, "the link was accepted");
        link.accepting.await.unwrap();
    }

    #[tokio::test]
    async fn a_machine_whose_list_holds_one_other_machine_refuses_a_link_from_a_third() {
        let mut link = Dialed::new(|_| vec![MachineId::random()], MachineId::random()).await;

        assert!(!link.accepted().await, "the link was accepted");
        link.accepting.await.unwrap();
    }

    #[tokio::test]
    async fn a_machine_removed_from_the_list_is_sent_the_copy_that_removed_it_before_its_link_closes()
     {
        use foundry_proto::settings::Edit;

        use crate::coordinator::{Coordinator, numbered};

        let data = tempfile::tempdir().unwrap();
        let (me, peer, third) = (
            MachineId::random(),
            MachineId::random(),
            MachineId::random(),
        );
        let mut store = Store::open(data.path().join("foundry.db")).unwrap();
        let mut held = store.settings().unwrap();
        held.seq += 1;
        held.machines = [me, peer, third]
            .iter()
            .enumerate()
            .map(|(i, id)| Machine {
                id: *id,
                name: format!("machine-{i}"),
                address: format!("100.64.0.{}", i + 1),
                key: String::new(),
            })
            .collect();
        store.apply_settings(held).unwrap();
        let store = Arc::new(Mutex::new(store));
        let peers = Peers::new(me, store.clone(), Arc::new(NoDial));
        let coordinator = Arc::new(Coordinator::new(me, store.clone(), peers.clone()));
        peers.coordinate(coordinator.clone());
        let (ours, mut theirs) = Conn::pair();
        let accepting = tokio::spawn(async move { peers.accept(ours).await });
        let hello = PeerMessage::Hello {
            protocol: PROTOCOL,
            machine: peer,
            versions: BTreeMap::new(),
            factory_versions: BTreeMap::new(),
        };
        theirs
            .send
            .send(serde_json::to_string(&hello).unwrap())
            .await
            .unwrap();
        let next = |recv: &mut tokio::sync::mpsc::Receiver<String>| {
            let text = recv.try_recv().ok()?;
            Some(serde_json::from_str::<PeerMessage>(&text).unwrap())
        };
        // The link is up: the peer has been sent this machine's settings.
        let deadline = Instant::now() + WAIT;
        let mut settled = false;
        while !settled {
            assert!(Instant::now() < deadline, "no settings on the open link");
            while let Some(message) = next(&mut theirs.recv) {
                settled |= matches!(message, PeerMessage::Settings { .. });
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        {
            let mut store = store.lock().unwrap();
            let held = store.settings().unwrap();
            let removal = numbered(&held, &Edit::RemoveMachine { id: peer }, me);
            store.apply_settings(removal).unwrap();
        }

        // Read to the close: the copy that removed the peer comes first.
        let mut sent_removal = false;
        while let Ok(Some(text)) = timeout(WAIT, theirs.recv.recv()).await {
            if let PeerMessage::Settings { settings, .. } = serde_json::from_str(&text).unwrap() {
                sent_removal |= crate::join::is_removed(&settings, peer);
            }
        }
        assert!(
            timeout(WAIT, accepting).await.is_ok(),
            "the link did not close"
        );
        assert!(sent_removal, "the link closed without sending the removal");
    }

    #[tokio::test]
    async fn a_machine_whose_list_cannot_be_read_refuses_a_link() {
        let mut link = Dialed::open(|me| vec![me], MachineId::random(), true).await;

        assert!(!link.accepted().await, "the link was accepted");
        link.accepting.await.unwrap();
    }
}
