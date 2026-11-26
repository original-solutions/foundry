//! The factory: the `Factory` handle and its service, run by the acting coordinator for each
//! project whose lease it holds.
//!
//! [`Factory::run`] is the daemon's service. While this machine acts as coordinator and speaks
//! protocol 5 ([`FACTORY_SINCE`]), each project whose factory is on gets two tasks of its own:
//!
//! - its lease task, which ticks the project's [`Lease`] every [`LEASE_TICK`] on a blocking
//!   thread, keeps what the tick said for the pass, and calls [`Freshness::held`] on the
//!   daemon's one freshness each time a tick answers that this machine holds the lease. The
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
//! after the wait. A project whose repo is not a GitHub repo is not run, and says so in the log.
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
/// and two passes over the same runs would claim the same issues twice.
pub fn watched(settings: &SharedSettings) -> Vec<Watched> {
    watched_and_shared(settings).0
}

/// [`watched`], with the projects left out because another one names their repo.
fn watched_and_shared(settings: &SharedSettings) -> (Vec<Watched>, BTreeSet<ProjectId>) {
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
    (alone, shared.into_iter().map(|w| w.project).collect())
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
    /// Whether this machine acts as coordinator now.
    fn acting(&self) -> Answering<'_, bool>;
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
        // The projects last said to share a repo, so the line is written when it changes.
        let mut said_shared = BTreeSet::new();
        loop {
            let looked = self.clock.now();
            let acting = PROTOCOL >= FACTORY_SINCE && self.host.acting().await;
            let want = if acting {
                let store = self.store.clone();
                match tokio::task::spawn_blocking(move || lock(&store).settings()).await {
                    Ok(Ok(settings)) => {
                        let (watched, shared) = watched_and_shared(&settings);
                        if shared != said_shared {
                            if !shared.is_empty() {
                                tracing::warn!(projects = ?shared, "factory: projects that name one repo are watched from none of them");
                            }
                            said_shared = shared;
                        }
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
            } else {
                // A machine that does not act stops at once, whatever the store says.
                Some(Vec::new())
            };
            if let Some(want) = want {
                running.retain(|project, run| {
                    // A lease made with another token than the one held now, or with none, or
                    // whose token could not be given to git, is made again.
                    // A store that cannot be read says nothing of the token: the lease stays.
                    let same = match self.host.token_mark(&run.watched.login) {
                        Ok(mark) => {
                            mark == run.mark
                                && !(mark.is_some()
                                    && matches!(*lock(&run.state), Some(LeaseState::NoToken)))
                        }
                        Err(error) => {
                            tracing::warn!(project = %project, "factory: the token could not be read: {error}");
                            true
                        }
                    };
                    let keep = want.contains(&run.watched) && same;
                    if !keep {
                        run.lease.release();
                        self.book.forget(*project);
                    }
                    keep
                });
                for watched in want {
                    if let std::collections::btree_map::Entry::Vacant(slot) =
                        running.entry(watched.project)
                    {
                        slot.insert(self.start(watched.clone(), offline.clone()));
                    }
                }
            }
            wait(&*self.clock, looked, SCAN).await;
        }
    }

    /// Starts the lease task and the pass task of `watched`.
    fn start(&self, watched: Watched, offline: Arc<Mutex<Offline>>) -> Running {
        let mark = self.host.token_mark(&watched.login).ok().flatten();
        let lease = self.host.lease(&watched);
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

/// The highest lease generation the run events of `repo` were written under. The store's lock
/// is the whole daemon's, so it is taken for one run at a time, never across the history.
fn highest_event_generation(store: &Mutex<Store>, repo: &str) -> u64 {
    let runs = lock(store).runs_of(repo).unwrap_or_default();
    let mut highest = 0;
    for run in runs {
        let events = lock(store).run_events(&run).unwrap_or_default();
        for event in events {
            highest = highest.max(event.generation);
        }
    }
    highest
}

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
        let ticked = tokio::task::spawn_blocking(move || {
            let statuses = lock(&store2).highest_generation(project).unwrap_or(0);
            let events = known.unwrap_or_else(|| highest_event_generation(&store2, &repo));
            (lease2.tick(statuses.max(events)), events)
        })
        .await;
        match ticked {
            Ok((tick, seen)) => {
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
) {
    let shared = Arc::new(shared);
    while lock(&state).is_none() {
        tokio::time::sleep(POLL).await;
    }
    let ledger = ledger::Ledger::new(shared.store.clone(), shared.me, 0, shared.host.hide());
    let mut memo = pass::Memo::default();
    loop {
        let looked = shared.clock.now();
        let (shared2, watched2, lease2, state2, ledger2) = (
            shared.clone(),
            watched.clone(),
            lease.clone(),
            state.clone(),
            ledger.clone(),
        );
        let ran = tokio::task::spawn_blocking(move || {
            let poll = one_pass(&shared2, &watched2, &*lease2, &state2, &ledger2, &mut memo);
            (memo, poll)
        })
        .await;
        let poll = match ran {
            Ok((kept, poll)) => {
                memo = kept;
                poll
            }
            Err(error) => {
                tracing::warn!(project = %watched.project, "factory: a pass did not finish: {error}");
                memo = pass::Memo::default();
                Duration::from_secs(u64::from(FactorySettings::default().poll_seconds))
            }
        };
        wait(&*shared.clock, looked, poll).await;
    }
}

/// One pass of `watched`, on a blocking thread. Answers how long to wait for the next.
fn one_pass<H: Host>(
    shared: &Shared<H>,
    watched: &Watched,
    lease: &dyn Leasing,
    state: &Mutex<Option<LeaseState>>,
    ledger: &ledger::Ledger,
    memo: &mut pass::Memo,
) -> Duration {
    let default = Duration::from_secs(u64::from(FactorySettings::default().poll_seconds));
    let settings = match lock(&shared.store).settings() {
        Ok(settings) => settings,
        Err(error) => {
            tracing::warn!(project = %watched.project, "factory: the settings could not be read: {error}");
            return default;
        }
    };
    let poll = settings
        .factory
        .as_ref()
        .map_or(default, |f| Duration::from_secs(u64::from(f.poll_seconds)));
    let online = shared.host.online();
    let unreachable = lock(&shared.offline).update(&settings, &online, shared.clock.now());
    let view = match worker::FleetView::read(&lock(&shared.store), online, unreachable) {
        Ok(view) => view,
        Err(error) => {
            tracing::warn!(project = %watched.project, "factory: the fleet could not be read: {error}");
            return poll;
        }
    };
    let (tracker, lease_state) = match shared.host.tracker(&watched.login) {
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
    };
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
    poll
}

/// The token held for `login`, trimmed, when git can be given it: none held, a blank one and one
/// with a control character, which an environment variable could not carry whole, are `None`.
/// The one place that rule is written in this module; `git_auth` holds its own copy.
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
    fn acting(&self) -> Answering<'_, bool> {
        Box::pin(async move {
            match self.coordinator.acting().await {
                Ok(acting) => acting == self.api.peers.me(),
                Err(error) => {
                    tracing::warn!("factory: the acting coordinator could not be read: {error}");
                    false
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
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Condvar, Mutex};
    use std::time::Instant;

    use foundry_proto::factory::{FactoryStatus, NotHolding, ProjectFactory};
    use foundry_proto::settings::Project;

    use super::*;
    use crate::clock::FakeClock;
    use crate::factory::github::{Hiding, Tracker, TrackerError};
    use crate::factory::ledger::testing::open_store;
    use crate::factory::pass::testing::{Hub, LOGIN};
    use crate::factory::worker::{AlwaysHeld, FakeWorker};

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
        fn acting(&self) -> Answering<'_, bool> {
            let acting = self.acting.load(Ordering::SeqCst);
            Box::pin(async move { acting })
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
            let lease = Arc::new(FakeLease {
                state: if self.token_mark(&watched.login).ok().flatten().is_some() {
                    LeaseState::Held { generation: 1 }
                } else {
                    LeaseState::NoToken
                },
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
}
