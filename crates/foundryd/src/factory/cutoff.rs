//! A machine that is cut off ends its factory sessions.

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use foundry_proto::factory::NotHolding;
    use foundry_proto::ids::{MachineId, ProjectId, SessionId, WorkspaceId};
    use foundry_proto::state::SessionState;
    use tempfile::TempDir;

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
                self.me,
                self.store.clone(),
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

        async fn until_ended(&self, id: SessionId) {
            until("the session to end", || self.ended(id)).await;
        }

        /// That `id` still runs a while after everything the cut-off did.
        async fn assert_runs(&self, id: SessionId) {
            tokio::time::sleep(Duration::from_millis(500)).await;
            assert!(!self.ended(id), "{id} ended: {:?}", self.state(id));
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
        f.freshness
            .received(&not_holding(project, NotHolding::NoToken { login: LOGIN.into() }));
        assert_eq!(cutoff.tick().await.len(), 1);
        f.until_ended(session).await;
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
}
