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
//! sessions and terminals as unreachable.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use foundry_proto::ids::MachineId;
use foundry_proto::record::AnyRecord;
use foundry_proto::settings::Machine;
use foundry_proto::state::{SessionState, TerminalState};
use futures::future::BoxFuture;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::store::{Store, StoreError};

mod link;
pub mod relay;
pub mod ws;

/// A machine not heard from for this long is offline.
pub const OFFLINE_AFTER: Duration = Duration::from_secs(15);

/// How often a link sends this machine's new records. An empty send is the heartbeat that
/// keeps the machine online on the other end.
const TICK: Duration = Duration::from_secs(1);

/// How long to wait before dialing a peer again after a link ends or a dial fails, and how
/// often the machine list is read for peers added or removed.
const REDIAL: Duration = Duration::from_secs(1);

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
}

impl Peers {
    pub fn new(me: MachineId, store: Arc<Mutex<Store>>, dialer: Arc<dyn Dialer>) -> Self {
        Self {
            inner: Arc::new(Inner {
                me,
                store,
                dialer,
                heard: Mutex::new(BTreeMap::new()),
            }),
        }
    }

    /// Keeps a link open to every other machine in the machine list, redialing a link that
    /// ends, until the returned future is dropped. A machine added to the list is dialed and
    /// a machine removed from it is dropped within [`REDIAL`].
    pub async fn run(&self) {
        let mut links: BTreeMap<MachineId, (Machine, Task)> = BTreeMap::new();
        let mut every = tokio::time::interval(REDIAL);
        loop {
            every.tick().await;
            let machines = match lock(&self.inner.store).settings() {
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

    /// Serves a link a peer opened, until it ends.
    pub async fn accept(&self, conn: Conn) {
        link::run(&self.inner, conn, None).await;
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
    /// owner shown as unreachable.
    pub fn state(&self) -> Result<Vec<AnyRecord>, StoreError> {
        let online = self.online();
        let store = lock(&self.inner.store);
        let mut records = Vec::new();
        for owner in store.versions()?.into_keys() {
            records.extend(store.since(owner, 0)?);
        }
        Ok(records
            .into_iter()
            .map(|record| as_seen(record, &online))
            .collect())
    }

    async fn dial_forever(self, peer: Machine) {
        loop {
            match self.inner.dialer.dial(&peer).await {
                Ok(conn) => link::run(&self.inner, conn, Some(peer.id)).await,
                Err(error) => tracing::debug!("sync: cannot reach {}: {error}", peer.name),
            }
            tokio::time::sleep(REDIAL).await;
        }
    }
}

impl Inner {
    fn heard_from(&self, peer: MachineId) {
        lock(&self.heard).insert(peer, Instant::now());
    }
}

/// `record` as a machine shows it: a session or terminal whose owner is offline is
/// unreachable.
fn as_seen(record: AnyRecord, online: &BTreeSet<MachineId>) -> AnyRecord {
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

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
