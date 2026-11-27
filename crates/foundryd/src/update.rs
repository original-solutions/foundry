//! Makes newer release tags ready to install, and rolls back a trial that does not stay up.
//!
//! The data directory holds `versions/<version>/{foundryd,foundry-holder}`, `current` (a
//! relative link to one of them, which the service runs) and `update.json`. The check every
//! ten minutes downloads, verifies and unpacks a newer tag into `versions/` and saves it as the
//! ready tag; it never installs it. [`install_ready`] installs the ready tag when asked, with a
//! trial record in `update.json`. A version installed with a trial record is a trial: the
//! daemon that starts from it counts its starts, and `update::run` calls it healthy once it
//! has run for a minute. A trial that starts three times without becoming healthy is put back
//! to the previous version and its tag is refused until a higher one appears. A hand install
//! (`foundryd install`) starts no trial and clears the ready tag. Only a daemon that runs from
//! `<home>/current` takes part in any of this.
//!
//! `foundryd install` and the daemon are separate processes that both change `update.json`
//! and `current`, so every change to either is made while holding `<home>/update.lock`.

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::future::pending;
use std::io::{self, Write as _};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};
use nix::sys::signal::kill;
use nix::unistd::Pid;
use ring::signature::{ED25519, UnparsedPublicKey};
use serde::{Deserialize, Serialize};

/// The repository the releases are read from.
const REPO: &str = "original-solutions/foundry";

/// How long the daemon must have run to count as healthy.
const HEALTHY_AFTER: Duration = Duration::from_secs(60);

/// How long a trial may take to become healthy before the process gives up and exits, so a
/// start that hangs (on a disk that does not answer, say) counts as a failed start.
const TRIAL_DEADLINE: Duration = Duration::from_secs(300);

/// The release public key, as hex. The release workflow reads it from this line and refuses to
/// publish when its signing key does not match, so keep it on one line.
const RELEASE_KEY_HEX: &str = "1884d7afeb3f7af2805f1727358a10baa8688264fd531685d116b9bec30b45e1";

/// The Ed25519 public key a release's archive must be signed with before it is unpacked.
const RELEASE_KEY: [u8; 32] = hex_key(RELEASE_KEY_HEX);

/// Reads 64 lowercase hex digits as 32 bytes, at compile time.
const fn hex_key(hex: &str) -> [u8; 32] {
    const fn digit(c: u8) -> u8 {
        match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            _ => panic!("the release key is lowercase hex"),
        }
    }
    let hex = hex.as_bytes();
    assert!(hex.len() == 64, "the release key is 32 bytes");
    let mut key = [0; 32];
    let mut i = 0;
    while i < 32 {
        key[i] = (digit(hex[2 * i]) << 4) | digit(hex[2 * i + 1]);
        i += 1;
    }
    key
}

/// How long to wait between two looks for a newer tag.
const CHECK_EVERY: Duration = Duration::from_secs(600);

/// Starts a trial may make without becoming healthy; the start after that rolls back.
const MAX_STARTS: u32 = 3;

/// The binaries a release holds, at the top level of its archive.
const BINARIES: [&str; 2] = ["foundryd", "foundry-holder"];

/// How long `gh release list` may take.
const LIST_LIMIT: Duration = Duration::from_secs(60);

/// How long `gh release download` may take.
const DOWNLOAD_LIMIT: Duration = Duration::from_secs(600);

/// How long `tar` may take.
const TAR_LIMIT: Duration = Duration::from_secs(120);

/// How long a new `foundryd --version` may take.
const VERSION_LIMIT: Duration = Duration::from_secs(10);

/// Set once this daemon has run long enough to be healthy; the watchdog reads it.
static HEALTHY: AtomicBool = AtomicBool::new(false);

/// Why the ready tag was not installed. Neither holds a secret.
#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    /// The tag cannot be installed now, and nothing was changed apart from a stranded trial.
    /// `ready` is the tag the sample's `update_ready` would report, and `reason` says which
    /// case it was.
    #[error("{reason}")]
    NotReady {
        ready: Option<String>,
        reason: String,
    },
    /// The state could not be read or written, or `current` could not be pointed.
    #[error(transparent)]
    Failed(#[from] UpdateError),
}

/// Why a tag could not be made ready, or a version installed. None of these ever holds a secret.
#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    /// The release could not be listed, downloaded or checked; tried again later.
    #[error("{0}")]
    Fetch(String),
    /// The release's archive is not signed by the release key.
    #[error("{0} is refused: its signature did not verify")]
    BadSignature(String),
    /// The release does not hold one of the two binaries.
    #[error("{0} is missing: a release holds foundryd and foundry-holder")]
    Missing(&'static str),
    /// The new binary ran and printed something other than its version.
    #[error("{tag} is refused: its foundryd printed {printed:?}, not \"foundryd {version}\"")]
    WrongVersion {
        tag: String,
        version: String,
        printed: String,
    },
    /// The new binary cannot be executed, or exited non-zero when asked its version.
    #[error("{tag} is refused: {detail}")]
    BadCheck { tag: String, detail: String },
    /// The tag is not `vX.Y.Z`.
    #[error("{0:?} is not a release tag")]
    BadTag(String),
    /// A file or the new binary could not be handled.
    #[error("{what}: {source}")]
    Io { what: String, source: io::Error },
}

/// Labels an I/O error with what was being done.
pub(crate) fn io_error(what: impl Into<String>) -> impl FnOnce(io::Error) -> UpdateError {
    let what = what.into();
    move |source| UpdateError::Io { what, source }
}

/// Why a child process did not give a result.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    /// It could not be started.
    #[error("cannot run {name}: {source}")]
    Spawn { name: String, source: io::Error },
    /// It ran past its deadline and was killed.
    #[error("{name} timed out after {secs} s")]
    TimedOut { name: String, secs: u64 },
    /// It exited non-zero. `stderr` is what it said, trimmed.
    #[error("{name} failed: {stderr}")]
    Failed { name: String, stderr: String },
}

/// The program and its first two arguments, to say which command went wrong.
fn command_name(command: &Command) -> String {
    let program = Path::new(command.get_program())
        .file_name()
        .unwrap_or(command.get_program())
        .to_string_lossy()
        .into_owned();
    command.get_args().take(2).fold(program, |name, arg| {
        format!("{name} {}", arg.to_string_lossy())
    })
}

/// Reads a child's pipe to its end on a thread of its own, so a full pipe never blocks it.
fn drain(pipe: Option<impl std::io::Read + Send + 'static>) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut bytes);
        }
        bytes
    })
}

/// Runs `command` and returns its stdout. It is killed and reaped when it outlives `limit`.
/// Every child process of the updater and of `install` goes through here.
pub(crate) fn run_bounded(command: &mut Command, limit: Duration) -> Result<String, RunError> {
    let name = command_name(command);
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| RunError::Spawn {
            name: name.clone(),
            source,
        })?;
    let out = drain(child.stdout.take());
    let err = drain(child.stderr.take());
    let end = Instant::now() + limit;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < end => std::thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                // The readers are left to end by themselves: a grandchild may hold the pipes.
                return Err(RunError::TimedOut {
                    name,
                    secs: limit.as_secs(),
                });
            }
            Err(source) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(RunError::Spawn { name, source });
            }
        }
    };
    let stdout = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();
    if status.success() {
        return Ok(String::from_utf8_lossy(&stdout).into_owned());
    }
    Err(RunError::Failed {
        name,
        stderr: String::from_utf8_lossy(&stderr).trim().to_string(),
    })
}

/// True when a spawn error says the file cannot be executed (no such file or interpreter,
/// not executable, not a program for this system). Any other (out of processes or memory, a
/// busy file) says nothing about the file.
fn unrunnable(error: &io::Error) -> bool {
    let Some(code) = error.raw_os_error() else {
        return false;
    };
    // EBADARCH, which macOS gives for a binary built for another architecture.
    #[cfg(target_os = "macos")]
    if code == 86 {
        return true;
    }
    [Errno::ENOEXEC, Errno::ENOENT, Errno::EACCES]
        .iter()
        .any(|errno| code == *errno as i32)
}

/// A version a trial record in `update.json` names, which has not yet shown it stays up.
/// [`install_ready`] writes one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Trial {
    /// The version on trial, like `0.2.0`.
    pub version: String,
    /// The version `current` goes back to.
    pub previous: String,
    /// How many times the trial has started without becoming healthy.
    pub starts: u32,
}

/// What `update.json` holds.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct State {
    /// The version on trial, if any.
    #[serde(default)]
    pub trial: Option<Trial>,
    /// A tag that was refused, like `v0.2.0`. It is skipped while it is the highest.
    #[serde(default)]
    pub refused: Option<String>,
    /// The tag the check downloaded, verified and placed in `versions/`, waiting to be
    /// installed, like `v0.2.0`. It counts only while [`is_ready`] says so.
    #[serde(default)]
    pub ready: Option<String>,
}

/// The version a release tag names: `v0.2.0` gives `0.2.0`. `None` unless the tag is `v`
/// and three numbers.
pub(crate) fn tag_version(tag: &str) -> Option<&str> {
    tag.strip_prefix('v')
        .filter(|version| parse(version).is_some())
}

/// True when both binaries of `tag`'s version are in `versions/`, so the tag counts as ready.
pub fn is_ready(home: &Path, tag: &str) -> bool {
    tag_version(tag).is_some_and(|version| {
        let dir = home.join("versions").join(version);
        BINARIES.iter().all(|name| dir.join(name).is_file())
    })
}

/// True when `state` holds a trial and `current` names its version: a trial is on, by the rule
/// the check goes by. [`trial_on`] keeps its own, wider rule for its callers.
fn current_names_trial(home: &Path, state: &State) -> bool {
    state
        .trial
        .as_ref()
        .is_some_and(|trial| current_version(home).as_deref() == Some(trial.version.as_str()))
}

/// True when `state` holds a trial whose version `current` does not name, as a crash between the
/// two writes of an install that records a trial leaves.
fn stranded(home: &Path, state: &State) -> bool {
    state.trial.is_some() && !current_names_trial(home, state)
}

/// Clears a stranded trial from `state`. True when there was one.
fn clear_stranded_trial(home: &Path, state: &mut State) -> bool {
    let cleared = stranded(home, state);
    if cleared {
        state.trial = None;
    }
    cleared
}

/// Reads `update.json`. A missing file is an empty state.
pub fn state(home: &Path) -> io::Result<State> {
    match fs::read(home.join("update.json")) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(State::default()),
        Err(error) => Err(error),
    }
}

/// Takes `<home>/update.lock`, waiting for another process that holds it. Every holder only
/// reads and writes `update.json`, removes or renames directories under `versions/` and moves
/// `current` (no network, no copy and no child process), so the wait is short. The lock is let
/// go when the value is dropped. Never take it twice in one thread: a second open of the file
/// waits for the first. The check and a hand install swap a version into `versions/` under it
/// ([`make_ready`], [`place_version`]).
fn lock(home: &Path) -> io::Result<Flock<File>> {
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(home.join("update.lock"))?;
    Flock::lock(file, FlockArg::LockExclusive).map_err(|(_, errno)| io::Error::from(errno))
}

/// Writes `update.json` (mode 0600) to a temporary name and renames it into place. Callers
/// hold [`lock`], so no two writers share the temporary file.
fn save(home: &Path, state: &State) -> io::Result<()> {
    let temp = home.join("update.json.tmp");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temp)?;
    file.write_all(&serde_json::to_vec_pretty(state)?)?;
    file.sync_all()?;
    fs::rename(temp, home.join("update.json"))
}

/// Points `current` at `versions/<version>`, replacing the link in one rename.
pub fn point_current(home: &Path, version: &str) -> io::Result<()> {
    let next = home.join("current.new");
    match fs::remove_file(&next) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    symlink(Path::new("versions").join(version), &next)?;
    fs::rename(next, home.join("current"))
}

/// The version `current` points at, if it points at one.
pub fn current_version(home: &Path) -> Option<String> {
    let target = fs::read_link(home.join("current")).ok()?;
    let version = target.strip_prefix("versions").ok()?;
    Some(version.to_str()?.to_string())
}

/// Reads `X.Y.Z` as three numbers. Any other shape is not a version.
fn parse(text: &str) -> Option<(u64, u64, u64)> {
    let mut parts = text.split('.');
    let mut number = || {
        let part = parts.next()?;
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        part.parse::<u64>().ok()
    };
    let version = (number()?, number()?, number()?);
    parts.next().is_none().then_some(version)
}

/// What a start of the daemon does about an update on trial.
#[derive(Debug, PartialEq)]
pub enum Start {
    /// Carry on.
    Run,
    /// The trial failed: `current` now points at `to` and this process must exit so the
    /// service starts that one.
    RolledBack {
        /// The version `current` went back to.
        to: String,
    },
    /// The trial failed and the rollback stopped part way: this process must exit, and the
    /// next start, with the trial still at its cap, tries the rollback again.
    RollbackStopped {
        /// Why it stopped.
        error: String,
    },
}

/// Counts this start against a trial of `running`, or rolls the trial back. Any other
/// version (a dev build, a manual run, a version that is not on trial) is never counted.
fn start_with(home: &Path, running: &str) -> io::Result<Start> {
    let _lock = lock(home)?;
    let mut state = state(home)?;
    let Some(trial) = state.trial.as_mut().filter(|t| t.version == running) else {
        return Ok(Start::Run);
    };
    if trial.starts < MAX_STARTS {
        trial.starts += 1;
        save(home, &state)?;
        return Ok(Start::Run);
    }
    let previous = trial.previous.clone();
    let back = home.join("versions").join(&previous);
    if let Some(missing) = BINARIES.iter().find(|name| !back.join(name).is_file()) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("cannot roll back: versions/{previous}/{missing} is missing"),
        ));
    }
    // Three steps, each safe to stop after: the refusal is saved with the trial still at its
    // cap, `current` moves, and only then is the trial cleared. A failure or a crash in
    // between leaves a trial at the cap, so the next start rolls back again. A trial never
    // copies a Mac's secrets out of the keychain (only a version installed by hand does), so
    // the version `current` goes back to finds them where it left them.
    let mut roll_back = || -> io::Result<()> {
        state.refused = Some(format!("v{running}"));
        save(home, &state)?;
        point_current(home, &previous)?;
        state.trial = None;
        save(home, &state)
    };
    Ok(match roll_back() {
        Ok(()) => Start::RolledBack { to: previous },
        Err(error) => Start::RollbackStopped {
            error: error.to_string(),
        },
    })
}

/// True when this process runs from `<home>/current`, the only place that is updated: that
/// link names `running`, and `exe` is the file the link reaches.
pub(crate) fn from_current(home: &Path, exe: &Path, running: &str) -> bool {
    if current_version(home).as_deref() != Some(running) {
        return false;
    }
    match (
        fs::canonicalize(exe),
        fs::canonicalize(home.join("current/foundryd")),
    ) {
        (Ok(exe), Ok(current)) => exe == current,
        _ => false,
    }
}

/// Waits for `healthy` to be set, looking every `poll`. False once `deadline` has passed.
fn watch(healthy: &AtomicBool, deadline: Duration, poll: Duration) -> bool {
    let end = Instant::now() + deadline;
    while !healthy.load(Ordering::Relaxed) {
        if Instant::now() >= end {
            return false;
        }
        std::thread::sleep(poll);
    }
    true
}

/// True when an update is on trial in `home`, or when `update.json` cannot be read, so a caller
/// that must stay out of a trial stays out when it cannot tell.
pub fn trial_on(home: &Path) -> bool {
    !state(home).is_ok_and(|state| state.trial.is_none())
}

/// True when this process runs from `current` and a trial of `running` is on.
fn on_trial(home: &Path, exe: &Path, running: &str) -> bool {
    from_current(home, exe, running)
        && state(home).is_ok_and(|s| s.trial.is_some_and(|t| t.version == running))
}

/// What [`begin`] decided.
#[derive(Debug, PartialEq)]
enum Begin {
    /// Not run from `current`: nothing counted, nothing written.
    Skip,
    /// Carry on. `watch` is true when a trial of this version is on, so the watchdog is armed.
    Run { watch: bool },
    /// The trial failed and `current` went back to `to`.
    RolledBack { to: String },
    /// The trial failed and the rollback stopped part way, for `error`.
    RollbackStopped { error: String },
}

/// What a start does about an update, for the process at `exe`.
fn begin(home: &Path, exe: &Path, running: &str) -> io::Result<Begin> {
    if !from_current(home, exe, running) {
        return Ok(Begin::Skip);
    }
    Ok(match start_with(home, running)? {
        Start::Run => Begin::Run {
            watch: on_trial(home, exe, running),
        },
        Start::RolledBack { to } => Begin::RolledBack { to },
        Start::RollbackStopped { error } => Begin::RollbackStopped { error },
    })
}

/// Called by `foundryd run` before it does anything else. False means this version was
/// rolled back, or its rollback stopped part way, and the process must exit with success so
/// the service starts the previous one or tries the rollback again.
/// A start that goes on while a trial of this version is on gets a watchdog: if the daemon is
/// not healthy by [`TRIAL_DEADLINE`] the process exits, so a hang counts as a failed start.
pub fn starting(home: &Path) -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return true;
    };
    starting_with(home, &exe, env!("CARGO_PKG_VERSION"), || {
        std::thread::spawn(|| {
            if !watch_until_healthy(&HEALTHY) {
                std::process::exit(1);
            }
        });
    })
}

/// [`starting`] for the process at `exe`; `arm` starts the watchdog.
fn starting_with(home: &Path, exe: &Path, running: &str, arm: impl FnOnce()) -> bool {
    match begin(home, exe, running) {
        Ok(Begin::Skip) => true,
        Ok(Begin::Run { watch }) => {
            if watch {
                arm();
            }
            true
        }
        Ok(Begin::RolledBack { to }) => {
            tracing::error!(
                "version {running} did not become healthy in {MAX_STARTS} starts: back to {to}, and v{running} is refused until a higher tag appears"
            );
            false
        }
        Ok(Begin::RollbackStopped { error }) => {
            tracing::error!(
                "version {running} did not become healthy in {MAX_STARTS} starts, and putting the previous version back stopped part way: {error}; exiting so the next start tries again"
            );
            false
        }
        Err(error) => {
            tracing::error!("cannot check the update trial: {error}");
            true
        }
    }
}

/// Waits for `flag` up to [`TRIAL_DEADLINE`] and logs when it does not come.
fn watch_until_healthy(flag: &AtomicBool) -> bool {
    let healthy = watch(flag, TRIAL_DEADLINE, Duration::from_secs(1));
    if !healthy {
        tracing::error!(
            "not healthy after {} s: exiting so this start counts as failed",
            TRIAL_DEADLINE.as_secs()
        );
    }
    healthy
}

/// Ends the trial when it is of `running`: this version has stayed up. False when the state
/// could not be read or saved, so the caller tries again.
fn mark_healthy(home: &Path, running: &str) -> bool {
    let result = lock(home).and_then(|_lock| {
        let mut state = state(home)?;
        if state.trial.as_ref().is_some_and(|t| t.version == running) {
            state.trial = None;
            save(home, &state)?;
        }
        Ok(())
    });
    if let Err(error) = result {
        tracing::warn!("cannot mark {running} healthy, trying again at the next check: {error}");
        return false;
    }
    true
}

/// True when `version` is lower than `before`, the version `current` named.
pub fn lowered(before: Option<&str>, version: &str) -> bool {
    before.is_some_and(|before| parse(before).is_some() && parse(before) > parse(version))
}

/// True when `version` and `running` are both versions and `version` is the higher.
pub(crate) fn above(version: &str, running: &str) -> bool {
    matches!((parse(version), parse(running)), (Some(version), Some(running)) if version > running)
}

/// Records a hand install of `version` over `before` (the version `current` named). It runs as
/// the `record` of [`place_version`] or [`commit_version`], both of which hold the lock, so it
/// takes none. A trial `current` does not name is stranded and is cleared, as is a trial of
/// any other version; a trial of `version` that `current` names stays, so installing it again
/// does not stop its rollback. When `version` is lower it refuses `v<before>`, as an automatic
/// rollback does, so the updater does not undo the install until a higher tag ships; a higher
/// refusal already held stays. Returns the tag that is refused after a go-back, and `None`
/// when the version did not go down. It clears the ready tag, whatever version it installs.
pub fn placed_by_hand(
    home: &Path,
    version: &str,
    before: Option<&str>,
) -> io::Result<Option<String>> {
    let mut state = state(home)?;
    // `current` still names `before` here, so a trial it does not name is one a crash stranded.
    clear_stranded_trial(home, &mut state);
    // Installing the version that is on trial again must not stop its rollback.
    if state.trial.as_ref().is_some_and(|t| t.version != version) {
        state.trial = None;
    }
    state.ready = None;
    let went_back = lowered(before, version);
    if let Some(before) = before.filter(|_| went_back) {
        let held = state
            .refused
            .as_deref()
            .and_then(|r| parse(tag_version(r)?));
        if held.is_none_or(|held| held < parse(before).unwrap_or_default()) {
            state.refused = Some(format!("v{before}"));
        }
    }
    save(home, &state)?;
    Ok(state.refused.filter(|_| went_back))
}

/// The tag to make ready: the highest valid one, if it is above `running` and is not the
/// refused one. A tag above the refused one is picked.
fn pick(tags: &[String], running: &str, refused: Option<&str>) -> Option<String> {
    let (top, version) = tags
        .iter()
        .filter_map(|tag| Some((tag, parse(tag_version(tag)?)?)))
        .max_by_key(|(_, version)| *version)?;
    let beyond = version > parse(running)?;
    (beyond && refused != Some(top.as_str())).then(|| top.clone())
}

/// Makes `versions/<version>` from a fresh stage directory: `fill` puts the files in it,
/// both binaries must be there and are made executable. Then, under one hold of [`lock`], the
/// stage is swapped in, `record` runs and `current` is pointed at it, so nothing runs between
/// them. The stage is filled before the lock is taken. `record` must not take the lock itself.
pub(crate) fn place_version(
    home: &Path,
    version: &str,
    fill: impl FnOnce(&Path) -> Result<(), UpdateError>,
    record: impl FnOnce() -> io::Result<()>,
) -> Result<(), UpdateError> {
    let stage = stage_version(home, version, fill)?;
    let result = lock(home)
        .map_err(io_error("cannot lock the update state"))
        .and_then(|_lock| {
            swap_in(home, &stage, version)?;
            record_and_point(home, version, record)
        });
    if result.is_err() {
        let _ = fs::remove_dir_all(&stage);
    }
    result
}

/// Fills a fresh `versions/.staging-<version>-<pid>` with `fill` and checks that both binaries
/// are there, made executable. Returns its path; a stage that fails is removed. The stage is this
/// process's own, so a hand install and the daemon's check never wipe each other's. It takes no
/// lock: nothing under it touches `update.json` or `current`.
fn stage_version(
    home: &Path,
    version: &str,
    fill: impl FnOnce(&Path) -> Result<(), UpdateError>,
) -> Result<PathBuf, UpdateError> {
    let stage = home
        .join("versions")
        .join(format!(".staging-{version}-{}", std::process::id()));
    sweep_stages(home);
    let filled = (|| {
        let _ = fs::remove_dir_all(&stage);
        fs::create_dir_all(&stage).map_err(io_error("cannot make the staging directory"))?;
        fill(&stage)?;
        make_executable(&stage)
    })();
    match filled {
        Ok(()) => Ok(stage),
        Err(error) => {
            let _ = fs::remove_dir_all(&stage);
            Err(error)
        }
    }
}

/// Removes every stage under `versions/` whose process has exited, so a process killed
/// mid-stage leaves nothing for good. A stage of a live process is its own and is left alone,
/// and so is one an older release named without a pid: its name says nothing of whether the
/// process filling it is alive.
fn sweep_stages(home: &Path) {
    let Ok(entries) = fs::read_dir(home.join("versions")) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(rest) = name.to_str().and_then(|n| n.strip_prefix(".staging-")) else {
            continue;
        };
        let dead = rest
            .rsplit_once('-')
            .and_then(|(_, pid)| pid.parse::<i32>().ok())
            .is_some_and(|pid| kill(Pid::from_raw(pid), None) == Err(Errno::ESRCH));
        if dead {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// Renames `stage` to `versions/<version>`, replacing what is there. It takes no lock: a caller
/// that must not race the check calls it under [`lock`], as [`make_ready`] does.
fn swap_in(home: &Path, stage: &Path, version: &str) -> Result<(), UpdateError> {
    let target = home.join("versions").join(version);
    match fs::remove_dir_all(&target) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(io_error("cannot clear the old copy")(error)),
    }
    fs::rename(stage, &target).map_err(io_error("cannot move the new version into place"))
}

/// Checks that `stage` holds both binaries and makes them executable.
fn make_executable(stage: &Path) -> Result<(), UpdateError> {
    for name in BINARIES {
        let path = stage.join(name);
        if !path.is_file() {
            return Err(UpdateError::Missing(name));
        }
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
            .map_err(io_error(format!("cannot make {name} executable")))?;
    }
    Ok(())
}

/// Runs `record`, then points `current` at `versions/<version>`, both under [`lock`]. `record`
/// runs first, so a crash in between never leaves a version running that nothing recorded.
/// `record` must not take the lock itself.
pub(crate) fn commit_version(
    home: &Path,
    version: &str,
    record: impl FnOnce() -> io::Result<()>,
) -> Result<(), UpdateError> {
    let _lock = lock(home).map_err(io_error("cannot lock the update state"))?;
    record_and_point(home, version, record)
}

/// The body of [`commit_version`], for a caller that already holds [`lock`].
fn record_and_point(
    home: &Path,
    version: &str,
    record: impl FnOnce() -> io::Result<()>,
) -> Result<(), UpdateError> {
    record().map_err(io_error("cannot save the update state"))?;
    point_current(home, version).map_err(io_error("cannot point current at the new version"))
}

/// The tag the sample's `update_ready` would report for `state`: the ready tag while it is above
/// `running`, is not refused, has both binaries on disk and the daemon at `exe` runs from
/// `current`.
fn reportable(home: &Path, state: &State, exe: &Path, running: &str) -> Option<String> {
    let ready = state.ready.as_deref()?;
    (tag_version(ready).is_some_and(|version| above(version, running))
        && state.refused.as_deref() != Some(ready)
        && is_ready(home, ready)
        && from_current(home, exe, running))
    .then(|| ready.to_string())
}

/// Installs `tag`, the ready tag, when asked: it saves a trial of its version with `running` as
/// `previous`, clears the ready tag in the same write, then points `current` at it. It never
/// stops the daemon; its caller does. The whole call holds [`lock`]. A trial `current` does not
/// name is cleared first, and stays cleared whatever the call answers. It refuses, with
/// [`InstallError::NotReady`] and nothing else changed, when `tag` is not the ready tag (or
/// its binaries are gone), is not above `running` or is refused, when `exe` is not run from
/// `<home>/current`, when `current` does not name `running`, or when a trial is on. A refusal
/// of a tag below `tag` is cleared; a higher one stays. If `current` cannot be pointed the state
/// is saved back as it was.
pub fn install_ready(
    home: &Path,
    tag: &str,
    running: &str,
    exe: &Path,
) -> Result<(), InstallError> {
    let _lock = lock(home).map_err(io_error("cannot lock the update state"))?;
    let mut held = state(home).map_err(io_error("cannot read the update state"))?;
    let cleared = clear_stranded_trial(home, &mut held);
    let ready = reportable(home, &held, exe, running);
    let verdict: Result<&str, String> =
        match tag_version(tag).filter(|_| held.ready.as_deref() == Some(tag)) {
            None => Err(format!("{tag} is not the ready tag")),
            Some(_) if !is_ready(home, tag) => {
                Err(format!("{tag} is not ready: its binaries are gone"))
            }
            Some(version) if !above(version, running) => {
                Err(format!("{tag} is not above the running version {running}"))
            }
            Some(_) if held.refused.as_deref() == Some(tag) => Err(format!("{tag} is refused")),
            Some(_) if current_version(home).as_deref() != Some(running) => Err(format!(
                "current does not name the running version {running}"
            )),
            Some(_) if !from_current(home, exe, running) => Err(format!(
                "this daemon is not run from {}",
                home.join("current").display()
            )),
            Some(version) => match &held.trial {
                Some(trial) => Err(format!("a trial of {} is on", trial.version)),
                None => Ok(version),
            },
        };
    let version = match verdict {
        Err(reason) => {
            if cleared {
                save(home, &held).map_err(io_error("cannot save the update state"))?;
            }
            return Err(InstallError::NotReady { ready, reason });
        }
        Ok(version) => version,
    };
    let mut next = held.clone();
    next.trial = Some(Trial {
        version: version.to_string(),
        previous: running.to_string(),
        starts: 0,
    });
    next.ready = None;
    if held
        .refused
        .as_deref()
        .and_then(tag_version)
        .is_none_or(|refused| above(version, refused))
    {
        next.refused = None;
    }
    save(home, &next).map_err(io_error("cannot save the update state"))?;
    if let Err(error) = point_current(home, version) {
        if let Err(restore) = save(home, &held) {
            tracing::warn!("the update state could not be put back: {restore}");
        }
        return Err(io_error("cannot point current at the new version")(error).into());
    }
    Ok(())
}

/// Makes `tag` ready: `fetch` puts the release's files in a fresh stage directory, the
/// binaries are checked and the new `foundryd` must say it is this version. Then, in one step
/// under [`lock`], the stage becomes `versions/<version>` and `tag` is saved as the ready tag,
/// leaving `refused`, `current` and any live trial as they are. That step goes ahead only while
/// `current` still names `running` and no trial is live; otherwise it changes nothing and the
/// stage is removed.
fn make_ready(
    home: &Path,
    tag: &str,
    running: &str,
    version_limit: Duration,
    fetch: impl FnOnce(&str, &Path) -> Result<(), UpdateError>,
) -> Result<(), UpdateError> {
    let version = tag_version(tag).ok_or_else(|| UpdateError::BadTag(tag.to_string()))?;
    let stage = stage_version(home, version, |stage| {
        fetch(tag, stage)?;
        make_executable(stage)?;
        let result = run_bounded(
            Command::new(stage.join("foundryd")).arg("--version"),
            version_limit,
        );
        judge_version_check(result, tag, version)
    })?;
    let placed = lock(home)
        .map_err(io_error("cannot lock the update state"))
        .and_then(|_lock| {
            let mut held = state(home).map_err(io_error("cannot read the update state"))?;
            let waits = |why: String| io_error(format!("{tag} waits"))(io::Error::other(why));
            // Checked again under the lock: an install may have moved `current` while the
            // release was downloaded, and what it did must stand.
            if current_version(home).as_deref() != Some(running) {
                return Err(waits(format!(
                    "current no longer names {running}: a hand install moved it"
                )));
            }
            clear_stranded_trial(home, &mut held);
            if let Some(trial) = held.trial.as_ref() {
                return Err(waits(format!("a trial of {} is on", trial.version)));
            }
            swap_in(home, &stage, version)?;
            held.ready = Some(tag.to_string());
            save(home, &held).map_err(io_error("cannot save the update state"))
        });
    if placed.is_err() {
        let _ = fs::remove_dir_all(&stage);
    }
    placed
}

/// Clears, under [`lock`], a stranded trial and a ready tag `tags` does not hold (that release
/// was pulled). The ready tag is cleared only if the state re-read under the lock still names
/// it. Returns the state that remains. The lock is taken only when there is something to clear.
fn tidy(home: &Path, tags: &[String]) -> io::Result<State> {
    let seen = state(home)?;
    let pulled = seen.ready.clone().filter(|ready| !tags.contains(ready));
    if pulled.is_none() && !stranded(home, &seen) {
        return Ok(seen);
    }
    let _lock = lock(home)?;
    let mut held = state(home)?;
    let mut changed = clear_stranded_trial(home, &mut held);
    if pulled.is_some() && held.ready == pulled {
        held.ready = None;
        changed = true;
    }
    if changed {
        save(home, &held)?;
    }
    Ok(held)
}

/// Judges what happened when the new `foundryd` was asked its version. A binary the system
/// cannot execute, or one that ran and failed or printed the wrong text, is a bad asset. A
/// timeout or a passing system error says nothing about the release: that is retried.
fn judge_version_check(
    result: Result<String, RunError>,
    tag: &str,
    version: &str,
) -> Result<(), UpdateError> {
    let bad = |detail: String| UpdateError::BadCheck {
        tag: tag.to_string(),
        detail,
    };
    match result {
        Ok(printed) if printed == format!("foundryd {version}\n") => Ok(()),
        Ok(printed) => Err(UpdateError::WrongVersion {
            tag: tag.to_string(),
            version: version.to_string(),
            printed: printed.trim().to_string(),
        }),
        Err(error @ RunError::Failed { .. }) => Err(bad(error.to_string())),
        Err(RunError::Spawn { name, source }) if unrunnable(&source) => {
            Err(bad(format!("{name} cannot be executed: {source}")))
        }
        Err(error) => Err(UpdateError::Fetch(error.to_string())),
    }
}

/// The release tags on GitHub, read with the machine's own `gh` login.
fn list_tags() -> Result<Vec<String>, UpdateError> {
    list_tags_with(OsStr::new("gh"))
}

/// [`list_tags`] with the `gh` program given.
fn list_tags_with(gh: &OsStr) -> Result<Vec<String>, UpdateError> {
    let out = run_bounded(
        Command::new(gh).args([
            "release",
            "list",
            "--repo",
            REPO,
            "--exclude-drafts",
            "--exclude-pre-releases",
            "--limit",
            "100",
            "--json",
            "tagName",
            "--jq",
            ".[].tagName",
        ]),
        LIST_LIMIT,
    )
    .map_err(|error| UpdateError::Fetch(error.to_string()))?;
    Ok(out
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect())
}

/// The asset a release holds for this machine.
fn asset() -> String {
    format!(
        "foundryd-{}-{}.tar.gz",
        std::env::consts::ARCH,
        std::env::consts::OS
    )
}

/// Downloads this machine's asset of `tag` and its signature into `stage`, and unpacks it there
/// once the signature verifies against [`RELEASE_KEY`].
fn download(tag: &str, stage: &Path) -> Result<(), UpdateError> {
    download_with(OsStr::new("gh"), &RELEASE_KEY, tag, stage)
}

/// [`download`] with the `gh` program and the public key given. A signature that cannot be
/// downloaded is a fetch error, tried again later; one that does not verify is
/// [`UpdateError::BadSignature`]. Either way nothing is unpacked.
fn download_with(gh: &OsStr, key: &[u8], tag: &str, stage: &Path) -> Result<(), UpdateError> {
    let asset = asset();
    let sig = format!("{asset}.sig");
    run_bounded(
        Command::new(gh)
            .args(["release", "download", tag, "--repo", REPO])
            .args(["--pattern", &asset, "--pattern", &sig])
            .arg("--dir")
            .arg(stage),
        DOWNLOAD_LIMIT,
    )
    .map_err(|error| UpdateError::Fetch(error.to_string()))?;
    let read = |name: &str| {
        fs::read(stage.join(name))
            .map_err(|error| UpdateError::Fetch(format!("cannot read {name} of {tag}: {error}")))
    };
    let signature = read(&sig)?;
    if !verifies(key, &read(&asset)?, &signature) {
        return Err(UpdateError::BadSignature(tag.to_string()));
    }
    let _ = fs::remove_file(stage.join(&sig));
    let archive = stage.join(&asset);
    run_bounded(
        Command::new("tar")
            .arg("-xzf")
            .arg(&archive)
            .arg("-C")
            .arg(stage),
        TAR_LIMIT,
    )
    .map_err(|error| UpdateError::Fetch(error.to_string()))?;
    let _ = fs::remove_file(archive);
    Ok(())
}

/// True when `signature` is the Ed25519 signature of `archive` by `key`, as the release
/// workflow makes it (`openssl pkeyutl -sign -rawin`).
fn verifies(key: &[u8], archive: &[u8], signature: &[u8]) -> bool {
    UnparsedPublicKey::new(&ED25519, key)
        .verify(archive, signature)
        .is_ok()
}

/// One look for a newer tag. `Some` is the tag this look made ready. A ready tag the list no
/// longer holds is cleared first, and a tag already ready is not fetched again. A release is
/// refused, so it is not downloaded again, when its signature does not verify, when it lacks a
/// binary, when its `foundryd` cannot be executed, or when that `foundryd` ran and failed or
/// printed the wrong version. Any other failure, including a missing signature or a version
/// check that times out or hits a passing system error, is tried again later. Neither changes
/// the ready tag.
fn check_with(
    home: &Path,
    running: &str,
    version_limit: Duration,
    list: impl FnOnce() -> Result<Vec<String>, UpdateError>,
    fetch: impl FnOnce(&str, &Path) -> Result<(), UpdateError>,
) -> Result<Option<String>, UpdateError> {
    // A hand install moved `current` away from this daemon: the service starts that version
    // at its next restart, so this one makes nothing ready over it.
    if current_version(home).as_deref() != Some(running) {
        return Ok(None);
    }
    let tags = list()?;
    let held = tidy(home, &tags).map_err(io_error("cannot read or tidy the update state"))?;
    let Some(tag) = pick(&tags, running, held.refused.as_deref()) else {
        return Ok(None);
    };
    if held.ready.as_deref() == Some(tag.as_str()) && is_ready(home, &tag) {
        return Ok(None);
    }
    match make_ready(home, &tag, running, version_limit, fetch) {
        Ok(()) => Ok(Some(tag)),
        Err(
            error @ (UpdateError::BadSignature(_)
            | UpdateError::WrongVersion { .. }
            | UpdateError::BadCheck { .. }
            | UpdateError::Missing(_)),
        ) => {
            let refuse = lock(home).and_then(|_lock| {
                let mut state = state(home)?;
                state.refused = Some(tag);
                save(home, &state)
            });
            if let Err(error) = refuse {
                tracing::warn!("cannot record the refused tag: {error}");
            }
            Err(error)
        }
        Err(error) => Err(error),
    }
}

/// What the daemon does once it has run for a minute: tells the watchdog through `flag`, and
/// says whether it runs from `current`. False means updates are off for this process.
fn became_healthy(flag: &AtomicBool, home: &Path, exe: &Path, running: &str) -> bool {
    flag.store(true, Ordering::Relaxed);
    from_current(home, exe, running)
}

/// Marks this version healthy after a minute (again at each check until the mark is saved),
/// then looks for a newer tag every ten minutes and makes it ready. It never installs one and
/// never stops the daemon. Never returns: a daemon that is not run from `current` just waits.
pub async fn run(home: PathBuf) {
    let running = env!("CARGO_PKG_VERSION");
    let work = home.clone();
    run_with(
        home,
        std::env::current_exe(),
        running,
        &HEALTHY,
        move || check_with(&work, running, VERSION_LIMIT, list_tags, download),
    )
    .await;
}

/// [`run`] with its inputs given: this process's path, the flag the watchdog reads, and one
/// update check.
async fn run_with(
    home: PathBuf,
    exe: io::Result<PathBuf>,
    running: &'static str,
    flag: &AtomicBool,
    check: impl Fn() -> Result<Option<String>, UpdateError> + Clone + Send + 'static,
) {
    tokio::time::sleep(HEALTHY_AFTER).await;
    // The flag is set whatever `exe` is, so a failed lookup never lets the watchdog kill a
    // healthy daemon.
    flag.store(true, Ordering::Relaxed);
    let on = exe.is_ok_and(|exe| became_healthy(flag, &home, &exe, running));
    if !on {
        tracing::info!(
            "updates are off: this daemon is not run from {}",
            home.join("current").display()
        );
        pending::<()>().await;
    }
    let mut marked = false;
    loop {
        if !marked {
            let home = home.clone();
            marked = tokio::task::spawn_blocking(move || mark_healthy(&home, running))
                .await
                .unwrap_or(false);
        }
        match tokio::task::spawn_blocking(check.clone()).await {
            Ok(Ok(Some(tag))) => {
                tracing::info!("{tag} is downloaded and checked, and waits to be installed");
            }
            Ok(Ok(None)) => {}
            Ok(Err(error)) => tracing::warn!("update check failed, trying again later: {error}"),
            Err(error) => tracing::warn!("update check stopped: {error}"),
        }
        tokio::time::sleep(CHECK_EVERY).await;
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::AtomicBool;
    use std::time::Instant;

    use nix::errno::Errno;
    use ring::rand::SystemRandom;
    use ring::signature::{Ed25519KeyPair, KeyPair};

    use super::*;

    /// A data directory with versions 0.1.0 and 0.2.0 on disk, `current` on 0.2.0 and a
    /// trial of 0.2.0 that has not started yet.
    fn trial_home() -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        for version in ["0.1.0", "0.2.0"] {
            let dir = home.path().join("versions").join(version);
            fs::create_dir_all(&dir).unwrap();
            release(&dir, version);
        }
        point_current(home.path(), "0.2.0").unwrap();
        save(
            home.path(),
            &State {
                trial: Some(Trial {
                    version: "0.2.0".into(),
                    previous: "0.1.0".into(),
                    starts: 0,
                }),
                refused: None,
                ready: None,
            },
        )
        .unwrap();
        home
    }

    fn current(home: &Path) -> PathBuf {
        fs::read_link(home.join("current")).unwrap()
    }

    #[test]
    fn a_trial_is_on_while_update_json_holds_one_or_cannot_be_read() {
        let home = trial_home();
        assert!(trial_on(home.path()));

        mark_healthy(home.path(), "0.2.0");
        assert!(!trial_on(home.path()));

        fs::write(home.path().join("update.json"), "not json").unwrap();
        assert!(trial_on(home.path()));

        assert!(!trial_on(tempfile::tempdir().unwrap().path()));
    }

    #[test]
    fn three_starts_that_never_report_healthy_roll_back_on_the_next() {
        let home = trial_home();

        for _ in 0..3 {
            assert_eq!(start_with(home.path(), "0.2.0").unwrap(), Start::Run);
        }
        let fourth = start_with(home.path(), "0.2.0").unwrap();

        assert_eq!(fourth, Start::RolledBack { to: "0.1.0".into() });
        assert_eq!(current(home.path()), PathBuf::from("versions/0.1.0"));
        let state = state(home.path()).unwrap();
        assert_eq!(state.refused.as_deref(), Some("v0.2.0"));
        assert_eq!(state.trial, None);
    }

    #[test]
    fn a_version_that_turns_healthy_before_the_third_start_is_not_rolled_back() {
        let home = trial_home();
        start_with(home.path(), "0.2.0").unwrap();
        start_with(home.path(), "0.2.0").unwrap();

        mark_healthy(home.path(), "0.2.0");

        assert_eq!(state(home.path()).unwrap().trial, None);
        assert_eq!(start_with(home.path(), "0.2.0").unwrap(), Start::Run);
        assert_eq!(current(home.path()), PathBuf::from("versions/0.2.0"));
        assert_eq!(state(home.path()).unwrap().refused, None);
    }

    #[test]
    fn healthy_for_another_version_keeps_the_trial() {
        let home = trial_home();

        mark_healthy(home.path(), "0.1.0");

        assert!(state(home.path()).unwrap().trial.is_some());
    }

    #[test]
    fn a_start_of_a_version_that_is_not_the_trial_is_not_counted_and_writes_nothing() {
        let home = trial_home();
        let before = fs::read(home.path().join("update.json")).unwrap();

        assert_eq!(start_with(home.path(), "0.1.0").unwrap(), Start::Run);
        assert_eq!(start_with(home.path(), "9.9.9").unwrap(), Start::Run);

        assert_eq!(fs::read(home.path().join("update.json")).unwrap(), before);
    }

    #[test]
    fn a_start_with_no_state_file_runs_and_writes_nothing() {
        let home = tempfile::tempdir().unwrap();

        assert_eq!(start_with(home.path(), "0.1.0").unwrap(), Start::Run);

        assert!(!home.path().join("update.json").exists());
    }

    #[test]
    fn a_rollback_to_a_version_that_is_gone_is_an_error_and_changes_nothing() {
        let home = trial_home();
        fs::remove_dir_all(home.path().join("versions/0.1.0")).unwrap();
        for _ in 0..3 {
            start_with(home.path(), "0.2.0").unwrap();
        }

        let error = start_with(home.path(), "0.2.0").unwrap_err();

        assert!(error.to_string().contains("0.1.0"), "{error}");
        assert_eq!(current(home.path()), PathBuf::from("versions/0.2.0"));
        assert_eq!(state(home.path()).unwrap().refused, None);
    }

    #[test]
    fn the_state_file_is_private() {
        let home = trial_home();

        let mode = fs::metadata(home.path().join("update.json"))
            .unwrap()
            .permissions()
            .mode();

        assert_eq!(mode & 0o777, 0o600);
    }

    const LIMIT: Duration = Duration::from_secs(10);

    fn tags(list: &[&str]) -> Vec<String> {
        list.iter().map(|tag| tag.to_string()).collect()
    }

    #[test]
    fn tag_version_reads_only_v_and_three_numbers() {
        assert_eq!(tag_version("v0.2.0"), Some("0.2.0"));
        assert_eq!(tag_version("v10.0.1"), Some("10.0.1"));
        for tag in [
            "0.2.0",
            "v0.2",
            "v0.2.0.1",
            "v0.2.0-rc1",
            "v../x",
            "V0.2.0",
            "",
        ] {
            assert_eq!(tag_version(tag), None, "{tag}");
        }
    }

    #[test]
    fn pick_takes_the_highest_tag_above_the_running_version() {
        let picked = pick(&tags(&["v0.2.0", "v0.3.0", "v0.1.5"]), "0.2.0", None);

        assert_eq!(picked.as_deref(), Some("v0.3.0"));
    }

    #[test]
    fn pick_compares_numbers_not_text() {
        let picked = pick(&tags(&["v0.9.0", "v0.10.0"]), "0.9.0", None);

        assert_eq!(picked.as_deref(), Some("v0.10.0"));
    }

    #[test]
    fn pick_takes_nothing_when_the_highest_is_not_above_running() {
        assert_eq!(pick(&tags(&["v0.1.0", "v0.2.0"]), "0.2.0", None), None);
        assert_eq!(pick(&tags(&["v0.1.0"]), "0.2.0", None), None);
        assert_eq!(pick(&[], "0.2.0", None), None);
    }

    #[test]
    fn pick_skips_a_refused_tag_while_it_is_the_highest() {
        let list = tags(&["v0.2.0", "v0.3.0"]);

        assert_eq!(pick(&list, "0.2.0", Some("v0.3.0")), None);
    }

    #[test]
    fn pick_takes_a_tag_above_the_refused_one() {
        let list = tags(&["v0.3.0", "v0.4.0"]);

        assert_eq!(
            pick(&list, "0.2.0", Some("v0.3.0")).as_deref(),
            Some("v0.4.0")
        );
    }

    #[test]
    fn pick_ignores_tags_that_are_not_three_numbers() {
        let list = tags(&[
            "v1.0",
            "v1.0.0-rc1",
            "1.0.0",
            "vx.y.z",
            "v1.0.0.1",
            "latest",
            "",
        ]);

        assert_eq!(pick(&list, "0.2.0", None), None);
    }

    /// Writes a shell script that answers `--version` with `printed`.
    fn script(path: &Path, printed: &str) {
        fs::write(path, format!("#!/bin/sh\necho \"{printed}\"\n")).unwrap();
    }

    fn installed_home() -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.path().join("versions/0.1.0")).unwrap();
        point_current(home.path(), "0.1.0").unwrap();
        home
    }

    #[test]
    fn make_ready_places_the_version_and_saves_it_ready_without_moving_current() {
        let home = installed_home();
        save(
            home.path(),
            &State {
                trial: None,
                refused: Some("v0.2.0".into()),
                ready: None,
            },
        )
        .unwrap();

        make_ready(home.path(), "v0.2.0", "0.1.0", LIMIT, |tag, stage| {
            assert_eq!(tag, "v0.2.0");
            script(&stage.join("foundryd"), "foundryd 0.2.0");
            script(&stage.join("foundry-holder"), "holder");
            Ok(())
        })
        .unwrap();

        assert_eq!(current(home.path()), PathBuf::from("versions/0.1.0"));
        for name in ["foundryd", "foundry-holder"] {
            let mode = fs::metadata(home.path().join("versions/0.2.0").join(name))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o755, 0o755, "{name}");
        }
        assert_eq!(
            state(home.path()).unwrap(),
            State {
                trial: None,
                refused: Some("v0.2.0".into()),
                ready: Some("v0.2.0".into()),
            }
        );
        assert_eq!(stages(home.path()), Vec::<String>::new());
    }

    #[test]
    fn make_ready_refuses_a_binary_that_prints_another_version() {
        let home = installed_home();

        let error = make_ready(home.path(), "v0.2.0", "0.1.0", LIMIT, |_, stage| {
            script(&stage.join("foundryd"), "foundryd 0.9.9");
            script(&stage.join("foundry-holder"), "holder");
            Ok(())
        })
        .unwrap_err();

        assert!(error.to_string().contains("foundryd 0.9.9"), "{error}");
        assert_eq!(current(home.path()), PathBuf::from("versions/0.1.0"));
        assert!(!home.path().join("versions/0.2.0").exists());
        assert_eq!(state(home.path()).unwrap(), State::default());
    }

    #[test]
    fn make_ready_refuses_an_asset_without_the_holder() {
        let home = installed_home();

        let error = make_ready(home.path(), "v0.2.0", "0.1.0", LIMIT, |_, stage| {
            script(&stage.join("foundryd"), "foundryd 0.2.0");
            Ok(())
        })
        .unwrap_err();

        assert!(error.to_string().contains("foundry-holder"), "{error}");
        assert_eq!(current(home.path()), PathBuf::from("versions/0.1.0"));
    }

    #[test]
    fn make_ready_passes_a_failed_fetch_on_and_changes_nothing() {
        let home = installed_home();

        let error = make_ready(home.path(), "v0.2.0", "0.1.0", LIMIT, |_, _| {
            Err(UpdateError::Fetch("gh is not logged in".into()))
        })
        .unwrap_err();

        assert!(error.to_string().contains("gh is not logged in"), "{error}");
        assert_eq!(current(home.path()), PathBuf::from("versions/0.1.0"));
    }

    #[test]
    fn from_current_needs_current_to_name_the_version_and_the_exe_to_be_reached_through_it() {
        let home = installed_home();
        let inside = home.path().join("versions/0.1.0/foundryd");
        script(&inside, "foundryd 0.1.0");
        let elsewhere = home.path().join("elsewhere");
        script(&elsewhere, "foundryd 0.1.0");

        assert!(from_current(
            home.path(),
            &home.path().join("current/foundryd"),
            "0.1.0"
        ));
        assert!(from_current(home.path(), &inside, "0.1.0"));
        assert!(!from_current(home.path(), &elsewhere, "0.1.0"));
        assert!(!from_current(home.path(), &inside, "0.2.0"));
        assert!(!from_current(
            tempfile::tempdir().unwrap().path(),
            &inside,
            "0.1.0"
        ));
    }

    #[test]
    fn a_rollback_that_fails_part_way_rolls_back_on_the_next_start() {
        let home = trial_home();
        for _ in 0..3 {
            start_with(home.path(), "0.2.0").unwrap();
        }
        let obstacle = home.path().join("current.new");
        fs::create_dir_all(obstacle.join("inside")).unwrap();

        let stopped = start_with(home.path(), "0.2.0").unwrap();
        assert!(
            matches!(stopped, Start::RollbackStopped { .. }),
            "{stopped:?}"
        );
        assert_eq!(current(home.path()), PathBuf::from("versions/0.2.0"));
        fs::remove_dir_all(&obstacle).unwrap();
        let next = start_with(home.path(), "0.2.0").unwrap();

        assert_eq!(next, Start::RolledBack { to: "0.1.0".into() });
        assert_eq!(current(home.path()), PathBuf::from("versions/0.1.0"));
        let state = state(home.path()).unwrap();
        assert_eq!(state.refused.as_deref(), Some("v0.2.0"));
        assert_eq!(state.trial, None);
    }

    #[test]
    fn the_watchdog_gives_up_when_the_daemon_is_never_healthy() {
        let flag = AtomicBool::new(false);

        let started = Instant::now();
        let healthy = watch(&flag, Duration::from_millis(60), Duration::from_millis(5));

        assert!(!healthy);
        assert!(started.elapsed() >= Duration::from_millis(60));
    }

    #[test]
    fn the_watchdog_returns_as_soon_as_the_daemon_is_healthy() {
        let flag = AtomicBool::new(true);

        let started = Instant::now();
        let healthy = watch(&flag, Duration::from_secs(30), Duration::from_millis(5));

        assert!(healthy);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_hand_install_of_an_older_version_refuses_the_newer_one_until_a_higher_tag() {
        let home = tempfile::tempdir().unwrap();
        for version in ["0.1.0", "0.2.0"] {
            fs::create_dir_all(home.path().join("versions").join(version)).unwrap();
        }
        point_current(home.path(), "0.2.0").unwrap();
        save(
            home.path(),
            &State {
                trial: Some(Trial {
                    version: "0.2.0".into(),
                    previous: "0.1.0".into(),
                    starts: 1,
                }),
                refused: None,
                ready: None,
            },
        )
        .unwrap();
        let before = current_version(home.path());
        assert_eq!(before.as_deref(), Some("0.2.0"));

        point_current(home.path(), "0.1.0").unwrap();
        placed_by_hand(home.path(), "0.1.0", before.as_deref()).unwrap();

        let state = state(home.path()).unwrap();
        assert_eq!(state.trial, None);
        assert_eq!(state.refused.as_deref(), Some("v0.2.0"));
        let list = tags(&["v0.1.0", "v0.2.0"]);
        assert_eq!(pick(&list, "0.1.0", state.refused.as_deref()), None);
        let list = tags(&["v0.1.0", "v0.2.0", "v0.3.0"]);
        assert_eq!(
            pick(&list, "0.1.0", state.refused.as_deref()).as_deref(),
            Some("v0.3.0")
        );
    }

    #[test]
    fn a_hand_install_of_a_higher_version_refuses_nothing_and_clears_the_trial() {
        let home = trial_home();

        placed_by_hand(home.path(), "0.3.0", Some("0.2.0")).unwrap();

        let state = state(home.path()).unwrap();
        assert_eq!(state.trial, None);
        assert_eq!(state.refused, None);
    }

    #[test]
    fn a_hand_install_keeps_a_higher_refusal() {
        let home = tempfile::tempdir().unwrap();
        save(
            home.path(),
            &State {
                trial: None,
                refused: Some("v0.5.0".into()),
                ready: None,
            },
        )
        .unwrap();

        placed_by_hand(home.path(), "0.1.0", Some("0.2.0")).unwrap();

        assert_eq!(
            state(home.path()).unwrap().refused.as_deref(),
            Some("v0.5.0")
        );
    }

    #[test]
    fn current_version_reads_the_link() {
        let home = installed_home();

        assert_eq!(current_version(home.path()).as_deref(), Some("0.1.0"));
        assert_eq!(current_version(tempfile::tempdir().unwrap().path()), None);
    }

    #[test]
    fn a_command_that_outlives_its_deadline_is_killed_and_named() {
        let started = Instant::now();

        let error = run_bounded(
            Command::new("sleep").arg("31.7"),
            Duration::from_millis(300),
        )
        .unwrap_err();

        assert!(
            started.elapsed() < Duration::from_secs(20),
            "{:?}",
            started.elapsed()
        );
        assert!(matches!(error, RunError::TimedOut { .. }), "{error}");
        assert!(error.to_string().contains("sleep 31.7"), "{error}");
        let table = Command::new("ps")
            .args(["-axo", "stat,command"])
            .output()
            .unwrap();
        let table = String::from_utf8_lossy(&table.stdout);
        let left: Vec<_> = table.lines().filter(|l| l.contains("sleep 31.7")).collect();
        assert!(left.is_empty(), "still there: {left:?}");
    }

    #[test]
    fn a_command_that_finishes_gives_its_output_and_one_that_fails_gives_its_stderr() {
        let out = run_bounded(Command::new("echo").arg("hi"), Duration::from_secs(10)).unwrap();
        assert_eq!(out, "hi\n");

        let error = run_bounded(
            Command::new("sh").args(["-c", "echo no good >&2; exit 3"]),
            Duration::from_secs(10),
        )
        .unwrap_err();
        assert!(matches!(error, RunError::Failed { .. }), "{error}");
        assert!(error.to_string().contains("no good"), "{error}");

        let error = run_bounded(
            &mut Command::new("/no/such/program"),
            Duration::from_secs(10),
        )
        .unwrap_err();
        assert!(matches!(error, RunError::Spawn { .. }), "{error}");
    }

    fn release(stage: &Path, version: &str) {
        script(&stage.join("foundryd"), &format!("foundryd {version}"));
        script(&stage.join("foundry-holder"), "holder");
    }

    #[test]
    fn check_with_does_nothing_for_a_refused_tag_that_is_the_highest() {
        let home = installed_home();
        save(
            home.path(),
            &State {
                trial: None,
                refused: Some("v0.2.0".into()),
                ready: None,
            },
        )
        .unwrap();

        let done = check_with(
            home.path(),
            "0.1.0",
            LIMIT,
            || Ok(tags(&["v0.1.0", "v0.2.0"])),
            |_, _| panic!("a refused tag was fetched"),
        )
        .unwrap();

        assert_eq!(done, None);
        assert_eq!(current(home.path()), PathBuf::from("versions/0.1.0"));
    }

    #[test]
    fn check_with_makes_a_higher_tag_ready_and_leaves_current_the_trial_and_the_refusal() {
        let home = installed_home();
        save(
            home.path(),
            &State {
                trial: None,
                refused: Some("v0.2.0".into()),
                ready: None,
            },
        )
        .unwrap();

        let done = check_with(
            home.path(),
            "0.1.0",
            LIMIT,
            || Ok(tags(&["v0.1.0", "v0.2.0", "v0.3.0"])),
            |tag, stage| {
                assert_eq!(tag, "v0.3.0");
                release(stage, "0.3.0");
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(done.as_deref(), Some("v0.3.0"));
        assert_eq!(current(home.path()), PathBuf::from("versions/0.1.0"));
        assert_eq!(
            state(home.path()).unwrap(),
            State {
                trial: None,
                refused: Some("v0.2.0".into()),
                ready: Some("v0.3.0".into()),
            }
        );
        assert!(is_ready(home.path(), "v0.3.0"));
        assert_eq!(versions(home.path()), ["0.1.0", "0.3.0"]);
    }

    #[test]
    fn an_update_json_the_previous_release_wrote_without_a_ready_tag_is_read_and_made_ready() {
        let home = installed_home();
        // Written by hand in the shape the release before `ready` existed wrote, with no
        // `ready` key. `save` always writes one, so a fixture made with it cannot hold this.
        fs::write(
            home.path().join("update.json"),
            r#"{"trial":null,"refused":"v0.2.0"}"#,
        )
        .unwrap();

        assert_eq!(
            state(home.path()).unwrap(),
            State {
                trial: None,
                refused: Some("v0.2.0".into()),
                ready: None,
            }
        );

        let done = check_with(
            home.path(),
            "0.1.0",
            LIMIT,
            || Ok(tags(&["v0.1.0", "v0.2.0", "v0.3.0"])),
            |_, stage| {
                release(stage, "0.3.0");
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(done.as_deref(), Some("v0.3.0"));
        assert_eq!(
            state(home.path()).unwrap(),
            State {
                trial: None,
                refused: Some("v0.2.0".into()),
                ready: Some("v0.3.0".into()),
            }
        );
    }

    /// Puts both binaries of `version` in `versions/` and saves `v<version>` as the ready tag,
    /// with `refused`.
    fn with_ready(home: &Path, version: &str, refused: Option<&str>) {
        let dir = home.join("versions").join(version);
        fs::create_dir_all(&dir).unwrap();
        release(&dir, version);
        save(
            home,
            &State {
                trial: None,
                refused: refused.map(str::to_string),
                ready: Some(format!("v{version}")),
            },
        )
        .unwrap();
    }

    #[test]
    fn a_ready_tag_counts_only_while_both_its_binaries_are_on_disk() {
        for gone in BINARIES {
            let home = installed_home();
            with_ready(home.path(), "0.2.0", None);
            assert!(is_ready(home.path(), "v0.2.0"));

            fs::remove_file(home.path().join("versions/0.2.0").join(gone)).unwrap();

            assert!(!is_ready(home.path(), "v0.2.0"), "{gone}");
        }
        let home = installed_home();
        assert!(!is_ready(home.path(), "v0.3.0"));
        assert!(!is_ready(home.path(), "0.1.0"));
        let outside = home.path().join("x");
        fs::create_dir_all(&outside).unwrap();
        release(&outside, "0.2.0");
        assert!(!is_ready(home.path(), "v../x"));
    }

    #[test]
    fn binaries_on_disk_without_a_ready_tag_are_fetched_and_made_ready() {
        let home = installed_home();
        let dir = home.path().join("versions/0.2.0");
        fs::create_dir_all(&dir).unwrap();
        release(&dir, "0.2.0");
        let fetched = AtomicUsize::new(0);

        let done = check_with(
            home.path(),
            "0.1.0",
            LIMIT,
            || Ok(tags(&["v0.2.0"])),
            |_, stage| {
                fetched.fetch_add(1, Ordering::Relaxed);
                release(stage, "0.2.0");
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(done.as_deref(), Some("v0.2.0"));
        assert_eq!(fetched.load(Ordering::Relaxed), 1);
        assert_eq!(state(home.path()).unwrap().ready.as_deref(), Some("v0.2.0"));
    }

    #[test]
    fn a_stage_a_live_process_is_filling_is_left_alone() {
        let home = installed_home();
        let live = std::os::unix::process::parent_id();
        let theirs = home.path().join(format!("versions/.staging-0.2.0-{live}"));
        fs::create_dir_all(&theirs).unwrap();
        fs::write(theirs.join("foundryd"), "half copied").unwrap();

        let done = check_with(
            home.path(),
            "0.1.0",
            LIMIT,
            || Ok(tags(&["v0.2.0"])),
            |_, stage| {
                release(stage, "0.2.0");
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(done.as_deref(), Some("v0.2.0"));
        assert_eq!(
            fs::read_to_string(theirs.join("foundryd")).unwrap(),
            "half copied"
        );
        assert_eq!(stages(home.path()), [format!(".staging-0.2.0-{live}")]);
    }

    /// The pid of a process that has exited.
    fn dead_pid() -> u32 {
        let mut child = Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    #[test]
    fn stages_left_by_dead_processes_are_removed_by_the_next_stage() {
        let home = installed_home();
        let dead = dead_pid();
        for name in [
            format!(".staging-0.2.0-{dead}"),
            format!(".staging-0.1.5-{dead}"),
        ] {
            let left = home.path().join("versions").join(name);
            fs::create_dir_all(&left).unwrap();
            fs::write(left.join("foundryd"), "left behind").unwrap();
        }

        let done = check_with(
            home.path(),
            "0.1.0",
            LIMIT,
            || Ok(tags(&["v0.2.0"])),
            |_, stage| {
                release(stage, "0.2.0");
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(done.as_deref(), Some("v0.2.0"));
        assert_eq!(stages(home.path()), Vec::<String>::new());
        assert_eq!(versions(home.path()), ["0.1.0", "0.2.0"]);
    }

    #[test]
    fn a_stage_an_older_release_named_without_a_pid_is_left_alone() {
        // An older release's process may still be filling it, and its name says nothing of
        // whether that process is alive.
        let home = installed_home();
        let theirs = home.path().join("versions/.staging-0.2.0");
        fs::create_dir_all(&theirs).unwrap();
        fs::write(theirs.join("foundryd"), "half copied").unwrap();

        let done = check_with(
            home.path(),
            "0.1.0",
            LIMIT,
            || Ok(tags(&["v0.2.0"])),
            |_, stage| {
                release(stage, "0.2.0");
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(done.as_deref(), Some("v0.2.0"));
        assert_eq!(
            fs::read_to_string(theirs.join("foundryd")).unwrap(),
            "half copied"
        );
        assert_eq!(stages(home.path()), [".staging-0.2.0"]);
    }

    #[test]
    fn a_trial_stranded_during_the_download_is_cleared_and_the_tag_made_ready() {
        let home = installed_home();
        let path = home.path().to_path_buf();

        let done = check_with(
            home.path(),
            "0.1.0",
            LIMIT,
            || Ok(tags(&["v0.2.0"])),
            move |_, stage| {
                release(stage, "0.2.0");
                let stranded = State {
                    trial: Some(Trial {
                        version: "0.3.0".into(),
                        previous: "0.1.0".into(),
                        starts: 0,
                    }),
                    refused: None,
                    ready: None,
                };
                save(&path, &stranded).unwrap();
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(done.as_deref(), Some("v0.2.0"));
        assert_eq!(
            state(home.path()).unwrap(),
            State {
                trial: None,
                refused: None,
                ready: Some("v0.2.0".into()),
            }
        );
    }

    #[test]
    fn a_check_whose_tag_is_already_ready_fetches_nothing_and_changes_nothing() {
        let home = installed_home();
        with_ready(home.path(), "0.2.0", None);
        let before = fs::read(home.path().join("update.json")).unwrap();

        let done = check_with(
            home.path(),
            "0.1.0",
            LIMIT,
            || Ok(tags(&["v0.1.0", "v0.2.0"])),
            |_, _| panic!("a ready tag was fetched"),
        )
        .unwrap();

        assert_eq!(done, None);
        assert_eq!(fs::read(home.path().join("update.json")).unwrap(), before);
        assert_eq!(versions(home.path()), ["0.1.0", "0.2.0"]);
        assert_eq!(current(home.path()), PathBuf::from("versions/0.1.0"));
    }

    #[test]
    fn a_ready_tag_whose_foundryd_was_deleted_is_fetched_again() {
        let home = installed_home();
        with_ready(home.path(), "0.2.0", None);
        fs::remove_file(home.path().join("versions/0.2.0/foundryd")).unwrap();
        let fetched = AtomicUsize::new(0);

        let done = check_with(
            home.path(),
            "0.1.0",
            LIMIT,
            || Ok(tags(&["v0.2.0"])),
            |_, stage| {
                fetched.fetch_add(1, Ordering::Relaxed);
                release(stage, "0.2.0");
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(done.as_deref(), Some("v0.2.0"));
        assert_eq!(fetched.load(Ordering::Relaxed), 1);
        assert!(is_ready(home.path(), "v0.2.0"));
        assert_eq!(state(home.path()).unwrap().ready.as_deref(), Some("v0.2.0"));
    }

    #[test]
    fn a_refused_highest_tag_makes_nothing_ready_and_leaves_the_ready_tag() {
        let home = installed_home();
        with_ready(home.path(), "0.2.0", Some("v0.3.0"));
        let before = fs::read(home.path().join("update.json")).unwrap();

        let done = check_with(
            home.path(),
            "0.1.0",
            LIMIT,
            || Ok(tags(&["v0.2.0", "v0.3.0"])),
            |_, _| panic!("a refused tag was fetched"),
        )
        .unwrap();

        assert_eq!(done, None);
        assert_eq!(fs::read(home.path().join("update.json")).unwrap(), before);
        assert_eq!(state(home.path()).unwrap().ready.as_deref(), Some("v0.2.0"));
    }

    #[test]
    fn a_higher_tag_replaces_the_ready_one_and_the_old_version_stays_on_disk() {
        let home = installed_home();
        with_ready(home.path(), "0.2.0", None);

        let done = check_with(
            home.path(),
            "0.1.0",
            LIMIT,
            || Ok(tags(&["v0.2.0", "v0.3.0"])),
            |tag, stage| {
                assert_eq!(tag, "v0.3.0");
                release(stage, "0.3.0");
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(done.as_deref(), Some("v0.3.0"));
        assert_eq!(state(home.path()).unwrap().ready.as_deref(), Some("v0.3.0"));
        assert_eq!(versions(home.path()), ["0.1.0", "0.2.0", "0.3.0"]);
        assert!(is_ready(home.path(), "v0.2.0"));
    }

    /// The names under `versions/` that are stage directories.
    fn stages(home: &Path) -> Vec<String> {
        versions(home)
            .into_iter()
            .filter(|name| name.starts_with(".staging"))
            .collect()
    }

    #[test]
    fn current_moved_during_the_download_makes_nothing_ready_and_leaves_no_stage() {
        let home = installed_home();
        let path = home.path().to_path_buf();

        let result = check_with(
            home.path(),
            "0.1.0",
            LIMIT,
            || Ok(tags(&["v0.2.0"])),
            |_, stage| {
                release(stage, "0.2.0");
                fs::create_dir_all(path.join("versions/0.0.9")).unwrap();
                point_current(&path, "0.0.9").unwrap();
                Ok(())
            },
        );

        assert!(result.is_err(), "{result:?}");
        assert_eq!(current(home.path()), PathBuf::from("versions/0.0.9"));
        assert_eq!(state(home.path()).unwrap(), State::default());
        assert_eq!(versions(home.path()), ["0.0.9", "0.1.0"]);
        assert_eq!(stages(home.path()), Vec::<String>::new());
    }

    #[test]
    fn a_trial_on_during_the_download_makes_nothing_ready_and_leaves_no_stage() {
        let home = installed_home();
        let path = home.path().to_path_buf();
        let trial = State {
            trial: Some(Trial {
                version: "0.1.0".into(),
                previous: "0.0.9".into(),
                starts: 1,
            }),
            refused: None,
            ready: None,
        };
        let saved = trial.clone();

        let result = check_with(
            home.path(),
            "0.1.0",
            LIMIT,
            || Ok(tags(&["v0.2.0"])),
            move |_, stage| {
                release(stage, "0.2.0");
                save(&path, &saved).unwrap();
                Ok(())
            },
        );

        assert!(result.is_err(), "{result:?}");
        assert_eq!(state(home.path()).unwrap(), trial);
        assert_eq!(versions(home.path()), ["0.1.0"]);
    }

    #[test]
    fn a_live_trial_is_kept_by_a_check_that_clears_a_pulled_tag_and_nothing_is_made_ready() {
        // `current` names the trial's version: a daemon in its first minute, or one whose
        // healthy mark could not be saved yet.
        let home = trial_home();
        let pulled = home.path().join("versions/0.2.5");
        fs::create_dir_all(&pulled).unwrap();
        release(&pulled, "0.2.5");
        let mut before = state(home.path()).unwrap();
        before.ready = Some("v0.2.5".into());
        save(home.path(), &before).unwrap();

        let result = check_with(
            home.path(),
            "0.2.0",
            LIMIT,
            // v0.2.5 was pulled, so `tidy` takes the lock to clear it; v0.3.0 is picked.
            || Ok(tags(&["v0.2.0", "v0.3.0"])),
            |_, stage| {
                release(stage, "0.3.0");
                Ok(())
            },
        );

        let error = result.unwrap_err().to_string();
        assert!(error.contains("a trial of 0.2.0 is on"), "{error}");
        assert_eq!(
            state(home.path()).unwrap(),
            State {
                ready: None,
                ..before
            }
        );
        assert_eq!(current(home.path()), PathBuf::from("versions/0.2.0"));
        assert_eq!(versions(home.path()), ["0.1.0", "0.2.0", "0.2.5"]);
    }

    #[test]
    fn a_trial_current_does_not_name_is_cleared_and_the_check_makes_the_tag_ready() {
        let home = installed_home();
        save(
            home.path(),
            &State {
                trial: Some(Trial {
                    version: "0.2.0".into(),
                    previous: "0.1.0".into(),
                    starts: 0,
                }),
                refused: Some("v0.1.5".into()),
                ready: None,
            },
        )
        .unwrap();
        assert!(!current_names_trial(
            home.path(),
            &state(home.path()).unwrap()
        ));

        let done = check_with(
            home.path(),
            "0.1.0",
            LIMIT,
            || Ok(tags(&["v0.2.0"])),
            |_, stage| {
                release(stage, "0.2.0");
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(done.as_deref(), Some("v0.2.0"));
        assert_eq!(
            state(home.path()).unwrap(),
            State {
                trial: None,
                refused: Some("v0.1.5".into()),
                ready: Some("v0.2.0".into()),
            }
        );
        assert_eq!(current(home.path()), PathBuf::from("versions/0.1.0"));
    }

    #[test]
    fn a_trial_is_on_for_the_check_only_while_current_names_its_version() {
        let home = trial_home();
        let held = state(home.path()).unwrap();
        assert!(current_names_trial(home.path(), &held));

        point_current(home.path(), "0.1.0").unwrap();
        assert!(!current_names_trial(home.path(), &held));
        assert!(trial_on(home.path()), "trial_on keeps its rule");
        assert!(!current_names_trial(home.path(), &State::default()));
    }

    #[test]
    fn a_list_without_the_ready_tag_clears_it_before_any_fetch() {
        for left in [tags(&["v0.1.5"]), tags(&[])] {
            let home = installed_home();
            with_ready(home.path(), "0.2.0", Some("v0.0.5"));
            let path = home.path().to_path_buf();

            let done = check_with(
                home.path(),
                "0.1.0",
                LIMIT,
                || Ok(left.clone()),
                |tag, stage| {
                    assert_eq!(tag, "v0.1.5");
                    assert_eq!(
                        state(&path).unwrap().ready,
                        None,
                        "cleared before the fetch"
                    );
                    release(stage, "0.1.5");
                    Ok(())
                },
            )
            .unwrap();

            let state = state(home.path()).unwrap();
            if left.is_empty() {
                assert_eq!(done, None);
                assert_eq!(state.ready, None);
            } else {
                assert_eq!(done.as_deref(), Some("v0.1.5"));
                assert_eq!(state.ready.as_deref(), Some("v0.1.5"));
            }
            assert_eq!(state.refused.as_deref(), Some("v0.0.5"), "{left:?}");
            assert_eq!(state.trial, None);
            assert!(is_ready(home.path(), "v0.2.0"), "{left:?}");
            assert_eq!(current(home.path()), PathBuf::from("versions/0.1.0"));
        }
    }

    #[test]
    fn a_pulled_ready_tag_is_cleared_even_when_the_next_download_fails() {
        let home = installed_home();
        with_ready(home.path(), "0.2.0", None);

        let result = check_with(
            home.path(),
            "0.1.0",
            LIMIT,
            || Ok(tags(&["v0.1.5"])),
            |_, _| Err(UpdateError::Fetch("offline".into())),
        );

        assert!(matches!(result, Err(UpdateError::Fetch(_))), "{result:?}");
        assert_eq!(state(home.path()).unwrap(), State::default());
        assert!(is_ready(home.path(), "v0.2.0"));
    }

    #[test]
    fn a_stranded_trial_is_cleared_even_when_the_check_picks_nothing() {
        for listed in [tags(&["v0.2.0"]), tags(&["v0.1.0"])] {
            let home = installed_home();
            with_ready(home.path(), "0.2.0", Some("v0.0.5"));
            let mut held = state(home.path()).unwrap();
            held.trial = Some(Trial {
                version: "0.3.0".into(),
                previous: "0.1.0".into(),
                starts: 0,
            });
            save(home.path(), &held).unwrap();

            let done = check_with(
                home.path(),
                "0.1.0",
                LIMIT,
                || Ok(listed.clone()),
                |_, _| panic!("fetched"),
            )
            .unwrap();

            assert_eq!(done, None);
            let expected = if listed.contains(&"v0.2.0".to_string()) {
                Some("v0.2.0".to_string())
            } else {
                None
            };
            assert_eq!(
                state(home.path()).unwrap(),
                State {
                    trial: None,
                    refused: Some("v0.0.5".into()),
                    ready: expected,
                },
                "{listed:?}"
            );
        }
    }

    #[test]
    fn a_failed_tag_list_leaves_the_ready_tag() {
        let home = installed_home();
        with_ready(home.path(), "0.2.0", None);
        let before = fs::read(home.path().join("update.json")).unwrap();

        let result = check_with(
            home.path(),
            "0.1.0",
            LIMIT,
            || Err(UpdateError::Fetch("offline".into())),
            |_, _| panic!("fetched"),
        );

        assert!(result.is_err());
        assert_eq!(fs::read(home.path().join("update.json")).unwrap(), before);
    }

    #[test]
    fn check_with_refuses_a_tag_whose_asset_is_bad_so_it_is_not_fetched_again() {
        for bad in ["wrong version", "no holder"] {
            let home = installed_home();

            let result = check_with(
                home.path(),
                "0.1.0",
                LIMIT,
                || Ok(tags(&["v0.2.0"])),
                |_, stage| {
                    if bad == "wrong version" {
                        release(stage, "0.9.9");
                    } else {
                        script(&stage.join("foundryd"), "foundryd 0.2.0");
                    }
                    Ok(())
                },
            );

            assert!(result.is_err(), "{bad}");
            assert_eq!(
                state(home.path()).unwrap().refused.as_deref(),
                Some("v0.2.0"),
                "{bad}"
            );
            let again = check_with(
                home.path(),
                "0.1.0",
                LIMIT,
                || Ok(tags(&["v0.2.0"])),
                |_, _| panic!("fetched again"),
            );
            assert_eq!(again.unwrap(), None, "{bad}");
        }
    }

    #[test]
    fn check_with_retries_after_a_failed_list_or_fetch() {
        let home = installed_home();

        let listed = check_with(
            home.path(),
            "0.1.0",
            LIMIT,
            || Err(UpdateError::Fetch("offline".into())),
            |_, _| panic!("fetched"),
        );
        let fetched = check_with(
            home.path(),
            "0.1.0",
            LIMIT,
            || Ok(tags(&["v0.2.0"])),
            |_, _| Err(UpdateError::Fetch("offline".into())),
        );

        assert!(listed.is_err() && fetched.is_err());
        assert_eq!(state(home.path()).unwrap().refused, None);
    }

    #[test]
    fn a_version_check_that_times_out_is_retried_not_refused() {
        let home = installed_home();

        let error = check_with(
            home.path(),
            "0.1.0",
            Duration::from_millis(300),
            || Ok(tags(&["v0.2.0"])),
            |_, stage| {
                fs::write(stage.join("foundryd"), "#!/bin/sh\nexec sleep 32.3\n").unwrap();
                fs::write(stage.join("foundry-holder"), "holder").unwrap();
                Ok(())
            },
        )
        .unwrap_err();

        assert!(matches!(error, UpdateError::Fetch(_)), "{error}");
        assert!(error.to_string().contains("timed out"), "{error}");
        assert_eq!(state(home.path()).unwrap().refused, None);
        assert_eq!(current(home.path()), PathBuf::from("versions/0.1.0"));
    }

    #[test]
    fn a_binary_the_system_cannot_execute_is_refused() {
        let home = installed_home();

        let error = check_with(
            home.path(),
            "0.1.0",
            LIMIT,
            || Ok(tags(&["v0.2.0"])),
            |_, stage| {
                // Its interpreter does not exist, so the system says ENOENT.
                fs::write(stage.join("foundryd"), "#!/no/such/interpreter\n").unwrap();
                fs::write(stage.join("foundry-holder"), "holder").unwrap();
                Ok(())
            },
        );

        assert!(
            matches!(error, Err(UpdateError::BadCheck { .. })),
            "{error:?}"
        );
        assert_eq!(
            state(home.path()).unwrap().refused.as_deref(),
            Some("v0.2.0")
        );
    }

    #[test]
    fn only_errors_that_say_the_file_cannot_be_executed_are_unrunnable() {
        for code in [
            libc_code(Errno::ENOEXEC),
            libc_code(Errno::ENOENT),
            libc_code(Errno::EACCES),
        ] {
            assert!(unrunnable(&io::Error::from_raw_os_error(code)), "{code}");
        }
        for code in [
            libc_code(Errno::EAGAIN),
            libc_code(Errno::ENOMEM),
            libc_code(Errno::ETXTBSY),
        ] {
            assert!(!unrunnable(&io::Error::from_raw_os_error(code)), "{code}");
        }
        assert!(!unrunnable(&io::Error::other("no code")));
        #[cfg(target_os = "macos")]
        assert!(unrunnable(&io::Error::from_raw_os_error(86)));
    }

    fn libc_code(errno: Errno) -> i32 {
        errno as i32
    }

    #[test]
    fn a_binary_that_exits_non_zero_is_refused() {
        let home = installed_home();

        let error = check_with(
            home.path(),
            "0.1.0",
            LIMIT,
            || Ok(tags(&["v0.2.0"])),
            |_, stage| {
                fs::write(stage.join("foundryd"), "#!/bin/sh\nexit 3\n").unwrap();
                fs::write(stage.join("foundry-holder"), "holder").unwrap();
                Ok(())
            },
        );

        assert!(matches!(error, Err(UpdateError::BadCheck { .. })));
        assert_eq!(
            state(home.path()).unwrap().refused.as_deref(),
            Some("v0.2.0")
        );
    }

    #[test]
    fn re_running_install_on_the_trial_keeps_the_trial() {
        let home = trial_home();

        placed_by_hand(home.path(), "0.2.0", Some("0.2.0")).unwrap();

        assert_eq!(state(home.path()).unwrap().trial.unwrap().version, "0.2.0");
    }

    #[test]
    fn a_hand_install_of_the_version_of_a_stranded_trial_starts_no_trial() {
        let home = installed_home();
        save(
            home.path(),
            &State {
                trial: Some(Trial {
                    version: "0.3.0".into(),
                    previous: "0.1.0".into(),
                    starts: 0,
                }),
                refused: None,
                ready: None,
            },
        )
        .unwrap();
        let path = home.path().to_path_buf();

        place_version(
            home.path(),
            "0.3.0",
            |stage| {
                release(stage, "0.3.0");
                Ok(())
            },
            || placed_by_hand(&path, "0.3.0", Some("0.1.0")).map(drop),
        )
        .unwrap();

        assert_eq!(current(home.path()), PathBuf::from("versions/0.3.0"));
        assert_eq!(state(home.path()).unwrap().trial, None);
    }

    #[test]
    fn a_hand_install_of_a_lower_version_below_a_lower_refusal_raises_it() {
        let home = tempfile::tempdir().unwrap();
        save(
            home.path(),
            &State {
                trial: None,
                refused: Some("v0.1.5".into()),
                ready: None,
            },
        )
        .unwrap();

        placed_by_hand(home.path(), "0.2.0", Some("0.3.0")).unwrap();

        assert_eq!(
            state(home.path()).unwrap().refused.as_deref(),
            Some("v0.3.0")
        );
    }

    #[test]
    fn lowered_says_whether_a_version_is_below_the_one_before() {
        assert!(lowered(Some("0.2.0"), "0.1.0"));
        assert!(!lowered(Some("0.1.0"), "0.2.0"));
        assert!(!lowered(Some("0.1.0"), "0.1.0"));
        assert!(!lowered(None, "0.1.0"));
        assert!(!lowered(Some("junk"), "0.1.0"));
    }

    #[test]
    fn became_healthy_from_current_sets_the_flag_and_says_updates_are_on() {
        let home = trial_home();
        let exe = home.path().join("versions/0.2.0/foundryd");
        script(&exe, "foundryd 0.2.0");
        let flag = AtomicBool::new(false);

        let on = became_healthy(&flag, home.path(), &exe, "0.2.0");

        assert!(on);
        assert!(flag.load(Ordering::Relaxed));
    }

    #[test]
    fn became_healthy_from_elsewhere_leaves_the_trial_and_turns_updates_off() {
        let home = trial_home();
        let exe = home.path().join("elsewhere");
        script(&exe, "foundryd 0.2.0");
        let flag = AtomicBool::new(false);

        let on = became_healthy(&flag, home.path(), &exe, "0.2.0");

        assert!(!on);
        assert!(flag.load(Ordering::Relaxed));
        assert!(state(home.path()).unwrap().trial.is_some());
    }

    #[test]
    fn on_trial_is_true_only_for_a_trial_of_this_version_run_from_current() {
        let home = trial_home();
        let inside = home.path().join("versions/0.2.0/foundryd");
        script(&inside, "foundryd 0.2.0");
        let elsewhere = home.path().join("elsewhere");
        script(&elsewhere, "foundryd 0.2.0");

        assert!(on_trial(home.path(), &inside, "0.2.0"));
        assert!(!on_trial(home.path(), &elsewhere, "0.2.0"));
        assert!(!on_trial(home.path(), &inside, "0.1.0"));
        // `current` is on 0.1.0 and the trial is of 0.2.0: not this process's trial.
        let other = trial_home();
        point_current(other.path(), "0.1.0").unwrap();
        let old = other.path().join("versions/0.1.0/foundryd");
        script(&old, "foundryd 0.1.0");
        assert!(!on_trial(other.path(), &old, "0.1.0"));
        let bare = installed_home();
        let exe = bare.path().join("versions/0.1.0/foundryd");
        script(&exe, "foundryd 0.1.0");
        assert!(!on_trial(bare.path(), &exe, "0.1.0"));
    }

    #[test]
    fn a_failed_record_leaves_current_and_no_staging_directory() {
        let home = installed_home();

        let result = place_version(
            home.path(),
            "0.2.0",
            |stage| {
                release(stage, "0.2.0");
                Ok(())
            },
            || Err(io::Error::other("disk full")),
        );

        assert!(result.is_err());
        assert_eq!(current(home.path()), PathBuf::from("versions/0.1.0"));
        let left: Vec<_> = fs::read_dir(home.path().join("versions"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".staging"))
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }

    fn exe_in(home: &Path, version: &str) -> PathBuf {
        let exe = home.join("versions").join(version).join("foundryd");
        script(&exe, &format!("foundryd {version}"));
        exe
    }

    #[test]
    fn begin_skips_a_process_that_does_not_run_from_current_and_writes_nothing() {
        let home = trial_home();
        for _ in 0..3 {
            start_with(home.path(), "0.2.0").unwrap();
        }
        let elsewhere = home.path().join("elsewhere");
        script(&elsewhere, "foundryd 0.2.0");
        let before = fs::read(home.path().join("update.json")).unwrap();

        let begun = begin(home.path(), &elsewhere, "0.2.0").unwrap();

        assert_eq!(begun, Begin::Skip);
        assert_eq!(fs::read(home.path().join("update.json")).unwrap(), before);
        assert_eq!(current(home.path()), PathBuf::from("versions/0.2.0"));
    }

    #[test]
    fn begin_counts_a_trial_run_from_current_and_asks_for_the_watchdog() {
        let home = trial_home();
        let exe = exe_in(home.path(), "0.2.0");

        let begun = begin(home.path(), &exe, "0.2.0").unwrap();

        assert_eq!(begun, Begin::Run { watch: true });
        assert_eq!(state(home.path()).unwrap().trial.unwrap().starts, 1);
    }

    #[test]
    fn begin_runs_without_the_watchdog_when_there_is_no_trial() {
        let home = installed_home();
        let exe = exe_in(home.path(), "0.1.0");

        let begun = begin(home.path(), &exe, "0.1.0").unwrap();

        assert_eq!(begun, Begin::Run { watch: false });
        assert!(!home.path().join("update.json").exists());
    }

    #[test]
    fn begin_rolls_back_a_trial_at_its_cap() {
        let home = trial_home();
        let exe = exe_in(home.path(), "0.2.0");
        for _ in 0..3 {
            begin(home.path(), &exe, "0.2.0").unwrap();
        }

        let begun = begin(home.path(), &exe, "0.2.0").unwrap();

        assert_eq!(begun, Begin::RolledBack { to: "0.1.0".into() });
        assert_eq!(current(home.path()), PathBuf::from("versions/0.1.0"));
    }

    use std::sync::atomic::AtomicUsize;

    fn nothing() -> Result<Option<String>, UpdateError> {
        Ok(None)
    }

    fn found() -> Result<Option<String>, UpdateError> {
        Ok(Some("v0.3.0".into()))
    }

    /// Runs `run_with` on a paused clock with a check that gives `result`, counted in `checks`.
    fn spawn_run(
        home: &Path,
        exe: io::Result<PathBuf>,
        flag: &'static AtomicBool,
        checks: &'static AtomicUsize,
        result: fn() -> Result<Option<String>, UpdateError>,
    ) -> tokio::task::JoinHandle<()> {
        let home = home.to_path_buf();
        tokio::spawn(run_with(home, exe, "0.2.0", flag, move || {
            checks.fetch_add(1, Ordering::Relaxed);
            result()
        }))
    }

    /// Lets the blocking check threads run until `counter` reaches `n`.
    async fn reach(counter: &AtomicUsize, n: usize) {
        for _ in 0..500 {
            if counter.load(Ordering::Relaxed) >= n {
                return;
            }
            tokio::task::yield_now().await;
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn run_marks_a_trial_healthy_after_the_wait_and_then_checks_every_interval() {
        static FLAG: AtomicBool = AtomicBool::new(false);
        static CHECKS: AtomicUsize = AtomicUsize::new(0);
        let home = trial_home();
        let exe = exe_in(home.path(), "0.2.0");
        let task = spawn_run(home.path(), Ok(exe), &FLAG, &CHECKS, nothing);

        tokio::time::sleep(HEALTHY_AFTER / 2).await;
        assert!(!FLAG.load(Ordering::Relaxed));
        assert!(state(home.path()).unwrap().trial.is_some());
        tokio::time::sleep(HEALTHY_AFTER / 2 + Duration::from_secs(1)).await;
        reach(&CHECKS, 1).await;

        assert!(FLAG.load(Ordering::Relaxed));
        assert_eq!(state(home.path()).unwrap().trial, None);
        assert_eq!(CHECKS.load(Ordering::Relaxed), 1);
        tokio::time::sleep(CHECK_EVERY + Duration::from_secs(1)).await;
        reach(&CHECKS, 2).await;
        assert_eq!(CHECKS.load(Ordering::Relaxed), 2);
        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn run_never_stops_the_daemon_when_a_check_makes_a_tag_ready_and_keeps_checking() {
        // `run_with` has nothing it could call to stop the daemon, and a SIGTERM raised here
        // would end this test binary.
        static FLAG: AtomicBool = AtomicBool::new(false);
        static CHECKS: AtomicUsize = AtomicUsize::new(0);
        let home = trial_home();
        let exe = exe_in(home.path(), "0.2.0");
        let task = spawn_run(home.path(), Ok(exe), &FLAG, &CHECKS, found);

        tokio::time::sleep(HEALTHY_AFTER + Duration::from_secs(1)).await;
        reach(&CHECKS, 1).await;
        for n in 2..=3 {
            tokio::time::sleep(CHECK_EVERY).await;
            reach(&CHECKS, n).await;
        }

        assert_eq!(CHECKS.load(Ordering::Relaxed), 3);
        assert!(!task.is_finished());
        assert_eq!(current(home.path()), PathBuf::from("versions/0.2.0"));
        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn run_from_elsewhere_sets_the_flag_but_never_checks() {
        static FLAG: AtomicBool = AtomicBool::new(false);
        static CHECKS: AtomicUsize = AtomicUsize::new(0);
        let home = trial_home();
        let elsewhere = home.path().join("elsewhere");
        script(&elsewhere, "foundryd 0.2.0");
        let task = spawn_run(home.path(), Ok(elsewhere), &FLAG, &CHECKS, nothing);

        tokio::time::sleep(HEALTHY_AFTER + CHECK_EVERY * 3).await;
        std::thread::sleep(Duration::from_millis(100));

        assert!(FLAG.load(Ordering::Relaxed));
        assert_eq!(CHECKS.load(Ordering::Relaxed), 0);
        assert!(state(home.path()).unwrap().trial.is_some());
        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn run_with_no_executable_path_still_sets_the_flag() {
        static FLAG: AtomicBool = AtomicBool::new(false);
        static CHECKS: AtomicUsize = AtomicUsize::new(0);
        let home = trial_home();
        let task = spawn_run(
            home.path(),
            Err(io::Error::other("no exe")),
            &FLAG,
            &CHECKS,
            nothing,
        );

        tokio::time::sleep(HEALTHY_AFTER + Duration::from_secs(1)).await;

        assert!(FLAG.load(Ordering::Relaxed));
        assert_eq!(CHECKS.load(Ordering::Relaxed), 0);
        task.abort();
    }

    #[test]
    fn the_timings_are_the_ones_the_spec_gives() {
        assert_eq!(HEALTHY_AFTER, Duration::from_secs(60));
        assert_eq!(TRIAL_DEADLINE, Duration::from_secs(300));
        assert_eq!(CHECK_EVERY, Duration::from_secs(600));
    }

    fn started(home: &Path, exe: &Path) -> (bool, usize) {
        let armed = std::cell::Cell::new(0);
        let go = starting_with(home, exe, "0.2.0", || armed.set(armed.get() + 1));
        (go, armed.get())
    }

    #[test]
    fn a_counted_trial_from_current_starts_and_arms_the_watchdog_once() {
        let home = trial_home();
        let exe = exe_in(home.path(), "0.2.0");

        assert_eq!(started(home.path(), &exe), (true, 1));
    }

    #[test]
    fn no_trial_starts_without_the_watchdog() {
        let home = installed_home();
        let exe = exe_in(home.path(), "0.1.0");
        let armed = std::cell::Cell::new(0);

        let go = starting_with(home.path(), &exe, "0.1.0", || armed.set(1));

        assert!(go);
        assert_eq!(armed.get(), 0);
    }

    #[test]
    fn an_unreadable_state_file_starts_the_daemon_without_the_watchdog() {
        let home = trial_home();
        let exe = exe_in(home.path(), "0.2.0");
        fs::write(home.path().join("update.json"), "not json").unwrap();

        assert_eq!(started(home.path(), &exe), (true, 0));
    }

    /// A rollback that stops part way exits rather than run the failed version unwatched, and
    /// the trial stays at its cap, so the next start tries the rollback again.
    #[test]
    fn a_rollback_that_stops_part_way_exits_and_rolls_back_on_the_next_start() {
        let home = trial_home();
        let exe = exe_in(home.path(), "0.2.0");
        for _ in 0..3 {
            assert_eq!(started(home.path(), &exe), (true, 1));
        }
        // A directory with a file in it stops `current` from moving.
        let obstacle = home.path().join("current.new");
        fs::create_dir_all(obstacle.join("inside")).unwrap();

        assert_eq!(started(home.path(), &exe), (false, 0));
        assert_eq!(current(home.path()), PathBuf::from("versions/0.2.0"));

        fs::remove_dir_all(&obstacle).unwrap();
        assert_eq!(started(home.path(), &exe), (false, 0));
        assert_eq!(current(home.path()), PathBuf::from("versions/0.1.0"));
        assert_eq!(state(home.path()).unwrap().trial, None);
    }

    #[test]
    fn a_trial_at_its_cap_does_not_start_and_does_not_arm() {
        let home = trial_home();
        let exe = exe_in(home.path(), "0.2.0");
        for _ in 0..3 {
            assert!(started(home.path(), &exe).0);
        }

        assert_eq!(started(home.path(), &exe), (false, 0));
        assert_eq!(current(home.path()), PathBuf::from("versions/0.1.0"));
    }

    fn judged(result: Result<String, RunError>) -> Result<(), UpdateError> {
        judge_version_check(result, "v0.2.0", "0.2.0")
    }

    fn spawn_error(code: Errno) -> RunError {
        RunError::Spawn {
            name: "foundryd --version".into(),
            source: io::Error::from_raw_os_error(code as i32),
        }
    }

    #[test]
    fn a_version_check_is_judged_by_what_happened_to_it() {
        assert!(matches!(
            judged(Err(spawn_error(Errno::EAGAIN))),
            Err(UpdateError::Fetch(_))
        ));
        assert!(matches!(
            judged(Err(RunError::TimedOut {
                name: "foundryd --version".into(),
                secs: 10
            })),
            Err(UpdateError::Fetch(_))
        ));
        assert!(matches!(
            judged(Err(spawn_error(Errno::ENOEXEC))),
            Err(UpdateError::BadCheck { .. })
        ));
        assert!(matches!(
            judged(Err(RunError::Failed {
                name: "foundryd --version".into(),
                stderr: "boom".into()
            })),
            Err(UpdateError::BadCheck { .. })
        ));
        assert!(matches!(
            judged(Ok("foundryd 0.9.9\n".into())),
            Err(UpdateError::WrongVersion { .. })
        ));
        assert!(judged(Ok("foundryd 0.2.0\n".into())).is_ok());
    }

    #[test]
    fn a_refusal_says_what_the_binary_did_without_claiming_it_printed_an_error() {
        let message = judged(Err(spawn_error(Errno::ENOEXEC)))
            .unwrap_err()
            .to_string();

        assert!(message.contains("v0.2.0 is refused"), "{message}");
        assert!(!message.contains("printed"), "{message}");
    }

    fn refused_state(refused: &str) -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        save(
            home.path(),
            &State {
                trial: None,
                refused: Some(refused.into()),
                ready: None,
            },
        )
        .unwrap();
        home
    }

    #[test]
    fn placed_by_hand_says_nothing_when_the_version_did_not_go_down() {
        let home = refused_state("v0.3.0");

        let same = placed_by_hand(home.path(), "0.2.0", Some("0.2.0")).unwrap();
        let up = placed_by_hand(home.path(), "0.3.1", Some("0.2.0")).unwrap();

        assert_eq!(same, None);
        assert_eq!(up, None);
    }

    /// Runs `write` on another thread while this thread holds the update lock, and says
    /// whether it finished before the lock was let go.
    fn finished_while_locked(home: &Path, write: impl FnOnce() + Send + 'static) -> bool {
        let held = lock(home).unwrap();
        let done = std::sync::Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        let writer = std::thread::spawn(move || {
            write();
            flag.store(true, Ordering::SeqCst);
        });
        std::thread::sleep(Duration::from_millis(300));
        let early = done.load(Ordering::SeqCst);
        drop(held);
        writer.join().unwrap();
        early
    }

    #[test]
    fn a_hand_install_swaps_the_new_version_in_only_while_it_holds_the_lock() {
        let home = installed_home();
        let path = home.path().to_path_buf();
        let held = lock(home.path()).unwrap();
        let (staged_tx, staged) = std::sync::mpsc::channel();
        let hand = std::thread::spawn(move || {
            place_version(
                &path,
                "0.3.0",
                |stage| {
                    release(stage, "0.3.0");
                    staged_tx.send(stage.to_path_buf()).unwrap();
                    Ok(())
                },
                || Ok(()),
            )
            .unwrap();
        });
        // The stage is filled while this thread holds the lock: the copy is made outside it.
        let stage = staged.recv_timeout(Duration::from_secs(10)).unwrap();
        std::thread::sleep(Duration::from_millis(300));

        assert!(stage.join("foundryd").is_file());
        assert!(!home.path().join("versions/0.3.0").exists());
        assert_eq!(current(home.path()), PathBuf::from("versions/0.1.0"));

        drop(held);
        hand.join().unwrap();

        assert!(home.path().join("versions/0.3.0/foundryd").is_file());
        assert_eq!(current(home.path()), PathBuf::from("versions/0.3.0"));
    }

    #[test]
    fn every_change_to_the_update_state_waits_for_the_lock() {
        let home = trial_home();
        let path = home.path().to_path_buf();
        assert!(!finished_while_locked(home.path(), move || {
            start_with(&path, "0.2.0").unwrap();
        }));
        assert_eq!(state(home.path()).unwrap().trial.unwrap().starts, 1);

        let path = home.path().to_path_buf();
        assert!(!finished_while_locked(home.path(), move || {
            mark_healthy(&path, "0.2.0");
        }));
        assert_eq!(state(home.path()).unwrap().trial, None);

        let path = home.path().to_path_buf();
        assert!(!finished_while_locked(home.path(), move || {
            commit_version(&path, "0.1.0", || {
                placed_by_hand(&path, "0.1.0", Some("0.2.0")).map(drop)
            })
            .unwrap();
        }));
        assert_eq!(current(home.path()), PathBuf::from("versions/0.1.0"));

        let home = installed_home();
        let path = home.path().to_path_buf();
        assert!(!finished_while_locked(home.path(), move || {
            let _ = check_with(
                &path,
                "0.1.0",
                LIMIT,
                || Ok(tags(&["v0.2.0"])),
                |_, stage| {
                    script(&stage.join("foundryd"), "foundryd 0.2.0");
                    Ok(())
                },
            );
        }));
        assert_eq!(
            state(home.path()).unwrap().refused.as_deref(),
            Some("v0.2.0")
        );

        let home = installed_home();
        let path = home.path().to_path_buf();
        assert!(!finished_while_locked(home.path(), move || {
            check_with(
                &path,
                "0.1.0",
                LIMIT,
                || Ok(tags(&["v0.2.0"])),
                |_, stage| {
                    release(stage, "0.2.0");
                    Ok(())
                },
            )
            .unwrap();
        }));
        assert_eq!(state(home.path()).unwrap().ready.as_deref(), Some("v0.2.0"));

        let home = installed_home();
        with_ready(home.path(), "0.2.0", None);
        let path = home.path().to_path_buf();
        assert!(!finished_while_locked(home.path(), move || {
            check_with(
                &path,
                "0.1.0",
                LIMIT,
                || Ok(tags(&[])),
                |_, _| unreachable!(),
            )
            .unwrap();
        }));
        assert_eq!(state(home.path()).unwrap().ready, None);
    }

    #[test]
    fn commit_version_records_before_it_moves_current() {
        let home = installed_home();

        let error = commit_version(home.path(), "0.2.0", || Err(io::Error::other("disk full")))
            .unwrap_err();

        assert!(error.to_string().contains("disk full"), "{error}");
        assert_eq!(current(home.path()), PathBuf::from("versions/0.1.0"));
    }

    #[test]
    fn a_rollback_to_a_version_without_both_binaries_is_an_error_and_changes_nothing() {
        for missing in BINARIES {
            let home = trial_home();
            fs::remove_file(home.path().join("versions/0.1.0").join(missing)).unwrap();
            for _ in 0..3 {
                start_with(home.path(), "0.2.0").unwrap();
            }
            let before = fs::read(home.path().join("update.json")).unwrap();

            let error = start_with(home.path(), "0.2.0").unwrap_err();

            assert!(error.to_string().contains(missing), "{missing}: {error}");
            assert_eq!(current(home.path()), PathBuf::from("versions/0.2.0"));
            assert_eq!(fs::read(home.path().join("update.json")).unwrap(), before);
        }
    }

    #[test]
    fn the_asset_is_the_name_the_release_workflow_publishes() {
        let published = [
            "foundryd-aarch64-macos.tar.gz",
            "foundryd-x86_64-macos.tar.gz",
            "foundryd-aarch64-linux.tar.gz",
            "foundryd-x86_64-linux.tar.gz",
        ];

        assert!(published.contains(&asset().as_str()), "{}", asset());
    }

    #[test]
    fn the_release_key_is_the_issued_public_key() {
        let hex: String = RELEASE_KEY.iter().map(|b| format!("{b:02x}")).collect();

        assert_eq!(
            hex,
            "1884d7afeb3f7af2805f1727358a10baa8688264fd531685d116b9bec30b45e1"
        );
    }

    #[test]
    fn the_release_workflow_reads_the_key_from_its_one_line_in_this_file() {
        let line = format!("const RELEASE_KEY_HEX: &str = \"{RELEASE_KEY_HEX}\";");
        let source = include_str!("update.rs");
        let workflow = include_str!("../../../.github/workflows/release.yml");

        let found: Vec<&str> = source
            .lines()
            .filter(|l| l.starts_with("const RELEASE_KEY_HEX"))
            .collect();

        assert_eq!(found, [line.as_str()]);
        assert!(workflow.contains(
            r#"sed -n 's/^const RELEASE_KEY_HEX: &str = "\([0-9a-f]\{64\}\)";$/\1/p' crates/foundryd/src/update.rs"#
        ));
    }

    #[test]
    fn every_action_the_release_workflow_runs_is_pinned_to_a_commit() {
        // Every job builds or handles the archives that publish signs, not only publish itself.
        let workflow = include_str!("../../../.github/workflows/release.yml");

        let unpinned: Vec<&str> = workflow
            .lines()
            .filter_map(|l| {
                l.trim_start()
                    .trim_start_matches("- ")
                    .strip_prefix("uses: ")
            })
            .filter(|action| {
                let reference = action
                    .split_whitespace()
                    .next()
                    .and_then(|a| a.split_once('@'))
                    .map(|(_, r)| r);
                !reference
                    .is_some_and(|r| r.len() == 40 && r.bytes().all(|b| b.is_ascii_hexdigit()))
            })
            .collect();

        assert!(workflow.contains("uses: "));
        assert_eq!(unpinned, Vec::<&str>::new());
    }

    /// Bytes from hex, for fixtures.
    fn unhex(hex: &str) -> Vec<u8> {
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn a_signature_made_the_way_the_release_workflow_signs_verifies() {
        // Made once with a throwaway key and the command in release.yml's sign step:
        // `openssl pkeyutl -sign -rawin -inkey <key> -in <message> -out <message>.sig`.
        let key = unhex("c5f260650d6e1334514f0e40ccdc1b4e2cdb1ea1c2e4085c9c0469eaffbd185d");
        let signature = unhex(
            "6ef89c2ded9259bc97d9d6ecfbbf88751456adcb970225b383dff3e44e774d03fec4858e9ba95d14d7ede588bd68f5d697aa0e9e076b6c35645b00ba9bcb9f0c",
        );

        assert!(verifies(&key, b"foundryd release archive", &signature));
        assert!(!verifies(&key, b"foundryd release archivf", &signature));
    }

    /// A fresh Ed25519 key, as the release workflow's would be.
    fn signing_key() -> Ed25519KeyPair {
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
        Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap()
    }

    /// A release on GitHub, as the fake `gh` in `work/gh` serves it from `work/served`.
    struct Served {
        work: tempfile::TempDir,
    }

    impl Served {
        /// A real archive of `version`'s two binaries under this machine's asset name, and a
        /// `gh` that copies each `--pattern` asset there is into `--dir`, saying nothing about
        /// one that is not, as `gh release download` does.
        fn release(version: &str) -> Self {
            let work = tempfile::tempdir().unwrap();
            let packed = work.path().join("packed");
            let served = work.path().join("served");
            fs::create_dir_all(&packed).unwrap();
            fs::create_dir_all(&served).unwrap();
            release(&packed, version);
            run_bounded(
                Command::new("tar")
                    .arg("-czf")
                    .arg(served.join(asset()))
                    .arg("-C")
                    .arg(&packed)
                    .args(BINARIES),
                LIMIT,
            )
            .unwrap();
            let gh = work.path().join("gh");
            fs::write(
                &gh,
                format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{args}'\nnames=''\nwhile [ $# -gt 0 ]; do\n  case \"$1\" in\n    --pattern) names=\"$names $2\"; shift ;;\n    --dir) dir=\"$2\"; shift ;;\n  esac\n  shift\ndone\nfor name in $names; do\n  if [ -f '{served}'/\"$name\" ]; then cp '{served}'/\"$name\" \"$dir/$name\"; fi\ndone\n",
                    args = work.path().join("args").display(),
                    served = served.display()
                ),
            )
            .unwrap();
            fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();
            Served { work }
        }

        /// Serves a signature of the archive made with `key`.
        fn signed_by(self, key: &Ed25519KeyPair) -> Self {
            let served = self.work.path().join("served");
            let archive = fs::read(served.join(asset())).unwrap();
            fs::write(
                served.join(format!("{}.sig", asset())),
                key.sign(&archive).as_ref(),
            )
            .unwrap();
            self
        }

        fn gh(&self) -> PathBuf {
            self.work.path().join("gh")
        }

        /// The arguments the fake `gh` was last called with.
        fn said(&self) -> Vec<String> {
            fs::read_to_string(self.work.path().join("args"))
                .unwrap()
                .lines()
                .map(str::to_string)
                .collect()
        }
    }

    /// What `versions/` holds, by name.
    fn versions(home: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(home.join("versions"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// One look for a newer tag that fetches `v0.2.0` from `served` and checks it with `key`.
    fn check_served(
        home: &Path,
        served: &Served,
        key: &[u8],
    ) -> Result<Option<String>, UpdateError> {
        check_with(
            home,
            "0.1.0",
            LIMIT,
            || Ok(tags(&["v0.2.0"])),
            |tag, stage| download_with(served.gh().as_os_str(), key, tag, stage),
        )
    }

    #[test]
    fn download_fetches_this_machines_asset_and_its_signature_with_gh_and_unpacks_both_binaries() {
        let key = signing_key();
        let served = Served::release("0.2.0").signed_by(&key);
        let stage = tempfile::tempdir().unwrap();

        download_with(
            served.gh().as_os_str(),
            key.public_key().as_ref(),
            "v0.2.0",
            stage.path(),
        )
        .unwrap();

        let sig = format!("{}.sig", asset());
        assert_eq!(
            served.said(),
            [
                "release",
                "download",
                "v0.2.0",
                "--repo",
                REPO,
                "--pattern",
                &asset(),
                "--pattern",
                &sig,
                "--dir",
                stage.path().to_str().unwrap()
            ]
        );
        for name in BINARIES {
            assert!(stage.path().join(name).is_file(), "{name}");
        }
        assert!(!stage.path().join(asset()).exists());
        assert!(!stage.path().join(sig).exists());
    }

    #[test]
    fn download_unpacks_nothing_when_the_signature_does_not_verify() {
        let served = Served::release("0.2.0").signed_by(&signing_key());
        let stage = tempfile::tempdir().unwrap();

        let error = download_with(
            served.gh().as_os_str(),
            signing_key().public_key().as_ref(),
            "v0.2.0",
            stage.path(),
        )
        .unwrap_err();

        assert!(matches!(error, UpdateError::BadSignature(_)), "{error}");
        for name in BINARIES {
            assert!(!stage.path().join(name).exists(), "{name}");
        }
    }

    #[test]
    fn a_release_signed_with_another_key_is_refused_and_nothing_is_placed() {
        let home = installed_home();
        let served = Served::release("0.2.0").signed_by(&signing_key());
        let trusted = signing_key();

        let error = check_served(home.path(), &served, trusted.public_key().as_ref()).unwrap_err();

        let said = error.to_string();
        assert!(
            said.contains("v0.2.0") && said.contains("signature did not verify"),
            "{said}"
        );
        assert_eq!(versions(home.path()), ["0.1.0"]);
        assert_eq!(current(home.path()), PathBuf::from("versions/0.1.0"));
        assert_eq!(
            state(home.path()).unwrap().refused.as_deref(),
            Some("v0.2.0")
        );
        let again = check_served(home.path(), &served, trusted.public_key().as_ref());
        assert_eq!(again.unwrap(), None);
    }

    #[test]
    fn a_release_with_no_signature_is_tried_again_and_nothing_is_placed() {
        let home = installed_home();
        let served = Served::release("0.2.0");
        let key = signing_key();

        let error = check_served(home.path(), &served, key.public_key().as_ref()).unwrap_err();

        assert!(matches!(error, UpdateError::Fetch(_)), "{error}");
        assert_eq!(versions(home.path()), ["0.1.0"]);
        assert_eq!(current(home.path()), PathBuf::from("versions/0.1.0"));
        assert_eq!(state(home.path()).unwrap().refused, None);
    }

    #[test]
    fn a_release_with_a_valid_signature_places_both_binaries_and_is_ready() {
        let home = installed_home();
        let key = signing_key();
        let served = Served::release("0.2.0").signed_by(&key);

        let ready = check_served(home.path(), &served, key.public_key().as_ref()).unwrap();

        assert_eq!(ready.as_deref(), Some("v0.2.0"));
        assert_eq!(versions(home.path()), ["0.1.0", "0.2.0"]);
        for name in BINARIES {
            assert!(
                home.path().join("versions/0.2.0").join(name).is_file(),
                "{name}"
            );
        }
        assert_eq!(current(home.path()), PathBuf::from("versions/0.1.0"));
        assert_eq!(state(home.path()).unwrap().ready.as_deref(), Some("v0.2.0"));
    }

    #[test]
    fn a_bad_signature_refuses_the_tag_and_keeps_the_ready_one() {
        let home = installed_home();
        with_ready(home.path(), "0.2.0", None);
        let served = Served::release("0.3.0").signed_by(&signing_key());
        let trusted = signing_key();

        let error = check_with(
            home.path(),
            "0.1.0",
            LIMIT,
            || Ok(tags(&["v0.2.0", "v0.3.0"])),
            |tag, stage| {
                download_with(
                    served.gh().as_os_str(),
                    trusted.public_key().as_ref(),
                    tag,
                    stage,
                )
            },
        )
        .unwrap_err();

        assert!(matches!(error, UpdateError::BadSignature(_)), "{error}");
        let state = state(home.path()).unwrap();
        assert_eq!(state.refused.as_deref(), Some("v0.3.0"));
        assert_eq!(state.ready.as_deref(), Some("v0.2.0"));
        assert_eq!(versions(home.path()), ["0.1.0", "0.2.0"]);
    }

    #[test]
    fn download_passes_a_failed_gh_on_as_a_fetch_error() {
        let work = tempfile::tempdir().unwrap();
        let gh = work.path().join("gh");
        fs::write(&gh, "#!/bin/sh\necho 'release not found' >&2\nexit 1\n").unwrap();
        fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();

        let error = download_with(gh.as_os_str(), &RELEASE_KEY, "v0.2.0", work.path()).unwrap_err();

        assert!(matches!(error, UpdateError::Fetch(_)), "{error}");
        assert!(error.to_string().contains("release not found"), "{error}");
    }

    fn failing() -> Result<Option<String>, UpdateError> {
        Err(UpdateError::Fetch("gh is not logged in".into()))
    }

    #[tokio::test(start_paused = true)]
    async fn run_keeps_checking_after_a_failed_check_and_never_stops_the_daemon() {
        static FLAG: AtomicBool = AtomicBool::new(false);
        static CHECKS: AtomicUsize = AtomicUsize::new(0);
        let home = trial_home();
        let exe = exe_in(home.path(), "0.2.0");
        let task = spawn_run(home.path(), Ok(exe), &FLAG, &CHECKS, failing);

        tokio::time::sleep(HEALTHY_AFTER + Duration::from_secs(1)).await;
        reach(&CHECKS, 1).await;
        assert_eq!(CHECKS.load(Ordering::Relaxed), 1);
        tokio::time::sleep(CHECK_EVERY).await;
        reach(&CHECKS, 2).await;

        assert_eq!(CHECKS.load(Ordering::Relaxed), 2);
        assert!(!task.is_finished());
        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn a_healthy_mark_that_cannot_be_saved_is_tried_again_at_the_next_check() {
        static FLAG: AtomicBool = AtomicBool::new(false);
        static CHECKS: AtomicUsize = AtomicUsize::new(0);
        let home = trial_home();
        let exe = exe_in(home.path(), "0.2.0");
        // A directory where `save` writes its temporary file: every save fails.
        let blocker = home.path().join("update.json.tmp");
        fs::create_dir(&blocker).unwrap();
        let task = spawn_run(home.path(), Ok(exe), &FLAG, &CHECKS, nothing);

        tokio::time::sleep(HEALTHY_AFTER + Duration::from_secs(1)).await;
        reach(&CHECKS, 1).await;
        assert!(state(home.path()).unwrap().trial.is_some());
        fs::remove_dir(&blocker).unwrap();
        tokio::time::sleep(CHECK_EVERY).await;
        reach(&CHECKS, 2).await;

        assert_eq!(state(home.path()).unwrap().trial, None);
        task.abort();
    }

    #[test]
    fn check_with_installs_nothing_once_a_hand_install_moved_current() {
        let home = installed_home();
        fs::create_dir_all(home.path().join("versions/0.0.9")).unwrap();
        point_current(home.path(), "0.0.9").unwrap();

        let found = check_with(
            home.path(),
            "0.1.0",
            LIMIT,
            || Ok(tags(&["v0.2.0"])),
            |_, _| panic!("nothing is fetched"),
        )
        .unwrap();

        assert_eq!(found, None);
        assert_eq!(current(home.path()), PathBuf::from("versions/0.0.9"));
    }

    #[test]
    fn a_hand_install_during_the_download_stands_and_the_tag_waits() {
        let home = installed_home();
        let path = home.path().to_path_buf();

        let error = check_with(
            home.path(),
            "0.1.0",
            LIMIT,
            || Ok(tags(&["v0.2.0"])),
            |_, stage| {
                release(stage, "0.2.0");
                fs::create_dir_all(path.join("versions/0.0.9")).unwrap();
                point_current(&path, "0.0.9").unwrap();
                placed_by_hand(&path, "0.0.9", Some("0.1.0")).unwrap();
                Ok(())
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("hand install"), "{error}");
        assert_eq!(current(home.path()), PathBuf::from("versions/0.0.9"));
        let state = state(home.path()).unwrap();
        assert_eq!(state.trial, None);
        assert_eq!(state.refused.as_deref(), Some("v0.1.0"));
    }

    #[test]
    fn list_tags_reads_the_published_tags_with_gh() {
        let work = tempfile::tempdir().unwrap();
        let args = work.path().join("args");
        let gh = work.path().join("gh");
        fs::write(
            &gh,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nprintf 'v0.2.0\\nv0.1.0\\n\\n'\n",
                args.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();

        let listed = list_tags_with(gh.as_os_str()).unwrap();

        assert_eq!(listed, tags(&["v0.2.0", "v0.1.0"]));
        let said = fs::read_to_string(&args).unwrap();
        let said: Vec<&str> = said.lines().collect();
        assert_eq!(
            said,
            [
                "release",
                "list",
                "--repo",
                REPO,
                "--exclude-drafts",
                "--exclude-pre-releases",
                "--limit",
                "100",
                "--json",
                "tagName",
                "--jq",
                ".[].tagName"
            ]
        );
    }

    #[test]
    fn list_tags_passes_a_failed_gh_on_as_a_fetch_error() {
        let work = tempfile::tempdir().unwrap();
        let gh = work.path().join("gh");
        fs::write(&gh, "#!/bin/sh\necho 'not logged in' >&2\nexit 1\n").unwrap();
        fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();

        let error = list_tags_with(gh.as_os_str()).unwrap_err();

        assert!(matches!(error, UpdateError::Fetch(_)), "{error}");
        assert!(error.to_string().contains("not logged in"), "{error}");
    }

    /// Everything `install_ready` must leave alone when it refuses: `current`, the state and
    /// what `versions/` holds.
    fn snapshot(home: &Path) -> (PathBuf, State, Vec<String>) {
        (current(home), state(home).unwrap(), versions(home))
    }

    /// A home on 0.1.0 whose daemon runs from `current`, with `v0.2.0` ready and `refused`.
    fn ready_home(refused: Option<&str>) -> (tempfile::TempDir, PathBuf) {
        let home = installed_home();
        let exe = exe_in(home.path(), "0.1.0");
        with_ready(home.path(), "0.2.0", refused);
        (home, exe)
    }

    /// Asks `install_ready` for `tag` and checks it refuses with `ready`, a reason that
    /// contains `reason`, and the state `after` (the state before when `None`) and nothing else
    /// changed.
    fn refuses(
        home: &Path,
        exe: &Path,
        tag: &str,
        running: &str,
        ready: Option<&str>,
        reason: &str,
        after: Option<State>,
    ) {
        let before = snapshot(home);
        let error = install_ready(home, tag, running, exe).unwrap_err();
        match &error {
            InstallError::NotReady {
                ready: got,
                reason: why,
            } => {
                assert_eq!(got.as_deref(), ready, "{error}");
                assert!(why.contains(reason), "{why:?} lacks {reason:?}");
                assert_eq!(error.to_string(), *why);
            }
            other => panic!("{other:?}"),
        }
        let now = snapshot(home);
        assert_eq!(now.0, before.0);
        assert_eq!(now.1, after.unwrap_or(before.1));
        assert_eq!(now.2, before.2);
    }

    #[test]
    fn install_ready_refuses_a_tag_that_is_not_the_ready_tag() {
        let (home, exe) = ready_home(None);
        refuses(
            home.path(),
            &exe,
            "v0.3.0",
            "0.1.0",
            Some("v0.2.0"),
            "v0.3.0 is not the ready tag",
            None,
        );
    }

    #[test]
    fn install_ready_refuses_a_tag_with_binaries_on_disk_that_is_not_the_ready_tag() {
        let (home, exe) = ready_home(None);
        let other = home.path().join("versions/0.3.0");
        fs::create_dir_all(&other).unwrap();
        release(&other, "0.3.0");
        refuses(
            home.path(),
            &exe,
            "v0.3.0",
            "0.1.0",
            Some("v0.2.0"),
            "v0.3.0 is not the ready tag",
            None,
        );
    }

    #[test]
    fn install_ready_refuses_a_ready_tag_whose_binaries_are_gone() {
        let (home, exe) = ready_home(None);
        fs::remove_file(home.path().join("versions/0.2.0/foundry-holder")).unwrap();
        refuses(
            home.path(),
            &exe,
            "v0.2.0",
            "0.1.0",
            None,
            "v0.2.0 is not ready: its binaries are gone",
            None,
        );
    }

    #[test]
    fn install_ready_refuses_a_tag_not_above_the_running_version() {
        let home = installed_home();
        let exe = exe_in(home.path(), "0.1.0");
        with_ready(home.path(), "0.1.0", None);
        refuses(
            home.path(),
            &exe,
            "v0.1.0",
            "0.1.0",
            None,
            "not above the running version 0.1.0",
            None,
        );
    }

    #[test]
    fn a_stale_tag_after_a_hand_install_of_a_higher_version_is_refused() {
        let home = tempfile::tempdir().unwrap();
        for version in ["0.9.1", "0.9.2"] {
            fs::create_dir_all(home.path().join("versions").join(version)).unwrap();
        }
        point_current(home.path(), "0.9.2").unwrap();
        let exe = exe_in(home.path(), "0.9.2");
        with_ready(home.path(), "0.9.1", None);
        refuses(
            home.path(),
            &exe,
            "v0.9.1",
            "0.9.2",
            None,
            "not above the running version 0.9.2",
            None,
        );
    }

    #[test]
    fn install_ready_refuses_the_refused_tag() {
        let (home, exe) = ready_home(Some("v0.2.0"));
        refuses(
            home.path(),
            &exe,
            "v0.2.0",
            "0.1.0",
            None,
            "v0.2.0 is refused",
            None,
        );
    }

    #[test]
    fn install_ready_refuses_a_daemon_not_run_from_current() {
        let (home, _) = ready_home(None);
        let elsewhere = tempfile::tempdir().unwrap();
        let exe = elsewhere.path().join("foundryd");
        script(&exe, "foundryd 0.1.0");
        refuses(
            home.path(),
            &exe,
            "v0.2.0",
            "0.1.0",
            None,
            "not run from",
            None,
        );
    }

    #[test]
    fn install_ready_refuses_when_current_does_not_name_the_running_version() {
        let (home, exe) = ready_home(None);
        refuses(
            home.path(),
            &exe,
            "v0.2.0",
            "0.0.9",
            None,
            "current does not name the running version 0.0.9",
            None,
        );
    }

    #[test]
    fn install_ready_refuses_while_a_trial_is_on() {
        let (home, exe) = ready_home(None);
        let mut held = state(home.path()).unwrap();
        held.trial = Some(Trial {
            version: "0.1.0".into(),
            previous: "0.0.9".into(),
            starts: 1,
        });
        save(home.path(), &held).unwrap();
        refuses(
            home.path(),
            &exe,
            "v0.2.0",
            "0.1.0",
            Some("v0.2.0"),
            "a trial of 0.1.0 is on",
            None,
        );
    }

    #[test]
    fn a_refused_install_still_clears_a_trial_current_does_not_name() {
        let (home, exe) = ready_home(None);
        let mut held = state(home.path()).unwrap();
        held.trial = Some(Trial {
            version: "0.9.9".into(),
            previous: "0.1.0".into(),
            starts: 1,
        });
        save(home.path(), &held).unwrap();
        held.trial = None;
        refuses(
            home.path(),
            &exe,
            "v0.3.0",
            "0.1.0",
            Some("v0.2.0"),
            "not the ready tag",
            Some(held),
        );
    }

    #[test]
    fn install_ready_saves_the_trial_and_moves_current() {
        let (home, exe) = ready_home(Some("v0.1.5"));

        install_ready(home.path(), "v0.2.0", "0.1.0", &exe).unwrap();

        assert_eq!(current(home.path()), PathBuf::from("versions/0.2.0"));
        assert_eq!(
            state(home.path()).unwrap(),
            State {
                trial: Some(Trial {
                    version: "0.2.0".into(),
                    previous: "0.1.0".into(),
                    starts: 0,
                }),
                refused: None,
                ready: None,
            }
        );
    }

    #[test]
    fn install_ready_clears_a_stranded_trial_with_its_one_save() {
        let (home, exe) = ready_home(None);
        let mut held = state(home.path()).unwrap();
        held.trial = Some(Trial {
            version: "0.9.9".into(),
            previous: "0.1.0".into(),
            starts: 2,
        });
        save(home.path(), &held).unwrap();

        install_ready(home.path(), "v0.2.0", "0.1.0", &exe).unwrap();

        let trial = state(home.path()).unwrap().trial.unwrap();
        assert_eq!((trial.version.as_str(), trial.starts), ("0.2.0", 0));
    }

    #[test]
    fn install_ready_restores_the_state_when_it_cannot_point_current() {
        let (home, exe) = ready_home(Some("v0.1.5"));
        let before = snapshot(home.path());
        // A directory with something in it cannot be removed as the old link, so `current`
        // cannot be pointed.
        fs::create_dir_all(home.path().join("current.new")).unwrap();
        fs::write(home.path().join("current.new/in-the-way"), "x").unwrap();

        let error = install_ready(home.path(), "v0.2.0", "0.1.0", &exe).unwrap_err();

        assert!(matches!(error, InstallError::Failed(_)), "{error:?}");
        assert!(error.to_string().contains("current"), "{error}");
        assert_eq!(snapshot(home.path()).0, before.0);
        assert_eq!(snapshot(home.path()).1, before.1);
    }

    #[test]
    fn install_ready_moves_nothing_when_it_cannot_save_the_trial() {
        let (home, exe) = ready_home(Some("v0.1.5"));
        let before = snapshot(home.path());
        // A directory with something in it cannot be opened as the file the state is written
        // to, so the save fails. `current` must not have moved ahead of it.
        let temp = home.path().join("update.json.tmp");
        fs::create_dir_all(&temp).unwrap();
        fs::write(temp.join("in-the-way"), "x").unwrap();

        let error = install_ready(home.path(), "v0.2.0", "0.1.0", &exe).unwrap_err();

        assert!(matches!(error, InstallError::Failed(_)), "{error:?}");
        assert_eq!(current(home.path()), PathBuf::from("versions/0.1.0"));
        assert_eq!(snapshot(home.path()), before);
    }

    #[test]
    fn install_ready_keeps_a_higher_refusal_and_clears_a_lower_one() {
        // A home on 0.9.0, run from `current`, with `v<ready>` ready and `v0.9.2` refused.
        let on_090 = |ready: &str| {
            let home = tempfile::tempdir().unwrap();
            fs::create_dir_all(home.path().join("versions/0.9.0")).unwrap();
            point_current(home.path(), "0.9.0").unwrap();
            let exe = exe_in(home.path(), "0.9.0");
            with_ready(home.path(), ready, Some("v0.9.2"));
            (home, exe)
        };

        let (home, exe) = on_090("0.9.1");
        install_ready(home.path(), "v0.9.1", "0.9.0", &exe).unwrap();
        assert_eq!(
            state(home.path()).unwrap().refused.as_deref(),
            Some("v0.9.2")
        );

        let (home, exe) = on_090("0.9.3");
        install_ready(home.path(), "v0.9.3", "0.9.0", &exe).unwrap();
        assert_eq!(state(home.path()).unwrap().refused, None);
    }

    #[test]
    fn a_hand_install_clears_the_ready_tag_whatever_version_it_installs() {
        for version in ["0.1.0", "0.2.0", "0.3.0"] {
            let (home, _) = ready_home(None);
            commit_version(home.path(), version, || {
                placed_by_hand(home.path(), version, Some("0.1.0")).map(drop)
            })
            .unwrap();
            assert_eq!(state(home.path()).unwrap().ready, None, "{version}");
        }
    }

    #[test]
    fn install_ready_waits_for_a_hand_install_and_sees_what_it_wrote() {
        let (home, exe) = ready_home(None);
        let path = home.path().to_path_buf();
        let (inside_tx, inside) = std::sync::mpsc::channel();
        let (go, wait) = std::sync::mpsc::channel::<()>();
        let hand = {
            let path = path.clone();
            std::thread::spawn(move || {
                place_version(
                    &path,
                    "0.3.0",
                    |stage| {
                        release(stage, "0.3.0");
                        Ok(())
                    },
                    || {
                        inside_tx.send(()).unwrap();
                        wait.recv().unwrap();
                        placed_by_hand(&path, "0.3.0", Some("0.1.0")).map(drop)
                    },
                )
                .unwrap();
            })
        };
        inside.recv().unwrap();
        let done = std::sync::Arc::new(AtomicBool::new(false));
        let install = {
            let (path, flag) = (path.clone(), done.clone());
            std::thread::spawn(move || {
                let result = install_ready(&path, "v0.2.0", "0.1.0", &exe);
                flag.store(true, Ordering::SeqCst);
                result
            })
        };
        std::thread::sleep(Duration::from_millis(300));
        assert!(!done.load(Ordering::SeqCst), "it did not wait for the lock");

        go.send(()).unwrap();
        hand.join().unwrap();
        let error = install.join().unwrap().unwrap_err();

        // The hand install cleared the ready tag, so the answer is that it is not the ready
        // tag (the first rung of the ladder), with no ready tag left to report.
        match &error {
            InstallError::NotReady {
                ready: None,
                reason,
            } => {
                assert_eq!(reason, "v0.2.0 is not the ready tag");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(current(&path), PathBuf::from("versions/0.3.0"));
        assert_eq!(state(&path).unwrap().ready, None);
    }
}
