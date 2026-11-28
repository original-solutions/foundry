//! The GitHub logins, the skill and the gateway keys this machine holds.
//!
//! Every [`CHECK_EVERY`] this machine reads the logins it holds a token for, its default login,
//! which gateway keys are set, and whether the `factory-build` skill is installed for the
//! service user, and writes a [`MachineHeld`] record when any differs from what it last wrote,
//! the first check at once. The age between checks is read on the sleep-counting clock of
//! [`crate::clock`]. Only the names of the secrets are listed, and the default login (which is no
//! secret) is read: no token or key is read, and none is ever logged.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use foundry_proto::factory::{FactoryBody, FactoryRecord, GatewayKeys, MachineHeld};
use foundry_proto::ids::MachineId;

use crate::clock::Clock;
use crate::secrets::SecretsError;
use crate::secrets::{self, Secrets};
use crate::store::{Applied, Store, StoreError};

/// Why a check wrote nothing.
#[derive(Debug, thiserror::Error)]
enum HeldError {
    #[error(transparent)]
    Secrets(#[from] SecretsError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("the store did not take the record: {0:?}")]
    NotStored(Applied),
}

/// How often the logins and the skill are checked.
pub const CHECK_EVERY: Duration = Duration::from_secs(5);

/// How often the clock is read while waiting. It only paces the reads: the wait is measured on
/// the clock.
const POLL: Duration = Duration::from_millis(100);

/// What the skill is called under the service user's home.
const SKILL: &str = ".claude/skills/factory-build/SKILL.md";

/// Keeps this machine's [`MachineHeld`] record current, for ever.
pub async fn run(store: Arc<Mutex<Store>>, me: MachineId, secrets: Arc<dyn Secrets>) {
    let skill = skill_file(std::env::var_os("HOME"));
    run_with(store, me, secrets, skill, crate::clock::system()).await;
}

/// The skill file of the service user: under `home`, or when that is unset or empty under the
/// home the password database gives. `None` when neither is known.
pub fn skill_file(home: Option<OsString>) -> Option<PathBuf> {
    let home = home
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .or_else(passwd_home)?;
    Some(home.join(SKILL))
}

fn passwd_home() -> Option<PathBuf> {
    let user = nix::unistd::User::from_uid(nix::unistd::getuid()).ok()??;
    (!user.dir.as_os_str().is_empty()).then_some(user.dir)
}

/// [`run`] with the skill file and the clock given.
pub(crate) async fn run_with(
    store: Arc<Mutex<Store>>,
    me: MachineId,
    secrets: Arc<dyn Secrets>,
    skill: Option<PathBuf>,
    clock: Arc<dyn Clock>,
) {
    let mut written: Option<MachineHeld> = None;
    // What the last failed check said, while checks keep failing.
    let mut failing: Option<String> = None;
    loop {
        let checked = clock.now();
        let (store, secrets, skill, last) = (
            store.clone(),
            secrets.clone(),
            skill.clone(),
            written.clone(),
        );
        match tokio::task::spawn_blocking(move || check(&store, me, &*secrets, skill, last)).await {
            Ok(Ok(held)) => {
                if failing.take().is_some() {
                    tracing::info!("what this machine holds can be read and written again");
                }
                written = Some(held);
            }
            Ok(Err(error)) => note_failure(&mut failing, error.to_string()),
            Err(error) => {
                // JoinError's text carries a task id that differs each time: key on a fixed one.
                let key = if error.is_panic() {
                    "the check panicked"
                } else {
                    "the check was cancelled"
                };
                if failing.as_deref() != Some(key) {
                    tracing::warn!(%error, "what this machine holds not written");
                }
                failing = Some(key.to_owned());
            }
        }
        while clock.now().saturating_sub(checked) < CHECK_EVERY {
            tokio::time::sleep(POLL).await;
        }
    }
}

/// Warns when checks start failing or the reason changes, not on every failed check.
fn note_failure(failing: &mut Option<String>, error: String) {
    if failing.as_ref() != Some(&error) {
        tracing::warn!(%error, "what this machine holds not written");
        *failing = Some(error);
    }
}

/// One check: what is held now, written when it is not what `last` says was written. Gives
/// what the record holds.
fn check(
    store: &Mutex<Store>,
    me: MachineId,
    secrets: &dyn Secrets,
    skill: Option<PathBuf>,
    last: Option<MachineHeld>,
) -> Result<MachineHeld, HeldError> {
    let logins = secrets::github_logins(secrets)?;
    let held = MachineHeld {
        logins,
        skill: skill.is_some_and(|path| path.exists()),
        default: secrets::github_default(secrets),
        // A list that cannot be read says nothing of the keys.
        gateway: secrets.names().ok().map(|names| GatewayKeys {
            key: names.iter().any(|name| name == secrets::GATEWAY),
            management: names.iter().any(|name| name == secrets::GATEWAY_MANAGEMENT),
        }),
    };
    if last.as_ref() == Some(&held) {
        return Ok(held);
    }
    let mut store = store.lock().unwrap_or_else(PoisonError::into_inner);
    let version = store.own_next_factory_version()?;
    match store.apply_factory(
        &FactoryRecord {
            owner: me,
            version,
            body: FactoryBody::Held(held.clone()),
        },
        me,
    )? {
        Applied::Stored => Ok(held),
        other => Err(HeldError::NotStored(other)),
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, PoisonError};
    use std::time::Duration;

    use foundry_proto::factory::{GatewayKeys, MachineHeld};
    use foundry_proto::ids::MachineId;

    use super::*;
    use crate::clock::FakeClock;
    use crate::secrets::{FileSecrets, Secrets, set_from};
    use crate::store::Store;

    struct Rig {
        dir: tempfile::TempDir,
        store: Arc<Mutex<Store>>,
        secrets: Arc<FileSecrets>,
        clock: Arc<FakeClock>,
        me: MachineId,
        skill: PathBuf,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    /// Counts how many times a check has listed the names, and gives what the file gives.
    struct Counting {
        inner: Arc<FileSecrets>,
        lists: Arc<AtomicUsize>,
    }

    impl Secrets for Counting {
        fn get(&self, name: &str) -> Result<Option<String>, SecretsError> {
            self.inner.get(name)
        }
        fn set(&self, name: &str, value: &str) -> Result<(), SecretsError> {
            self.inner.set(name, value)
        }
        fn update(&self, name: &str, edit: crate::secrets::Edit<'_>) -> Result<(), SecretsError> {
            self.inner.update(name, edit)
        }
        fn names(&self) -> Result<Vec<String>, SecretsError> {
            self.lists.fetch_add(1, Ordering::SeqCst);
            self.inner.names()
        }
    }

    fn rig() -> Rig {
        rig_counting().0
    }

    /// A rig, and the number of checks that have started listing names.
    fn rig_counting() -> (Rig, Arc<AtomicUsize>) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Mutex::new(
            Store::open(dir.path().join("foundry.db")).unwrap(),
        ));
        let secrets = Arc::new(FileSecrets::new(dir.path().join("secrets.json")));
        let lists = Arc::new(AtomicUsize::new(0));
        let counting = Arc::new(Counting {
            inner: secrets.clone(),
            lists: lists.clone(),
        });
        let clock = Arc::new(FakeClock::new());
        let me = MachineId::random();
        let skill = dir.path().join("SKILL.md");
        let task = tokio::spawn(run_with(
            store.clone(),
            me,
            counting,
            Some(skill.clone()),
            clock.clone(),
        ));
        let rig = Rig {
            dir,
            store,
            secrets,
            clock,
            me,
            skill,
            task,
        };
        (rig, lists)
    }

    fn held(rig: &Rig) -> Option<MachineHeld> {
        let store = rig.store.lock().unwrap_or_else(PoisonError::into_inner);
        store.held(rig.me).unwrap()
    }

    fn version(rig: &Rig) -> Option<u64> {
        let store = rig.store.lock().unwrap_or_else(PoisonError::into_inner);
        store.factory_versions().unwrap().get(&rig.me).copied()
    }

    async fn settle() {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
    }

    /// Each check runs on the blocking pool: wait in real time, up to 5 s, for the version.
    async fn version_after(rig: &Rig, before: Option<u64>) -> Option<u64> {
        wait_for(|| version(rig) != before).await;
        version(rig)
    }

    /// Moves the fake clock by `by` and lets the loop's pacing sleep fire once.
    async fn pass(rig: &Rig, by: Duration) {
        rig.clock.advance(by);
        tokio::time::advance(POLL * 2).await;
        settle().await;
    }

    /// Time to let a check that writes nothing run.
    fn let_a_check_run() {
        std::thread::sleep(Duration::from_millis(300));
    }

    #[tokio::test(start_paused = true)]
    async fn run_writes_a_record_at_start() {
        let rig = rig();
        version_after(&rig, None).await.expect("a record at once");
        assert_eq!(
            held(&rig),
            Some(MachineHeld {
                logins: vec![],
                skill: false,
                default: None,
                gateway: Some(GatewayKeys {
                    key: false,
                    management: false
                }),
            })
        );
    }

    #[tokio::test(start_paused = true)]
    async fn run_lists_a_login_within_5_seconds_of_a_secret_set() {
        let rig = rig();
        let first = version_after(&rig, None).await;
        set_from(&*rig.secrets, "github:octocat", "ghp_held_one\n".as_bytes()).unwrap();

        pass(&rig, Duration::from_millis(4_900)).await;
        let_a_check_run();
        assert_eq!(version(&rig), first, "nothing yet at 4.9 s");

        pass(&rig, Duration::from_millis(100)).await;
        let second = version_after(&rig, first).await;
        assert!(second > first, "{second:?} after {first:?}");
        assert_eq!(held(&rig).unwrap().logins, ["octocat"]);
    }

    #[tokio::test(start_paused = true)]
    async fn run_flips_skill_when_the_skill_file_appears() {
        let rig = rig();
        let first = version_after(&rig, None).await;
        assert!(!held(&rig).unwrap().skill);
        std::fs::write(&rig.skill, "skill").unwrap();

        pass(&rig, CHECK_EVERY).await;
        assert!(version_after(&rig, first).await > first);
        assert!(held(&rig).unwrap().skill);
    }

    #[tokio::test(start_paused = true)]
    async fn run_writes_no_new_version_when_nothing_changed() {
        let rig = rig();
        let first = version_after(&rig, None).await;
        pass(&rig, CHECK_EVERY).await;
        let_a_check_run();
        pass(&rig, CHECK_EVERY).await;
        let_a_check_run();
        assert_eq!(version(&rig), first);
    }

    #[tokio::test(start_paused = true)]
    async fn run_writes_the_same_record_again_after_ten_minutes_and_not_before() {
        let rig = rig();
        let first = version_after(&rig, None).await;
        let body = held(&rig);

        pass(&rig, Duration::from_secs(599)).await;
        let_a_check_run();
        assert_eq!(version(&rig), first, "nothing changed, so nothing yet");

        pass(&rig, CHECK_EVERY).await;
        let second = version_after(&rig, first).await;
        assert!(second > first, "{second:?} after {first:?}");
        assert_eq!(held(&rig), body, "the same body, at a new version");
    }

    #[tokio::test(start_paused = true)]
    async fn run_does_not_list_a_name_that_is_no_login() {
        let rig = rig();
        let first = version_after(&rig, None).await;
        rig.secrets.set("github:bad/name", "ghp_held_two").unwrap();
        rig.secrets.set("github:alice", "ghp_held_three").unwrap();
        pass(&rig, CHECK_EVERY).await;
        assert!(version_after(&rig, first).await > first);
        assert_eq!(held(&rig).unwrap().logins, ["alice"]);
    }

    #[tokio::test(start_paused = true)]
    async fn run_retries_after_a_read_fails_and_logs_no_token() {
        let logged = crate::test_log::Logged::everywhere();
        let rig = rig();
        let first = version_after(&rig, None).await;
        rig.secrets.set("github:octocat", "ghp_held_four").unwrap();
        let path = rig.dir.path().join("secrets.json");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        pass(&rig, CHECK_EVERY).await;
        let_a_check_run();
        assert_eq!(version(&rig), first, "a failed read writes nothing");

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        pass(&rig, CHECK_EVERY).await;
        assert!(version_after(&rig, first).await > first);
        assert_eq!(held(&rig).unwrap().logins, ["octocat"]);
        let text = logged.text();
        for token in [
            "ghp_held_one",
            "ghp_held_two",
            "ghp_held_three",
            "ghp_held_four",
        ] {
            assert!(!text.contains(token), "{text}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn run_writes_the_default_login_once_github_default_names_one() {
        let rig = rig();
        let first = version_after(&rig, None).await;
        assert_eq!(held(&rig).unwrap().default, None);
        set_from(
            &*rig.secrets,
            "github:octocat",
            "ghp_held_eight\n".as_bytes(),
        )
        .unwrap();
        rig.secrets.set(secrets::GITHUB_DEFAULT, "octocat").unwrap();

        pass(&rig, CHECK_EVERY).await;
        assert!(version_after(&rig, first).await > first);
        let held = held(&rig).unwrap();
        assert_eq!(held.default.as_deref(), Some("octocat"));
        assert_eq!(held.logins, ["octocat"]);
    }

    #[tokio::test(start_paused = true)]
    async fn run_says_which_gateway_keys_are_set_and_logs_no_key() {
        let logged = crate::test_log::Logged::everywhere();
        let rig = rig();
        let first = version_after(&rig, None).await;
        assert_eq!(
            held(&rig).unwrap().gateway,
            Some(GatewayKeys {
                key: false,
                management: false
            })
        );
        set_from(
            &*rig.secrets,
            "gateway-management",
            "sk-mgmt-unique\n".as_bytes(),
        )
        .unwrap();

        pass(&rig, CHECK_EVERY).await;
        assert!(version_after(&rig, first).await > first);
        assert_eq!(
            held(&rig).unwrap().gateway,
            Some(GatewayKeys {
                key: false,
                management: true
            })
        );
        set_from(&*rig.secrets, "gateway", "sk-key-unique\n".as_bytes()).unwrap();
        let second = version(&rig);
        pass(&rig, CHECK_EVERY).await;
        assert!(version_after(&rig, second).await > second);
        assert_eq!(
            held(&rig).unwrap().gateway,
            Some(GatewayKeys {
                key: true,
                management: true
            })
        );
        let text = logged.text();
        for value in ["sk-mgmt-unique", "sk-key-unique"] {
            assert!(!text.contains(value), "{text}");
        }
    }

    fn rig_without_a_task() -> (tempfile::TempDir, Mutex<Store>, FileSecrets, MachineId) {
        let dir = tempfile::tempdir().unwrap();
        let store = Mutex::new(Store::open(dir.path().join("foundry.db")).unwrap());
        let secrets = FileSecrets::new(dir.path().join("secrets.json"));
        (dir, store, secrets, MachineId::random())
    }

    fn version_of(store: &Mutex<Store>, me: MachineId) -> Option<u64> {
        store
            .lock()
            .unwrap()
            .factory_versions()
            .unwrap()
            .get(&me)
            .copied()
    }

    #[test]
    fn check_writes_nothing_when_what_is_held_is_what_was_written() {
        let (_dir, store, secrets, me) = rig_without_a_task();
        let first = check(&store, me, &secrets, None, None).unwrap();
        let version = version_of(&store, me);
        assert!(version.is_some());
        let again = check(&store, me, &secrets, None, Some(first.clone())).unwrap();
        assert_eq!(again, first);
        assert_eq!(version_of(&store, me), version);
    }

    /// Lists the names on every first call and fails on every second, as a store that fails
    /// between two reads does.
    struct EverySecondListFails {
        inner: FileSecrets,
        lists: AtomicUsize,
    }

    impl Secrets for EverySecondListFails {
        fn get(&self, name: &str) -> Result<Option<String>, SecretsError> {
            self.inner.get(name)
        }
        fn set(&self, name: &str, value: &str) -> Result<(), SecretsError> {
            self.inner.set(name, value)
        }
        fn update(&self, name: &str, edit: crate::secrets::Edit<'_>) -> Result<(), SecretsError> {
            self.inner.update(name, edit)
        }
        fn names(&self) -> Result<Vec<String>, SecretsError> {
            if self.lists.fetch_add(1, Ordering::SeqCst) % 2 == 1 {
                return Err(SecretsError::Unreadable {
                    path: PathBuf::from("secrets.json"),
                });
            }
            self.inner.names()
        }
    }

    #[test]
    fn check_leaves_gateway_none_when_only_the_gateway_names_read_fails() {
        let (dir, store, _, me) = rig_without_a_task();
        let secrets = EverySecondListFails {
            inner: FileSecrets::new(dir.path().join("secrets.json")),
            lists: AtomicUsize::new(0),
        };
        secrets.set("github:octocat", "ghp_held_nine").unwrap();

        let held = check(&store, me, &secrets, None, None).unwrap();

        assert_eq!(held.logins, ["octocat"]);
        assert_eq!(held.gateway, None);
        assert_eq!(store.lock().unwrap().held(me).unwrap(), Some(held));
    }

    #[test]
    fn check_with_a_failed_read_writes_nothing_and_errors() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, store, secrets, me) = rig_without_a_task();
        secrets.set("github:octocat", "ghp_held_five").unwrap();
        let path = dir.path().join("secrets.json");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(check(&store, me, &secrets, None, None).is_err());
        assert_eq!(version_of(&store, me), None);
    }

    #[test]
    fn check_writes_a_new_version_with_no_logins_when_the_last_login_is_gone() {
        let (dir, store, secrets, me) = rig_without_a_task();
        secrets.set("github:octocat", "ghp_held_six").unwrap();
        let first = check(&store, me, &secrets, None, None).unwrap();
        assert_eq!(first.logins, ["octocat"]);
        let version = version_of(&store, me);

        let path = dir.path().join("secrets.json");
        std::fs::write(&path, "{}").unwrap();
        let second = check(&store, me, &secrets, None, Some(first)).unwrap();
        assert!(second.logins.is_empty());
        assert!(version_of(&store, me) > version);
        assert_eq!(store.lock().unwrap().held(me).unwrap(), Some(second));
    }

    #[tokio::test(start_paused = true)]
    async fn run_warns_once_while_the_store_stays_unreadable() {
        use std::os::unix::fs::PermissionsExt;
        let logged = crate::test_log::Logged::everywhere();
        let (rig, lists) = rig_counting();
        let first = version_after(&rig, None).await;
        rig.secrets.set("github:octocat", "ghp_held_seven").unwrap();
        let path = rig.dir.path().join("secrets.json");
        let mark = path.display().to_string();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let warnings = |logged: &crate::test_log::Logged| {
            logged
                .text()
                .lines()
                .filter(|line| line.contains(&mark) && line.contains("WARN"))
                .count()
        };
        let before = lists.load(Ordering::SeqCst);
        // Three checks fail; a fourth starting means the third has been handled. Each pass waits
        // for its check to start, so two passes never fold into one check.
        for checks in 1..=4 {
            pass(&rig, CHECK_EVERY).await;
            wait_for(|| lists.load(Ordering::SeqCst) >= before + checks).await;
        }
        assert!(lists.load(Ordering::SeqCst) >= before + 4, "checks ran");
        assert_eq!(warnings(&logged), 1, "{}", logged.text());

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        pass(&rig, CHECK_EVERY).await;
        assert!(version_after(&rig, first).await > first);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        pass(&rig, CHECK_EVERY).await;
        wait_for(|| warnings(&logged) >= 2).await;
        assert_eq!(warnings(&logged), 2, "a new failure warns again");
    }

    /// A store whose listing panics while `panicking` is set, and counts the listings.
    struct Panicking {
        panicking: Arc<std::sync::atomic::AtomicBool>,
        lists: Arc<AtomicUsize>,
    }

    const PANIC_MARK: &str = "held-test-unique-panic-mark";

    impl Secrets for Panicking {
        fn get(&self, _name: &str) -> Result<Option<String>, SecretsError> {
            Ok(None)
        }
        fn set(&self, _name: &str, _value: &str) -> Result<(), SecretsError> {
            Ok(())
        }
        fn update(&self, _name: &str, _edit: crate::secrets::Edit<'_>) -> Result<(), SecretsError> {
            Ok(())
        }
        fn names(&self) -> Result<Vec<String>, SecretsError> {
            self.lists.fetch_add(1, Ordering::SeqCst);
            if self.panicking.load(Ordering::SeqCst) {
                panic!("{PANIC_MARK}");
            }
            Ok(vec![])
        }
    }

    #[tokio::test(start_paused = true)]
    async fn run_warns_once_while_the_check_keeps_panicking() {
        let logged = crate::test_log::Logged::everywhere();
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Mutex::new(
            Store::open(dir.path().join("foundry.db")).unwrap(),
        ));
        let panicking = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let lists = Arc::new(AtomicUsize::new(0));
        let clock = Arc::new(FakeClock::new());
        let me = MachineId::random();
        let task = tokio::spawn(run_with(
            store.clone(),
            me,
            Arc::new(Panicking {
                panicking: panicking.clone(),
                lists: lists.clone(),
            }),
            None,
            clock.clone(),
        ));
        let rig = Rig {
            dir,
            store,
            secrets: Arc::new(FileSecrets::new(PathBuf::from("unused"))),
            clock,
            me,
            skill: PathBuf::from("unused"),
            task,
        };
        let warnings = || {
            logged
                .text()
                .lines()
                .filter(|line| {
                    line.contains(PANIC_MARK)
                        && line.contains("WARN")
                        && line.contains("not written")
                })
                .count()
        };
        // Three checks panic; each pass waits for its check to start.
        wait_for(|| lists.load(Ordering::SeqCst) >= 1).await;
        for checks in 2..=4 {
            pass(&rig, CHECK_EVERY).await;
            wait_for(|| lists.load(Ordering::SeqCst) >= checks).await;
        }
        assert!(lists.load(Ordering::SeqCst) >= 4, "checks ran");
        wait_for(|| warnings() >= 1).await;
        assert_eq!(warnings(), 1, "{}", logged.text());

        panicking.store(false, Ordering::SeqCst);
        pass(&rig, CHECK_EVERY).await;
        assert!(
            version_after(&rig, None).await.is_some(),
            "a later check writes"
        );
    }

    /// Waits in real time, up to 5 s, for `done`.
    async fn wait_for(done: impl Fn() -> bool) {
        for _ in 0..1_000 {
            if done() {
                return;
            }
            tokio::task::yield_now().await;
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn check_gives_not_stored_for_a_record_that_is_not_stored_and_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = Mutex::new(Store::open(dir.path().join("foundry.db")).unwrap());
        let secrets = FileSecrets::new(dir.path().join("secrets.json"));
        let me = MachineId::random();
        let stale = MachineHeld {
            logins: vec!["old".into()],
            skill: true,
            ..Default::default()
        };
        let applied = store
            .lock()
            .unwrap()
            .apply_factory(
                &FactoryRecord {
                    owner: me,
                    version: 1_000,
                    body: FactoryBody::Held(stale.clone()),
                },
                me,
            )
            .unwrap();
        assert_eq!(applied, Applied::Stored);

        let result = check(&store, me, &secrets, None, None);

        assert!(matches!(result, Err(HeldError::NotStored(_))), "{result:?}");
        assert_eq!(store.lock().unwrap().held(me).unwrap(), Some(stale));
    }

    #[test]
    fn skill_file_is_under_the_given_home() {
        assert_eq!(
            skill_file(Some("/home/svc".into())),
            Some(Path::new("/home/svc/.claude/skills/factory-build/SKILL.md").to_path_buf())
        );
    }

    #[test]
    fn skill_file_with_an_empty_home_never_uses_the_empty_string() {
        assert_eq!(skill_file(Some("".into())), skill_file(None));
        let has_home = nix::unistd::User::from_uid(nix::unistd::getuid())
            .ok()
            .flatten()
            .is_some_and(|user| !user.dir.as_os_str().is_empty());
        if has_home {
            let path = skill_file(None).expect("the test user has a passwd entry");
            assert!(path.is_absolute(), "{}", path.display());
            assert!(path.ends_with(".claude/skills/factory-build/SKILL.md"));
        } else {
            assert_eq!(skill_file(None), None);
        }
    }
}
