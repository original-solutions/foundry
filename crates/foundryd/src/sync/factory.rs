//! Factory records on protocol 5 links.
//!
//! A link whose two ends speak [`FACTORY_SINCE`] or later carries factory records beside P1's,
//! under the same rules and on a cursor of their own. Each hello names the highest factory
//! version it holds per owner (`factory_versions`), apart from the P1 versions. Past the hellos
//! each end sends the factory records it owns above the version the other holds, then keeps
//! sending its own as they are written. A machine sends only factory records it owns, and takes
//! one only from its owner and only when newer ([`Store::apply_factory`]), so one relayed by
//! anyone else is refused. A link in an older protocol carries no factory record either way:
//! nothing is sent on it and anything that arrives on it is dropped. A machine that updates
//! later says hello on 5 holding none, and is sent every one.
//!
//! Sending stops at the highest factory version sent, so a machine's own factory records must
//! be stored in the order of their versions: a writer takes
//! [`Store::own_next_factory_version`] and applies the record while holding the store's lock
//! once.
//!
//! A run event may carry 64 KB of output, so a factory message is cut by size as well as by
//! count ([`FACTORY_BATCH_BYTES`]): a link refuses a WebSocket message far smaller than
//! [`BATCH`] such events.
//!
//! [`Peers::factory_changes`](super::Peers::factory_changes) tells readers that a factory
//! record was stored: at once for one taken from a peer, and within a [`TICK`] for one this
//! machine wrote, which [`watch_own`] looks for while [`Peers::run`](super::Peers::run) runs.
//!
//! [`Store::apply_factory`]: crate::store::Store::apply_factory
//! [`Store::own_next_factory_version`]: crate::store::Store::own_next_factory_version

use std::collections::BTreeMap;

use foundry_proto::factory::FactoryRecord;
use foundry_proto::ids::MachineId;
use foundry_proto::messages::{FACTORY_SINCE, PeerMessage};
use tokio::sync::mpsc::Sender;
use tokio::time::Instant;

use super::link::send_message;
use super::{BATCH, Inner, TICK, WARN_EVERY, on_store};
use crate::store::{Applied, Store, StoreError};

/// The most bytes of records, as JSON, one factory message carries, past its first record. A
/// record larger than this goes in a message of its own.
///
/// A link queues a factory message only when its queue is empty ([`send_own`]), and the link's
/// writer takes a message off the queue before it writes it, so anything sent after a factory
/// message waits behind at most two: one being written and one queued. The other end ends a
/// link it hears nothing on for [`OFFLINE_AFTER`](super::OFFLINE_AFTER), and a message is heard
/// only once it has all arrived, so those two cross a path of 70 KB a second in under 8 s:
/// within that limit even when the two links of a pair share the path. A socket's own send
/// buffer comes on top of that.
pub const FACTORY_BATCH_BYTES: usize = 256 << 10;

/// Whether a link in protocol `speak` carries factory records.
pub(super) fn carried(speak: u16) -> bool {
    speak >= FACTORY_SINCE
}

/// Whether `text`, a message from a peer that could not be read, is a factory message: only
/// its `type` is read.
pub(super) fn is_message(text: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(text)
        .is_ok_and(|message| message["type"] == "factory_records")
}

/// The highest factory version this machine holds per owner, for its hello in protocol
/// `speak`. None for a protocol that carries no factory record. A store that cannot be read
/// gives none, with a warning: the peer then sends every factory record it owns again, and
/// each copy already held changes nothing, but P1's records still sync.
pub(super) async fn versions(inner: &Inner, speak: u16) -> BTreeMap<MachineId, u64> {
    if !carried(speak) {
        return BTreeMap::new();
    }
    match on_store(&inner.store, |store| store.factory_versions()).await {
        Ok(versions) => versions,
        Err(error) => {
            tracing::warn!("sync: cannot read the factory versions held: {error}");
            BTreeMap::new()
        }
    }
}

/// Where a link is in sending this machine's factory records.
pub(super) struct Cursor {
    /// The highest version of this machine's factory records the peer has been sent.
    sent: u64,
    /// Records read above `sent`, in version order, that did not fit in the last message.
    pending: Vec<FactoryRecord>,
    /// Whether the batch `pending` came from was full, so more may wait above it.
    more: bool,
    /// When the warning that they cannot be read last went.
    warned: Option<Instant>,
}

impl Cursor {
    /// A cursor for a peer that holds this machine's factory records up to `held`.
    pub(super) fn new(held: u64) -> Self {
        Self {
            sent: held,
            pending: Vec::new(),
            more: false,
            warned: None,
        }
    }
}

/// What one round of [`send_own`] did.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Sent {
    /// The link closed.
    Closed,
    /// Every factory record of this machine's that could be read has been sent.
    Done,
    /// More wait: the next round should come as soon as the link's queue is empty, not a
    /// [`TICK`] later.
    More,
}

/// Sends one message of this machine's factory records above the cursor to `peer`, on a link
/// in protocol `speak`: as many as fit in [`FACTORY_BATCH_BYTES`], from a batch of at most
/// [`BATCH`] read and kept on the cursor until all of it is sent. One message a round, and only
/// when the link's queue was empty as the round began (`room`), so a long catch-up leaves room
/// for the link's settings, its check of the machine list and P1's records, and none of them
/// waits behind more than one factory message. It says [`Sent::More`] while more wait, so the
/// round comes again once the queue is empty. Nothing goes on a link that carries no factory
/// record, and nothing goes when nothing is new: the P1 heartbeat keeps the link alive.
///
/// A factory record of this machine's own that cannot be read stops the send at the batch that
/// holds it, as it does for P1's records, with a warning once a minute.
pub(super) async fn send_own(
    inner: &Inner,
    send: &Sender<String>,
    peer: MachineId,
    speak: u16,
    cursor: &mut Cursor,
    room: bool,
) -> Sent {
    if !carried(speak) {
        return Sent::Done;
    }
    if !room {
        return if cursor.more || !cursor.pending.is_empty() {
            Sent::More
        } else {
            Sent::Done
        };
    }
    if cursor.pending.is_empty() {
        let (me, above) = (inner.me, cursor.sent);
        match on_store(&inner.store, move |store| {
            store.factory_since(me, above, BATCH)
        })
        .await
        {
            Ok(batch) => {
                cursor.more = batch.len() == BATCH;
                cursor.pending = batch;
            }
            Err(error) => {
                if cursor.warned.is_none_or(|at| at.elapsed() >= WARN_EVERY) {
                    tracing::warn!(
                        "sync: this machine's factory records above version {above} cannot be \
                         read, so {peer} is not sent them: {error}"
                    );
                    cursor.warned = Some(Instant::now());
                }
                return Sent::Done;
            }
        }
    }
    let rest = cursor
        .pending
        .split_off(fitting(&cursor.pending, FACTORY_BATCH_BYTES));
    let records = std::mem::replace(&mut cursor.pending, rest);
    let Some(newest) = records.last().map(|record| record.version) else {
        return Sent::Done;
    };
    let message = PeerMessage::FactoryRecords {
        protocol: speak,
        records,
    };
    if !send_message(send, &message).await {
        return Sent::Closed;
    }
    cursor.sent = newest;
    if cursor.more || !cursor.pending.is_empty() {
        Sent::More
    } else {
        Sent::Done
    }
}

/// How many of `records`, from the first, go in one message: as many as fit in `budget` bytes
/// of JSON, and never fewer than one while any are left.
fn fitting(records: &[FactoryRecord], budget: usize) -> usize {
    let mut used = 0;
    for (i, record) in records.iter().enumerate() {
        used += serde_json::to_vec(record).map_or(0, |json| json.len());
        if used > budget {
            return i.max(1);
        }
    }
    records.len()
}

/// Takes the factory records in a message from `peer`, which sent it on a link in protocol
/// `speak`, each only if `peer` owns it and it is newer, and tells
/// [`Peers::factory_changes`](super::Peers::factory_changes) when any was stored. A record
/// `peer` does not own, or whose key another machine holds, is refused with a warning, as a P1
/// record is. A record that cannot be stored is an error, and nothing after it is taken: the
/// store then holds a prefix of the peer's factory records, and the next link's hello asks
/// again for the rest.
///
/// Each record is stored under the store's lock taken for it alone, since each commits on its
/// own, so a catch-up never holds the store from its other users for a whole message. Each is
/// told where it is stored, so a take that a closing link drops part way still tells of every
/// record it stored.
///
/// A link in a protocol before [`FACTORY_SINCE`] carries no factory record, so one that
/// arrives on it is dropped.
pub(super) async fn take(
    inner: &Inner,
    peer: MachineId,
    speak: u16,
    records: Vec<FactoryRecord>,
) -> Result<(), String> {
    if !carried(speak) {
        tracing::debug!("sync: {peer} sent factory records on a protocol {speak} link; dropped");
        return Ok(());
    }
    for record in records {
        let changed = inner.factory_changed.clone();
        let (applied, record) = on_store(&inner.store, move |store| {
            let applied = store.apply_factory(&record, peer);
            if matches!(applied, Ok(Applied::Stored)) {
                super::tell(&changed);
            }
            (applied, record)
        })
        .await;
        match applied {
            Ok(Applied::Stored | Applied::Stale) => {}
            Ok(Applied::NotOwner) if record.owner == peer => tracing::warn!(
                "sync: {peer} sent a {} factory record whose key another machine holds; refused",
                record.body.kind()
            ),
            Ok(Applied::NotOwner) => tracing::warn!(
                "sync: {peer} sent a {} factory record owned by {}; refused",
                record.body.kind(),
                record.owner
            ),
            Err(error) => return Err(format!("a factory record from {peer} not stored: {error}")),
        }
    }
    Ok(())
}

/// Tells [`Peers::factory_changes`](super::Peers::factory_changes) within a [`TICK`] each time
/// this machine has stored factory records of its own, until the returned future is dropped.
/// What is held when it starts is not told. It reads the records above the newest it has seen,
/// through the owner's index, and reads the highest version held per owner, which parses no
/// record, only at the start and when that read fails: a factory record of its own that this
/// build cannot read does not stop it.
pub(super) async fn watch_own(inner: &Inner) {
    let me = inner.me;
    let mut seen: Option<u64> = None;
    let mut warned: Option<Instant> = None;
    let mut every = tokio::time::interval(TICK);
    loop {
        every.tick().await;
        let above = seen;
        let newest = on_store(&inner.store, move |store| newest_own(store, me, above)).await;
        match newest {
            Ok(newest) => {
                if seen.is_some_and(|seen| seen != newest) {
                    super::tell(&inner.factory_changed);
                }
                seen = Some(newest);
            }
            Err(error) => {
                if warned.is_none_or(|at| at.elapsed() >= WARN_EVERY) {
                    tracing::warn!(
                        "sync: cannot read this machine's factory versions to tell of changes: \
                         {error}"
                    );
                    warned = Some(Instant::now());
                }
            }
        }
    }
}

/// The highest version of `me`'s own factory records: the newest of at most [`BATCH`] above
/// `seen`, so a burst is read a batch a tick, or, with nothing seen yet or a record above `seen`
/// that cannot be read, the highest version held, which parses no record. When that cannot be
/// read either, which a row of another owner's can cause, `me`'s own records above `seen` are
/// read to the last.
fn newest_own(store: &mut Store, me: MachineId, seen: Option<u64>) -> Result<u64, StoreError> {
    if let Some(seen) = seen
        && let Ok(batch) = store.factory_since(me, seen, BATCH)
    {
        return Ok(batch.last().map_or(seen, |record| record.version));
    }
    if let Ok(versions) = store.factory_versions() {
        return Ok(versions.get(&me).copied().unwrap_or(0));
    }
    let mut newest = seen.unwrap_or(0);
    loop {
        let batch = store.factory_since(me, newest, BATCH)?;
        let Some(last) = batch.last() else {
            return Ok(newest);
        };
        newest = last.version;
        if batch.len() < BATCH {
            return Ok(newest);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use foundry_proto::factory::{FactoryBody, RunEvent, RunId};
    use foundry_proto::settings::Machine;
    use futures::future::BoxFuture;

    use super::*;
    use crate::sync::{Conn, Dialer, Peers, lock};

    struct NoDial;

    impl Dialer for NoDial {
        fn dial(&self, _: &Machine) -> BoxFuture<'static, io::Result<Conn>> {
            Box::pin(async { Err(io::Error::other("no peers in these tests")) })
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn records_a_take_stored_before_it_was_cancelled_still_tell_factory_changes() {
        let data = tempfile::tempdir().unwrap();
        let store = Store::open(data.path().join("foundry.db")).unwrap();
        let peers = Peers::new(
            MachineId::random(),
            Arc::new(Mutex::new(store)),
            Arc::new(NoDial),
        );
        let peer = MachineId::random();
        let records: Vec<FactoryRecord> = (1..=2000)
            .map(|version| FactoryRecord {
                owner: peer,
                ..event(version, 10)
            })
            .collect();
        let mut changes = peers.factory_changes();
        changes.borrow_and_update();

        let taking = {
            let peers = peers.clone();
            tokio::spawn(async move { take(&peers.inner, peer, FACTORY_SINCE, records).await })
        };
        // Cancel the take, as a link that ends drops it, once it has stored a record and while
        // it waits on the store.
        loop {
            let aborted = {
                let store = lock(&peers.inner.store);
                let stored = store.factory_versions().unwrap().contains_key(&peer);
                if stored {
                    taking.abort();
                }
                stored
            };
            if aborted {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert!(
            taking.await.is_err_and(|e| e.is_cancelled()),
            "the take ended first"
        );
        assert!(
            changes.has_changed().unwrap(),
            "records stored before the cancel were never told"
        );
    }

    fn event(version: u64, output: usize) -> FactoryRecord {
        FactoryRecord {
            owner: MachineId::random(),
            version,
            body: FactoryBody::RunEvent(RunEvent {
                run: RunId::new("original-solutions/foundry", 132, 1).unwrap(),
                generation: 1,
                ts: "2026-11-24T05:16:30.000Z".into(),
                ms: 1_795_497_390_000,
                event: "gate_failed".into(),
                data: serde_json::json!({ "output": "x".repeat(output) }),
                cause: None,
            }),
        }
    }

    #[test]
    fn records_that_fit_the_budget_go_in_one_message() {
        let records: Vec<FactoryRecord> = (1..=3).map(|v| event(v, 10)).collect();
        assert_eq!(fitting(&records, FACTORY_BATCH_BYTES), 3);
        assert_eq!(fitting(&[], FACTORY_BATCH_BYTES), 0);
    }

    #[test]
    fn a_message_stops_before_the_record_that_passes_the_budget() {
        let records: Vec<FactoryRecord> = (1..=4).map(|v| event(v, 1000)).collect();
        let one = serde_json::to_vec(&records[0]).unwrap().len();
        assert_eq!(fitting(&records, one * 2 + 1), 2);
        assert_eq!(fitting(&records, one * 2), 2);
    }

    #[test]
    fn a_record_larger_than_the_budget_goes_alone() {
        let records = vec![event(1, 5000), event(2, 10)];
        assert_eq!(fitting(&records, 100), 1);
    }

    #[test]
    fn a_batch_of_run_events_with_64_kb_of_output_each_is_cut_under_the_budget() {
        let records: Vec<FactoryRecord> = (1..=200).map(|v| event(v, 64 * 1024)).collect();
        let n = fitting(&records, FACTORY_BATCH_BYTES);
        assert!(n > 1 && n < records.len(), "{n} of them in one message");
        let message = PeerMessage::FactoryRecords {
            protocol: FACTORY_SINCE,
            records: records[..n].to_vec(),
        };
        let size = serde_json::to_vec(&message).unwrap().len();
        assert!(
            size <= FACTORY_BATCH_BYTES + 1024,
            "a message of {size} bytes"
        );
    }
}
