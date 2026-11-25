//! The lease on `refs/factory/lease` that says which coordinator runs a project's factory.
//!
//! One ref in the project's repo, `refs/factory/lease`, points at a commit with no parent
//! whose message holds the holder's machine id, a generation and a renewal count. A machine
//! acts on a project only while less than 120 seconds have passed since it started its last
//! successful push of that ref ([`LeaseGuard::check`]). The age is counted from before the
//! push began, since GitHub may take the push long before the answer arrives. Every age and
//! wait reads the factory's [`Clock`], which counts through sleep; nothing here compares two
//! machines' clocks.
//!
//! A [`Lease`] keeps a small bare git directory of its own for the ref alone. It is no clone:
//! it holds the empty tree and lease commits, and it fetches nothing but this ref. Every git
//! call runs with the login's token in its environment only (see [`GitAuth::apply`]), never in
//! an argument, and ends within [`PUSH_LIMIT`] on the clock or is killed with everything it
//! started. Nothing git says on stderr is kept, so no error or reason can quote a URL.
//!
//! [`Lease::tick`] reads the ref and acts on what it finds:
//!
//! - A missing ref is taken at once, at one more than the highest generation seen, with the
//!   push expecting no ref.
//! - A ref that names this machine is renewed at once when this machine holds nothing yet (a
//!   restart) and otherwise once 60 seconds have passed since the last push began. A commit
//!   naming this machine at a generation below one seen since is an old one the ref was put
//!   back to, and counts as another machine's.
//! - A ref that names another machine, or holds a commit the lease did not write, is taken
//!   over once the same commit has been seen unchanged for 180 seconds, at
//!   `max(ref generation, highest seen) + 1`: the ref's generation raised by one unless the
//!   caller or this lease has seen higher.
//! - A generation or count that cannot be raised is never pushed, and a message that holds
//!   `u64::MAX` is a commit the lease did not write.
//!
//! Every push is a compare-and-swap (`--force-with-lease`), so a race has one winner. The loser
//! stops acting at once: its hold is dropped before it reads who won.

use std::fmt;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use foundry_proto::ids::{MachineId, ProjectId};
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use uuid::Uuid;

use crate::clock::Clock;
use crate::factory::worker::{Guard, LeaseLost};
use crate::git_auth::GitAuth;

/// The ref that holds the lease.
pub const REF: &str = "refs/factory/lease";

/// How long after the start of its last push a holder renews.
pub const RENEW_AFTER: Duration = Duration::from_secs(60);

/// How long after the start of its last successful push a holder may still act.
pub const ACT_WITHIN: Duration = Duration::from_secs(120);

/// How long another machine sees one lease commit unchanged before it takes over.
pub const TAKE_OVER_AFTER: Duration = Duration::from_secs(180);

/// How long a git call of the lease may run before it is killed. A push is one of them.
pub const PUSH_LIMIT: Duration = Duration::from_secs(30);

/// How often a running git call is looked at.
const POLL: Duration = Duration::from_millis(10);

/// The most git's output the lease keeps. The rest is read and dropped.
const OUTPUT_LIMIT: usize = 1024 * 1024;

/// The largest generation or renewal count the lease reads or writes. Generations are kept in
/// SQLite records, which hold an `i64`, so a larger number is no lease's: a commit that names
/// one is read as a commit foundryd did not write, and a count that could not be raised to
/// at most this is never pushed. Whatever the lease pushes therefore parses as its own.
pub const MAX_COUNT: u64 = i64::MAX as u64;

/// The first line of every lease commit's message.
const SUBJECT: &str = "foundry factory lease";

/// What a tick found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LeaseState {
    /// This machine holds the lease at `generation` and may act.
    Held {
        /// The generation of the lease.
        generation: u64,
    },
    /// This machine may not act. `machine` and `generation` are the holder and generation last
    /// read in the ref. That is this machine itself when its own hold has lapsed.
    HeldBy {
        /// The holder named in the ref.
        machine: MachineId,
        /// The generation named in the ref.
        generation: u64,
    },
    /// The lease was made with no token, so it runs no git.
    NoToken,
    /// This machine does not hold the lease and no holder is known, so there is no honest
    /// [`MachineId`] to name for [`LeaseState::HeldBy`]: the ref could not be read, or this
    /// tick's push failed, or the ref is missing and could not be taken, or it holds a commit
    /// the lease did not write. `reason` is one of a few fixed texts, never git's own words.
    ///
    /// A pass writes it as a `FactoryStatus` with `holds: false`, `not_holding: None` and
    /// `reason` as `idle`, since `NotHolding` has only `NoToken` and `HeldBy`, and neither can
    /// name an unknown holder.
    Unknown {
        /// Why no holder is known: a fixed text.
        reason: &'static str,
    },
}

/// The holder, generation and renewal count a lease commit's message names.
#[derive(Clone, Copy, Debug)]
struct Parsed {
    holder: MachineId,
    generation: u64,
    renewals: u64,
}

impl Parsed {
    /// The fields of a lease commit's message, or `None` for any other message.
    fn from_message(message: &str) -> Option<Parsed> {
        let (subject, body) = message.split_once("\n\n")?;
        if subject.trim() != SUBJECT {
            return None;
        }
        let field = |name: &str| {
            body.lines()
                .find_map(|line| line.strip_prefix(name)?.strip_prefix(": "))
                .map(str::trim)
        };
        let generation: u64 = field("generation")?.parse().ok()?;
        let renewals: u64 = field("renewals")?.parse().ok()?;
        if generation > MAX_COUNT || renewals > MAX_COUNT {
            return None;
        }
        Some(Parsed {
            holder: MachineId(field("holder")?.parse::<Uuid>().ok()?),
            generation,
            renewals,
        })
    }
}

/// What this machine holds: its last successful push.
#[derive(Clone, Debug)]
struct Hold {
    generation: u64,
    /// The clock reading taken before the push child was started.
    started: Duration,
    sha: String,
}

/// The commit last read in the ref.
#[derive(Clone, Debug)]
struct Seen {
    sha: String,
    /// The clock reading when this lease first read this commit.
    since: Duration,
    /// `None` when the message is not one the lease writes, or the commit could not be
    /// fetched to be read.
    lease: Option<Parsed>,
    /// Whether the commit is in the lease directory. A commit that could not be fetched is
    /// still waited for and taken over, since the swap needs only its id.
    fetched: bool,
}

/// What went wrong in one tick, for its report.
#[derive(Clone, Copy, Debug, Default)]
struct Notes {
    read_failed: bool,
    push_failed: bool,
}

#[derive(Debug)]
struct State {
    hold: Option<Hold>,
    seen: Option<Seen>,
    /// Whether the ref was ever read, as missing or as a commit.
    has_read: bool,
    /// What went wrong in the tick that is running or ran last.
    note: Notes,
    /// The highest generation seen anywhere: told by the caller, read in the ref, or pushed.
    highest: u64,
    released: bool,
}

/// What a lease and its guards share.
struct Shared {
    clock: Arc<dyn Clock>,
    state: Mutex<State>,
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The generation while the lease is held at `now`, else why not.
    fn check_at(&self, now: Duration) -> Result<u64, LeaseLost> {
        let state = self.state();
        let lost = |reason: String| Err(LeaseLost { reason });
        if state.released {
            return lost("the lease was released".into());
        }
        let Some(hold) = &state.hold else {
            return lost("this machine does not hold the lease".into());
        };
        let age = now.saturating_sub(hold.started);
        if age < ACT_WITHIN {
            Ok(hold.generation)
        } else {
            lost(format!(
                "the last successful lease push started {} s ago",
                age.as_secs()
            ))
        }
    }
}

/// How one git call ended.
#[derive(Debug)]
enum Outcome {
    /// The call exited: `code` is `None` when a signal ended it.
    Exit { code: Option<i32>, stdout: String },
    /// The call was still running at [`PUSH_LIMIT`] and was killed.
    TimedOut,
    /// The call could not be started or waited for.
    NotStarted,
}

/// A git call's end and the clock reading taken before it started.
struct Ran {
    started: Duration,
    outcome: Outcome,
}

impl Ran {
    /// The trimmed stdout of a call that exited 0.
    fn ok_stdout(&self) -> Option<&str> {
        match &self.outcome {
            Outcome::Exit {
                code: Some(0),
                stdout,
            } => Some(stdout.trim()),
            _ => None,
        }
    }
}

/// What reading the ref found.
enum Found {
    Missing,
    Present {
        sha: String,
        parsed: Option<Parsed>,
        fetched: bool,
    },
    Failed,
}

/// What a tick does after it read the ref.
enum Plan {
    Nothing,
    Push {
        generation: u64,
        renewals: u64,
        /// The commit the ref must be at for the push to land, or none for no ref.
        expect: Option<String>,
    },
}

/// How a push ended.
enum Pushed {
    Landed,
    Refused,
    Failed,
}

/// One project's lease on this machine. Share it between a task that ticks and the guards it
/// gives out: [`Lease::tick`] blocks on git children and is called from a blocking thread.
pub struct Lease {
    project: ProjectId,
    url: String,
    auth: Option<GitAuth>,
    me: MachineId,
    dir: PathBuf,
    git: PathBuf,
    shared: Arc<Shared>,
    /// Held for a whole tick, so two ticks never read and push at once.
    ticking: Mutex<()>,
}

impl fmt::Debug for Lease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Lease")
            .field("project", &self.project)
            .field("me", &self.me)
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

impl Lease {
    /// The lease for `project` in the repo at `remote_url`, held by `me`, with its bare git
    /// directory at `lease_dir`, made on first use. With no `git_auth` the lease runs no git
    /// and every tick is [`LeaseState::NoToken`].
    pub fn new(
        project: ProjectId,
        remote_url: impl Into<String>,
        git_auth: Option<GitAuth>,
        clock: Arc<dyn Clock>,
        me: MachineId,
        lease_dir: impl Into<PathBuf>,
    ) -> Lease {
        Lease {
            project,
            url: remote_url.into(),
            auth: git_auth,
            me,
            dir: lease_dir.into(),
            git: PathBuf::from("git"),
            shared: Arc::new(Shared {
                clock,
                state: Mutex::new(State {
                    hold: None,
                    seen: None,
                    has_read: false,
                    note: Notes::default(),
                    highest: 0,
                    released: false,
                }),
            }),
            ticking: Mutex::new(()),
        }
    }

    /// The same lease running `program` in place of `git`, for tests.
    #[cfg(test)]
    fn with_git(mut self, program: impl Into<PathBuf>) -> Self {
        self.git = program.into();
        self
    }

    /// The guard that says whether this machine may act now. It never waits on a push.
    pub fn guard(&self) -> LeaseGuard {
        LeaseGuard {
            shared: self.shared.clone(),
        }
    }

    /// Stops acting for good: a released lease never acts again, and a machine that acts
    /// again makes a new [`Lease`], which renews at once since the ref still names it. The
    /// guard fails from now on and no later tick pushes. Nothing is pushed
    /// here. A machine that stops acting stops renewing, and the next coordinator takes over
    /// after the wait.
    pub fn release(&self) {
        let mut state = self.shared.state();
        state.released = true;
        state.hold = None;
    }

    /// Reads the ref and takes, renews or takes over the lease as the module doc says, then
    /// says what this machine may do. `highest_seen_generation` is the highest generation the
    /// caller knows for the project, from a `run_event` or a `factory_status`. Blocking: it
    /// runs git children, each ended within [`PUSH_LIMIT`], so call it from a blocking thread.
    pub fn tick(&self, highest_seen_generation: u64) -> LeaseState {
        let _ticking = self.ticking.lock().unwrap_or_else(PoisonError::into_inner);
        {
            let mut state = self.shared.state();
            // Above the bound it is ignored down to the bound: no generation past it is pushed.
            state.highest = state.highest.max(highest_seen_generation.min(MAX_COUNT));
            if self.auth.is_some() && state.released {
                drop(state);
                return self.report();
            }
            state.note = Notes::default();
        }
        let Some(auth) = &self.auth else {
            return LeaseState::NoToken;
        };
        let read = self.read_ref(auth);
        let now = self.shared.clock.now();
        if let Plan::Push {
            generation,
            renewals,
            expect,
        } = self.decide(&read, now)
        {
            self.push(auth, generation, renewals, expect.as_deref());
        }
        self.report()
    }

    /// What this machine may do, from what it holds and last read.
    fn report(&self) -> LeaseState {
        let now = self.shared.clock.now();
        if let Ok(generation) = self.shared.check_at(now) {
            return LeaseState::Held { generation };
        }
        let state = self.shared.state();
        // A failed read first: a machine that could not look must not name a holder it read
        // before as if it had just looked.
        if state.note.read_failed {
            return LeaseState::Unknown {
                reason: "the lease ref could not be read",
            };
        }
        // Who holds the lease, as this tick read it in the ref, whether or not a push of this
        // tick failed.
        if let Some(Seen {
            lease: Some(lease), ..
        }) = &state.seen
        {
            return LeaseState::HeldBy {
                machine: lease.holder,
                generation: lease.generation,
            };
        }
        // No lease commit was read. A push that failed over a commit the lease did not write
        // says so; for a missing ref the take that failed is "missing and could not be taken".
        let reason = match &state.seen {
            Some(_) if state.note.push_failed => "the lease could not be pushed",
            Some(seen) if !seen.fetched => "the lease commit could not be fetched",
            Some(_) => "the lease ref holds a commit foundryd did not write",
            None if !state.has_read => "the lease ref has not been read",
            None => "the lease ref is missing and could not be taken",
        };
        LeaseState::Unknown { reason }
    }

    /// Records what was read in the ref, without deciding anything.
    fn observe(&self, read: &Found, now: Duration) {
        let mut state = self.shared.state();
        match read {
            Found::Failed => state.note.read_failed = true,
            Found::Missing => {
                state.has_read = true;
                state.seen = None;
            }
            Found::Present {
                sha,
                parsed,
                fetched,
            } => {
                state.has_read = true;
                let since = match &state.seen {
                    Some(seen) if seen.sha == *sha => seen.since,
                    _ => now,
                };
                if let Some(parsed) = parsed {
                    state.highest = state.highest.max(parsed.generation);
                }
                state.seen = Some(Seen {
                    sha: sha.clone(),
                    since,
                    lease: *parsed,
                    fetched: *fetched,
                });
            }
        }
    }

    /// Updates what was seen and held after a read, and says what to push, if anything.
    fn decide(&self, read: &Found, now: Duration) -> Plan {
        self.observe(read, now);
        let mut state = self.shared.state();
        match read {
            Found::Failed => Plan::Nothing,
            Found::Missing => {
                state.hold = None;
                match raised(state.highest) {
                    Some(generation) => Plan::Push {
                        generation,
                        renewals: 0,
                        expect: None,
                    },
                    None => Plan::Nothing,
                }
            }
            Found::Present { sha, parsed, .. } => {
                // A commit naming this machine at a generation below one seen since is an old
                // one that the ref was put back to: it is treated as another machine's.
                let mine = parsed.filter(|parsed| {
                    parsed.holder == self.me && parsed.generation >= state.highest
                });
                if let Some(parsed) = mine {
                    let fresh = state.hold.as_ref().is_some_and(|hold| {
                        hold.sha == *sha && now.saturating_sub(hold.started) < RENEW_AFTER
                    });
                    if fresh {
                        return Plan::Nothing;
                    }
                    // A count that cannot be raised is not renewed this tick.
                    return match raised(parsed.renewals) {
                        Some(renewals) => Plan::Push {
                            generation: parsed.generation,
                            renewals,
                            expect: Some(sha.clone()),
                        },
                        None => Plan::Nothing,
                    };
                }
                state.hold = None;
                let since = state.seen.as_ref().map_or(now, |seen| seen.since);
                if now.saturating_sub(since) < TAKE_OVER_AFTER {
                    return Plan::Nothing;
                }
                // `highest` is at least the ref's own generation: it was raised when it was read.
                match raised(state.highest) {
                    Some(generation) => Plan::Push {
                        generation,
                        renewals: 0,
                        expect: Some(sha.clone()),
                    },
                    None => Plan::Nothing,
                }
            }
        }
    }

    /// Makes a lease commit with no parent and pushes it as the ref, expecting the ref to be
    /// at `expect` (or missing). A landed push is the hold; a refused one drops the hold and
    /// reads who won; one that failed or timed out changes nothing, and the hold lapses by
    /// age.
    fn push(&self, auth: &GitAuth, generation: u64, renewals: u64, expect: Option<&str>) {
        let Some(tree) = self
            .git(auth, &["hash-object", "-t", "tree", "-w", "--stdin"], true)
            .ok_stdout()
            .map(str::to_owned)
        else {
            self.shared.state().note.push_failed = true;
            return;
        };
        let message = format!(
            "{SUBJECT}\n\nholder: {}\ngeneration: {generation}\nrenewals: {renewals}\n",
            self.me
        );
        let Some(commit) = self
            .git(auth, &["commit-tree", &tree, "-m", &message], true)
            .ok_stdout()
            .map(str::to_owned)
        else {
            self.shared.state().note.push_failed = true;
            return;
        };
        let lease = format!("--force-with-lease={REF}:{}", expect.unwrap_or(""));
        let target = format!("{commit}:{REF}");
        let ran = self.git(
            auth,
            &["push", "--porcelain", &lease, "--", &self.url, &target],
            true,
        );
        match pushed(&ran.outcome) {
            Pushed::Landed => {
                let now = self.shared.clock.now();
                let mut state = self.shared.state();
                state.highest = state.highest.max(generation);
                state.has_read = true;
                state.seen = Some(Seen {
                    sha: commit.clone(),
                    since: now,
                    lease: Some(Parsed {
                        holder: self.me,
                        generation,
                        renewals,
                    }),
                    fetched: true,
                });
                if !state.released {
                    state.hold = Some(Hold {
                        generation,
                        started: ran.started,
                        sha: commit,
                    });
                }
            }
            Pushed::Refused => {
                // The hold goes at once. It comes back only if the ref proves unchanged: then
                // a rule or a hook of the remote refused the push, no other machine won, and
                // the hold lapses by age like that of any push that failed.
                let held = self.shared.state().hold.take();
                let read = self.read_ref(auth);
                self.observe(&read, self.shared.clock.now());
                let unchanged = match (&read, expect) {
                    (Found::Present { sha, .. }, Some(expect)) => sha == expect,
                    (Found::Missing, None) => true,
                    _ => false,
                };
                if unchanged {
                    let mut state = self.shared.state();
                    state.note.push_failed = true;
                    if !state.released {
                        state.hold = held;
                    }
                }
            }
            Pushed::Failed => self.shared.state().note.push_failed = true,
        }
    }

    /// Reads the ref in the remote and parses its commit. The ref is listed first, and only
    /// a line naming exactly [`REF`] counts, so a branch or tag that ends in the same words is
    /// no lease. The commit is fetched (one commit, no history) only when the lease directory
    /// has not got it, so a holder never fetches its own commit. A commit that cannot be
    /// fetched is still `Present`, with no message, so the wait and the swap still run.
    ///
    /// Before it, any lock file a killed git call left in the lease directory is removed (see
    /// [`remove_stale`]).
    fn read_ref(&self, auth: &GitAuth) -> Found {
        remove_stale(&self.dir);
        if !self.dir.join("HEAD").exists() {
            let init = self.git(
                auth,
                &[
                    "init",
                    "--bare",
                    "--quiet",
                    "--template=",
                    &self.dir.to_string_lossy(),
                ],
                false,
            );
            if init.ok_stdout().is_none() {
                return Found::Failed;
            }
        }
        let listed = self.git(
            auth,
            &["ls-remote", "--exit-code", "--", &self.url, REF],
            true,
        );
        let sha = match &listed.outcome {
            Outcome::Exit { code: Some(2), .. } => return Found::Missing,
            Outcome::Exit {
                code: Some(0),
                stdout,
            } => {
                // The pattern matches the tail of a name, so other refs may be listed.
                let named = stdout.lines().find_map(|line| {
                    let mut parts = line.split_whitespace();
                    let sha = parts.next()?;
                    (parts.next()? == REF).then_some(sha)
                });
                match named {
                    Some(sha) if sha.len() >= 40 && sha.chars().all(|c| c.is_ascii_hexdigit()) => {
                        sha.to_owned()
                    }
                    Some(_) => return Found::Failed,
                    None => return Found::Missing,
                }
            }
            _ => return Found::Failed,
        };
        let commit = format!("{sha}^{{commit}}");
        let have = self
            .git(auth, &["cat-file", "-e", &commit], true)
            .ok_stdout()
            .is_some();
        let fetched = have || self.fetch(auth, &sha);
        let parsed = fetched
            .then(|| {
                self.git(auth, &["cat-file", "commit", &sha], true)
                    .ok_stdout()
                    .and_then(|commit| commit.split_once("\n\n"))
                    .and_then(|(_, message)| Parsed::from_message(message))
            })
            .flatten();
        Found::Present {
            sha,
            parsed,
            fetched,
        }
    }

    /// Fetches the lease ref, one commit deep, and says whether it is `sha`. Until the check
    /// the ref fetched could be another that the name resolved to in a race, so the check is
    /// what makes the read exact.
    fn fetch(&self, auth: &GitAuth, sha: &str) -> bool {
        let refspec = format!("+{REF}:{REF}");
        let fetch = self.git(
            auth,
            &[
                "fetch",
                "--no-tags",
                "--no-auto-gc",
                "--depth=1",
                "--quiet",
                "--",
                &self.url,
                &refspec,
            ],
            true,
        );
        fetch.ok_stdout().is_some()
            && self
                .git(auth, &["rev-parse", "--verify", "--quiet", REF], true)
                .ok_stdout()
                == Some(sha)
    }

    /// Runs git with `args` and the token, ending within [`PUSH_LIMIT`] on the clock. A call
    /// that `in_repo` reads the lease directory and no other repository.
    fn git(&self, auth: &GitAuth, args: &[&str], in_repo: bool) -> Ran {
        let mut command = Command::new(&self.git);
        auth.apply(&mut command);
        if in_repo {
            // After `apply`, which removes any inherited: git reads this repo and no other.
            command
                .env("GIT_DIR", &self.dir)
                .env("GIT_COMMON_DIR", &self.dir)
                .current_dir(&self.dir);
        }
        if self.url.starts_with('/') || self.url.starts_with("file://") {
            command.env("GIT_ALLOW_PROTOCOL", "https:file");
        }
        command
            .args(args)
            .env("GIT_AUTHOR_NAME", "foundryd")
            .env("GIT_AUTHOR_EMAIL", "foundryd@localhost")
            .env("GIT_COMMITTER_NAME", "foundryd")
            .env("GIT_COMMITTER_EMAIL", "foundryd@localhost")
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Its own process group, so a call killed at the limit takes what it started too.
            .process_group(0);
        let clock = &self.shared.clock;
        let started = clock.now();
        let ended = |outcome| Ran { started, outcome };
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(_) => return ended(Outcome::NotStarted),
        };
        let stdout = drain(child.stdout.take());
        let stderr = drain(child.stderr.take());
        // The leader's exit does not end the call: a descendant may still hold the pipes.
        let mut status = None;
        loop {
            if status.is_none() {
                match child.try_wait() {
                    Ok(found) => status = found,
                    Err(_) => {
                        kill_group(&mut child);
                        return ended(Outcome::NotStarted);
                    }
                }
            }
            if status.is_some() && stdout.is_finished() && stderr.is_finished() {
                break;
            }
            if clock.now().saturating_sub(started) >= PUSH_LIMIT {
                // A machine that slept through the call reads the clock past the limit at its
                // first read after waking, and the answer is already there.
                if status.is_none() {
                    status = child.try_wait().ok().flatten();
                }
                kill_group(&mut child);
                if status.is_none() {
                    return ended(Outcome::TimedOut);
                }
                break;
            }
            thread::sleep(POLL);
        }
        let stdout = stdout.join().unwrap_or_default();
        let _ = stderr.join();
        ended(Outcome::Exit {
            code: status.and_then(|status| status.code()),
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
        })
    }
}

/// `n` raised by one, or `None` when that is above [`MAX_COUNT`].
fn raised(n: u64) -> Option<u64> {
    n.checked_add(1).filter(|raised| *raised <= MAX_COUNT)
}

/// Removes every `*.lock` file under `dir` and every `objects/pack/tmp_*` file, which a git
/// call killed at the limit may have left, and which would make every later read fail for
/// good. It is safe because no git child of this lease is alive when it runs: it is called
/// under the lock that serialises ticks, and every child is killed and reaped before
/// [`Lease::git`] returns. There is one daemon per machine and one lease per directory, so
/// no other process writes there.
fn remove_stale(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if kind.is_dir() {
            remove_stale(&path);
        } else if kind.is_file()
            && (name.ends_with(".lock")
                || (name.starts_with("tmp_") && dir.ends_with("objects/pack")))
        {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// How a push ended, from the `--porcelain` line for the lease ref.
fn pushed(outcome: &Outcome) -> Pushed {
    let Outcome::Exit { code, stdout } = outcome else {
        return Pushed::Failed;
    };
    // `<flag>\t<from>:<to>\t<summary>`; the flag may be a space, so lines are not trimmed.
    let flag = stdout.lines().find_map(|line| {
        let mut parts = line.split('\t');
        let flag = parts.next()?;
        let (_, to) = parts.next()?.rsplit_once(':')?;
        (to == REF && flag.chars().count() == 1).then(|| flag.chars().next())?
    });
    match (code, flag) {
        (Some(0), Some('*' | '+' | ' ' | '=')) => Pushed::Landed,
        (_, Some('!')) => Pushed::Refused,
        _ => Pushed::Failed,
    }
}

/// Kills the child's process group and reaps the child. The leader may already be reaped when
/// a descendant still holds a pipe; its id still names the group while any member is alive.
fn kill_group(child: &mut Child) {
    if let Ok(pid) = i32::try_from(child.id()) {
        let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Reads `pipe` to its end on a thread of its own, keeping at most [`OUTPUT_LIMIT`] bytes.
fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut kept = Vec::new();
        let Some(mut pipe) = pipe else {
            return kept;
        };
        let mut buf = [0u8; 16 * 1024];
        loop {
            match pipe.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let room = OUTPUT_LIMIT.saturating_sub(kept.len());
                    kept.extend_from_slice(&buf[..n.min(room)]);
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        kept
    })
}

/// The guard of one [`Lease`]: it passes only while less than 120 seconds have passed since
/// the start of the lease's last successful push. It reads no git and waits on no push.
#[derive(Clone)]
pub struct LeaseGuard {
    shared: Arc<Shared>,
}

impl Guard for LeaseGuard {
    fn check(&self) -> Result<u64, LeaseLost> {
        // Read before the lock is taken, so a read of the clock never holds up the lease.
        let now = self.shared.clock.now();
        self.shared.check_at(now)
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

    /// Runs git on the bare repo at `git_dir` with `input` on stdin.
    fn git_stdin(git_dir: &Path, args: &[&str], input: &str) -> String {
        use std::io::Write;
        let mut child = Command::new("git")
            .arg("--git-dir")
            .arg(git_dir)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// The reason of an `Unknown` state, whatever type it is held as.
    fn reason_of(state: &LeaseState) -> String {
        match state {
            LeaseState::Unknown { reason } => reason.to_string(),
            other => panic!("not Unknown: {other:?}"),
        }
    }

    const NOT_WRITTEN: &str = "the lease ref holds a commit foundryd did not write";
    const NOT_READ: &str = "the lease ref could not be read";
    const NOT_PUSHED: &str = "the lease could not be pushed";
    const NOT_FETCHED: &str = "the lease commit could not be fetched";
    const NOT_TAKEN: &str = "the lease ref is missing and could not be taken";
    const MAX_COUNT: &str = "9223372036854775807";
    const ABOVE_MAX_COUNT: &str = "9223372036854775808";

    /// A lease commit's message.
    fn lease_message(holder: impl std::fmt::Display, generation: &str, renewals: &str) -> String {
        format!(
            "foundry factory lease\n\nholder: {holder}\ngeneration: {generation}\nrenewals: {renewals}\n"
        )
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
            let field = |name: &str| -> String {
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

        /// Points the lease ref at a commit this lease did not make: it has a parent and a
        /// tree that is not empty. Returns the parent.
        fn write_foreign(&self, message: &str) -> String {
            let blob = git_text(&self.path, &["hash-object", "-w", "--stdin"]);
            let tree = git_stdin(
                &self.path,
                &["mktree"],
                &format!("100644 blob {blob}\tfile\n"),
            );
            let parent = git_text(
                &self.path,
                &["commit-tree", &tree, "-p", &self.main, "-m", "second"],
            );
            git_text(&self.path, &["update-ref", "refs/heads/main", &parent]);
            let sha = git_text(
                &self.path,
                &["commit-tree", &tree, "-p", &parent, "-m", message],
            );
            git_text(&self.path, &["update-ref", REF, &sha]);
            parent
        }

        fn set_lease(&self, sha: &str) {
            git_text(&self.path, &["update-ref", REF, sha]);
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
            // Another test's fork may hold the script open for a moment after it is written, and
            // an exec of it then fails with "text file busy". Run it until it starts.
            wait_until("the fake git can be started", || {
                Command::new(&wrapper).arg("--probe").status().is_ok()
            });
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
    /// ask: `readfail` (a fetch or ls-remote fails), `fetchfail` (only a fetch fails), `reject` (the
    /// remote rejects the push with a `!` line, as a ruleset does), `linger` (the push leaves a child holding the
    /// pipes and exits 0), `fail` (the push fails), `hang` (it never returns), `hold` (it waits for `go`
    /// before it pushes) and `after` (it pushes, then waits for `go` before it exits).
    fn wrapper_script(marks: &Path) -> String {
        format!(
            r#"#!/bin/sh
M='{}'
if [ "$1" = --probe ]; then exit 0; fi
echo "$@" >> "$M/argv"
names=$(env | cut -d= -f1 | grep -E '^(GIT_ASKPASS|FOUNDRY_GIT_TOKEN|GIT_CONFIG_GLOBAL)$' | tr '\n' ' ')
args=$(printf '%s' "$*" | tr '\n' ' ')
printf '%s| %s\n' "$names" "$args" >> "$M/calls"
push=no
for a in "$@"; do
  if [ "$a" = push ]; then push=yes; fi
  if [ "$a" = fetch ] && [ -e "$M/fetchfail" ]; then echo 'fatal: unable to access' >&2; exit 128; fi
  if [ "$a" = fetch ] || [ "$a" = ls-remote ]; then
    if [ -e "$M/readfail" ]; then echo 'fatal: unable to access' >&2; exit 128; fi
  fi
done
if [ $push = yes ]; then
  if [ -e "$M/reject" ]; then
    for last; do :; done
    from=${{last%%:*}}
    printf '!\t%s:refs/factory/lease\t[remote rejected] (push declined due to repository rule violations)\n' "$from"
    exit 1
  fi
  if [ -e "$M/fail" ]; then echo 'fatal: unable to access' >&2; exit 128; fi
  if [ -e "$M/linger" ]; then sleep 47.3192 & echo $! > "$M/gpid"; exit 0; fi
  if [ -e "$M/hang" ]; then sleep 47.3191 & echo $! > "$M/gpid"; echo $$ > "$M/pid"; touch "$M/hung"; while :; do sleep 0.0173; done; fi
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
        // The old holder finds the ref held by C and stops.
        assert_eq!(a.tick(0), c.held_by(2));
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
        let guard = a.lease.guard();
        a.mark("hold");
        let renewal = a.spawn_tick(0);
        wait_for(&a.at("pushing"));
        // The guard never waits on a push.
        assert_eq!(guard.check(), Ok(1));
        assert_eq!(b.tick(0), held(2));
        assert_eq!(guard.check(), Ok(1));
        a.mark("go");
        assert_eq!(answer(&renewal), b.held_by(2));
        // Its last successful push began only 60 seconds ago on its own clock.
        assert!(guard.check().is_err());
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
        // The wrapper's own background child, in the same group, is killed too.
        let child: i32 = fs::read_to_string(a.at("gpid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(goes_away(child));
        assert!(rig.remote.lease_sha().is_none());
    }

    #[test]
    fn a_push_that_exits_but_leaves_a_child_on_the_pipes_is_answered_at_30_seconds() {
        let rig = Rig::new();
        let a = rig.machine();
        a.mark("linger");
        let running = a.spawn_tick(0);
        wait_for(&a.at("gpid"));
        // Written before the child's id is: wait for the id to be whole.
        wait_until("the child's id is written", || {
            fs::read_to_string(a.at("gpid")).is_ok_and(|id| id.trim().parse::<i32>().is_ok())
        });
        a.advance(millis(29_999));
        assert_eq!(
            running.recv_timeout(millis(300)),
            Err(RecvTimeoutError::Timeout)
        );
        a.advance(millis(1));
        let state = answer(&running);
        assert!(!matches!(state, LeaseState::Held { .. }), "{state:?}");
        let child: i32 = fs::read_to_string(a.at("gpid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(goes_away(child));
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
            // The directory fetches nothing but the lease ref, and a machine that only pushed
            // has no ref at all.
            assert!(refs.lines().all(|name| name == REF), "{refs}");
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
        let guard = a.lease.guard();
        assert_eq!(guard.check(), Ok(1));
        a.lease.release();
        assert!(guard.check().is_err());
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
        assert_eq!(reason_of(&unknown), NOT_TAKEN);
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
        assert_eq!(reason_of(&unread), NOT_READ);
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

    #[test]
    fn stale_lock_files_in_the_lease_directory_do_not_stop_the_next_read() {
        let rig = Rig::new();
        let a = rig.machine();
        assert_eq!(a.tick(0), held(1));
        // What a git call killed at 30 s may leave behind.
        let locks = [
            a.dir.join("refs/factory/lease.lock"),
            a.dir.join("shallow.lock"),
            a.dir.join("packed-refs.lock"),
            a.dir.join("objects/pack/tmp_pack_Abc123"),
        ];
        for lock in &locks {
            fs::create_dir_all(lock.parent().unwrap()).unwrap();
            fs::write(lock, "").unwrap();
        }
        a.advance(secs(60));
        assert_eq!(a.tick(0), held(1));
        assert_eq!(rig.remote.fields().renewals, 1);
        for lock in &locks {
            assert!(!lock.exists(), "{} is still there", lock.display());
        }
    }

    #[test]
    fn a_ref_pointed_at_a_commit_with_history_brings_none_of_it() {
        let rig = Rig::new();
        let b = rig.machine();
        let parent = rig.remote.write_foreign("not a lease");
        let state = b.tick(0);
        assert_eq!(reason_of(&state), NOT_WRITTEN);
        let has = git_out(&b.dir, &["cat-file", "-e", &parent]);
        assert!(
            !has.status.success(),
            "the lease directory holds the parent"
        );
        let has = git_out(&b.dir, &["cat-file", "-e", &rig.remote.main]);
        assert!(!has.status.success(), "the lease directory holds main");
    }

    #[test]
    fn a_generation_of_u64_max_in_a_foreign_message_reads_as_not_written_and_is_taken_above_highest()
     {
        let rig = Rig::new();
        let b = rig.machine();
        let other = MachineId::random();
        rig.remote
            .write_foreign(&lease_message(other, "18446744073709551615", "0"));
        let sha = rig.remote.sha();
        assert_eq!(reason_of(&b.tick(0)), NOT_WRITTEN);
        b.advance(secs(180));
        assert_eq!(b.tick(0), held(1));
        assert_ne!(rig.remote.sha(), sha);
        assert_eq!(rig.remote.fields().generation, 1);
    }

    #[test]
    fn a_renewal_count_of_u64_max_naming_this_machine_is_not_renewed_at_once() {
        let rig = Rig::new();
        let a = rig.machine();
        rig.remote
            .write_foreign(&lease_message(a.me, "1", "18446744073709551615"));
        let sha = rig.remote.sha();
        let state = a.tick(0);
        assert_eq!(reason_of(&state), NOT_WRITTEN);
        assert_eq!(rig.remote.sha(), sha);
        assert!(a.check().is_err());
    }

    #[test]
    fn a_ref_rolled_back_to_an_older_commit_naming_this_machine_is_not_renewed_at_once() {
        let rig = Rig::new();
        let b = rig.machine();
        assert_eq!(b.tick(0), held(1));
        let old = rig.remote.sha();
        rig.remote.delete_lease();
        assert_eq!(b.tick(1), held(2));
        // The ref goes back to the old commit, which names B at generation 1.
        rig.remote.set_lease(&old);
        b.advance(secs(60));
        let state = b.tick(0);
        assert!(!matches!(state, LeaseState::Held { .. }), "{state:?}");
        assert_eq!(rig.remote.sha(), old);
        assert!(b.check().is_err());
        b.advance(millis(179_999));
        assert!(!matches!(b.tick(0), LeaseState::Held { .. }));
        assert_eq!(rig.remote.sha(), old);
        b.advance(millis(1));
        assert_eq!(b.tick(0), held(3));
        assert_eq!(rig.remote.fields().generation, 3);
    }

    #[test]
    fn reads_that_fail_keep_the_hold_until_its_age_and_then_say_the_ref_could_not_be_read() {
        let rig = Rig::new();
        let a = rig.machine();
        assert_eq!(a.tick(0), held(1));
        a.mark("readfail");
        a.advance(secs(60));
        assert_eq!(a.tick(0), held(1));
        assert_eq!(a.check(), Ok(1));
        a.advance(millis(59_999));
        assert_eq!(a.tick(0), held(1));
        assert_eq!(a.check(), Ok(1));
        a.advance(millis(1));
        assert_eq!(reason_of(&a.tick(0)), NOT_READ);
        assert!(a.check().is_err());
        a.unmark("readfail");
        assert_eq!(a.tick(0), held(1));
        assert_eq!(rig.remote.fields().renewals, 1);
        assert_eq!(a.check(), Ok(1));
    }

    #[test]
    fn a_watcher_whose_takeover_push_fails_says_the_lease_could_not_be_pushed() {
        let rig = Rig::new();
        let a = rig.machine();
        let b = rig.machine();
        assert_eq!(a.tick(0), held(1));
        assert_eq!(b.tick(0), a.held_by(1));
        let sha = rig.remote.sha();
        b.advance(secs(180));
        b.mark("fail");
        assert_eq!(b.tick(0), a.held_by(1));
        assert_eq!(rig.remote.sha(), sha);
        assert!(b.check().is_err());
    }

    #[test]
    fn a_takeover_of_a_commit_the_lease_did_not_write_that_fails_to_push_says_so() {
        let rig = Rig::new();
        let b = rig.machine();
        rig.remote.write_foreign("not a lease");
        let sha = rig.remote.sha();
        assert_eq!(reason_of(&b.tick(0)), NOT_WRITTEN);
        b.advance(secs(180));
        b.mark("fail");
        assert_eq!(reason_of(&b.tick(0)), NOT_PUSHED);
        assert_eq!(rig.remote.sha(), sha);
    }

    #[test]
    fn a_live_holder_renewing_is_never_taken_over_by_a_watcher() {
        let rig = Rig::new();
        let a = rig.machine();
        let b = rig.machine();
        assert_eq!(a.tick(0), held(1));
        assert_eq!(b.tick(0), a.held_by(1));
        let mut last = rig.remote.sha();
        for renewals in 1..=6 {
            a.advance(secs(60));
            b.advance(secs(60));
            assert_eq!(a.tick(0), held(1));
            assert_eq!(b.tick(0), a.held_by(1));
            never_both(&a, &b);
            let now = rig.remote.sha();
            assert_ne!(now, last);
            last = now;
            assert_eq!(rig.remote.fields().renewals, renewals);
            assert_eq!(rig.remote.fields().holder, a.me.to_string());
        }
    }

    #[test]
    fn a_watcher_told_a_higher_generation_takes_over_above_it() {
        let rig = Rig::new();
        let a = rig.machine();
        let b = rig.machine();
        assert_eq!(a.tick(0), held(1));
        assert_eq!(b.tick(5), a.held_by(1));
        b.advance(secs(180));
        assert_eq!(b.tick(5), held(6));
        assert_eq!(rig.remote.fields().generation, 6);
    }

    #[test]
    fn a_commit_foundryd_did_not_write_is_taken_after_180_seconds_above_highest() {
        let rig = Rig::new();
        let b = rig.machine();
        rig.remote.write_foreign("not a lease");
        let sha = rig.remote.sha();
        assert_eq!(reason_of(&b.tick(3)), NOT_WRITTEN);
        b.advance(millis(179_999));
        assert_eq!(reason_of(&b.tick(3)), NOT_WRITTEN);
        assert_eq!(rig.remote.sha(), sha);
        b.advance(millis(1));
        assert_eq!(b.tick(3), held(4));
        assert_eq!(rig.remote.fields().renewals, 0);
        assert_eq!(rig.remote.parents(&rig.remote.sha()), 0);
    }

    #[test]
    fn every_git_call_carries_the_token_hardening_and_no_token_argument() {
        let rig = Rig::new();
        let a = rig.machine();
        let b = rig.machine();
        assert_eq!(a.tick(0), held(1));
        a.advance(secs(60));
        assert_eq!(a.tick(0), held(1));
        assert_eq!(b.tick(0), a.held_by(1));
        for machine in [&a, &b] {
            let calls = fs::read_to_string(machine.at("calls")).unwrap();
            assert!(calls.lines().count() >= 4, "{calls}");
            for call in calls.lines() {
                let (names, args) = call.split_once('|').unwrap();
                for name in ["GIT_ASKPASS", "FOUNDRY_GIT_TOKEN", "GIT_CONFIG_GLOBAL"] {
                    assert!(names.split_whitespace().any(|n| n == name), "{call}");
                }
                assert!(args.contains("-c credential.helper="), "{call}");
                assert!(args.contains("-c core.hooksPath=/dev/null"), "{call}");
                assert!(!call.contains(TOKEN), "{call}");
            }
        }
    }

    #[test]
    fn a_remote_ref_that_only_ends_in_the_lease_name_reads_as_missing() {
        let rig = Rig::new();
        let a = rig.machine();
        git_text(
            &rig.remote.path,
            &["update-ref", "refs/x/refs/factory/lease", &rig.remote.main],
        );
        assert_eq!(a.tick(0), held(1));
        assert_eq!(rig.remote.fields().generation, 1);
    }

    #[test]
    fn a_generation_at_the_largest_valid_count_in_a_forged_commit_is_never_raised() {
        let rig = Rig::new();
        let b = rig.machine();
        let other = MachineId::random();
        rig.remote
            .write_foreign(&lease_message(other, MAX_COUNT, "0"));
        let sha = rig.remote.sha();
        let at = LeaseState::HeldBy {
            machine: other,
            generation: i64::MAX as u64,
        };
        assert_eq!(b.tick(0), at);
        b.advance(secs(180));
        assert_eq!(b.tick(0), at);
        assert_eq!(rig.remote.sha(), sha);
        assert!(b.check().is_err());
    }

    #[test]
    fn a_count_above_the_largest_valid_one_reads_as_not_written_and_the_taker_then_renews() {
        for forged in [ABOVE_MAX_COUNT, "18446744073709551615"] {
            let rig = Rig::new();
            let b = rig.machine();
            rig.remote
                .write_foreign(&lease_message(MachineId::random(), forged, "0"));
            assert_eq!(reason_of(&b.tick(0)), NOT_WRITTEN);
            b.advance(secs(180));
            assert_eq!(b.tick(0), held(1));
            b.advance(secs(60));
            assert_eq!(b.tick(0), held(1));
            let fields = rig.remote.fields();
            assert_eq!((fields.generation, fields.renewals), (1, 1));
            assert_eq!(b.check(), Ok(1));
        }
    }

    #[test]
    fn a_renewal_count_at_the_largest_valid_count_naming_this_machine_is_not_renewed() {
        let rig = Rig::new();
        let a = rig.machine();
        rig.remote
            .write_foreign(&lease_message(a.me, "1", MAX_COUNT));
        let sha = rig.remote.sha();
        let state = a.tick(0);
        assert!(!matches!(state, LeaseState::Held { .. }), "{state:?}");
        assert_eq!(rig.remote.sha(), sha);
        assert!(a.check().is_err());
    }

    #[test]
    fn a_caller_that_has_seen_u64_max_takes_nothing_and_says_so() {
        let rig = Rig::new();
        let a = rig.machine();
        assert_eq!(reason_of(&a.tick(u64::MAX)), NOT_TAKEN);
        assert!(rig.remote.lease_sha().is_none());
    }

    #[test]
    fn a_branch_named_like_the_lease_ref_is_not_read_as_the_lease() {
        for namespace in ["heads", "tags"] {
            let rig = Rig::new();
            let a = rig.machine();
            git_text(
                &rig.remote.path,
                &[
                    "update-ref",
                    &format!("refs/{namespace}/refs/factory/lease"),
                    &rig.remote.main,
                ],
            );
            // git itself resolves a push to `refs/factory/lease` to such a branch when the ref
            // is missing, and the push expecting no ref is refused: the lease cannot be taken
            // there. What matters is that the branch is neither read as the lease nor moved.
            assert_eq!(reason_of(&a.tick(0)), NOT_TAKEN);
            let ref_name = format!("refs/{namespace}/refs/factory/lease");
            assert_eq!(
                git_text(&rig.remote.path, &["rev-parse", &ref_name]),
                rig.remote.main
            );
            let has = git_out(&a.dir, &["cat-file", "-e", &rig.remote.main]);
            assert!(!has.status.success(), "the lease directory holds main");
        }
    }

    #[test]
    fn a_commit_that_cannot_be_fetched_is_still_waited_for_and_taken_over() {
        let rig = Rig::new();
        let a = rig.machine();
        let b = rig.machine();
        assert_eq!(a.tick(0), held(1));
        let sha = rig.remote.sha();
        b.mark("fetchfail");
        assert_eq!(reason_of(&b.tick(1)), NOT_FETCHED);
        b.advance(millis(179_999));
        assert_eq!(reason_of(&b.tick(1)), NOT_FETCHED);
        assert_eq!(rig.remote.sha(), sha);
        b.advance(millis(1));
        assert_eq!(b.tick(1), held(2));
        assert_eq!(rig.remote.fields().holder, b.me.to_string());
    }

    #[test]
    fn a_renewing_holder_never_fetches_its_own_commit() {
        let rig = Rig::new();
        let a = rig.machine();
        for _ in 0..3 {
            assert_eq!(a.tick(0), held(1));
            a.advance(secs(60));
        }
        let calls = fs::read_to_string(a.at("calls")).unwrap();
        assert!(calls.lines().any(|call| call.contains(" ls-remote ")));
        assert!(
            !calls.lines().any(|call| call.contains(" fetch ")),
            "{calls}"
        );
    }

    #[test]
    fn a_push_the_remote_rejects_while_the_ref_is_unchanged_keeps_the_hold_until_its_age() {
        let rig = Rig::new();
        let a = rig.machine();
        assert_eq!(a.tick(0), held(1));
        a.mark("reject");
        a.advance(secs(60));
        assert_eq!(a.tick(0), held(1));
        assert_eq!(a.check(), Ok(1));
        a.advance(millis(59_999));
        assert_eq!(a.check(), Ok(1));
        a.advance(millis(1));
        assert!(a.check().is_err());
    }

    #[test]
    fn a_takeover_the_remote_rejects_leaves_the_watcher_without_a_hold() {
        let rig = Rig::new();
        let a = rig.machine();
        let b = rig.machine();
        assert_eq!(a.tick(0), held(1));
        assert_eq!(b.tick(0), a.held_by(1));
        let sha = rig.remote.sha();
        b.advance(secs(180));
        b.mark("reject");
        assert_eq!(b.tick(0), a.held_by(1));
        assert!(b.check().is_err());
        assert_eq!(rig.remote.sha(), sha);
    }

    #[test]
    fn reads_that_fail_do_not_restart_a_watchers_180_seconds() {
        let rig = Rig::new();
        let a = rig.machine();
        let b = rig.machine();
        assert_eq!(a.tick(0), held(1));
        assert_eq!(b.tick(0), a.held_by(1));
        b.mark("readfail");
        for step in [60, 60, 30] {
            b.advance(secs(step));
            assert_eq!(reason_of(&b.tick(0)), NOT_READ);
        }
        b.unmark("readfail");
        b.advance(secs(30));
        assert_eq!(b.tick(0), held(2));
    }
}
