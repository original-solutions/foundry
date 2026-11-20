//! `foundryd install` and `uninstall` end to end, with a data directory and a `HOME` of
//! their own. On macOS a fake `launchctl` first on `PATH` logs what it is asked, so the real
//! one is never called and no agent is loaded.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const GATEWAY: &str = "https://gateway.example";

/// A copy of `foundryd` with a holder beside it, as a release unpacks them.
fn release(dir: &Path) -> PathBuf {
    let exe = dir.join("foundryd");
    fs::copy(env!("CARGO_BIN_EXE_foundryd"), &exe).unwrap();
    fs::write(dir.join("foundry-holder"), "holder").unwrap();
    exe
}

/// A data directory only this user can reach, as `install` requires.
fn data_home() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    fs::set_permissions(home.path(), fs::Permissions::from_mode(0o700)).unwrap();
    home
}

/// Runs `exe <command>` with `bin` first on `PATH` and the gateway URL set.
fn run(exe: &Path, command: &str, home: &Path, user: &Path, bin: &Path) -> Output {
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    Command::new(exe)
        .arg(command)
        .env("FOUNDRY_HOME", home)
        .env("HOME", user)
        .env("PATH", path)
        .env("FOUNDRY_GATEWAY_URL", GATEWAY)
        .env("FOUNDRY_SECRETS", "file")
        .output()
        .unwrap()
}

fn assert_ok(out: &Output, what: &str) {
    assert!(
        out.status.success(),
        "{what}: {:?}\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[cfg(target_os = "macos")]
#[test]
fn install_writes_and_loads_the_agent_and_uninstall_unloads_and_removes_it() {
    let home = data_home();
    let user = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let exe = release(work.path());
    let bin = work.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let log = work.path().join("launchctl.log");
    let launchctl = bin.join("launchctl");
    // `print` fails, as for an agent that is not loaded, so install stops waiting at once.
    fs::write(
        &launchctl,
        format!(
            "#!/bin/sh\necho \"$1\" >> '{}'\n[ \"$1\" = print ] && exit 1\nexit 0\n",
            log.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&launchctl, fs::Permissions::from_mode(0o755)).unwrap();
    let plist = user
        .path()
        .join("Library/LaunchAgents/io.github.original-solutions.foundryd.plist");

    let out = run(&exe, "install", home.path(), user.path(), &bin);

    assert_ok(&out, "install");
    let text = fs::read_to_string(&plist).unwrap();
    assert!(text.contains(GATEWAY), "{text}");
    assert!(
        text.contains(&home.path().join("current/foundryd").display().to_string()),
        "{text}"
    );
    assert!(home.path().join("current/foundryd").is_file());
    let calls = fs::read_to_string(&log).unwrap();
    let calls: Vec<&str> = calls.lines().collect();
    let bootout = calls.iter().position(|c| *c == "bootout");
    let bootstrap = calls.iter().position(|c| *c == "bootstrap");
    assert!(
        bootout.is_some() && bootout < bootstrap,
        "launchctl calls: {calls:?}"
    );

    let out = run(&exe, "uninstall", home.path(), user.path(), &bin);

    assert_ok(&out, "uninstall");
    assert!(!plist.exists());
    let calls = fs::read_to_string(&log).unwrap();
    assert_eq!(calls.lines().last(), Some("bootout"), "{calls}");
}

#[cfg(target_os = "linux")]
#[test]
fn install_writes_the_unit_and_uninstall_removes_it() {
    let home = data_home();
    let user = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let exe = release(work.path());
    let bin = work.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let unit = home.path().join("foundryd.service");

    let out = run(&exe, "install", home.path(), user.path(), &bin);

    assert_ok(&out, "install");
    let text = fs::read_to_string(&unit).unwrap();
    assert!(
        text.contains(&format!("Environment=\"FOUNDRY_GATEWAY_URL={GATEWAY}\"")),
        "{text}"
    );
    assert_eq!(
        fs::metadata(&unit).unwrap().permissions().mode() & 0o777,
        0o644
    );
    assert!(home.path().join("current/foundryd").is_file());

    let out = run(&exe, "uninstall", home.path(), user.path(), &bin);

    assert_ok(&out, "uninstall");
    assert!(!unit.exists());
}
