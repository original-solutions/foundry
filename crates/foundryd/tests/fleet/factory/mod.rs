//! Fleet tests of the factory.
//!
//! The factory harness: three daemons, each with its whole API, its terminals, workspaces and
//! sessions, and the factory's service, as `foundryd` runs them. Daemon 0 is the acting
//! coordinator and holds the lease; the others build and gate. What the daemons share is
//! what a fleet shares: GitHub, which here is one [`FakeTracker`] for the test, and the
//! project's repo, which is a local bare repo that is each project's `origin` and its lease
//! remote. Every age the factory measures is read on one [`FakeClock`] that the harness moves
//! on, ten times as fast as the wall clock. Each session runs the fake agent, scripted per run
//! by the test ([`Harness::script`]).
//!
//! The project's repo is `https://github.com/acme/widgets.git`, as a factory project's must
//! be. git reaches it in the bare repo through a `git` put first on `PATH` that rewrites
//! `https://github.com/` to the folder of bare repos, for every git call: the daemons' own,
//! with or without a token, and those of a session or a gate terminal. A `gh` there fails, so
//! nothing reaches GitHub. `PATH` must be set before any thread starts, so each test runs in
//! a child process of the test binary that is given its `PATH`, its `HOME` and a credential
//! helper set for the machine ([`isolated`]).

mod build;
mod failover;
mod github;
mod workers;

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use foundry_proto::factory::{
    FactoryBody, FactoryRecord, FactorySettings, MachineHeld, ProjectFactory, RunEvent, RunId,
};
use foundry_proto::ids::{MachineId, ProjectId, SessionId, TerminalId, WorkspaceId};
use foundry_proto::record::{AnyRecord, Record, Session, Workspace};
use foundry_proto::settings::{Limits, Project};
use foundry_proto::state::SessionState;
use foundryd::api::{self, Api};
use foundryd::clock::{Clock, FakeClock};
use foundryd::coordinator::Coordinator;
use foundryd::factory::github::{Commenting, Hiding, Tracker};
use foundryd::factory::lease::Lease;
use foundryd::factory::ledger::Hide;
use foundryd::factory::messages::Message;
use foundryd::factory::status::Freshness;
use foundryd::factory::worker::{CreateWorkspace, Worker, WorkerError};
use foundryd::factory::{Answering, Factory, Host, Leasing, Watched};
use foundryd::git_auth::GitAuth;
use foundryd::secrets::{Edit, Secrets, SecretsError};
use foundryd::sessions::factory::{FactoryStart, SendRefused};
use foundryd::sessions::turns::TurnText;
use foundryd::sessions::{self, Gateway, Sessions};
use foundryd::store::Applied;
use foundryd::terminals::Terminals;
use foundryd::workspaces::Workspaces;
use tempfile::TempDir;
use tokio::task::JoinHandle;

pub use github::FakeTracker;

use crate::{Daemon, Fleet, lock, ran_in_child, run_child, sample, until};

/// The project's repo on GitHub.
pub const REPO: &str = "acme/widgets";
/// The login the factory acts as.
pub const LOGIN: &str = "widget-bot";
/// Its token, held by the machines that act as it.
pub const TOKEN: &str = "ghp_FleetFactoryToken0123456789abcdefWXYZ";
/// The instructor who approves issues.
pub const INSTRUCTOR: &str = "tom";
/// What the credential helper set for the machine answers as a password.
pub const HELPER_SECRET: &str = "MachineHelperPassword0123456789qrstuv";

/// How long a test waits for a step of the factory, on the wall clock.
pub const WAIT: Duration = Duration::from_secs(120);

/// The folder of bare repos `https://github.com/` stands for, in a child process.
const ORIGINS: &str = "FOUNDRY_FLEET_FACTORY_ORIGINS";

/// The default branch's `.dw/config.yml`, with `gate` as its gate commands, `provision` as its
/// provision commands and `gate_retries` retries.
pub fn config(provision: &[&str], gate: &[&str], gate_retries: u32) -> String {
    let list = |commands: &[&str]| {
        let quoted: Vec<String> = commands
            .iter()
            .map(|c| serde_json::Value::from(*c).to_string())
            .collect();
        format!("[{}]", quoted.join(", "))
    };
    format!(
        "factory:\n  instructors: [{INSTRUCTOR}]\n  base: main\n  provision: {}\n  gate: {}\n  autonomy: pr\n  limits: {{ minutes: 180, gate_retries: {gate_retries}, checks_minutes: 30, interruption_resumes: 2 }}\n",
        list(provision),
        list(gate)
    )
}

/// Runs the test `name` of this module path: in this process when it is the child started for
/// it, else in a child process of this test binary whose `PATH`, `HOME` and git config are the
/// harness's, which must pass ([`run_child`]). `name` is the test function's name.
pub fn isolated<F, Fut>(module: &str, name: &str, test: F)
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = ()>,
{
    if ran_in_child(
        module,
        name,
        tokio::runtime::Builder::new_multi_thread(),
        test,
    ) {
        return;
    }
    let dir = tempfile::Builder::new()
        .prefix("ff")
        .tempdir_in("/tmp")
        .unwrap();
    let bin = dir.path().join("bin");
    let home = dir.path().join("home");
    let origins = dir.path().join("origins");
    for folder in [&bin, &home, &origins] {
        std::fs::create_dir_all(folder).unwrap();
    }
    let real = which("git").expect("git on PATH");
    write_program(&bin.join("git"), &git_program(&real, &origins));
    write_program(
        &bin.join("gh"),
        "#!/bin/sh\necho 'gh does not reach GitHub in the fleet tests' >&2\nexit 1\n",
    );
    let gitconfig = home.join(".gitconfig");
    std::fs::write(
        &gitconfig,
        format!(
            "[credential]\n\thelper = \"!f() {{ echo username=machine; echo password={HELPER_SECRET}; }}; f\"\n"
        ),
    )
    .unwrap();
    // One child at a time: each runs three daemons, and the factory's limits are on a clock
    // ten times as fast as the wall clock.
    static ALONE: Mutex<()> = Mutex::new(());
    let _alone = lock(&ALONE);
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    run_child(module, name, &home, &gitconfig, |child| {
        child.env(ORIGINS, &origins).env("PATH", path);
    });
}

/// `name` on `PATH`.
fn which(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}

fn write_program(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// The `git` of the fleet tests: the real one at `real`, with `https://github.com/` rewritten
/// to `origins` and the local transport allowed where only https was. `git config` runs as
/// it is, so the rewrite is in no config a factory start reads.
fn git_program(real: &Path, origins: &Path) -> String {
    format!(
        r#"#!/bin/sh
real='{real}'
skip=0
sub=
for a do
  if [ $skip = 1 ]; then skip=0; continue; fi
  case $a in
    -C|-c|--git-dir|--work-tree|--namespace|--config-env) skip=1 ;;
    -*) ;;
    *) sub=$a; break ;;
  esac
done
if [ "$sub" = config ]; then exec "$real" "$@"; fi
if [ -n "${{GIT_ALLOW_PROTOCOL+set}}" ]; then
  GIT_ALLOW_PROTOCOL="$GIT_ALLOW_PROTOCOL:file"
  export GIT_ALLOW_PROTOCOL
fi
exec "$real" -c 'url.{origins}/.insteadOf=https://github.com/' "$@"
"#,
        real = real.display(),
        origins = origins.display(),
    )
}

/// Runs git in `dir` with an identity of its own, and answers its output.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=Fleet Test",
            "-c",
            "user.email=fleet@foundry.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// `cargo test --workspace` builds the holder and the fake agent beside the test binary's
/// `deps` folder.
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

/// The `claude` a session runs: it takes the next script the test gave for the run's issue,
/// which it reads from the branch `fac/<N>-...`, and runs it with the helpers below. With no
/// script left the agent waits. The helpers:
///
/// - `agent '<steps>'` runs the fake agent with those steps, on the session's settings;
/// - `commit <file> <text> <message>` writes the file and commits it.
///
/// `$ISSUE` is the issue's number and `$MARKS` a folder the test reads.
const SHIM: &str = r#"agent_bin=$1; scripts=$2; marks=$3; shift 3
settings=; model=
while [ $# -gt 0 ]; do
  case $1 in
    --settings) settings=$2; shift 2 ;;
    --model) model=$2; shift 2 ;;
    *) shift ;;
  esac
done
export GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@foundry.invalid
export GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@foundry.invalid
MARKS=$marks
agent() {
  steps=$(mktemp "$scripts/steps.XXXXXX")
  printf '%s\n' "$1" > "$steps"
  "$agent_bin" --script "$steps" --settings "$settings" --model "$model"
}
commit() {
  printf '%s\n' "$2" > "$1" && git add -- "$1" && git commit -q -m "$3"
}
branch=$(git rev-parse --abbrev-ref HEAD)
ISSUE=${branch#fac/}; ISSUE=${ISSUE%%-*}
k=1
while [ -e "$scripts/$ISSUE/$k.sh" ]; do
  if mkdir "$scripts/$ISSUE/$k.taken" 2>/dev/null; then
    . "$scripts/$ISSUE/$k.sh"
    exit 0
  fi
  k=$((k+1))
done
agent '- hook: {event: SessionStart}
- wait_input'
"#;

/// A secret store in memory.
#[derive(Default)]
pub struct Held(Mutex<BTreeMap<String, String>>);

impl Held {
    /// A store holding the project login's token, when `token`.
    fn new(token: bool) -> Arc<Self> {
        let held = Held::default();
        if token {
            held.set(&format!("github:{LOGIN}"), TOKEN).unwrap();
        }
        Arc::new(held)
    }
}

impl Secrets for Held {
    fn get(&self, name: &str) -> Result<Option<String>, SecretsError> {
        Ok(lock(&self.0).get(name).cloned())
    }

    fn set(&self, name: &str, value: &str) -> Result<(), SecretsError> {
        lock(&self.0).insert(name.to_owned(), value.to_owned());
        Ok(())
    }

    fn update(&self, name: &str, edit: Edit<'_>) -> Result<(), SecretsError> {
        let mut held = lock(&self.0);
        if let Some(value) = edit(held.get(name).cloned()) {
            held.insert(name.to_owned(), value);
        }
        Ok(())
    }

    fn names(&self) -> Result<Vec<String>, SecretsError> {
        Ok(lock(&self.0).keys().cloned().collect())
    }

    fn entries(&self) -> Result<BTreeMap<String, String>, SecretsError> {
        Ok(lock(&self.0).clone())
    }
}

/// A message the factory gave a session: the first of a start, or one sent.
#[derive(Clone, Debug)]
pub struct Sent {
    pub line: String,
    pub body: Option<String>,
}

impl Sent {
    /// The line and the body, as one text.
    pub fn text(&self) -> String {
        format!("{}\n{}", self.line, self.body.as_deref().unwrap_or(""))
    }
}

/// The real worker, with every message it is given kept for the test.
struct Recording {
    inner: foundryd::factory::fleet::FleetWorker,
    sent: Arc<Mutex<Vec<Sent>>>,
}

impl Recording {
    /// Keeps `message` once the worker took it: a send the worker refused reached no one.
    fn keep(&self, message: &Message) {
        lock(&self.sent).push(Sent {
            line: message.line.clone(),
            body: message.body.clone(),
        });
    }
}

impl Worker for Recording {
    fn create(
        &self,
        machine: MachineId,
        request: CreateWorkspace,
    ) -> Result<WorkspaceId, WorkerError> {
        self.inner.create(machine, request)
    }

    fn start_session(
        &self,
        workspace: WorkspaceId,
        start: FactoryStart,
        first: Message,
    ) -> Result<SessionId, WorkerError> {
        let kept = first.clone();
        let started = self.inner.start_session(workspace, start, first);
        if started.is_ok() {
            self.keep(&kept);
        }
        started
    }

    fn send(
        &self,
        session: SessionId,
        message: Message,
        only_if_prompts: Option<u64>,
    ) -> Result<u64, SendRefused> {
        let kept = message.clone();
        let sent = self.inner.send(session, message, only_if_prompts);
        if sent.is_ok() {
            self.keep(&kept);
        }
        sent
    }

    fn turn(&self, session: SessionId, n: u64) -> Result<TurnText, WorkerError> {
        self.inner.turn(session, n)
    }

    fn tail(&self, terminal: TerminalId) -> Result<String, WorkerError> {
        self.inner.tail(terminal)
    }

    fn end_sessions(&self, workspace: WorkspaceId) -> Result<(), WorkerError> {
        self.inner.end_sessions(workspace)
    }

    fn archive(&self, workspace: WorkspaceId) -> Result<(), WorkerError> {
        self.inner.archive(workspace)
    }
}

/// What the factory's service needs of one daemon of the harness: the daemon's own pieces,
/// with the fake GitHub in place of `gh`.
struct FleetHost {
    me: MachineId,
    coordinator: Arc<Coordinator>,
    api: Api,
    secrets: Arc<Held>,
    github: Arc<FakeTracker>,
    clock: Arc<dyn Clock>,
    data: PathBuf,
    runtime: tokio::runtime::Handle,
    sent: Arc<Mutex<Vec<Sent>>>,
}

impl FleetHost {
    fn token(&self, login: &str) -> Option<String> {
        foundryd::secrets::github_token(&*self.secrets, login)
            .ok()
            .flatten()
    }
}

impl Host for FleetHost {
    fn acting(&self) -> Answering<'_, Option<bool>> {
        Box::pin(async move {
            self.coordinator
                .acting()
                .await
                .ok()
                .map(|acting| acting == self.me)
        })
    }

    fn tracker(&self, login: &str) -> Option<Arc<dyn Commenting>> {
        self.token(login)?;
        let github: Arc<dyn Tracker> = self.github.clone();
        Some(Arc::new(Hiding::new(github, self.hide())))
    }

    fn token_mark(&self, login: &str) -> Result<Option<u64>, String> {
        use std::hash::{Hash as _, Hasher as _};
        Ok(self.token(login).map(|token| {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            token.hash(&mut hasher);
            hasher.finish()
        }))
    }

    fn lease(&self, watched: &Watched) -> Arc<dyn Leasing> {
        let auth = GitAuth::for_login(&*self.secrets, &watched.login, &self.data).ok();
        Arc::new(Lease::new(
            watched.project,
            watched.remote_url(),
            auth,
            self.clock.clone(),
            self.me,
            self.data.join("leases").join(watched.project.to_string()),
        ))
    }

    fn worker(&self) -> Arc<dyn Worker> {
        Arc::new(Recording {
            inner: foundryd::factory::fleet::FleetWorker::new(
                self.api.clone(),
                self.runtime.clone(),
            ),
            sent: self.sent.clone(),
        })
    }

    fn online(&self) -> BTreeSet<MachineId> {
        self.api.peers.online()
    }

    fn hide(&self) -> Hide {
        let secrets = self.secrets.clone();
        Arc::new(move |text: &str| foundryd::secrets::hide_known(text, &*secrets))
    }
}

/// One daemon's API, sessions and factory service, beside its store and peers in the fleet.
pub struct Machine {
    data: TempDir,
    servers: Vec<JoinHandle<()>>,
    _home: TempDir,
}

impl Drop for Machine {
    fn drop(&mut self) {
        use std::io::Write as _;
        for server in &self.servers {
            server.abort();
        }
        let Ok(entries) = std::fs::read_dir(self.data.path().join("terminals")) else {
            return;
        };
        for entry in entries.flatten() {
            if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(entry.path()) {
                let _ = stream.write_all(&foundry_holder::protocol::Request::Close.encode());
            }
        }
    }
}

/// What a test can change of the harness it starts.
pub struct Options {
    /// The machines that hold the project login's token and the skill, so may build and gate.
    /// The coordinator, machine 0, holds the token and not the skill, so builds nothing.
    pub builders: Vec<usize>,
    /// Runs that hold a slot at once.
    pub max_runs: u32,
    /// The default branch's `.dw/config.yml`, with `{marks}` standing for [`Harness::marks`].
    pub config: String,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            builders: vec![1, 2],
            max_runs: 3,
            config: config(&[], &["test -f feature.txt"], 2),
        }
    }
}

/// The factory harness: see the module's words.
pub struct Harness {
    pub fleet: Fleet,
    /// What each daemon runs beside its store and peers, kept for the harness's life.
    _machines: Vec<Machine>,
    pub github: Arc<FakeTracker>,
    pub clock: Arc<FakeClock>,
    /// The bare repo that is the project's `origin`.
    pub origin: PathBuf,
    /// A clone of `origin` the test commits from, as a person would.
    pub work: PathBuf,
    /// A folder a session's script or a gate command writes into for the test to read.
    pub marks: PathBuf,
    scripts: PathBuf,
    /// Every message the factory gave a session, on any machine, in order.
    sent: Arc<Mutex<Vec<Sent>>>,
    ticker: JoinHandle<()>,
    _dir: TempDir,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.ticker.abort();
    }
}

impl Harness {
    /// Starts the harness with `options`, in a child process [`isolated`] started.
    pub async fn start(options: Options) -> Harness {
        let origins = PathBuf::from(std::env::var_os(ORIGINS).expect("run through `isolated`"));
        let dir = tempfile::Builder::new()
            .prefix("fh")
            .tempdir_in("/tmp")
            .unwrap();
        let (scripts, marks, work) = (
            dir.path().join("scripts"),
            dir.path().join("marks"),
            dir.path().join("work"),
        );
        std::fs::create_dir_all(&scripts).unwrap();
        std::fs::create_dir_all(&marks).unwrap();
        let (owner, name) = REPO.split_once('/').unwrap();
        std::fs::create_dir_all(origins.join(owner)).unwrap();
        let origin = origins.join(owner).join(format!("{name}.git"));
        git(
            &origins.join(owner),
            &["init", "-q", "--bare", &format!("{name}.git")],
        );
        git(
            dir.path(),
            &[
                "clone",
                "-q",
                &format!("https://github.com/{REPO}.git"),
                "work",
            ],
        );
        std::fs::write(work.join("README"), "widgets\n").unwrap();
        std::fs::create_dir_all(work.join(".dw")).unwrap();
        let config = options
            .config
            .replace("{marks}", &marks.display().to_string());
        std::fs::write(work.join(".dw/config.yml"), config).unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "-q", "-m", "first"]);
        git(&work, &["push", "-q", "origin", "HEAD:main"]);

        let github = Arc::new(FakeTracker::new(REPO, LOGIN, origin.clone()));
        let clock = Arc::new(FakeClock::new());
        let sent: Arc<Mutex<Vec<Sent>>> = Arc::default();
        let fleet = Fleet::start_coordinated(3);
        let project = ProjectId::random();
        // The coordinators edit the settings too as they start, so the project and the
        // factory settings are laid over the newest copy any daemon holds, again until every
        // daemon holds them.
        let factory = FactorySettings {
            poll_seconds: 10,
            feedback_seconds: 10,
            max_runs: options.max_runs,
            max_gates: 2,
            model: "test-model".into(),
            effort: "high".into(),
            ..FactorySettings::default()
        };
        let on = ProjectFactory {
            on: true,
            login: LOGIN.into(),
            paused: false,
        };
        let limits = Limits {
            max_sessions: 4,
            max_load_per_core: 100.0,
            min_free_disk: 0,
        };
        let ids: Vec<MachineId> = fleet.daemons.iter().map(|d| d.id).collect();
        let holds = |settings: &foundry_proto::settings::SharedSettings| {
            settings.projects.iter().any(|p| p.id == project)
                && settings.project_factory.get(&project) == Some(&on)
                && settings.factory.as_ref() == Some(&factory)
                && ids
                    .iter()
                    .all(|id| settings.limits.get(id) == Some(&limits))
        };
        until(WAIT, "every daemon holds the project", || {
            let held: Vec<_> = fleet.daemons.iter().map(Daemon::settings).collect();
            if held.iter().all(holds) {
                return true;
            }
            let mut settings = held.into_iter().max_by_key(|s| s.seq).unwrap();
            settings.seq += 1;
            if !settings.projects.iter().any(|p| p.id == project) {
                settings.projects.push(Project {
                    id: project,
                    name: "widgets".into(),
                    repo: format!("https://github.com/{REPO}.git"),
                });
            }
            settings.project_factory.insert(project, on.clone());
            settings.factory = Some(factory.clone());
            for id in &ids {
                settings.limits.insert(*id, limits.clone());
            }
            for daemon in &fleet.daemons {
                lock(&daemon.store)
                    .apply_settings(settings.clone())
                    .unwrap();
            }
            false
        })
        .await;
        let mut machines = Vec::new();
        for (i, daemon) in fleet.daemons.iter().enumerate() {
            let builds = options.builders.contains(&i);
            let machine = Machine::start(
                daemon,
                Held::new(i == 0 || builds),
                &scripts,
                &marks,
                github.clone(),
                clock.clone(),
                sent.clone(),
                None,
            )
            .await;
            write_held(daemon, i == 0 || builds, builds);
            daemon.write(sample(0));
            machines.push(machine);
        }
        until(WAIT, "every daemon hears every other", || {
            fleet.daemons.iter().all(|d| {
                ids.iter()
                    .all(|id| *id == d.id || d.peers.online().contains(id))
            })
        })
        .await;
        let ticking = clock.clone();
        let ticker = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(50)).await;
                ticking.advance(Duration::from_millis(500));
            }
        });
        Harness {
            fleet,
            _machines: machines,
            github,
            clock,
            origin,
            work,
            marks,
            scripts,
            sent,
            ticker,
            _dir: dir,
        }
    }

    /// The next session of issue `issue` runs `script`, a shell script with the helpers
    /// [`SHIM`] names.
    pub fn script(&self, issue: u64, script: &str) {
        let dir = self.scripts.join(issue.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        let k = (1..)
            .find(|k| !dir.join(format!("{k}.sh")).exists())
            .unwrap();
        // Whole or not there: a session may look for it at any time.
        let partial = dir.join(format!("{k}.partial"));
        std::fs::write(&partial, script).unwrap();
        std::fs::rename(partial, dir.join(format!("{k}.sh"))).unwrap();
    }

    /// Opens issue `number` and has the instructor label it `factory:ready`.
    pub fn ready(&self, number: u64, title: &str) {
        self.github.issue(number, title, "Make the widgets better.");
        self.github.label(number, "factory:ready", INSTRUCTOR);
    }

    /// The factory labels issue `number` has.
    pub fn state(&self, number: u64) -> BTreeSet<String> {
        github::factory_labels(&self.github.labels(number))
    }

    /// Waits until issue `number` has only `label` of the factory's labels.
    pub async fn reaches(&self, number: u64, label: &str) {
        eprintln!("fleet: waiting for #{number} to reach {label}");
        let want = BTreeSet::from([label.to_owned()]);
        let mut said = BTreeSet::new();
        let start = tokio::time::Instant::now();
        loop {
            let now = self.state(number);
            if now == want {
                return;
            }
            if now != said {
                eprintln!("fleet: #{number} is {now:?}");
                said = now;
            }
            assert!(
                start.elapsed() <= WAIT,
                "issue #{number} reaches {label}: not done within {WAIT:?}; it is {said:?}, with the comments {:#?}",
                self.github.comments_on(number)
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// The factory's clock now.
    pub fn clock_now(&self) -> Duration {
        self.clock.now()
    }

    /// Every message the factory gave a session so far.
    pub fn sent(&self) -> Vec<Sent> {
        lock(&self.sent).clone()
    }

    /// The events of attempt `attempt` of issue `issue`, in order, as the coordinator holds them.
    pub fn events(&self, issue: u64, attempt: u32) -> Vec<RunEvent> {
        let run = RunId::new(REPO, issue, attempt).unwrap();
        lock(&self.fleet.daemons[0].store).run_events(&run).unwrap()
    }

    /// Every workspace record named `name` that any daemon holds, archived ones too.
    pub fn workspaces_named(&self, name: &str) -> Vec<Record<Workspace>> {
        let mut found: BTreeMap<WorkspaceId, Record<Workspace>> = BTreeMap::new();
        for daemon in &self.fleet.daemons {
            for record in all(daemon) {
                if let AnyRecord::Workspace(w) = record
                    && w.body.name == name
                {
                    let newer = found
                        .get(&w.body.id)
                        .is_none_or(|held| held.version < w.version);
                    if newer {
                        found.insert(w.body.id, w);
                    }
                }
            }
        }
        found.into_values().collect()
    }

    /// Every session record of `workspace` that any daemon holds, the newest of each.
    pub fn sessions_in(&self, workspace: WorkspaceId) -> Vec<Record<Session>> {
        let mut found: BTreeMap<SessionId, Record<Session>> = BTreeMap::new();
        for daemon in &self.fleet.daemons {
            for record in all(daemon) {
                if let AnyRecord::Session(s) = record
                    && s.body.workspace == workspace
                {
                    let newer = found
                        .get(&s.body.id)
                        .is_none_or(|held| held.version < s.version);
                    if newer {
                        found.insert(s.body.id, s);
                    }
                }
            }
        }
        found.into_values().collect()
    }

    /// The machine that owns the workspace named `name`, the first one made.
    pub fn owner_of(&self, name: &str) -> Option<MachineId> {
        self.workspaces_named(name).first().map(|w| w.owner)
    }

    /// Waits until a workspace named `name` is made, and answers it.
    pub async fn made(&self, name: &str) -> Record<Workspace> {
        eprintln!("fleet: waiting for {name} to be made");
        until(WAIT, &format!("a workspace named {name} is made"), || {
            !self.workspaces_named(name).is_empty()
        })
        .await;
        self.workspaces_named(name).remove(0)
    }

    /// Commits `file` with `text` on branch `branch` of `origin`, as a person would, and
    /// answers the commit's id.
    pub fn push_to(&self, branch: &str, file: &str, text: &str) -> String {
        eprintln!("fleet: pushing {file} to {branch}");
        git(&self.work, &["fetch", "-q", "origin"]);
        git(
            &self.work,
            &["checkout", "-q", "-B", branch, &format!("origin/{branch}")],
        );
        std::fs::write(self.work.join(file), text).unwrap();
        git(&self.work, &["add", "--", file]);
        git(&self.work, &["commit", "-q", "-m", &format!("edit {file}")]);
        git(
            &self.work,
            &["push", "-q", "origin", &format!("HEAD:{branch}")],
        );
        git(&self.work, &["rev-parse", "HEAD"])
    }

    /// Asserts that the fake GitHub was asked nothing it does not know.
    pub fn no_unknown_calls(&self) {
        assert_eq!(self.github.unknown(), Vec::<String>::new());
    }
}

/// Whether `session` is live: not ended, failed or deleted.
pub fn live(session: &Record<Session>) -> bool {
    !session.deleted
        && !matches!(
            session.body.state,
            SessionState::Finished | SessionState::Failed
        )
}

/// Every record `daemon` holds, from every owner.
fn all(daemon: &Daemon) -> Vec<AnyRecord> {
    let store = lock(&daemon.store);
    let owners: Vec<MachineId> = store.versions().unwrap().into_keys().collect();
    owners
        .into_iter()
        .flat_map(|owner| store.since(owner, 0).unwrap())
        .collect()
}

/// Writes `daemon`'s [`MachineHeld`]: the project's login when it holds its `token`, and the
/// skill when it `builds`.
fn write_held(daemon: &Daemon, token: bool, builds: bool) {
    let mut store = lock(&daemon.store);
    let version = store.own_next_factory_version().unwrap();
    let record = FactoryRecord {
        owner: daemon.id,
        version,
        body: FactoryBody::Held(MachineHeld {
            logins: if token {
                vec![LOGIN.into()]
            } else {
                Vec::new()
            },
            skill: builds,
        }),
    };
    assert_eq!(
        store.apply_factory(&record, daemon.id).unwrap(),
        Applied::Stored
    );
}

impl Machine {
    /// The API, sessions and factory service of `daemon`, holding `secrets`, beside the cut-off,
    /// as `foundryd` runs them. `earlier` is the data folder and home a daemon before it used,
    /// for a restart: its terminals are reattached, its workspaces swept and its sessions
    /// adopted before anything is served, in `foundryd`'s order.
    #[allow(clippy::too_many_arguments)]
    async fn start(
        daemon: &Daemon,
        secrets: Arc<Held>,
        scripts: &Path,
        marks: &Path,
        github: Arc<FakeTracker>,
        clock: Arc<FakeClock>,
        sent: Arc<Mutex<Vec<Sent>>>,
        earlier: Option<(TempDir, TempDir)>,
    ) -> Machine {
        let (me, store, peers) = (daemon.id, daemon.store.clone(), daemon.peers.clone());
        let restarted = earlier.is_some();
        let (data, home) = earlier.unwrap_or_else(|| {
            // A Unix socket path is at most about 100 bytes.
            let data = tempfile::Builder::new()
                .prefix("fm")
                .tempdir_in("/tmp")
                .unwrap();
            (data, tempfile::tempdir().unwrap())
        });
        let agent = built("foundry-fake-agent");
        let claude: Vec<String> = [
            "sh".into(),
            "-c".into(),
            SHIM.into(),
            "sh".into(),
            agent.display().to_string(),
            scripts.display().to_string(),
            marks.display().to_string(),
        ]
        .to_vec();
        let terminals = Arc::new(Terminals::new(
            me,
            store.clone(),
            data.path(),
            built("foundry-holder"),
        ));
        if restarted {
            terminals.reattach_all().await.unwrap();
        }
        let held: Arc<dyn Secrets> = secrets.clone();
        let workspaces = Arc::new(
            Workspaces::new(me, store.clone(), data.path(), terminals.clone())
                .with_secrets(held.clone()),
        );
        if restarted {
            workspaces.sweep().await.unwrap();
        }
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
            .with_secrets(held),
        );
        if restarted {
            sessions.adopt().await;
        }
        let hook_router = sessions.router();
        let api = Api::new(
            store,
            peers.clone(),
            terminals,
            workspaces,
            sessions,
            Vec::new(),
        );
        let listener = api::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let addr = listener.local_addr().unwrap();
        peers.serve_calls(addr);
        let freshness = Arc::new(Freshness::new(clock.clone()));
        peers.hear_statuses(freshness.clone());
        let host = Arc::new(FleetHost {
            me,
            coordinator: daemon.coordinator(),
            api: api.clone(),
            secrets: secrets.clone(),
            github,
            clock: clock.clone(),
            data: data.path().to_path_buf(),
            runtime: tokio::runtime::Handle::current(),
            sent,
        });
        let clock = clock as Arc<dyn Clock>;
        let factory = Factory::new(
            daemon.store.clone(),
            me,
            host,
            freshness.clone(),
            clock.clone(),
        );
        let (placing, served) = (api.clone(), api.clone());
        let servers = vec![
            tokio::spawn(foundryd::placement::run(placing)),
            tokio::spawn(async move {
                axum::serve(
                    hooks,
                    hook_router.into_make_service_with_connect_info::<SocketAddr>(),
                )
                .await
                .unwrap();
            }),
            tokio::spawn(async move {
                axum::serve(listener, api::router(served)).await.unwrap();
            }),
            tokio::spawn(factory.serve()),
            tokio::spawn(foundryd::factory::cutoff::run(
                daemon.store.clone(),
                me,
                api.sessions.clone(),
                freshness,
                clock,
            )),
        ];
        Machine {
            data,
            servers,
            _home: home,
        }
    }
}
