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

use foundry_proto::factory::{MachineHeld, SessionTurn};
use foundry_proto::ids::{MachineId, ProjectId, SessionId, TerminalId, WorkspaceId};
use foundry_proto::record::{AnyRecord, MachineSample, Record, Session, Terminal, Workspace};
use foundry_proto::state::{SessionState, TerminalState};
use serde::{Deserialize, Serialize};

use crate::factory::messages::Message;
use crate::sessions::factory::{FactoryStart, SendRefused};
use crate::sessions::turns::TurnText;
use crate::store::{Store, StoreError};
use crate::sync::as_seen;
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
        Ok(self.0)
    }
}

/// A guard that has lost the lease, for tests.
#[derive(Clone, Copy, Debug)]
pub struct Lost;

impl Guard for Lost {
    fn check(&self) -> Result<u64, LeaseLost> {
        Err(LeaseLost {
            reason: "the test's guard is lost".into(),
        })
    }
}

/// The fleet as one pass of the factory sees it: a snapshot, so a pass reads no store twice.
#[derive(Clone, Debug, Default)]
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
        let mut builder = FleetView::builder();
        builder.view.online = online;
        builder.view.unreachable = unreachable;
        let versions = store.versions()?;
        let mut machines: BTreeSet<MachineId> = versions.keys().copied().collect();
        machines.extend(store.factory_versions()?.into_keys());
        for &owner in versions.keys() {
            let records = match store.since(owner, 0) {
                Ok(records) => records,
                Err(error) => {
                    tracing::warn!("factory: records of {owner} not shown: {error}");
                    continue;
                }
            };
            for record in records {
                match record {
                    AnyRecord::Workspace(r) => builder.view.workspaces.push(r),
                    AnyRecord::Session(r) => builder.view.sessions.push(r),
                    AnyRecord::Terminal(r) => {
                        builder.view.terminals.insert(r.body.id, r);
                    }
                    AnyRecord::MachineSample(r) => builder.view.samples.push(r),
                }
            }
        }
        for session in builder.view.sessions.iter().filter(|r| !r.deleted) {
            if let Some(turn) = store.session_turn(session.body.id)? {
                builder.view.turns.insert(session.body.id, turn);
            }
        }
        for machine in machines {
            if let Some(held) = store.held(machine)? {
                builder.view.held.insert(machine, held);
            }
        }
        Ok(builder.build())
    }

    /// A builder for a view made of records given directly, for tests.
    pub fn builder() -> FleetViewBuilder {
        FleetViewBuilder::default()
    }

    /// The workspace of `project` named `name` that is not archived. The first in the order
    /// held: by owner machine id, then version, for [`FleetView::read`], and the order given
    /// for the builder. A run may have workspaces of one name on two machines, after a
    /// relaunch on another machine: the caller then reads [`FleetView::workspaces_named`].
    pub fn workspace_named(&self, project: ProjectId, name: &str) -> Option<&Record<Workspace>> {
        self.workspaces_named(project, name).into_iter().next()
    }

    /// Every workspace of `project` named `name` that is not archived, in the order held.
    pub fn workspaces_named(&self, project: ProjectId, name: &str) -> Vec<&Record<Workspace>> {
        self.workspaces
            .iter()
            .filter(|r| !r.deleted && r.body.project == project && r.body.name == name)
            .collect()
    }

    /// The session of `workspace` to read: a live one before an ended one, one with a turn
    /// (a factory session) before one without, and among equals the highest version. An
    /// ended session is still answered when it is the only one, so a run sees it ended.
    pub fn session_of(&self, workspace: WorkspaceId) -> Option<&Record<Session>> {
        self.sessions
            .iter()
            .filter(|r| !r.deleted && r.body.workspace == workspace)
            .max_by_key(|r| {
                let ended = matches!(r.body.state, SessionState::Finished | SessionState::Failed);
                (!ended, self.turns.contains_key(&r.body.id), r.version)
            })
    }

    /// The state of `session`, as its owner's record shows it: [`SessionState::Unreachable`]
    /// while its owner is offline. `None` for one unknown or deleted.
    pub fn session_state(&self, session: SessionId) -> Option<SessionState> {
        self.sessions
            .iter()
            .find(|r| !r.deleted && r.body.id == session)
            .map(|r| r.body.state)
    }

    /// The state and exit code of `terminal`. `None` for one unknown. A terminal whose owner
    /// is offline is unreachable, and a deleted one has exited, as P1 treats it.
    pub fn terminal(&self, terminal: TerminalId) -> Option<(TerminalState, Option<i32>)> {
        let record = self.terminals.get(&terminal)?;
        let state = if record.deleted && record.body.state != TerminalState::Unreachable {
            TerminalState::Exited
        } else {
            record.body.state
        };
        Some((state, record.body.exit_code))
    }

    /// The newest turn of `session`, when it is a factory session.
    pub fn session_turn(&self, session: SessionId) -> Option<&SessionTurn> {
        self.turns.get(&session)
    }

    /// Whether `machine` has been heard from.
    pub fn online(&self, machine: MachineId) -> bool {
        self.online.contains(&machine)
    }

    /// How long `machine` has been unreachable: zero while it is online, and zero when no span
    /// was given.
    pub fn unreachable_for(&self, machine: MachineId) -> Duration {
        if self.online.contains(&machine) {
            return Duration::ZERO;
        }
        self.unreachable.get(&machine).copied().unwrap_or_default()
    }

    /// Every machine sample held.
    pub fn samples(&self) -> &[Record<MachineSample>] {
        &self.samples
    }

    /// The logins `machine` holds tokens for, when it has said.
    pub fn held(&self, machine: MachineId) -> Option<&MachineHeld> {
        self.held.get(&machine)
    }
}

/// Builds a [`FleetView`] from records given directly.
#[derive(Debug, Default)]
pub struct FleetViewBuilder {
    view: FleetView,
}

impl FleetViewBuilder {
    /// Adds a workspace record, archived or not.
    pub fn workspace(mut self, record: Record<Workspace>) -> Self {
        self.view.workspaces.push(record);
        self
    }

    /// Adds a session record, ended or not.
    pub fn session(mut self, record: Record<Session>) -> Self {
        self.view.sessions.push(record);
        self
    }

    /// Adds a terminal record.
    pub fn terminal(mut self, record: Record<Terminal>) -> Self {
        self.view.terminals.insert(record.body.id, record);
        self
    }

    /// Adds a factory session's newest turn.
    pub fn turn(mut self, turn: SessionTurn) -> Self {
        self.view.turns.insert(turn.session, turn);
        self
    }

    /// Marks `machine` online.
    pub fn online(mut self, machine: MachineId) -> Self {
        self.view.online.insert(machine);
        self
    }

    /// Says `machine` has been unreachable for `span`.
    pub fn unreachable(mut self, machine: MachineId, span: Duration) -> Self {
        self.view.unreachable.insert(machine, span);
        self
    }

    /// Adds a machine sample.
    pub fn sample(mut self, record: Record<MachineSample>) -> Self {
        self.view.samples.push(record);
        self
    }

    /// Says what `machine` holds.
    pub fn held(mut self, machine: MachineId, held: MachineHeld) -> Self {
        self.view.held.insert(machine, held);
        self
    }

    /// The view, with every session and terminal of an offline owner shown as unreachable.
    pub fn build(self) -> FleetView {
        let mut view = self.view;
        let online = view.online.clone();
        view.sessions = std::mem::take(&mut view.sessions)
            .into_iter()
            .map(|r| match as_seen(AnyRecord::Session(r), &online) {
                AnyRecord::Session(r) => r,
                _ => unreachable!("as_seen keeps the kind of a record"),
            })
            .collect();
        view.terminals = std::mem::take(&mut view.terminals)
            .into_iter()
            .map(|(id, r)| match as_seen(AnyRecord::Terminal(r), &online) {
                AnyRecord::Terminal(r) => (id, r),
                _ => unreachable!("as_seen keeps the kind of a record"),
            })
            .collect();
        view
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
/// Each method answers from its own queue, one answer per call in order; a method whose queue
/// is empty gives its default.
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
    /// The workspaces the default create made, by machine, tag and name.
    made: Vec<(MachineId, FactoryTag, String, WorkspaceId)>,
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
        self.lock().calls.clone()
    }

    /// Queues an answer for [`Worker::create`].
    pub fn on_create(&self, answer: Result<WorkspaceId, WorkerError>) -> &Self {
        self.lock().create.push_back(answer);
        self
    }

    /// Queues an answer for [`Worker::start_session`].
    pub fn on_start_session(&self, answer: Result<SessionId, WorkerError>) -> &Self {
        self.lock().start_session.push_back(answer);
        self
    }

    /// Queues an answer for [`Worker::send`].
    pub fn on_send(&self, answer: Result<u64, SendRefused>) -> &Self {
        self.lock().send.push_back(answer);
        self
    }

    /// Queues an answer for [`Worker::turn`].
    pub fn on_turn(&self, answer: Result<TurnText, WorkerError>) -> &Self {
        self.lock().turn.push_back(answer);
        self
    }

    /// Queues an answer for [`Worker::tail`].
    pub fn on_tail(&self, answer: Result<String, WorkerError>) -> &Self {
        self.lock().tail.push_back(answer);
        self
    }

    /// Queues an answer for [`Worker::end_sessions`].
    pub fn on_end_sessions(&self, answer: Result<(), WorkerError>) -> &Self {
        self.lock().end_sessions.push_back(answer);
        self
    }

    /// Queues an answer for [`Worker::archive`].
    pub fn on_archive(&self, answer: Result<(), WorkerError>) -> &Self {
        self.lock().archive.push_back(answer);
        self
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
        let mut state = self.lock();
        state.calls.push(Call::Create {
            machine,
            request: request.clone(),
        });
        if let Some(answer) = state.create.pop_front() {
            return answer;
        }
        let known = state.made.iter().find(|(on, tag, name, _)| {
            *on == machine && *tag == request.tag && *name == request.name
        });
        if let Some((_, _, _, made)) = known {
            return Ok(*made);
        }
        let made = WorkspaceId::random();
        state.made.push((machine, request.tag, request.name, made));
        Ok(made)
    }

    fn start_session(
        &self,
        workspace: WorkspaceId,
        start: FactoryStart,
        first: Message,
    ) -> Result<SessionId, WorkerError> {
        let mut state = self.lock();
        state.calls.push(Call::StartSession {
            workspace,
            project: start.project,
            login: start.login,
            model: start.model,
            effort: start.effort,
            first,
        });
        let answer = state
            .start_session
            .pop_front()
            .unwrap_or_else(|| Ok(SessionId::random()));
        if let Ok(session) = &answer {
            state.delivered.insert(*session, 1);
        }
        answer
    }

    fn send(
        &self,
        session: SessionId,
        message: Message,
        only_if_prompts: Option<u64>,
    ) -> Result<u64, SendRefused> {
        let mut state = self.lock();
        state.calls.push(Call::Send {
            session,
            message,
            only_if_prompts,
        });
        let before = state.delivered.get(&session).copied();
        let answer = state
            .send
            .pop_front()
            .unwrap_or(match (before, only_if_prompts) {
                (None, _) => Err(SendRefused::NoSuchSession),
                (Some(before), Some(k)) if before > k => {
                    Err(SendRefused::CountRose { now: before })
                }
                (Some(before), _) => Ok(before),
            });
        if answer.is_ok() {
            *state.delivered.entry(session).or_insert(0) += 1;
        }
        answer
    }

    fn turn(&self, session: SessionId, n: u64) -> Result<TurnText, WorkerError> {
        let mut state = self.lock();
        state.calls.push(Call::Turn { session, n });
        state.turn.pop_front().unwrap_or_else(|| {
            Ok(TurnText {
                message: String::new(),
                question: None,
            })
        })
    }

    fn tail(&self, terminal: TerminalId) -> Result<String, WorkerError> {
        let mut state = self.lock();
        state.calls.push(Call::Tail { terminal });
        state.tail.pop_front().unwrap_or_else(|| Ok(String::new()))
    }

    fn end_sessions(&self, workspace: WorkspaceId) -> Result<(), WorkerError> {
        let mut state = self.lock();
        state.calls.push(Call::EndSessions { workspace });
        state.end_sessions.pop_front().unwrap_or(Ok(()))
    }

    fn archive(&self, workspace: WorkspaceId) -> Result<(), WorkerError> {
        let mut state = self.lock();
        state.calls.push(Call::Archive { workspace });
        let answer = state.archive.pop_front().unwrap_or(Ok(()));
        if answer.is_ok() {
            state.made.retain(|(_, _, _, made)| *made != workspace);
        }
        answer
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
        let asked = SessionId(id(6));
        let term = TerminalId(id(7));
        fake.archive(ws).unwrap();
        fake.tail(term).unwrap();
        let req = request("a", "r1");
        fake.create(m, req.clone()).unwrap();
        fake.turn(asked, 3).unwrap();
        let sess = fake.start_session(ws, start(), msg("go")).unwrap();
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
                Call::Turn {
                    session: asked,
                    n: 3
                },
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
    fn scripted_answers_come_in_order_then_the_default() {
        let fake = FakeWorker::new();
        let ws = WorkspaceId(id(5));
        fake.on_end_sessions(Err(WorkerError::Failed("one".into())))
            .on_end_sessions(Err(WorkerError::Failed("two".into())));
        assert_eq!(
            fake.end_sessions(ws),
            Err(WorkerError::Failed("one".into()))
        );
        assert_eq!(
            fake.end_sessions(ws),
            Err(WorkerError::Failed("two".into()))
        );
        assert_eq!(fake.end_sessions(ws), Ok(()));
        let term = TerminalId(id(7));
        fake.on_tail(Ok("out".into()));
        assert_eq!(fake.tail(term), Ok("out".into()));
        assert_eq!(fake.tail(term), Ok(String::new()));
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
        let fresh = fake.create(m, request("a", "r1")).unwrap();
        assert_ne!(fresh, scripted, "the scripted answer answered one call");
        assert_eq!(fake.create(m, request("a", "r1")).unwrap(), fresh);
    }

    #[test]
    fn a_default_create_is_per_machine_and_forgets_an_archived_workspace() {
        let fake = FakeWorker::new();
        let (a, b) = (machine(1), machine(2));
        let first = fake.create(a, request("a", "r1")).unwrap();
        let elsewhere = fake.create(b, request("a", "r1")).unwrap();
        assert_ne!(elsewhere, first);
        assert_eq!(fake.create(a, request("a", "r1")).unwrap(), first);
        fake.archive(first).unwrap();
        let again = fake.create(a, request("a", "r1")).unwrap();
        assert_ne!(again, first);
        assert_eq!(fake.create(b, request("a", "r1")).unwrap(), elsewhere);
    }

    #[test]
    fn a_failed_archive_does_not_forget_the_workspace() {
        let fake = FakeWorker::new();
        let m = machine(1);
        let first = fake.create(m, request("a", "r1")).unwrap();
        fake.on_archive(Err(WorkerError::Failed("no".into())));
        assert!(fake.archive(first).is_err());
        assert_eq!(fake.create(m, request("a", "r1")).unwrap(), first);
    }

    #[test]
    fn the_default_send_refuses_a_session_it_never_started_and_a_count_that_rose() {
        let fake = FakeWorker::new();
        let ws = WorkspaceId(id(5));
        assert_eq!(
            fake.send(SessionId(id(6)), msg("a"), None),
            Err(SendRefused::NoSuchSession)
        );
        let sess = fake.start_session(ws, start(), msg("go")).unwrap();
        assert_eq!(
            fake.send(sess, msg("a"), Some(0)),
            Err(SendRefused::CountRose { now: 1 })
        );
        assert_eq!(fake.send(sess, msg("a"), Some(1)), Ok(1));
        assert_eq!(
            fake.send(sess, msg("a"), Some(1)),
            Err(SendRefused::CountRose { now: 2 })
        );
        assert_eq!(fake.send(sess, msg("a"), None), Ok(2));
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
        // The refusal delivered nothing, so a send after it still follows the start alone.
        assert_eq!(fake.send(sess, msg("a"), None), Ok(1));
        assert_eq!(fake.send(sess, msg("b"), None), Ok(2));
        assert_eq!(fake.calls().len(), 4);
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
        assert_eq!(
            view.workspace_named(project(1), "run").unwrap().body.id,
            made
        );
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
                &AnyRecord::Terminal(terminal_record(
                    m,
                    false,
                    1020,
                    TerminalState::Running,
                    None,
                )),
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
                &AnyRecord::Session(session_record(
                    gone,
                    1,
                    false,
                    21,
                    11,
                    SessionState::Working,
                )),
                gone,
            )
            .unwrap();
        store
            .apply(
                &AnyRecord::Terminal(terminal_record(
                    gone,
                    false,
                    30,
                    TerminalState::Running,
                    None,
                )),
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
        assert_eq!(
            view.session_state(SessionId(id(20))),
            Some(SessionState::Idle)
        );
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
    fn session_of_ranks_a_live_session_above_an_ended_one_with_a_turn() {
        let m = machine(1);
        let view = FleetView::builder()
            .online(m)
            .session(session_record(m, 5, false, 20, 10, SessionState::Finished))
            .session(session_record(m, 3, false, 21, 10, SessionState::Idle))
            .turn(turn_of(20, 10))
            .build();
        assert_eq!(
            view.session_of(WorkspaceId(id(10))).unwrap().body.id,
            SessionId(id(21))
        );
        let failed = FleetView::builder()
            .online(m)
            .session(session_record(m, 5, false, 20, 10, SessionState::Failed))
            .session(session_record(m, 3, false, 21, 10, SessionState::Working))
            .turn(turn_of(20, 10))
            .build();
        assert_eq!(
            failed.session_of(WorkspaceId(id(10))).unwrap().body.id,
            SessionId(id(21))
        );
        let only_ended = FleetView::builder()
            .online(m)
            .session(session_record(m, 5, false, 20, 10, SessionState::Finished))
            .build();
        assert_eq!(
            only_ended.session_of(WorkspaceId(id(10))).unwrap().body.id,
            SessionId(id(20))
        );
    }

    #[test]
    fn workspaces_named_gives_every_live_workspace_of_the_name() {
        let (a, b) = (machine(1), machine(2));
        let view = FleetView::builder()
            .workspace(workspace_record(a, 1, false, 10, 1, "run"))
            .workspace(workspace_record(b, 1, false, 11, 1, "run"))
            .workspace(workspace_record(b, 2, true, 12, 1, "run"))
            .workspace(workspace_record(b, 3, false, 13, 1, "other"))
            .workspace(workspace_record(b, 4, false, 14, 2, "run"))
            .build();
        let ids: Vec<WorkspaceId> = view
            .workspaces_named(project(1), "run")
            .iter()
            .map(|r| r.body.id)
            .collect();
        assert_eq!(ids, vec![WorkspaceId(id(10)), WorkspaceId(id(11))]);
        assert!(view.workspaces_named(project(3), "run").is_empty());
    }

    #[test]
    fn a_machine_that_holds_only_a_factory_record_is_read() {
        let (_dir, mut store) = open();
        let m = machine(1);
        let held = MachineHeld {
            logins: vec!["bot".into()],
            skill: false,
        };
        store
            .apply_factory(
                &FactoryRecord {
                    owner: m,
                    version: 1,
                    body: FactoryBody::Held(held.clone()),
                },
                m,
            )
            .unwrap();
        let view = FleetView::read(&store, BTreeSet::new(), BTreeMap::new()).unwrap();
        assert_eq!(view.held(m), Some(&held));
    }

    #[test]
    fn a_deleted_session_with_a_stored_turn_has_no_turn_in_the_view() {
        let (_dir, mut store) = open();
        let m = machine(1);
        store
            .apply(
                &AnyRecord::Session(session_record(m, 2, true, 20, 10, SessionState::Idle)),
                m,
            )
            .unwrap();
        store
            .apply_factory(
                &FactoryRecord {
                    owner: m,
                    version: 1,
                    body: FactoryBody::Turn(turn_of(20, 10)),
                },
                m,
            )
            .unwrap();
        let view = FleetView::read(&store, BTreeSet::from([m]), BTreeMap::new()).unwrap();
        assert_eq!(view.session_turn(SessionId(id(20))), None);
    }

    #[test]
    fn online_added_after_the_records_still_applies() {
        let (a, b) = (machine(1), machine(2));
        let view = FleetView::builder()
            .session(session_record(a, 1, false, 20, 10, SessionState::Idle))
            .terminal(terminal_record(a, false, 30, TerminalState::Running, None))
            .session(session_record(b, 1, false, 21, 10, SessionState::Idle))
            .online(a)
            .build();
        assert_eq!(
            view.session_state(SessionId(id(20))),
            Some(SessionState::Idle)
        );
        assert_eq!(
            view.session_state(SessionId(id(21))),
            Some(SessionState::Unreachable)
        );
        assert_eq!(
            view.terminal(TerminalId(id(30))),
            Some((TerminalState::Running, None))
        );
    }

    #[test]
    fn a_deleted_terminal_reads_as_exited_with_its_code() {
        let m = machine(1);
        let view = FleetView::builder()
            .online(m)
            .terminal(terminal_record(
                m,
                true,
                30,
                TerminalState::Running,
                Some(3),
            ))
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
