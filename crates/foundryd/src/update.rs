//! Installs newer release tags and rolls back a version that does not stay up.
//!
//! The data directory holds `versions/<version>/{foundryd,foundry-holder}`, `current` (a
//! relative link to one of them, which the service runs) and `update.json`. A new version is
//! a trial: the daemon that starts from it counts its starts, and `update::run` calls it
//! healthy once it has run for a minute. A trial that starts three times without becoming
//! healthy is put back to the previous version and its tag is refused until a higher one
//! appears.

use std::fs::{self, OpenOptions};
use std::future::pending;
use std::io::{self, Write as _};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use nix::sys::signal::{Signal, raise};
use serde::{Deserialize, Serialize};

/// The repository the releases are read from.
const REPO: &str = "original-solutions/foundry";

/// How long the daemon must have run to count as healthy.
const HEALTHY_AFTER: Duration = Duration::from_secs(60);

/// How long to wait between two looks for a newer tag.
const CHECK_EVERY: Duration = Duration::from_secs(600);

/// Starts a trial may make without becoming healthy; the start after that rolls back.
const MAX_STARTS: u32 = 3;

/// The binaries a release holds, at the top level of its archive.
const BINARIES: [&str; 2] = ["foundryd", "foundry-holder"];

/// Why a tag could not be installed. None of these ever holds a secret.
#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    /// The release could not be listed or downloaded.
    #[error("{0}")]
    Fetch(String),
    /// The release does not hold one of the two binaries.
    #[error("the release has no {0}")]
    Missing(&'static str),
    /// The new binary does not say it is the version of the tag.
    #[error("{tag} is refused: its foundryd printed {printed:?}, not \"foundryd {version}\"")]
    WrongVersion {
        tag: String,
        version: String,
        printed: String,
    },
    /// The tag is not `vX.Y.Z`.
    #[error("{0:?} is not a release tag")]
    BadTag(String),
    /// A file or the new binary could not be handled.
    #[error("{what}: {source}")]
    Io { what: String, source: io::Error },
}

/// Labels an I/O error with what was being done.
fn io_error(what: impl Into<String>) -> impl FnOnce(io::Error) -> UpdateError {
    let what = what.into();
    move |source| UpdateError::Io { what, source }
}

/// A version that was installed and has not yet shown it stays up.
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
    /// A tag that was rolled back, like `v0.2.0`. It is skipped while it is the highest.
    #[serde(default)]
    pub refused: Option<String>,
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

/// Writes `update.json` (mode 0600) to a temporary name and renames it into place.
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
}

/// Counts this start against a trial of `running`, or rolls the trial back. Any other
/// version (a dev build, a manual run, a version that is not on trial) is never counted.
fn start_with(home: &Path, running: &str) -> io::Result<Start> {
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
    if !home.join("versions").join(&previous).is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("cannot roll back: versions/{previous} is missing"),
        ));
    }
    state.refused = Some(format!("v{running}"));
    state.trial = None;
    // The state first: a crash between the two leaves the refusal recorded and the trial
    // started once more, which rolls back again.
    save(home, &state)?;
    point_current(home, &previous)?;
    Ok(Start::RolledBack { to: previous })
}

/// Called by `foundryd run` before it does anything else. False means this version was
/// rolled back and the process must exit with success so the service starts the previous one.
pub fn starting(home: &Path) -> bool {
    match start_with(home, env!("CARGO_PKG_VERSION")) {
        Ok(Start::Run) => true,
        Ok(Start::RolledBack { to }) => {
            tracing::error!(
                "version {} did not become healthy in {MAX_STARTS} starts: back to {to}, and v{} is refused until a higher tag appears",
                env!("CARGO_PKG_VERSION"),
                env!("CARGO_PKG_VERSION"),
            );
            false
        }
        Err(error) => {
            tracing::error!("cannot check the update trial: {error}");
            true
        }
    }
}

/// Ends the trial when it is of `running`: this version has stayed up.
fn mark_healthy(home: &Path, running: &str) {
    let result = state(home).and_then(|mut state| {
        if state.trial.as_ref().is_some_and(|t| t.version == running) {
            state.trial = None;
            save(home, &state)?;
        }
        Ok(())
    });
    if let Err(error) = result {
        tracing::warn!("cannot mark {running} healthy: {error}");
    }
}

/// The tag to install: the highest valid one, if it is above `running` and is not the
/// refused one. A tag above the refused one is picked.
fn pick(tags: &[String], running: &str, refused: Option<&str>) -> Option<String> {
    let (top, version) = tags
        .iter()
        .filter_map(|tag| Some((tag, parse(tag.strip_prefix('v')?)?)))
        .max_by_key(|(_, version)| *version)?;
    let beyond = version > parse(running)?;
    (beyond && refused != Some(top.as_str())).then(|| top.clone())
}

/// True when this process runs from `<home>/current`, the only place that is updated.
fn installed(home: &Path, running: &str) -> bool {
    fs::read_link(home.join("current"))
        .is_ok_and(|target| target == Path::new("versions").join(running))
}

/// Installs `tag`: `fetch` puts the release's files in a fresh stage directory, the binaries
/// are checked, the stage becomes `versions/<version>`, the trial is recorded and then
/// `current` is pointed at it.
fn install_tag(
    home: &Path,
    tag: &str,
    running: &str,
    fetch: impl FnOnce(&str, &Path) -> Result<(), UpdateError>,
) -> Result<(), UpdateError> {
    let version = tag
        .strip_prefix('v')
        .filter(|version| parse(version).is_some())
        .ok_or_else(|| UpdateError::BadTag(tag.to_string()))?;
    let versions = home.join("versions");
    let stage = versions.join(format!(".staging-{version}"));
    let result = stage_and_swap(home, &versions, &stage, tag, version, running, fetch);
    if result.is_err() {
        let _ = fs::remove_dir_all(&stage);
    }
    result
}

fn stage_and_swap(
    home: &Path,
    versions: &Path,
    stage: &Path,
    tag: &str,
    version: &str,
    running: &str,
    fetch: impl FnOnce(&str, &Path) -> Result<(), UpdateError>,
) -> Result<(), UpdateError> {
    let _ = fs::remove_dir_all(stage);
    fs::create_dir_all(stage).map_err(io_error("cannot make the staging directory"))?;
    fetch(tag, stage)?;
    for name in BINARIES {
        let path = stage.join(name);
        if !path.is_file() {
            return Err(UpdateError::Missing(name));
        }
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
            .map_err(io_error(format!("cannot make {name} executable")))?;
    }
    let out = Command::new(stage.join("foundryd"))
        .arg("--version")
        .output()
        .map_err(io_error("cannot run the new foundryd"))?;
    let printed = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() || printed != format!("foundryd {version}\n") {
        return Err(UpdateError::WrongVersion {
            tag: tag.to_string(),
            version: version.to_string(),
            printed: printed.trim().to_string(),
        });
    }
    let target = versions.join(version);
    match fs::remove_dir_all(&target) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(io_error("cannot clear the old copy")(error)),
    }
    fs::rename(stage, &target).map_err(io_error("cannot move the new version into place"))?;
    let state = State {
        trial: Some(Trial {
            version: version.to_string(),
            previous: running.to_string(),
            starts: 0,
        }),
        refused: None,
    };
    // The trial is written before `current` moves, so a crash between the two never leaves
    // an uncounted trial running.
    save(home, &state).map_err(io_error("cannot save the update state"))?;
    point_current(home, version).map_err(io_error("cannot point current at the new version"))
}

/// Runs a command and returns its stdout, or what it said on stderr, trimmed.
fn capture(command: &mut Command) -> Result<String, UpdateError> {
    let out = command.output().map_err(|error| {
        UpdateError::Fetch(format!("cannot run {:?}: {error}", command.get_program()))
    })?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    Err(UpdateError::Fetch(format!(
        "{:?} failed: {}",
        command.get_program(),
        String::from_utf8_lossy(&out.stderr).trim()
    )))
}

/// The release tags on GitHub, read with the machine's own `gh` login.
fn list_tags() -> Result<Vec<String>, UpdateError> {
    let out = capture(Command::new("gh").args([
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
    ]))?;
    Ok(out.lines().map(|line| line.trim().to_string()).collect())
}

/// The asset a release holds for this machine.
fn asset() -> String {
    format!(
        "foundryd-{}-{}.tar.gz",
        std::env::consts::ARCH,
        std::env::consts::OS
    )
}

/// Downloads this machine's asset of `tag` into `stage` and unpacks it there.
fn download(tag: &str, stage: &Path) -> Result<(), UpdateError> {
    let asset = asset();
    capture(
        Command::new("gh")
            .args(["release", "download", tag, "--repo", REPO, "--pattern"])
            .arg(&asset)
            .arg("--dir")
            .arg(stage),
    )?;
    let archive = stage.join(&asset);
    capture(
        Command::new("tar")
            .arg("-xzf")
            .arg(&archive)
            .arg("-C")
            .arg(stage),
    )?;
    let _ = fs::remove_file(archive);
    Ok(())
}

/// One look for a newer tag. `Some` is the tag that was installed.
fn check(home: &Path, running: &str) -> Result<Option<String>, UpdateError> {
    let tags = list_tags()?;
    let refused = state(home)
        .map_err(io_error("cannot read the update state"))?
        .refused;
    let Some(tag) = pick(&tags, running, refused.as_deref()) else {
        return Ok(None);
    };
    install_tag(home, &tag, running, download)?;
    Ok(Some(tag))
}

/// Marks this version healthy after a minute, then looks for a newer tag every ten minutes.
/// After installing one it stops the daemon with SIGTERM, so the service starts the new
/// `current`. Never returns: a daemon that is not run from `current` just waits.
pub async fn run(home: PathBuf) {
    let running = env!("CARGO_PKG_VERSION");
    tokio::time::sleep(HEALTHY_AFTER).await;
    mark_healthy(&home, running);
    if !installed(&home, running) {
        tracing::info!(
            "updates are off: this daemon is not run from {}",
            home.join("current").display()
        );
        pending::<()>().await;
    }
    loop {
        let work = home.clone();
        match tokio::task::spawn_blocking(move || check(&work, running)).await {
            Ok(Ok(Some(tag))) => {
                tracing::info!("installed {tag}: stopping so the service starts it");
                if let Err(error) = raise(Signal::SIGTERM) {
                    tracing::error!("cannot stop for the update: {error}");
                }
                pending::<()>().await;
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
    use std::os::unix::fs::symlink;
    use std::sync::atomic::AtomicBool;
    use std::time::Instant;

    use super::*;

    /// A data directory with versions 0.1.0 and 0.2.0 on disk, `current` on 0.2.0 and a
    /// trial of 0.2.0 that has not started yet.
    fn trial_home() -> tempfile::TempDir {
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
                    starts: 0,
                }),
                refused: None,
            },
        )
        .unwrap();
        home
    }

    fn current(home: &Path) -> PathBuf {
        fs::read_link(home.join("current")).unwrap()
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

    fn tags(list: &[&str]) -> Vec<String> {
        list.iter().map(|tag| tag.to_string()).collect()
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
    fn install_tag_installs_points_current_and_starts_a_trial() {
        let home = installed_home();
        save(
            home.path(),
            &State {
                trial: None,
                refused: Some("v0.2.0".into()),
            },
        )
        .unwrap();

        install_tag(home.path(), "v0.2.0", "0.1.0", |tag, stage| {
            assert_eq!(tag, "v0.2.0");
            script(&stage.join("foundryd"), "foundryd 0.2.0");
            script(&stage.join("foundry-holder"), "holder");
            Ok(())
        })
        .unwrap();

        assert_eq!(current(home.path()), PathBuf::from("versions/0.2.0"));
        for name in ["foundryd", "foundry-holder"] {
            let mode = fs::metadata(home.path().join("versions/0.2.0").join(name))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o755, 0o755, "{name}");
        }
        let state = state(home.path()).unwrap();
        assert_eq!(
            state.trial,
            Some(Trial {
                version: "0.2.0".into(),
                previous: "0.1.0".into(),
                starts: 0
            })
        );
        assert_eq!(state.refused, None);
        assert!(!home.path().join("versions/.staging-0.2.0").exists());
    }

    #[test]
    fn install_tag_refuses_a_binary_that_prints_another_version() {
        let home = installed_home();

        let error = install_tag(home.path(), "v0.2.0", "0.1.0", |_, stage| {
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
    fn install_tag_refuses_an_asset_without_the_holder() {
        let home = installed_home();

        let error = install_tag(home.path(), "v0.2.0", "0.1.0", |_, stage| {
            script(&stage.join("foundryd"), "foundryd 0.2.0");
            Ok(())
        })
        .unwrap_err();

        assert!(error.to_string().contains("foundry-holder"), "{error}");
        assert_eq!(current(home.path()), PathBuf::from("versions/0.1.0"));
    }

    #[test]
    fn install_tag_passes_a_failed_fetch_on_and_changes_nothing() {
        let home = installed_home();

        let error = install_tag(home.path(), "v0.2.0", "0.1.0", |_, _| {
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

        assert!(from_current(home.path(), &home.path().join("current/foundryd"), "0.1.0"));
        assert!(from_current(home.path(), &inside, "0.1.0"));
        assert!(!from_current(home.path(), &elsewhere, "0.1.0"));
        assert!(!from_current(home.path(), &inside, "0.2.0"));
        assert!(!from_current(tempfile::tempdir().unwrap().path(), &inside, "0.1.0"));
    }

    #[test]
    fn a_rollback_that_fails_part_way_rolls_back_on_the_next_start() {
        let home = trial_home();
        for _ in 0..3 {
            start_with(home.path(), "0.2.0").unwrap();
        }
        let obstacle = home.path().join("current.new");
        fs::create_dir_all(obstacle.join("inside")).unwrap();

        assert!(start_with(home.path(), "0.2.0").is_err());
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
            },
        )
        .unwrap();

        placed_by_hand(home.path(), "0.1.0", Some("0.2.0")).unwrap();

        assert_eq!(state(home.path()).unwrap().refused.as_deref(), Some("v0.5.0"));
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

        assert!(started.elapsed() < Duration::from_secs(20), "{:?}", started.elapsed());
        assert!(matches!(error, RunError::TimedOut { .. }), "{error}");
        assert!(error.to_string().contains("sleep 31.7"), "{error}");
        let table = Command::new("ps").args(["-axo", "stat,command"]).output().unwrap();
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
            },
        )
        .unwrap();

        let done = check_with(
            home.path(),
            "0.1.0",
            || Ok(tags(&["v0.1.0", "v0.2.0"])),
            |_, _| panic!("a refused tag was fetched"),
        )
        .unwrap();

        assert_eq!(done, None);
        assert_eq!(current(home.path()), PathBuf::from("versions/0.1.0"));
    }

    #[test]
    fn check_with_installs_a_higher_tag_and_clears_the_refusal() {
        let home = installed_home();
        save(
            home.path(),
            &State {
                trial: None,
                refused: Some("v0.2.0".into()),
            },
        )
        .unwrap();

        let done = check_with(
            home.path(),
            "0.1.0",
            || Ok(tags(&["v0.1.0", "v0.2.0", "v0.3.0"])),
            |tag, stage| {
                assert_eq!(tag, "v0.3.0");
                release(stage, "0.3.0");
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(done.as_deref(), Some("v0.3.0"));
        assert_eq!(current(home.path()), PathBuf::from("versions/0.3.0"));
        let state = state(home.path()).unwrap();
        assert_eq!(state.refused, None);
        assert_eq!(state.trial.unwrap().version, "0.3.0");
    }

    #[test]
    fn check_with_refuses_a_tag_whose_asset_is_bad_so_it_is_not_fetched_again() {
        for bad in ["wrong version", "no holder"] {
            let home = installed_home();

            let result = check_with(
                home.path(),
                "0.1.0",
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
            assert_eq!(state(home.path()).unwrap().refused.as_deref(), Some("v0.2.0"), "{bad}");
            let again = check_with(
                home.path(),
                "0.1.0",
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
            || Err(UpdateError::Fetch("offline".into())),
            |_, _| panic!("fetched"),
        );
        let fetched = check_with(
            home.path(),
            "0.1.0",
            || Ok(tags(&["v0.2.0"])),
            |_, _| Err(UpdateError::Fetch("offline".into())),
        );

        assert!(listed.is_err() && fetched.is_err());
        assert_eq!(state(home.path()).unwrap().refused, None);
    }
}
