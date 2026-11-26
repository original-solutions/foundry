//! The command line: `--version` works, `join` names a bad port, `secret set` stores a gateway
//! key or a GitHub token without printing it, `github default` stores a login, `factory export`
//! writes the runner's own run file back byte for byte, and `docs/install.md` runs only commands
//! `foundryd --help` lists. Every test that runs the binary uses a temporary data directory and
//! skips the copy from the login keychain.

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

/// Runs `foundryd factory export <dir>` with the data directory `home`.
fn export(home: &tempfile::TempDir, dir: &std::path::Path) -> Output {
    isolated(&["factory", "export"], home)
        .arg(dir)
        .output()
        .expect("foundryd runs")
}

fn assert_exported_runner_run(ran: &Output, dir: &std::path::Path) {
    assert!(
        ran.status.success(),
        "exit: {:?} stderr: {}",
        ran.status,
        words(&ran.stderr)
    );
    let exported = std::fs::read(dir.join("runs").join(format!("{RUNNER_RUN}.jsonl")))
        .expect("the run's file was written");
    assert!(
        exported == runner_file(),
        "exported:\n{}",
        String::from_utf8_lossy(&exported)
    );
}

/// How many schema steps the store at `path` has had.
fn schema_steps(path: &std::path::Path) -> i64 {
    rusqlite::Connection::open(path)
        .unwrap()
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap()
}

/// The export opens the store as the daemon does, so a store an older build left is brought up
/// to this build's schema, rows kept. The store's schema steps only add, so the older build
/// keeps working on it.
#[test]
fn factory_export_opens_a_store_an_older_build_left_as_the_daemon_does_and_keeps_its_rows() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("store.sqlite");
    // A store that has had none of this build's schema steps, as one an older build left.
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch("CREATE TABLE kept (x INTEGER); INSERT INTO kept VALUES (1);")
        .unwrap();
    let current = tempfile::tempdir().unwrap();
    let current = current.path().join("store.sqlite");
    drop(Store::open(&current).unwrap());
    let out = tempfile::tempdir().unwrap();

    let ran = export(&home, out.path());

    assert!(
        ran.status.success(),
        "exit: {:?} stderr: {}",
        ran.status,
        words(&ran.stderr)
    );
    assert!(
        words(&ran.stdout).contains("0 runs"),
        "stdout: {}",
        words(&ran.stdout)
    );
    assert_eq!(schema_steps(&path), schema_steps(&current));
    let kept: i64 = rusqlite::Connection::open(&path)
        .unwrap()
        .query_row("SELECT x FROM kept", [], |row| row.get(0))
        .unwrap();
    assert_eq!(kept, 1);
}

/// A daemon killed in the middle of a write leaves its rollback journal beside the store. The
/// export rolls that write back first, as the daemon's next start would, and reads the store as
/// it stood before it.
#[test]
fn factory_export_reads_a_store_a_killed_daemon_left_in_the_middle_of_a_write() {
    let home = tempfile::tempdir().unwrap();
    store_runner_run(&home);
    let live = home.path().join("store.sqlite");
    let writer = rusqlite::Connection::open(&live).unwrap();
    // The unfinished write removes the run's own events, then writes enough beside them that
    // SQLite moves its changes into the store file before any commit.
    writer
        .execute_batch(
            "PRAGMA cache_size = 2; BEGIN IMMEDIATE; DELETE FROM factory_records;
             CREATE TABLE scratch (x BLOB);",
        )
        .unwrap();
    for _ in 0..200 {
        writer
            .execute("INSERT INTO scratch VALUES (zeroblob(4096))", [])
            .unwrap();
    }
    // The store and its journal as a kill at this moment leaves them.
    let crashed = tempfile::tempdir().unwrap();
    let store = crashed.path().join("store.sqlite");
    std::fs::copy(&live, &store).unwrap();
    std::fs::copy(
        home.path().join("store.sqlite-journal"),
        crashed.path().join("store.sqlite-journal"),
    )
    .unwrap();
    writer.execute_batch("ROLLBACK").unwrap();
    // A read of the store file that skipped the journal would find none of the run's events.
    let skipping = rusqlite::Connection::open_with_flags(
        format!("file:{}?immutable=1", store.display()),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .unwrap();
    let held: i64 = skipping
        .query_row("SELECT COUNT(*) FROM factory_records", [], |row| row.get(0))
        .unwrap();
    drop(skipping);
    assert_eq!(held, 0, "the unfinished write never reached the store file");
    let out = tempfile::tempdir().unwrap();

    let ran = export(&crashed, out.path());

    assert_exported_runner_run(&ran, out.path());
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

/// The commands `foundryd <args> --help` lists, `help` among them; none for a command that has
/// no subcommands.
fn listed_commands(args: &[&str]) -> Vec<String> {
    let mut args = args.to_vec();
    args.push("--help");
    let out = foundryd(&args);
    assert!(out.status.success(), "{args:?}: {:?}", out.status);
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .skip_while(|line| *line != "Commands:")
        .skip(1)
        .take_while(|line| !line.trim().is_empty())
        .filter_map(|line| line.split_whitespace().next().map(str::to_owned))
        .collect()
}

/// The text of every code span and code block of the Markdown `doc`.
fn code_of(doc: &str) -> Vec<&str> {
    doc.split("```")
        .enumerate()
        .flat_map(|(n, part)| {
            if n % 2 == 1 {
                vec![part]
            } else {
                part.split('`').skip(1).step_by(2).collect()
            }
        })
        .collect()
}

/// Each `foundryd` command the code of `doc` runs, as the words after the binary's name: at
/// most two, and none that is a flag. A binary is named `foundryd` or by a path ending in it.
fn named_commands(doc: &str) -> Vec<Vec<String>> {
    let mut named = Vec::new();
    for line in code_of(doc).into_iter().flat_map(str::lines) {
        let words: Vec<&str> = line.split_whitespace().collect();
        for (at, word) in words.iter().enumerate() {
            if *word != "foundryd" && !word.ends_with("/foundryd") {
                continue;
            }
            let command: Vec<String> = words[at + 1..]
                .iter()
                .take(2)
                .take_while(|word| {
                    word.chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                        && !word.starts_with('-')
                })
                .map(|word| (*word).to_owned())
                .collect();
            if !command.is_empty() {
                named.push(command);
            }
        }
    }
    named
}

#[test]
fn install_doc_names_only_commands_foundryd_help_lists() {
    let doc = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/install.md"),
    )
    .unwrap();
    let named = named_commands(&doc);
    // The commands a machine is set up for the factory with, and the export `retro` reads.
    for command in [
        ["secret", "set"],
        ["github", "default"],
        ["factory", "export"],
    ] {
        assert!(
            named.iter().any(|words| words == &command),
            "docs/install.md never runs `foundryd {}`",
            command.join(" ")
        );
    }

    let top = listed_commands(&[]);
    for words in &named {
        let command = &words[0];
        assert!(
            top.contains(command),
            "docs/install.md runs `foundryd {}`, which `foundryd --help` does not list: {top:?}",
            words.join(" ")
        );
        let under = listed_commands(&[command]);
        if let (false, Some(sub)) = (under.is_empty(), words.get(1)) {
            assert!(
                under.contains(sub),
                "docs/install.md runs `foundryd {command} {sub}`, which `foundryd {command} \
                 --help` does not list: {under:?}"
            );
        }
    }
}
