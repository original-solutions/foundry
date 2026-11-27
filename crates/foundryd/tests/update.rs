//! A daemon whose update trial has used up its starts rolls back: it exits 0, `current` goes
//! back to the previous version and the tag is refused. A daemon asked to install its ready
//! release answers `202`, points `current` at it with a trial and exits. The binary runs from
//! `<home>/current`, as the service runs it, with the file secret store and a data directory of
//! its own.

// A release build ignores `FOUNDRY_SECRETS`, so on macOS this run would copy secrets out of the
// login keychain.
#[cfg(all(target_os = "macos", not(debug_assertions)))]
compile_error!(
    "run these tests in a debug build: a release build on macOS copies secrets out of the login keychain"
);

use std::fs;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::Path;
use std::process::{Child, Command, Stdio};
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

/// Waits up to `limit` for `child` to exit, and kills it if it has not.
fn exited(child: &mut Child, limit: Duration, log: &Path) -> std::process::ExitStatus {
    let end = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() > end {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "the daemon did not exit within {limit:?}; {}",
                fs::read_to_string(log).unwrap_or_default()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_daemon_asked_to_install_its_ready_release_answers_202_and_exits_onto_it_on_trial() {
    // A terminal's socket path is at most about 100 bytes, which the system's own temporary
    // folder on macOS would take most of.
    let home = tempfile::Builder::new()
        .prefix("fu")
        .tempdir_in("/tmp")
        .unwrap();
    fs::set_permissions(home.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let user = tempfile::tempdir().unwrap();
    let running = home.path().join("versions").join(VERSION);
    fs::create_dir_all(&running).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_foundryd"), running.join("foundryd")).unwrap();
    fs::write(running.join("foundry-holder"), "holder").unwrap();
    let next = home.path().join("versions/999.0.0");
    fs::create_dir_all(&next).unwrap();
    fs::write(next.join("foundryd"), "new").unwrap();
    fs::write(next.join("foundry-holder"), "new").unwrap();
    symlink(
        Path::new("versions").join(VERSION),
        home.path().join("current"),
    )
    .unwrap();
    fs::write(home.path().join("update.json"), r#"{"ready":"v999.0.0"}"#).unwrap();
    // A stand-in for the Tailscale CLI that answers as a stopped Tailscale does.
    let tailscale = user.path().join("tailscale");
    fs::write(
        &tailscale,
        "#!/bin/sh\necho 'Tailscale is stopped (fake).' >&2\nexit 1\n",
    )
    .unwrap();
    fs::set_permissions(&tailscale, fs::Permissions::from_mode(0o755)).unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let log = user.path().join("daemon.log");

    let mut child = Command::new(home.path().join("current/foundryd"))
        .arg("run")
        .env("FOUNDRY_HOME", home.path())
        .env("FOUNDRY_PORT", port.to_string())
        .env("FOUNDRY_SECRETS", "file")
        .env("FOUNDRY_TAILSCALE", &tailscale)
        .env("FOUNDRY_CLOCK_AHEAD", user.path().join("clock-ahead"))
        .env("HOME", user.path())
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("FOUNDRY_GATEWAY_URL")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(fs::File::create(&log).unwrap())
        .spawn()
        .expect("foundryd runs");
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(20)))
        .build()
        .into();
    let base = format!("http://127.0.0.1:{port}");
    let end = Instant::now() + Duration::from_secs(30);
    let me = loop {
        if let Some(status) = child.try_wait().unwrap() {
            panic!(
                "the daemon exited at start: {status}; {}",
                fs::read_to_string(&log).unwrap_or_default()
            );
        }
        if let Ok(mut answer) = agent.get(format!("{base}/api/machine")).call()
            && answer.status() == 200
        {
            let machine: serde_json::Value = answer.body_mut().read_json().unwrap();
            break machine["id"].as_str().unwrap().to_owned();
        }
        if Instant::now() > end {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "the daemon did not answer; {}",
                fs::read_to_string(&log).unwrap_or_default()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    let answer = agent
        .post(format!("{base}/api/machines/{me}/update"))
        .header("content-type", "application/json")
        .send(r#"{"tag":"v999.0.0"}"#);
    let status = match answer {
        Ok(mut answer) => {
            // Read whole, to its last chunk: a stop before the answer was written cuts it off.
            let body = answer.body_mut().read_to_string().unwrap();
            assert_eq!(answer.status(), 202, "{body}");
            assert_eq!(body, "");
            exited(&mut child, Duration::from_secs(10), &log)
        }
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the install was not answered: {error}");
        }
    };

    assert_eq!(status.code(), Some(0), "{status:?}");
    assert_eq!(
        fs::read_link(home.path().join("current")).unwrap(),
        Path::new("versions/999.0.0")
    );
    let state: serde_json::Value =
        serde_json::from_slice(&fs::read(home.path().join("update.json")).unwrap()).unwrap();
    assert_eq!(state["trial"]["version"], "999.0.0", "{state}");
    assert_eq!(state["trial"]["previous"], VERSION, "{state}");
    assert!(state["ready"].is_null(), "{state}");
}
