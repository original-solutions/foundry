//! A machine that is cut off ends its factory sessions (G10).
//!
//! Every [`TICK`] this machine looks at the factory sessions it runs, by project, and ends
//! them through [`Sessions::end_factory`] when, for [`CUT_OFF_AFTER`], it has neither held the
//! project's lease nor heard from its holder: [`Freshness::since_holding`] says how long that
//! is, and only the lease pushed here or a holding status at the highest generation seen
//! counts there, so a not-holding status, from this machine's own coordinator or any other,
//! saves nothing. [`Freshness::held`] is called by the pass's lease task (#157) on each
//! successful push, on the daemon's one `Freshness`, the one given here; until that lands no
//! factory session starts, so there is nothing to end. A project's timer starts when this
//! machine first finds one of its factory sessions running, so a session started long after
//! the last word from a holder still gets its ten minutes. It starts again once none runs.
//!
//! Every age and wait is read on the sleep-counting clock of [`crate::clock`]: a Mac that
//! slept past the ten minutes ends its sessions on the first look after it wakes.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use foundry_proto::ids::{MachineId, ProjectId, WorkspaceId};

use crate::clock::Clock;
use crate::factory::status::Freshness;
use crate::sessions::Sessions;
use crate::store::{Store, StoreError};

/// How often the cut-off looks.
pub const TICK: Duration = Duration::from_secs(30);

/// How long a machine may go without holding a project's lease or hearing from its holder
/// before it ends its factory sessions of the project.
pub const CUT_OFF_AFTER: Duration = Duration::from_secs(10 * 60);

/// How often the clock is read while waiting. It only paces the reads: the wait is measured on
/// the clock.
const POLL: Duration = Duration::from_millis(100);

/// Ends this machine's factory sessions of each project it is cut off from, for ever.
pub async fn run(
    store: Arc<Mutex<Store>>,
    me: MachineId,
    sessions: Arc<Sessions>,
    freshness: Arc<Freshness>,
    clock: Arc<dyn Clock>,
) {
    Cutoff::new(store, me, sessions, freshness, clock)
        .run()
        .await;
}

/// The cut-off of one machine: what it reads, and when it first found each project's factory
/// sessions running.
pub struct Cutoff {
    me: MachineId,
    store: Arc<Mutex<Store>>,
    sessions: Arc<Sessions>,
    freshness: Arc<Freshness>,
    clock: Arc<dyn Clock>,
    /// Per project with factory sessions running here, the clock's reading at the look that
    /// first found one.
    started: BTreeMap<ProjectId, Duration>,
    /// What the last failed read of the store said, while reads keep failing.
    failing: Option<String>,
}

impl Cutoff {
    /// The cut-off of machine `me`, whose session records are in `store`, read on `clock`,
    /// which must be the clock `freshness` reads.
    pub fn new(
        store: Arc<Mutex<Store>>,
        me: MachineId,
        sessions: Arc<Sessions>,
        freshness: Arc<Freshness>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            me,
            store,
            sessions,
            freshness,
            clock,
            started: BTreeMap::new(),
            failing: None,
        }
    }

    /// Looks at once, then every [`TICK`] on the clock, for ever.
    pub async fn run(mut self) {
        loop {
            let looked = self.clock.now();
            self.tick().await;
            while self.clock.now().saturating_sub(looked) < TICK {
                tokio::time::sleep(POLL).await;
            }
        }
    }

    /// One look: ends the factory sessions of each project this machine is cut off from. Gives
    /// the workspaces whose factory sessions it ended, sorted. A workspace whose end failed is
    /// left out, and is tried again at the next look.
    pub async fn tick(&mut self) -> Vec<WorkspaceId> {
        let running = match self.running().await {
            Ok(running) => {
                if self.failing.take().is_some() {
                    tracing::info!("the cut-off can read this machine's sessions again");
                }
                running
            }
            Err(error) => {
                if self.failing.as_ref() != Some(&error) {
                    tracing::warn!(%error, "the cut-off cannot read this machine's sessions");
                    self.failing = Some(error);
                }
                return Vec::new();
            }
        };
        let now = self.clock.now();
        self.started
            .retain(|project, _| running.contains_key(project));
        let mut ended = Vec::new();
        for (project, workspaces) in running {
            let started = *self.started.entry(project).or_insert(now);
            let quiet = self
                .freshness
                .since_holding(project)
                .min(now.saturating_sub(started));
            if quiet < CUT_OFF_AFTER {
                continue;
            }
            for workspace in workspaces {
                match self.sessions.end_factory(workspace).await {
                    Ok(()) => {
                        tracing::warn!(
                            %project,
                            %workspace,
                            minutes = quiet.as_secs() / 60,
                            "cut off from the lease holder: factory sessions ended"
                        );
                        ended.push(workspace);
                    }
                    Err(error) => tracing::warn!(
                        %project,
                        %workspace,
                        %error,
                        "cut off from the lease holder, but the factory sessions did not end"
                    ),
                }
            }
        }
        ended.sort();
        ended
    }

    /// The workspaces of the factory sessions that run here, by project.
    async fn running(&self) -> Result<BTreeMap<ProjectId, BTreeSet<WorkspaceId>>, String> {
        let (store, me) = (self.store.clone(), self.me);
        let records = tokio::task::spawn_blocking(move || {
            store
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .sessions_of(me)
        })
        .await
        // A JoinError's text carries a task id that differs each time: say a fixed one.
        .map_err(|_| "the read of the store did not finish".to_owned())?
        .map_err(|error: StoreError| error.to_string())?;
        let mut live: BTreeMap<ProjectId, BTreeSet<_>> = BTreeMap::new();
        let mut running: BTreeMap<ProjectId, BTreeSet<WorkspaceId>> = BTreeMap::new();
        for record in records {
            let session = record.body;
            let Some(project) = self.sessions.is_factory(session.id) else {
                continue;
            };
            let live = live.entry(project).or_insert_with(|| {
                self.sessions
                    .factory_sessions(project)
                    .into_iter()
                    .collect()
            });
            if live.contains(&session.id) {
                running
                    .entry(project)
                    .or_default()
                    .insert(session.workspace);
            }
        }
        Ok(running)
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use foundry_holder::protocol::Reply;
    use foundry_proto::factory::NotHolding;
    use foundry_proto::ids::{MachineId, ProjectId, SessionId, WorkspaceId};
    use foundry_proto::state::SessionState;
    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::{CUT_OFF_AFTER, Cutoff, TICK};
    use crate::clock::{Clock, FakeClock};
    use crate::factory::messages::Message;
    use crate::factory::status::{Freshness, PassResults, build, not_holding};
    use crate::secrets::{MemorySecrets, Secrets};
    use crate::sessions::factory::FactoryStart;
    use crate::sessions::{Config, Gateway, Sessions};
    use crate::store::Store;
    use crate::terminals::Terminals;

    const WAIT: Duration = Duration::from_secs(20);
    const MINUTE: Duration = Duration::from_secs(60);
    const MODEL: &str = "test-model";
    const LOGIN: &str = "factory-bot";
    const TOKEN: &str = "ghp_madeUpCutoffToken1234567890abcdEFGHij";

    /// Stands between the daemon and the fake agent, whose own command line refuses the two
    /// factory flags: it drops them and runs the agent with the rest.
    const SHIM: &str = r#"agent=$1; shift
skip=0
for a do
  shift
  if [ $skip = 1 ]; then skip=0; continue; fi
  case $a in
    --dangerously-skip-permissions) continue ;;
    --effort) skip=1; continue ;;
  esac
  set -- "$@" "$a"
done
exec "$agent" "$@""#;

    fn built(name: &str) -> PathBuf {
        let exe = std::env::current_exe().unwrap();
        let path = exe.parent().and_then(Path::parent).unwrap().join(name);
        assert!(
            path.exists(),
            "{} is not built: run `cargo test --workspace`",
            path.display()
        );
        path
    }

    /// One machine: real sessions over a real holder and the fake agent, which waits for
    /// input until its terminal is closed, the fake clock and a freshness read on it.
    struct Fx {
        data: TempDir,
        worktree: TempDir,
        _home: TempDir,
        _git_home: TempDir,
        me: MachineId,
        store: Arc<Mutex<Store>>,
        clock: Arc<FakeClock>,
        freshness: Arc<Freshness>,
        sessions: Arc<Sessions>,
        server: tokio::task::JoinHandle<()>,
    }

    impl Fx {
        async fn new() -> Fx {
            let data = tempfile::Builder::new()
                .prefix("co")
                .tempdir_in("/tmp")
                .unwrap();
            let script = data.path().join("script.yaml");
            std::fs::write(&script, "- wait_input\n").unwrap();
            let me = MachineId::random();
            let store = Arc::new(Mutex::new(
                Store::open(data.path().join("foundry.db")).unwrap(),
            ));
            let terminals = Arc::new(Terminals::new(
                me,
                store.clone(),
                data.path(),
                built("foundry-holder"),
            ));
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let port = listener.local_addr().unwrap().port();
            let home = tempfile::tempdir().unwrap();
            let config = Config {
                claude: [
                    "sh",
                    "-c",
                    SHIM,
                    "sh",
                    &built("foundry-fake-agent").to_string_lossy(),
                    "--script",
                    &script.to_string_lossy(),
                ]
                .map(str::to_owned)
                .to_vec(),
                claude_json: home.path().join(".claude.json"),
                hook_port: port,
                gateway: Gateway {
                    base_url: "http://127.0.0.1:9/gateway".into(),
                    key: format!("gw-key-{}", uuid::Uuid::new_v4()),
                },
            };
            let secrets = Arc::new(MemorySecrets::default());
            secrets.set(&format!("github:{LOGIN}"), TOKEN).unwrap();
            let git_home = tempfile::tempdir().unwrap();
            let sessions = Arc::new(
                Sessions::new(me, store.clone(), terminals, data.path(), config)
                    .with_git_home(git_home.path().to_path_buf())
                    .with_secrets(secrets),
            );
            let router = sessions.router();
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            let server = tokio::spawn(async move {
                axum::serve(
                    listener,
                    router.into_make_service_with_connect_info::<SocketAddr>(),
                )
                .await
                .unwrap();
            });
            let clock = Arc::new(FakeClock::new());
            let freshness = Arc::new(Freshness::new(clock.clone() as Arc<dyn Clock>));
            Fx {
                data,
                worktree: tempfile::tempdir().unwrap(),
                _home: home,
                _git_home: git_home,
                me,
                store,
                clock,
                freshness,
                sessions,
                server,
            }
        }

        fn cutoff(&self) -> Cutoff {
            Cutoff::new(
                self.store.clone(),
                self.me,
                self.sessions.clone(),
                self.freshness.clone(),
                self.clock.clone(),
            )
        }

        async fn start_factory(&self, workspace: WorkspaceId, project: ProjectId) -> SessionId {
            self.sessions
                .start_factory(
                    workspace,
                    self.worktree.path(),
                    FactoryStart {
                        project,
                        login: LOGIN.into(),
                        model: MODEL.into(),
                        effort: "xhigh".into(),
                    },
                    Message {
                        line: "/factory-build acme/widgets#7".into(),
                        body: None,
                    },
                )
                .await
                .unwrap()
        }

        async fn start_p1(&self, workspace: WorkspaceId) -> SessionId {
            self.sessions
                .start(workspace, self.worktree.path(), MODEL)
                .await
                .unwrap()
        }

        fn state(&self, id: SessionId) -> Option<SessionState> {
            let store = self.store.lock().unwrap();
            store.session(id).unwrap().map(|record| record.body.state)
        }

        fn ended(&self, id: SessionId) -> bool {
            matches!(
                self.state(id),
                Some(SessionState::Finished | SessionState::Failed)
            )
        }

        /// Waits until `id`'s record says it ended and it is no longer among the sessions that
        /// run: the record is written before the session leaves them, and a look in between
        /// still finds it running.
        async fn until_ended(&self, id: SessionId) {
            until("the session to end", || {
                self.ended(id)
                    && self.sessions.is_factory(id).is_none_or(|project| {
                        !self.sessions.factory_sessions(project).contains(&id)
                    })
            })
            .await;
        }

        /// That `id` still runs a while after everything the cut-off did.
        async fn assert_runs(&self, id: SessionId) {
            tokio::time::sleep(Duration::from_millis(500)).await;
            assert!(!self.ended(id), "{id} ended: {:?}", self.state(id));
        }

        /// The socket of the holder of session `id`'s terminal.
        fn socket(&self, id: SessionId) -> PathBuf {
            let terminal = self
                .store
                .lock()
                .unwrap()
                .session(id)
                .unwrap()
                .unwrap()
                .body
                .terminal;
            self.data
                .path()
                .join("terminals")
                .join(format!("{terminal}.sock"))
        }

        /// A second connection to the store's database, to break and mend it under the store.
        fn database(&self) -> rusqlite::Connection {
            rusqlite::Connection::open(self.data.path().join("foundry.db")).unwrap()
        }

        /// A status from the holder of `project`'s lease, at generation 1.
        fn holder_says(&self, project: ProjectId, pass: u64) {
            self.freshness.received(&build(PassResults {
                generation: 1,
                pass,
                ..PassResults::new(project)
            }));
        }
    }

    impl Drop for Fx {
        fn drop(&mut self) {
            self.server.abort();
        }
    }

    async fn until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + WAIT;
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Serves `socket` as a holder that refuses every request, so a close of its terminal fails.
    fn refusing_holder(socket: &Path) -> tokio::task::JoinHandle<()> {
        let listener = tokio::net::UnixListener::bind(socket).unwrap();
        tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    let refused = Reply::Refused("not now".into()).encode();
                    stream.write_all(&refused).await.unwrap();
                    // Held open until the daemon reads the refusal and lets it go.
                    let _ = stream.read(&mut [0; 64]).await;
                });
            }
        })
    }

    #[test]
    fn it_looks_every_30_seconds_and_cuts_off_after_10_minutes() {
        assert_eq!(TICK, Duration::from_secs(30));
        assert_eq!(CUT_OFF_AFTER, Duration::from_secs(600));
    }

    #[tokio::test]
    async fn a_machine_with_no_holding_status_for_10_minutes_ends_its_sessions_and_one_at_9_minutes_does_not()
     {
        let f = Fx::new().await;
        let project = ProjectId::random();
        let workspace = WorkspaceId::random();
        let session = f.start_factory(workspace, project).await;
        let mut cutoff = f.cutoff();
        f.holder_says(project, 1);
        assert_eq!(cutoff.tick().await, []);

        f.clock.advance(9 * MINUTE);
        assert_eq!(cutoff.tick().await, []);
        f.assert_runs(session).await;

        f.clock.advance(MINUTE);
        assert_eq!(cutoff.tick().await, [workspace]);
        f.until_ended(session).await;
    }

    #[tokio::test]
    async fn a_status_from_the_holder_puts_the_cut_off_back_by_10_minutes() {
        let f = Fx::new().await;
        let project = ProjectId::random();
        let session = f.start_factory(WorkspaceId::random(), project).await;
        let mut cutoff = f.cutoff();
        assert_eq!(cutoff.tick().await, []);

        f.clock.advance(9 * MINUTE);
        f.holder_says(project, 1);
        f.clock.advance(9 * MINUTE);
        assert_eq!(cutoff.tick().await, []);
        f.assert_runs(session).await;

        f.clock.advance(MINUTE);
        assert_eq!(cutoff.tick().await.len(), 1);
        f.until_ended(session).await;
    }

    #[tokio::test]
    async fn a_machine_that_holds_the_lease_ends_nothing_with_no_peer_at_all() {
        let f = Fx::new().await;
        let project = ProjectId::random();
        let session = f.start_factory(WorkspaceId::random(), project).await;
        let mut cutoff = f.cutoff();
        // The lease is renewed every minute, and the cut-off looks every 30 seconds, for an
        // hour.
        for _ in 0..60 {
            f.freshness.held(project);
            assert_eq!(cutoff.tick().await, []);
            f.clock.advance(TICK);
            assert_eq!(cutoff.tick().await, []);
            f.clock.advance(TICK);
        }
        f.assert_runs(session).await;
    }

    #[tokio::test]
    async fn a_not_holding_status_from_its_own_coordinator_saves_nothing() {
        let f = Fx::new().await;
        let project = ProjectId::random();
        let session = f.start_factory(WorkspaceId::random(), project).await;
        let mut cutoff = f.cutoff();
        let coordinator = MachineId::random();
        for minute in 0..10 {
            let mut status = not_holding(
                project,
                NotHolding::HeldBy {
                    machine: coordinator,
                    generation: 1,
                },
            );
            status.pass = minute;
            f.freshness.received(&status);
            assert_eq!(cutoff.tick().await, [], "at minute {minute}");
            f.clock.advance(MINUTE);
        }
        f.freshness.received(&not_holding(
            project,
            NotHolding::NoToken {
                login: LOGIN.into(),
            },
        ));
        assert_eq!(cutoff.tick().await.len(), 1);
        f.until_ended(session).await;
    }

    #[tokio::test]
    async fn run_looks_again_only_30_seconds_after_its_last_look() {
        let f = Fx::new().await;
        let (early, late) = (ProjectId::random(), ProjectId::random());
        let early_session = f.start_factory(WorkspaceId::random(), early).await;
        let mut cutoff = f.cutoff();
        assert_eq!(cutoff.tick().await, []);
        f.clock.advance(Duration::from_secs(20));
        let late_session = f.start_factory(WorkspaceId::random(), late).await;
        f.holder_says(late, 1);
        assert_eq!(cutoff.tick().await, []);
        let running = tokio::spawn(cutoff.run());

        // At 10:00 a look is due and ends the early project, 9:40 into the late one's.
        f.clock.advance(10 * MINUTE - Duration::from_secs(20));
        f.until_ended(early_session).await;
        // At 10:25 the late project is 10:05 in, but the next look is not due until 10:30.
        f.clock.advance(Duration::from_secs(25));
        f.assert_runs(late_session).await;

        f.clock.advance(Duration::from_secs(5));
        until("the look at 10:30", || f.ended(late_session)).await;
        running.abort();
    }

    #[tokio::test]
    async fn a_clock_that_jumps_20_minutes_ends_the_sessions_on_the_next_tick() {
        let f = Fx::new().await;
        let project = ProjectId::random();
        let session = f.start_factory(WorkspaceId::random(), project).await;
        f.holder_says(project, 1);
        let mut cutoff = f.cutoff();
        assert_eq!(cutoff.tick().await, []);
        let running = tokio::spawn(cutoff.run());
        // The clock stands still, so the loop looks once at its start and not again.
        f.assert_runs(session).await;

        f.clock.advance(20 * MINUTE);
        until("the session to end on the next tick", || f.ended(session)).await;
        running.abort();
    }

    #[tokio::test]
    async fn a_project_s_timer_starts_when_its_first_factory_session_starts() {
        let f = Fx::new().await;
        let project = ProjectId::random();
        let mut cutoff = f.cutoff();
        // Nothing held or heard for 15 minutes, with no session of the project here.
        f.clock.advance(15 * MINUTE);
        assert_eq!(cutoff.tick().await, []);

        let session = f.start_factory(WorkspaceId::random(), project).await;
        assert_eq!(cutoff.tick().await, []);
        f.clock.advance(9 * MINUTE);
        assert_eq!(cutoff.tick().await, []);
        f.assert_runs(session).await;

        f.clock.advance(MINUTE);
        assert_eq!(cutoff.tick().await.len(), 1);
        f.until_ended(session).await;
    }

    #[tokio::test]
    async fn the_timer_starts_again_with_the_next_first_session() {
        let f = Fx::new().await;
        let project = ProjectId::random();
        let first = f.start_factory(WorkspaceId::random(), project).await;
        let mut cutoff = f.cutoff();
        assert_eq!(cutoff.tick().await, []);
        f.clock.advance(CUT_OFF_AFTER);
        assert_eq!(cutoff.tick().await.len(), 1);
        f.until_ended(first).await;
        assert_eq!(cutoff.tick().await, []);

        f.clock.advance(5 * MINUTE);
        let second = f.start_factory(WorkspaceId::random(), project).await;
        assert_eq!(cutoff.tick().await, []);
        f.assert_runs(second).await;
    }

    #[tokio::test]
    async fn only_the_project_that_is_cut_off_ends() {
        let f = Fx::new().await;
        let (cut, held) = (ProjectId::random(), ProjectId::random());
        let (cut_in, held_in) = (WorkspaceId::random(), WorkspaceId::random());
        let cut_session = f.start_factory(cut_in, cut).await;
        let held_session = f.start_factory(held_in, held).await;
        let mut cutoff = f.cutoff();
        assert_eq!(cutoff.tick().await, []);

        f.clock.advance(CUT_OFF_AFTER);
        f.freshness.held(held);
        assert_eq!(cutoff.tick().await, [cut_in]);
        f.until_ended(cut_session).await;
        f.assert_runs(held_session).await;
    }

    #[tokio::test]
    async fn each_workspace_of_a_project_that_is_cut_off_ends() {
        let f = Fx::new().await;
        let project = ProjectId::random();
        let mut workspaces = [WorkspaceId::random(), WorkspaceId::random()];
        workspaces.sort();
        let first = f.start_factory(workspaces[0], project).await;
        let second = f.start_factory(workspaces[1], project).await;
        let mut cutoff = f.cutoff();
        assert_eq!(cutoff.tick().await, []);

        f.clock.advance(CUT_OFF_AFTER);
        assert_eq!(cutoff.tick().await, workspaces);
        f.until_ended(first).await;
        f.until_ended(second).await;
    }

    #[tokio::test]
    async fn a_p1_session_is_never_ended() {
        let f = Fx::new().await;
        let project = ProjectId::random();
        let factory_in = WorkspaceId::random();
        let alone = f.start_p1(WorkspaceId::random()).await;
        let beside = f.start_p1(factory_in).await;
        let factory = f.start_factory(factory_in, project).await;
        let mut cutoff = f.cutoff();
        assert_eq!(cutoff.tick().await, []);

        f.clock.advance(30 * MINUTE);
        assert_eq!(cutoff.tick().await, [factory_in]);
        f.until_ended(factory).await;
        f.clock.advance(30 * MINUTE);
        assert_eq!(cutoff.tick().await, []);
        f.assert_runs(alone).await;
        f.assert_runs(beside).await;
    }

    #[tokio::test]
    async fn a_machine_with_only_p1_sessions_ends_nothing() {
        let f = Fx::new().await;
        let session = f.start_p1(WorkspaceId::random()).await;
        let mut cutoff = f.cutoff();
        assert_eq!(cutoff.tick().await, []);
        f.clock.advance(60 * MINUTE);
        assert_eq!(cutoff.tick().await, []);
        f.assert_runs(session).await;
    }

    #[tokio::test]
    async fn a_workspace_whose_end_fails_is_tried_again_at_the_next_look_and_the_others_end() {
        let f = Fx::new().await;
        let project = ProjectId::random();
        let mut workspaces = [WorkspaceId::random(), WorkspaceId::random()];
        workspaces.sort();
        let [refusing_in, other_in] = workspaces;
        let refusing = f.start_factory(refusing_in, project).await;
        let other = f.start_factory(other_in, project).await;
        let mut cutoff = f.cutoff();
        assert_eq!(cutoff.tick().await, []);

        // The first workspace's holder refuses to close, so its end fails before the other's.
        let socket = f.socket(refusing);
        let aside = socket.with_extension("aside");
        std::fs::rename(&socket, &aside).unwrap();
        let holder = refusing_holder(&socket);
        f.clock.advance(CUT_OFF_AFTER);
        assert_eq!(cutoff.tick().await, [other_in]);
        f.until_ended(other).await;
        f.assert_runs(refusing).await;

        holder.abort();
        std::fs::remove_file(&socket).unwrap();
        std::fs::rename(&aside, &socket).unwrap();
        assert_eq!(cutoff.tick().await, [refusing_in]);
        f.until_ended(refusing).await;
    }

    #[tokio::test]
    async fn a_look_that_cannot_read_the_sessions_ends_nothing_and_keeps_the_timers() {
        let f = Fx::new().await;
        let project = ProjectId::random();
        let workspace = WorkspaceId::random();
        let session = f.start_factory(workspace, project).await;
        let mut cutoff = f.cutoff();
        assert_eq!(cutoff.tick().await, []);

        f.clock.advance(9 * MINUTE);
        let database = f.database();
        database
            .execute_batch("ALTER TABLE records RENAME TO records_away")
            .unwrap();
        assert_eq!(cutoff.tick().await, []);
        database
            .execute_batch("ALTER TABLE records_away RENAME TO records")
            .unwrap();
        f.assert_runs(session).await;

        // The timer still counts from the first look, so the ten minutes end at 10:00.
        f.clock.advance(MINUTE);
        assert_eq!(cutoff.tick().await, [workspace]);
        f.until_ended(session).await;
    }
}
