//! The command line: `--version` works, `join` names a bad port, `secret set` stores a gateway
//! key or a GitHub token without printing it, `github default` stores a login, and
//! `factory export` writes the runner's own run file back byte for byte without writing the
//! store. Every test that runs the binary uses a temporary data directory and skips the copy
//! from the login keychain.

// A release build ignores `FOUNDRY_SECRETS`, so on macOS these runs of the binary would move
// secrets out of the login keychain.
#[cfg(all(target_os = "macos", not(debug_assertions)))]
compile_error!(
    "run these tests in a debug build: a release build on macOS copies secrets out of the login keychain"
);

use std::io::Write as _;
use std::process::{Command, Output, Stdio};

use foundry_proto::factory::{FactoryBody, FactoryRecord, RunEvent, RunId};
use foundry_proto::ids::MachineId;
use foundryd::secrets::{FileSecrets, LOGIN_RULE, Secrets};
use foundryd::store::{Applied, Store};

/// `foundryd` with a data directory and `HOME` of its own and no copy from the keychain, so no
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
/// A refused name exits before stdin is read, so the write can find the pipe closed.
fn secret_set(home: &tempfile::TempDir, name: &str, input: &str) -> Output {
    let mut child = isolated(&["secret", "set", name], home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("foundryd runs");
    match child.stdin.take().unwrap().write_all(input.as_bytes()) {
        Err(error) if error.kind() != std::io::ErrorKind::BrokenPipe => panic!("stdin: {error}"),
        _ => {}
    }
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
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("restart foundryd"), "{name}: {stdout}");
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

#[test]
fn dashboard_with_no_daemon_running_exits_1_and_says_so() {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let home = tempfile::tempdir().unwrap();
    let out = isolated(&["dashboard"], &home)
        .env("FOUNDRY_PORT", port.to_string())
        .output()
        .expect("foundryd runs");
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert_eq!(out.status.code(), Some(1), "exit: {:?}", out.status);
    assert!(
        stderr.contains(&format!("no daemon is running on 127.0.0.1:{port}")),
        "stderr: {stderr}"
    );
}

#[test]
fn github_default_stores_the_login() {
    let home = tempfile::tempdir().unwrap();

    let out = isolated(&["github", "default", "octocat"], &home)
        .output()
        .expect("foundryd runs");

    assert_eq!(out.status.code(), Some(0), "exit: {:?}", out.status);
    assert_eq!(held(&home, "github-default").as_deref(), Some("octocat"));
}

#[test]
fn github_default_with_a_bad_login_exits_1_and_stores_nothing() {
    let home = tempfile::tempdir().unwrap();

    let out = isolated(&["github", "default", "bad/name"], &home)
        .output()
        .expect("foundryd runs");

    assert_eq!(out.status.code(), Some(1), "exit: {:?}", out.status);
    assert_eq!(held(&home, "github-default"), None);
}

#[test]
fn github_default_refusing_an_argument_does_not_print_it() {
    let home = tempfile::tempdir().unwrap();

    let out = isolated(&["github", "default", "ghp_secret_like/x"], &home)
        .output()
        .expect("foundryd runs");

    assert_eq!(out.status.code(), Some(1), "exit: {:?}", out.status);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("ghp_secret_like"), "stderr: {stderr}");
    assert!(stderr.contains("not a GitHub login"), "stderr: {stderr}");
    assert!(words(&out.stderr).contains(LOGIN_RULE), "stderr: {stderr}");
}

#[test]
fn secret_set_stores_a_github_token_and_never_prints_it() {
    let home = tempfile::tempdir().unwrap();

    let out = secret_set(&home, "github:octocat", "ghp_clitoken\n");

    assert_eq!(out.status.code(), Some(0), "exit: {:?}", out.status);
    let printed = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!printed.contains("ghp_clitoken"), "printed: {printed}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("no restart is needed"), "stdout: {stdout}");
    assert_eq!(
        held(&home, "github:octocat").as_deref(),
        Some("ghp_clitoken")
    );
}

#[test]
fn secret_set_with_a_bad_github_login_exits_1_and_stores_nothing() {
    for name in ["github:bad/name", "github:"] {
        let home = tempfile::tempdir().unwrap();

        let out = secret_set(&home, name, "ghp_clitoken\n");

        assert_eq!(out.status.code(), Some(1), "{name}: {:?}", out.status);
        assert_eq!(held(&home, name), None, "{name} was stored");
    }
}

#[test]
fn secret_set_help_names_github_tokens() {
    let out = foundryd(&["secret", "--help"]);
    assert!(out.status.success(), "exit: {:?}", out.status);
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(help.contains("GitHub token"), "help: {help}");

    let out = foundryd(&["secret", "set", "--help"]);
    assert!(out.status.success(), "exit: {:?}", out.status);
    // Help is wrapped to the terminal: compare with its white space made single spaces.
    let help = words(&out.stdout);
    for words in ["gateway-management", "github:<login>", LOGIN_RULE] {
        assert!(help.contains(words), "{words} missing from help: {help}");
    }
}

/// `bytes` as text with every run of white space made one space.
fn words(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The runner's run file the export test compares with: copied byte for byte from the runner's
/// own ledger, as `tests/fixtures/runner/README.md` says.
const RUNNER_RUN: &str = "original-solutions__foundry__179__1";

fn runner_file() -> Vec<u8> {
    std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/runner")
            .join(format!("{RUNNER_RUN}.jsonl")),
    )
    .unwrap()
}

/// Stores each line of the runner's run file as a run event of one machine, at lease
/// generation 1, in the store of the data directory `home`, as the daemon keeps it.
fn store_runner_run(home: &tempfile::TempDir) {
    let run: RunId = RUNNER_RUN.parse().unwrap();
    let owner = MachineId(uuid::Uuid::from_u128(7));
    let mut store = Store::open(home.path().join("store.sqlite")).unwrap();
    let file = runner_file();
    for (n, line) in std::str::from_utf8(&file).unwrap().lines().enumerate() {
        let mut line: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(line).unwrap();
        let record = FactoryRecord {
            owner,
            version: u64::try_from(n).unwrap() + 1,
            body: FactoryBody::RunEvent(RunEvent {
                run: run.clone(),
                generation: 1,
                ts: line["ts"].as_str().unwrap().to_owned(),
                ms: line["ms"].as_u64().unwrap(),
                event: line["event"].as_str().unwrap().to_owned(),
                data: line.remove("data").unwrap(),
                cause: line.remove("cause"),
            }),
        };
        assert_eq!(
            store.apply_factory(&record, owner).unwrap(),
            Applied::Stored
        );
    }
}

#[test]
fn factory_export_writes_the_runners_own_run_file_byte_for_byte() {
    let home = tempfile::tempdir().unwrap();
    store_runner_run(&home);
    let out = tempfile::tempdir().unwrap();

    let ran = isolated(&["factory", "export", out.path().to_str().unwrap()], &home)
        .output()
        .expect("foundryd runs");

    assert!(
        ran.status.success(),
        "exit: {:?} stderr: {}",
        ran.status,
        words(&ran.stderr)
    );
    let exported = std::fs::read(out.path().join("runs").join(format!("{RUNNER_RUN}.jsonl")))
        .expect("the run's file was written");
    assert!(
        exported == runner_file(),
        "exported:\n{}",
        String::from_utf8_lossy(&exported)
    );
    assert!(
        words(&ran.stdout).contains("1 run written"),
        "stdout: {}",
        words(&ran.stdout)
    );
}

#[test]
fn factory_export_on_a_machine_with_no_store_writes_no_run_and_makes_no_store() {
    let home = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();

    let ran = isolated(&["factory", "export", out.path().to_str().unwrap()], &home)
        .output()
        .expect("foundryd runs");

    assert!(
        ran.status.success(),
        "exit: {:?} stderr: {}",
        ran.status,
        words(&ran.stderr)
    );
    assert_eq!(
        std::fs::read_dir(out.path().join("runs")).unwrap().count(),
        0
    );
    assert!(!home.path().join("store.sqlite").exists());
    assert!(
        words(&ran.stdout).contains("0 runs"),
        "stdout: {}",
        words(&ran.stdout)
    );
}

#[test]
fn factory_export_never_writes_the_store_even_one_older_than_this_build() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("store.sqlite");
    // A store that has had none of this build's schema steps, as one an older build left.
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch("CREATE TABLE kept (x INTEGER); INSERT INTO kept VALUES (1);")
        .unwrap();
    let before = std::fs::read(&path).unwrap();
    let out = tempfile::tempdir().unwrap();

    let ran = isolated(&["factory", "export", out.path().to_str().unwrap()], &home)
        .output()
        .expect("foundryd runs");

    assert!(
        ran.status.success(),
        "exit: {:?} stderr: {}",
        ran.status,
        words(&ran.stderr)
    );
    assert!(
        std::fs::read(&path).unwrap() == before,
        "the store was written"
    );
    assert!(
        words(&ran.stdout).contains("0 runs"),
        "stdout: {}",
        words(&ran.stdout)
    );
    let left: Vec<_> = std::fs::read_dir(out.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(left, ["runs"], "left in the export folder: {left:?}");
}

/// The store is copied beside itself, in the data directory only its owner can read, never into
/// the export directory: so an export directory whose name SQLite cannot take still exports.
#[cfg(unix)]
#[test]
fn factory_export_copies_the_store_beside_it_and_not_into_the_export_directory() {
    use std::os::unix::ffi::OsStrExt as _;

    let home = tempfile::tempdir().unwrap();
    store_runner_run(&home);
    let out = tempfile::tempdir().unwrap();
    let dir = out
        .path()
        .join(std::ffi::OsStr::from_bytes(b"not-utf-8-\xff"));
    if std::fs::create_dir(&dir).is_err() {
        // This file system refuses a name that is not UTF-8, so no export directory has one.
        return;
    }

    let ran = isolated(&["factory", "export"], &home)
        .arg(&dir)
        .output()
        .expect("foundryd runs");

    assert!(
        ran.status.success(),
        "exit: {:?} stderr: {}",
        ran.status,
        words(&ran.stderr)
    );
    let exported = std::fs::read(dir.join("runs").join(format!("{RUNNER_RUN}.jsonl"))).unwrap();
    assert!(exported == runner_file());
    let held: Vec<_> = std::fs::read_dir(home.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.contains("sqlite"))
        .collect();
    assert_eq!(held, ["store.sqlite"], "left beside the store: {held:?}");
}

#[test]
fn factory_export_into_a_file_exits_1_and_names_it() {
    let home = tempfile::tempdir().unwrap();
    store_runner_run(&home);
    let out = tempfile::tempdir().unwrap();
    let blocked = out.path().join("blocked");
    std::fs::write(&blocked, "a file").unwrap();

    let ran = isolated(&["factory", "export", blocked.to_str().unwrap()], &home)
        .output()
        .expect("foundryd runs");

    assert_eq!(ran.status.code(), Some(1), "exit: {:?}", ran.status);
    assert!(
        words(&ran.stderr).contains("blocked"),
        "stderr: {}",
        words(&ran.stderr)
    );
}

#[test]
fn secret_set_with_a_token_in_place_of_the_login_never_prints_it() {
    let home = tempfile::tempdir().unwrap();
    let name = "github:ghp_secret_like_x_123";

    let out = secret_set(&home, name, "x\n");

    assert_eq!(out.status.code(), Some(1), "{:?}", out.status);
    assert_eq!(held(&home, name), None, "{name} was stored");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stdout.contains("ghp_secret_like_x_123"),
        "stdout: {stdout}"
    );
    assert!(
        !stderr.contains("ghp_secret_like_x_123"),
        "stderr: {stderr}"
    );
}

#[test]
fn github_default_with_a_40_character_token_exits_1_and_does_not_print_it() {
    let home = tempfile::tempdir().unwrap();
    let token = "0123456789abcdef0123456789abcdef01234567";

    let out = isolated(&["github", "default", token], &home)
        .output()
        .expect("foundryd runs");

    assert_eq!(out.status.code(), Some(1), "exit: {:?}", out.status);
    assert_eq!(held(&home, "github-default"), None);
    let printed = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!printed.contains(token), "printed: {printed}");
}
