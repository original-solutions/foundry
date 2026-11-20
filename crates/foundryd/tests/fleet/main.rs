//! The fleet harness: several daemons in one process with links a test can cut and heal.
//!
//! Each daemon is a store and its [`Peers`]. Links are in-memory channels that pass through
//! the harness, so a test can cut a machine off and heal it, and run with tokio's paused clock
//! to simulate hours in milliseconds.

mod call;
mod coordinator;
mod relay;
mod sync;

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use foundry_proto::ids::{MachineId, ProjectId, SessionId, TerminalId, WorkspaceId};
use foundry_proto::record::{AnyRecord, MachineSample, Record, Session, Terminal, Workspace};
use foundry_proto::settings::{Machine, SharedSettings};
use foundry_proto::state::{SessionState, TerminalState};
use foundryd::coordinator::Coordinator;
use foundryd::store::{Applied, Store};
use foundryd::sync::{Conn, Dialer, Peers};
use futures::future::BoxFuture;
use tempfile::TempDir;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;

/// One machine's daemon: its store and its peer links, running until it is dropped.
pub struct Daemon {
    pub id: MachineId,
    pub store: Arc<Mutex<Store>>,
    pub peers: Peers,
    pub db: PathBuf,
    /// The coordinator its links hand settings and edits to, once [`Daemon::coordinate`] ran.
    pub coordinator: Option<Arc<Coordinator>>,
    run: JoinHandle<()>,
    _dir: TempDir,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.run.abort();
    }
}

impl Daemon {
    /// Starts a daemon for machine `id` holding `settings`, dialing its peers with `dialer`.
    pub fn start(id: MachineId, settings: &SharedSettings, dialer: Arc<dyn Dialer>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("foundry.db");
        let mut store = Store::open(&db).unwrap();
        assert!(store.apply_settings(settings.clone()).unwrap());
        let store = Arc::new(Mutex::new(store));
        let peers = Peers::new(id, store.clone(), dialer);
        let running = peers.clone();
        let run = tokio::spawn(async move { running.run().await });
        Self {
            id,
            store,
            peers,
            db,
            coordinator: None,
            run,
            _dir: dir,
        }
    }

    /// Stops this daemon and starts it again on the same database, dialing with `dialer`, with
    /// a new coordinator when it had one. A task the stopped daemon left running goes on with a
    /// scratch store, so it writes nothing the restarted daemon reads, as a stopped process
    /// writes nothing. Its links must already be closed: [`Fleet::restart`] cuts them.
    fn restart(&mut self, dialer: Arc<dyn Dialer>) {
        self.run.abort();
        let scratch = Store::open(self._dir.path().join("stopped.db")).unwrap();
        drop(std::mem::replace(&mut *lock(&self.store), scratch));
        self.store = Arc::new(Mutex::new(Store::open(&self.db).unwrap()));
        self.peers = Peers::new(self.id, self.store.clone(), dialer);
        let running = self.peers.clone();
        self.run = tokio::spawn(async move { running.run().await });
        if self.coordinator.is_some() {
            self.coordinate();
        }
    }

    /// Gives this daemon a coordinator and returns it. Call it before the test first awaits,
    /// so no link opens without one.
    pub fn coordinate(&mut self) -> Arc<Coordinator> {
        let coordinator = Arc::new(Coordinator::new(
            self.id,
            self.store.clone(),
            self.peers.clone(),
        ));
        self.peers.coordinate(coordinator.clone());
        self.coordinator = Some(coordinator.clone());
        coordinator
    }

    /// Its coordinator; panics if [`Daemon::coordinate`] has not run.
    pub fn coordinator(&self) -> Arc<Coordinator> {
        self.coordinator
            .clone()
            .expect("a daemon with a coordinator")
    }

    /// The shared settings this daemon holds.
    pub fn settings(&self) -> SharedSettings {
        lock(&self.store).settings().unwrap()
    }

    /// Writes one of this machine's own records under its next version and returns it.
    pub fn write(&self, make: impl FnOnce(MachineId, u64) -> AnyRecord) -> AnyRecord {
        let mut store = lock(&self.store);
        let version = store.own_next_version().unwrap();
        let record = make(self.id, version);
        assert_eq!(store.apply(&record, self.id).unwrap(), Applied::Stored);
        record
    }

    /// Replaces the shared settings this daemon holds.
    pub fn set_settings(&self, settings: SharedSettings) {
        assert!(lock(&self.store).apply_settings(settings).unwrap());
    }

    /// Every record held, from every owner.
    fn all(&self) -> Vec<AnyRecord> {
        let store = lock(&self.store);
        let owners: Vec<MachineId> = store.versions().unwrap().into_keys().collect();
        owners
            .into_iter()
            .flat_map(|owner| store.since(owner, 0).unwrap())
            .collect()
    }

    /// Every record held, as JSON, in a stable order.
    pub fn dump(&self) -> Vec<String> {
        let mut records: Vec<String> = self
            .all()
            .iter()
            .map(|record| serde_json::to_string(record).unwrap())
            .collect();
        records.sort();
        records
    }

    /// The first record held that `find` accepts.
    pub fn held(&self, find: impl Fn(&AnyRecord) -> bool) -> Option<AnyRecord> {
        self.all().into_iter().find(|record| find(record))
    }
}

/// Settings listing `ids`, each named `machine-<i>` with its own address.
pub fn settings_for(ids: &[MachineId]) -> SharedSettings {
    SharedSettings {
        seq: 1,
        machines: ids
            .iter()
            .enumerate()
            .map(|(i, id)| Machine {
                id: *id,
                name: format!("machine-{i}"),
                address: format!("100.64.0.{}", i + 1),
            })
            .collect(),
        priority: ids.to_vec(),
        ..SharedSettings::default()
    }
}

/// Machines linked through the harness.
pub struct Fleet {
    pub daemons: Vec<Daemon>,
    net: Arc<Net>,
}

impl Fleet {
    /// Starts `n` daemons that list each other as peers.
    pub fn start(n: usize) -> Self {
        Self::start_with(n, &[])
    }

    /// Starts `n` daemons whose machine list also names `extra` machines that run no daemon,
    /// so a test can speak for them over a raw link.
    pub fn start_with(n: usize, extra: &[MachineId]) -> Self {
        let ids: Vec<MachineId> = (0..n).map(|_| MachineId::random()).collect();
        let listed: Vec<MachineId> = ids.iter().chain(extra).copied().collect();
        let settings = settings_for(&listed);
        let net = Arc::new(Net::default());
        let daemons = ids
            .into_iter()
            .map(|id| {
                let dialer = NetDialer {
                    from: id,
                    net: net.clone(),
                };
                let daemon = Daemon::start(id, &settings, Arc::new(dialer));
                lock(&net.peers).insert(id, daemon.peers.clone());
                daemon
            })
            .collect();
        Self { daemons, net }
    }

    /// Starts `n` daemons that list each other as peers, each with a coordinator.
    pub fn start_coordinated(n: usize) -> Self {
        Self::start_coordinated_with(n, &[])
    }

    /// As [`Fleet::start_with`], each daemon with a coordinator.
    pub fn start_coordinated_with(n: usize, extra: &[MachineId]) -> Self {
        let mut fleet = Self::start_with(n, extra);
        for daemon in &mut fleet.daemons {
            daemon.coordinate();
        }
        fleet
    }

    /// Cuts every link to and from daemon `i`, and refuses new ones until it is healed.
    pub fn cut(&self, i: usize) {
        let id = self.daemons[i].id;
        self.net.cut.send_modify(|cut| {
            cut.insert(id);
        });
    }

    /// Lets daemon `i` link again.
    pub fn heal(&self, i: usize) {
        let id = self.daemons[i].id;
        self.net.cut.send_modify(|cut| {
            cut.remove(&id);
        });
    }

    /// Restarts daemon `i` ([`Daemon::restart`]): its links close, it starts again on the same
    /// store with nothing held in memory, and it links again.
    pub async fn restart(&mut self, i: usize) {
        self.cut(i);
        // Each passing link sees the cut and closes both its ends.
        tokio::time::sleep(Duration::from_millis(10)).await;
        let id = self.daemons[i].id;
        let dialer = NetDialer {
            from: id,
            net: self.net.clone(),
        };
        self.daemons[i].restart(Arc::new(dialer));
        lock(&self.net.peers).insert(id, self.daemons[i].peers.clone());
        self.heal(i);
    }

    /// True when every daemon holds the same records.
    pub fn converged(&self) -> bool {
        let first = self.daemons[0].dump();
        self.daemons.iter().all(|d| d.dump() == first)
    }
}

/// The in-memory network: which daemons exist and which machines are cut off.
#[derive(Default)]
struct Net {
    peers: Mutex<BTreeMap<MachineId, Peers>>,
    cut: watch::Sender<BTreeSet<MachineId>>,
}

struct NetDialer {
    from: MachineId,
    net: Arc<Net>,
}

impl Dialer for NetDialer {
    fn dial(&self, peer: &Machine) -> BoxFuture<'static, io::Result<Conn>> {
        let result = self.connect(peer.id);
        Box::pin(async move { result })
    }
}

impl NetDialer {
    fn connect(&self, to: MachineId) -> io::Result<Conn> {
        let ends = [self.from, to];
        if self.net.cut.borrow().iter().any(|id| ends.contains(id)) {
            return Err(io::Error::other("the link is cut"));
        }
        let Some(target) = lock(&self.net.peers).get(&to).cloned() else {
            return Err(io::Error::other("no daemon at that machine"));
        };
        let (dialer_end, a) = Conn::pair();
        let (b, acceptor_end) = Conn::pair();
        tokio::spawn(forward(a, b, self.net.cut.subscribe(), ends));
        tokio::spawn(async move { target.accept(acceptor_end).await });
        Ok(dialer_end)
    }
}

/// Passes messages both ways between `a` and `b` until either end closes or either machine
/// is cut off, then drops both, which closes the link at each daemon.
async fn forward(
    a: Conn,
    b: Conn,
    mut cut: watch::Receiver<BTreeSet<MachineId>>,
    ends: [MachineId; 2],
) {
    let Conn {
        send: to_a,
        recv: mut from_a,
    } = a;
    let Conn {
        send: to_b,
        recv: mut from_b,
    } = b;
    let is_cut = |cut: &BTreeSet<MachineId>| ends.iter().any(|id| cut.contains(id));
    loop {
        tokio::select! {
            message = from_a.recv() => {
                let Some(m) = message else { return };
                if to_b.send(m).await.is_err() {
                    return;
                }
            }
            message = from_b.recv() => {
                let Some(m) = message else { return };
                if to_a.send(m).await.is_err() {
                    return;
                }
            }
            _ = async { cut.wait_for(is_cut).await.is_ok() } => return,
        }
    }
}

pub fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Waits until `done` holds, checking every 100 ms, and returns how long that took. Fails if
/// it takes longer than `limit`.
pub async fn until(limit: Duration, what: &str, mut done: impl FnMut() -> bool) -> Duration {
    let start = Instant::now();
    loop {
        if done() {
            return start.elapsed();
        }
        assert!(
            start.elapsed() <= limit,
            "{what}: not done within {limit:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

pub fn workspace(id: WorkspaceId, deleted: bool) -> impl FnOnce(MachineId, u64) -> AnyRecord {
    move |owner, version| {
        AnyRecord::Workspace(Record {
            owner,
            version,
            deleted,
            body: Workspace {
                id,
                project: ProjectId(uuid::Uuid::from_u128(1)),
                name: "fix login".into(),
                branch: "fix-login".into(),
                base: "main".into(),
                setup: None,
            },
        })
    }
}

pub fn terminal(
    id: TerminalId,
    workspace: WorkspaceId,
) -> impl FnOnce(MachineId, u64) -> AnyRecord {
    move |owner, version| {
        AnyRecord::Terminal(Record {
            owner,
            version,
            deleted: false,
            body: Terminal {
                id,
                workspace,
                state: TerminalState::Running,
                exit_code: None,
            },
        })
    }
}

pub fn session(
    id: SessionId,
    workspace: WorkspaceId,
    terminal: TerminalId,
    state: SessionState,
) -> impl FnOnce(MachineId, u64) -> AnyRecord {
    move |owner, version| {
        AnyRecord::Session(Record {
            owner,
            version,
            deleted: false,
            body: Session {
                id,
                workspace,
                terminal,
                model: "claude-opus-5-5".into(),
                state,
            },
        })
    }
}

pub fn sample(sessions: u32) -> impl FnOnce(MachineId, u64) -> AnyRecord {
    move |owner, version| {
        AnyRecord::MachineSample(Record {
            owner,
            version,
            deleted: false,
            body: MachineSample {
                load_per_core: 0.5,
                free_memory: 1 << 30,
                free_disk: 1 << 35,
                sessions,
                update_refused: None,
            },
        })
    }
}

/// The ids of one workspace with a terminal running a session.
#[derive(Clone, Copy)]
pub struct Ids {
    pub workspace: WorkspaceId,
    pub terminal: TerminalId,
    pub session: SessionId,
}

impl Ids {
    pub fn random() -> Self {
        Self {
            workspace: WorkspaceId::random(),
            terminal: TerminalId::random(),
            session: SessionId::random(),
        }
    }
}

/// Writes a workspace, its terminal, a session in `state` and a machine sample on `daemon`.
pub fn populate(daemon: &Daemon, ids: Ids, state: SessionState) {
    daemon.write(workspace(ids.workspace, false));
    daemon.write(terminal(ids.terminal, ids.workspace));
    daemon.write(session(ids.session, ids.workspace, ids.terminal, state));
    daemon.write(sample(1));
}
