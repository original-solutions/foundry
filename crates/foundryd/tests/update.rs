//! A daemon whose update trial has used up its starts rolls back: it exits 0, `current` goes
//! back to the previous version and the tag is refused. The binary runs from `<home>/current`,
//! as the service runs it, with the file secret store and a data directory of its own.

// A release build ignores `FOUNDRY_SECRETS`, so on macOS this run would read the login
// keychain to move its secrets.
#[cfg(all(target_os = "macos", not(debug_assertions)))]
compile_error!(
    "run these tests in a debug build: a release build on macOS reads the login keychain"
);

use std::fs;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[test]
fn a_trial_that_used_its_starts_rolls_back_and_exits_cleanly() {
    let home = tempfile::tempdir().unwrap();
    fs::set_permissions(home.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let user = tempfile::tempdir().unwrap();
    let running = home.path().join("versions").join(VERSION);
    fs::create_dir_all(&running).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_foundryd"), running.join("foundryd")).unwrap();
    fs::write(running.join("foundry-holder"), "holder").unwrap();
    let previous = home.path().join("versions/0.0.1");
    fs::create_dir_all(&previous).unwrap();
    fs::write(previous.join("foundryd"), "old").unwrap();
    fs::write(previous.join("foundry-holder"), "old").unwrap();
    symlink(
        Path::new("versions").join(VERSION),
        home.path().join("current"),
    )
    .unwrap();
    fs::write(
        home.path().join("update.json"),
        format!(
            r#"{{"trial":{{"version":"{VERSION}","previous":"0.0.1","starts":3}},"refused":null}}"#
        ),
    )
    .unwrap();

    let mut child = Command::new(home.path().join("current/foundryd"))
        .arg("run")
        .env("FOUNDRY_HOME", home.path())
        .env("FOUNDRY_SECRETS", "file")
        .env("HOME", user.path())
        .spawn()
        .expect("foundryd runs");
    let end = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > end {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the daemon did not exit within 10 s");
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    assert_eq!(status.code(), Some(0), "{status:?}");
    assert_eq!(
        fs::read_link(home.path().join("current")).unwrap(),
        Path::new("versions/0.0.1")
    );
    let state: serde_json::Value =
        serde_json::from_slice(&fs::read(home.path().join("update.json")).unwrap()).unwrap();
    assert_eq!(state["refused"], format!("v{VERSION}"));
    assert!(state["trial"].is_null(), "{state}");
}
