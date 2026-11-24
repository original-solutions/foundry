//! The GitHub logins and the skill this machine holds.

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex, PoisonError};
    use std::time::Duration;

    use foundry_proto::factory::MachineHeld;
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

    fn rig() -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Mutex::new(
            Store::open(dir.path().join("foundry.db")).unwrap(),
        ));
        let secrets = Arc::new(FileSecrets::new(dir.path().join("secrets.json")));
        let clock = Arc::new(FakeClock::new());
        let me = MachineId::random();
        let skill = dir.path().join("SKILL.md");
        let task = tokio::spawn(run_with(
            store.clone(),
            me,
            secrets.clone(),
            Some(skill.clone()),
            clock.clone(),
        ));
        Rig {
            dir,
            store,
            secrets,
            clock,
            me,
            skill,
            task,
        }
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
        for _ in 0..1_000 {
            let now = version(rig);
            if now != before {
                return now;
            }
            tokio::task::yield_now().await;
            std::thread::sleep(Duration::from_millis(5));
        }
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
                skill: false
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
        for token in ["ghp_held_one", "ghp_held_two", "ghp_held_three", "ghp_held_four"] {
            assert!(!text.contains(token), "{text}");
        }
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
        let path = skill_file(Some("".into()));
        if let Some(path) = path {
            assert!(path.is_absolute(), "{}", path.display());
            assert!(path.ends_with(".claude/skills/factory-build/SKILL.md"));
        }
        let none = skill_file(None);
        if let Some(path) = none {
            assert!(path.is_absolute(), "{}", path.display());
        }
    }
}
