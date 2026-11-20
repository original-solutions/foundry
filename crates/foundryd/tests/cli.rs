//! The command line: `--version` works, `join` names a bad port, and `secret set`
//! stores a gateway key without printing it. Every test that runs the binary uses the file
//! secret store and a temporary data directory, never the login keychain.

// A release build ignores `FOUNDRY_SECRETS`, so on macOS these runs of the binary would use
// the login keychain.
#[cfg(all(target_os = "macos", not(debug_assertions)))]
compile_error!(
    "run these tests in a debug build: a release build on macOS uses the login keychain"
);

use std::io::Write as _;
use std::process::{Command, Output, Stdio};

use foundryd::secrets::{FileSecrets, Secrets};

/// `foundryd` with the file secret store and a data directory and `HOME` of its own, so no
/// run touches the login keychain or the real home.
fn isolated(args: &[&str], home: &tempfile::TempDir) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_foundryd"));
    command
        .args(args)
        .env("FOUNDRY_HOME", home.path())
        .env("FOUNDRY_SECRETS", "file")
        .env("HOME", home.path());
    command
}

fn foundryd(args: &[&str]) -> Output {
    let home = tempfile::tempdir().unwrap();
    isolated(args, &home).output().expect("foundryd runs")
}

#[test]
fn version_prints_the_crate_version() {
    let out = foundryd(&["--version"]);

    assert!(out.status.success(), "exit: {:?}", out.status);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("foundryd {}\n", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn join_with_a_port_that_is_not_a_number_exits_1_and_names_it() {
    let home = tempfile::tempdir().unwrap();
    let out = isolated(&["join", "100.64.0.1", "ABCDEFGHJK"], &home)
        .env("FOUNDRY_PORT", "not-a-port")
        .output()
        .expect("foundryd runs");
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert_eq!(out.status.code(), Some(1), "exit: {:?}", out.status);
    assert!(stderr.contains("FOUNDRY_PORT"), "stderr: {stderr}");
}

/// Runs `foundryd secret set <name>` with `input` on stdin, in a data directory of its own.
fn secret_set(home: &tempfile::TempDir, name: &str, input: &str) -> Output {
    let mut child = isolated(&["secret", "set", name], home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("foundryd runs");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn held(home: &tempfile::TempDir, name: &str) -> Option<String> {
    FileSecrets::new(home.path().join("secrets.json"))
        .get(name)
        .unwrap()
}

#[test]
fn secret_set_stores_the_gateway_keys_and_never_prints_them() {
    for name in ["gateway", "gateway-management"] {
        let home = tempfile::tempdir().unwrap();

        let out = secret_set(&home, name, "sk-abc\n");

        assert_eq!(out.status.code(), Some(0), "{name}: {:?}", out.status);
        let printed = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!printed.contains("sk-abc"), "{name} printed: {printed}");
        assert_eq!(held(&home, name).as_deref(), Some("sk-abc"));
    }
}

#[test]
fn secret_set_with_no_value_exits_1_and_stores_nothing() {
    for input in ["", "\n"] {
        let home = tempfile::tempdir().unwrap();

        let out = secret_set(&home, "gateway", input);

        assert_eq!(
            out.status.code(),
            Some(1),
            "input {input:?}: {:?}",
            out.status
        );
        assert_eq!(held(&home, "gateway"), None);
    }
}

#[test]
fn secret_set_with_any_other_name_exits_1_and_stores_nothing() {
    for name in ["machine", "machine-keys", "other"] {
        let home = tempfile::tempdir().unwrap();

        let out = secret_set(&home, name, "x\n");

        assert_eq!(out.status.code(), Some(1), "{name}: {:?}", out.status);
        assert_eq!(held(&home, name), None, "{name} was stored");
    }
}
