//! `foundryd install` and `foundryd uninstall`: the launchd agent on macOS and the systemd
//! unit on Linux. Both keep the daemon running, start it again when it stops, and leave the
//! holders (and so every terminal) alone when the daemon stops.

use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

use nix::unistd::{Uid, User};

use crate::daemon;
use crate::update::{self, UpdateError};

/// The launchd label, and so the plist's name.
const LABEL: &str = "io.github.original-solutions.foundryd";

/// The systemd unit's name.
const UNIT_NAME: &str = "foundryd.service";

/// Where `sudo` puts the unit.
const UNIT_PATH: &str = "/etc/systemd/system/foundryd.service";

/// The variables copied from the installing shell when they are set and not empty.
const FROM_SHELL: [&str; 4] = [
    "PATH",
    "FOUNDRY_GATEWAY_URL",
    "FOUNDRY_PORT",
    "CLAUDE_CONFIG_DIR",
];

/// The binary that holds a terminal's child; it sits beside `foundryd`.
const HOLDER: &str = "foundry-holder";

/// How long `install` waits for launchd to let go of an agent it was told to remove.
const BOOTOUT_WAIT: Duration = Duration::from_secs(10);

/// How long one `launchctl` call may take.
const LAUNCHCTL_LIMIT: Duration = Duration::from_secs(30);

/// Why an install or uninstall failed.
#[derive(Debug, thiserror::Error)]
enum ServiceError {
    #[error("set FOUNDRY_HOME or HOME to say where the data directory is")]
    NoHome,
    #[error("HOME is not set")]
    NoUserHome,
    #[error(
        "run it as the user that will run the service, not root: Claude Code refuses its permission mode as root"
    )]
    Root,
    #[error("cannot tell which user this is")]
    NoUser,
    #[error("{0}")]
    Home(String),
    #[error("{0}")]
    Place(#[from] UpdateError),
    #[error("{what}: {source}")]
    Io { what: String, source: io::Error },
    #[error("{0}")]
    Command(String),
    #[error("this operating system has no supported service manager; only macOS and Linux do")]
    Unsupported,
}

/// Labels an I/O error with what was being done.
fn io_error(what: impl Into<String>) -> impl FnOnce(io::Error) -> ServiceError {
    let what = what.into();
    move |source| ServiceError::Io { what, source }
}

/// The variables the service runs with: `FOUNDRY_HOME` always, the others as `lookup` gives
/// them. `lookup` is the installing shell's environment.
fn environment(lookup: impl Fn(&str) -> Option<String>, home: &Path) -> Vec<(String, String)> {
    let mut vars = vec![("FOUNDRY_HOME".to_string(), home.display().to_string())];
    for name in FROM_SHELL {
        if let Some(value) = lookup(name).filter(|value| !value.is_empty()) {
            vars.push((name.to_string(), value));
        }
    }
    vars
}

/// Escapes text for an XML string.
fn xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// The launchd agent. `AbandonProcessGroup` keeps launchd from killing the holders when it
/// stops the daemon.
fn plist(program: &Path, env: &[(String, String)], log: &Path) -> String {
    let program = xml(&program.display().to_string());
    let log = xml(&log.display().to_string());
    let vars: String = env
        .iter()
        .map(|(key, value)| {
            format!(
                "\t\t<key>{}</key>\n\t\t<string>{}</string>\n",
                xml(key),
                xml(value)
            )
        })
        .collect();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n\
<dict>\n\
\t<key>Label</key>\n\t<string>{LABEL}</string>\n\
\t<key>ProgramArguments</key>\n\t<array>\n\t\t<string>{program}</string>\n\t\t<string>run</string>\n\t</array>\n\
\t<key>EnvironmentVariables</key>\n\t<dict>\n{vars}\t</dict>\n\
\t<key>RunAtLoad</key>\n\t<true/>\n\
\t<key>KeepAlive</key>\n\t<true/>\n\
\t<key>AbandonProcessGroup</key>\n\t<true/>\n\
\t<key>ThrottleInterval</key>\n\t<integer>10</integer>\n\
\t<key>StandardOutPath</key>\n\t<string>{log}</string>\n\
\t<key>StandardErrorPath</key>\n\t<string>{log}</string>\n\
</dict>\n\
</plist>\n"
    )
}

/// Quotes text for a systemd value inside double quotes.
fn systemd(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%")
}

/// The systemd unit. It runs as `user`, never root, and `KillMode=process` keeps systemd from
/// killing the holders when it stops the daemon.
fn unit(program: &Path, env: &[(String, String)], user: &str) -> Result<String, ServiceError> {
    if user == "root" {
        return Err(ServiceError::Root);
    }
    let program = systemd(&program.display().to_string());
    let vars: String = env
        .iter()
        .map(|(key, value)| format!("Environment=\"{}\"\n", systemd(&format!("{key}={value}"))))
        .collect();
    Ok(format!(
        "[Unit]\n\
Description=Foundry daemon\n\
After=network-online.target\n\
Wants=network-online.target\n\
\n\
[Service]\n\
User={user}\n\
ExecStart=\"{program}\" run\n\
{vars}\
Restart=always\n\
RestartSec=10\n\
KillMode=process\n\
\n\
[Install]\n\
WantedBy=multi-user.target\n"
    ))
}

/// Puts `exe` and the holder beside it in `versions/<version>` and points `current` there,
/// and records the install: a version lower than the one `current` named is kept by refusing
/// the higher one. Returns the tag a go-back left refused. A copy is made in a stage
/// directory outside the update lock and renamed in under it; when `exe` already runs from that
/// directory nothing is copied.
fn place(home: &Path, exe: &Path, version: &str) -> Result<Option<String>, UpdateError> {
    let from = exe.parent().unwrap_or(Path::new("."));
    let holder = from.join(HOLDER);
    if !holder.is_file() {
        return Err(UpdateError::Missing(HOLDER));
    }
    let mut refused = None;
    // Runs under the update lock, before `current` moves, so `current` still names the
    // version this install replaces.
    let record = || {
        let before = update::current_version(home);
        refused = update::placed_by_hand(home, version, before.as_deref())?;
        Ok(())
    };
    let in_place = match (
        fs::canonicalize(from),
        fs::canonicalize(home.join("versions").join(version)),
    ) {
        (Ok(from), Ok(target)) => from == target,
        _ => false,
    };
    if in_place {
        update::commit_version(home, version, record)?;
        return Ok(refused);
    }
    let copy = |stage: &Path| {
        for (from, name) in [(exe, "foundryd"), (holder.as_path(), HOLDER)] {
            fs::copy(from, stage.join(name))
                .map_err(update::io_error(format!("cannot copy {}", from.display())))?;
        }
        Ok(())
    };
    update::place_version(home, version, copy, record)?;
    Ok(refused)
}

/// What `install` prints once the version is placed: where, and when it went back to a lower
/// version, which tag is refused.
fn placed_note(home: &Path, version: &str, refused: Option<&str>) -> String {
    let back = refused
        .map(|tag| format!("Went back to {version}; {tag} is refused until a higher tag ships.\n"))
        .unwrap_or_default();
    format!(
        "Placed version {version} in {}\n{back}",
        home.join("versions").display()
    )
}

/// Makes the data directory as `foundryd run` does (and refuses one other users can reach),
/// then places this version. Returns the tag a go-back left refused.
fn stage(home: &Path, exe: &Path, version: &str) -> Result<Option<String>, ServiceError> {
    daemon::make_home(home).map_err(|error| ServiceError::Home(error.to_string()))?;
    Ok(place(home, exe, version)?)
}

/// Runs a `launchctl` call within its limit.
fn launchctl(args: &[&str]) -> Result<(), ServiceError> {
    update::run_bounded(Command::new("launchctl").args(args), LAUNCHCTL_LIMIT)
        .map(|_| ())
        .map_err(|error| ServiceError::Command(error.to_string()))
}

/// `gui/<uid>/<label>`, the launchd name of the agent.
fn launchd_target() -> String {
    format!("gui/{}/{LABEL}", Uid::current())
}

/// Where the launchd agent's plist lives.
fn plist_path() -> Result<PathBuf, ServiceError> {
    let home = std::env::var_os("HOME").ok_or(ServiceError::NoUserHome)?;
    Ok(PathBuf::from(home)
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist")))
}

/// Stops and removes the agent. A failure is ignored: it may not be loaded.
fn bootout() {
    let _ = launchctl(&["bootout", &launchd_target()]);
}

fn install_macos(home: &Path) -> Result<String, ServiceError> {
    let user_home = PathBuf::from(std::env::var_os("HOME").ok_or(ServiceError::NoUserHome)?);
    let log = user_home.join("Library/Logs/foundry/foundryd.log");
    let path = plist_path()?;
    let program = home.join("current/foundryd");
    let text = plist(
        &program,
        &environment(|n| std::env::var(n).ok(), home),
        &log,
    );
    for dir in [path.parent(), log.parent()].into_iter().flatten() {
        fs::create_dir_all(dir).map_err(io_error(format!("cannot make {}", dir.display())))?;
    }
    fs::write(&path, text).map_err(io_error(format!("cannot write {}", path.display())))?;
    bootout();
    let started = Instant::now();
    while started.elapsed() < BOOTOUT_WAIT {
        if launchctl(&["print", &launchd_target()]).is_err() {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    launchctl(&[
        "bootstrap",
        &format!("gui/{}", Uid::current()),
        &path.display().to_string(),
    ])?;
    Ok(format!(
        "Installed the launchd agent {}\nIt runs {} and logs to {}",
        path.display(),
        program.display(),
        log.display()
    ))
}

fn install_linux(home: &Path) -> Result<String, ServiceError> {
    let user = User::from_uid(Uid::current())
        .ok()
        .flatten()
        .ok_or(ServiceError::NoUser)?
        .name;
    let program = home.join("current/foundryd");
    let text = unit(
        &program,
        &environment(|n| std::env::var(n).ok(), home),
        &user,
    )?;
    let file = home.join(UNIT_NAME);
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o644)
        .open(&file)
        .and_then(|mut f| f.write_all(text.as_bytes()))
        .map_err(io_error(format!("cannot write {}", file.display())))?;
    Ok(format!(
        "Wrote the systemd unit {file}\nInstall and start it with:\n  sudo install -m 644 {file} {UNIT_PATH}\n  sudo systemctl daemon-reload\n  sudo systemctl enable foundryd\n  sudo systemctl restart foundryd",
        file = file.display()
    ))
}

/// The data directory as the service will see it. A relative one is made absolute against
/// this shell's directory: the service starts elsewhere, and would read it from there.
fn service_home(home: Option<PathBuf>) -> Result<PathBuf, ServiceError> {
    let home = home.ok_or(ServiceError::NoHome)?;
    std::path::absolute(home).map_err(|_| ServiceError::NoHome)
}

fn do_install() -> Result<String, ServiceError> {
    let home = service_home(daemon::home())?;
    if !cfg!(any(target_os = "macos", target_os = "linux")) {
        return Err(ServiceError::Unsupported);
    }
    if Uid::current().is_root() {
        return Err(ServiceError::Root);
    }
    let exe = std::env::current_exe().map_err(io_error("cannot find this program"))?;
    let version = env!("CARGO_PKG_VERSION");
    let refused = stage(&home, &exe, version)?;
    let done = if cfg!(target_os = "macos") {
        install_macos(&home)?
    } else {
        install_linux(&home)?
    };
    Ok(format!(
        "{}{done}",
        placed_note(&home, version, refused.as_deref())
    ))
}

/// Removes `path`; one that is already gone is fine.
fn remove(path: &Path) -> Result<(), ServiceError> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            Err(io_error(format!("cannot remove {}", path.display()))(error))
        }
        _ => Ok(()),
    }
}

fn do_uninstall() -> Result<String, ServiceError> {
    let home = service_home(daemon::home())?;
    let kept = format!(
        "The data directory {} and its versions are kept.",
        home.display()
    );
    if cfg!(target_os = "macos") {
        let path = plist_path()?;
        bootout();
        remove(&path)?;
        Ok(format!(
            "Removed the launchd agent {}\n{kept}",
            path.display()
        ))
    } else if cfg!(target_os = "linux") {
        let file = home.join(UNIT_NAME);
        remove(&file)?;
        Ok(format!(
            "Removed {}\nStop and remove the service with:\n  sudo systemctl disable --now foundryd\n  sudo rm {UNIT_PATH}\n  sudo systemctl daemon-reload\n{kept}",
            file.display()
        ))
    } else {
        Err(ServiceError::Unsupported)
    }
}

/// Prints the result and turns it into the exit code.
fn report(command: &str, result: Result<String, ServiceError>) -> ExitCode {
    match result {
        Ok(text) => {
            println!("{text}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("foundryd {command}: {error}");
            ExitCode::FAILURE
        }
    }
}

/// `foundryd install`.
pub fn install() -> ExitCode {
    report("install", do_install())
}

/// `foundryd uninstall`.
pub fn uninstall() -> ExitCode {
    report("uninstall", do_uninstall())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt};

    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn program() -> PathBuf {
        PathBuf::from("/data/foundry/current/foundryd")
    }

    #[test]
    fn the_plist_spares_child_processes_and_keeps_the_daemon_alive() {
        let text = plist(&program(), &env(&[]), Path::new("/logs/foundryd.log"));

        assert!(
            text.contains("<key>AbandonProcessGroup</key>\n\t<true/>"),
            "{text}"
        );
        assert!(text.contains("<key>KeepAlive</key>\n\t<true/>"), "{text}");
        assert!(text.contains("<key>RunAtLoad</key>\n\t<true/>"), "{text}");
        assert!(text.contains("<string>/data/foundry/current/foundryd</string>"));
        assert!(text.contains("<string>run</string>"));
        assert!(text.contains(LABEL));
        assert!(text.contains("<string>/logs/foundryd.log</string>"));
    }

    #[test]
    fn the_plist_carries_the_gateway_url_only_when_it_is_given() {
        let with = plist(
            &program(),
            &env(&[("FOUNDRY_GATEWAY_URL", "https://gw.example")]),
            Path::new("/l"),
        );
        let without = plist(&program(), &env(&[("FOUNDRY_HOME", "/h")]), Path::new("/l"));

        assert!(with.contains("<key>FOUNDRY_GATEWAY_URL</key>"));
        assert!(with.contains("<string>https://gw.example</string>"));
        assert!(!without.contains("FOUNDRY_GATEWAY_URL"));
        assert!(without.contains("<key>FOUNDRY_HOME</key>"));
    }

    #[test]
    fn the_plist_escapes_xml() {
        let text = plist(
            &program(),
            &env(&[("PATH", "/a&b:<c>\"'")]),
            Path::new("/l"),
        );

        assert!(text.contains("/a&amp;b:&lt;c&gt;&quot;&apos;"), "{text}");
    }

    #[test]
    fn the_unit_spares_child_processes_and_runs_as_the_user() {
        let text = unit(&program(), &env(&[]), "foundry").unwrap();

        assert!(text.contains("\nKillMode=process\n"), "{text}");
        assert!(text.contains("\nUser=foundry\n"), "{text}");
        assert!(text.contains("\nRestart=always\n"), "{text}");
        assert!(text.contains("\nExecStart=\"/data/foundry/current/foundryd\" run\n"));
        assert!(text.contains("WantedBy=multi-user.target"));
    }

    #[test]
    fn the_unit_carries_the_gateway_url_only_when_it_is_given() {
        let with = unit(
            &program(),
            &env(&[("FOUNDRY_GATEWAY_URL", "https://gw.example")]),
            "foundry",
        )
        .unwrap();
        let without = unit(&program(), &env(&[("FOUNDRY_HOME", "/h")]), "foundry").unwrap();

        assert!(with.contains("\nEnvironment=\"FOUNDRY_GATEWAY_URL=https://gw.example\"\n"));
        assert!(!without.contains("FOUNDRY_GATEWAY_URL"));
        assert!(without.contains("Environment=\"FOUNDRY_HOME=/h\""));
    }

    #[test]
    fn the_unit_quotes_for_systemd() {
        let text = unit(
            &PathBuf::from("/d/100%/a\"b\\c/foundryd"),
            &env(&[("PATH", "/x%y\"z")]),
            "foundry",
        )
        .unwrap();

        assert!(
            text.contains("ExecStart=\"/d/100%%/a\\\"b\\\\c/foundryd\" run"),
            "{text}"
        );
        assert!(text.contains("Environment=\"PATH=/x%%y\\\"z\""), "{text}");
    }

    #[test]
    fn the_unit_refuses_root() {
        let error = unit(&program(), &env(&[]), "root").unwrap_err();

        assert!(error.to_string().contains("root"), "{error}");
    }

    #[test]
    fn the_environment_always_has_home_and_takes_the_listed_variables_when_set() {
        let lookup = |name: &str| match name {
            "PATH" => Some("/usr/bin".to_string()),
            "FOUNDRY_GATEWAY_URL" => Some("https://gw".to_string()),
            "FOUNDRY_PORT" => Some(String::new()),
            "HOME" => Some("/ignored".to_string()),
            _ => None,
        };

        let vars = environment(lookup, Path::new("/data"));

        assert_eq!(
            vars,
            env(&[
                ("FOUNDRY_HOME", "/data"),
                ("PATH", "/usr/bin"),
                ("FOUNDRY_GATEWAY_URL", "https://gw"),
            ])
        );
    }

    #[test]
    fn place_copies_both_binaries_executable_and_points_current() {
        let home = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        fs::write(src.path().join("foundryd"), "daemon").unwrap();
        fs::write(src.path().join("foundry-holder"), "holder").unwrap();

        place(home.path(), &src.path().join("foundryd"), "0.1.0").unwrap();

        let dir = home.path().join("versions/0.1.0");
        assert_eq!(fs::read_to_string(dir.join("foundryd")).unwrap(), "daemon");
        assert_eq!(
            fs::read_to_string(dir.join("foundry-holder")).unwrap(),
            "holder"
        );
        for name in ["foundryd", "foundry-holder"] {
            let mode = fs::metadata(dir.join(name)).unwrap().permissions().mode();
            assert_eq!(mode & 0o755, 0o755, "{name}");
        }
        assert_eq!(
            fs::read_link(home.path().join("current")).unwrap(),
            PathBuf::from("versions/0.1.0")
        );
    }

    #[test]
    fn place_names_a_missing_holder_and_changes_nothing() {
        let home = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        fs::write(src.path().join("foundryd"), "daemon").unwrap();

        let error = place(home.path(), &src.path().join("foundryd"), "0.1.0").unwrap_err();

        assert!(error.to_string().contains("foundry-holder"), "{error}");
        assert!(!home.path().join("current").exists());
    }

    #[test]
    fn place_from_inside_the_version_directory_copies_nothing() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("versions/0.1.0");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("foundryd"), "daemon").unwrap();
        fs::write(dir.join("foundry-holder"), "holder").unwrap();

        let inode = fs::metadata(dir.join("foundryd")).unwrap().ino();

        place(home.path(), &dir.join("foundryd"), "0.1.0").unwrap();

        assert_eq!(fs::metadata(dir.join("foundryd")).unwrap().ino(), inode);
        assert_eq!(fs::read_to_string(dir.join("foundryd")).unwrap(), "daemon");
        assert_eq!(
            fs::read_link(home.path().join("current")).unwrap(),
            PathBuf::from("versions/0.1.0")
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_plist_passes_plutil() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.plist");
        let text = plist(
            &program(),
            &env(&[("PATH", "/a&b"), ("FOUNDRY_HOME", "/h")]),
            Path::new("/logs/f.log"),
        );
        fs::write(&file, text).unwrap();

        let out = std::process::Command::new("plutil")
            .arg("-lint")
            .arg(&file)
            .output()
            .unwrap();

        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stdout)
        );
    }

    fn binaries(dir: &Path, daemon: &str) {
        fs::write(dir.join("foundryd"), daemon).unwrap();
        fs::write(dir.join("foundry-holder"), "holder").unwrap();
    }

    #[test]
    fn a_hand_install_of_an_older_version_refuses_the_newer_one() {
        let home = tempfile::tempdir().unwrap();
        let (newer, older) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        binaries(newer.path(), "two");
        binaries(older.path(), "one");

        place(home.path(), &newer.path().join("foundryd"), "0.2.0").unwrap();
        place(home.path(), &older.path().join("foundryd"), "0.1.0").unwrap();

        let state = update::state(home.path()).unwrap();
        assert_eq!(state.refused.as_deref(), Some("v0.2.0"));
        assert_eq!(state.trial, None);
    }

    #[test]
    fn a_data_directory_other_users_can_reach_is_refused_and_nothing_is_written() {
        let home = tempfile::tempdir().unwrap();
        fs::set_permissions(home.path(), fs::Permissions::from_mode(0o755)).unwrap();
        let src = tempfile::tempdir().unwrap();
        binaries(src.path(), "daemon");

        let error = stage(home.path(), &src.path().join("foundryd"), "0.1.0").unwrap_err();

        assert!(error.to_string().contains("chmod"), "{error}");
        assert!(!home.path().join("versions").exists());
        assert!(!home.path().join("current").exists());
    }

    #[test]
    fn a_private_data_directory_is_made_and_filled() {
        let parent = tempfile::tempdir().unwrap();
        let home = parent.path().join("new/home");
        let src = tempfile::tempdir().unwrap();
        binaries(src.path(), "daemon");

        stage(&home, &src.path().join("foundryd"), "0.1.0").unwrap();

        assert_eq!(
            fs::metadata(&home).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert!(home.join("current/foundryd").is_file());
    }

    #[test]
    fn placing_from_inside_a_lower_version_goes_back_and_refuses_the_higher() {
        let home = tempfile::tempdir().unwrap();
        let newer = tempfile::tempdir().unwrap();
        binaries(newer.path(), "two");
        place(home.path(), &newer.path().join("foundryd"), "0.2.0").unwrap();
        let old = home.path().join("versions/0.1.0");
        fs::create_dir_all(&old).unwrap();
        binaries(&old, "one");

        let back = place(home.path(), &old.join("foundryd"), "0.1.0").unwrap();

        assert_eq!(back.as_deref(), Some("v0.2.0"));
        assert_eq!(
            fs::read_link(home.path().join("current")).unwrap(),
            PathBuf::from("versions/0.1.0")
        );
        let state = update::state(home.path()).unwrap();
        assert_eq!(state.refused.as_deref(), Some("v0.2.0"));
        assert_eq!(state.trial, None);
    }

    #[test]
    fn place_says_which_tag_a_go_back_left_refused_and_nothing_going_forward() {
        let home = tempfile::tempdir().unwrap();
        let (one, two) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        binaries(one.path(), "one");
        binaries(two.path(), "two");

        let first = place(home.path(), &one.path().join("foundryd"), "0.1.0").unwrap();
        let forward = place(home.path(), &two.path().join("foundryd"), "0.2.0").unwrap();
        let back = place(home.path(), &one.path().join("foundryd"), "0.1.0").unwrap();

        assert_eq!(first, None);
        assert_eq!(forward, None);
        assert_eq!(back.as_deref(), Some("v0.2.0"));
    }

    #[test]
    fn place_reports_a_higher_refusal_that_was_already_held() {
        let home = tempfile::tempdir().unwrap();
        let (three, two) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        binaries(three.path(), "three");
        binaries(two.path(), "two");
        place(home.path(), &three.path().join("foundryd"), "0.3.0").unwrap();
        fs::write(
            home.path().join("update.json"),
            r#"{"trial":null,"refused":"v0.4.0"}"#,
        )
        .unwrap();

        let back = place(home.path(), &two.path().join("foundryd"), "0.2.0").unwrap();

        assert_eq!(back.as_deref(), Some("v0.4.0"));
    }

    #[test]
    fn stage_passes_the_refused_tag_on() {
        let home = tempfile::tempdir().unwrap();
        fs::set_permissions(home.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let (two, one) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        binaries(two.path(), "two");
        binaries(one.path(), "one");

        let first = stage(home.path(), &two.path().join("foundryd"), "0.2.0").unwrap();
        let back = stage(home.path(), &one.path().join("foundryd"), "0.1.0").unwrap();

        assert_eq!(first, None);
        assert_eq!(back.as_deref(), Some("v0.2.0"));
    }

    #[test]
    fn the_placed_note_names_a_go_back_and_says_nothing_otherwise() {
        let home = Path::new("/data");

        let back = placed_note(home, "0.1.0", Some("v0.2.0"));
        let plain = placed_note(home, "0.1.0", None);

        assert!(
            back.contains("Placed version 0.1.0 in /data/versions\n"),
            "{back}"
        );
        assert!(
            back.contains("Went back to 0.1.0; v0.2.0 is refused until a higher tag ships.\n"),
            "{back}"
        );
        assert!(
            plain.contains("Placed version 0.1.0 in /data/versions\n"),
            "{plain}"
        );
        assert!(!plain.contains("Went back"), "{plain}");
    }

    #[test]
    fn a_relative_data_directory_is_made_absolute_for_the_service() {
        let cwd = std::env::current_dir().unwrap();

        let home = service_home(Some(PathBuf::from("data/foundry"))).unwrap();

        assert_eq!(home, cwd.join("data/foundry"));
        assert_eq!(
            service_home(Some(PathBuf::from("/srv/foundry"))).unwrap(),
            PathBuf::from("/srv/foundry")
        );
    }

    #[test]
    fn no_data_directory_or_an_empty_one_is_refused() {
        assert!(matches!(service_home(None), Err(ServiceError::NoHome)));
        assert!(matches!(
            service_home(Some(PathBuf::new())),
            Err(ServiceError::NoHome)
        ));
    }

    #[test]
    fn a_hand_install_clears_the_ready_tag_for_a_version_below_equal_to_and_above_it() {
        for version in ["0.1.0", "0.2.0", "0.3.0"] {
            let home = tempfile::tempdir().unwrap();
            let src = tempfile::tempdir().unwrap();
            binaries(src.path(), version);
            fs::write(home.path().join("update.json"), r#"{"ready":"v0.2.0"}"#).unwrap();

            place(home.path(), &src.path().join("foundryd"), version).unwrap();

            assert_eq!(update::state(home.path()).unwrap().ready, None, "{version}");
        }
    }
}
