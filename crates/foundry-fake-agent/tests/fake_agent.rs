//! The `foundry-fake-agent` binary, run the way the sessions tests run it: a script of steps,
//! and a settings file whose command hooks receive each payload on stdin.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const AGENT: &str = env!("CARGO_BIN_EXE_foundry-fake-agent");

/// A scratch directory, removed on drop. This crate declares no temp-dir dependency.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("fake-agent-{name}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Settings with one command hook for each of `events`, each appending its stdin to `log`
/// followed by a newline, as Claude Code would hand it the payload.
fn settings(dir: &Path, events: &[&str], log: &Path) -> PathBuf {
    let command = format!("cat >> '{0}'; printf '\\n' >> '{0}'", log.display());
    let hooks: serde_json::Map<String, serde_json::Value> = events
        .iter()
        .map(|event| {
            (
                (*event).to_string(),
                serde_json::json!([{"hooks": [{"type": "command", "command": command}]}]),
            )
        })
        .collect();
    let path = dir.join("settings.json");
    std::fs::write(&path, serde_json::json!({ "hooks": hooks }).to_string()).unwrap();
    path
}

fn run(dir: &Scratch, script: &str, settings: &Path, env: &[(&str, &str)]) -> Output {
    let script_path = dir.path().join("script.yaml");
    std::fs::write(&script_path, script).unwrap();
    Command::new(AGENT)
        .args(["--script".as_ref(), script_path.as_os_str()])
        .args(["--settings".as_ref(), settings.as_os_str()])
        .args(["--model", "fake-model"])
        .envs(env.iter().copied())
        .current_dir(dir.path())
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn payloads(log: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn hooks_are_posted_in_order_with_their_extra_fields_then_the_script_exit_code_is_the_exit() {
    let dir = Scratch::new("order");
    let log = dir.path().join("log");
    let settings = settings(dir.path(), &["PreToolUse", "Stop"], &log);
    let script = "
- hook: {event: PreToolUse, tool_name: Bash}
- print: hello from the agent
- hook: {event: Stop}
- exit: 4
";
    let output = run(&dir, script, &settings, &[]);

    assert_eq!(output.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&output.stdout).contains("hello from the agent"));
    let got = payloads(&log);
    assert_eq!(got.len(), 2, "{got:?}");
    assert_eq!(got[0]["hook_event_name"], "PreToolUse");
    assert_eq!(got[0]["tool_name"], "Bash");
    assert!(got[0]["session_id"].is_string());
    assert!(got[0]["cwd"].is_string());
    assert_eq!(got[1]["hook_event_name"], "Stop");
    assert!(got[1].get("tool_name").is_none());
}

#[test]
fn an_event_with_no_registered_hook_posts_nothing_and_the_end_of_the_script_exits_zero() {
    let dir = Scratch::new("none");
    let log = dir.path().join("log");
    let settings = settings(dir.path(), &["Stop"], &log);
    let script = "
- hook: {event: SessionEnd}
- wait_input
";
    let output = run(&dir, script, &settings, &[]);

    assert_eq!(output.status.code(), Some(0));
    assert!(!log.exists(), "something was posted: {:?}", payloads(&log));
}

#[test]
fn a_failing_hook_command_is_reported_on_stderr_and_the_script_carries_on() {
    let dir = Scratch::new("fails");
    let log = dir.path().join("log");
    let path = dir.path().join("settings.json");
    let good = format!("cat >> '{0}'; printf '\\n' >> '{0}'", log.display());
    std::fs::write(
        &path,
        serde_json::json!({"hooks": {
            "PreToolUse": [{"hooks": [{"type": "command", "command": "exit 1"}]}],
            "Stop": [{"hooks": [{"type": "command", "command": good}]}],
        }})
        .to_string(),
    )
    .unwrap();
    let script = "
- hook: {event: PreToolUse}
- hook: {event: Stop}
";
    let output = run(&dir, script, &path, &[]);

    assert_eq!(output.status.code(), Some(0));
    assert!(!output.stderr.is_empty());
    assert_eq!(payloads(&log).len(), 1);
}

#[test]
fn print_env_prints_a_set_variable_and_says_an_unset_one_is_unset() {
    let dir = Scratch::new("env");
    let settings = settings(dir.path(), &[], &dir.path().join("log"));
    let script = "
- print_env: FAKE_AGENT_SET
- print_env: FAKE_AGENT_UNSET_VARIABLE
- print_args
";
    let output = run(&dir, script, &settings, &[("FAKE_AGENT_SET", "value-1")]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("FAKE_AGENT_SET=value-1"), "{stdout}");
    assert!(
        stdout.contains("FAKE_AGENT_UNSET_VARIABLE unset"),
        "{stdout}"
    );
    assert!(stdout.contains("ARG=--model"), "{stdout}");
    assert!(stdout.contains("ARG=fake-model"), "{stdout}");
}

#[test]
fn sleep_ms_waits_at_least_that_long() {
    let dir = Scratch::new("sleep");
    let settings = settings(dir.path(), &[], &dir.path().join("log"));
    let started = std::time::Instant::now();
    let output = run(&dir, "- sleep_ms: 300\n", &settings, &[]);

    assert_eq!(output.status.code(), Some(0));
    assert!(started.elapsed() >= std::time::Duration::from_millis(300));
}
