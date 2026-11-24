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
use std::time::Duration;

use foundry_proto::factory::FactoryRecord;
use foundry_proto::ids::MachineId;
use foundry_proto::messages::{FACTORY_SINCE, PeerMessage};
use tokio::sync::mpsc::Sender;
use tokio::time::Instant;

use super::link::send_message;
use super::{BATCH, Inner, TICK, on_store};
use crate::store::{Applied, StoreError};

/// The most bytes of records, as JSON, one factory message carries, past its first record. A
/// record larger than this goes in a message of its own. Kept well under the 16 MiB frame a
/// WebSocket end takes by default.
pub const FACTORY_BATCH_BYTES: usize = 4 << 20;

/// How often a link, or [`watch_own`], repeats the warning that this machine's factory records
/// cannot be read.
const WARN_EVERY: Duration = Duration::from_secs(60);

/// Whether a link in protocol `speak` carries factory records.
pub(super) fn carried(speak: u16) -> bool {
    speak >= FACTORY_SINCE
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
    /// The highest version of this machine's factory records the peer holds.
    sent: u64,
    /// When the warning that they cannot be read last went.
    warned: Option<Instant>,
}

impl Cursor {
    /// A cursor for a peer that holds this machine's factory records up to `held`.
    pub(super) fn new(held: u64) -> Self {
        Self {
            sent: held,
            warned: None,
        }
    }
}

/// Sends this machine's factory records above the cursor to `peer`, on a link in protocol
/// `speak`, reading one batch of at most [`BATCH`] at a time and sending it in messages of at
/// most [`FACTORY_BATCH_BYTES`], moving the cursor past each one sent, and says whether the
/// link is still open. Nothing goes on a link that carries no factory record, and nothing goes
/// when nothing is new: the P1 heartbeat keeps the link alive.
///
/// A factory record of this machine's own that cannot be read stops the send at the batch that
/// holds it, as it does for P1's records, with a warning once a minute.
pub(super) async fn send_own(
    inner: &Inner,
    send: &Sender<String>,
    peer: MachineId,
    speak: u16,
    cursor: &mut Cursor,
) -> bool {
    if !carried(speak) {
        return true;
    }
    loop {
        let (me, above) = (inner.me, cursor.sent);
        let batch = match on_store(&inner.store, move |store| {
            store.factory_since(me, above, BATCH)
        })
        .await
        {
            Ok(batch) => batch,
            Err(error) => {
                if cursor.warned.is_none_or(|at| at.elapsed() >= WARN_EVERY) {
                    tracing::warn!(
                        "sync: this machine's factory records above version {above} cannot be \
                         read, so {peer} is not sent them: {error}"
                    );
                    cursor.warned = Some(Instant::now());
                }
                return true;
            }
        };
        let full = batch.len() == BATCH;
        let mut rest = batch;
        while !rest.is_empty() {
            let tail = rest.split_off(fitting(&rest, FACTORY_BATCH_BYTES));
            let Some(newest) = rest.last().map(|record| record.version) else {
                break;
            };
            let message = PeerMessage::FactoryRecords {
                protocol: speak,
                records: rest,
            };
            if !send_message(send, &message).await {
                return false;
            }
            cursor.sent = newest;
            rest = tail;
        }
        if !full {
            return true;
        }
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
/// `peer` does not own is refused with a warning, as a P1 record is. A record that cannot be
/// stored is an error, and nothing after it is taken: the store then holds a prefix of the
/// peer's factory records, and the next link's hello asks again for the rest.
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
    if records.is_empty() {
        return Ok(());
    }
    let (stored, result) = on_store(&inner.store, move |store| {
        let mut stored = false;
        for record in &records {
            match store.apply_factory(record, peer) {
                Ok(Applied::Stored) => stored = true,
                Ok(Applied::Stale) => {}
                Ok(Applied::NotOwner) => tracing::warn!(
                    "sync: {peer} sent a {} factory record of {} that it does not own; refused",
                    record.body.kind(),
                    record.owner
                ),
                Err(error) => {
                    return (
                        stored,
                        Err(format!("a factory record from {peer} not stored: {error}")),
                    );
                }
            }
        }
        (stored, Ok(()))
    })
    .await;
    if stored {
        inner.factory_changed();
    }
    result
}

/// Tells [`Peers::factory_changes`](super::Peers::factory_changes) within a [`TICK`] each time
/// this machine has stored factory records of its own, until the returned future is dropped.
/// What is held when it starts is not told.
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
                if seen.is_some() && newest != seen {
                    inner.factory_changed();
                }
                seen = newest.or(seen);
            }
            Err(error) => {
                if warned.is_none_or(|at| at.elapsed() >= WARN_EVERY) {
                    tracing::warn!(
                        "sync: cannot read this machine's factory records to tell of changes: \
                         {error}"
                    );
                    warned = Some(Instant::now());
                }
            }
        }
    }
}

/// The highest version of `me`'s own factory records: read from every owner's versions the
/// first time (`above` is `None`), so nothing held is parsed, and after that from one batch of
/// at most [`BATCH`] above `above`, so a backlog is read a batch a tick. `None` the first time
/// means none is held; after that, `above` again when nothing is new.
fn newest_own(
    store: &mut crate::store::Store,
    me: MachineId,
    above: Option<u64>,
) -> Result<Option<u64>, StoreError> {
    let Some(above) = above else {
        return Ok(Some(
            store.factory_versions()?.get(&me).copied().unwrap_or(0),
        ));
    };
    let batch = store.factory_since(me, above, BATCH)?;
    Ok(Some(batch.last().map_or(above, |record| record.version)))
}

#[cfg(test)]
mod tests {
    use foundry_proto::factory::{FactoryBody, RunEvent, RunId};

    use super::*;

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
        assert!(size <= FACTORY_BATCH_BYTES + 1024, "a message of {size} bytes");
    }
}
