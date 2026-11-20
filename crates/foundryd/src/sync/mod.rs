//! Peer links, record exchange and catch-up.
//!
//! [`Peers::run`] keeps a link open to every other machine in the shared settings' machine
//! list, and [`Peers::accept`] serves a link a peer opened. Both ends of a link say hello with
//! the highest version they hold per owner, then each sends the records it owns that the
//! other does not have, then keeps sending its own records as they change. A machine sends
//! only records it owns, and takes a record only from its owner and only when newer (the
//! store's rule), so a record relayed by anyone else is refused.
//!
//! A machine not heard from for [`OFFLINE_AFTER`] is offline, and [`Peers::state`] shows its
//! sessions and terminals as unreachable. A machine is heard from when this machine takes a
//! message from it, so one whose records this machine refuses also shows offline.
//!
//! Every store call runs on the blocking pool ([`on_store`]), so a slow disk or a busy store
//! never stalls an async worker.
//!
//! Each machine dials every peer and also serves the link each peer dials, so a pair runs two
//! links and each carries records both ways. That is deliberate: a pair stays in sync when
//! only one of them can reach the other. The second copy of a record is stale and changes
//! nothing.
//!
//! A link trusts the machine id its peer names in its first message, a hello, a relay or a call,
//! checked only against the machine list. Proving that id with a machine key belongs to
//! joining (task 13): the daemon serves every peer link through `join::Keys::accept`.
//!
//! A link that opens with a [`PeerMessage::Relay`] in place of a hello carries one of this
//! machine's terminals to the peer, not records ([`relay`]). That is shell access on this
//! machine, so a link that has not passed `join::Keys::accept` must never reach it. A link that
//! opens with a [`PeerMessage::Call`] carries one call to this machine's API and its answer
//! ([`call`]), and is kept behind the same keys.
//!
//! Sending stops at the highest version sent, so a machine's own records must be stored in
//! the order of their versions: a writer takes [`Store::own_next_version`] and applies the
//! record while holding the store's lock once.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use foundry_proto::ids::MachineId;
use foundry_proto::messages::{ANSWERS_SINCE, EDITS_SINCE, PeerMessage};
use foundry_proto::record::AnyRecord;
use foundry_proto::settings::{Edit, EditId, Machine, Outcome};
use foundry_proto::state::{SessionState, TerminalState};
use futures::future::BoxFuture;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::coordinator::Coordinator;
use crate::store::{Store, StoreError};
use crate::terminals::Terminals;

pub mod call;
mod link;
pub mod relay;
pub mod ws;

/// A machine not heard from for this long is offline.
pub const OFFLINE_AFTER: Duration = Duration::from_secs(15);

/// How often a link sends this machine's new records. An empty send is the heartbeat that
/// keeps the machine online on the other end.
const TICK: Duration = Duration::from_secs(1);

/// How long a link that refused a peer's message stays open before it ends, so the next link's
/// hello asks again for everything past what is stored. A record that can never be stored
/// then costs one resend per link each time, not one a second.
pub const RETRY_REFUSED: Duration = Duration::from_secs(60);

/// How long to wait before dialing a peer again after a link ends or a dial fails, and how
/// often the machine list is read for peers added or removed.
const REDIAL: Duration = Duration::from_secs(1);

/// The most records one message carries. A larger catch-up goes out as several messages.
pub const BATCH: usize = 500;

/// How long a dial may take before it is given up and tried again.
const DIAL_TIMEOUT: Duration = Duration::from_secs(5);

/// How many messages a link buffers each way.
const BUFFER: usize = 64;

/// One open link: JSON text messages each way. A closed channel is a closed link.
pub struct Conn {
    pub send: mpsc::Sender<String>,
    pub recv: mpsc::Receiver<String>,
}

impl Conn {
    /// Two ends of a link, each sending to the other.
    pub fn pair() -> (Self, Self) {
        let (a_send, b_recv) = mpsc::channel(BUFFER);
        let (b_send, a_recv) = mpsc::channel(BUFFER);
        (
            Self {
                send: a_send,
                recv: a_recv,
            },
            Self {
                send: b_send,
                recv: b_recv,
            },
        )
    }
}

/// Opens a link to a peer: a WebSocket in the daemon ([`ws::WsDialer`]), an in-memory
/// channel in the fleet tests.
pub trait Dialer: Send + Sync + 'static {
    fn dial(&self, peer: &Machine) -> BoxFuture<'static, io::Result<Conn>>;
}

/// Why [`Peers::send_edit`] sent nothing.
#[derive(Debug)]
pub enum Unsent {
    /// No link to the machine is open.
    NotLinked,
    /// The open links to the machine speak this protocol, older than [`ANSWERS_SINCE`].
    Older(u16),
    /// The edit cannot be written as JSON.
    Unwritable(serde_json::Error),
}

/// An open record link: the protocol both ends speak, and the sender that writes to it.
#[derive(Clone)]
struct Link {
    speak: u16,
    send: mpsc::Sender<String>,
}

/// This machine's links to its peers, and when each peer was last heard from.
#[derive(Clone)]
pub struct Peers {
    inner: Arc<Inner>,
}

struct Inner {
    me: MachineId,
    store: Arc<Mutex<Store>>,
    dialer: Arc<dyn Dialer>,
    heard: Mutex<BTreeMap<MachineId, Instant>>,
    /// The terminals a relay serves to peers; none until [`Peers::serve_terminals`].
    terminals: Mutex<Option<Arc<Terminals>>>,
    /// The loopback address of the API a carried call is made to; none until
    /// [`Peers::serve_calls`].
    calls: Mutex<Option<SocketAddr>>,
    /// The coordinator a record link hands settings and edits to; none until
    /// [`Peers::coordinate`]. Held weakly, as the coordinator holds these peers.
    coordinator: Mutex<Option<Weak<Coordinator>>>,
    /// Each open record link past its hellos, by peer and link number.
    links: Mutex<BTreeMap<MachineId, BTreeMap<u64, Link>>>,
    next_link: AtomicU64,
}

impl Peers {
    pub fn new(me: MachineId, store: Arc<Mutex<Store>>, dialer: Arc<dyn Dialer>) -> Self {
        Self {
            inner: Arc::new(Inner {
                me,
                store,
                dialer,
                heard: Mutex::new(BTreeMap::new()),
                terminals: Mutex::new(None),
                calls: Mutex::new(None),
                coordinator: Mutex::new(None),
                links: Mutex::new(BTreeMap::new()),
                next_link: AtomicU64::new(0),
            }),
        }
    }

    /// Keeps a link open to every other machine in the machine list, redialing a link that
    /// ends, until the returned future is dropped. A machine added to the list is dialed
    /// within [`REDIAL`]. A machine removed from it is no longer dialed, and every link to it,
    /// dialed or served, closes within a second.
    pub async fn run(&self) {
        let mut links: BTreeMap<MachineId, (Machine, Task)> = BTreeMap::new();
        let mut every = tokio::time::interval(REDIAL);
        loop {
            every.tick().await;
            let machines = match on_store(&self.inner.store, |store| store.settings()).await {
                Ok(settings) => settings.machines,
                Err(error) => {
                    tracing::warn!("sync: cannot read the machine list: {error}");
                    continue;
                }
            };
            let wanted: BTreeMap<MachineId, Machine> = machines
                .into_iter()
                .filter(|m| m.id != self.inner.me)
                .map(|m| (m.id, m))
                .collect();
            links.retain(|id, (machine, _)| wanted.get(id) == Some(machine));
            for (id, machine) in wanted {
                links.entry(id).or_insert_with(|| {
                    let task = Task(tokio::spawn(self.clone().dial_forever(machine.clone())));
                    (machine, task)
                });
            }
        }
    }

    /// Serves a link a peer opened, until it ends: reads and parses its first message, then
    /// serves it as [`Peers::accept_opened`] does.
    pub async fn accept(&self, mut conn: Conn) {
        let Ok(Some(text)) = tokio::time::timeout(OFFLINE_AFTER, conn.recv.recv()).await else {
            return;
        };
        let Ok(first) = serde_json::from_str(&text) else {
            tracing::warn!("sync: a link opened without a hello");
            return;
        };
        self.accept_opened(conn, first).await;
    }

    /// Serves a link a peer opened whose first message, `first`, has already been read from
    /// `conn`, until it ends. A link that opens with a [`PeerMessage::Relay`] goes to the
    /// relay, and one that opens with a [`PeerMessage::Call`] to [`call`]; every other link
    /// carries records, and takes `first` as its hello.
    pub(crate) async fn accept_opened(&self, conn: Conn, first: PeerMessage) {
        match first {
            PeerMessage::Relay {
                machine, terminal, ..
            } => relay::serve(&self.inner, machine, terminal, conn).await,
            PeerMessage::Call { machine, call, .. } => {
                call::serve(&self.inner, machine, call, conn).await;
            }
            hello => link::run(&self.inner, conn, None, Some(hello)).await,
        }
    }

    /// Serves `terminals` to peers that open a relay to one of them. Until this is called, a
    /// relay is closed with the reason.
    pub fn serve_terminals(&self, terminals: Arc<Terminals>) {
        *lock(&self.inner.terminals) = Some(terminals);
    }

    /// Hands the shared settings and edits record links receive to `coordinator`, and has each
    /// record link send this machine's settings after the hellos and whenever their `seq`
    /// changes. Until this is called, links neither send settings nor take them.
    pub fn coordinate(&self, coordinator: Arc<Coordinator>) {
        *lock(&self.inner.coordinator) = Some(Arc::downgrade(&coordinator));
    }

    /// Sends `edit` to `machine` under `id` on an open record link that speaks
    /// [`ANSWERS_SINCE`] or later, in that link's protocol, and returns the number of the link
    /// it went on. It says why when no link took it.
    pub async fn send_edit(
        &self,
        machine: MachineId,
        id: EditId,
        edit: &Edit,
    ) -> Result<u64, Unsent> {
        let links: Vec<(u64, Link)> = lock(&self.inner.links)
            .get(&machine)
            .map(|open| open.iter().map(|(n, link)| (*n, link.clone())).collect())
            .unwrap_or_default();
        let mut unsent = Unsent::NotLinked;
        for (number, Link { speak, send }) in links {
            if speak < ANSWERS_SINCE {
                unsent = Unsent::Older(speak);
                continue;
            }
            let message = PeerMessage::Edit {
                protocol: speak,
                id: Some(id),
                edit: edit.clone(),
            };
            let text = serde_json::to_string(&message).map_err(Unsent::Unwritable)?;
            if link::send_text(&send, text).await {
                return Ok(number);
            }
        }
        Err(unsent)
    }

    /// Whether record link `link` to `machine`, as [`Peers::send_edit`] numbered it, is still
    /// open.
    pub fn is_open(&self, machine: MachineId, link: u64) -> bool {
        lock(&self.inner.links)
            .get(&machine)
            .is_some_and(|open| open.contains_key(&link))
    }

    /// Answers the edit `machine` sent under `id` on the first open record link to it that
    /// speaks [`ANSWERS_SINCE`] or later. False when no link took it.
    pub(crate) async fn send_answer(
        &self,
        machine: MachineId,
        id: EditId,
        outcome: Outcome,
    ) -> bool {
        let links: Vec<Link> = lock(&self.inner.links)
            .get(&machine)
            .map(|open| open.values().cloned().collect())
            .unwrap_or_default();
        for Link { speak, send } in links {
            if speak < ANSWERS_SINCE {
                continue;
            }
            let message = PeerMessage::Answer {
                protocol: speak,
                id,
                outcome: outcome.clone(),
            };
            if link::send_message(&send, &message).await {
                return true;
            }
        }
        false
    }

    /// The machines with an open record link whose every open link speaks a protocol older than
    /// [`EDITS_SINCE`]: they send no shared settings and take no edits.
    pub fn older(&self) -> BTreeSet<MachineId> {
        lock(&self.inner.links)
            .iter()
            .filter(|(_, open)| open.values().all(|link| link.speak < EDITS_SINCE))
            .map(|(id, _)| *id)
            .collect()
    }

    /// The machines heard from within [`OFFLINE_AFTER`], and this machine.
    pub fn online(&self) -> BTreeSet<MachineId> {
        let now = Instant::now();
        let heard = lock(&self.inner.heard);
        heard
            .iter()
            .filter(|(_, at)| now.duration_since(**at) < OFFLINE_AFTER)
            .map(|(id, _)| *id)
            .chain([self.inner.me])
            .collect()
    }

    /// Every record held, from every owner, with the sessions and terminals of an offline
    /// owner shown as unreachable. An owner with a record this build cannot read, one a newer
    /// build wrote before a rollback, is left out with a warning so every other owner still
    /// shows.
    pub async fn state(&self) -> Result<Vec<AnyRecord>, StoreError> {
        let online = self.online();
        let records = on_store(&self.inner.store, |store| {
            let mut records = Vec::new();
            for owner in store.versions()?.into_keys() {
                match store.since(owner, 0) {
                    Ok(held) => records.extend(held),
                    Err(error) => tracing::warn!("sync: records of {owner} not shown: {error}"),
                }
            }
            Ok::<_, StoreError>(records)
        })
        .await?;
        Ok(records
            .into_iter()
            .map(|record| as_seen(record, &online))
            .collect())
    }

    async fn dial_forever(self, peer: Machine) {
        loop {
            match tokio::time::timeout(DIAL_TIMEOUT, self.inner.dialer.dial(&peer)).await {
                Ok(Ok(conn)) => link::run(&self.inner, conn, Some(peer.id), None).await,
                Ok(Err(error)) => tracing::debug!("sync: cannot reach {}: {error}", peer.name),
                Err(_) => tracing::debug!("sync: dialing {} timed out", peer.name),
            }
            tokio::time::sleep(REDIAL).await;
        }
    }
}

/// Why a link of its own, a relay's or a call's, did not open.
enum Unopened {
    /// The peer refused this machine's key; the reason says so.
    Refused(String),
    /// The peer could not be reached, or the link closed before its first message went.
    Down,
}

impl Inner {
    /// Dials `peer` with the dialer the record links use, so the link proves both machines'
    /// keys, and opens it with `first` in place of a hello: how a relay and a call each open a
    /// link of their own.
    async fn open_link(&self, peer: &Machine, first: &PeerMessage) -> Result<Conn, Unopened> {
        let conn = match tokio::time::timeout(DIAL_TIMEOUT, self.dialer.dial(peer)).await {
            Ok(Ok(conn)) => conn,
            Ok(Err(e)) if e.kind() == io::ErrorKind::PermissionDenied => {
                return Err(Unopened::Refused(e.to_string()));
            }
            Ok(Err(_)) | Err(_) => return Err(Unopened::Down),
        };
        if !link::send_message(&conn.send, first).await {
            return Err(Unopened::Down);
        }
        Ok(conn)
    }

    fn heard_from(&self, peer: MachineId) {
        lock(&self.heard).insert(peer, Instant::now());
    }

    fn coordinator(&self) -> Option<Arc<Coordinator>> {
        lock(&self.coordinator).as_ref().and_then(Weak::upgrade)
    }

    /// Records an open link to `peer` that speaks `speak` and sends on `send`, until the
    /// returned guard drops.
    fn opened(&self, peer: MachineId, speak: u16, send: mpsc::Sender<String>) -> Opened<'_> {
        let number = self.next_link.fetch_add(1, Ordering::Relaxed);
        lock(&self.links)
            .entry(peer)
            .or_default()
            .insert(number, Link { speak, send });
        Opened {
            inner: self,
            peer,
            number,
        }
    }
}

/// An open link in [`Inner::links`], removed when this drops.
struct Opened<'a> {
    inner: &'a Inner,
    peer: MachineId,
    number: u64,
}

impl Drop for Opened<'_> {
    fn drop(&mut self) {
        let mut links = lock(&self.inner.links);
        if let Some(open) = links.get_mut(&self.peer) {
            open.remove(&self.number);
            if open.is_empty() {
                links.remove(&self.peer);
            }
        }
    }
}

/// `record` as a machine shows it: a session or terminal whose owner is offline is
/// unreachable.
pub(crate) fn as_seen(record: AnyRecord, online: &BTreeSet<MachineId>) -> AnyRecord {
    if online.contains(&record.owner()) {
        return record;
    }
    match record {
        AnyRecord::Session(mut r) => {
            r.body.state = SessionState::Unreachable;
            AnyRecord::Session(r)
        }
        AnyRecord::Terminal(mut r) => {
            r.body.state = TerminalState::Unreachable;
            AnyRecord::Terminal(r)
        }
        other => other,
    }
}

/// A spawned task that is stopped when this is dropped.
struct Task(JoinHandle<()>);

impl Drop for Task {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Runs `work` with the store on the blocking pool, as [`crate::machine::run`] does, so a
/// slow disk or a busy store never stalls an async worker. A panic in `work` is raised again
/// here.
pub(crate) async fn on_store<T: Send + 'static>(
    store: &Arc<Mutex<Store>>,
    work: impl FnOnce(&mut Store) -> T + Send + 'static,
) -> T {
    let store = Arc::clone(store);
    match tokio::task::spawn_blocking(move || work(&mut lock(&store))).await {
        Ok(value) => value,
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        // The runtime is shutting down, and drops the task that waits here with it.
        Err(_) => std::future::pending().await,
    }
}
