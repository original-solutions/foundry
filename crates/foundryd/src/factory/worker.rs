//! What a run asks of a machine, and its fake.
//!
//! [`Worker`] is the seam between a run and the fleet: it makes workspaces, starts and talks to
//! sessions, and reads their turns. [`Guard`] says whether the factory may still act at all.
//! [`FleetView`] is what a run reads of the fleet in one pass, taken from the store.
//! [`FakeWorker`] and [`FleetView::builder`] stand in for both in tests, so a run is tested
//! with no daemon.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use foundry_proto::factory::{FactoryBody, MachineHeld, SessionTurn};
use foundry_proto::ids::{MachineId, ProjectId, SessionId, TerminalId, WorkspaceId};
use foundry_proto::record::{AnyRecord, MachineSample, Record, Session, Terminal, Workspace};
use foundry_proto::state::{SessionState, TerminalState};
use serde::{Deserialize, Serialize};

use crate::factory::messages::Message;
use crate::sessions::factory::{FactoryStart, SendRefused};
use crate::sessions::turns::TurnText;
use crate::store::{Store, StoreError};
use crate::workspaces::{Checkout, FactoryTag, Setup};

/// What a new workspace is made from. The machine it is made on is a separate argument of
/// [`Worker::create`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateWorkspace {
    /// The project the workspace belongs to.
    pub project: ProjectId,
    /// The workspace's name.
    pub name: String,
    /// How its worktree is checked out.
    pub checkout: Checkout,
    /// The setup it runs.
    pub setup: Setup,
    /// What marks it as the factory's, so a second create answers with the first.
    pub tag: FactoryTag,
}

/// Why a call to a worker did not do its work. A reason never holds a secret.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WorkerError {
    /// The machine could not be reached, or did not answer in time.
    #[error("the machine is unreachable: {0}")]
    Unreachable(String),
    /// The machine answered, and the call failed.
    #[error("the call failed: {0}")]
    Failed(String),
}

/// What a run asks of the fleet. Every call is made after [`Guard::check`] passes.
pub trait Worker: Send + Sync {
    /// Makes a workspace on `machine`. A second create with the same tag and name answers with
    /// the first workspace.
    fn create(
        &self,
        machine: MachineId,
        request: CreateWorkspace,
    ) -> Result<WorkspaceId, WorkerError>;

    /// Starts a factory session in `workspace` with `first` as its first message.
    fn start_session(
        &self,
        workspace: WorkspaceId,
        start: FactoryStart,
        first: Message,
    ) -> Result<SessionId, WorkerError>;

    /// Sends `message` to `session`, only if it has had no more than `only_if_prompts` prompts
    /// when one is given. Answers the count of prompts before the send. A call that could not
    /// reach the machine is [`SendRefused::Failed`].
    fn send(
        &self,
        session: SessionId,
        message: Message,
        only_if_prompts: Option<u64>,
    ) -> Result<u64, SendRefused>;

    /// The final message and the waiting question of turn `n` of `session`.
    fn turn(&self, session: SessionId, n: u64) -> Result<TurnText, WorkerError>;

    /// The output of `terminal`, from its end.
    fn tail(&self, terminal: TerminalId) -> Result<String, WorkerError>;

    /// Ends every factory session of `workspace`.
    fn end_sessions(&self, workspace: WorkspaceId) -> Result<(), WorkerError>;

    /// Archives `workspace`, forced: uncommitted work is lost.
    fn archive(&self, workspace: WorkspaceId) -> Result<(), WorkerError>;
}

/// Why the factory may no longer act.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the lease is lost: {reason}")]
pub struct LeaseLost {
    /// What was found.
    pub reason: String,
}

/// Whether the factory still holds its project's lease. It is called before every change on
/// GitHub and every call to a worker.
pub trait Guard: Send + Sync {
    /// The lease generation, while the lease is held.
    fn check(&self) -> Result<u64, LeaseLost>;
}

/// A guard that always holds the lease, at this generation, for tests.
#[derive(Clone, Copy, Debug)]
pub struct AlwaysHeld(pub u64);

impl Guard for AlwaysHeld {
    fn check(&self) -> Result<u64, LeaseLost> {
        todo!()
    }
}

/// A guard that has lost the lease, for tests.
#[derive(Clone, Copy, Debug)]
pub struct Lost;

impl Guard for Lost {
    fn check(&self) -> Result<u64, LeaseLost> {
        todo!()
    }
}

/// The fleet as one pass of the factory sees it: a snapshot, so a pass reads no store twice.
#[derive(Clone, Debug)]
pub struct FleetView {
    workspaces: Vec<Record<Workspace>>,
    sessions: Vec<Record<Session>>,
    terminals: BTreeMap<TerminalId, Record<Terminal>>,
    turns: BTreeMap<SessionId, SessionTurn>,
    online: BTreeSet<MachineId>,
    unreachable: BTreeMap<MachineId, Duration>,
    samples: Vec<Record<MachineSample>>,
    held: BTreeMap<MachineId, MachineHeld>,
}

impl FleetView {
    /// Reads the snapshot from `store`. `online` is the machines heard from, as P1's
    /// `Peers::online` gives them, and `unreachable` how long each offline machine has been
    /// unreachable, measured by the caller on the sleep-counting clock of [`crate::clock`].
    /// An owner whose records cannot be read is left out, with a warning.
    pub fn read(
        store: &Store,
        online: BTreeSet<MachineId>,
        unreachable: BTreeMap<MachineId, Duration>,
    ) -> Result<FleetView, StoreError> {
        let _ = (store, online, unreachable);
        todo!()
    }

    /// A builder for a view made of records given directly, for tests.
    pub fn builder() -> FleetViewBuilder {
        FleetViewBuilder::default()
    }

    /// The workspace of `project` named `name` that is not archived. The first in the order
    /// the records were held.
    pub fn workspace_named(&self, project: ProjectId, name: &str) -> Option<&Record<Workspace>> {
        let _ = (project, name);
        todo!()
    }

    /// The session of `workspace` that is not ended: a factory session, one with a turn,
    /// before any other, and among equals the highest version.
    pub fn session_of(&self, workspace: WorkspaceId) -> Option<&Record<Session>> {
        let _ = workspace;
        todo!()
    }

    /// The state of `session`. `None` for one unknown or deleted, and
    /// [`SessionState::Unreachable`] while its owner is offline.
    pub fn session_state(&self, session: SessionId) -> Option<SessionState> {
        let _ = session;
        todo!()
    }

    /// The state and exit code of `terminal`. `None` for one unknown. A terminal whose owner
    /// is offline is unreachable, and a deleted one has exited, as P1 treats it.
    pub fn terminal(&self, terminal: TerminalId) -> Option<(TerminalState, Option<i32>)> {
        let _ = terminal;
        todo!()
    }

    /// The newest turn of `session`, when it is a factory session.
    pub fn session_turn(&self, session: SessionId) -> Option<&SessionTurn> {
        let _ = session;
        todo!()
    }

    /// Whether `machine` has been heard from.
    pub fn online(&self, machine: MachineId) -> bool {
        let _ = machine;
        todo!()
    }

    /// How long `machine` has been unreachable: zero while it is online, and zero when no span
    /// was given.
    pub fn unreachable_for(&self, machine: MachineId) -> Duration {
        let _ = machine;
        todo!()
    }

    /// Every machine sample held.
    pub fn samples(&self) -> &[Record<MachineSample>] {
        todo!()
    }

    /// The logins `machine` holds tokens for, when it has said.
    pub fn held(&self, machine: MachineId) -> Option<&MachineHeld> {
        let _ = machine;
        todo!()
    }
}

/// Builds a [`FleetView`] from records given directly.
#[derive(Debug, Default)]
pub struct FleetViewBuilder {
    workspaces: Vec<Record<Workspace>>,
    sessions: Vec<Record<Session>>,
    terminals: BTreeMap<TerminalId, Record<Terminal>>,
    turns: BTreeMap<SessionId, SessionTurn>,
    online: BTreeSet<MachineId>,
    unreachable: BTreeMap<MachineId, Duration>,
    samples: Vec<Record<MachineSample>>,
    held: BTreeMap<MachineId, MachineHeld>,
}

impl FleetViewBuilder {
    /// Adds a workspace record, archived or not.
    pub fn workspace(mut self, record: Record<Workspace>) -> Self {
        self.workspaces.push(record);
        self
    }

    /// Adds a session record, ended or not.
    pub fn session(mut self, record: Record<Session>) -> Self {
        self.sessions.push(record);
        self
    }

    /// Adds a terminal record.
    pub fn terminal(mut self, record: Record<Terminal>) -> Self {
        self.terminals.insert(record.body.id, record);
        self
    }

    /// Adds a factory session's newest turn.
    pub fn turn(mut self, turn: SessionTurn) -> Self {
        self.turns.insert(turn.session, turn);
        self
    }

    /// Marks `machine` online.
    pub fn online(mut self, machine: MachineId) -> Self {
        self.online.insert(machine);
        self
    }

    /// Says `machine` has been unreachable for `span`.
    pub fn unreachable(mut self, machine: MachineId, span: Duration) -> Self {
        self.unreachable.insert(machine, span);
        self
    }

    /// Adds a machine sample.
    pub fn sample(mut self, record: Record<MachineSample>) -> Self {
        self.samples.push(record);
        self
    }

    /// Says what `machine` holds.
    pub fn held(mut self, machine: MachineId, held: MachineHeld) -> Self {
        self.held.insert(machine, held);
        self
    }

    /// The view.
    pub fn build(self) -> FleetView {
        FleetView {
            workspaces: self.workspaces,
            sessions: self.sessions,
            terminals: self.terminals,
            turns: self.turns,
            online: self.online,
            unreachable: self.unreachable,
            samples: self.samples,
            held: self.held,
        }
    }
}

/// One call to a [`FakeWorker`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Call {
    /// [`Worker::create`].
    Create {
        machine: MachineId,
        request: CreateWorkspace,
    },
    /// [`Worker::start_session`], with the four fields of its [`FactoryStart`].
    StartSession {
        workspace: WorkspaceId,
        project: ProjectId,
        login: String,
        model: String,
        effort: String,
        first: Message,
    },
    /// [`Worker::send`].
    Send {
        session: SessionId,
        message: Message,
        only_if_prompts: Option<u64>,
    },
    /// [`Worker::turn`].
    Turn { session: SessionId, n: u64 },
    /// [`Worker::tail`].
    Tail { terminal: TerminalId },
    /// [`Worker::end_sessions`].
    EndSessions { workspace: WorkspaceId },
    /// [`Worker::archive`].
    Archive { workspace: WorkspaceId },
}

/// A worker that answers from a script and records every call, in order.
///
/// Each method answers from its own queue: the next answer is taken while more than one is
/// queued, and the last is repeated. A method with nothing queued gives its default.
#[derive(Debug, Default)]
pub struct FakeWorker {
    state: Mutex<FakeState>,
}

#[derive(Debug, Default)]
struct FakeState {
    calls: Vec<Call>,
    create: VecDeque<Result<WorkspaceId, WorkerError>>,
    start_session: VecDeque<Result<SessionId, WorkerError>>,
    send: VecDeque<Result<u64, SendRefused>>,
    turn: VecDeque<Result<TurnText, WorkerError>>,
    tail: VecDeque<Result<String, WorkerError>>,
    end_sessions: VecDeque<Result<(), WorkerError>>,
    archive: VecDeque<Result<(), WorkerError>>,
    /// The workspaces the default create made, by tag and name.
    made: Vec<(FactoryTag, String, WorkspaceId)>,
    /// How many messages each session was delivered.
    delivered: BTreeMap<SessionId, u64>,
}

impl FakeWorker {
    /// A worker with nothing scripted.
    pub fn new() -> Self {
        Self::default()
    }

    /// Every call so far, in order, whether it was answered from the script or not, and
    /// whether it failed.
    pub fn calls(&self) -> Vec<Call> {
        todo!()
    }

    /// Queues an answer for [`Worker::create`].
    pub fn on_create(&self, answer: Result<WorkspaceId, WorkerError>) -> &Self {
        let _ = answer;
        todo!()
    }

    /// Queues an answer for [`Worker::start_session`].
    pub fn on_start_session(&self, answer: Result<SessionId, WorkerError>) -> &Self {
        let _ = answer;
        todo!()
    }

    /// Queues an answer for [`Worker::send`].
    pub fn on_send(&self, answer: Result<u64, SendRefused>) -> &Self {
        let _ = answer;
        todo!()
    }

    /// Queues an answer for [`Worker::turn`].
    pub fn on_turn(&self, answer: Result<TurnText, WorkerError>) -> &Self {
        let _ = answer;
        todo!()
    }

    /// Queues an answer for [`Worker::tail`].
    pub fn on_tail(&self, answer: Result<String, WorkerError>) -> &Self {
        let _ = answer;
        todo!()
    }

    /// Queues an answer for [`Worker::end_sessions`].
    pub fn on_end_sessions(&self, answer: Result<(), WorkerError>) -> &Self {
        let _ = answer;
        todo!()
    }

    /// Queues an answer for [`Worker::archive`].
    pub fn on_archive(&self, answer: Result<(), WorkerError>) -> &Self {
        let _ = answer;
        todo!()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FakeState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Worker for FakeWorker {
    fn create(
        &self,
        machine: MachineId,
        request: CreateWorkspace,
    ) -> Result<WorkspaceId, WorkerError> {
        let _ = (machine, request, self.lock());
        todo!()
    }

    fn start_session(
        &self,
        workspace: WorkspaceId,
        start: FactoryStart,
        first: Message,
    ) -> Result<SessionId, WorkerError> {
        let _ = (workspace, start, first);
        todo!()
    }

    fn send(
        &self,
        session: SessionId,
        message: Message,
        only_if_prompts: Option<u64>,
    ) -> Result<u64, SendRefused> {
        let _ = (session, message, only_if_prompts);
        todo!()
    }

    fn turn(&self, session: SessionId, n: u64) -> Result<TurnText, WorkerError> {
        let _ = (session, n);
        todo!()
    }

    fn tail(&self, terminal: TerminalId) -> Result<String, WorkerError> {
        let _ = terminal;
        todo!()
    }

    fn end_sessions(&self, workspace: WorkspaceId) -> Result<(), WorkerError> {
        let _ = workspace;
        todo!()
    }

    fn archive(&self, workspace: WorkspaceId) -> Result<(), WorkerError> {
        let _ = workspace;
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use foundry_proto::factory::{FactoryBody, FactoryRecord};

    use super::*;

    fn id(n: u128) -> uuid::Uuid {
        uuid::Uuid::from_u128(n)
    }

    fn machine(n: u128) -> MachineId {
        MachineId(id(n))
    }

    fn project(n: u128) -> ProjectId {
        ProjectId(id(n))
    }

    fn msg(line: &str) -> Message {
        Message {
            line: line.into(),
            body: None,
        }
    }

    fn request(name: &str, run: &str) -> CreateWorkspace {
        CreateWorkspace {
            project: project(1),
            name: name.into(),
            checkout: Checkout::NewBranch {
                base: "main".into(),
                branch: None,
            },
            setup: Setup::None,
            tag: FactoryTag {
                project: project(1),
                run: run.into(),
            },
        }
    }

    fn start() -> FactoryStart {
        FactoryStart {
            project: project(1),
            login: "bot".into(),
            model: "m".into(),
            effort: "high".into(),
        }
    }

    fn workspace_record(
        owner: MachineId,
        version: u64,
        deleted: bool,
        wid: u128,
        proj: u128,
        name: &str,
    ) -> Record<Workspace> {
        Record {
            owner,
            version,
            deleted,
            body: Workspace {
                id: WorkspaceId(id(wid)),
                project: project(proj),
                name: name.into(),
                branch: name.into(),
                base: "main".into(),
                setup: None,
            },
        }
    }

    fn session_record(
        owner: MachineId,
        version: u64,
        deleted: bool,
        sid: u128,
        wid: u128,
        state: SessionState,
    ) -> Record<Session> {
        Record {
            owner,
            version,
            deleted,
            body: Session {
                id: SessionId(id(sid)),
                workspace: WorkspaceId(id(wid)),
                terminal: TerminalId(id(sid + 1000)),
                model: "m".into(),
                state,
            },
        }
    }

    fn terminal_record(
        owner: MachineId,
        deleted: bool,
        tid: u128,
        state: TerminalState,
        exit_code: Option<i32>,
    ) -> Record<Terminal> {
        Record {
            owner,
            version: 1,
            deleted,
            body: Terminal {
                id: TerminalId(id(tid)),
                workspace: WorkspaceId(id(1)),
                state,
                exit_code,
            },
        }
    }

    fn turn_of(sid: u128, wid: u128) -> SessionTurn {
        SessionTurn {
            session: SessionId(id(sid)),
            workspace: WorkspaceId(id(wid)),
            project: project(1),
            turn: 2,
            prompts: 3,
            question: false,
        }
    }

    fn open() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("foundry.db")).unwrap();
        (dir, store)
    }

    #[test]
    fn the_fake_records_each_call_in_order() {
        let fake = FakeWorker::new();
        let m = machine(1);
        let ws = WorkspaceId(id(5));
        let sess = SessionId(id(6));
        let term = TerminalId(id(7));
        fake.archive(ws).unwrap();
        fake.tail(term).unwrap();
        let req = request("a", "r1");
        fake.create(m, req.clone()).unwrap();
        fake.turn(sess, 3).unwrap();
        fake.start_session(ws, start(), msg("go")).unwrap();
        fake.end_sessions(ws).unwrap();
        fake.send(sess, msg("hi"), Some(2)).unwrap();
        assert_eq!(
            fake.calls(),
            vec![
                Call::Archive { workspace: ws },
                Call::Tail { terminal: term },
                Call::Create {
                    machine: m,
                    request: req
                },
                Call::Turn { session: sess, n: 3 },
                Call::StartSession {
                    workspace: ws,
                    project: project(1),
                    login: "bot".into(),
                    model: "m".into(),
                    effort: "high".into(),
                    first: msg("go"),
                },
                Call::EndSessions { workspace: ws },
                Call::Send {
                    session: sess,
                    message: msg("hi"),
                    only_if_prompts: Some(2)
                },
            ]
        );
    }

    #[test]
    fn a_failed_call_is_recorded_too() {
        let fake = FakeWorker::new();
        let ws = WorkspaceId(id(5));
        fake.on_archive(Err(WorkerError::Unreachable("down".into())));
        assert_eq!(
            fake.archive(ws),
            Err(WorkerError::Unreachable("down".into()))
        );
        assert_eq!(fake.calls(), vec![Call::Archive { workspace: ws }]);
    }

    #[test]
    fn scripted_answers_come_in_order_and_the_last_repeats() {
        let fake = FakeWorker::new();
        let ws = WorkspaceId(id(5));
        fake.on_end_sessions(Err(WorkerError::Failed("one".into())))
            .on_end_sessions(Ok(()));
        assert_eq!(
            fake.end_sessions(ws),
            Err(WorkerError::Failed("one".into()))
        );
        assert_eq!(fake.end_sessions(ws), Ok(()));
        assert_eq!(fake.end_sessions(ws), Ok(()));
        let term = TerminalId(id(7));
        fake.on_tail(Ok("out".into()));
        assert_eq!(fake.tail(term), Ok("out".into()));
        assert_eq!(fake.tail(term), Ok("out".into()));
    }

    #[test]
    fn an_unscripted_method_gives_its_default() {
        let fake = FakeWorker::new();
        let sess = SessionId(id(6));
        assert_eq!(
            fake.turn(sess, 1),
            Ok(TurnText {
                message: String::new(),
                question: None
            })
        );
        assert_eq!(fake.tail(TerminalId(id(7))), Ok(String::new()));
        assert_eq!(fake.archive(WorkspaceId(id(5))), Ok(()));
        let a = fake.start_session(WorkspaceId(id(5)), start(), msg("x"));
        let b = fake.start_session(WorkspaceId(id(5)), start(), msg("x"));
        assert_ne!(a.unwrap(), b.unwrap());
    }

    #[test]
    fn a_second_default_create_with_the_same_tag_and_name_answers_the_first() {
        let fake = FakeWorker::new();
        let m = machine(1);
        let first = fake.create(m, request("a", "r1")).unwrap();
        assert_eq!(fake.create(m, request("a", "r1")).unwrap(), first);
        assert_ne!(fake.create(m, request("b", "r1")).unwrap(), first);
        assert_ne!(fake.create(m, request("a", "r2")).unwrap(), first);
    }

    #[test]
    fn a_scripted_create_is_not_made_the_first_of_its_tag() {
        let fake = FakeWorker::new();
        let m = machine(1);
        let scripted = WorkspaceId(id(9));
        fake.on_create(Ok(scripted));
        assert_eq!(fake.create(m, request("a", "r1")).unwrap(), scripted);
        let queue_done = fake.create(m, request("a", "r1")).unwrap();
        assert_eq!(queue_done, scripted, "the last scripted answer repeats");
    }

    #[test]
    fn the_default_send_answers_the_count_of_messages_delivered_before() {
        let fake = FakeWorker::new();
        let ws = WorkspaceId(id(5));
        let sess = fake.start_session(ws, start(), msg("go")).unwrap();
        assert_eq!(fake.send(sess, msg("a"), None), Ok(1));
        assert_eq!(fake.send(sess, msg("b"), None), Ok(2));
    }

    #[test]
    fn a_refused_send_is_not_counted() {
        let fake = FakeWorker::new();
        let ws = WorkspaceId(id(5));
        let sess = fake.start_session(ws, start(), msg("go")).unwrap();
        fake.on_send(Err(SendRefused::NotIdle));
        assert_eq!(fake.send(sess, msg("a"), None), Err(SendRefused::NotIdle));
        // The script holds one answer, which repeats; a fresh fake shows the default again.
        let fake = FakeWorker::new();
        let sess = fake.start_session(ws, start(), msg("go")).unwrap();
        fake.on_send(Err(SendRefused::NotIdle)).on_send(Ok(0));
        assert_eq!(fake.send(sess, msg("a"), None), Err(SendRefused::NotIdle));
        let state_after = FakeWorker::new();
        let sess = state_after.start_session(ws, start(), msg("go")).unwrap();
        assert_eq!(state_after.send(sess, msg("a"), None), Ok(1));
    }

    #[test]
    fn the_guards_answer_the_generation_or_a_loss() {
        assert_eq!(AlwaysHeld(7).check(), Ok(7));
        assert!(Lost.check().is_err());
    }

    #[test]
    fn workspace_named_ignores_an_archived_workspace() {
        let m = machine(1);
        let view = FleetView::builder()
            .workspace(workspace_record(m, 1, true, 10, 1, "run"))
            .workspace(workspace_record(m, 2, false, 11, 1, "run"))
            .workspace(workspace_record(m, 3, false, 12, 2, "run"))
            .build();
        let found = view.workspace_named(project(1), "run").unwrap();
        assert_eq!(found.body.id, WorkspaceId(id(11)));
        assert_eq!(
            view.workspace_named(project(2), "run").unwrap().body.id,
            WorkspaceId(id(12))
        );
        assert!(view.workspace_named(project(3), "run").is_none());

        let only_archived = FleetView::builder()
            .workspace(workspace_record(m, 1, true, 10, 1, "run"))
            .build();
        assert!(only_archived.workspace_named(project(1), "run").is_none());
    }

    #[test]
    fn workspace_named_finds_a_factory_workspace_whose_record_arrived_after_its_create() {
        let (_dir, mut store) = open();
        let m = machine(1);
        let fake = FakeWorker::new();
        let made = fake.create(m, request("run", "r1")).unwrap();
        let view = FleetView::read(&store, BTreeSet::from([m]), BTreeMap::new()).unwrap();
        assert!(view.workspace_named(project(1), "run").is_none());

        let record = Record {
            owner: m,
            version: 1,
            deleted: false,
            body: Workspace {
                id: made,
                project: project(1),
                name: "run".into(),
                branch: "run".into(),
                base: "main".into(),
                setup: None,
            },
        };
        store.apply(&AnyRecord::Workspace(record), m).unwrap();
        let view = FleetView::read(&store, BTreeSet::from([m]), BTreeMap::new()).unwrap();
        assert_eq!(view.workspace_named(project(1), "run").unwrap().body.id, made);
    }

    #[test]
    fn a_read_carries_sessions_terminals_turns_held_and_samples() {
        let (_dir, mut store) = open();
        let m = machine(1);
        let gone = machine(2);
        store
            .apply(
                &AnyRecord::Workspace(workspace_record(m, 1, false, 10, 1, "run")),
                m,
            )
            .unwrap();
        store
            .apply(
                &AnyRecord::Session(session_record(m, 2, false, 20, 10, SessionState::Idle)),
                m,
            )
            .unwrap();
        store
            .apply(
                &AnyRecord::Terminal(terminal_record(m, false, 1020, TerminalState::Running, None)),
                m,
            )
            .unwrap();
        store
            .apply(
                &AnyRecord::MachineSample(Record {
                    owner: m,
                    version: 4,
                    deleted: false,
                    body: MachineSample {
                        load_per_core: 0.5,
                        free_memory: 1,
                        free_disk: 2,
                        sessions: 1,
                        update_refused: None,
                    },
                }),
                m,
            )
            .unwrap();
        // A session and a terminal of a machine that is not online.
        store
            .apply(
                &AnyRecord::Session(session_record(gone, 1, false, 21, 11, SessionState::Working)),
                gone,
            )
            .unwrap();
        store
            .apply(
                &AnyRecord::Terminal(terminal_record(gone, false, 30, TerminalState::Running, None)),
                gone,
            )
            .unwrap();
        let turn = turn_of(20, 10);
        store
            .apply_factory(
                &FactoryRecord {
                    owner: m,
                    version: 1,
                    body: FactoryBody::Turn(turn.clone()),
                },
                m,
            )
            .unwrap();
        let held = MachineHeld {
            logins: vec!["bot".into()],
            skill: true,
        };
        store
            .apply_factory(
                &FactoryRecord {
                    owner: m,
                    version: 2,
                    body: FactoryBody::Held(held.clone()),
                },
                m,
            )
            .unwrap();

        let view = FleetView::read(
            &store,
            BTreeSet::from([m]),
            BTreeMap::from([(gone, Duration::from_secs(90))]),
        )
        .unwrap();
        assert_eq!(
            view.session_of(WorkspaceId(id(10))).unwrap().body.id,
            SessionId(id(20))
        );
        assert_eq!(view.session_state(SessionId(id(20))), Some(SessionState::Idle));
        assert_eq!(
            view.terminal(TerminalId(id(1020))),
            Some((TerminalState::Running, None))
        );
        assert_eq!(view.session_turn(SessionId(id(20))), Some(&turn));
        assert_eq!(view.held(m), Some(&held));
        assert_eq!(view.held(gone), None);
        assert_eq!(view.samples().len(), 1);
        assert!(view.online(m));
        assert!(!view.online(gone));
        assert_eq!(
            view.session_state(SessionId(id(21))),
            Some(SessionState::Unreachable)
        );
        assert_eq!(
            view.terminal(TerminalId(id(30))),
            Some((TerminalState::Unreachable, None))
        );
        assert_eq!(view.unreachable_for(gone), Duration::from_secs(90));
    }

    #[test]
    fn session_of_prefers_a_factory_session_and_ignores_deleted_ones() {
        let m = machine(1);
        let view = FleetView::builder()
            .online(m)
            .session(session_record(m, 5, false, 20, 10, SessionState::Idle))
            .session(session_record(m, 3, false, 21, 10, SessionState::Idle))
            .session(session_record(m, 9, true, 22, 10, SessionState::Idle))
            .turn(turn_of(21, 10))
            .build();
        let found = view.session_of(WorkspaceId(id(10))).unwrap();
        assert_eq!(found.body.id, SessionId(id(21)));
        assert_eq!(view.session_state(SessionId(id(22))), None);

        let plain = FleetView::builder()
            .session(session_record(m, 5, false, 20, 10, SessionState::Idle))
            .session(session_record(m, 6, false, 23, 10, SessionState::Idle))
            .session(session_record(m, 9, true, 22, 10, SessionState::Idle))
            .build();
        assert_eq!(
            plain.session_of(WorkspaceId(id(10))).unwrap().body.id,
            SessionId(id(23))
        );
        assert!(plain.session_of(WorkspaceId(id(99))).is_none());
    }

    #[test]
    fn a_deleted_terminal_reads_as_exited_with_its_code() {
        let m = machine(1);
        let view = FleetView::builder()
            .online(m)
            .terminal(terminal_record(m, true, 30, TerminalState::Running, Some(3)))
            .build();
        assert_eq!(
            view.terminal(TerminalId(id(30))),
            Some((TerminalState::Exited, Some(3)))
        );
        assert_eq!(view.terminal(TerminalId(id(31))), None);
    }

    #[test]
    fn unreachable_for_is_zero_online_and_the_span_offline() {
        let a = machine(1);
        let b = machine(2);
        let c = machine(3);
        let view = FleetView::builder()
            .online(a)
            .unreachable(a, Duration::from_secs(5))
            .unreachable(b, Duration::from_secs(40))
            .build();
        assert_eq!(view.unreachable_for(a), Duration::ZERO);
        assert_eq!(view.unreachable_for(b), Duration::from_secs(40));
        assert_eq!(view.unreachable_for(c), Duration::ZERO);
    }
}
