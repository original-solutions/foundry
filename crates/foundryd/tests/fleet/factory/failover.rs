//! Fleet tests of coordinators and leases: a project's factory moving between machines.
//!
//! Each test starts the factory harness and then rigs every machine ([`Rig`]): the machine's
//! API, sessions and factory service start again as the harness starts them, with three more
//! things a daemon has and these tests need. Its GitHub calls go through a [`Trap`], which can
//! hold them, cut the machine off at one of them, or kill the machine at one, before or after
//! it lands, so a test stops a coordinator between two steps of a claim. It runs the cut-off of
//! its factory sessions, as `foundryd` does. And it serves the Factory view's routes and the
//! attention list beside its API, with the factory's leases shared with them, so a test reads
//! them and sends Stop from any machine.
//!
//! A killed machine is cut off from the fleet for good and its tasks are stopped. A GitHub call
//! it had under way when it died never lands: a call killed before it lands waits until the
//! machine is dead and then fails, and so does every call it makes after it.

use std::collections::BTreeSet;
use std::process::Command;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

use foundry_proto::factory::{FactoryKey, FactoryRecord, NotHolding, RunEvent, RunId};
use foundry_proto::ids::{MachineId, ProjectId, SessionId, WorkspaceId};
use foundry_proto::messages::{PROTOCOL, PeerMessage};
use foundry_proto::record::{Record, Session};
use foundryd::api::factory::{View, view_router};
use foundryd::api::{self, Api};
use foundryd::attention::{Attention, Item, Kind, Marks};
use foundryd::clock::Clock;
use foundryd::factory::fleet::FleetWorker;
use foundryd::factory::github::{
    Commenting, Hiding, OtherKind, STATUS_MARKER, Tracker, TrackerError,
};
use foundryd::factory::ledger::Hide;
use foundryd::factory::status::Freshness;
use foundryd::factory::worker::{CreateWorkspace, Worker};
use foundryd::factory::{Answering, Factory, Host, Leases, Leasing, Watched};
use foundryd::secrets::Secrets;
use foundryd::sessions::{self, Gateway, Sessions};
use foundryd::sync::Conn;
use foundryd::terminals::Terminals;
use foundryd::workspaces::{Checkout, FactoryTag, Setup, Workspaces};
use serde_json::Value;
use tokio::task::JoinHandle;

use super::{
    FakeTracker, FleetHost, Harness, Held, LOGIN, Machine, Options, REPO, SHIM, WAIT, built,
    config, isolated, live, write_held,
};
use crate::{lock, until};

// --- the rig ---

/// What a [`Trap`] does at the call a rule matches.
enum Then {
    /// The machine dies before the call lands.
    DieBefore,
    /// The call lands, and the machine dies before it hears the answer.
    DieAfter,
    /// This runs, and then the call goes on.
    Run(Box<dyn Fn() + Send + Sync>),
}

type Matcher = Box<dyn Fn(&[String]) -> bool + Send + Sync>;

#[derive(Default)]
struct Gate {
    /// Calls wait while it is held.
    held: bool,
    /// The machine has died at a call: every call from now on waits for it to be killed.
    sprung: bool,
    /// The machine is killed: every call fails at once.
    dead: bool,
}

/// What a test does to one machine's GitHub calls. Each rule fires once, at the first call it
/// matches.
#[derive(Default)]
struct Trap {
    rules: Mutex<Vec<(Matcher, Then)>>,
    gate: Mutex<Gate>,
    moved: Condvar,
    /// Every call that changes GitHub that reached it, as one line each.
    writes: Mutex<Vec<String>>,
}

impl Trap {
    /// At the first call `matches` accepts, `then`.
    fn on(&self, matches: impl Fn(&[String]) -> bool + Send + Sync + 'static, then: Then) {
        lock(&self.rules).push((Box::new(matches), then));
    }

    fn gate(&self) -> std::sync::MutexGuard<'_, Gate> {
        lock(&self.gate)
    }

    /// Holds every call until [`Trap::release`].
    fn hold(&self) {
        self.gate().held = true;
    }

    fn release(&self) {
        self.gate().held = false;
        self.moved.notify_all();
    }

    /// Whether the machine died at a call.
    fn sprung(&self) -> bool {
        self.gate().sprung
    }

    /// The machine is dead: a call that waits fails.
    fn kill(&self) {
        self.gate().dead = true;
        self.moved.notify_all();
    }

    fn writes(&self) -> Vec<String> {
        lock(&self.writes).clone()
    }

    fn dead() -> TrackerError {
        TrackerError::Other {
            kind: OtherKind::Failed,
            message: "the machine is dead".into(),
        }
    }

    /// Waits while calls are held, and while a machine that died is not yet killed. An error
    /// when the machine died.
    fn enter(&self) -> Result<(), TrackerError> {
        let mut gate = self.gate();
        while (gate.held || gate.sprung) && !gate.dead {
            gate = self
                .moved
                .wait(gate)
                .unwrap_or_else(PoisonError::into_inner);
        }
        if gate.dead || gate.sprung {
            return Err(Self::dead());
        }
        Ok(())
    }

    /// The machine dies here: waits until it is killed, and fails.
    fn die(&self) -> TrackerError {
        self.gate().sprung = true;
        self.moved.notify_all();
        let mut gate = self.gate();
        while !gate.dead {
            gate = self
                .moved
                .wait(gate)
                .unwrap_or_else(PoisonError::into_inner);
        }
        Self::dead()
    }
}

/// Whether `args` change GitHub.
fn is_write(args: &[String]) -> bool {
    let word = |at: usize| args.get(at).map(String::as_str);
    matches!(
        (word(0), word(1)),
        (
            Some("issue" | "pr"),
            Some("comment" | "edit" | "ready" | "close" | "merge")
        ) | (Some("label"), Some("create"))
    ) || (word(0) == Some("api") && args.iter().any(|a| a == "-X"))
}

/// The value `args` give after `flag`, the last one.
fn value_of<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.windows(2)
        .rev()
        .find(|pair| pair[0] == flag)
        .map(|pair| pair[1].as_str())
}

/// Whether `args` are `gh issue <verb> <issue> ...`.
fn issue_call(args: &[String], verb: &str, issue: u64) -> bool {
    args.len() > 2 && args[0] == "issue" && args[1] == verb && args[2] == issue.to_string()
}

/// The claim's label: `issue edit <issue>` adding `factory:triage`.
fn adds_triage(issue: u64) -> impl Fn(&[String]) -> bool + Send + Sync + 'static {
    move |args| {
        issue_call(args, "edit", issue)
            && args
                .windows(2)
                .any(|pair| pair[0] == "--add-label" && pair[1] == "factory:triage")
    }
}

/// The issue's status comment, posted the first time.
fn posts_status(issue: u64) -> impl Fn(&[String]) -> bool + Send + Sync + 'static {
    move |args| {
        issue_call(args, "comment", issue)
            && value_of(args, "--body").is_some_and(|b| b.trim_end().ends_with(STATUS_MARKER))
    }
}

/// The read of the issue's label history, which the pass makes before it claims.
fn reads_approval(issue: u64) -> impl Fn(&[String]) -> bool + Send + Sync + 'static {
    move |args| {
        args.first().is_some_and(|a| a == "api")
            && args.get(1).is_some_and(|a| a == "graphql")
            && args.iter().any(|a| a.contains("timelineItems"))
            && args
                .windows(2)
                .any(|pair| pair[0] == "-F" && pair[1] == format!("number={issue}"))
    }
}

/// GitHub as one machine reaches it: the fleet's fake, through the machine's trap.
struct Trapped {
    inner: Arc<FakeTracker>,
    trap: Arc<Trap>,
}

impl Tracker for Trapped {
    fn gh(&self, args: &[String]) -> Result<String, TrackerError> {
        self.trap.enter()?;
        let rule = {
            let mut rules = lock(&self.trap.rules);
            rules
                .iter()
                .position(|(matches, _)| matches(args))
                .map(|at| rules.remove(at).1)
        };
        let after = matches!(rule, Some(Then::DieAfter));
        match rule {
            Some(Then::DieBefore) => return Err(self.trap.die()),
            Some(Then::Run(run)) => run(),
            _ => {}
        }
        if is_write(args) {
            lock(&self.trap.writes).push(args.join(" "));
        }
        let answer = self.inner.gh(args);
        if after {
            return Err(self.trap.die());
        }
        answer
    }
}

/// The harness's host, with GitHub reached through the machine's trap.
struct Rigged {
    inner: FleetHost,
    trap: Arc<Trap>,
}

impl Host for Rigged {
    fn acting(&self) -> Answering<'_, Option<bool>> {
        self.inner.acting()
    }

    fn tracker(&self, login: &str) -> Option<Arc<dyn Commenting>> {
        self.inner.token(login)?;
        let github: Arc<dyn Tracker> = Arc::new(Trapped {
            inner: self.inner.github.clone(),
            trap: self.trap.clone(),
        });
        Some(Arc::new(Hiding::new(github, self.inner.hide())))
    }

    fn token_mark(&self, login: &str) -> Result<Option<u64>, String> {
        self.inner.token_mark(login)
    }

    fn lease(&self, watched: &Watched) -> Arc<dyn Leasing> {
        self.inner.lease(watched)
    }

    fn worker(&self) -> Arc<dyn Worker> {
        self.inner.worker()
    }

    fn online(&self) -> BTreeSet<MachineId> {
        self.inner.online()
    }

    fn hide(&self) -> Hide {
        self.inner.hide()
    }
}

/// What the machine marks as factory work, as the daemon's marks do.
struct RigMarks {
    sessions: Arc<Sessions>,
    workspaces: Arc<Workspaces>,
}

impl Marks for RigMarks {
    fn factory_session(&self, session: SessionId) -> Option<ProjectId> {
        self.sessions.is_factory(session)
    }

    fn factory_workspace(&self, workspace: WorkspaceId) -> Option<ProjectId> {
        self.workspaces.factory_project(workspace)
    }
}

/// What a test reaches of one rigged machine.
struct Part {
    trap: Arc<Trap>,
    /// Where its API, the Factory view's routes and the attention list are served.
    addr: std::net::SocketAddr,
    api: Api,
    attention: Arc<Attention>,
}

/// Which machines hold what.
struct RigOptions {
    /// The machines that hold the project login's token.
    tokens: Vec<usize>,
    /// The machines that have the skill, so may build and gate. Each must hold the token.
    skills: Vec<usize>,
    config: String,
}

impl Default for RigOptions {
    fn default() -> Self {
        Self {
            tokens: vec![0, 1, 2],
            skills: vec![1, 2],
            config: config(&[], &["test -f feature.txt"], 2),
        }
    }
}

/// The factory harness with every machine rigged: see the module's words.
struct Rig {
    h: Harness,
    parts: Vec<Part>,
    project: ProjectId,
    /// Each foreign claim any machine's status listed, while the test ran.
    foreign: Arc<Mutex<Vec<String>>>,
    watcher: JoinHandle<()>,
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.watcher.abort();
        // A machine that died at a call is let go, so no blocking thread waits for ever.
        for part in &self.parts {
            part.trap.kill();
        }
    }
}

impl Rig {
    async fn start(options: RigOptions) -> Rig {
        let mut h = Harness::start(Options {
            builders: options.skills.clone(),
            max_runs: 3,
            config: options.config,
        })
        .await;
        let project = h.fleet.daemons[0].settings().projects[0].id;
        let mut parts = Vec::new();
        for i in 0..h.fleet.daemons.len() {
            let token = options.tokens.contains(&i);
            let (machine, part) = rig_machine(&h, i, token).await;
            // The harness's own start of the machine stops as it is dropped.
            h._machines[i] = machine;
            write_held(&h.fleet.daemons[i], token, options.skills.contains(&i));
            parts.push(part);
        }
        let foreign: Arc<Mutex<Vec<String>>> = Arc::default();
        let stores: Vec<_> = h.fleet.daemons.iter().map(|d| d.store.clone()).collect();
        let seen = foreign.clone();
        let watcher = tokio::spawn(async move {
            loop {
                for store in &stores {
                    let statuses = lock(store).statuses(project).unwrap_or_default();
                    for (writer, status) in statuses {
                        for claim in status.foreign {
                            lock(&seen).push(format!("#{} by {writer}", claim.issue));
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
        Rig {
            h,
            parts,
            project,
            foreign,
            watcher,
        }
    }

    fn id(&self, i: usize) -> MachineId {
        self.h.fleet.daemons[i].id
    }

    fn trap(&self, i: usize) -> &Trap {
        &self.parts[i].trap
    }

    /// Kills machine `i`: it is cut off for good, its tasks stop, and a GitHub call it waits in
    /// fails.
    async fn kill(&mut self, i: usize) {
        eprintln!("fleet: killing machine {i}");
        self.h.fleet.cut(i);
        for server in &self.h._machines[i].servers {
            server.abort();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        self.parts[i].trap.kill();
    }

    /// A closure that cuts machine `i` off, and waits until its links are closed.
    fn cutter(&self, i: usize) -> Box<dyn Fn() + Send + Sync> {
        let (net, id) = (self.h.fleet.net.clone(), self.id(i));
        Box::new(move || {
            eprintln!("fleet: cutting {id} off");
            net.cut.send_modify(|cut| {
                cut.insert(id);
            });
            std::thread::sleep(Duration::from_millis(300));
        })
    }

    /// The holder and the generation `refs/factory/lease` names on `origin`.
    fn lease(&self) -> Option<(String, u64)> {
        let out = Command::new("git")
            .args(["log", "-1", "--format=%B", "refs/factory/lease"])
            .env("GIT_DIR", &self.h.origin)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        let field = |name: &str| {
            text.lines()
                .find_map(|line| line.strip_prefix(name))
                .map(|v| v.trim().to_owned())
        };
        Some((field("holder:")?, field("generation:")?.parse().ok()?))
    }

    /// Waits until machine `i` holds the lease, and answers the generation it holds it at.
    async fn lease_held_by(&self, i: usize) -> u64 {
        eprintln!("fleet: waiting for machine {i} to hold the lease");
        let me = self.id(i).to_string();
        until(WAIT, &format!("machine {i} holds the lease"), || {
            self.lease().is_some_and(|(holder, _)| holder == me)
        })
        .await;
        self.lease().unwrap().1
    }

    /// Waits until machine `i` holds the lease at `generation`.
    async fn holds_lease(&self, i: usize, generation: u64) {
        eprintln!("fleet: waiting for machine {i} to hold the lease at {generation}");
        let want = (self.id(i).to_string(), generation);
        until(
            WAIT,
            &format!("machine {i} holds the lease at generation {generation}"),
            || self.lease().as_ref() == Some(&want),
        )
        .await;
    }

    /// The events of `run` machine `on` holds, in order.
    fn events(&self, on: usize, run: &RunId) -> Vec<RunEvent> {
        lock(&self.h.fleet.daemons[on].store)
            .run_events(run)
            .unwrap()
    }

    /// The events of `run` machine `writer` wrote, as machine `on` holds them.
    fn events_by(&self, on: usize, run: &RunId, writer: usize) -> Vec<RunEvent> {
        lock(&self.h.fleet.daemons[on].store)
            .run_events_by(run, self.id(writer), None)
            .unwrap()
            .into_iter()
            .map(|(_, event)| event)
            .collect()
    }

    /// The runs and machines Foundry's claim comments on `issue` name, oldest first.
    fn claims(&self, issue: u64) -> Vec<(RunId, String)> {
        self.h
            .github
            .comments()
            .into_iter()
            .filter(|c| c.on == issue && c.author == LOGIN)
            .filter_map(|c| {
                let last = c.body.trim_end().lines().last()?.trim().to_owned();
                let named = last
                    .strip_prefix("<!-- foundry:claim ")?
                    .strip_suffix(" -->")?
                    .to_owned();
                let (run, machine) = named.split_once(' ')?;
                Some((run.parse().ok()?, machine.to_owned()))
            })
            .collect()
    }

    /// The status comments on `issue`.
    fn status_comments(&self, issue: u64) -> Vec<String> {
        self.h
            .github
            .comments_on(issue)
            .into_iter()
            .filter(|body| body.trim_end().ends_with(STATUS_MARKER))
            .collect()
    }

    /// Asserts that no machine's status listed a foreign claim while the test ran, and that no
    /// machine still running raises a foreign claim's attention item.
    async fn no_foreign_claim(&self, alive: &[usize]) {
        assert_eq!(
            *lock(&self.foreign),
            Vec::<String>::new(),
            "a claim of the factory's own was taken for a foreign claim"
        );
        for &i in alive {
            let items = self.items(i).await;
            assert!(
                items.iter().all(|item| item.kind != Kind::ForeignClaim),
                "machine {i} raises a foreign claim: {items:?}"
            );
        }
    }

    /// The attention items machine `i` lists now.
    async fn items(&self, i: usize) -> Vec<Item> {
        self.parts[i].attention.items().await.unwrap()
    }

    /// Asks machine `i` for the build workspace of `run` on machine `on`, as a coordinator's
    /// launch asks for it, and answers the workspace's id.
    async fn create_build(&self, i: usize, on: usize, run: &RunId) -> WorkspaceId {
        let request = CreateWorkspace {
            project: self.project,
            name: format!("fac/{}-{}", run.issue(), run.attempt()),
            checkout: Checkout::NewBranch {
                base: "main".into(),
                branch: Some(format!("fac/{}-{}", run.issue(), run.attempt())),
            },
            setup: Setup::Own,
            tag: FactoryTag {
                project: self.project,
                run: run.to_string(),
            },
        };
        let worker = FleetWorker::new(self.parts[i].api.clone(), tokio::runtime::Handle::current());
        let on = self.id(on);
        tokio::task::spawn_blocking(move || worker.create(on, request))
            .await
            .unwrap()
            .expect("the build workspace is made")
    }

    /// `GET path` on machine `i`, as JSON.
    async fn get(&self, i: usize, path: &str) -> Value {
        let url = format!("http://{}{path}", self.parts[i].addr);
        tokio::task::spawn_blocking(move || {
            let mut answer = http().get(&url).call().unwrap();
            let status = answer.status();
            let text = answer.body_mut().read_to_string().unwrap();
            assert_eq!(status, 200, "GET {url}: {text}");
            serde_json::from_str(&text).unwrap()
        })
        .await
        .unwrap()
    }

    /// `POST path` on machine `i` with no body: the status and the text answered.
    async fn post(&self, i: usize, path: &str) -> (u16, String) {
        let url = format!("http://{}{path}", self.parts[i].addr);
        tokio::task::spawn_blocking(move || {
            let mut answer = http().post(&url).send_empty().unwrap();
            let text = answer.body_mut().read_to_string().unwrap();
            (answer.status().as_u16(), text)
        })
        .await
        .unwrap()
    }

    /// The live sessions of the workspaces named `name`.
    fn live_sessions(&self, name: &str) -> Vec<Record<Session>> {
        self.h
            .workspaces_named(name)
            .iter()
            .flat_map(|w| self.h.sessions_in(w.body.id))
            .filter(live)
            .collect()
    }
}

fn http() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(60)))
        .build()
        .into()
}

/// Starts machine `i` of `h` again as the harness starts it, holding the project login's token
/// when `token`, rigged: see the module's words.
async fn rig_machine(h: &Harness, i: usize, token: bool) -> (Machine, Part) {
    let daemon = &h.fleet.daemons[i];
    let secrets = Held::new(token);
    let (me, store, peers) = (daemon.id, daemon.store.clone(), daemon.peers.clone());
    let clock: Arc<dyn Clock> = h.clock.clone();
    let data = tempfile::Builder::new()
        .prefix("fm")
        .tempdir_in("/tmp")
        .unwrap();
    let home = tempfile::tempdir().unwrap();
    let agent = built("foundry-fake-agent");
    let claude: Vec<String> = vec![
        "sh".into(),
        "-c".into(),
        SHIM.into(),
        "sh".into(),
        agent.display().to_string(),
        h.scripts.display().to_string(),
        h.marks.display().to_string(),
    ];
    let terminals = Arc::new(Terminals::new(
        me,
        store.clone(),
        data.path(),
        built("foundry-holder"),
    ));
    let held: Arc<dyn Secrets> = secrets.clone();
    let workspaces = Arc::new(
        Workspaces::new(me, store.clone(), data.path(), terminals.clone())
            .with_secrets(held.clone()),
    );
    let hooks = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let sessions = Arc::new(
        Sessions::new(
            me,
            store.clone(),
            terminals.clone(),
            data.path(),
            sessions::Config {
                claude,
                claude_json: home.path().join(".claude.json"),
                hook_port: hooks.local_addr().unwrap().port(),
                gateway: Gateway {
                    base_url: "http://127.0.0.1:9/gateway".into(),
                    key: "unused".into(),
                },
            },
        )
        .with_secrets(held.clone()),
    );
    let hook_router = sessions.router();
    let api = Api::new(
        store.clone(),
        peers.clone(),
        terminals,
        workspaces.clone(),
        sessions.clone(),
        Vec::new(),
    );
    let listener = api::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let addr = listener.local_addr().unwrap();
    peers.serve_calls(addr);
    let freshness = Arc::new(Freshness::new(clock.clone()));
    peers.hear_statuses(freshness.clone());
    let trap = Arc::new(Trap::default());
    let host = Arc::new(Rigged {
        inner: FleetHost {
            me,
            coordinator: daemon.coordinator(),
            api: api.clone(),
            secrets,
            github: h.github.clone(),
            clock: clock.clone(),
            data: data.path().to_path_buf(),
            runtime: tokio::runtime::Handle::current(),
            sent: h.sent.clone(),
        },
        trap: trap.clone(),
    });
    let leases = Arc::new(Leases::default());
    let factory = Factory::new(store.clone(), me, host, freshness.clone(), clock.clone())
        .with_leases(leases.clone());
    let attention = Arc::new(
        Attention::new(me, store.clone(), peers.clone(), daemon.coordinator()).with_factory(
            clock.clone(),
            freshness.clone(),
            Arc::new(RigMarks {
                sessions: sessions.clone(),
                workspaces,
            }),
        ),
    );
    let app = api::router(api.clone())
        .merge(view_router(View {
            store: store.clone(),
            peers: peers.clone(),
            coordinator: daemon.coordinator(),
            secrets: held,
            leases,
        }))
        .merge(attention.clone().router());
    let placing = api.clone();
    let servers = vec![
        tokio::spawn(foundryd::placement::run(placing)),
        tokio::spawn(async move {
            axum::serve(
                hooks,
                hook_router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            .unwrap();
        }),
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }),
        tokio::spawn(factory.serve()),
        tokio::spawn(foundryd::factory::cutoff::run(
            store, me, sessions, freshness, clock,
        )),
    ];
    let machine = Machine {
        data,
        servers,
        _home: home,
    };
    let part = Part {
        trap,
        addr,
        api,
        attention,
    };
    (machine, part)
}

// --- the sessions' scripts ---

/// A session that commits the feature, pushes its branch and reports pull request `pr`.
fn builds_and_opens(pr: u64) -> String {
    format!(
        r#"commit feature.txt 'the feature' 'add the feature'
git push -q origin HEAD
agent '- hook: {{event: SessionStart}}
- wait_prompt
- stop: {{last_assistant_message: "Opened the pull request.\nFACTORY-DONE: https://github.com/acme/widgets/pull/{pr}"}}
- wait_input'
"#
    )
}

/// As [`builds_and_opens`], with the pull request of the attempt the session's branch is
/// for: 101 for the first, 102 for the second.
fn builds_and_opens_its_attempt() -> &'static str {
    r#"commit feature.txt 'the feature' 'add the feature'
git push -q origin HEAD
case $(git rev-parse --abbrev-ref HEAD) in *-1) pr=101 ;; *) pr=102 ;; esac
agent "- hook: {event: SessionStart}
- wait_prompt
- stop: {last_assistant_message: \"Opened the pull request.\\nFACTORY-DONE: https://github.com/acme/widgets/pull/$pr\"}
- wait_input"
"#
}

/// A session that takes the launch message, waits until the test writes `release-<issue>` in
/// the marks folder, then builds and reports pull request `pr`. It writes `launched-<issue>`
/// once it has the message.
fn waits_then_opens(issue: u64, pr: u64) -> String {
    format!(
        r#"agent '- hook: {{event: SessionStart}}
- wait_prompt'
touch "$MARKS/launched-{issue}"
while [ ! -e "$MARKS/release-{issue}" ]; do sleep 0.1; done
commit feature.txt 'the feature' 'add the feature'
git push -q origin HEAD
agent '- stop: {{last_assistant_message: "Opened the pull request.\nFACTORY-DONE: https://github.com/acme/widgets/pull/{pr}"}}
- wait_input'
"#
    )
}

/// The run of attempt `attempt` of `issue`.
fn run_of(issue: u64, attempt: u32) -> RunId {
    RunId::new(REPO, issue, attempt).unwrap()
}

/// The event names of `events`, each with its step when it is a step.
fn names(events: &[RunEvent]) -> Vec<String> {
    events
        .iter()
        .map(|e| match e.data["step"].as_str() {
            Some(step) => format!("{} {step}", e.event),
            None => e.event.clone(),
        })
        .collect()
}

/// Whether `events` hold one whose cause gives `reason`.
fn caused_by(events: &[RunEvent], reason: &str) -> bool {
    events
        .iter()
        .any(|e| e.cause.as_ref().is_some_and(|c| c["reason"] == reason))
}

/// Waits until `marks/<name>` exists.
async fn marked(rig: &Rig, name: &str) {
    eprintln!("fleet: waiting for the mark {name}");
    let path = rig.h.marks.join(name);
    until(WAIT, &format!("the mark {name} is written"), || {
        path.exists()
    })
    .await;
}

fn release(rig: &Rig, issue: u64) {
    std::fs::write(rig.h.marks.join(format!("release-{issue}")), "").unwrap();
}

// --- the tests ---

#[test]
fn the_coordinator_killed_during_a_build_and_the_next_carries_the_run_on_with_its_events_in_order()
{
    isolated(
        module_path!(),
        "the_coordinator_killed_during_a_build_and_the_next_carries_the_run_on_with_its_events_in_order",
        || async {
            let mut rig = Rig::start(RigOptions::default()).await;
            let run = run_of(12, 1);
            rig.h.github.pull(101, "fac/12-1", 12);
            rig.h.script(12, &waits_then_opens(12, 101));
            rig.h.ready(12, "Make the widgets faster");
            rig.h.reaches(12, "factory:building").await;
            marked(&rig, "launched-12").await;
            // The old coordinator's events have reached the next before it dies.
            until(WAIT, "the launch is recorded on machine 1", || {
                rig.events(1, &run)
                    .iter()
                    .any(|e| e.event == "session_launched")
            })
            .await;
            let old = rig.lease_held_by(0).await;

            rig.kill(0).await;
            rig.holds_lease(1, old + 1).await;
            release(&rig, 12);
            rig.h.reaches(12, "factory:pr-open").await;

            let events = rig.events(1, &run);
            let generations: Vec<u64> = events.iter().map(|e| e.generation).collect();
            let next = old + 1;
            let old = events.iter().take_while(|e| e.generation == old).count();
            assert!(old > 0, "the old coordinator's events are kept");
            assert!(
                generations[old..].iter().all(|g| *g == next),
                "the old coordinator's events come first, then the next's: {generations:?}"
            );
            assert_eq!(
                rig.events_by(1, &run, 0),
                events[..old].to_vec(),
                "the old generation is the old coordinator's"
            );
            assert_eq!(
                rig.events_by(1, &run, 1),
                events[old..].to_vec(),
                "the next generation is the next coordinator's"
            );
            assert!(
                rig.events_by(1, &run, 2).is_empty(),
                "a machine that never coordinated wrote no event"
            );
            // The next coordinator carried the build on: it launched nothing again.
            let launched = events
                .iter()
                .filter(|e| e.event == "session_launched")
                .count();
            assert_eq!(launched, 1, "one launch: {:?}", names(&events));
            assert_eq!(
                events.iter().filter(|e| e.event == "claimed").count(),
                1,
                "one claim: {:?}",
                names(&events)
            );
            assert!(
                events[old..].iter().any(|e| e.event == "gate_checkout"),
                "the next coordinator gated the run: {:?}",
                names(&events)
            );
            assert_eq!(
                rig.h.workspaces_named("fac/12-1").len(),
                1,
                "one build workspace"
            );
            assert!(
                rig.h.workspaces_named("fac/12-1-r1").is_empty(),
                "the build was not resumed elsewhere"
            );
            let build = &rig.h.workspaces_named("fac/12-1")[0];
            assert_eq!(rig.h.sessions_in(build.body.id).len(), 1, "one session");
            let launches = rig
                .h
                .sent()
                .iter()
                .filter(|m| m.line.starts_with("/factory-build acme/widgets#12"))
                .count();
            assert_eq!(launches, 1, "the launch message was typed once");
            assert_eq!(rig.claims(12).len(), 1, "the issue was claimed once");
            // Every machine still running holds the next coordinator's events in the same order.
            // It writes on at every pass, so machine 2 is waited for until it holds what machine
            // 1 held at pr-open. Of the old coordinator's events, each machine holds those its
            // links sent before it died, and no machine passes on another's records, so one
            // machine's are the first of the other's.
            until(
                WAIT,
                "machine 2 holds the next coordinator's events as machine 1 does",
                || rig.events_by(2, &run, 1).starts_with(&events[old..]),
            )
            .await;
            let (on_1, on_2) = (rig.events_by(1, &run, 0), rig.events_by(2, &run, 0));
            assert!(
                on_1.starts_with(&on_2) || on_2.starts_with(&on_1),
                "machine 2 holds the old coordinator's events as machine 1 does, up to the last \
                 one sent: {:?} and {:?}",
                names(&on_1),
                names(&on_2)
            );
            rig.no_foreign_claim(&[1, 2]).await;
            rig.h.no_unknown_calls();
        },
    );
}

#[test]
fn a_coordinator_killed_between_the_claim_comment_and_the_label_leaves_the_issue_claimed_again() {
    isolated(
        module_path!(),
        "a_coordinator_killed_between_the_claim_comment_and_the_label_leaves_the_issue_claimed_again",
        || async {
            let mut rig = Rig::start(RigOptions::default()).await;
            let old = rig.lease_held_by(0).await;
            rig.trap(0).on(adds_triage(12), Then::DieBefore);
            rig.h.github.pull(101, "fac/12-1", 12);
            rig.h.github.pull(102, "fac/12-2", 12);
            rig.h.script(12, builds_and_opens_its_attempt());
            rig.h.ready(12, "Make the widgets faster");

            until(WAIT, "the coordinator dies at the label", || {
                rig.trap(0).sprung()
            })
            .await;
            assert_eq!(
                rig.h.state(12),
                BTreeSet::from(["factory:ready".to_owned()]),
                "the label never moved"
            );
            let first = rig.claims(12);
            assert_eq!(first.len(), 1, "the claim comment is posted");
            let first_run = first[0].0.clone();
            until(WAIT, "the claim's events reach machine 1", || {
                let held = rig.events(1, &first_run);
                !held.is_empty() && held == rig.events(0, &first_run)
            })
            .await;
            rig.kill(0).await;

            rig.h.reaches(12, "factory:pr-open").await;
            let claims = rig.claims(12);
            assert_eq!(claims.len(), 2, "the issue was claimed again: {claims:?}");
            let (run, _) = claims.last().unwrap().clone();
            assert_ne!(run, first_run, "the newest claim names a run of its own");
            let events = rig.events(1, &run);
            assert!(
                !events.is_empty() && events.iter().all(|e| e.generation == old + 1),
                "the new claim is the next coordinator's: {:?}",
                names(&events)
            );
            let name = format!("fac/12-{}", run.attempt());
            assert_eq!(rig.h.workspaces_named(&name).len(), 1, "{name} is built");
            // The cut-short claim's run is finished, so it holds no slot.
            until(WAIT, "the cut-short run is finished", || {
                rig.events(1, &first_run)
                    .iter()
                    .any(|e| e.event == "finished")
            })
            .await;
            assert_eq!(rig.status_comments(12).len(), 1, "one status comment");
            rig.no_foreign_claim(&[1, 2]).await;
            rig.h.no_unknown_calls();
        },
    );
}

#[test]
fn a_coordinator_killed_between_the_label_and_the_status_comment_has_its_claim_carried_on() {
    isolated(
        module_path!(),
        "a_coordinator_killed_between_the_label_and_the_status_comment_has_its_claim_carried_on",
        || async {
            let mut rig = Rig::start(RigOptions::default()).await;
            let run = run_of(12, 1);
            let old = rig.lease_held_by(0).await;
            rig.trap(0).on(posts_status(12), Then::DieBefore);
            rig.h.github.pull(101, "fac/12-1", 12);
            rig.h.script(12, &builds_and_opens(101));
            rig.h.ready(12, "Make the widgets faster");

            until(WAIT, "the coordinator dies at the status comment", || {
                rig.trap(0).sprung()
            })
            .await;
            assert_eq!(
                rig.h.state(12),
                BTreeSet::from(["factory:triage".to_owned()]),
                "the label moved"
            );
            assert!(rig.status_comments(12).is_empty(), "no status comment yet");
            until(WAIT, "the claim's events reach machine 1", || {
                let held = rig.events(1, &run);
                !held.is_empty() && held == rig.events(0, &run)
            })
            .await;
            rig.kill(0).await;

            rig.h.reaches(12, "factory:pr-open").await;
            let claims = rig.claims(12);
            assert_eq!(claims.len(), 1, "the claim was carried on: {claims:?}");
            assert_eq!(claims[0].0, run);
            let events = rig.events(1, &run);
            assert_eq!(
                events.iter().filter(|e| e.event == "claimed").count(),
                1,
                "{:?}",
                names(&events)
            );
            assert!(
                events
                    .iter()
                    .filter(|e| e.event == "session_launched")
                    .all(|e| e.generation == old + 1),
                "the next coordinator launched it"
            );
            assert_eq!(rig.h.workspaces_named("fac/12-1").len(), 1);
            assert_eq!(rig.status_comments(12).len(), 1, "one status comment");
            rig.no_foreign_claim(&[1, 2]).await;
            rig.h.no_unknown_calls();
        },
    );
}

/// Has machine 0, the coordinator, claim issue 12 for machine 2, the one builder, cut off from
/// the fleet before it records anything, so none of the run's events leave it, and die once
/// its status comment has landed. Answers the run its claim names and the lease generation
/// the dead coordinator held.
async fn claim_and_die_before_any_event_leaves(rig: &Rig) -> (RunId, u64) {
    let old = rig.lease_held_by(0).await;
    rig.trap(0).on(reads_approval(12), Then::Run(rig.cutter(0)));
    rig.trap(0).on(posts_status(12), Then::DieAfter);
    rig.h.github.pull(101, "fac/12-1", 12);
    rig.h.script(12, &builds_and_opens(101));
    rig.h.ready(12, "Make the widgets faster");
    until(
        WAIT,
        "the coordinator dies after its status comment",
        || rig.trap(0).sprung(),
    )
    .await;
    assert_eq!(
        rig.h.state(12),
        BTreeSet::from(["factory:triage".to_owned()])
    );
    let claims = rig.claims(12);
    assert_eq!(claims.len(), 1, "{claims:?}");
    let (run, machine) = claims[0].clone();
    assert_eq!(
        machine,
        rig.id(2).to_string(),
        "the claim names the builder"
    );
    assert_eq!(rig.status_comments(12).len(), 1);
    assert!(
        !rig.events(0, &run).is_empty(),
        "the dead coordinator recorded its claim"
    );
    for i in [1, 2] {
        assert!(
            rig.events(i, &run).is_empty(),
            "none of its events reached machine {i}"
        );
    }
    (run, old)
}

/// Waits for issue 12 to reach `pr-open` under the claim of `run` and checks what the next
/// coordinator did: it carried the claim on, at the generation after `old`, on the machine the
/// claim names.
async fn carried_on_from_the_claim(rig: &Rig, run: &RunId, old: u64) -> Vec<RunEvent> {
    rig.h.reaches(12, "factory:pr-open").await;
    let claims = rig.claims(12);
    assert_eq!(claims.len(), 1, "the claim was carried on: {claims:?}");
    let events = rig.events(1, run);
    assert!(
        events.iter().all(|e| e.generation == old + 1),
        "every event is the next coordinator's: {:?}",
        names(&events)
    );
    assert!(
        caused_by(&events, "claim-found"),
        "the run was found by its claim comment: {:?}",
        names(&events)
    );
    let builds = rig.h.workspaces_named("fac/12-1");
    assert_eq!(builds.len(), 1, "one build workspace: {builds:?}");
    assert_eq!(builds[0].owner, rig.id(2), "built where the claim says");
    assert!(rig.h.workspaces_named("fac/12-1-r1").is_empty());
    events
}

#[test]
fn a_coordinator_killed_between_the_claim_and_its_first_event_with_no_workspace_is_launched_where_the_claim_says()
 {
    isolated(
        module_path!(),
        "a_coordinator_killed_between_the_claim_and_its_first_event_with_no_workspace_is_launched_where_the_claim_says",
        || async {
            let mut rig = Rig::start(RigOptions {
                skills: vec![2],
                ..RigOptions::default()
            })
            .await;
            let (run, old) = claim_and_die_before_any_event_leaves(&rig).await;
            assert!(rig.h.workspaces_named("fac/12-1").is_empty());
            rig.kill(0).await;

            let events = carried_on_from_the_claim(&rig, &run, old).await;
            assert!(
                caused_by(&events, "workspace-created"),
                "the next coordinator asked for the workspace: {:?}",
                names(&events)
            );
            rig.no_foreign_claim(&[1, 2]).await;
            rig.h.no_unknown_calls();
        },
    );
}

#[test]
fn a_coordinator_killed_between_the_claim_and_its_first_event_with_a_workspace_has_it_carried_on() {
    isolated(
        module_path!(),
        "a_coordinator_killed_between_the_claim_and_its_first_event_with_a_workspace_has_it_carried_on",
        || async {
            let mut rig = Rig::start(RigOptions {
                skills: vec![2],
                ..RigOptions::default()
            })
            .await;
            let (run, old) = claim_and_die_before_any_event_leaves(&rig).await;
            // The dead coordinator's ask for the build had landed on the builder.
            let made = rig.create_build(2, 2, &run).await;
            until(WAIT, "machine 1 holds the workspace's record", || {
                rig.h.workspaces_named("fac/12-1").len() == 1
                    && lock(&rig.h.fleet.daemons[1].store)
                        .since(rig.id(2), 0)
                        .unwrap()
                        .iter()
                        .any(|r| matches!(r, foundry_proto::record::AnyRecord::Workspace(w) if w.body.id == made))
            })
            .await;
            rig.kill(0).await;

            let events = carried_on_from_the_claim(&rig, &run, old).await;
            assert_eq!(rig.h.workspaces_named("fac/12-1")[0].body.id, made);
            assert!(
                caused_by(&events, "workspace-found"),
                "the next coordinator found the workspace: {:?}",
                names(&events)
            );
            rig.no_foreign_claim(&[1, 2]).await;
            rig.h.no_unknown_calls();
        },
    );
}

#[test]
fn a_coordinator_killed_between_the_claim_and_its_first_event_with_a_workspace_whose_record_arrives_a_pass_later()
 {
    isolated(
        module_path!(),
        "a_coordinator_killed_between_the_claim_and_its_first_event_with_a_workspace_whose_record_arrives_a_pass_later",
        || async {
            let mut rig = Rig::start(RigOptions {
                skills: vec![2],
                ..RigOptions::default()
            })
            .await;
            let (run, old) = claim_and_die_before_any_event_leaves(&rig).await;
            // The next coordinator looks at nothing until the workspace is made.
            rig.trap(1).hold();
            rig.kill(0).await;
            rig.holds_lease(1, old + 1).await;
            // The dead coordinator's ask lands on the builder while the builder is cut off, so
            // its record cannot reach the next coordinator yet.
            rig.h.fleet.cut(2);
            let made = rig.create_build(2, 2, &run).await;
            assert!(
                lock(&rig.h.fleet.daemons[1].store)
                    .since(rig.id(2), 0)
                    .unwrap()
                    .iter()
                    .all(|r| !matches!(r, foundry_proto::record::AnyRecord::Workspace(w) if w.body.id == made)),
                "the record is held back"
            );
            rig.trap(1).release();
            until(WAIT, "the next coordinator asks for the workspace", || {
                rig.events(1, &run).iter().any(|e| {
                    e.data["step"]
                        .as_str()
                        .is_some_and(|s| s.starts_with("workspace-requested"))
                })
            })
            .await;
            // A pass later, the record arrives.
            let start = rig.h.clock_now();
            until(WAIT, "a pass goes by", || {
                rig.h.clock_now() - start > Duration::from_secs(20)
            })
            .await;
            assert!(
                rig.h
                    .workspaces_named("fac/12-1")
                    .iter()
                    .all(|w| w.body.id == made),
                "no second workspace was made"
            );
            rig.h.fleet.heal(2);

            let events = carried_on_from_the_claim(&rig, &run, old).await;
            assert_eq!(rig.h.workspaces_named("fac/12-1")[0].body.id, made);
            assert!(
                caused_by(&events, "workspace-found") || caused_by(&events, "workspace-created"),
                "{:?}",
                names(&events)
            );
            rig.no_foreign_claim(&[1, 2]).await;
            rig.h.no_unknown_calls();
        },
    );
}

#[test]
fn a_split_where_only_the_lease_holders_side_acts_and_the_other_sides_status_keeps_no_session_alive()
 {
    isolated(
        module_path!(),
        "a_split_where_only_the_lease_holders_side_acts_and_the_other_sides_status_keeps_no_session_alive",
        || async {
            let rig = Rig::start(RigOptions::default()).await;
            rig.h.github.pull(101, "fac/12-1", 12);
            rig.h.script(12, &waits_then_opens(12, 101));
            rig.h.ready(12, "Make the widgets faster");
            rig.h.reaches(12, "factory:building").await;
            marked(&rig, "launched-12").await;
            let generation = rig.lease_held_by(0).await;
            assert_eq!(rig.live_sessions("fac/12-1").len(), 1);

            // Machine 0 holds the lease alone; machines 1 and 2 have the build and their own
            // coordinator.
            rig.h.fleet.cut(0);
            let split = rig.h.clock_now();
            eprintln!("fleet: split at {split:?}");
            let (one, zero) = (rig.id(1), rig.id(0));
            until(WAIT, "machine 1 writes a status that does not hold", || {
                lock(&rig.h.fleet.daemons[1].store)
                    .statuses(rig.project)
                    .unwrap()
                    .iter()
                    .any(|(writer, s)| *writer == one && !s.holds)
            })
            .await;
            until(WAIT * 2, "the build session ends", || {
                rig.live_sessions("fac/12-1").is_empty()
            })
            .await;
            let ended = rig.h.clock_now() - split;
            eprintln!("fleet: the session ended {ended:?} after the split");
            assert!(
                ended >= Duration::from_secs(9 * 60),
                "ended by the cut-off, ten minutes on: {ended:?}"
            );
            assert!(
                ended < Duration::from_secs(15 * 60),
                "ended before the holder could resume it: {ended:?}"
            );

            // Only the holder's side acted.
            assert_eq!(
                rig.lease(),
                Some((zero.to_string(), generation)),
                "the lease never moved"
            );
            for (writer, status) in lock(&rig.h.fleet.daemons[1].store)
                .statuses(rig.project)
                .unwrap()
            {
                if writer == one {
                    assert!(!status.holds);
                    assert_eq!(
                        status.not_holding,
                        Some(NotHolding::HeldBy {
                            machine: zero,
                            generation
                        })
                    );
                }
            }
            for i in [1, 2] {
                assert_eq!(
                    rig.trap(i).writes(),
                    Vec::<String>::new(),
                    "machine {i} changed GitHub"
                );
            }
            assert_eq!(
                rig.h.state(12),
                BTreeSet::from(["factory:building".to_owned()])
            );
            rig.h.no_unknown_calls();
        },
    );
}

#[test]
fn a_lone_coordinator_hosting_its_own_run_ends_nothing() {
    isolated(
        module_path!(),
        "a_lone_coordinator_hosting_its_own_run_ends_nothing",
        || async {
            let rig = Rig::start(RigOptions {
                tokens: vec![0],
                skills: vec![0],
                ..RigOptions::default()
            })
            .await;
            rig.h.fleet.cut(1);
            rig.h.fleet.cut(2);
            let zero = rig.id(0);
            until(WAIT, "machine 0 is alone", || {
                rig.h.fleet.daemons[0].peers.online() == BTreeSet::from([zero])
            })
            .await;
            rig.h.github.pull(101, "fac/12-1", 12);
            rig.h.script(12, &waits_then_opens(12, 101));
            rig.h.ready(12, "Make the widgets faster");
            rig.h.reaches(12, "factory:building").await;
            marked(&rig, "launched-12").await;
            assert_eq!(
                rig.h.owner_of("fac/12-1"),
                Some(zero),
                "it hosts its own run"
            );

            // Well past the cut-off's ten minutes, the session still runs.
            let start = rig.h.clock_now();
            until(WAIT * 2, "twelve minutes pass", || {
                rig.h.clock_now() - start > Duration::from_secs(12 * 60)
            })
            .await;
            assert_eq!(
                rig.live_sessions("fac/12-1").len(),
                1,
                "the lone coordinator ended its own session"
            );
            release(&rig, 12);
            rig.h.reaches(12, "factory:pr-open").await;

            let events = rig.events(0, &run_of(12, 1));
            assert!(
                events.iter().all(|e| e.event != "session_interrupted"),
                "{:?}",
                names(&events)
            );
            let generation = rig.lease_held_by(0).await;
            assert!(events.iter().all(|e| e.generation == generation));
            assert!(rig.h.workspaces_named("fac/12-1-r1").is_empty());
            rig.h.no_unknown_calls();
        },
    );
}

#[test]
fn the_factory_view_its_attention_items_and_stop_work_from_a_machine_that_is_not_the_coordinator() {
    isolated(
        module_path!(),
        "the_factory_view_its_attention_items_and_stop_work_from_a_machine_that_is_not_the_coordinator",
        || async {
            // Machine 1 builds; machine 2 neither coordinates nor builds.
            let rig = Rig::start(RigOptions {
                skills: vec![1],
                ..RigOptions::default()
            })
            .await;
            let run = run_of(12, 1);
            rig.h.github.pull(101, "fac/12-1", 12);
            rig.h.script(12, &waits_then_opens(12, 101));
            rig.h.ready(12, "Make the widgets faster");
            rig.h.reaches(12, "factory:building").await;
            marked(&rig, "launched-12").await;

            let zero = rig.id(0).to_string();
            let mut view = Value::Null;
            for _ in 0..600 {
                view = rig.get(2, "/api/factory").await;
                let project = &view["projects"][0];
                let running = project["status"]["runs"]
                    .as_array()
                    .is_some_and(|runs| runs.iter().any(|r| r["run"] == run.to_string()));
                if project["writer"] == zero.as_str() && running {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            let project = &view["projects"][0];
            assert_eq!(project["writer"], zero.as_str(), "{view:#}");
            assert_eq!(project["status"]["holds"], true, "{view:#}");
            let generation = rig.lease_held_by(0).await;
            assert_eq!(project["status"]["generation"], generation, "{view:#}");
            let live = project["status"]["runs"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["run"] == run.to_string())
                .unwrap_or_else(|| panic!("the run is listed live: {view:#}"));
            assert_eq!(live["machine"], rig.id(1).to_string(), "{live:#}");
            assert!(
                rig.items(2)
                    .await
                    .iter()
                    .all(|i| !matches!(i.kind, Kind::RunStuck | Kind::FactoryIdle)),
                "nothing needs a person yet"
            );

            let (status, text) = rig.post(2, &format!("/api/factory/runs/{run}/stop")).await;
            assert!(
                (200..300).contains(&status),
                "Stop from machine 2: {status} {text}"
            );
            rig.h.reaches(12, "factory:stuck").await;

            let stuck = format!("run_stuck:{run}");
            let start = tokio::time::Instant::now();
            let item = loop {
                let items = rig.items(2).await;
                if let Some(item) = items
                    .into_iter()
                    .find(|i| i.kind == Kind::RunStuck && i.id == stuck)
                {
                    break item;
                }
                if start.elapsed() > WAIT {
                    let view = rig.get(2, "/api/factory").await;
                    panic!(
                        "machine 2 lists the stuck run: it lists {:#?}, and the view {view:#}, and the events {:?}",
                        rig.items(2).await,
                        names(&rig.events(2, &run))
                    );
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            };
            assert!(item.what.contains("acme/widgets#12"), "{item:?}");
            let view = rig.get(2, "/api/factory").await;
            let runs = view["projects"][0]["status"]["runs"].as_array().unwrap();
            assert!(
                runs.iter().all(|r| r["run"] != run.to_string()),
                "the run is no longer live: {view:#}"
            );
            until(WAIT, "machine 2 holds the stop's event", || {
                rig.events(2, &run)
                    .iter()
                    .any(|e| e.event == "stop_requested")
            })
            .await;
            assert!(
                rig.trap(2).writes().is_empty(),
                "machine 2 changed nothing on GitHub itself"
            );
            until(WAIT, "the stuck run's session is ended", || {
                rig.live_sessions("fac/12-1").is_empty()
            })
            .await;
            rig.h.no_unknown_calls();
        },
    );
}

// --- a peer on protocol 4 ---

/// A link to machine `i` that the test speaks on as `machine`, in `protocol`: the hello is
/// sent.
async fn raw_link(rig: &Rig, i: usize, machine: MachineId, protocol: u16) -> Conn {
    let (mine, theirs) = Conn::pair();
    let peers = rig.h.fleet.daemons[i].peers.clone();
    tokio::spawn(async move { peers.accept(theirs).await });
    let hello = PeerMessage::Hello {
        protocol,
        machine,
        versions: Default::default(),
        factory_versions: Default::default(),
    };
    mine.send
        .send(serde_json::to_string(&hello).unwrap())
        .await
        .expect("the link is open");
    mine
}

/// Every message `conn` gets until `enough` holds of them or `long` passes, while the test
/// sends a heartbeat in `protocol` each second so the link stays open.
async fn listen(
    conn: &mut Conn,
    protocol: u16,
    long: Duration,
    enough: impl Fn(&[Value]) -> bool,
) -> Vec<Value> {
    let heartbeat = serde_json::to_string(&PeerMessage::Records {
        protocol,
        records: Vec::new(),
    })
    .unwrap();
    let mut messages = Vec::new();
    let start = tokio::time::Instant::now();
    let mut beat = tokio::time::Instant::now();
    while start.elapsed() < long && !enough(&messages) {
        if beat.elapsed() >= Duration::from_secs(1) {
            let _ = conn.send.try_send(heartbeat.clone());
            beat = tokio::time::Instant::now();
        }
        match tokio::time::timeout(Duration::from_millis(200), conn.recv.recv()).await {
            Ok(Some(text)) => messages.push(serde_json::from_str(&text).unwrap()),
            Ok(None) => panic!("the link closed"),
            Err(_) => {}
        }
    }
    messages
}

/// The factory records in `messages`, each as its owner and key.
fn factory_records_in(messages: &[Value]) -> BTreeSet<(MachineId, FactoryKey)> {
    messages
        .iter()
        .filter(|m| m["type"] == "factory_records")
        .flat_map(|m| serde_json::from_value::<Vec<FactoryRecord>>(m["records"].clone()).unwrap())
        .map(|r| (r.owner, r.key()))
        .collect()
}

#[test]
fn a_peer_on_protocol_4_receives_no_factory_record_then_updates_and_receives_them_all() {
    isolated(
        module_path!(),
        "a_peer_on_protocol_4_receives_no_factory_record_then_updates_and_receives_them_all",
        || async {
            let rig = Rig::start(RigOptions::default()).await;
            rig.h.github.pull(101, "fac/12-1", 12);
            rig.h.script(12, &builds_and_opens(101));
            rig.h.ready(12, "Make the widgets faster");
            rig.h.reaches(12, "factory:pr-open").await;

            // A fourth machine joins the fleet's list.
            let old = MachineId::random();
            until(WAIT, "every daemon lists the new machine", || {
                let held: Vec<_> = rig.h.fleet.daemons.iter().map(|d| d.settings()).collect();
                if held.iter().all(|s| s.machines.iter().any(|m| m.id == old)) {
                    return true;
                }
                let mut settings = held.into_iter().max_by_key(|s| s.seq).unwrap();
                settings.seq += 1;
                if !settings.machines.iter().any(|m| m.id == old) {
                    settings.machines.push(foundry_proto::settings::Machine {
                        id: old,
                        name: "machine-old".into(),
                        address: "100.64.0.9".into(),
                        key: String::new(),
                    });
                }
                for daemon in &rig.h.fleet.daemons {
                    lock(&daemon.store)
                        .apply_settings(settings.clone())
                        .unwrap();
                }
                false
            })
            .await;
            // Every factory record machine `i` holds, by owner and key: a status written again
            // keeps its key under a newer version. Each machine sends its own records, so a
            // peer has them all once it has each machine's own.
            let held = |rig: &Rig, i: usize| -> BTreeSet<(MachineId, FactoryKey)> {
                let store = lock(&rig.h.fleet.daemons[i].store);
                let mut all = BTreeSet::new();
                for (owner, _) in store.factory_versions().unwrap() {
                    for record in store.factory_since(owner, 0, usize::MAX).unwrap() {
                        all.insert((record.owner, record.key()));
                    }
                }
                all
            };
            let own = |rig: &Rig, i: usize| -> BTreeSet<(MachineId, FactoryKey)> {
                let me = rig.id(i);
                held(rig, i)
                    .into_iter()
                    .filter(|(owner, _)| *owner == me)
                    .collect()
            };
            let kinds: BTreeSet<&str> = held(&rig, 1).iter().map(|(_, key)| key.kind).collect();
            for kind in [
                "run_event",
                "factory_status",
                "machine_held",
                "session_turn",
            ] {
                assert!(kinds.contains(kind), "machine 1 holds a {kind}: {kinds:?}");
            }

            // On protocol 4 it gets P1's records from every machine and no factory record.
            for i in 0..3 {
                let mut conn = raw_link(&rig, i, old, 4).await;
                let messages = listen(&mut conn, 4, Duration::from_secs(5), |_| false).await;
                assert!(
                    messages.iter().any(|m| m["type"] == "hello"),
                    "machine {i} answers the hello: {messages:?}"
                );
                let p1: usize = messages
                    .iter()
                    .filter(|m| m["type"] == "records")
                    .map(|m| m["records"].as_array().map_or(0, Vec::len))
                    .sum();
                assert!(p1 > 0, "machine {i} sends P1's records on protocol 4");
                for message in &messages {
                    let text = message.to_string();
                    assert!(
                        message["type"] != "factory_records"
                            && !text.contains("run_event")
                            && !text.contains("factory_status"),
                        "machine {i} sent a factory record to a peer on protocol 4: {text}"
                    );
                }
                drop(conn);
            }
            tokio::time::sleep(Duration::from_secs(2)).await;

            // Updated, the same machine gets every machine's own factory records, which are
            // every factory record the fleet holds.
            let fleet: BTreeSet<_> = (0..3).flat_map(|i| held(&rig, i)).collect();
            let mut got = BTreeSet::new();
            for i in 0..3 {
                let want = own(&rig, i);
                assert!(!want.is_empty(), "machine {i} owns factory records");
                let mut conn = raw_link(&rig, i, old, PROTOCOL).await;
                let messages = listen(&mut conn, PROTOCOL, WAIT, |messages| {
                    factory_records_in(messages).is_superset(&want)
                })
                .await;
                let sent = factory_records_in(&messages);
                let missing: Vec<_> = want.difference(&sent).collect();
                assert!(missing.is_empty(), "machine {i} did not send {missing:?}");
                got.extend(sent);
                drop(conn);
            }
            let missing: Vec<_> = fleet.difference(&got).collect();
            assert!(
                missing.is_empty(),
                "factory records never sent: {missing:?}"
            );
            rig.h.no_unknown_calls();
        },
    );
}

#[test]
fn the_traps_rules_match_the_claims_calls() {
    let call = |args: &[&str]| args.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>();
    let edit = call(&[
        "issue",
        "edit",
        "12",
        "--repo",
        REPO,
        "--add-label",
        "factory:triage",
        "--remove-label",
        "factory:ready",
    ]);
    assert!(adds_triage(12)(&edit));
    assert!(!adds_triage(13)(&edit));
    assert!(!adds_triage(12)(&call(&[
        "issue",
        "edit",
        "12",
        "--add-label",
        "factory:building"
    ])));
    let status = format!("triage\n\n{STATUS_MARKER}");
    assert!(posts_status(12)(&call(&[
        "issue", "comment", "12", "--repo", REPO, "--body", &status
    ])));
    assert!(!posts_status(12)(&call(&[
        "issue", "comment", "12", "--body", "claimed"
    ])));
    assert!(reads_approval(12)(&call(&[
        "api",
        "graphql",
        "-f",
        "query=query { repository { issue { timelineItems { nodes } } } }",
        "-F",
        "number=12",
    ])));
    assert!(is_write(&edit));
    assert!(is_write(&call(&[
        "api",
        "-X",
        "PATCH",
        "repos/acme/widgets/issues/comments/1"
    ])));
    assert!(!is_write(&call(&["issue", "view", "12"])));
    assert!(!is_write(&call(&["api", "graphql", "-f", "query=x"])));
}
