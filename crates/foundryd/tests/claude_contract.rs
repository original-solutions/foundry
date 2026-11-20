//! The contract with Claude Code that `sessions` rests on, checked against the real thing.
//!
//! One session, started the way `sessions` starts one: the daemon's own settings file and curl
//! config, its trust mark on a folder Claude Code has never seen, and `claude --settings <file>
//! --model <model>` in a PTY with the gateway variables set. The test then runs one shell
//! command from a first prompt typed when and as `sessions` types one, answers a permission
//! with yes, denies one, interrupts a turn with Esc while its tool runs, and exits with
//! `/exit`. It checks that no trust prompt shows, that each hook `sessions`
//! registers arrives with the fields it reads, and that each step posts exactly the hooks the doc
//! lists for it, in order: none after the denial or the Esc. It prints every hook of each step.
//! `docs/claude-code-contract.md` records what it found.
//!
//! It spends model calls, so it is ignored and runs only with `FOUNDRY_CONTRACT=1`, `claude` on
//! `PATH`, and `ANTHROPIC_BASE_URL` and `ANTHROPIC_AUTH_TOKEN` set:
//!
//! ```text
//! FOUNDRY_CONTRACT=1 cargo test -p foundryd --test claude_contract -- --ignored --nocapture
//! ```

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read as _, Write as _};
use std::os::fd::OwnedFd;
use std::path::Path;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header::AUTHORIZATION};
use axum::routing::post;
use bytes::Bytes;
use foundry_holder::pty::Pty;
use foundry_proto::state::SessionState;
use foundryd::sessions::claude;
use foundryd::sessions::state::{Event, Hook, next_state};
use serde_json::Value;

/// The model the session runs, as `sessions` passes one.
const MODEL: &str = "sonnet";
/// How long Claude Code may take to start and post `SessionStart`.
const STARTUP: Duration = Duration::from_secs(60);
/// How long one turn may take to post the hook a step waits for.
const TURN: Duration = Duration::from_secs(180);
/// How long a step that has no hook to wait for watches for hooks after its key press.
const WATCH: Duration = Duration::from_secs(15);

#[test]
#[ignore = "spends model calls: run with FOUNDRY_CONTRACT=1 and --ignored"]
fn claude_code_posts_the_hooks_sessions_reads() {
    if std::env::var("FOUNDRY_CONTRACT").as_deref() != Ok("1") {
        eprintln!("claude_contract: skipped, FOUNDRY_CONTRACT is not 1");
        return;
    }
    let gateway = ["ANTHROPIC_BASE_URL", "ANTHROPIC_AUTH_TOKEN"].map(|name| {
        let value = std::env::var(name).unwrap_or_default();
        assert!(
            !value.is_empty(),
            "{name} must be set for the contract test"
        );
        (name.to_string(), value)
    });
    let version = Command::new("claude")
        .arg("--version")
        .output()
        .expect("`claude` is on PATH");
    eprintln!(
        "claude --version: {}",
        String::from_utf8_lossy(&version.stdout).trim()
    );

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let token = format!("{}", uuid::Uuid::new_v4().simple());
    let posted = Posted::default();
    let port = runtime.block_on(posted.serve(token.clone()));

    // What `Sessions::start` writes: the curl config, the settings, and the trust mark.
    let data = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let folder = folder.path().canonicalize().unwrap();
    let curl = data.path().join("sessions").join("contract.curl");
    let settings = data.path().join("sessions").join("contract.json");
    let url = format!("http://127.0.0.1:{port}/hooks/contract");
    claude::write_curl_config(&curl, &url, &token).unwrap();
    let registered = claude::settings_json(&curl);
    claude::write_settings(&settings, &registered).unwrap();
    let claude_json = claude::claude_json(
        std::env::var_os("CLAUDE_CONFIG_DIR"),
        std::env::var_os("HOME"),
    )
    .expect("CLAUDE_CONFIG_DIR or HOME is set");
    claude::mark_trusted(&claude_json, &folder).unwrap();

    let session = Session::start(&folder, &settings, &gateway);
    let run = Run {
        posted: &posted,
        session: &session,
    };

    // Start: no trust prompt, and `SessionStart`, which the prompt holds back until answered.
    let started = run.within(0, "SessionStart", STARTUP);
    assert!(
        !shows_trust_prompt(&session.screen()),
        "Claude Code showed the trust prompt:\n{}",
        session.tail()
    );
    assert!(
        started.is_some(),
        "no SessionStart within {STARTUP:?}:\n{}",
        session.tail()
    );

    // One shell command, which the default permission mode runs without asking, as the first
    // prompt, typed when `sessions` types one: a paste and Enter the moment Claude Code turns
    // bracketed paste on after `SessionStart`, with no wait for the screen to settle. Its
    // newline stays in the one prompt.
    let until = Instant::now() + STARTUP;
    while !claude::turns_paste_on(&mut Vec::new(), &session.screen()) {
        assert!(
            Instant::now() < until,
            "Claude Code did not turn bracketed paste on:\n{}",
            session.tail()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    run.report("start", 0);
    run.expect("start", 0, &["SessionStart"]);
    let mark = posted.len();
    let first =
        "Run the shell command `echo foundry-contract` with your Bash tool.\nDo nothing else.";
    session.send(&claude::typed_prompt(first));
    run.wait_for(mark, "PostToolUse", TURN);
    run.wait_for(mark, "Stop", TURN);
    run.report("shell command", mark);
    let turn = ["UserPromptSubmit", "PreToolUse", "PostToolUse", "Stop"];
    run.expect("shell command", mark, &turn);
    let submitted = posted.all()[mark..]
        .iter()
        .find(|p| p["hook_event_name"] == "UserPromptSubmit")
        .cloned()
        .unwrap();
    assert_eq!(
        submitted["prompt"], first,
        "the first prompt arrived changed: {submitted}"
    );

    // A write the default permission mode asks about, answered yes.
    let mark = posted.len();
    session.prompt(
        "Create a file named allowed.txt in the current folder containing the word yes, \
         with your Write tool. Do nothing else.",
    );
    let asked = run.wait_for(mark, "Notification", TURN);
    session.answer(b"\r");
    run.wait_for(asked + 1, "PostToolUse", TURN);
    run.wait_for(mark, "Stop", TURN);
    assert!(
        folder.join("allowed.txt").exists(),
        "the allowed write did not run"
    );
    run.report("permission answered yes", mark);
    let answered_turn = [
        "UserPromptSubmit",
        "PreToolUse",
        "Notification",
        "PostToolUse",
        "Stop",
    ];
    run.expect("permission answered yes", mark, &answered_turn);

    // A write the default permission mode asks about, denied.
    let mark = posted.len();
    session.prompt(
        "Create a file named denied.txt in the current folder containing the word no, \
         with your Write tool. Do nothing else.",
    );
    run.wait_for(mark, "Notification", TURN);
    let seen = session.screen().len();
    session.answer(b"3");
    run.watch();
    run.report("permission denied", mark);
    assert!(
        shows(&session.screen()[seen..], "User rejected"),
        "the denial was not taken:\n{}",
        session.tail()
    );
    assert!(!folder.join("denied.txt").exists(), "the denied write ran");
    let asked = ["UserPromptSubmit", "PreToolUse", "Notification"];
    run.expect("permission denied", mark, &asked);

    // A turn interrupted with Esc while its tool runs: the tool's output file shows it runs.
    // Not `sleep`, which Claude Code refuses to run on its own before any hook.
    let mark = posted.len();
    session.prompt(
        "Run the shell command `ping -c 60 127.0.0.1 > ping.txt` with your Bash tool, in the \
         foreground. Do nothing else.",
    );
    // The dialog is answered yes once it is up. Its `Notification` is part of the step's row
    // below, so a Claude Code that stops asking about the command fails the test there.
    let ran = || folder.join("ping.txt").exists();
    let until = Instant::now() + TURN;
    let mut answered = false;
    while !ran() {
        assert!(
            Instant::now() < until,
            "the tool did not start:\n{}",
            session.tail()
        );
        if !answered && posted.find(mark, "Notification").is_some() {
            session.answer(b"\r");
            answered = true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_secs(2));
    let seen = session.screen().len();
    session.send(b"\x1b");
    run.watch();
    run.report("turn interrupted", mark);
    assert!(
        shows(&session.screen()[seen..], "Interrupted"),
        "Esc did not interrupt the tool:\n{}",
        session.tail()
    );
    run.expect("turn interrupted", mark, &asked);

    // Exit.
    let mark = posted.len();
    session.prompt("/exit");
    run.wait_for(mark, "SessionEnd", TURN);
    let code = session.exit_code(Duration::from_secs(30));
    run.report("exit", mark);
    run.expect("exit", mark, &["SessionEnd"]);
    assert_eq!(code, Some(0), "Claude Code exits 0 after /exit");

    // Each hook `sessions` registers arrived, and `sessions` reads it as a hook it acts on.
    let hooks = posted.all();
    for name in registered["hooks"].as_object().unwrap().keys() {
        let payload = hooks
            .iter()
            .find(|p| p["hook_event_name"] == name.as_str())
            .unwrap_or_else(|| panic!("no {name} arrived"));
        let hook = Hook::from_payload(payload);
        assert!(
            hook.is_some_and(|h| h != Hook::Other),
            "sessions does not read {name} as a hook it acts on: {payload}"
        );
    }
    let end = hooks
        .iter()
        .find(|p| p["hook_event_name"] == "SessionEnd")
        .unwrap();
    assert!(
        end["reason"].as_str().is_some_and(|r| r != "clear"),
        "SessionEnd after /exit has a reason that is not `clear`: {end}"
    );
    assert_eq!(
        posted.refused(),
        0,
        "every hook presented the session's token"
    );
    assert_eq!(posted.unread(), 0, "every hook posted JSON");

    // The states `sessions` derives from them, from launch to exit.
    let mut state = SessionState::Starting;
    for payload in &hooks {
        let hook = Hook::from_payload(payload).unwrap();
        state = next_state(state, Event::Hook(hook));
        eprintln!("{hook:?} -> {state:?}");
    }
    assert_eq!(state, SessionState::Finished);
}

/// The hooks the session posted, in the order they arrived, how many were refused, and how many
/// were not JSON.
#[derive(Clone, Default)]
struct Posted {
    hooks: Arc<Mutex<Vec<Value>>>,
    refused: Arc<AtomicUsize>,
    unread: Arc<AtomicUsize>,
}

impl Posted {
    /// Serves `POST /hooks/contract` on a loopback port, taking calls that present `token` as
    /// `sessions` does, and gives the port.
    async fn serve(&self, token: String) -> u16 {
        async fn take(
            State((posted, wanted)): State<(Posted, String)>,
            headers: HeaderMap,
            body: Bytes,
        ) -> StatusCode {
            let presented = headers.get(AUTHORIZATION).map(|v| v.as_bytes());
            if presented != Some(wanted.as_bytes()) {
                posted.refused.fetch_add(1, Ordering::SeqCst);
                return StatusCode::FORBIDDEN;
            }
            let Ok(payload) = serde_json::from_slice::<Value>(&body) else {
                posted.unread.fetch_add(1, Ordering::SeqCst);
                return StatusCode::BAD_REQUEST;
            };
            posted.hooks.lock().unwrap().push(payload);
            StatusCode::NO_CONTENT
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let router = Router::new()
            .route("/hooks/contract", post(take))
            .with_state((self.clone(), format!("Bearer {token}")));
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        port
    }

    fn all(&self) -> Vec<Value> {
        self.hooks.lock().unwrap().clone()
    }

    fn len(&self) -> usize {
        self.hooks.lock().unwrap().len()
    }

    fn refused(&self) -> usize {
        self.refused.load(Ordering::SeqCst)
    }

    fn unread(&self) -> usize {
        self.unread.load(Ordering::SeqCst)
    }

    /// The index of the first hook named `name` at or after `from`.
    fn find(&self, from: usize, name: &str) -> Option<usize> {
        let hooks = self.hooks.lock().unwrap();
        (from..hooks.len()).find(|&i| hooks[i]["hook_event_name"] == name)
    }
}

/// The session and what it posted, for steps that wait on one and report the other.
struct Run<'a> {
    posted: &'a Posted,
    session: &'a Session,
}

impl Run<'_> {
    /// The index of the first hook named `name` at or after `from`, waiting up to `within`.
    fn within(&self, from: usize, name: &str, within: Duration) -> Option<usize> {
        let until = Instant::now() + within;
        loop {
            if let Some(i) = self.posted.find(from, name) {
                return Some(i);
            }
            if Instant::now() >= until {
                return None;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// [`Self::within`], failing the test with what arrived and the screen when it does not.
    fn wait_for(&self, from: usize, name: &str, within: Duration) -> usize {
        self.within(from, name, within).unwrap_or_else(|| {
            panic!(
                "no {name} within {within:?}; hooks since: {:?}\nscreen:\n{}",
                names(&self.posted.all()[from..]),
                self.session.tail()
            )
        })
    }

    /// Waits [`WATCH`] for any hooks a key press brings.
    fn watch(&self) {
        std::thread::sleep(WATCH);
    }

    /// Fails the test unless the hooks from `from` on are exactly `want`, in order: the row of
    /// `step` in `docs/claude-code-contract.md`.
    fn expect(&self, step: &str, from: usize, want: &[&str]) {
        let got = names(&self.posted.all()[from..]);
        assert_eq!(
            got,
            want,
            "{step} posted other hooks than docs/claude-code-contract.md says; update it\n{}",
            self.session.tail()
        );
    }

    /// Prints the hooks that arrived from `from` on, with the fields each had.
    fn report(&self, step: &str, from: usize) {
        let hooks = self.posted.all();
        eprintln!("{step}: {:?}", names(&hooks[from.min(hooks.len())..]));
        for payload in &hooks[from.min(hooks.len())..] {
            let fields: Vec<&String> = payload
                .as_object()
                .map(|o| o.keys().collect())
                .unwrap_or_default();
            eprintln!("  {} {fields:?}", payload["hook_event_name"]);
            for field in [
                "reason",
                "source",
                "notification_type",
                "message",
                "tool_name",
            ] {
                if let Some(value) = payload.get(field) {
                    eprintln!("    {field}: {value}");
                }
            }
        }
    }
}

fn names(hooks: &[Value]) -> Vec<String> {
    hooks
        .iter()
        .map(|p| p["hook_event_name"].as_str().unwrap_or("?").to_string())
        .collect()
}

/// Claude Code in a PTY, with its output kept.
struct Session {
    child: Mutex<Child>,
    master: File,
    _slave: OwnedFd,
    screen: Arc<Mutex<Vec<u8>>>,
    stop: Arc<AtomicBool>,
}

impl Session {
    /// Starts `claude --settings <settings> --model <MODEL>` in `folder` with the gateway
    /// variables, as `sessions` does, with three changes that keep the machine running the test
    /// out of it. Every inherited `CLAUDE*` variable but `CLAUDE_CONFIG_DIR` is unset, such as
    /// those a parent Claude Code session sets. `--setting-sources ""` keeps the user's own
    /// settings (their hooks, permission rules and `env`) from changing what the session asks
    /// and posts. `--permission-mode default` makes Claude Code ask, where the machine's default
    /// mode may answer for itself. `docs/claude-code-contract.md` lists every difference.
    fn start(folder: &Path, settings: &Path, gateway: &[(String, String)]) -> Self {
        let mut argv: Vec<OsString> = vec!["env".into()];
        for (name, _) in std::env::vars_os() {
            let name = name.to_string_lossy();
            if name.starts_with("CLAUDE") && name != "CLAUDE_CONFIG_DIR" {
                argv.extend(["-u".into(), name.as_ref().into()]);
            }
        }
        argv.extend(
            [
                "claude",
                "--setting-sources",
                "",
                "--permission-mode",
                "default",
                "--settings",
                &settings.to_string_lossy(),
                "--model",
                MODEL,
            ]
            .map(OsString::from),
        );
        let pty = Pty::open(120, 40).unwrap();
        let child = pty.spawn(&argv, folder, gateway).unwrap();
        let (master, slave) = pty.into_master().unwrap();
        let screen = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let reader = {
            let (mut master, screen, stop) =
                (master.try_clone().unwrap(), screen.clone(), stop.clone());
            move || {
                let mut buf = [0u8; 8192];
                while !stop.load(Ordering::SeqCst) {
                    match master.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => screen.lock().unwrap().extend_from_slice(&buf[..n]),
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(20));
                        }
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                        Err(_) => break,
                    }
                }
            }
        };
        std::thread::spawn(reader);
        Session {
            child: Mutex::new(child),
            master,
            _slave: slave,
            screen,
            stop,
        }
    }

    fn screen(&self) -> Vec<u8> {
        self.screen.lock().unwrap().clone()
    }

    /// The last of the output, as text, for a failure message.
    fn tail(&self) -> String {
        let screen = self.screen();
        let text = plain(&screen[screen.len().saturating_sub(4000)..]);
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Waits until no output has come for a second, at most 20 seconds.
    fn quiet(&self) {
        let until = Instant::now() + Duration::from_secs(20);
        let mut seen = self.screen.lock().unwrap().len();
        while Instant::now() < until {
            std::thread::sleep(Duration::from_secs(1));
            let now = self.screen.lock().unwrap().len();
            if now == seen {
                return;
            }
            seen = now;
        }
    }

    fn send(&self, bytes: &[u8]) {
        let mut rest = bytes;
        while !rest.is_empty() {
            match (&self.master).write(rest) {
                Ok(n) => rest = &rest[n..],
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => panic!("cannot write to the terminal: {e}"),
            }
        }
    }

    /// Types `text`, then Enter on its own, as a person would: Enter in the same write as the
    /// text is read as part of a paste.
    fn prompt(&self, text: &str) {
        self.quiet();
        self.send(text.as_bytes());
        std::thread::sleep(Duration::from_millis(500));
        self.send(b"\r");
    }

    /// Presses `keys` in the dialog on screen, once it has drawn: the `Notification` that
    /// says it is up arrives before the dialog takes keys.
    fn answer(&self, keys: &[u8]) {
        self.quiet();
        self.send(keys);
    }

    /// The child's exit code, waiting up to `within`.
    fn exit_code(&self, within: Duration) -> Option<i32> {
        let until = Instant::now() + within;
        loop {
            if let Some(status) = self.child.lock().unwrap().try_wait().unwrap() {
                return status.code();
            }
            if Instant::now() >= until {
                return None;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for Session {
    /// Ends Claude Code and everything it started, if it is still running.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let mut child = self.child.lock().unwrap();
        if child.try_wait().ok().flatten().is_none() {
            let group = nix::unistd::Pid::from_raw(child.id() as i32);
            let _ = nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGKILL);
            let _ = child.wait();
        }
    }
}

/// `bytes` with the terminal's escape sequences taken out.
fn plain(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.next() {
            // CSI: parameters, then one final byte from `@` to `~`. A cursor move stands in for
            // the spaces it skips, so it becomes one.
            Some('[') => {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        out.push(' ');
                        break;
                    }
                }
            }
            // OSC: up to BEL or ESC \.
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\x07' || (c == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Whether the output shows `phrase`, in any case. Spaces are left out of both, as the screen
/// draws some with cursor moves.
fn shows(screen: &[u8], phrase: &str) -> bool {
    let squash = |text: &str| -> String {
        text.chars()
            .filter(|c| !c.is_whitespace())
            .flat_map(char::to_lowercase)
            .collect()
    };
    squash(&plain(screen)).contains(&squash(phrase))
}

/// Whether the output shows Claude Code's trust prompt, in its current words or older ones.
fn shows_trust_prompt(screen: &[u8]) -> bool {
    shows(screen, "trust this folder") || shows(screen, "trust the files")
}
