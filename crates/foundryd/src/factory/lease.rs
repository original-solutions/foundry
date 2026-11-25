//! The lease on `refs/factory/lease` that says which coordinator runs a project's factory.
//!
//! One ref in the project's repo, `refs/factory/lease`, points at a commit with no parent
//! whose message holds the holder's machine id, a generation and a renewal count. A machine
//! acts on a project only while less than 120 seconds have passed since it started its last
//! successful push of that ref. Every age is read from the factory's [`Clock`].

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use foundry_proto::ids::{MachineId, ProjectId};

use crate::clock::Clock;
use crate::factory::worker::{Guard, LeaseLost};
use crate::git_auth::GitAuth;

/// The ref that holds the lease.
pub const REF: &str = "refs/factory/lease";
/// How long a holder waits after its last push started before it renews.
pub const RENEW_AFTER: Duration = Duration::from_secs(60);
/// How long after the start of its last successful push a holder may still act.
pub const ACT_WITHIN: Duration = Duration::from_secs(120);
/// How long another machine sees one lease commit unchanged before it takes over.
pub const TAKE_OVER_AFTER: Duration = Duration::from_secs(180);
/// How long a git call of the lease may run before it is killed.
pub const PUSH_LIMIT: Duration = Duration::from_secs(30);

/// What a tick found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LeaseState {
    /// This machine holds the lease at `generation`.
    Held { generation: u64 },
    /// Another machine holds it, or this one's hold has lapsed.
    HeldBy { machine: MachineId, generation: u64 },
    /// The lease was made with no token.
    NoToken,
    /// Not held and no holder is known.
    Unknown { reason: String },
}

/// One project's lease on this machine.
pub struct Lease {
    git: PathBuf,
}

impl Lease {
    /// The lease for `project` in the repo at `remote_url`.
    pub fn new(
        _project: ProjectId,
        _remote_url: impl Into<String>,
        _git_auth: Option<GitAuth>,
        _clock: Arc<dyn Clock>,
        _me: MachineId,
        _lease_dir: impl Into<PathBuf>,
    ) -> Lease {
        todo!("the lease is not built yet")
    }

    /// Runs `program` in place of `git`, for tests.
    #[cfg(test)]
    fn with_git(mut self, program: impl Into<PathBuf>) -> Self {
        self.git = program.into();
        self
    }

    /// The project.
    pub fn project(&self) -> ProjectId {
        todo!()
    }

    /// One look at the lease.
    pub fn tick(&self, _highest_seen_generation: u64) -> LeaseState {
        todo!()
    }

    /// The guard.
    pub fn guard(&self) -> LeaseGuard {
        todo!()
    }

    /// Stops acting.
    pub fn release(&self) {
        todo!()
    }
}

/// The guard of one lease.
#[derive(Clone)]
pub struct LeaseGuard {}

impl Guard for LeaseGuard {
    fn check(&self) -> Result<u64, LeaseLost> {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
    use std::sync::{Arc, Condvar, Mutex, PoisonError};
    use std::thread;
    use std::time::{Duration, Instant};

    use foundry_proto::ids::{MachineId, ProjectId};
    use nix::unistd::Pid;

    use super::*;
    use crate::clock::FakeClock;
    use crate::secrets::{MemorySecrets, Secrets};

    const TOKEN: &str = "ghp_leaseTestToken0123456789abcdefABCDEF";
    const WAIT: Duration = Duration::from_secs(10);

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn millis(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// Runs git on the bare repo at `git_dir`.
    fn git_out(git_dir: &Path, args: &[&str]) -> std::process::Output {
        Command::new("git")
            .arg("--git-dir")
            .arg(git_dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@localhost")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@localhost")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    /// The trimmed stdout of a git call that must succeed.
    fn git_text(git_dir: &Path, args: &[&str]) -> String {
        let out = git_out(git_dir, args);
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn wait_for(path: &Path) {
        let begun = Instant::now();
        while !path.exists() {
            assert!(begun.elapsed() < WAIT, "{} never appeared", path.display());
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        let begun = Instant::now();
        while !done() {
            assert!(begun.elapsed() < WAIT, "{what} never happened");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn goes_away(pid: i32) -> bool {
        let begun = Instant::now();
        while nix::sys::signal::kill(Pid::from_raw(pid), None).is_ok()
            && begun.elapsed() < Duration::from_secs(5)
        {
            thread::sleep(Duration::from_millis(10));
        }
        nix::sys::signal::kill(Pid::from_raw(pid), None).is_err()
    }

    /// The fields of a lease commit's message.
    struct Fields {
        subject: String,
        holder: String,
        generation: u64,
        renewals: u64,
    }

    /// The project's repo: a bare repo with a `main` branch.
    struct Remote {
        _dir: tempfile::TempDir,
        path: PathBuf,
        main: String,
    }

    impl Remote {
        fn new() -> Remote {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("remote.git");
            let init = Command::new("git")
                .args(["init", "--bare", "--quiet"])
                .arg(&path)
                .status()
                .unwrap();
            assert!(init.success());
            let tree = git_text(&path, &["hash-object", "-t", "tree", "-w", "--stdin"]);
            let main = git_text(&path, &["commit-tree", &tree, "-m", "main"]);
            git_text(&path, &["update-ref", "refs/heads/main", &main]);
            Remote {
                _dir: dir,
                path,
                main,
            }
        }

        fn url(&self) -> String {
            self.path.display().to_string()
        }

        fn lease_sha(&self) -> Option<String> {
            let out = git_out(&self.path, &["rev-parse", "--verify", "--quiet", REF]);
            out.status
                .success()
                .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        }

        fn sha(&self) -> String {
            self.lease_sha().expect("the remote holds a lease")
        }

        fn fields(&self) -> Fields {
            let message = git_text(&self.path, &["cat-file", "commit", &self.sha()]);
            let body = message.split_once("\n\n").expect("a message").1;
            let mut lines = body.lines();
            let subject = lines.next().unwrap().to_string();
            let mut field = |name: &str| -> String {
                body.lines()
                    .find_map(|line| line.strip_prefix(&format!("{name}: ")))
                    .unwrap_or_else(|| panic!("no {name} in {body:?}"))
                    .to_string()
            };
            Fields {
                subject,
                holder: field("holder"),
                generation: field("generation").parse().unwrap(),
                renewals: field("renewals").parse().unwrap(),
            }
        }

        fn parents(&self, sha: &str) -> usize {
            git_text(&self.path, &["rev-list", "--parents", "-n", "1", sha])
                .split_whitespace()
                .count()
                - 1
        }

        fn delete_lease(&self) {
            git_text(&self.path, &["update-ref", "-d", REF]);
        }
    }

    /// One machine's lease, its clock and the markers that steer its fake git.
    struct Machine {
        me: MachineId,
        fake: Arc<FakeClock>,
        lease: Arc<Lease>,
        dir: PathBuf,
        marks: PathBuf,
    }

    impl Machine {
        fn tick(&self, highest: u64) -> LeaseState {
            self.lease.tick(highest)
        }

        fn check(&self) -> Result<u64, LeaseLost> {
            self.lease.guard().check()
        }

        fn advance(&self, by: Duration) {
            self.fake.advance(by);
        }

        fn mark(&self, name: &str) {
            fs::write(self.marks.join(name), "").unwrap();
        }

        fn unmark(&self, name: &str) {
            fs::remove_file(self.marks.join(name)).unwrap();
        }

        fn at(&self, name: &str) -> PathBuf {
            self.marks.join(name)
        }

        fn spawn_tick(&self, highest: u64) -> Receiver<LeaseState> {
            let (tx, rx) = mpsc::channel();
            let lease = self.lease.clone();
            thread::spawn(move || {
                let _ = tx.send(lease.tick(highest));
            });
            rx
        }

        fn pid(&self) -> i32 {
            fs::read_to_string(self.at("pid"))
                .unwrap()
                .trim()
                .parse()
                .unwrap()
        }

        fn held_by(&self, generation: u64) -> LeaseState {
            LeaseState::HeldBy {
                machine: self.me,
                generation,
            }
        }
    }

    fn held(generation: u64) -> LeaseState {
        LeaseState::Held { generation }
    }

    fn answer(rx: &Receiver<LeaseState>) -> LeaseState {
        rx.recv_timeout(WAIT).expect("the tick returned")
    }

    /// A remote and the room for the machines that use it.
    struct Rig {
        remote: Remote,
        dir: tempfile::TempDir,
        next: Cell<usize>,
    }

    impl Rig {
        fn new() -> Rig {
            Rig {
                remote: Remote::new(),
                dir: tempfile::tempdir().unwrap(),
                next: Cell::new(0),
            }
        }

        fn machine(&self) -> Machine {
            self.machine_as(MachineId::random())
        }

        fn machine_as(&self, me: MachineId) -> Machine {
            let fake = Arc::new(FakeClock::new());
            self.machine_with(me, fake.clone(), fake, true)
        }

        fn machine_with(
            &self,
            me: MachineId,
            fake: Arc<FakeClock>,
            clock: Arc<dyn Clock>,
            token: bool,
        ) -> Machine {
            let n = self.next.get();
            self.next.set(n + 1);
            let root = self.dir.path();
            let marks = root.join(format!("marks-{n}"));
            let data = root.join(format!("data-{n}"));
            fs::create_dir_all(&marks).unwrap();
            fs::create_dir_all(&data).unwrap();
            let auth = token.then(|| {
                let secrets = MemorySecrets::default();
                secrets.set("github:tester", TOKEN).unwrap();
                GitAuth::for_login(&secrets, "tester", &data).unwrap()
            });
            let wrapper = root.join(format!("git-{n}"));
            fs::write(&wrapper, wrapper_script(&marks)).unwrap();
            fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
            let dir = root.join(format!("lease-{n}"));
            let lease = Lease::new(
                ProjectId::random(),
                self.remote.url(),
                auth,
                clock,
                me,
                &dir,
            )
            .with_git(wrapper);
            Machine {
                me,
                fake,
                lease: Arc::new(lease),
                dir,
                marks,
            }
        }
    }

    /// A `git` that logs its arguments and, for a push, does what the marker files in `marks`
    /// ask: `fail` (the push fails), `hang` (it never returns), `hold` (it waits for `go`
    /// before it pushes) and `after` (it pushes, then waits for `go` before it exits).
    fn wrapper_script(marks: &Path) -> String {
        format!(
            r#"#!/bin/sh
M='{}'
echo "$@" >> "$M/argv"
push=no
for a in "$@"; do if [ "$a" = push ]; then push=yes; fi; done
if [ $push = yes ]; then
  if [ -e "$M/fail" ]; then echo 'fatal: unable to access' >&2; exit 128; fi
  if [ -e "$M/hang" ]; then echo $$ > "$M/pid"; touch "$M/hung"; while :; do sleep 0.0173; done; fi
  if [ -e "$M/hold" ]; then touch "$M/pushing"; while [ ! -e "$M/go" ]; do sleep 0.0119; done; fi
  if [ -e "$M/after" ]; then
    git "$@"; st=$?
    echo $$ > "$M/pid"; touch "$M/landed"
    while [ ! -e "$M/go" ]; do sleep 0.0119; done
    exit $st
  fi
fi
exec git "$@"
"#,
            marks.display()
        )
    }

    /// A clock that can be put to sleep: while asleep, a read blocks.
    struct Sleepy {
        inner: Arc<FakeClock>,
        state: Mutex<(bool, usize)>,
        wake: Condvar,
    }

    impl Sleepy {
        fn new(inner: Arc<FakeClock>) -> Self {
            Sleepy {
                inner,
                state: Mutex::new((false, 0)),
                wake: Condvar::new(),
            }
        }

        fn sleep(&self) {
            self.state.lock().unwrap().0 = true;
        }

        fn blocked(&self) -> usize {
            self.state.lock().unwrap().1
        }

        fn wake(&self) {
            self.state.lock().unwrap().0 = false;
            self.wake.notify_all();
        }
    }

    impl Clock for Sleepy {
        fn now(&self) -> Duration {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            if state.0 {
                state.1 += 1;
                while state.0 {
                    state = self
                        .wake
                        .wait(state)
                        .unwrap_or_else(PoisonError::into_inner);
                }
                state.1 -= 1;
            }
            drop(state);
            self.inner.now()
        }
    }

    fn never_both(a: &Machine, b: &Machine) {
        assert!(
            a.check().is_err() || b.check().is_err(),
            "both machines may act"
        );
    }

    #[test]
    fn a_first_take_of_a_missing_ref_is_at_once_at_one_more_than_the_highest_generation_seen() {
        let rig = Rig::new();
        let a = rig.machine();
        assert_eq!(a.tick(0), held(1));
        let fields = rig.remote.fields();
        assert_eq!(fields.subject, "foundry factory lease");
        assert_eq!(fields.holder, a.me.to_string());
        assert_eq!((fields.generation, fields.renewals), (1, 0));
        assert_eq!(rig.remote.parents(&rig.remote.sha()), 0);
        assert_eq!(a.check(), Ok(1));

        let other = Rig::new();
        let b = other.machine();
        assert_eq!(b.tick(5), held(6));
        assert_eq!(other.remote.fields().generation, 6);
    }

    #[test]
    fn a_holder_renews_after_60_seconds_keeping_the_generation_and_counting_the_renewal() {
        let rig = Rig::new();
        let a = rig.machine();
        assert_eq!(a.tick(0), held(1));
        let first = rig.remote.sha();
        a.advance(millis(59_999));
        assert_eq!(a.tick(0), held(1));
        assert_eq!(rig.remote.sha(), first);
        a.advance(millis(1));
        assert_eq!(a.tick(0), held(1));
        let second = rig.remote.sha();
        assert_ne!(second, first);
        let fields = rig.remote.fields();
        assert_eq!(fields.holder, a.me.to_string());
        assert_eq!((fields.generation, fields.renewals), (1, 1));
        assert_eq!(rig.remote.parents(&second), 0);
        assert_eq!(a.check(), Ok(1));
    }

    #[test]
    fn two_machines_racing_for_a_missing_ref_leave_one_holder_and_the_loser_stops_at_once() {
        let rig = Rig::new();
        let a = rig.machine();
        let b = rig.machine();
        b.mark("hold");
        let loser = b.spawn_tick(0);
        // B has read the ref as missing and is about to push.
        wait_for(&b.at("pushing"));
        assert_eq!(a.tick(0), held(1));
        b.mark("go");
        assert_eq!(answer(&loser), a.held_by(1));
        assert!(b.check().is_err());
        assert_eq!(a.check(), Ok(1));
        assert_eq!(rig.remote.fields().holder, a.me.to_string());
    }

    #[test]
    fn a_takeover_comes_after_the_commit_is_seen_unchanged_for_180_seconds() {
        let rig = Rig::new();
        let a = rig.machine();
        let b = rig.machine();
        assert_eq!(a.tick(0), held(1));
        assert_eq!(b.tick(0), a.held_by(1));
        // A died: it ticks no more. Both clocks move.
        a.advance(millis(179_999));
        b.advance(millis(179_999));
        assert_eq!(b.tick(0), a.held_by(1));
        never_both(&a, &b);
        a.advance(millis(1));
        b.advance(millis(1));
        assert_eq!(b.tick(0), held(2));
        assert_eq!(rig.remote.fields().holder, b.me.to_string());
        assert_eq!(rig.remote.fields().generation, 2);
        assert_eq!(rig.remote.fields().renewals, 0);
        assert!(a.check().is_err());
        assert_eq!(b.check(), Ok(2));
        never_both(&a, &b);
    }

    #[test]
    fn a_ref_deleted_under_a_holder_is_taken_again_at_a_higher_generation() {
        let rig = Rig::new();
        let a = rig.machine();
        let b = rig.machine();
        assert_eq!(a.tick(0), held(1));
        assert_eq!(b.tick(0), a.held_by(1));
        rig.remote.delete_lease();
        assert_eq!(b.tick(0), held(2));
        assert_eq!(rig.remote.fields().generation, 2);
        assert_eq!(rig.remote.fields().holder, b.me.to_string());
        assert_eq!(a.tick(0), b.held_by(2));
        assert!(a.check().is_err());
        assert_eq!(b.check(), Ok(2));
    }

    #[test]
    fn a_machine_that_saw_a_generation_elsewhere_takes_a_deleted_ref_above_it() {
        let rig = Rig::new();
        let a = rig.machine();
        assert_eq!(a.tick(0), held(1));
        rig.remote.delete_lease();
        let c = rig.machine();
        assert_eq!(c.tick(1), held(2));
        assert_eq!(rig.remote.fields().generation, 2);
        // The old holder finds the ref gone, takes it above everything it has seen.
        assert_eq!(a.tick(0), a.held_by(2));
        assert!(a.check().is_err());
    }

    #[test]
    fn a_restart_finding_its_own_id_renews_the_lease_at_once() {
        let rig = Rig::new();
        let me = MachineId::random();
        let before = rig.machine_as(me);
        assert_eq!(before.tick(0), held(1));
        let after = rig.machine_as(me);
        assert_eq!(after.tick(0), held(1));
        let fields = rig.remote.fields();
        assert_eq!(fields.holder, me.to_string());
        assert_eq!((fields.generation, fields.renewals), (1, 1));
        assert_eq!(after.check(), Ok(1));
    }

    #[test]
    fn a_holder_that_lost_a_push_race_fails_its_check_at_once() {
        let rig = Rig::new();
        let a = rig.machine();
        let b = rig.machine();
        assert_eq!(a.tick(0), held(1));
        assert_eq!(b.tick(0), a.held_by(1));
        a.advance(secs(60));
        b.advance(secs(180));
        a.mark("hold");
        let renewal = a.spawn_tick(0);
        wait_for(&a.at("pushing"));
        assert_eq!(b.tick(0), held(2));
        a.mark("go");
        assert_eq!(answer(&renewal), b.held_by(2));
        // Its last successful push began only 60 seconds ago on its own clock.
        assert!(a.check().is_err());
        assert_eq!(b.check(), Ok(2));
        assert_eq!(rig.remote.fields().holder, b.me.to_string());
    }

    #[test]
    fn a_clock_that_jumps_20_minutes_fails_the_check_and_a_tick_renews() {
        let rig = Rig::new();
        let a = rig.machine();
        assert_eq!(a.tick(0), held(1));
        a.advance(secs(20 * 60));
        let lost = a.check().unwrap_err();
        assert!(lost.reason.contains("1200 s"), "{}", lost.reason);
        assert_eq!(a.tick(0), held(1));
        assert_eq!(a.check(), Ok(1));
        assert_eq!(rig.remote.fields().renewals, 1);
    }

    #[test]
    fn a_push_whose_answer_comes_at_50_seconds_counts_its_age_from_its_start() {
        let rig = Rig::new();
        let fake = Arc::new(FakeClock::new());
        let sleepy = Arc::new(Sleepy::new(fake.clone()));
        let a = rig.machine_with(MachineId::random(), fake.clone(), sleepy.clone(), true);
        a.mark("after");
        let first = a.spawn_tick(0);
        wait_for(&a.at("landed"));
        sleepy.sleep();
        wait_until("a reader of the clock is blocked", || sleepy.blocked() > 0);
        a.mark("go");
        let pid = a.pid();
        wait_until("the wrapper has exited", || {
            let out = Command::new("ps")
                .args(["-o", "stat=", "-p", &pid.to_string()])
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).trim().starts_with('Z')
        });
        fake.advance(secs(50));
        sleepy.wake();
        assert_eq!(answer(&first), held(1));
        assert_eq!(a.check(), Ok(1));
        fake.advance(millis(69_999));
        assert_eq!(a.check(), Ok(1));
        fake.advance(millis(1));
        assert!(a.check().is_err());
    }

    #[test]
    fn a_push_still_running_at_30_seconds_is_killed_and_counts_as_failed() {
        let rig = Rig::new();
        let a = rig.machine();
        a.mark("hang");
        let running = a.spawn_tick(0);
        wait_for(&a.at("hung"));
        a.advance(millis(29_999));
        assert_eq!(
            running.recv_timeout(millis(300)),
            Err(RecvTimeoutError::Timeout)
        );
        a.advance(millis(1));
        let state = answer(&running);
        assert!(!matches!(state, LeaseState::Held { .. }), "{state:?}");
        assert!(a.check().is_err());
        assert!(goes_away(a.pid()));
        assert!(rig.remote.lease_sha().is_none());
    }

    #[test]
    fn a_split_where_one_sides_pushes_fail_lets_only_the_other_side_act() {
        let rig = Rig::new();
        let a = rig.machine();
        let b = rig.machine();
        assert_eq!(a.tick(0), held(1));
        assert_eq!(b.tick(0), a.held_by(1));
        a.mark("fail");
        // 60 s: A's renewal fails and A still holds; B sees the same commit.
        a.advance(secs(60));
        b.advance(secs(60));
        assert_eq!(a.tick(0), held(1));
        assert_eq!(b.tick(0), a.held_by(1));
        never_both(&a, &b);
        // 120 s: A's age is 120 and it no longer acts, though it is the holder in the ref.
        a.advance(secs(60));
        b.advance(secs(60));
        assert!(a.check().is_err());
        assert_eq!(a.tick(0), a.held_by(1));
        assert_eq!(b.tick(0), a.held_by(1));
        never_both(&a, &b);
        // 180 s: B has seen the commit unchanged for 180 s.
        a.advance(secs(60));
        b.advance(secs(60));
        assert_eq!(b.tick(0), held(2));
        never_both(&a, &b);
        a.unmark("fail");
        assert_eq!(a.tick(0), b.held_by(2));
        assert!(a.check().is_err());
        assert_eq!(b.check(), Ok(2));
    }

    #[test]
    fn a_failed_renewal_keeps_the_hold_until_its_age_reaches_120_seconds() {
        let rig = Rig::new();
        let a = rig.machine();
        assert_eq!(a.tick(0), held(1));
        a.mark("fail");
        a.advance(secs(60));
        assert_eq!(a.tick(0), held(1));
        assert_eq!(a.check(), Ok(1));
        a.advance(secs(60));
        assert!(a.check().is_err());
        assert_eq!(a.tick(0), a.held_by(1));
    }

    #[test]
    fn the_lease_directory_never_holds_a_branch_of_the_repo() {
        let rig = Rig::new();
        let a = rig.machine();
        let b = rig.machine();
        assert_eq!(a.tick(0), held(1));
        a.advance(secs(60));
        assert_eq!(a.tick(0), held(1));
        assert_eq!(b.tick(0), a.held_by(1));
        for machine in [&a, &b] {
            let refs = git_text(&machine.dir, &["for-each-ref", "--format=%(refname)"]);
            assert_eq!(refs, REF);
            let remotes = git_out(&machine.dir, &["config", "--get-regexp", r"^remote\."]);
            assert!(remotes.stdout.is_empty());
            let main = git_out(&machine.dir, &["cat-file", "-e", &rig.remote.main]);
            assert!(!main.status.success(), "the lease directory holds main");
        }
    }

    #[test]
    fn a_lease_with_no_token_says_so_and_runs_no_git() {
        let rig = Rig::new();
        let fake = Arc::new(FakeClock::new());
        let a = rig.machine_with(MachineId::random(), fake.clone(), fake, false);
        assert_eq!(a.tick(0), LeaseState::NoToken);
        assert!(!a.dir.exists());
        assert!(!a.at("argv").exists());
        assert!(a.check().is_err());
        assert!(rig.remote.lease_sha().is_none());
    }

    #[test]
    fn a_released_lease_stops_acting_and_pushes_nothing() {
        let rig = Rig::new();
        let a = rig.machine();
        assert_eq!(a.tick(0), held(1));
        let sha = rig.remote.sha();
        a.lease.release();
        let lost = a.check().unwrap_err();
        assert!(lost.reason.contains("released"), "{}", lost.reason);
        a.advance(secs(60));
        let state = a.tick(0);
        assert!(!matches!(state, LeaseState::Held { .. }), "{state:?}");
        assert_eq!(rig.remote.sha(), sha);
    }

    #[test]
    fn the_token_is_in_no_argument_reason_or_debug_form() {
        let rig = Rig::new();
        let a = rig.machine();
        let mut seen = Vec::new();
        a.mark("fail");
        // A missing ref that cannot be taken.
        let unknown = a.tick(0);
        assert!(matches!(unknown, LeaseState::Unknown { .. }), "{unknown:?}");
        if let LeaseState::Unknown { reason } = &unknown {
            assert!(!reason.contains("unable to access"), "{reason}");
        }
        seen.push(format!("{unknown:?}"));
        a.unmark("fail");
        seen.push(format!("{:?}", a.tick(0)));
        a.mark("fail");
        a.advance(secs(60));
        seen.push(format!("{:?}", a.tick(0)));
        a.advance(secs(120));
        seen.push(format!("{:?}", a.check().unwrap_err()));
        // A repo that is not there.
        let gone = Rig::new();
        let fake = Arc::new(FakeClock::new());
        let c = gone.machine_with(MachineId::random(), fake.clone(), fake, true);
        fs::remove_dir_all(&gone.remote.path).unwrap();
        let unread = c.tick(0);
        assert!(matches!(unread, LeaseState::Unknown { .. }), "{unread:?}");
        seen.push(format!("{unread:?}"));
        for text in &seen {
            assert!(!text.contains(TOKEN), "{text}");
        }
        let argv = fs::read_to_string(a.at("argv")).unwrap();
        assert!(!argv.is_empty());
        assert!(!argv.contains(TOKEN));
        assert!(!fs::read_to_string(c.at("argv")).unwrap().contains(TOKEN));
        assert!(!format!("{:?}", a.lease.guard().check()).contains(TOKEN));
    }
}
