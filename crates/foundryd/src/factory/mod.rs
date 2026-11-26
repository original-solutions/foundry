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
    let _ = url;
    todo!()
}

/// The projects whose factory is on and whose repo is on GitHub, with a login.
pub fn watched(settings: &SharedSettings) -> Vec<Watched> {
    let _ = settings;
    todo!()
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
        let _ = (
            &self.store,
            self.me,
            &self.host,
            &self.freshness,
            &self.clock,
            &self.book,
            POLL,
            PROTOCOL,
            FACTORY_SINCE,
            BTreeMap::<u8, u8>::new(),
            PoisonError::<u8>::new(0),
            FactorySettings::default(),
        );
        todo!()
    }
}

/// The daemon's [`Host`].
pub struct Daemon {
    api: Api,
    coordinator: Arc<Coordinator>,
    secrets: Arc<dyn Secrets>,
    clock: Arc<dyn Clock>,
    home: PathBuf,
}

impl Host for Daemon {
    fn acting(&self) -> Answering<'_, bool> {
        let _ = (&self.api, &self.coordinator, &self.secrets, &self.clock, &self.home);
        todo!()
    }
    fn tracker(&self, _: &str) -> Option<Arc<dyn Commenting>> {
        todo!()
    }
    fn lease(&self, _: &Watched) -> Arc<dyn Leasing> {
        todo!()
    }
    fn worker(&self) -> Arc<dyn Worker> {
        todo!()
    }
    fn online(&self) -> BTreeSet<MachineId> {
        todo!()
    }
    fn hide(&self) -> Hide {
        todo!()
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
        let _ = (api, coordinator, secrets, freshness, clock, home);
        todo!()
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
        fn lease(&self, watched: &Watched) -> Arc<dyn Leasing> {
            let lease = Arc::new(FakeLease {
                state: LeaseState::Held { generation: 1 },
                released: AtomicBool::new(false),
            });
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
                repo: "https://github.com/rawphp/softfac".into(),
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

    fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
        let started = Instant::now();
        while !done() {
            assert!(started.elapsed() < Duration::from_secs(20), "{what}");
            std::thread::sleep(Duration::from_millis(20));
        }
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
        tokio::task::spawn_blocking(move || {
            wait_for("the slow project's gh is called", || {
                slow2.entered.load(Ordering::SeqCst)
            });
            wait_for("the other project's pass ends while the slow one waits", || {
                status_of(&store2, project(2)).is_some_and(|status| status.holds)
            });
        })
        .await
        .unwrap();
        assert!(status_of(&store, project(1)).is_none());
        // The quick project claimed its ready issue.
        assert!(
            quick
                .writes()
                .iter()
                .any(|write| write.contains("foundry:claim"))
        );
        {
            let (open, turned) = &*gate;
            *open.lock().unwrap() = true;
            turned.notify_all();
        }
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
}
