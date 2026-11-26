//! The factory: the `Factory` handle and its service, run by the acting coordinator for each
//! project whose lease it holds.
//!
//! [`Factory::run`] is the daemon's service. While this machine acts as coordinator and speaks
//! protocol 5 ([`FACTORY_SINCE`]), each project whose factory is on gets two tasks of its own:
//!
//! - its lease task, which ticks the project's [`Lease`] every [`LEASE_TICK`] on a blocking
//!   thread, given the highest generation of the project's statuses and run events (a tick
//!   waits while either cannot be read), keeps what the tick said for the pass, and calls
//!   [`Freshness::held`] on the daemon's one freshness each time a tick answers that this
//!   machine holds the lease. The
//!   lease gives no word of its pushes, and it answers held only while less than
//!   [`lease::ACT_WITHIN`] has passed since the start of its last successful push, renewed every
//!   [`lease::RENEW_AFTER`]: so `held` is called on each successful push, and on the ticks that
//!   follow it while the hold lasts, which puts the cut-off's count at most two minutes behind
//!   the push, well inside the five minutes between the cut-off and a resume elsewhere;
//! - its pass task, which runs [`pass::run`] every `poll_seconds` on a blocking thread, with a
//!   fleet view read from the store, GitHub as the project's login through `gh`, and the
//!   [`FleetWorker`](fleet::FleetWorker).
//!
//! The projects run apart, so a slow `gh` in one project holds up no other: each pass and each
//! tick waits only on its own project's calls. They share the fleet's slots through one
//! [`pass::Book`].
//!
//! Every [`SCAN`] the service looks again at whether this machine acts and which projects are
//! on. A project that is turned off, whose login or repo changed, or a machine that stops
//! acting has its tasks stopped and its lease released, so the next coordinator takes over
//! after the wait; a scan that cannot read either changes nothing. A project started again runs
//! no pass until its old pass has returned. A project whose repo is not a GitHub repo is not
//! run, and says so in the log.
//!
//! [`Host`] is what the service needs of the daemon, so it is tested with fakes; [`Daemon`] is
//! the real one.

pub mod config;
pub mod cutoff;
pub mod fleet;
pub mod gate;
pub mod github;
pub mod held;
pub mod lease;
pub mod ledger;
pub mod lifecycle;
pub mod messages;
pub mod pass;
pub mod placing;
pub mod runs;
pub mod status;
pub mod text;
pub mod worker;

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use foundry_proto::factory::FactorySettings;
use foundry_proto::ids::{MachineId, ProjectId};
use foundry_proto::messages::{FACTORY_SINCE, PROTOCOL};
use foundry_proto::settings::SharedSettings;

use crate::api::Api;
use crate::clock::Clock;
use crate::coordinator::Coordinator;
use crate::secrets::Secrets;
use crate::store::Store;
use github::Commenting;
use lease::{Lease, LeaseState};
use ledger::Hide;
use status::Freshness;
use worker::{Guard, Worker};

/// How often a project's lease ticks.
pub const LEASE_TICK: Duration = Duration::from_secs(10);

/// How often the service looks again at whether this machine acts and which projects are on.
pub const SCAN: Duration = Duration::from_secs(10);

/// How often a wait reads the clock. It only paces the reads: every wait is measured on the
/// clock that counts through sleep.
const POLL: Duration = Duration::from_millis(100);

/// One project whose factory is on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Watched {
    pub project: ProjectId,
    /// `owner/name`.
    pub repo: String,
    /// The login the factory acts as there.
    pub login: String,
}

impl Watched {
    /// The URL the lease fetches and pushes, over https so the login's token answers for it.
    pub fn remote_url(&self) -> String {
        format!("https://github.com/{}.git", self.repo)
    }
}

/// The `owner/name` of a GitHub repo's clone URL: `https://github.com/o/r(.git)`,
/// `git@github.com:o/r(.git)` or `ssh://git@github.com/o/r(.git)`. `None` for any other.
pub fn github_repo(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("git@github.com:"))
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))?;
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let (owner, name) = rest.split_once('/')?;
    let plain = |part: &str| {
        !part.is_empty()
            && !part.starts_with(['-', '.'])
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    (plain(owner) && plain(name)).then(|| format!("{owner}/{name}"))
}

/// The projects whose factory is on and whose repo is on GitHub, with a login. A repo that
/// more than one of them names is watched from none of them: the lease is one ref of the repo,
/// and two passes over the same runs would claim the same issues twice. The service writes each
/// of them a status that says so.
pub fn watched(settings: &SharedSettings) -> Vec<Watched> {
    watched_and_shared(settings).0
}

/// [`watched`], with the projects left out because another one names their repo, each with the
/// names of the other projects that name it.
fn watched_and_shared(
    settings: &SharedSettings,
) -> (Vec<Watched>, BTreeMap<ProjectId, Vec<String>>) {
    let mut found = Vec::new();
    for project in &settings.projects {
        let Some(factory) = settings.project_factory.get(&project.id) else {
            continue;
        };
        if !factory.on || !crate::secrets::is_login(&factory.login) {
            continue;
        }
        let Some(repo) = github_repo(&project.repo) else {
            tracing::warn!(project = %project.id, "factory: the project's repo is not a GitHub repo, so its factory does not run");
            continue;
        };
        found.push(Watched {
            project: project.id,
            repo,
            login: factory.login.clone(),
        });
    }
    // Repos are compared as a run id folds them: GitHub names are case-insensitive.
    let folded = |watched: &Watched| watched.repo.to_ascii_lowercase();
    let mut count: BTreeMap<String, usize> = BTreeMap::new();
    for watched in &found {
        *count.entry(folded(watched)).or_default() += 1;
    }
    let (alone, shared): (Vec<Watched>, Vec<Watched>) =
        found.into_iter().partition(|w| count[&folded(w)] == 1);
    let name = |id: ProjectId| {
        settings
            .projects
            .iter()
            .find(|p| p.id == id)
            .map_or_else(|| id.to_string(), |p| p.name.clone())
    };
    let left_out = shared
        .iter()
        .map(|w| {
            let others = shared
                .iter()
                .filter(|o| o.project != w.project && folded(o) == folded(w))
                .map(|o| name(o.project))
                .collect();
            (w.project, others)
        })
        .collect();
    (alone, left_out)
}

/// Why the factory is idle for a project left out because the projects `others` name its repo.
fn shared_reason(others: &[String]) -> String {
    match others {
        [one] => format!(
            "the repo of this project is also named by {one}, a project whose factory is on, so the factory watches neither"
        ),
        _ => format!(
            "the repo of this project is also named by {}, projects whose factories are on, so the factory watches none of them",
            others.join(", ")
        ),
    }
}

/// Writes the status of a project left out because other projects name its repo, unless this
/// machine's own status for it already says `reason` and does not hold: not holding, with
/// `reason` as the idle text, in the shape a pass writes for a lease it cannot read. It carries
/// the pass and generation of this machine's stored status, so the count never falls.
fn write_shared_status(
    store: &Mutex<Store>,
    me: MachineId,
    project: ProjectId,
    reason: String,
) -> Result<(), String> {
    let mut store = lock(store);
    let own = store
        .statuses(project)
        .map_err(|error| format!("the status could not be read: {error}"))?
        .into_iter()
        .find(|(writer, _)| *writer == me)
        .map(|(_, status)| status);
    if own
        .as_ref()
        .is_some_and(|s| !s.holds && s.not_holding.is_none() && s.idle.as_deref() == Some(&reason))
    {
        return Ok(());
    }
    let mut status = pass::idle_not_holding(project, reason);
    if let Some(own) = own {
        status.pass = own.pass;
        status.generation = own.generation;
    }
    pass::store_status(&mut store, me, status)
}

/// A project's lease, as the service uses it.
pub trait Leasing: Send + Sync {
    /// Ticks the lease: see [`Lease::tick`]. Blocking.
    fn tick(&self, highest_seen_generation: u64) -> LeaseState;
    /// The guard of the lease.
    fn guard(&self) -> Arc<dyn Guard>;
    /// Stops acting for good.
    fn release(&self);
}

impl Leasing for Lease {
    fn tick(&self, highest_seen_generation: u64) -> LeaseState {
        Lease::tick(self, highest_seen_generation)
    }

    fn guard(&self) -> Arc<dyn Guard> {
        Arc::new(Lease::guard(self))
    }

    fn release(&self) {
        Lease::release(self);
    }
}

/// A future the host answers with.
pub type Answering<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What the service needs of the daemon.
pub trait Host: Send + Sync + 'static {
    /// Whether this machine acts as coordinator now; `None` when that cannot be read, which
    /// changes nothing.
    fn acting(&self) -> Answering<'_, Option<bool>>;
    /// GitHub as `login`, with the machine's hider; `None` when no token is held for it.
    fn tracker(&self, login: &str) -> Option<Arc<dyn Commenting>>;
    /// A fingerprint of the token held for `login` now, never the token: a lease made with
    /// another one, or with none, is made again. `None` when none is held; an error when the
    /// secret store cannot be read, which changes nothing.
    fn token_mark(&self, login: &str) -> Result<Option<u64>, String>;
    /// The lease of `watched`.
    fn lease(&self, watched: &Watched) -> Arc<dyn Leasing>;
    /// The worker that calls the fleet.
    fn worker(&self) -> Arc<dyn Worker>;
    /// The machines heard from, this one with them.
    fn online(&self) -> BTreeSet<MachineId>;
    /// Hides the machine's known secrets in text the ledger keeps.
    fn hide(&self) -> Hide;
}

/// The factory's service.
pub struct Factory<H: Host> {
    store: Arc<Mutex<Store>>,
    me: MachineId,
    host: Arc<H>,
    freshness: Arc<Freshness>,
    clock: Arc<dyn Clock>,
    book: Arc<pass::Book>,
}

impl<H: Host> Factory<H> {
    /// The service of machine `me` over `host`, read on `clock`, which `freshness` reads too.
    pub fn new(
        store: Arc<Mutex<Store>>,
        me: MachineId,
        host: Arc<H>,
        freshness: Arc<Freshness>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            store,
            me,
            host,
            freshness,
            clock,
            book: Arc::default(),
        }
    }

    /// Runs the service for ever.
    pub async fn serve(self) {
        let mut running: BTreeMap<ProjectId, Running> = BTreeMap::new();
        let offline = Arc::new(Mutex::new(Offline::default()));
        // What each project left out for sharing a repo was last told, so the log line is
        // written when that changes, not every scan. The status is checked against the store
        // every scan.
        let mut said_shared: BTreeMap<ProjectId, String> = BTreeMap::new();
        // One lock per project, held by its pass for the whole pass. It outlives the project's
        // tasks: stopping them does not stop a pass on its blocking thread, so a project
        // started again runs no pass until the old one has returned. One per project ever
        // watched, never more.
        let mut alone: BTreeMap<ProjectId, Arc<Mutex<()>>> = BTreeMap::new();
        loop {
            let looked = self.clock.now();
            let acting = if PROTOCOL >= FACTORY_SINCE {
                self.host.acting().await
            } else {
                Some(false)
            };
            let want = if acting == Some(true) {
                let store = self.store.clone();
                match tokio::task::spawn_blocking(move || lock(&store).settings()).await {
                    Ok(Ok(settings)) => {
                        let (watched, shared) = watched_and_shared(&settings);
                        let reasons: BTreeMap<ProjectId, String> = shared
                            .iter()
                            .map(|(project, others)| (*project, shared_reason(others)))
                            .collect();
                        if reasons != said_shared && !reasons.is_empty() {
                            tracing::warn!(projects = ?reasons.keys().collect::<Vec<_>>(), "factory: projects that name one repo are watched from none of them");
                        }
                        // Every scan checks what the store holds, not what was said last: a
                        // pass still running on its blocking thread when its project became
                        // shared writes its own status after the shared one, and a failed
                        // write is tried again.
                        if !reasons.is_empty() {
                            // The hider reads the secret store, so it runs on the blocking
                            // thread with the writes.
                            let (store, me, hide) = (self.store.clone(), self.me, self.host.hide());
                            let reasons = reasons.clone();
                            let written = tokio::task::spawn_blocking(move || {
                                reasons
                                    .into_iter()
                                    .map(|(project, reason)| {
                                        let done =
                                            write_shared_status(&store, me, project, hide(&reason));
                                        (project, done)
                                    })
                                    .collect::<Vec<_>>()
                            })
                            .await
                            .unwrap_or_default();
                            for (project, done) in written {
                                if let Err(error) = done {
                                    tracing::warn!(%project, "factory: {error}");
                                }
                            }
                        }
                        said_shared = reasons;
                        Some(watched)
                    }
                    Ok(Err(error)) => {
                        tracing::warn!("factory: the settings could not be read: {error}");
                        None
                    }
                    Err(error) => {
                        tracing::warn!("factory: the settings read did not finish: {error}");
                        None
                    }
                }
            } else if acting == Some(false) {
                // A machine that does not act stops at once, whatever the store says.
                Some(Vec::new())
            } else {
                // Whether it acts could not be read: the projects go on as they are, as when
                // the settings cannot be read. A lease another machine takes stops them.
                None
            };
            if let Some(want) = want {
                // The secret store is read on a blocking thread, as the settings are.
                let logins: Vec<(ProjectId, String)> = running
                    .iter()
                    .map(|(project, run)| (*project, run.watched.login.clone()))
                    .collect();
                let host = self.host.clone();
                let marks = tokio::task::spawn_blocking(move || {
                    logins
                        .into_iter()
                        .map(|(project, login)| (project, host.token_mark(&login)))
                        .collect::<BTreeMap<_, _>>()
                })
                .await
                .unwrap_or_default();
                running.retain(|project, run| {
                    // A lease made with another token than the one held now, or with none, or
                    // whose token could not be given to git, is made again.
                    // A store that cannot be read says nothing of the token: the lease stays.
                    let same = match marks.get(project) {
                        Some(Ok(mark)) => {
                            *mark == run.mark
                                && !(mark.is_some()
                                    && matches!(*lock(&run.state), Some(LeaseState::NoToken)))
                        }
                        Some(Err(error)) => {
                            tracing::warn!(project = %project, "factory: the token could not be read: {error}");
                            true
                        }
                        None => true,
                    };
                    let keep = want.contains(&run.watched) && same;
                    if !keep {
                        run.lease.release();
                        // A project still watched (its token changed) starts again below,
                        // and its entry is seeded from its ledger there: its sessions go on.
                        if !want.iter().any(|w| w.project == *project) {
                            self.book.forget(*project);
                        }
                    }
                    keep
                });
                let starting: Vec<Watched> = want
                    .into_iter()
                    .filter(|watched| !running.contains_key(&watched.project))
                    .collect();
                for (watched, mark, lease) in self.prepare(starting).await {
                    let project = watched.project;
                    let alone = alone.entry(project).or_default().clone();
                    running.insert(
                        project,
                        self.start(watched, mark, lease, offline.clone(), alone),
                    );
                }
            }
            wait(&*self.clock, looked, SCAN).await;
        }
    }

    /// What the projects about to start need from the store and the secret store, on one
    /// blocking thread: each one's entry in the book, seeded from its ledger, so that every
    /// project about to start counts its runs before any pass of any project may claim; the
    /// fingerprint of its token; and its lease. A thread that fails starts none of them, and the
    /// next scan tries again.
    async fn prepare(
        &self,
        starting: Vec<Watched>,
    ) -> Vec<(Watched, Option<u64>, Arc<dyn Leasing>)> {
        if starting.is_empty() {
            return Vec::new();
        }
        let (store, me, host, book) = (
            self.store.clone(),
            self.me,
            self.host.clone(),
            self.book.clone(),
        );
        let prepared = tokio::task::spawn_blocking(move || {
            let ledger = ledger::Ledger::new(store, me, 0, host.hide());
            for watched in &starting {
                if let Err(error) = pass::seed(&book, &ledger, watched.project, &watched.repo) {
                    tracing::warn!(project = %watched.project, "factory: the project's runs could not be counted: {error}");
                }
            }
            starting
                .into_iter()
                .map(|watched| {
                    let mark = host.token_mark(&watched.login).ok().flatten();
                    let lease = host.lease(&watched);
                    (watched, mark, lease)
                })
                .collect()
        })
        .await;
        prepared.unwrap_or_else(|error| {
            tracing::warn!("factory: the projects could not be prepared: {error}");
            Vec::new()
        })
    }

    /// Starts the lease task and the pass task of `watched`, whose token's fingerprint is `mark`
    /// and whose lease is `lease`. Each pass holds `alone` while it runs.
    fn start(
        &self,
        watched: Watched,
        mark: Option<u64>,
        lease: Arc<dyn Leasing>,
        offline: Arc<Mutex<Offline>>,
        alone: Arc<Mutex<()>>,
    ) -> Running {
        let state: Arc<Mutex<Option<LeaseState>>> = Arc::default();
        let mut tasks = tokio::task::JoinSet::new();
        tasks.spawn(lease_task(
            self.store.clone(),
            watched.clone(),
            lease.clone(),
            state.clone(),
            self.freshness.clone(),
            self.clock.clone(),
        ));
        tasks.spawn(pass_task(
            Shared {
                store: self.store.clone(),
                me: self.me,
                host: self.host.clone(),
                book: self.book.clone(),
                clock: self.clock.clone(),
                offline,
            },
            watched.clone(),
            lease.clone(),
            state.clone(),
            alone,
        ));
        Running {
            watched,
            lease,
            mark,
            state,
            _tasks: tasks,
        }
    }
}

/// A project the service runs: dropping it stops its tasks.
struct Running {
    watched: Watched,
    lease: Arc<dyn Leasing>,
    /// The fingerprint of the token the lease was made with.
    mark: Option<u64>,
    /// What the lease's newest tick said.
    state: Arc<Mutex<Option<LeaseState>>>,
    _tasks: tokio::task::JoinSet<()>,
}

/// What a project's pass task shares with the service.
struct Shared<H: Host> {
    store: Arc<Mutex<Store>>,
    me: MachineId,
    host: Arc<H>,
    book: Arc<pass::Book>,
    clock: Arc<dyn Clock>,
    offline: Arc<Mutex<Offline>>,
}

/// Since when, on the clock, each listed machine has been offline.
#[derive(Debug, Default)]
struct Offline {
    since: BTreeMap<MachineId, Duration>,
}

impl Offline {
    /// How long each listed machine that is not `online` has been unreachable, counted from the
    /// first look that found it so.
    fn update(
        &mut self,
        settings: &SharedSettings,
        online: &BTreeSet<MachineId>,
        now: Duration,
    ) -> BTreeMap<MachineId, Duration> {
        let listed: BTreeSet<MachineId> = settings.machines.iter().map(|m| m.id).collect();
        self.since
            .retain(|machine, _| listed.contains(machine) && !online.contains(machine));
        for machine in listed.iter().filter(|m| !online.contains(m)) {
            self.since.entry(*machine).or_insert(now);
        }
        self.since
            .iter()
            .map(|(machine, since)| (*machine, now.saturating_sub(*since)))
            .collect()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Waits until `span` has passed on `clock` since `from`.
async fn wait(clock: &dyn Clock, from: Duration, span: Duration) {
    while clock.now().saturating_sub(from) < span {
        tokio::time::sleep(POLL).await;
    }
}

/// The highest lease generation the run events of `repo` were written under, or the store's
/// error: a run whose events cannot be read may hold a higher one. The store's lock is the
/// whole daemon's, so it is taken for one run at a time, never across the history.
fn highest_event_generation(
    store: &Mutex<Store>,
    repo: &str,
) -> Result<u64, crate::store::StoreError> {
    let runs = lock(store).runs_of(repo)?;
    let mut highest = 0;
    for run in runs {
        for event in lock(store).run_events(&run)? {
            highest = highest.max(event.generation);
        }
    }
    Ok(highest)
}

/// What a project's lease says before its first tick, when the generations it must be given
/// could not be read.
const GENERATIONS_UNREAD: &str =
    "the lease waits: the generations of the project's statuses and run events could not be read";

/// Ticks `lease` every [`LEASE_TICK`], for ever.
async fn lease_task(
    store: Arc<Mutex<Store>>,
    watched: Watched,
    lease: Arc<dyn Leasing>,
    state: Arc<Mutex<Option<LeaseState>>>,
    freshness: Arc<Freshness>,
    clock: Arc<dyn Clock>,
) {
    let project = watched.project;
    let mut events: Option<u64> = None;
    loop {
        let looked = clock.now();
        let (store2, lease2, repo) = (store.clone(), lease.clone(), watched.repo.clone());
        let known = events;
        // The lease is ticked only with every generation read: one it did not see could be
        // above the generation it takes. The run events are read until a read succeeds.
        let ticked = tokio::task::spawn_blocking(move || {
            let statuses = lock(&store2).highest_generation(project)?;
            let events = match known {
                Some(events) => events,
                None => highest_event_generation(&store2, &repo)?,
            };
            Ok::<_, crate::store::StoreError>((lease2.tick(statuses.max(events)), events))
        })
        .await;
        match ticked {
            Ok(Err(error)) => {
                tracing::warn!(%project, "factory: the lease did not tick: the generations could not be read: {error}");
                // Before any tick, the pass is told why the lease waits.
                lock(&state).get_or_insert(LeaseState::Unknown {
                    reason: GENERATIONS_UNREAD,
                });
            }
            Ok(Ok((tick, seen))) => {
                events = Some(seen);
                if matches!(tick, LeaseState::Held { .. }) && lease.guard().check().is_ok() {
                    freshness.held(project);
                }
                *lock(&state) = Some(tick);
            }
            Err(error) => tracing::warn!(%project, "factory: a lease tick did not finish: {error}"),
        }
        wait(&*clock, looked, LEASE_TICK).await;
    }
}

/// A tracker for a login this machine holds no token for: every call fails, and nothing is
/// called, since the pass reads such a project as holding no token.
struct NoTracker;

impl github::Tracker for NoTracker {
    fn gh(&self, _: &[String]) -> Result<String, github::TrackerError> {
        Err(github::TrackerError::Other {
            kind: github::OtherKind::NotStarted,
            message: "this machine holds no token for the project's login".into(),
        })
    }
}

/// Runs the project's pass every `poll_seconds`, once its lease has ticked, for ever.
async fn pass_task<H: Host>(
    shared: Shared<H>,
    watched: Watched,
    lease: Arc<dyn Leasing>,
    state: Arc<Mutex<Option<LeaseState>>>,
    alone: Arc<Mutex<()>>,
) {
    let shared = Arc::new(shared);
    while lock(&state).is_none() {
        tokio::time::sleep(POLL).await;
    }
    let ledger = ledger::Ledger::new(shared.store.clone(), shared.me, 0, shared.host.hide());
    // Shared with the blocking thread, so that a pass that dies outside `pass::run` (which
    // counts its own panics) still leaves the memo, and the failures it counts, as it was.
    let memo = Arc::new(Mutex::new(pass::Memo::default()));
    loop {
        let looked = shared.clock.now();
        let (shared2, watched2, lease2, state2, ledger2, memo2, alone2) = (
            shared.clone(),
            watched.clone(),
            lease.clone(),
            state.clone(),
            ledger.clone(),
            memo.clone(),
            alone.clone(),
        );
        let ran = tokio::task::spawn_blocking(move || {
            let _alone = lock(&alone2);
            let mut memo = lock(&memo2);
            let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                one_pass(&shared2, &watched2, &*lease2, &state2, &ledger2, &mut memo)
            }));
            // A pass that could not begin, or died outside `pass::run` (which counts its own
            // panics), is a failed pass too. The panic's own text is left out.
            let (poll, error) = match ran {
                Ok(Ok(poll)) => return poll,
                Ok(Err(why)) => why,
                Err(_) => (
                    Duration::from_secs(u64::from(FactorySettings::default().poll_seconds)),
                    "the pass did not finish".to_owned(),
                ),
            };
            not_begun(
                &shared2, &watched2, &*lease2, &state2, &ledger2, &mut memo, error,
            );
            poll
        })
        .await;
        let poll = match ran {
            Ok(poll) => poll,
            // The pass counted itself failed; a thread that could not even do that is logged.
            Err(error) => {
                tracing::warn!(project = %watched.project, "factory: a pass did not finish: {error}");
                Duration::from_secs(u64::from(FactorySettings::default().poll_seconds))
            }
        };
        wait(&*shared.clock, looked, poll).await;
    }
}

/// One pass of `watched`, on a blocking thread. Answers how long to wait for the next, or that
/// and why the pass could not begin.
fn one_pass<H: Host>(
    shared: &Shared<H>,
    watched: &Watched,
    lease: &dyn Leasing,
    state: &Mutex<Option<LeaseState>>,
    ledger: &ledger::Ledger,
    memo: &mut pass::Memo,
) -> Result<Duration, (Duration, String)> {
    let default = Duration::from_secs(u64::from(FactorySettings::default().poll_seconds));
    let settings = match lock(&shared.store).settings() {
        Ok(settings) => settings,
        Err(error) => return Err((default, format!("the settings could not be read: {error}"))),
    };
    let poll = settings
        .factory
        .as_ref()
        .map_or(default, |f| Duration::from_secs(u64::from(f.poll_seconds)));
    let online = shared.host.online();
    let unreachable = lock(&shared.offline).update(&settings, &online, shared.clock.now());
    let view = match worker::FleetView::read(&lock(&shared.store), online, unreachable) {
        Ok(view) => view,
        Err(error) => return Err((poll, format!("the fleet could not be read: {error}"))),
    };
    let (tracker, lease_state) = tracker_of(shared, watched, state);
    let guard = lease.guard();
    let worker = shared.host.worker();
    let project = runs::RunProject {
        id: watched.project,
        repo: watched.repo.clone(),
        login: watched.login.clone(),
    };
    let world = pass::World {
        project: &project,
        lease: &lease_state,
        guard: &*guard,
        tracker: &*tracker,
        worker: &*worker,
        view: &view,
        ledger,
        store: &shared.store,
        me: shared.me,
        shared: &settings,
        book: &shared.book,
        clock: &*shared.clock,
    };
    pass::run(&world, memo);
    Ok(poll)
}

/// GitHub as the project's login, and what the lease task's newest tick said: no token when
/// none is held.
fn tracker_of<H: Host>(
    shared: &Shared<H>,
    watched: &Watched,
    state: &Mutex<Option<LeaseState>>,
) -> (Arc<dyn Commenting>, LeaseState) {
    match shared.host.tracker(&watched.login) {
        Some(tracker) => (
            tracker,
            lock(state).clone().unwrap_or(LeaseState::Unknown {
                reason: "the lease ref has not been read",
            }),
        ),
        None => {
            let none: Arc<dyn Commenting> =
                Arc::new(github::Hiding::new(Arc::new(NoTracker), shared.host.hide()));
            (none, LeaseState::NoToken)
        }
    }
}

/// Counts and writes a pass of `watched` that could not begin, or died outside [`pass::run`],
/// as [`pass::fail`] does. It reads no fleet and calls no worker.
fn not_begun<H: Host>(
    shared: &Shared<H>,
    watched: &Watched,
    lease: &dyn Leasing,
    state: &Mutex<Option<LeaseState>>,
    ledger: &ledger::Ledger,
    memo: &mut pass::Memo,
    error: String,
) {
    let (tracker, lease_state) = tracker_of(shared, watched, state);
    let guard = lease.guard();
    let project = runs::RunProject {
        id: watched.project,
        repo: watched.repo.clone(),
        login: watched.login.clone(),
    };
    let (view, settings) = (worker::FleetView::default(), SharedSettings::default());
    let world = pass::World {
        project: &project,
        lease: &lease_state,
        guard: &*guard,
        tracker: &*tracker,
        worker: &NoWorker,
        view: &view,
        ledger,
        store: &shared.store,
        me: shared.me,
        shared: &settings,
        book: &shared.book,
        clock: &*shared.clock,
    };
    pass::fail(&world, memo, error);
}

/// The worker of a pass that did not begin: it is never called, and every call would fail.
struct NoWorker;

impl NoWorker {
    fn refused() -> worker::WorkerError {
        worker::WorkerError::Failed("the pass did not begin".into())
    }
}

impl Worker for NoWorker {
    fn create(
        &self,
        _: MachineId,
        _: worker::CreateWorkspace,
    ) -> Result<foundry_proto::ids::WorkspaceId, worker::WorkerError> {
        Err(Self::refused())
    }
    fn start_session(
        &self,
        _: foundry_proto::ids::WorkspaceId,
        _: crate::sessions::factory::FactoryStart,
        _: messages::Message,
    ) -> Result<foundry_proto::ids::SessionId, worker::WorkerError> {
        Err(Self::refused())
    }
    fn send(
        &self,
        _: foundry_proto::ids::SessionId,
        _: messages::Message,
        _: Option<u64>,
    ) -> Result<u64, crate::sessions::factory::SendRefused> {
        Err(crate::sessions::factory::SendRefused::Failed {
            reason: "the pass did not begin".into(),
            prompts: None,
        })
    }
    fn turn(
        &self,
        _: foundry_proto::ids::SessionId,
        _: u64,
    ) -> Result<crate::sessions::turns::TurnText, worker::WorkerError> {
        Err(Self::refused())
    }
    fn tail(&self, _: foundry_proto::ids::TerminalId) -> Result<String, worker::WorkerError> {
        Err(Self::refused())
    }
    fn end_sessions(&self, _: foundry_proto::ids::WorkspaceId) -> Result<(), worker::WorkerError> {
        Err(Self::refused())
    }
    fn archive(&self, _: foundry_proto::ids::WorkspaceId) -> Result<(), worker::WorkerError> {
        Err(Self::refused())
    }
}

/// The token held for `login`, trimmed, when git can be given it: none held, a blank one and one
/// with a control character, which an environment variable could not carry whole, are `None`.
/// The rule is `GitAuth::for_login`'s, whose token this module cannot read: it is copied here,
/// and a test holds the two equal.
fn usable_token(
    secrets: &dyn Secrets,
    login: &str,
) -> Result<Option<String>, crate::secrets::SecretsError> {
    let Some(token) = crate::secrets::github_token(secrets, login)? else {
        return Ok(None);
    };
    let token = token.trim();
    if token.is_empty() || token.chars().any(char::is_control) {
        return Ok(None);
    }
    Ok(Some(token.to_owned()))
}

/// A fingerprint of the token [`usable_token`] gives for `login`, never the token.
fn token_mark_in(secrets: &dyn Secrets, login: &str) -> Result<Option<u64>, String> {
    use std::hash::{Hash, Hasher};
    let token = usable_token(secrets, login).map_err(|e| e.to_string())?;
    Ok(token.map(|token| {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        token.hash(&mut hasher);
        hasher.finish()
    }))
}

/// The daemon's [`Host`].
pub struct Daemon {
    api: Api,
    coordinator: Arc<Coordinator>,
    secrets: Arc<dyn Secrets>,
    clock: Arc<dyn Clock>,
    home: PathBuf,
    runtime: tokio::runtime::Handle,
}

impl Host for Daemon {
    fn acting(&self) -> Answering<'_, Option<bool>> {
        Box::pin(async move {
            match self.coordinator.acting().await {
                Ok(acting) => Some(acting == self.api.peers.me()),
                Err(error) => {
                    tracing::warn!("factory: the acting coordinator could not be read: {error}");
                    None
                }
            }
        })
    }

    fn tracker(&self, login: &str) -> Option<Arc<dyn Commenting>> {
        let token = match usable_token(&*self.secrets, login) {
            Ok(token) => token?,
            Err(error) => {
                tracing::warn!("factory: the token for {login} could not be read: {error}");
                return None;
            }
        };
        let gh = github::Gh::new(login, token, self.clock.clone());
        Some(Arc::new(github::Hiding::new(Arc::new(gh), self.hide())))
    }

    fn token_mark(&self, login: &str) -> Result<Option<u64>, String> {
        token_mark_in(&*self.secrets, login)
    }

    fn lease(&self, watched: &Watched) -> Arc<dyn Leasing> {
        let auth = match crate::git_auth::GitAuth::for_login(
            &*self.secrets,
            &watched.login,
            &self.home,
        ) {
            Ok(auth) => Some(auth),
            Err(error) => {
                tracing::warn!(project = %watched.project, "factory: the lease has no token: {error}");
                None
            }
        };
        let dir = self
            .home
            .join("factory")
            .join("leases")
            .join(watched.project.to_string());
        Arc::new(Lease::new(
            watched.project,
            watched.remote_url(),
            auth,
            self.clock.clone(),
            self.api.peers.me(),
            dir,
        ))
    }

    fn worker(&self) -> Arc<dyn Worker> {
        Arc::new(fleet::FleetWorker::new(
            self.api.clone(),
            self.runtime.clone(),
        ))
    }

    fn online(&self) -> BTreeSet<MachineId> {
        self.api.peers.online()
    }

    fn hide(&self) -> Hide {
        let secrets = self.secrets.clone();
        Arc::new(move |text: &str| crate::secrets::hide_known(text, &*secrets))
    }
}

impl Factory<Daemon> {
    /// The daemon's factory service, for ever: see the module's words.
    pub async fn run(
        api: Api,
        coordinator: Arc<Coordinator>,
        secrets: Arc<dyn Secrets>,
        freshness: Arc<Freshness>,
        clock: Arc<dyn Clock>,
        home: PathBuf,
    ) {
        let (store, me) = (api.store.clone(), api.peers.me());
        let host = Arc::new(Daemon {
            api,
            coordinator,
            secrets,
            clock: clock.clone(),
            home,
            runtime: tokio::runtime::Handle::current(),
        });
        Factory::new(store, me, host, freshness, clock)
            .serve()
            .await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Condvar, Mutex};
    use std::time::Instant;

    use foundry_proto::factory::{
        FactoryBody, FactoryRecord, FactoryStatus, NotHolding, ProjectFactory,
    };
    use foundry_proto::settings::Project;

    use super::*;
    use crate::clock::FakeClock;
    use crate::factory::github::{Hiding, Tracker, TrackerError};
    use crate::factory::ledger::testing::open_store;
    use crate::factory::pass::testing::{Hub, LOGIN};
    use crate::factory::worker::{AlwaysHeld, FakeWorker};
    use crate::store::Applied;

    fn me() -> MachineId {
        MachineId(uuid::Uuid::from_u128(0x1))
    }

    fn project(n: u128) -> ProjectId {
        ProjectId(uuid::Uuid::from_u128(0x7000 + n))
    }

    #[test]
    fn a_github_clone_url_names_its_repo_and_any_other_names_none() {
        for url in [
            "https://github.com/rawphp/softfac",
            "https://github.com/rawphp/softfac.git",
            "https://github.com/rawphp/softfac/",
            "git@github.com:rawphp/softfac.git",
            "ssh://git@github.com/rawphp/softfac.git",
        ] {
            assert_eq!(github_repo(url).as_deref(), Some("rawphp/softfac"), "{url}");
        }
        for url in [
            "https://gitlab.com/rawphp/softfac",
            "https://github.com/rawphp",
            "https://github.com/rawphp/softfac/tree/main",
            "https://github.com/../x",
            "https://github.com/-x/y",
            "/srv/git/softfac",
            "",
        ] {
            assert_eq!(github_repo(url), None, "{url}");
        }
    }

    #[test]
    fn only_projects_whose_factory_is_on_with_a_login_and_a_github_repo_are_watched() {
        let mut settings = SharedSettings::default();
        for (n, repo) in [
            (1, "https://github.com/o/one"),
            (2, "https://github.com/o/two"),
            (3, "https://gitlab.com/o/three"),
            (4, "https://github.com/o/four"),
        ] {
            settings.projects.push(Project {
                id: project(n),
                name: format!("p{n}"),
                repo: repo.into(),
            });
        }
        let on = |login: &str| ProjectFactory {
            on: true,
            login: login.into(),
            paused: false,
        };
        settings.project_factory.insert(project(1), on("bot"));
        settings.project_factory.insert(
            project(2),
            ProjectFactory {
                on: false,
                ..on("bot")
            },
        );
        settings.project_factory.insert(project(3), on("bot"));
        settings.project_factory.insert(project(4), on(""));
        assert_eq!(
            watched(&settings),
            vec![Watched {
                project: project(1),
                repo: "o/one".into(),
                login: "bot".into(),
            }]
        );
    }

    // ----- the service over fakes -----

    /// A lease that always holds, or holds no token, and counts its releases.
    struct FakeLease {
        state: LeaseState,
        released: AtomicBool,
    }

    impl Leasing for FakeLease {
        fn tick(&self, _: u64) -> LeaseState {
            self.state.clone()
        }
        fn guard(&self) -> Arc<dyn Guard> {
            Arc::new(AlwaysHeld(1))
        }
        fn release(&self) {
            self.released.store(true, Ordering::SeqCst);
        }
    }

    /// A tracker whose every call waits until it is let go.
    struct Slow {
        inner: Arc<Hub>,
        gate: Arc<(Mutex<bool>, Condvar)>,
        entered: AtomicBool,
    }

    impl Tracker for Slow {
        fn gh(&self, args: &[String]) -> Result<String, TrackerError> {
            self.entered.store(true, Ordering::SeqCst);
            let (open, turned) = &*self.gate;
            let mut open = open.lock().unwrap();
            while !*open {
                open = turned.wait(open).unwrap();
            }
            self.inner.gh(args)
        }
    }

    struct FakeHost {
        acting: AtomicBool,
        /// Whether this machine acts cannot be read.
        acting_unread: AtomicBool,
        /// What every lease made from now on ticks, in place of the token's rule.
        ticks_as: Mutex<Option<LeaseState>>,
        trackers: Mutex<BTreeMap<String, Arc<dyn Commenting>>>,
        /// The token mark of each login; a login with a tracker and none here is marked 1.
        marks: Mutex<BTreeMap<String, u64>>,
        /// The secret store cannot be read.
        unreadable: AtomicBool,
        /// Every lease made, in order.
        made: Mutex<Vec<(ProjectId, Arc<FakeLease>)>>,
        leases: Mutex<BTreeMap<ProjectId, Arc<FakeLease>>>,
        worker: Arc<FakeWorker>,
    }

    impl Host for FakeHost {
        fn acting(&self) -> Answering<'_, Option<bool>> {
            let acting = self.acting.load(Ordering::SeqCst);
            let unread = self.acting_unread.load(Ordering::SeqCst);
            Box::pin(async move { (!unread).then_some(acting) })
        }
        fn tracker(&self, login: &str) -> Option<Arc<dyn Commenting>> {
            self.trackers.lock().unwrap().get(login).cloned()
        }
        fn token_mark(&self, login: &str) -> Result<Option<u64>, String> {
            if self.unreadable.load(Ordering::SeqCst) {
                return Err("the secret store cannot be read".into());
            }
            let held = self.trackers.lock().unwrap().contains_key(login);
            Ok(held.then(|| self.marks.lock().unwrap().get(login).copied().unwrap_or(1)))
        }
        fn lease(&self, watched: &Watched) -> Arc<dyn Leasing> {
            let held = self.token_mark(&watched.login).ok().flatten().is_some();
            let lease = Arc::new(FakeLease {
                state: self.ticks_as.lock().unwrap().clone().unwrap_or(if held {
                    LeaseState::Held { generation: 1 }
                } else {
                    LeaseState::NoToken
                }),
                released: AtomicBool::new(false),
            });
            self.made
                .lock()
                .unwrap()
                .push((watched.project, lease.clone()));
            self.leases
                .lock()
                .unwrap()
                .insert(watched.project, lease.clone());
            lease
        }
        fn worker(&self) -> Arc<dyn Worker> {
            self.worker.clone()
        }
        fn online(&self) -> BTreeSet<MachineId> {
            [me()].into_iter().collect()
        }
        fn hide(&self) -> Hide {
            Arc::new(|text: &str| text.to_owned())
        }
    }

    fn shown() -> github::Hider {
        Arc::new(|text: &str| text.to_owned())
    }

    /// Settings with a project per `(n, login)`, each on.
    fn settings_with(projects: &[(u128, &str)]) -> SharedSettings {
        let mut settings = SharedSettings {
            seq: 1,
            factory: Some(FactorySettings::default()),
            ..SharedSettings::default()
        };
        for (n, login) in projects {
            settings.projects.push(Project {
                id: project(*n),
                name: format!("p{n}"),
                // One repo each, as the fake GitHub's own for its login: projects that name
                // one repo are watched from none.
                repo: if *login == LOGIN {
                    "https://github.com/rawphp/softfac".into()
                } else {
                    format!("https://github.com/rawphp/softfac{n}")
                },
            });
            settings.project_factory.insert(
                project(*n),
                ProjectFactory {
                    on: true,
                    login: (*login).into(),
                    paused: false,
                },
            );
        }
        settings
    }

    fn status_of(store: &Arc<Mutex<Store>>, project: ProjectId) -> Option<FactoryStatus> {
        store
            .lock()
            .unwrap()
            .statuses(project)
            .unwrap()
            .into_iter()
            .find(|(writer, _)| *writer == me())
            .map(|(_, status)| status)
    }

    /// Whether `done` came true within 20 seconds.
    fn came(mut done: impl FnMut() -> bool) -> bool {
        let started = Instant::now();
        while !done() {
            if started.elapsed() > Duration::from_secs(20) {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        true
    }

    fn wait_for(what: &str, done: impl FnMut() -> bool) {
        assert!(came(done), "{what}");
    }

    /// A tracker that panics on every call.
    struct Panics;

    impl Tracker for Panics {
        fn gh(&self, _: &[String]) -> Result<String, TrackerError> {
            panic!("an answer of a shape nobody expected");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn passes_that_panic_are_counted_and_the_third_makes_the_status_idle() {
        let (_dir, store) = open_store();
        store
            .lock()
            .unwrap()
            .apply_settings(settings_with(&[(1, LOGIN)]))
            .unwrap();
        let host = fake_host();
        host.trackers.lock().unwrap().insert(
            LOGIN.into(),
            Arc::new(Hiding::new(Arc::new(Panics), shown())),
        );
        let clock = Arc::new(FakeClock::new());
        let freshness = Arc::new(Freshness::new(clock.clone()));
        let service = Factory::new(store.clone(), me(), host, freshness, clock.clone());
        let running = tokio::spawn(service.serve());
        let said = idle_said(&store, &clock).await;
        running.abort();
        let said = said.expect("three passes that panicked in a row leave the status idle");
        assert!(said.contains("3 passes in a row failed"), "{said}");
    }

    /// A run of issue 3 that holds a slot: approved and claimed.
    fn a_run_holding_a_slot(store: &Arc<Mutex<Store>>) {
        use crate::factory::ledger::events::{Cause, CauseStep, Event};
        let ledger = ledger::Ledger::new(
            store.clone(),
            me(),
            1,
            Arc::new(|text: &str| text.to_owned()),
        );
        let run = foundry_proto::factory::RunId::new("rawphp/softfac1", 3, 1).unwrap();
        let cause = Cause::new(CauseStep::Start, "test");
        let step = Event::StepChanged {
            step: "approved 2026-10-04T10:00:00Z".into(),
        };
        ledger.write(&run, &step, &cause).unwrap();
        let claimed = Event::Claimed {
            machine: me().to_string(),
            github_user: LOGIN.into(),
        };
        ledger.write(&run, &claimed, &cause).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_project_whose_pass_has_not_begun_still_counts_its_runs_and_a_restart_keeps_them() {
        let (_dir, store) = open_store();
        let mut settings = settings_with(&[(1, "slow-bot")]);
        settings.factory.as_mut().unwrap().max_runs = 1;
        store.lock().unwrap().apply_settings(settings).unwrap();
        a_run_holding_a_slot(&store);
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let slow = Arc::new(Slow {
            inner: Hub::new(),
            gate: gate.clone(),
            entered: AtomicBool::new(false),
        });
        let host = fake_host();
        host.trackers.lock().unwrap().insert(
            "slow-bot".into(),
            Arc::new(Hiding::new(slow.clone(), shown())),
        );
        let clock = Arc::new(FakeClock::new());
        let freshness = Arc::new(Freshness::new(clock.clone()));
        let service = Factory::new(store.clone(), me(), host.clone(), freshness, clock.clone());
        let book = service.book.clone();
        let running = tokio::spawn(service.serve());
        let (slow2, book2) = (slow.clone(), book.clone());
        let before_restart = tokio::task::spawn_blocking(move || {
            // The pass is parked in its first read of GitHub: it has not reached its begin.
            came(|| slow2.entered.load(Ordering::SeqCst)).then(|| book2.runs_free(1))
        })
        .await
        .unwrap();
        // The token is replaced: the project starts again, its old pass still parked.
        host.marks.lock().unwrap().insert("slow-bot".into(), 2);
        clock.advance(SCAN);
        let (host2, book2) = (host.clone(), book.clone());
        let after_restart = tokio::task::spawn_blocking(move || {
            came(|| host2.made.lock().unwrap().len() == 2).then(|| book2.runs_free(1))
        })
        .await
        .unwrap();
        {
            let (open, turned) = &*gate;
            *open.lock().unwrap() = true;
            turned.notify_all();
        }
        running.abort();
        assert_eq!(
            before_restart,
            Some(0),
            "the run its ledger holds is counted"
        );
        assert_eq!(after_restart, Some(0), "a restart does not drop it");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_slow_gh_in_one_project_does_not_hold_up_the_next() {
        let (_dir, store) = open_store();
        store
            .lock()
            .unwrap()
            .apply_settings(settings_with(&[(1, "slow-bot"), (2, LOGIN)]))
            .unwrap();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let slow = Arc::new(Slow {
            inner: Hub::new(),
            gate: gate.clone(),
            entered: AtomicBool::new(false),
        });
        let quick = Hub::new();
        quick.ready(5);
        let host = Arc::new(FakeHost {
            acting: AtomicBool::new(true),
            acting_unread: AtomicBool::new(false),
            ticks_as: Mutex::new(None),
            trackers: Mutex::new(BTreeMap::new()),
            marks: Mutex::new(BTreeMap::new()),
            unreadable: AtomicBool::new(false),
            made: Mutex::new(Vec::new()),
            leases: Mutex::new(BTreeMap::new()),
            worker: Arc::new(FakeWorker::new()),
        });
        host.trackers.lock().unwrap().insert(
            "slow-bot".into(),
            Arc::new(Hiding::new(slow.clone(), shown())),
        );
        host.trackers
            .lock()
            .unwrap()
            .insert(LOGIN.into(), Arc::new(Hiding::new(quick.clone(), shown())));
        let clock: Arc<dyn Clock> = Arc::new(FakeClock::new());
        let freshness = Arc::new(Freshness::new(clock.clone()));
        let service = Factory::new(store.clone(), me(), host.clone(), freshness, clock);
        let running = tokio::spawn(service.serve());
        let (store2, slow2) = (store.clone(), slow.clone());
        let (entered, quick_done) = tokio::task::spawn_blocking(move || {
            let entered = came(|| slow2.entered.load(Ordering::SeqCst));
            let done = came(|| status_of(&store2, project(2)).is_some_and(|status| status.holds));
            (entered, done)
        })
        .await
        .unwrap();
        let slow_done = status_of(&store, project(1)).is_some();
        // Let the slow project go before anything is asserted, so a failure cannot leave its
        // thread waiting for ever.
        {
            let (open, turned) = &*gate;
            *open.lock().unwrap() = true;
            turned.notify_all();
        }
        assert!(entered, "the slow project's gh is called");
        assert!(
            quick_done,
            "the other project's pass ends while the slow one waits"
        );
        assert!(!slow_done, "the slow project's pass waited for its gh");
        // The quick project read GitHub: its ready issue is in its queue.
        let quick_status = status_of(&store, project(2)).unwrap();
        assert_eq!(
            quick_status
                .ready
                .iter()
                .map(|w| w.issue)
                .collect::<Vec<_>>(),
            vec![5]
        );
        let store3 = store.clone();
        tokio::task::spawn_blocking(move || {
            wait_for("the slow project's pass ends once gh answers", || {
                status_of(&store3, project(1)).is_some()
            });
        })
        .await
        .unwrap();
        running.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_project_with_no_token_writes_no_token_and_a_machine_that_stops_acting_releases_its_leases()
     {
        let (_dir, store) = open_store();
        store
            .lock()
            .unwrap()
            .apply_settings(settings_with(&[(1, "nobody")]))
            .unwrap();
        let host = Arc::new(FakeHost {
            acting: AtomicBool::new(true),
            acting_unread: AtomicBool::new(false),
            ticks_as: Mutex::new(None),
            trackers: Mutex::new(BTreeMap::new()),
            marks: Mutex::new(BTreeMap::new()),
            unreadable: AtomicBool::new(false),
            made: Mutex::new(Vec::new()),
            leases: Mutex::new(BTreeMap::new()),
            worker: Arc::new(FakeWorker::new()),
        });
        let clock = Arc::new(FakeClock::new());
        let freshness = Arc::new(Freshness::new(clock.clone()));
        let service = Factory::new(
            store.clone(),
            me(),
            host.clone(),
            freshness.clone(),
            clock.clone(),
        );
        let running = tokio::spawn(service.serve());
        let store2 = store.clone();
        tokio::task::spawn_blocking(move || {
            wait_for("a no-token status", || {
                status_of(&store2, project(1)).is_some()
            });
        })
        .await
        .unwrap();
        let status = status_of(&store, project(1)).unwrap();
        assert!(!status.holds);
        assert_eq!(
            status.not_holding,
            Some(NotHolding::NoToken {
                login: "nobody".into()
            })
        );
        host.acting.store(false, Ordering::SeqCst);
        clock.advance(SCAN);
        let host2 = host.clone();
        tokio::task::spawn_blocking(move || {
            wait_for("the lease is released", || {
                host2.leases.lock().unwrap()[&project(1)]
                    .released
                    .load(Ordering::SeqCst)
            });
        })
        .await
        .unwrap();
        running.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_lease_tick_that_holds_counts_as_held_for_the_cut_off() {
        let (_dir, store) = open_store();
        store
            .lock()
            .unwrap()
            .apply_settings(settings_with(&[(1, LOGIN)]))
            .unwrap();
        let host = Arc::new(FakeHost {
            acting: AtomicBool::new(true),
            acting_unread: AtomicBool::new(false),
            ticks_as: Mutex::new(None),
            trackers: Mutex::new(BTreeMap::new()),
            marks: Mutex::new(BTreeMap::new()),
            unreadable: AtomicBool::new(false),
            made: Mutex::new(Vec::new()),
            leases: Mutex::new(BTreeMap::new()),
            worker: Arc::new(FakeWorker::new()),
        });
        host.trackers
            .lock()
            .unwrap()
            .insert(LOGIN.into(), Arc::new(Hiding::new(Hub::new(), shown())));
        let clock = Arc::new(FakeClock::new());
        let freshness = Arc::new(Freshness::new(clock.clone()));
        clock.advance(Duration::from_secs(300));
        assert_eq!(
            freshness.since_holding(project(1)),
            Duration::from_secs(300)
        );
        let service = Factory::new(
            store.clone(),
            me(),
            host.clone(),
            freshness.clone(),
            clock.clone(),
        );
        let running = tokio::spawn(service.serve());
        let fresh = freshness.clone();
        tokio::task::spawn_blocking(move || {
            wait_for("the tick that holds is counted", || {
                fresh.since_holding(project(1)) == Duration::ZERO
            });
        })
        .await
        .unwrap();
        running.abort();
    }
    fn fake_host() -> Arc<FakeHost> {
        Arc::new(FakeHost {
            acting: AtomicBool::new(true),
            acting_unread: AtomicBool::new(false),
            ticks_as: Mutex::new(None),
            trackers: Mutex::new(BTreeMap::new()),
            marks: Mutex::new(BTreeMap::new()),
            unreadable: AtomicBool::new(false),
            made: Mutex::new(Vec::new()),
            leases: Mutex::new(BTreeMap::new()),
            worker: Arc::new(FakeWorker::new()),
        })
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_lease_is_made_again_when_its_token_is_replaced_or_removed() {
        let (_dir, store) = open_store();
        store
            .lock()
            .unwrap()
            .apply_settings(settings_with(&[(1, LOGIN)]))
            .unwrap();
        let host = fake_host();
        host.trackers
            .lock()
            .unwrap()
            .insert(LOGIN.into(), Arc::new(Hiding::new(Hub::new(), shown())));
        let clock = Arc::new(FakeClock::new());
        let freshness = Arc::new(Freshness::new(clock.clone()));
        let service = Factory::new(store.clone(), me(), host.clone(), freshness, clock.clone());
        let running = tokio::spawn(service.serve());
        let made = |host: &Arc<FakeHost>| host.made.lock().unwrap().len();
        let host2 = host.clone();
        tokio::task::spawn_blocking(move || wait_for("the first lease", || made(&host2) == 1))
            .await
            .unwrap();
        // The token is replaced: the old lease is released and a new one made.
        host.marks.lock().unwrap().insert(LOGIN.into(), 2);
        clock.advance(SCAN);
        let host2 = host.clone();
        tokio::task::spawn_blocking(move || wait_for("a second lease", || made(&host2) == 2))
            .await
            .unwrap();
        assert!(
            host.made.lock().unwrap()[0]
                .1
                .released
                .load(Ordering::SeqCst)
        );
        // The token is removed: the lease is released and the new one runs no git.
        host.trackers.lock().unwrap().remove(LOGIN);
        clock.advance(SCAN);
        let host2 = host.clone();
        tokio::task::spawn_blocking(move || wait_for("a third lease", || made(&host2) == 3))
            .await
            .unwrap();
        let leases = host.made.lock().unwrap();
        assert!(leases[1].1.released.load(Ordering::SeqCst));
        assert_eq!(leases[2].1.state, LeaseState::NoToken);
        drop(leases);
        running.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_tracker_error_in_one_project_ends_its_pass_and_the_next_project_still_runs() {
        let (_dir, store) = open_store();
        store
            .lock()
            .unwrap()
            .apply_settings(settings_with(&[(1, "failing-bot"), (2, LOGIN)]))
            .unwrap();
        let failing = Hub::new();
        failing.fail("repos/", 1000);
        let quick = Hub::new();
        quick.ready(5);
        let host = fake_host();
        host.trackers.lock().unwrap().insert(
            "failing-bot".into(),
            Arc::new(Hiding::new(failing, shown())),
        );
        host.trackers
            .lock()
            .unwrap()
            .insert(LOGIN.into(), Arc::new(Hiding::new(quick, shown())));
        let clock: Arc<dyn Clock> = Arc::new(FakeClock::new());
        let freshness = Arc::new(Freshness::new(clock.clone()));
        let service = Factory::new(store.clone(), me(), host, freshness, clock);
        let running = tokio::spawn(service.serve());
        let store2 = store.clone();
        tokio::task::spawn_blocking(move || {
            wait_for("both projects wrote a status", || {
                status_of(&store2, project(1)).is_some() && status_of(&store2, project(2)).is_some()
            });
        })
        .await
        .unwrap();
        let quick_status = status_of(&store, project(2)).unwrap();
        assert!(quick_status.holds);
        assert_eq!(
            quick_status
                .ready
                .iter()
                .map(|w| w.issue)
                .collect::<Vec<_>>(),
            vec![5]
        );
        running.abort();
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_secret_store_that_cannot_be_read_leaves_the_lease_as_it_is() {
        let (_dir, store) = open_store();
        store
            .lock()
            .unwrap()
            .apply_settings(settings_with(&[(1, LOGIN)]))
            .unwrap();
        let host = fake_host();
        host.trackers
            .lock()
            .unwrap()
            .insert(LOGIN.into(), Arc::new(Hiding::new(Hub::new(), shown())));
        let clock = Arc::new(FakeClock::new());
        let freshness = Arc::new(Freshness::new(clock.clone()));
        let service = Factory::new(store.clone(), me(), host.clone(), freshness, clock.clone());
        let running = tokio::spawn(service.serve());
        let host2 = host.clone();
        tokio::task::spawn_blocking(move || {
            wait_for("the first lease", || host2.made.lock().unwrap().len() == 1)
        })
        .await
        .unwrap();
        host.unreadable.store(true, Ordering::SeqCst);
        for _ in 0..3 {
            clock.advance(SCAN);
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        let leases = host.made.lock().unwrap();
        assert_eq!(leases.len(), 1, "no lease was made again");
        assert!(!leases[0].1.released.load(Ordering::SeqCst));
        drop(leases);
        running.abort();
    }

    #[test]
    fn a_token_git_would_refuse_has_no_mark_so_its_lease_is_not_remade_every_scan() {
        let secrets = crate::secrets::MemorySecrets::default();
        secrets.set("github:bot", " ghp_ok ").unwrap();
        secrets.set("github:blank", "  ").unwrap();
        secrets.set("github:pasted", "ghp_a\rb").unwrap();
        let mark = |login: &str| token_mark_in(&secrets, login).unwrap();
        assert!(mark("bot").is_some());
        assert_eq!(mark("blank"), None);
        assert_eq!(mark("pasted"), None);
        assert_eq!(mark("nobody"), None);
        // The mark follows the token as git would take it: the trimmed text.
        secrets.set("github:other", "ghp_ok").unwrap();
        assert_eq!(mark("other"), mark("bot"));
    }

    /// `usable_token` copies `GitAuth::for_login`'s rule, which this module may not call for the
    /// token: the two give every login the same token, or both none, so a lease never ticks
    /// without a token the tracker holds, nor the other way round.
    #[test]
    fn the_token_the_tracker_uses_is_the_token_git_is_given_for_every_kind_of_token() {
        let secrets = crate::secrets::MemorySecrets::default();
        let tokens = [
            ("good", "ghp_good"),
            ("padded", "  ghp_padded \n"),
            ("blank", ""),
            ("spaces", "   "),
            ("tab", "ghp_a\tb"),
            ("newline", "ghp_a\nb"),
            ("cr", "ghp_a\rb"),
            ("nul", "ghp_a\0b"),
            ("del", "ghp_a\u{7f}b"),
            ("nel", "ghp_a\u{85}b"),
            ("unicode", "ghp_\u{e9}t\u{e9}"),
        ];
        for (login, token) in tokens {
            secrets.set(&format!("github:{login}"), token).unwrap();
        }
        let dir = tempfile::tempdir().unwrap();
        for login in tokens.iter().map(|(login, _)| *login).chain(["nobody"]) {
            let ours = usable_token(&secrets, login).unwrap();
            let gits = match crate::git_auth::GitAuth::for_login(&secrets, login, dir.path()) {
                Ok(auth) => {
                    let mut command = std::process::Command::new("git");
                    auth.apply(&mut command);
                    command
                        .get_envs()
                        .find(|(name, _)| *name == crate::git_auth::TOKEN_VAR)
                        .and_then(|(_, value)| value)
                        .map(|value| value.to_string_lossy().into_owned())
                }
                Err(crate::git_auth::NoToken::Unset { .. }) => None,
                Err(other) => panic!("{login}: {other}"),
            };
            assert_eq!(ours, gits, "{login}");
        }
    }

    #[test]
    fn projects_that_name_one_repo_are_watched_from_none_and_another_repo_still_is() {
        let mut settings = SharedSettings::default();
        for (n, repo) in [
            (1, "https://github.com/o/r"),
            (2, "git@github.com:O/R.git"),
            (3, "https://github.com/o/other"),
        ] {
            settings.projects.push(Project {
                id: project(n),
                name: format!("p{n}"),
                repo: repo.into(),
            });
            settings.project_factory.insert(
                project(n),
                ProjectFactory {
                    on: true,
                    login: "bot".into(),
                    paused: false,
                },
            );
        }
        assert_eq!(
            watched(&settings),
            vec![Watched {
                project: project(3),
                repo: "o/other".into(),
                login: "bot".into(),
            }]
        );
        // A project that is off does not make its repo shared.
        settings.project_factory.get_mut(&project(2)).unwrap().on = false;
        assert_eq!(
            watched(&settings)
                .iter()
                .map(|w| w.project)
                .collect::<Vec<_>>(),
            vec![project(1), project(3)]
        );
    }

    /// A host that counts the lease ticks of each project and the trackers asked for each
    /// login, which is one per pass, over a [`FakeHost`].
    struct Counting {
        inner: Arc<FakeHost>,
        ticks: Mutex<BTreeMap<ProjectId, Arc<AtomicUsize>>>,
        /// The highest seen generation each tick of each project was given, in order.
        given: Mutex<BTreeMap<ProjectId, Arc<Mutex<Vec<u64>>>>>,
        passes: Mutex<BTreeMap<String, usize>>,
    }

    /// A lease that counts its ticks and leaves the rest to `inner`.
    struct Ticked {
        inner: Arc<dyn Leasing>,
        ticks: Arc<AtomicUsize>,
        given: Arc<Mutex<Vec<u64>>>,
    }

    impl Leasing for Ticked {
        fn tick(&self, highest: u64) -> LeaseState {
            self.given.lock().unwrap().push(highest);
            self.ticks.fetch_add(1, Ordering::SeqCst);
            self.inner.tick(highest)
        }
        fn guard(&self) -> Arc<dyn Guard> {
            self.inner.guard()
        }
        fn release(&self) {
            self.inner.release();
        }
    }

    impl Counting {
        fn new(inner: Arc<FakeHost>) -> Arc<Self> {
            Arc::new(Self {
                inner,
                ticks: Mutex::new(BTreeMap::new()),
                given: Mutex::new(BTreeMap::new()),
                passes: Mutex::new(BTreeMap::new()),
            })
        }
        fn given(&self, project: ProjectId) -> Vec<u64> {
            self.given
                .lock()
                .unwrap()
                .get(&project)
                .map(|given| given.lock().unwrap().clone())
                .unwrap_or_default()
        }
        fn ticks(&self, project: ProjectId) -> usize {
            self.ticks
                .lock()
                .unwrap()
                .get(&project)
                .map_or(0, |n| n.load(Ordering::SeqCst))
        }
        fn passes(&self, login: &str) -> usize {
            self.passes.lock().unwrap().get(login).copied().unwrap_or(0)
        }
    }

    impl Host for Counting {
        fn acting(&self) -> Answering<'_, Option<bool>> {
            self.inner.acting()
        }
        fn tracker(&self, login: &str) -> Option<Arc<dyn Commenting>> {
            *self.passes.lock().unwrap().entry(login.into()).or_default() += 1;
            self.inner.tracker(login)
        }
        fn token_mark(&self, login: &str) -> Result<Option<u64>, String> {
            self.inner.token_mark(login)
        }
        fn lease(&self, watched: &Watched) -> Arc<dyn Leasing> {
            let ticks: Arc<AtomicUsize> = Arc::default();
            self.ticks
                .lock()
                .unwrap()
                .insert(watched.project, ticks.clone());
            let given: Arc<Mutex<Vec<u64>>> = Arc::default();
            self.given
                .lock()
                .unwrap()
                .insert(watched.project, given.clone());
            Arc::new(Ticked {
                inner: self.inner.lease(watched),
                ticks,
                given,
            })
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

    /// Lets the service run a little, so a thing that should not happen has had its chance.
    async fn a_while() {
        tokio::time::sleep(Duration::from_millis(400)).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_project_turned_off_releases_its_lease_and_another_projects_is_kept() {
        let (_dir, store) = open_store();
        store
            .lock()
            .unwrap()
            .apply_settings(settings_with(&[(1, LOGIN), (2, "other-bot")]))
            .unwrap();
        let host = fake_host();
        for login in [LOGIN, "other-bot"] {
            host.trackers
                .lock()
                .unwrap()
                .insert(login.into(), Arc::new(Hiding::new(Hub::new(), shown())));
        }
        let clock = Arc::new(FakeClock::new());
        let freshness = Arc::new(Freshness::new(clock.clone()));
        let service = Factory::new(store.clone(), me(), host.clone(), freshness, clock.clone());
        let running = tokio::spawn(service.serve());
        let host2 = host.clone();
        tokio::task::spawn_blocking(move || {
            wait_for("both leases", || host2.made.lock().unwrap().len() == 2)
        })
        .await
        .unwrap();
        let mut off = settings_with(&[(1, LOGIN), (2, "other-bot")]);
        off.seq = 2;
        off.project_factory.get_mut(&project(2)).unwrap().on = false;
        store.lock().unwrap().apply_settings(off).unwrap();
        clock.advance(SCAN);
        let host2 = host.clone();
        tokio::task::spawn_blocking(move || {
            wait_for("the turned-off project's lease is released", || {
                host2.leases.lock().unwrap()[&project(2)]
                    .released
                    .load(Ordering::SeqCst)
            })
        })
        .await
        .unwrap();
        a_while().await;
        let leases = host.leases.lock().unwrap();
        assert!(!leases[&project(1)].released.load(Ordering::SeqCst));
        drop(leases);
        assert_eq!(host.made.lock().unwrap().len(), 2, "no lease was remade");
        running.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_lease_ticks_once_every_lease_tick() {
        let (_dir, store) = open_store();
        store
            .lock()
            .unwrap()
            .apply_settings(settings_with(&[(1, LOGIN)]))
            .unwrap();
        let fake = fake_host();
        fake.trackers
            .lock()
            .unwrap()
            .insert(LOGIN.into(), Arc::new(Hiding::new(Hub::new(), shown())));
        let host = Counting::new(fake);
        let clock = Arc::new(FakeClock::new());
        let freshness = Arc::new(Freshness::new(clock.clone()));
        let service = Factory::new(store.clone(), me(), host.clone(), freshness, clock.clone());
        let running = tokio::spawn(service.serve());
        let host2 = host.clone();
        tokio::task::spawn_blocking(move || {
            wait_for("the first tick", || host2.ticks(project(1)) == 1)
        })
        .await
        .unwrap();
        clock.advance(LEASE_TICK - Duration::from_secs(1));
        a_while().await;
        assert_eq!(host.ticks(project(1)), 1, "no tick before LEASE_TICK");
        clock.advance(Duration::from_secs(1));
        let host2 = host.clone();
        tokio::task::spawn_blocking(move || {
            wait_for("a tick after LEASE_TICK", || host2.ticks(project(1)) == 2)
        })
        .await
        .unwrap();
        running.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pass_runs_every_configured_poll_seconds() {
        let (_dir, store) = open_store();
        let mut settings = settings_with(&[(1, LOGIN)]);
        // Not the default 30, so a pass that ignores the setting is seen.
        settings.factory.as_mut().unwrap().poll_seconds = 45;
        store.lock().unwrap().apply_settings(settings).unwrap();
        let fake = fake_host();
        fake.trackers
            .lock()
            .unwrap()
            .insert(LOGIN.into(), Arc::new(Hiding::new(Hub::new(), shown())));
        let host = Counting::new(fake);
        let clock = Arc::new(FakeClock::new());
        let freshness = Arc::new(Freshness::new(clock.clone()));
        let service = Factory::new(store.clone(), me(), host.clone(), freshness, clock.clone());
        let running = tokio::spawn(service.serve());
        let (host2, store2) = (host.clone(), store.clone());
        tokio::task::spawn_blocking(move || {
            wait_for("the first pass", || {
                host2.passes(LOGIN) == 1 && status_of(&store2, project(1)).is_some()
            })
        })
        .await
        .unwrap();
        clock.advance(Duration::from_secs(44));
        a_while().await;
        assert_eq!(host.passes(LOGIN), 1, "no pass before poll_seconds");
        clock.advance(Duration::from_secs(1));
        let host2 = host.clone();
        tokio::task::spawn_blocking(move || {
            wait_for("a pass after poll_seconds", || host2.passes(LOGIN) == 2)
        })
        .await
        .unwrap();
        running.abort();
    }

    /// A status of `project` by another machine, at `generation`, as its `version`.
    fn a_status_of_another_machine(
        store: &Arc<Mutex<Store>>,
        project: ProjectId,
        generation: u64,
        version: u64,
    ) {
        let other = MachineId(uuid::Uuid::from_u128(0x2));
        let mut status = status::not_holding(
            project,
            NotHolding::HeldBy {
                machine: other,
                generation,
            },
        );
        status.generation = generation;
        let record = FactoryRecord {
            owner: other,
            version,
            body: FactoryBody::Status(status),
        };
        assert_eq!(
            store.lock().unwrap().apply_factory(&record, other).unwrap(),
            Applied::Stored
        );
    }

    /// The run events in the store are at generation 7 and the statuses at 5: the first tick is
    /// given 7. A status at generation 9 comes in: a later tick is given 9.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_lease_is_given_the_highest_generation_of_the_statuses_and_the_run_events() {
        use crate::factory::ledger::events::{Cause, CauseStep, Event};
        let (_dir, store) = open_store();
        store
            .lock()
            .unwrap()
            .apply_settings(settings_with(&[(1, LOGIN)]))
            .unwrap();
        let run = foundry_proto::factory::RunId::new("rawphp/softfac", 3, 1).unwrap();
        ledger::Ledger::new(store.clone(), me(), 7, Arc::new(|t: &str| t.to_owned()))
            .write(
                &run,
                &Event::StepChanged {
                    step: "a step".into(),
                },
                &Cause::new(CauseStep::Start, "test"),
            )
            .unwrap();
        a_status_of_another_machine(&store, project(1), 5, 1);
        let fake = fake_host();
        fake.trackers
            .lock()
            .unwrap()
            .insert(LOGIN.into(), Arc::new(Hiding::new(Hub::new(), shown())));
        let host = Counting::new(fake);
        let clock = Arc::new(FakeClock::new());
        let freshness = Arc::new(Freshness::new(clock.clone()));
        let service = Factory::new(store.clone(), me(), host.clone(), freshness, clock.clone());
        let running = tokio::spawn(service.serve());
        let host2 = host.clone();
        tokio::task::spawn_blocking(move || {
            wait_for("the first tick", || host2.ticks(project(1)) == 1)
        })
        .await
        .unwrap();
        let first = host.given(project(1));
        a_status_of_another_machine(&store, project(1), 9, 2);
        clock.advance(LEASE_TICK);
        let host2 = host.clone();
        tokio::task::spawn_blocking(move || {
            wait_for("a second tick", || host2.ticks(project(1)) == 2)
        })
        .await
        .unwrap();
        running.abort();
        assert_eq!(
            first,
            vec![7],
            "the run events' generation is above the statuses'"
        );
        assert_eq!(host.given(project(1)), vec![7, 9]);
    }

    /// A run event row this build cannot read is in the project's repo: its generation is not
    /// known, so the lease is not ticked, and the pass runs on what the lease says, which is why
    /// it waits (the real lease's guard then fails, so the pass writes that as its idle text).
    /// Once the row is gone, the next tick is given the generation of the events it can read.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_lease_waits_while_a_generation_of_the_run_events_cannot_be_read() {
        use crate::factory::ledger::events::{Cause, CauseStep, Event};
        let (dir, store) = open_store();
        store
            .lock()
            .unwrap()
            .apply_settings(settings_with(&[(1, LOGIN)]))
            .unwrap();
        let run = foundry_proto::factory::RunId::new("rawphp/softfac", 3, 1).unwrap();
        ledger::Ledger::new(store.clone(), me(), 7, Arc::new(|t: &str| t.to_owned()))
            .write(
                &run,
                &Event::StepChanged {
                    step: "a step".into(),
                },
                &Cause::new(CauseStep::Start, "test"),
            )
            .unwrap();
        let newer = foundry_proto::factory::RunId::new("rawphp/softfac", 4, 1).unwrap();
        let other = MachineId(uuid::Uuid::from_u128(0x2));
        let db = dir.path().join("foundry.db");
        rusqlite::Connection::open(&db)
            .unwrap()
            .execute(
                "INSERT INTO factory_records (kind, id, owner, version, record) \
                 VALUES ('run_event', ?1, ?2, 1, ?3)",
                rusqlite::params![
                    format!("{newer}/{other}/1"),
                    other.to_string(),
                    r#"{"kind":"run_event","later":true}"#
                ],
            )
            .unwrap();
        let fake = fake_host();
        fake.trackers
            .lock()
            .unwrap()
            .insert(LOGIN.into(), Arc::new(Hiding::new(Hub::new(), shown())));
        let host = Counting::new(fake);
        let clock = Arc::new(FakeClock::new());
        let freshness = Arc::new(Freshness::new(clock.clone()));
        let service = Factory::new(store.clone(), me(), host.clone(), freshness, clock.clone());
        let running = tokio::spawn(service.serve());
        let host2 = host.clone();
        tokio::task::spawn_blocking(move || {
            wait_for("the lease", || host2.inner.made.lock().unwrap().len() == 1)
        })
        .await
        .unwrap();
        for _ in 0..3 {
            clock.advance(LEASE_TICK);
            a_while().await;
        }
        let while_unread = host.ticks(project(1));
        let host2 = host.clone();
        let passed = tokio::task::spawn_blocking(move || came(|| host2.passes(LOGIN) >= 1))
            .await
            .unwrap();
        rusqlite::Connection::open(&db)
            .unwrap()
            .execute(
                "DELETE FROM factory_records WHERE owner = ?1",
                [other.to_string()],
            )
            .unwrap();
        clock.advance(LEASE_TICK);
        let host2 = host.clone();
        tokio::task::spawn_blocking(move || {
            wait_for("a tick once the row is gone", || {
                host2.ticks(project(1)) == 1
            })
        })
        .await
        .unwrap();
        running.abort();
        assert_eq!(while_unread, 0, "no tick while a generation is unread");
        assert!(passed, "the pass does not wait for a first tick for ever");
        assert_eq!(host.given(project(1)), vec![7]);
    }

    /// A lease that ticks as held by another machine, or as holding no token, while its guard
    /// still answers: the cut-off's count since this machine held the lease is never reset.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_lease_tick_that_does_not_hold_never_counts_as_held_for_the_cut_off() {
        let other = MachineId(uuid::Uuid::from_u128(0x2));
        for ticks_as in [
            LeaseState::HeldBy {
                machine: other,
                generation: 2,
            },
            LeaseState::NoToken,
        ] {
            let (_dir, store) = open_store();
            store
                .lock()
                .unwrap()
                .apply_settings(settings_with(&[(1, LOGIN)]))
                .unwrap();
            let fake = fake_host();
            // A login with no token ticks as holding none; one with a token, as held elsewhere.
            if ticks_as != LeaseState::NoToken {
                fake.trackers
                    .lock()
                    .unwrap()
                    .insert(LOGIN.into(), Arc::new(Hiding::new(Hub::new(), shown())));
            }
            *fake.ticks_as.lock().unwrap() = Some(ticks_as.clone());
            let host = Counting::new(fake);
            let clock = Arc::new(FakeClock::new());
            let freshness = Arc::new(Freshness::new(clock.clone()));
            clock.advance(Duration::from_secs(300));
            let service = Factory::new(
                store.clone(),
                me(),
                host.clone(),
                freshness.clone(),
                clock.clone(),
            );
            let running = tokio::spawn(service.serve());
            for ticks in 1..=3 {
                let host2 = host.clone();
                tokio::task::spawn_blocking(move || {
                    wait_for("a tick", || host2.ticks(project(1)) >= ticks)
                })
                .await
                .unwrap();
                clock.advance(LEASE_TICK);
            }
            running.abort();
            assert!(
                freshness.since_holding(project(1)) >= Duration::from_secs(300),
                "{ticks_as:?}: {:?}",
                freshness.since_holding(project(1))
            );
        }
    }

    /// One scan cannot read whether this machine acts: nothing is stopped, no lease released
    /// and none made again, as when the settings cannot be read.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_read_of_whether_this_machine_acts_keeps_its_projects_running() {
        let (_dir, store) = open_store();
        store
            .lock()
            .unwrap()
            .apply_settings(settings_with(&[(1, LOGIN)]))
            .unwrap();
        let host = fake_host();
        host.trackers
            .lock()
            .unwrap()
            .insert(LOGIN.into(), Arc::new(Hiding::new(Hub::new(), shown())));
        let clock = Arc::new(FakeClock::new());
        let freshness = Arc::new(Freshness::new(clock.clone()));
        let service = Factory::new(store.clone(), me(), host.clone(), freshness, clock.clone());
        let running = tokio::spawn(service.serve());
        let host2 = host.clone();
        tokio::task::spawn_blocking(move || {
            wait_for("the lease", || host2.made.lock().unwrap().len() == 1)
        })
        .await
        .unwrap();
        host.acting_unread.store(true, Ordering::SeqCst);
        for _ in 0..3 {
            clock.advance(SCAN);
            a_while().await;
        }
        host.acting_unread.store(false, Ordering::SeqCst);
        clock.advance(SCAN);
        a_while().await;
        running.abort();
        let made = host.made.lock().unwrap();
        assert_eq!(made.len(), 1, "no lease is made again");
        assert!(
            !made[0].1.released.load(Ordering::SeqCst),
            "the lease is kept"
        );
    }

    fn listed(machines: &[MachineId]) -> SharedSettings {
        SharedSettings {
            machines: machines
                .iter()
                .map(|id| foundry_proto::settings::Machine {
                    id: *id,
                    name: id.to_string(),
                    address: String::new(),
                    key: String::new(),
                })
                .collect(),
            ..SharedSettings::default()
        }
    }

    #[test]
    fn a_listed_machine_is_unreachable_from_the_first_look_that_finds_it_offline() {
        let (a, b) = (
            MachineId(uuid::Uuid::from_u128(0xa)),
            MachineId(uuid::Uuid::from_u128(0xb)),
        );
        let settings = listed(&[a, b]);
        let mut offline = Offline::default();
        let secs = Duration::from_secs;
        let both = BTreeSet::from([a, b]);
        assert!(offline.update(&settings, &both, secs(100)).is_empty());
        // b goes offline at 200: it counts from 200, and the span grows on the clock.
        let only_a = BTreeSet::from([a]);
        assert_eq!(
            offline.update(&settings, &only_a, secs(200)),
            BTreeMap::from([(b, secs(0))])
        );
        assert_eq!(
            offline.update(&settings, &only_a, secs(500)),
            BTreeMap::from([(b, secs(300))])
        );
        // b is heard from again: its count is gone, and a later drop counts from that drop.
        assert!(offline.update(&settings, &both, secs(600)).is_empty());
        assert_eq!(
            offline.update(&settings, &only_a, secs(700)),
            BTreeMap::from([(b, secs(0))])
        );
        assert_eq!(
            offline.update(&settings, &only_a, secs(1600)),
            BTreeMap::from([(b, secs(900))])
        );
        // A machine no longer listed is dropped, and one never listed is never counted.
        let c = MachineId(uuid::Uuid::from_u128(0xc));
        assert!(
            offline
                .update(&listed(&[a]), &BTreeSet::from([a]), secs(1700))
                .is_empty()
        );
        assert_eq!(
            offline.update(&listed(&[a, b]), &BTreeSet::from([a, c]), secs(1800)),
            BTreeMap::from([(b, secs(0))]),
            "b listed again counts from now"
        );
    }

    /// Settings of projects 1 and 2 on one repo (written two ways) and 3 on another, all on.
    fn two_projects_on_one_repo_and_a_third() -> SharedSettings {
        let mut settings = settings_with(&[(1, "a-bot"), (2, "b-bot"), (3, "c-bot")]);
        settings.projects[0].repo = "https://github.com/rawphp/shared".into();
        settings.projects[1].repo = "git@github.com:RawPHP/shared.git".into();
        settings
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn projects_that_share_a_repo_each_get_a_status_that_names_the_other() {
        let (_dir, store) = open_store();
        store
            .lock()
            .unwrap()
            .apply_settings(two_projects_on_one_repo_and_a_third())
            .unwrap();
        let host = fake_host();
        for login in ["a-bot", "b-bot", "c-bot"] {
            host.trackers
                .lock()
                .unwrap()
                .insert(login.into(), Arc::new(Hiding::new(Hub::new(), shown())));
        }
        let clock = Arc::new(FakeClock::new());
        let freshness = Arc::new(Freshness::new(clock.clone()));
        let service = Factory::new(store.clone(), me(), host.clone(), freshness, clock.clone());
        let running = tokio::spawn(service.serve());
        let (store2, host2) = (store.clone(), host.clone());
        let said = tokio::task::spawn_blocking(move || {
            // The third project is watched: its pass writes its status.
            let third = came(|| status_of(&store2, project(3)).is_some());
            let both = came(|| {
                status_of(&store2, project(1)).is_some() && status_of(&store2, project(2)).is_some()
            });
            (third && both, host2.made.lock().unwrap().len())
        })
        .await
        .unwrap();
        running.abort();
        assert!(said.0, "each project has a status");
        assert_eq!(said.1, 1, "only the third project has a lease");
        for (mine, other) in [(1, "p2"), (2, "p1")] {
            let status = status_of(&store, project(mine)).unwrap();
            assert!(!status.holds, "{status:?}");
            assert_eq!(status.not_holding, None, "{status:?}");
            let idle = status.idle.clone().unwrap_or_default();
            assert!(idle.contains(other), "the other project is named: {idle}");
            assert!(idle.contains("neither"), "{idle}");
            assert!(!idle.contains(&format!("p{mine}")), "{idle}");
        }
        let third = status_of(&store, project(3)).unwrap();
        assert!(!third.idle.unwrap_or_default().contains("neither"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_status_of_a_shared_repo_is_written_once_and_not_every_scan() {
        let (_dir, store) = open_store();
        let mut settings = two_projects_on_one_repo_and_a_third();
        settings.projects.truncate(2);
        store.lock().unwrap().apply_settings(settings).unwrap();
        let host = fake_host();
        let clock = Arc::new(FakeClock::new());
        let freshness = Arc::new(Freshness::new(clock.clone()));
        let service = Factory::new(store.clone(), me(), host, freshness, clock.clone());
        let running = tokio::spawn(service.serve());
        let store2 = store.clone();
        tokio::task::spawn_blocking(move || {
            wait_for("both statuses", || {
                status_of(&store2, project(1)).is_some() && status_of(&store2, project(2)).is_some()
            })
        })
        .await
        .unwrap();
        let next =
            |store: &Arc<Mutex<Store>>| store.lock().unwrap().own_next_factory_version().unwrap();
        let before = next(&store);
        for _ in 0..3 {
            clock.advance(SCAN);
            a_while().await;
        }
        let after = next(&store);
        running.abort();
        assert_eq!(after, before + 1, "no write in three scans");
    }

    /// A run of issue 3 in `repo` that holds a slot, as `a_run_holding_a_slot` writes for project 1.
    fn a_run_holding_a_slot_in(store: &Arc<Mutex<Store>>, repo: &str, login: &str) {
        use crate::factory::ledger::events::{Cause, CauseStep, Event};
        let ledger = ledger::Ledger::new(
            store.clone(),
            me(),
            1,
            Arc::new(|text: &str| text.to_owned()),
        );
        let run = foundry_proto::factory::RunId::new(repo, 3, 1).unwrap();
        let cause = Cause::new(CauseStep::Start, "test");
        let step = Event::StepChanged {
            step: "approved 2026-10-04T10:00:00Z".into(),
        };
        ledger.write(&run, &step, &cause).unwrap();
        let claimed = Event::Claimed {
            machine: me().to_string(),
            github_user: login.into(),
        };
        ledger.write(&run, &claimed, &cause).unwrap();
    }

    /// Lets a parked tracker go.
    fn let_go(gate: &Arc<(Mutex<bool>, Condvar)>) {
        let (open, turned) = &**gate;
        *open.lock().unwrap() = true;
        turned.notify_all();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_project_turned_off_gives_the_slots_its_runs_held_back_to_the_book() {
        let (_dir, store) = open_store();
        let mut settings = settings_with(&[(1, LOGIN), (2, "slow-bot")]);
        settings.factory.as_mut().unwrap().max_runs = 1;
        store.lock().unwrap().apply_settings(settings).unwrap();
        // Project 2's repo is softfac2.
        a_run_holding_a_slot_in(&store, "rawphp/softfac2", "slow-bot");
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let slow = Arc::new(Slow {
            inner: Hub::new(),
            gate: gate.clone(),
            entered: AtomicBool::new(false),
        });
        let host = fake_host();
        host.trackers
            .lock()
            .unwrap()
            .insert(LOGIN.into(), Arc::new(Hiding::new(Hub::new(), shown())));
        host.trackers.lock().unwrap().insert(
            "slow-bot".into(),
            Arc::new(Hiding::new(slow.clone(), shown())),
        );
        let clock = Arc::new(FakeClock::new());
        let freshness = Arc::new(Freshness::new(clock.clone()));
        let service = Factory::new(store.clone(), me(), host.clone(), freshness, clock.clone());
        let book = service.book.clone();
        let running = tokio::spawn(service.serve());
        let (slow2, book2) = (slow.clone(), book.clone());
        // Project 2's pass is parked in its first read of GitHub: only its seed counts its run.
        let held = tokio::task::spawn_blocking(move || {
            came(|| slow2.entered.load(Ordering::SeqCst)).then(|| book2.runs_free(1))
        })
        .await
        .unwrap();
        let mut off = settings_with(&[(1, LOGIN), (2, "slow-bot")]);
        off.seq = 2;
        off.factory.as_mut().unwrap().max_runs = 1;
        off.project_factory.get_mut(&project(2)).unwrap().on = false;
        store.lock().unwrap().apply_settings(off).unwrap();
        clock.advance(SCAN);
        let (host2, book2) = (host.clone(), book.clone());
        let freed = tokio::task::spawn_blocking(move || {
            let released = came(|| {
                host2.leases.lock().unwrap()[&project(2)]
                    .released
                    .load(Ordering::SeqCst)
            });
            released && came(|| book2.runs_free(1) == 1)
        })
        .await
        .unwrap();
        let_go(&gate);
        running.abort();
        assert_eq!(held, Some(0), "the run its ledger holds is counted");
        assert!(freed, "the slot comes back once the project is turned off");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_restart_whose_seed_fails_keeps_what_the_book_held() {
        let (dir, store) = open_store();
        let mut settings = settings_with(&[(1, "slow-bot")]);
        settings.factory.as_mut().unwrap().max_runs = 1;
        store.lock().unwrap().apply_settings(settings).unwrap();
        a_run_holding_a_slot(&store);
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let slow = Arc::new(Slow {
            inner: Hub::new(),
            gate: gate.clone(),
            entered: AtomicBool::new(false),
        });
        let host = fake_host();
        host.trackers.lock().unwrap().insert(
            "slow-bot".into(),
            Arc::new(Hiding::new(slow.clone(), shown())),
        );
        let clock = Arc::new(FakeClock::new());
        let freshness = Arc::new(Freshness::new(clock.clone()));
        let service = Factory::new(store.clone(), me(), host.clone(), freshness, clock.clone());
        let book = service.book.clone();
        let running = tokio::spawn(service.serve());
        let slow2 = slow.clone();
        let entered =
            tokio::task::spawn_blocking(move || came(|| slow2.entered.load(Ordering::SeqCst)))
                .await
                .unwrap();
        // A row no run id parses in the project's repo: the next count of its runs fails.
        rusqlite::Connection::open(dir.path().join("foundry.db"))
            .unwrap()
            .execute(
                "INSERT INTO factory_records (kind, id, owner, version, record)
                 VALUES ('run_event', 'rawphp__softfac1__bad/x/1', 'x', 1, '{}')",
                [],
            )
            .unwrap();
        host.marks.lock().unwrap().insert("slow-bot".into(), 2);
        clock.advance(SCAN);
        let (host2, book2) = (host.clone(), book.clone());
        let after_restart = tokio::task::spawn_blocking(move || {
            came(|| host2.made.lock().unwrap().len() == 2).then(|| book2.runs_free(1))
        })
        .await
        .unwrap();
        let_go(&gate);
        running.abort();
        assert!(entered, "the first pass is under way");
        assert_eq!(
            after_restart,
            Some(0),
            "the old entry still counts when the new count fails"
        );
    }

    /// A tracker that refuses every call while it is down, and otherwise answers as `inner`.
    struct Flaky {
        inner: Arc<Hub>,
        down: AtomicBool,
        refused: AtomicUsize,
    }

    impl Tracker for Flaky {
        fn gh(&self, args: &[String]) -> Result<String, TrackerError> {
            if self.down.load(Ordering::SeqCst) {
                self.refused.fetch_add(1, Ordering::SeqCst);
                return Err(TrackerError::Other {
                    kind: github::OtherKind::NotStarted,
                    message: "down".into(),
                });
            }
            self.inner.gh(args)
        }
    }

    /// A host whose worker panics once when armed, then answers; or every time, when `always`.
    struct DiesOnce {
        inner: Arc<FakeHost>,
        armed: AtomicBool,
        fired: AtomicBool,
        always: bool,
    }

    impl Host for DiesOnce {
        fn acting(&self) -> Answering<'_, Option<bool>> {
            self.inner.acting()
        }
        fn tracker(&self, login: &str) -> Option<Arc<dyn Commenting>> {
            self.inner.tracker(login)
        }
        fn token_mark(&self, login: &str) -> Result<Option<u64>, String> {
            self.inner.token_mark(login)
        }
        fn lease(&self, watched: &Watched) -> Arc<dyn Leasing> {
            self.inner.lease(watched)
        }
        fn worker(&self) -> Arc<dyn Worker> {
            if self.armed.swap(false, Ordering::SeqCst) || self.always {
                self.fired.store(true, Ordering::SeqCst);
                panic!("the host could not make a worker");
            }
            self.inner.worker()
        }
        fn online(&self) -> BTreeSet<MachineId> {
            self.inner.online()
        }
        fn hide(&self) -> Hide {
            self.inner.hide()
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_pass_that_dies_outside_the_pass_keeps_the_memo_for_the_next() {
        let (_dir, store) = open_store();
        store
            .lock()
            .unwrap()
            .apply_settings(settings_with(&[(1, LOGIN)]))
            .unwrap();
        let hub = Hub::new();
        hub.ready(5);
        let flaky = Arc::new(Flaky {
            inner: hub,
            down: AtomicBool::new(false),
            refused: AtomicUsize::new(0),
        });
        let fake = fake_host();
        fake.trackers
            .lock()
            .unwrap()
            .insert(LOGIN.into(), Arc::new(Hiding::new(flaky.clone(), shown())));
        let host = Arc::new(DiesOnce {
            inner: fake,
            armed: AtomicBool::new(false),
            fired: AtomicBool::new(false),
            always: false,
        });
        let clock = Arc::new(FakeClock::new());
        let freshness = Arc::new(Freshness::new(clock.clone()));
        let service = Factory::new(store.clone(), me(), host.clone(), freshness, clock.clone());
        let running = tokio::spawn(service.serve());
        let (store2, host2, flaky2, clock2) =
            (store.clone(), host.clone(), flaky.clone(), clock.clone());
        let ready_after = tokio::task::spawn_blocking(move || {
            let first = came(|| {
                status_of(&store2, project(1)).is_some_and(|status| !status.ready.is_empty())
            });
            // A good pass is in the memo. The next pass dies outside `pass::run`.
            host2.armed.store(true, Ordering::SeqCst);
            let died = came(|| {
                clock2.advance(Duration::from_secs(3600));
                host2.fired.load(Ordering::SeqCst)
            });
            // Then GitHub fails: such a pass writes the last good results again.
            flaky2.down.store(true, Ordering::SeqCst);
            let failed = came(|| {
                clock2.advance(Duration::from_secs(3600));
                flaky2.refused.load(Ordering::SeqCst) >= 2
            });
            let ready = status_of(&store2, project(1)).map(|status| status.ready.len());
            (first, died, failed, ready)
        })
        .await
        .unwrap();
        running.abort();
        assert!(ready_after.0, "the first pass queued the ready issue");
        assert!(ready_after.1, "a pass died outside the pass");
        assert!(ready_after.2, "the passes after it ran");
        assert_eq!(
            ready_after.3,
            Some(1),
            "the failed pass wrote the last good results, which the dead pass did not drop"
        );
    }

    /// Waits until this machine's status of project 1 is idle, moving the clock an hour at a
    /// time so the passes run, and answers what the first idle status says. The status is read
    /// before each move, so no pass runs between the one that made it idle and the read.
    async fn idle_said(store: &Arc<Mutex<Store>>, clock: &Arc<FakeClock>) -> Option<String> {
        let (store, clock) = (store.clone(), clock.clone());
        tokio::task::spawn_blocking(move || {
            let mut said = None;
            came(|| {
                said = status_of(&store, project(1)).and_then(|status| status.idle);
                if said.is_none() {
                    clock.advance(Duration::from_secs(3600));
                }
                said.is_some()
            });
            said
        })
        .await
        .unwrap()
    }

    /// The fleet cannot be read, as when a newer build left a record: every pass fails before
    /// `pass::run` begins, and the third in a row makes the status idle.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn passes_that_cannot_read_the_fleet_are_counted_and_the_third_makes_the_status_idle() {
        let (dir, store) = open_store();
        store
            .lock()
            .unwrap()
            .apply_settings(settings_with(&[(1, LOGIN)]))
            .unwrap();
        rusqlite::Connection::open(dir.path().join("foundry.db"))
            .unwrap()
            .execute(
                "INSERT INTO records (kind, id, owner, version, record) VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    "printer",
                    "unreadable",
                    me().to_string(),
                    1000,
                    r#"{"kind":"printer"}"#
                ],
            )
            .unwrap();
        let host = fake_host();
        host.trackers
            .lock()
            .unwrap()
            .insert(LOGIN.into(), Arc::new(Hiding::new(Hub::new(), shown())));
        let clock = Arc::new(FakeClock::new());
        let freshness = Arc::new(Freshness::new(clock.clone()));
        let service = Factory::new(store.clone(), me(), host, freshness, clock.clone());
        let running = tokio::spawn(service.serve());
        let said = idle_said(&store, &clock).await;
        running.abort();
        let said = said.expect("three passes that could not read the fleet leave it idle");
        assert!(said.contains("3 passes in a row failed"), "{said}");
        assert!(said.contains("the fleet could not be read"), "{said}");
    }

    /// Every pass dies outside `pass::run`: each is a failed pass, and the third in a row makes
    /// the status idle.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn passes_that_die_outside_the_pass_are_counted_and_the_third_makes_the_status_idle() {
        let (_dir, store) = open_store();
        store
            .lock()
            .unwrap()
            .apply_settings(settings_with(&[(1, LOGIN)]))
            .unwrap();
        let fake = fake_host();
        fake.trackers
            .lock()
            .unwrap()
            .insert(LOGIN.into(), Arc::new(Hiding::new(Hub::new(), shown())));
        let host = Arc::new(DiesOnce {
            inner: fake,
            armed: AtomicBool::new(false),
            fired: AtomicBool::new(false),
            always: true,
        });
        let clock = Arc::new(FakeClock::new());
        let freshness = Arc::new(Freshness::new(clock.clone()));
        let service = Factory::new(store.clone(), me(), host, freshness, clock.clone());
        let running = tokio::spawn(service.serve());
        let said = idle_said(&store, &clock).await;
        running.abort();
        let said = said.expect("three passes that died leave it idle");
        assert!(said.contains("3 passes in a row failed"), "{said}");
    }

    /// The pass is parked in a slow `gh` when its project starts again (the token was
    /// replaced). The new pass does not begin until the old one has returned, so two passes
    /// never act on the project at once.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_project_started_again_runs_no_pass_while_its_old_pass_is_still_going() {
        let (_dir, store) = open_store();
        store
            .lock()
            .unwrap()
            .apply_settings(settings_with(&[(1, "slow-bot")]))
            .unwrap();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let slow = Arc::new(Slow {
            inner: Hub::new(),
            gate: gate.clone(),
            entered: AtomicBool::new(false),
        });
        let fake = fake_host();
        fake.trackers.lock().unwrap().insert(
            "slow-bot".into(),
            Arc::new(Hiding::new(slow.clone(), shown())),
        );
        let host = Counting::new(fake);
        let clock = Arc::new(FakeClock::new());
        let freshness = Arc::new(Freshness::new(clock.clone()));
        let service = Factory::new(store.clone(), me(), host.clone(), freshness, clock.clone());
        let running = tokio::spawn(service.serve());
        let slow2 = slow.clone();
        tokio::task::spawn_blocking(move || {
            wait_for("the old pass parks in gh", || {
                slow2.entered.load(Ordering::SeqCst)
            })
        })
        .await
        .unwrap();
        host.inner
            .marks
            .lock()
            .unwrap()
            .insert("slow-bot".into(), 2);
        clock.advance(SCAN);
        let host2 = host.clone();
        tokio::task::spawn_blocking(move || {
            wait_for("the project starts again", || {
                host2.inner.made.lock().unwrap().len() == 2
            })
        })
        .await
        .unwrap();
        for _ in 0..3 {
            clock.advance(Duration::from_secs(60));
            a_while().await;
        }
        let while_parked = host.passes("slow-bot");
        let_go(&gate);
        let host2 = host.clone();
        let next = tokio::task::spawn_blocking(move || {
            came(|| {
                clock.advance(Duration::from_secs(60));
                host2.passes("slow-bot") >= 2
            })
        })
        .await
        .unwrap();
        running.abort();
        assert_eq!(
            while_parked, 1,
            "no new pass begins while the old one is in gh"
        );
        assert!(next, "the new pass runs once the old one returned");
    }

    /// Whether the caller runs on a thread that may block: a blocking thread of the runtime does,
    /// an async worker thread panics when asked to wait on a future.
    fn may_block() -> bool {
        let handle = tokio::runtime::Handle::current();
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle.block_on(async {}))).is_ok()
    }

    /// A host that notes on which kind of thread the store and secret reads of the service run.
    struct Probed {
        inner: Arc<FakeHost>,
        /// Calls of `token_mark` and `lease` that ran on an async worker thread.
        async_calls: AtomicUsize,
        /// Calls of `hide` that ran on a blocking thread: the seed asks for the hider there.
        blocking_hides: AtomicUsize,
        /// Texts the hider hid on an async worker thread: the real one reads the secret store.
        async_hidings: Arc<AtomicUsize>,
    }

    impl Host for Probed {
        fn acting(&self) -> Answering<'_, Option<bool>> {
            self.inner.acting()
        }
        fn tracker(&self, login: &str) -> Option<Arc<dyn Commenting>> {
            self.inner.tracker(login)
        }
        fn token_mark(&self, login: &str) -> Result<Option<u64>, String> {
            if !may_block() {
                self.async_calls.fetch_add(1, Ordering::SeqCst);
            }
            self.inner.token_mark(login)
        }
        fn lease(&self, watched: &Watched) -> Arc<dyn Leasing> {
            if !may_block() {
                self.async_calls.fetch_add(1, Ordering::SeqCst);
            }
            self.inner.lease(watched)
        }
        fn worker(&self) -> Arc<dyn Worker> {
            self.inner.worker()
        }
        fn online(&self) -> BTreeSet<MachineId> {
            self.inner.online()
        }
        fn hide(&self) -> Hide {
            if may_block() {
                self.blocking_hides.fetch_add(1, Ordering::SeqCst);
            }
            let (inner, async_hidings) = (self.inner.hide(), self.async_hidings.clone());
            Arc::new(move |text: &str| {
                if !may_block() {
                    async_hidings.fetch_add(1, Ordering::SeqCst);
                }
                inner(text)
            })
        }
    }

    fn probed(inner: Arc<FakeHost>) -> Arc<Probed> {
        Arc::new(Probed {
            inner,
            async_calls: AtomicUsize::new(0),
            blocking_hides: AtomicUsize::new(0),
            async_hidings: Arc::default(),
        })
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_seed_and_the_secret_reads_of_a_scan_do_not_run_on_the_async_task() {
        let (_dir, store) = open_store();
        store
            .lock()
            .unwrap()
            .apply_settings(settings_with(&[(1, LOGIN)]))
            .unwrap();
        let fake = fake_host();
        fake.trackers
            .lock()
            .unwrap()
            .insert(LOGIN.into(), Arc::new(Hiding::new(Hub::new(), shown())));
        let host = probed(fake);
        let clock = Arc::new(FakeClock::new());
        let freshness = Arc::new(Freshness::new(clock.clone()));
        let service = Factory::new(store.clone(), me(), host.clone(), freshness, clock.clone());
        let running = tokio::spawn(service.serve());
        let host2 = host.clone();
        tokio::task::spawn_blocking(move || {
            wait_for("the lease", || host2.inner.made.lock().unwrap().len() == 1)
        })
        .await
        .unwrap();
        // A second scan reads the token mark of the running project.
        clock.advance(SCAN);
        a_while().await;
        running.abort();
        assert_eq!(
            host.async_calls.load(Ordering::SeqCst),
            0,
            "token_mark and lease run on a blocking thread"
        );
        assert!(
            host.blocking_hides.load(Ordering::SeqCst) >= 1,
            "the seed runs on a blocking thread"
        );
        assert_eq!(host.async_hidings.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_idle_text_of_projects_that_share_a_repo_is_hidden_on_a_blocking_thread() {
        let (_dir, store) = open_store();
        let mut settings = two_projects_on_one_repo_and_a_third();
        settings.projects.truncate(2);
        store.lock().unwrap().apply_settings(settings).unwrap();
        let host = probed(fake_host());
        let clock = Arc::new(FakeClock::new());
        let freshness = Arc::new(Freshness::new(clock.clone()));
        let service = Factory::new(store.clone(), me(), host.clone(), freshness, clock.clone());
        let running = tokio::spawn(service.serve());
        let store2 = store.clone();
        tokio::task::spawn_blocking(move || {
            wait_for("both statuses", || {
                status_of(&store2, project(1)).is_some() && status_of(&store2, project(2)).is_some()
            })
        })
        .await
        .unwrap();
        // The next scans hide the texts again before they check the store.
        for _ in 0..2 {
            clock.advance(SCAN);
            a_while().await;
        }
        running.abort();
        assert_eq!(
            host.async_hidings.load(Ordering::SeqCst),
            0,
            "the hider reads the secret store, so it runs on a blocking thread"
        );
    }

    /// Turns on a second project naming project 1's repo, as `seq`.
    fn share_repo(store: &Arc<Mutex<Store>>, seq: u64, shared: bool, logins: [&str; 2]) {
        let mut settings = settings_with(&[(1, logins[0]), (2, logins[1])]);
        settings.seq = seq;
        if shared {
            settings.projects[1].repo = settings.projects[0].repo.clone();
        }
        store.lock().unwrap().apply_settings(settings).unwrap();
    }

    fn says_shared(status: &Option<FactoryStatus>) -> bool {
        status
            .as_ref()
            .and_then(|s| s.idle.as_deref())
            .is_some_and(|idle| idle.contains("also named"))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_project_that_was_shared_and_is_watched_again_resumes_its_pass_count() {
        let (_dir, store) = open_store();
        share_repo(&store, 1, false, [LOGIN, "b-bot"]);
        let host = fake_host();
        host.trackers
            .lock()
            .unwrap()
            .insert(LOGIN.into(), Arc::new(Hiding::new(Hub::new(), shown())));
        let clock = Arc::new(FakeClock::new());
        let freshness = Arc::new(Freshness::new(clock.clone()));
        let service = Factory::new(store.clone(), me(), host, freshness, clock.clone());
        let running = tokio::spawn(service.serve());
        let store2 = store.clone();
        let reached = tokio::task::spawn_blocking(move || {
            wait_for("a pass", || {
                status_of(&store2, project(1)).is_some_and(|s| s.pass >= 1)
            });
            status_of(&store2, project(1)).unwrap().pass
        })
        .await
        .unwrap();
        share_repo(&store, 2, true, [LOGIN, "b-bot"]);
        clock.advance(SCAN);
        let store2 = store.clone();
        let shared = tokio::task::spawn_blocking(move || {
            wait_for("the shared status", || {
                says_shared(&status_of(&store2, project(1)))
            });
            status_of(&store2, project(1)).unwrap()
        })
        .await
        .unwrap();
        share_repo(&store, 3, false, [LOGIN, "b-bot"]);
        clock.advance(SCAN);
        let store2 = store.clone();
        let resumed = tokio::task::spawn_blocking(move || {
            came(|| {
                status_of(&store2, project(1))
                    .is_some_and(|s| !says_shared(&Some(s.clone())) && s.pass > reached)
            })
        })
        .await
        .unwrap();
        running.abort();
        assert!(
            shared.pass >= reached,
            "the shared status keeps the count: {} < {reached}",
            shared.pass
        );
        assert!(resumed, "the count goes on above {reached}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_pass_in_flight_when_its_project_becomes_shared_does_not_leave_its_own_status() {
        let (_dir, store) = open_store();
        share_repo(&store, 1, false, ["slow-bot", "b-bot"]);
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let slow = Arc::new(Slow {
            inner: Hub::new(),
            gate: gate.clone(),
            entered: AtomicBool::new(false),
        });
        let host = fake_host();
        host.trackers.lock().unwrap().insert(
            "slow-bot".into(),
            Arc::new(Hiding::new(slow.clone(), shown())),
        );
        let clock = Arc::new(FakeClock::new());
        let freshness = Arc::new(Freshness::new(clock.clone()));
        let service = Factory::new(store.clone(), me(), host, freshness, clock.clone());
        let running = tokio::spawn(service.serve());
        let slow2 = slow.clone();
        tokio::task::spawn_blocking(move || {
            wait_for("the pass in flight", || {
                slow2.entered.load(Ordering::SeqCst)
            })
        })
        .await
        .unwrap();
        share_repo(&store, 2, true, ["slow-bot", "b-bot"]);
        clock.advance(SCAN);
        let store2 = store.clone();
        tokio::task::spawn_blocking(move || {
            wait_for("the shared status", || {
                says_shared(&status_of(&store2, project(1)))
            })
        })
        .await
        .unwrap();
        let_go(&gate);
        let store2 = store.clone();
        let overwritten = tokio::task::spawn_blocking(move || {
            came(|| !says_shared(&status_of(&store2, project(1))))
        })
        .await
        .unwrap();
        let in_flight = status_of(&store, project(1)).map_or(0, |s| s.pass);
        clock.advance(SCAN);
        let store2 = store.clone();
        let again = tokio::task::spawn_blocking(move || {
            came(|| says_shared(&status_of(&store2, project(1))))
        })
        .await
        .unwrap();
        let last = status_of(&store, project(1));
        running.abort();
        assert!(overwritten, "the pass in flight writes its own status");
        assert!(
            again,
            "the next scan writes the shared status again: {last:?}"
        );
        let last = last.unwrap();
        assert!(!last.holds, "{last:?}");
        assert!(last.pass >= in_flight, "{} < {in_flight}", last.pass);
    }
}
