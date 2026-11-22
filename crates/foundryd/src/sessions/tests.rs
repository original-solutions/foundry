//! Sessions against a real holder, the fake agent and `curl`, with the hook endpoint served on
//! a loopback port.

use std::io::Write as _;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Path as UrlPath, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use bytes::Bytes;
use foundry_holder::protocol::Request;
use foundry_proto::ids::{MachineId, SessionId, WorkspaceId};
use foundry_proto::record::{AnyRecord, Record, Session};
use foundry_proto::settings::Machine;
use foundry_proto::state::SessionState;
use futures::future::BoxFuture;
use tempfile::TempDir;

use super::*;
use crate::store::Store;
use crate::sync::{Conn, Dialer, Peers};
use crate::terminals::Output;
use crate::test_log::Logged;

const WAIT: Duration = Duration::from_secs(20);
const MODEL: &str = "test-model";

/// `cargo test --workspace` builds the holder and the fake agent beside the test binary's
/// `deps` folder.
fn built(name: &str) -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let path = exe.parent().and_then(Path::parent).unwrap().join(name);
    assert!(
        path.exists(),
        "{} is not built: run `cargo test --workspace`",
        path.display()
    );
    path
}

fn holder() -> PathBuf {
    built("foundry-holder")
}

fn fake_agent() -> PathBuf {
    built("foundry-fake-agent")
}

/// One session's worth of daemon: a data directory, a worktree, a Claude Code state file, the
/// store, the terminals, the sessions and the hook endpoint served on a loopback port.
/// Dropping it closes every holder left under the data directory.
struct Fixture {
    data: TempDir,
    worktree: TempDir,
    home: TempDir,
    me: MachineId,
    key: String,
    store: Arc<Mutex<Store>>,
    terminals: Arc<Terminals>,
    sessions: Sessions,
    port: u16,
    server: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn new(script: &str) -> Self {
        Self::with_prefix(script, "fs").await
    }

    /// `prefix` starts the data directory's name. A Unix socket path is at most about 100
    /// bytes, which the system's own temporary folder on macOS would take most of, so the
    /// folder is made under `/tmp`.
    async fn with_prefix(script: &str, prefix: &str) -> Self {
        let data = tempfile::Builder::new()
            .prefix(prefix)
            .tempdir_in("/tmp")
            .unwrap();
        let script_path = data.path().join("script.yaml");
        std::fs::write(&script_path, script).unwrap();
        let me = MachineId::random();
        let store = Arc::new(Mutex::new(
            Store::open(data.path().join("foundry.db")).unwrap(),
        ));
        let terminals = Arc::new(Terminals::new(me, store.clone(), data.path(), holder()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let home = tempfile::tempdir().unwrap();
        let key = format!("gw-key-{}", uuid::Uuid::new_v4());
        let config = Config {
            claude: vec![
                fake_agent().to_string_lossy().into_owned(),
                "--script".into(),
                script_path.to_string_lossy().into_owned(),
            ],
            claude_json: home.path().join(".claude.json"),
            hook_port: port,
            gateway: Gateway {
                base_url: "http://127.0.0.1:9/gateway".into(),
                key: key.clone(),
            },
        };
        let sessions = Sessions::new(me, store.clone(), terminals.clone(), data.path(), config);
        let router = sessions.router();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        Fixture {
            data,
            worktree: tempfile::tempdir().unwrap(),
            home,
            me,
            key,
            store,
            terminals,
            sessions,
            port,
            server,
        }
    }

    fn claude_json(&self) -> PathBuf {
        self.home.path().join(".claude.json")
    }

    fn settings_file(&self, id: SessionId) -> PathBuf {
        self.data.path().join("sessions").join(format!("{id}.json"))
    }

    fn curl_file(&self, id: SessionId) -> PathBuf {
        self.data.path().join("sessions").join(format!("{id}.curl"))
    }

    fn url(&self, id: impl std::fmt::Display) -> String {
        format!("http://127.0.0.1:{}/hooks/{id}", self.port)
    }

    async fn start(&self) -> SessionId {
        self.sessions
            .start(WorkspaceId::random(), self.worktree.path(), MODEL)
            .await
            .unwrap()
    }

    fn record(&self, id: SessionId) -> Option<Record<Session>> {
        record(&self.store, self.me, id)
    }

    fn state(&self, id: SessionId) -> Option<SessionState> {
        self.record(id).map(|r| r.body.state)
    }

    async fn until_state(&self, id: SessionId, want: SessionState) {
        until(&format!("session to be {want:?}"), || {
            self.state(id) == Some(want)
        })
        .await;
    }

    /// The session's hook token, as the curl config file hands it to curl.
    fn token(&self, id: SessionId) -> String {
        let config = std::fs::read_to_string(self.curl_file(id)).unwrap();
        let after = config.split("Bearer ").nth(1).unwrap();
        after.split('"').next().unwrap().to_string()
    }

    /// Whether the session has left no file of its own behind.
    fn files_gone(&self, id: SessionId) -> bool {
        !self.settings_file(id).exists() && !self.curl_file(id).exists()
    }

    /// Types a line into the session's terminal.
    async fn enter(&self, id: SessionId) {
        let terminal = self.record(id).unwrap().body.terminal;
        self.terminals
            .input(terminal, Bytes::from_static(b"\n"))
            .await
            .unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
        let Ok(entries) = std::fs::read_dir(self.data.path().join("terminals")) else {
            return;
        };
        for entry in entries.flatten() {
            if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(entry.path()) {
                let _ = stream.write_all(&Request::Close.encode());
            }
        }
    }
}

/// The newest record held for session `id`.
fn record(store: &Mutex<Store>, me: MachineId, id: SessionId) -> Option<Record<Session>> {
    let store = store.lock().unwrap();
    store
        .since(me, 0)
        .unwrap()
        .into_iter()
        .filter_map(|record| match record {
            AnyRecord::Session(r) if r.body.id == id => Some(r),
            _ => None,
        })
        .next_back()
}

/// Every record held for session `id`.
fn records_of(store: &Mutex<Store>, me: MachineId, id: SessionId) -> Vec<Record<Session>> {
    let store = store.lock().unwrap();
    store
        .since(me, 0)
        .unwrap()
        .into_iter()
        .filter_map(|record| match record {
            AnyRecord::Session(r) if r.body.id == id => Some(r),
            _ => None,
        })
        .collect()
}

fn session_records(store: &Mutex<Store>, me: MachineId) -> usize {
    let store = store.lock().unwrap();
    store
        .since(me, 0)
        .unwrap()
        .into_iter()
        .filter(|record| matches!(record, AnyRecord::Session(_)))
        .count()
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Like [`until`] for a paused clock: yields instead of sleeping, so time does not move.
async fn until_paused(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::task::yield_now().await;
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Reads `snapshot` then the stream until `done` holds for what has been seen.
async fn read_until(snapshot: &[u8], stream: &mut Output, done: impl Fn(&str) -> bool) -> String {
    let mut seen = snapshot.to_vec();
    let deadline = Instant::now() + WAIT;
    while !done(&String::from_utf8_lossy(&seen)) {
        let left = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(left, stream.next()).await {
            Ok(Some(Ok(bytes))) => seen.extend_from_slice(&bytes),
            other => panic!(
                "no more output ({:?}); seen: {:?}",
                other.map(|o| o.is_some()),
                String::from_utf8_lossy(&seen)
            ),
        }
    }
    String::from_utf8_lossy(&seen).into_owned()
}

/// POSTs `body` to `url` and gives the status. `token` is sent as the bearer token when given.
async fn post(url: String, token: Option<String>, body: &'static str) -> u16 {
    tokio::task::spawn_blocking(move || {
        let mut request = ureq::post(&url);
        if let Some(token) = token {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        match request.send(body) {
            Ok(response) => response.status().as_u16(),
            Err(ureq::Error::StatusCode(code)) => code,
            Err(other) => panic!("the request failed: {other}"),
        }
    })
    .await
    .unwrap()
}

const WALK: &str = r"
- wait_input
- hook: {event: SessionStart}
- wait_input
- hook: {event: UserPromptSubmit}
- wait_input
- hook: {event: Notification}
- wait_input
- hook: {event: PostToolUse, tool_name: Bash}
- wait_input
- hook: {event: Stop}
- wait_input
- hook: {event: SessionEnd}
- wait_input
- exit: 0
";

#[tokio::test]
async fn every_row_of_the_state_table_is_reached_with_the_fake_agent() {
    let f = Fixture::new(WALK).await;
    let id = f.start().await;
    let token = f.token(id);
    assert_eq!(f.state(id), Some(SessionState::Starting));

    for want in [
        SessionState::Idle,
        SessionState::Working,
        SessionState::WaitingForYou,
        SessionState::Working,
        SessionState::Idle,
        SessionState::Finished,
    ] {
        f.enter(id).await;
        f.until_state(id, want).await;
    }

    // The last line lets the agent exit 0, and the settings file goes once the exit is seen.
    f.enter(id).await;
    until("the exit to be seen", || f.files_gone(id)).await;
    assert_eq!(f.state(id), Some(SessionState::Finished));

    // The session has ended: a late hook with its own token is refused like an unknown one.
    let body = r#"{"hook_event_name":"UserPromptSubmit"}"#;
    assert_eq!(post(f.url(id), Some(token), body).await, 403);
    assert_eq!(f.state(id), Some(SessionState::Finished));
}

#[tokio::test]
async fn a_child_that_exits_non_zero_is_failed() {
    let f = Fixture::new("- exit: 3\n").await;
    let id = f.start().await;

    f.until_state(id, SessionState::Failed).await;

    until("the exit to be seen", || f.files_gone(id)).await;
    assert_eq!(f.state(id), Some(SessionState::Failed));
}

#[tokio::test]
async fn a_session_of_an_owner_that_was_never_heard_from_shows_as_unreachable() {
    struct NoDialer;
    impl Dialer for NoDialer {
        fn dial(&self, _peer: &Machine) -> BoxFuture<'static, std::io::Result<Conn>> {
            Box::pin(async { Err(std::io::Error::other("no route")) })
        }
    }

    let f = Fixture::new("- wait_input\n").await;
    let id = f.start().await;
    let held = f.record(id).unwrap();
    assert_eq!(held.body.state, SessionState::Starting);

    let other = MachineId::random();
    let other_dir = tempfile::tempdir().unwrap();
    let other_store = Arc::new(Mutex::new(
        Store::open(other_dir.path().join("foundry.db")).unwrap(),
    ));
    other_store
        .lock()
        .unwrap()
        .apply(&AnyRecord::Session(held), f.me)
        .unwrap();
    let peers = Peers::new(other, other_store, Arc::new(NoDialer));

    let seen = peers.state().await.unwrap();

    let shown: Vec<_> = seen
        .iter()
        .filter_map(|r| match r {
            AnyRecord::Session(r) if r.body.id == id => Some(r.body.state),
            _ => None,
        })
        .collect();
    assert_eq!(shown, [SessionState::Unreachable]);
}

#[tokio::test]
async fn the_record_names_its_workspace_terminal_and_model_and_the_child_gets_its_settings() {
    let f = Fixture::new("- print_args\n- wait_input\n").await;
    let workspace = WorkspaceId::random();
    let id = f
        .sessions
        .start(workspace, f.worktree.path(), MODEL)
        .await
        .unwrap();

    let held = f.record(id).unwrap();
    assert_eq!(held.body.id, id);
    assert_eq!(held.body.workspace, workspace);
    assert_eq!(held.body.model, MODEL);
    assert!(!held.deleted);

    let (snapshot, mut output) = f.terminals.attach(held.body.terminal).await.unwrap();
    let settings = f.settings_file(id);
    let seen = read_until(&snapshot.bytes, &mut output, |seen| {
        seen.contains(&format!("ARG={}", settings.display())) && seen.contains("ARG=test-model")
    })
    .await;
    assert!(seen.contains("ARG=--settings"), "{seen}");
    assert!(seen.contains("ARG=--model"), "{seen}");

    assert_eq!(
        std::fs::metadata(&settings).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let claude: serde_json::Value =
        serde_json::from_slice(&std::fs::read(f.claude_json()).unwrap()).unwrap();
    let trusted = std::fs::canonicalize(f.worktree.path()).unwrap();
    assert_eq!(
        claude["projects"][trusted.to_str().unwrap()]["hasTrustDialogAccepted"],
        true
    );
}

/// What the fake agent showed in the session's terminal, read until `done` holds for it.
struct Shown {
    text: String,
}

impl Shown {
    async fn until(f: &Fixture, id: SessionId, done: impl Fn(&Shown) -> bool) -> Shown {
        let terminal = f.record(id).unwrap().body.terminal;
        let (snapshot, mut output) = f.terminals.attach(terminal).await.unwrap();
        let text = read_until(&snapshot.bytes, &mut output, |text| {
            done(&Shown {
                text: text.to_owned(),
            })
        })
        .await;
        Shown { text }
    }

    /// The arguments it printed with `print_args`.
    fn args(&self) -> Vec<String> {
        self.text
            .lines()
            .filter_map(|l| l.trim_end().strip_prefix("ARG=").map(str::to_owned))
            .collect()
    }

    /// The prompts it took with `wait_prompt`. A line still being printed is not read yet.
    fn prompts(&self) -> Vec<String> {
        let printed = self.text.rfind('\n').map_or("", |end| &self.text[..end]);
        printed
            .lines()
            // The sign that the agent takes input may come first on the line.
            .filter_map(|l| l.trim_end().split_once("PROMPT=").map(|(_, json)| json))
            .map(|json| serde_json::from_str(json).unwrap())
            .collect()
    }

    /// How many times it turned bracketed paste on: each `wait_prompt` that has begun.
    fn waits(&self) -> usize {
        self.text.matches("\u{1b}[?2004h").count()
    }
}

/// Types `keys` into the session's terminal, as the person would.
async fn type_keys(f: &Fixture, id: SessionId, keys: &'static [u8]) {
    let terminal = f.record(id).unwrap().body.terminal;
    f.terminals
        .input(terminal, Bytes::from_static(keys))
        .await
        .unwrap();
}

async fn start_on(f: &Fixture, prompt: &str) -> SessionId {
    f.sessions
        .start_with(
            WorkspaceId::random(),
            f.worktree.path(),
            MODEL,
            Some(prompt),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn a_first_prompt_is_in_no_argument_and_is_typed_as_one_prompt_once_the_agent_takes_input() {
    let f = Fixture::new(
        "- print_args\n- hook: {event: SessionStart}\n- wait_prompt\n- wait_prompt\n- wait_input\n",
    )
    .await;
    // A leading dash, a quote, a newline, and an escape that would end a paste early and
    // submit the rest as a second prompt.
    let id = start_on(
        &f,
        "-v fix the login,\nthen say \"done\" \u{1b}[201~\r and stop",
    )
    .await;

    let shown = Shown::until(&f, id, |s| s.prompts().len() == 1 && s.waits() == 2).await;
    let args = shown.args();
    assert!(
        args.ends_with(&["--model".to_string(), MODEL.to_string()]),
        "{args:?}"
    );
    assert!(
        !args.iter().any(|a| a.contains("fix the login")),
        "the prompt is in the arguments: {args:?}"
    );
    // The escape is dropped; everything else arrives, newlines kept, as the first input.
    let want = "-v fix the login,\nthen say \"done\" [201~\n and stop";
    assert_eq!(shown.prompts(), [want]);
    // Its hook reports it as any typed prompt.
    until("the prompt's hook", || {
        f.sessions.trail(id).is_some_and(|t| !t.is_empty())
    })
    .await;
    let trail = f.sessions.trail(id).unwrap();
    let texts: Vec<&str> = trail.iter().map(|m| m.text.as_str()).collect();
    assert_eq!(
        texts,
        ["-v fix the login, then say \"done\" [201~ and stop"]
    );

    // A second prompt typed by the person is the second prompt: nothing else was typed.
    type_keys(&f, id, b"next\r").await;
    let shown = Shown::until(&f, id, |s| s.prompts().len() == 2).await;
    assert_eq!(shown.prompts(), [want, "next"]);
}

#[tokio::test]
async fn a_first_prompt_waits_for_the_session_to_start_though_a_screen_before_it_takes_input() {
    // The first input is a screen before the session starts, as the trust prompt is: it turns
    // bracketed paste on before `SessionStart`. The input box draws a moment after the hook.
    let f = Fixture::new(
        "- wait_prompt\n- hook: {event: SessionStart}\n- sleep_ms: 300\n- print: BOX\n- wait_prompt\n- wait_input\n",
    )
    .await;
    let id = start_on(&f, "fix the login").await;
    Shown::until(&f, id, |s| s.waits() == 1).await;
    // Time enough for a daemon that went by the sign alone, even after the grace, to have typed
    // the prompt into it.
    tokio::time::sleep(claude::EARLIER_SIGN_GRACE + Duration::from_millis(500)).await;
    type_keys(&f, id, b"yes\r").await;
    let shown = Shown::until(&f, id, |s| s.prompts().len() == 2).await;
    assert_eq!(shown.prompts(), ["yes", "fix the login"]);
    // The earlier screen's sign does not count for the box: the paste waits for the box's own.
    let drawn = shown.text.find("BOX").unwrap();
    let pasted = shown.text.find("[200~").unwrap();
    assert!(
        drawn < pasted,
        "typed before the box took input: {:?}",
        shown.text
    );
}

#[tokio::test]
async fn a_first_prompt_after_an_earlier_screens_sign_is_typed_a_moment_after_the_session_starts() {
    // The box gives no sign of its own and the agent shows nothing more after `SessionStart`:
    // the earlier screen's sign still counts once the grace is over.
    let f = Fixture::new(
        "- wait_prompt\n- hook: {event: SessionStart}\n- wait_input\n- print: TOOK\n- wait_input\n",
    )
    .await;
    let id = start_on(&f, "fix the login").await;
    Shown::until(&f, id, |s| s.waits() == 1).await;
    type_keys(&f, id, b"yes\r").await;
    let shown = Shown::until(&f, id, |s| s.text.contains("TOOK")).await;
    assert!(shown.text.contains("[200~"), "{:?}", shown.text);
}

#[tokio::test]
async fn a_first_prompt_waits_for_the_agent_to_take_input_though_the_session_started() {
    let f = Fixture::new(
        "- hook: {event: SessionStart}\n- sleep_ms: 500\n- print: DRAWN\n- wait_prompt\n- wait_input\n",
    )
    .await;
    let id = start_on(&f, "fix the login").await;
    let shown = Shown::until(&f, id, |s| s.prompts().len() == 1).await;
    assert_eq!(shown.prompts(), ["fix the login"]);
    // The terminal echoes keys when they arrive, so the paste shows after what came before
    // the agent took input.
    let drawn = shown.text.find("DRAWN").unwrap();
    let pasted = shown.text.find("[200~").unwrap();
    assert!(
        drawn < pasted,
        "typed before the agent took input: {:?}",
        shown.text
    );
}

/// A first prompt whose keys are `claude::typed_prompt("go")`.
fn first_prompt() -> (FirstPrompt, Bytes) {
    (
        FirstPrompt::new(Some("go")),
        Bytes::from(claude::typed_prompt("go")),
    )
}

const SIGN: &[u8] = b"box \x1b[?2004h";

#[test]
fn a_first_prompt_is_not_ready_before_the_session_starts_or_with_no_sign() {
    let now = tokio::time::Instant::now();
    let long = Duration::from_secs(3600);

    // A sign, and no `SessionStart`.
    let (mut first, _) = first_prompt();
    first.read(SIGN, false);
    assert_eq!(first.ready(None, now + long), None);
    assert_eq!(first.grace_ends(None), None);

    // `SessionStart`, and no sign: there is no moment to wait for either.
    let (mut first, _) = first_prompt();
    first.read(b"drawing", true);
    assert_eq!(first.ready(Some(now), now + long), None);
    assert_eq!(first.grace_ends(Some(now)), None);
    assert!(first.waiting());
}

#[test]
fn a_first_prompt_is_ready_at_once_on_a_sign_after_the_start_and_is_given_once() {
    let now = tokio::time::Instant::now();
    let (mut first, keys) = first_prompt();
    // The sign is cut between two reads.
    first.read(b"box \x1b[?20", true);
    assert_eq!(first.ready(Some(now), now), None);
    first.read(b"04h", true);
    // The box's own sign needs no grace.
    assert_eq!(first.grace_ends(Some(now)), None);
    assert_eq!(first.ready(Some(now), now), Some(keys));
    assert!(!first.waiting());
    assert_eq!(first.ready(Some(now), now + Duration::from_secs(60)), None);
}

#[test]
fn a_first_prompt_with_a_sign_from_before_the_start_is_ready_when_the_grace_is_over() {
    let started = tokio::time::Instant::now();
    let over = started + claude::EARLIER_SIGN_GRACE;
    let (mut first, keys) = first_prompt();
    first.read(SIGN, false);
    assert_eq!(first.grace_ends(Some(started)), Some(over));
    assert_eq!(
        first.ready(Some(started), over - Duration::from_millis(1)),
        None
    );
    assert_eq!(first.ready(Some(started), over), Some(keys));
    assert_eq!(first.grace_ends(Some(started)), None);
}

#[test]
fn a_sign_after_the_start_takes_the_place_of_one_from_before_it() {
    let started = tokio::time::Instant::now();
    let (mut first, keys) = first_prompt();
    first.read(SIGN, false);
    first.read(SIGN, true);
    assert_eq!(first.ready(Some(started), started), Some(keys));
}

#[test]
fn a_sign_the_terminal_already_held_on_attach_counts_as_one_from_before_the_start() {
    // The follower was cut off after `SessionStart` and attaches again: the terminal's stored
    // output still holds an earlier screen's sign, and the box has not given its own.
    let started = tokio::time::Instant::now();
    let over = started + claude::EARLIER_SIGN_GRACE;
    let (mut first, keys) = first_prompt();
    first.attached(SIGN);
    assert_eq!(
        first.ready(Some(started), over - Duration::from_millis(1)),
        None
    );
    assert_eq!(first.grace_ends(Some(started)), Some(over));
    assert_eq!(first.ready(Some(started), over), Some(keys));

    // Half a sign read before the cut does not join what the terminal holds after it.
    let (mut first, _) = first_prompt();
    first.read(b"\x1b[?20", true);
    first.attached(b"04h");
    first.read(b"more", true);
    assert_eq!(first.ready(Some(started), over), None);

    // A sign the box gave before the cut still stands.
    let (mut first, keys) = first_prompt();
    first.read(SIGN, true);
    first.attached(SIGN);
    assert_eq!(first.ready(Some(started), started), Some(keys));
}

#[tokio::test]
async fn a_long_first_prompt_arrives_whole_and_is_typed_once() {
    let f =
        Fixture::new("- hook: {event: SessionStart}\n- wait_prompt\n- wait_prompt\n- wait_input\n")
            .await;
    // Past the 800 characters and three lines Claude Code shows as typed. Each line is short
    // enough for the fake agent's terminal, which hands input over a line at a time.
    let lines: Vec<String> = (0..25)
        .map(|n| format!("{n:02}{}", " fix the login é".repeat(5)))
        .collect();
    let want = lines.join("\n");
    assert!(want.chars().count() > 2000);
    let id = start_on(&f, &want).await;

    let shown = Shown::until(&f, id, |s| s.prompts().len() == 1 && s.waits() == 2).await;
    assert_eq!(shown.prompts(), [want.as_str()]);
    type_keys(&f, id, b"next\r").await;
    let shown = Shown::until(&f, id, |s| s.prompts().len() == 2).await;
    assert_eq!(shown.prompts(), [want.as_str(), "next"]);
}

#[tokio::test]
async fn a_blank_first_prompt_is_none_and_nothing_is_typed() {
    let f =
        Fixture::new("- print_args\n- hook: {event: SessionStart}\n- wait_prompt\n- wait_input\n")
            .await;
    let id = start_on(&f, "  \n ").await;
    let shown = Shown::until(&f, id, |s| s.waits() == 1 && !s.args().is_empty()).await;
    let args = shown.args();
    assert!(
        args.ends_with(&["--model".to_string(), MODEL.to_string()]),
        "{args:?}"
    );
    // Time enough for a daemon that kept a blank prompt to have typed it.
    tokio::time::sleep(Duration::from_millis(500)).await;
    type_keys(&f, id, b"mine\r").await;
    let shown = Shown::until(&f, id, |s| !s.prompts().is_empty()).await;
    assert_eq!(shown.prompts(), ["mine"]);
}

#[tokio::test]
async fn a_hook_with_a_wrong_token_or_for_an_unknown_session_is_403_and_changes_nothing() {
    let f = Fixture::new("- wait_input\n").await;
    let id = f.start().await;
    let before = f.record(id).unwrap();
    let body = r#"{"hook_event_name":"SessionStart"}"#;
    let token = f.token(id);

    // No token, a wrong token, a token for another session, an unknown session, a malformed id.
    assert_eq!(post(f.url(id), None, body).await, 403);
    assert_eq!(post(f.url(id), Some("nope".into()), body).await, 403);
    assert_eq!(post(f.url(id), Some(format!("{token}0")), body).await, 403);
    let unknown = SessionId::random();
    assert_eq!(post(f.url(unknown), Some(token.clone()), body).await, 403);
    assert_eq!(
        post(f.url("not-a-uuid"), Some(token.clone()), body).await,
        403
    );

    let after = f.record(id).unwrap();
    assert_eq!(after, before);
    assert!(f.record(unknown).is_none());
}

#[tokio::test]
async fn a_right_token_with_a_body_that_is_not_a_hook_is_400_and_changes_nothing() {
    let f = Fixture::new("- wait_input\n").await;
    let id = f.start().await;
    let before = f.record(id).unwrap();
    let token = f.token(id);

    assert_eq!(post(f.url(id), Some(token.clone()), "not json").await, 400);
    assert_eq!(
        post(f.url(id), Some(token.clone()), r#"{"a":1}"#).await,
        400
    );
    assert_eq!(
        post(f.url(id), Some(token.clone()), r#"{"hook_event_name":7}"#).await,
        400
    );
    assert_eq!(f.record(id).unwrap(), before);

    // The same token with a hook moves the state.
    let body = r#"{"hook_event_name":"SessionStart","session_id":"x"}"#;
    assert_eq!(post(f.url(id), Some(token), body).await, 204);
    f.until_state(id, SessionState::Idle).await;
}

#[tokio::test]
async fn a_hook_from_a_non_loopback_address_is_refused_and_changes_nothing() {
    let f = Fixture::new("- wait_input\n").await;
    let id = f.start().await;
    let before = f.record(id).unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {}", f.token(id))).unwrap(),
    );
    let body = Bytes::from_static(br#"{"hook_event_name":"UserPromptSubmit"}"#);

    let from = |peer: &str| ConnectInfo(peer.parse::<SocketAddr>().unwrap());
    let call = |peer: &str| {
        hooks::hook(
            State(f.sessions.inner.clone()),
            from(peer),
            UrlPath(id.to_string()),
            headers.clone(),
            body.clone(),
        )
    };

    assert_eq!(call("100.64.0.7:40000").await, StatusCode::FORBIDDEN);
    assert_eq!(call("[2001:db8::1]:40000").await, StatusCode::FORBIDDEN);
    assert_eq!(f.record(id).unwrap(), before);

    // The same call from loopback, plain or as an IPv4-mapped IPv6 address, is taken.
    assert_eq!(
        call("[::ffff:127.0.0.1]:40000").await,
        StatusCode::NO_CONTENT
    );
    f.until_state(id, SessionState::Working).await;
}

/// Lets every task that can run, run, without moving the paused clock.
async fn settle() {
    for _ in 0..100 {
        tokio::task::yield_now().await;
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// A hook call from loopback with the session's token, made straight at the handler.
async fn loopback_hook(f: &Fixture, id: SessionId, body: &'static str) -> StatusCode {
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {}", f.token(id))).unwrap(),
    );
    hooks::hook(
        State(f.sessions.inner.clone()),
        ConnectInfo("127.0.0.1:40000".parse::<SocketAddr>().unwrap()),
        UrlPath(id.to_string()),
        headers,
        Bytes::from_static(body.as_bytes()),
    )
    .await
}

#[tokio::test]
async fn ten_minutes_of_silence_since_the_last_hook_or_output_is_stuck_and_one_byte_clears_it() {
    let f = Fixture::new(
        "
- hook: {event: SessionStart}
- hook: {event: UserPromptSubmit}
- wait_input
",
    )
    .await;
    let id = f.start().await;
    f.until_state(id, SessionState::Working).await;
    let terminal = f.record(id).unwrap().body.terminal;
    // Let the agent's own output reach the follower before the clock stops.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let minutes = |m: u64| Duration::from_secs(m * 60);

    tokio::time::pause();
    tokio::time::advance(minutes(9)).await;
    settle().await;
    assert_eq!(f.state(id), Some(SessionState::Working), "9 minutes");

    // A hook restarts the count: 9 more minutes is 18 since the start, and not stuck.
    let body = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash"}"#;
    assert_eq!(loopback_hook(&f, id, body).await, StatusCode::NO_CONTENT);
    settle().await;
    tokio::time::advance(minutes(9)).await;
    settle().await;
    assert_eq!(f.state(id), Some(SessionState::Working), "9 since the hook");

    tokio::time::advance(minutes(1) + Duration::from_secs(1)).await;
    until_paused("the session to be stuck", || {
        f.state(id) == Some(SessionState::Stuck)
    })
    .await;

    // The terminal echoes the typed byte: output, and no hook.
    tokio::time::resume();
    f.terminals
        .input(terminal, Bytes::from_static(b"x"))
        .await
        .unwrap();
    f.until_state(id, SessionState::Working).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    // The count restarts at that byte, and the timer is armed again after the recovery.
    tokio::time::pause();
    settle().await;
    tokio::time::advance(minutes(9)).await;
    settle().await;
    tokio::time::resume();
    f.terminals
        .input(terminal, Bytes::from_static(b"y"))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    tokio::time::pause();
    settle().await;
    tokio::time::advance(minutes(9)).await;
    settle().await;
    assert_eq!(f.state(id), Some(SessionState::Working), "9 since the byte");
    tokio::time::advance(minutes(1) + Duration::from_secs(1)).await;
    until_paused("the session to be stuck again", || {
        f.state(id) == Some(SessionState::Stuck)
    })
    .await;
    tokio::time::resume();
}

/// Makes the next `n` writes of session records fail, as a busy store would.
fn fail_next_writes(f: &Fixture, n: usize) {
    f.sessions
        .inner
        .fail_writes
        .store(n, std::sync::atomic::Ordering::SeqCst);
}

#[tokio::test]
async fn an_exit_whose_record_cannot_be_written_is_written_once_the_store_answers() {
    let f = Fixture::new("- wait_input\n- exit: 0\n").await;
    let id = f.start().await;
    fail_next_writes(&f, 3);

    f.enter(id).await;

    f.until_state(id, SessionState::Finished).await;
    until("its files to go", || f.files_gone(id)).await;
    assert_eq!(
        f.sessions
            .inner
            .fail_writes
            .load(std::sync::atomic::Ordering::SeqCst),
        0,
        "every failure was met and tried past"
    );
}

#[tokio::test]
async fn a_stall_whose_record_cannot_be_written_is_written_once_the_store_answers() {
    let f = Fixture::new(
        "
- hook: {event: SessionStart}
- hook: {event: UserPromptSubmit}
- wait_input
",
    )
    .await;
    let id = f.start().await;
    f.until_state(id, SessionState::Working).await;
    // Let the agent's own output reach the follower before the clock stops.
    tokio::time::sleep(Duration::from_millis(300)).await;

    tokio::time::pause();
    settle().await;
    fail_next_writes(&f, 2);
    tokio::time::advance(STUCK_AFTER + Duration::from_secs(1)).await;
    // Each step moves the clock past the wait before the next try.
    for _ in 0..200 {
        if f.state(id) == Some(SessionState::Stuck) {
            break;
        }
        settle().await;
        tokio::time::advance(Duration::from_secs(1)).await;
    }
    assert_eq!(f.state(id), Some(SessionState::Stuck));
    tokio::time::resume();
}

#[tokio::test]
async fn a_hook_whose_record_cannot_be_written_at_first_is_written_and_answered_204() {
    let f = Fixture::new("- wait_input\n").await;
    let id = f.start().await;
    fail_next_writes(&f, 2);

    let body = r#"{"hook_event_name":"UserPromptSubmit"}"#;
    assert_eq!(post(f.url(id), Some(f.token(id)), body).await, 204);

    assert_eq!(f.state(id), Some(SessionState::Working));
}

#[tokio::test]
async fn a_hook_the_store_never_takes_is_500_before_the_hook_command_gives_up() {
    let f = Fixture::new("- wait_input\n").await;
    let id = f.start().await;
    let before = f.record(id).unwrap();
    fail_next_writes(&f, usize::MAX);

    let body = r#"{"hook_event_name":"UserPromptSubmit"}"#;
    let asked = Instant::now();
    let answer = tokio::time::timeout(WAIT, post(f.url(id), Some(f.token(id)), body)).await;
    assert_eq!(answer.expect("an answer"), 500);

    // The hook command waits 5 seconds for an answer.
    assert!(
        asked.elapsed() < Duration::from_secs(5),
        "{:?}",
        asked.elapsed()
    );
    fail_next_writes(&f, 0);
    assert_eq!(f.record(id).unwrap(), before);
}

#[tokio::test]
async fn a_hook_whose_caller_stops_waiting_mid_write_still_keeps_memory_and_record_alike() {
    let f = Fixture::new("- hook: {event: SessionStart}\n- wait_input\n").await;
    let id = f.start().await;
    f.until_state(id, SessionState::Idle).await;

    // Hold the store, so the hook's write waits on it.
    let (locked_tx, locked_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let store = f.store.clone();
    let holder = std::thread::spawn(move || {
        let _held = store.lock().unwrap();
        locked_tx.send(()).unwrap();
        release_rx.recv().unwrap();
    });
    locked_rx.recv().unwrap();

    // curl gives up and the request is dropped while the write waits.
    let body = r#"{"hook_event_name":"UserPromptSubmit"}"#;
    let gave_up =
        tokio::time::timeout(Duration::from_millis(300), loopback_hook(&f, id, body)).await;
    assert!(
        gave_up.is_err(),
        "the hook was answered while the store was held"
    );
    release_tx.send(()).unwrap();
    holder.join().unwrap();
    f.until_state(id, SessionState::Working).await;

    // The next Stop moves the state on from Working, so it is written.
    let body = r#"{"hook_event_name":"Stop"}"#;
    assert_eq!(loopback_hook(&f, id, body).await, StatusCode::NO_CONTENT);
    assert_eq!(f.state(id), Some(SessionState::Idle));
}

#[tokio::test]
async fn an_exit_that_cannot_be_read_is_tried_again_with_a_growing_wait_and_one_warning() {
    let logs = Logged::everywhere();
    let f = Fixture::new("- wait_input\n- exit: 0\n").await;
    let id = f.start().await;
    let terminal = f.record(id).unwrap().body.terminal;
    f.sessions
        .inner
        .fail_exit_reads
        .store(4, std::sync::atomic::Ordering::SeqCst);

    let entered = Instant::now();
    f.enter(id).await;

    f.until_state(id, SessionState::Finished).await;
    // Four failed reads wait 100, 200, 400 and 800 ms before the next try.
    assert!(
        entered.elapsed() >= Duration::from_millis(1500),
        "{:?}",
        entered.elapsed()
    );
    let logs = logs.text();
    let warnings = logs
        .lines()
        .filter(|l| l.contains("terminal exit not read") && l.contains(&terminal.to_string()))
        .count();
    assert_eq!(warnings, 1, "{logs}");
}

#[tokio::test]
async fn the_gateway_key_is_in_the_childs_environment_and_in_no_log_line_or_file() {
    let logs = Logged::everywhere();
    let f = Fixture::new(
        "
- hook: {event: SessionStart}
- print_env: ANTHROPIC_AUTH_TOKEN
- print_env: ANTHROPIC_BASE_URL
- wait_input
",
    )
    .await;
    let id = f.start().await;
    let held = f.record(id).unwrap();
    let token = f.token(id);
    // The script fires SessionStart first. The store keeps the newest record only, so this is
    // the one record there is: it carries the real terminal, never the nil placeholder.
    let records = records_of(&f.store, f.me, id);
    assert!(!records.is_empty());
    assert!(
        records
            .iter()
            .all(|r| r.body.terminal.0 != uuid::Uuid::nil()),
        "{records:?}"
    );

    let (snapshot, mut output) = f.terminals.attach(held.body.terminal).await.unwrap();
    let want_key = format!("ANTHROPIC_AUTH_TOKEN={}", f.key);
    read_until(&snapshot.bytes, &mut output, |seen| {
        seen.contains(&want_key) && seen.contains("ANTHROPIC_BASE_URL=http://127.0.0.1:9/gateway")
    })
    .await;

    // A refused hook and a failed start, the two ways a key could leak into a log.
    assert_eq!(
        post(
            f.url(id),
            Some("wrong".into()),
            r#"{"hook_event_name":"Stop"}"#
        )
        .await,
        403
    );
    std::fs::write(f.claude_json(), "not json").unwrap();
    assert!(
        f.sessions
            .start(WorkspaceId::random(), f.worktree.path(), MODEL)
            .await
            .is_err()
    );
    tokio::time::sleep(Duration::from_millis(300)).await;

    let logged = logs.text();
    assert!(
        logged.contains("session started") && logged.contains(&id.to_string()),
        "the capture works: {logged}"
    );
    assert!(!logged.contains(&f.key), "the gateway key is logged");
    assert!(!logged.contains(&token), "the hook token is logged");
    let settings = std::fs::read_to_string(f.settings_file(id)).unwrap();
    assert!(
        !settings.contains(&token),
        "the hook token is in the settings file"
    );
    assert!(
        !settings.contains(&f.key),
        "the gateway key is in the settings file"
    );
    let gateway = Gateway {
        base_url: "http://x".into(),
        key: f.key.clone(),
    };
    let shown = format!("{gateway:?}");
    assert!(
        !shown.contains(&f.key) && shown.contains("<redacted>"),
        "{shown}"
    );
}

#[tokio::test]
async fn a_start_with_an_unusable_claude_json_fails_and_leaves_nothing_behind() {
    let f = Fixture::new("- wait_input\n").await;
    let text = r#"{"oauthAccount": "SECRET-VALUE" nope"#;
    std::fs::write(f.claude_json(), text).unwrap();

    let error = f
        .sessions
        .start(WorkspaceId::random(), f.worktree.path(), MODEL)
        .await
        .unwrap_err();

    assert!(
        matches!(error, SessionError::ClaudeState { .. }),
        "{error:?}"
    );
    assert!(!format!("{error} {error:?}").contains("SECRET-VALUE"));
    assert_eq!(std::fs::read_to_string(f.claude_json()).unwrap(), text);
    let sockets = std::fs::read_dir(f.data.path().join("terminals"))
        .map(|dir| dir.flatten().count())
        .unwrap_or(0);
    assert_eq!(sockets, 0, "a terminal was started");
    assert_eq!(session_records(&f.store, f.me), 0);
    let settings = std::fs::read_dir(f.data.path().join("sessions"))
        .map(|dir| dir.flatten().count())
        .unwrap_or(0);
    assert_eq!(settings, 0, "a settings file is left");
}

#[tokio::test]
async fn a_command_that_cannot_start_fails_the_start_and_leaves_no_session_or_settings() {
    let mut f = Fixture::new("- wait_input\n").await;
    // Rebuild the sessions over a command that does not exist.
    let config = Config {
        claude: vec!["/nonexistent/claude".into()],
        claude_json: f.claude_json(),
        hook_port: f.port,
        gateway: Gateway {
            base_url: "http://127.0.0.1:9".into(),
            key: "k".into(),
        },
    };
    f.sessions = Sessions::new(
        f.me,
        f.store.clone(),
        f.terminals.clone(),
        f.data.path(),
        config,
    );

    let error = f
        .sessions
        .start(WorkspaceId::random(), f.worktree.path(), MODEL)
        .await
        .unwrap_err();

    assert!(matches!(error, SessionError::Terminal(_)), "{error:?}");
    assert_eq!(session_records(&f.store, f.me), 0);
    let settings = std::fs::read_dir(f.data.path().join("sessions"))
        .map(|dir| dir.flatten().count())
        .unwrap_or(0);
    assert_eq!(settings, 0, "a settings file is left");
}

#[tokio::test]
async fn a_start_whose_record_cannot_be_written_closes_its_terminal_and_leaves_nothing_behind() {
    let f = Fixture::new("- wait_input\n").await;
    fail_next_writes(&f, 1);

    let error = f
        .sessions
        .start(WorkspaceId::random(), f.worktree.path(), MODEL)
        .await
        .unwrap_err();

    assert!(matches!(error, SessionError::Store(_)), "{error:?}");
    assert_eq!(session_records(&f.store, f.me), 0);
    let settings = std::fs::read_dir(f.data.path().join("sessions"))
        .map(|dir| dir.flatten().count())
        .unwrap_or(0);
    assert_eq!(settings, 0, "a settings or curl file is left");
    assert!(
        lock(&f.sessions.inner.live).is_empty(),
        "the session is live"
    );
    // The terminal was started, then closed: its holder ends and removes its socket.
    until("the terminal's holder to go", || {
        std::fs::read_dir(f.data.path().join("terminals"))
            .map(|dir| dir.flatten().count())
            .unwrap_or(0)
            == 0
    })
    .await;
}

/// Reads the terminal until `marker` is printed. Under load this reader can fall behind and be
/// cut off by the holder too; it then attaches again, and the new snapshot holds what it missed.
async fn until_printed(f: &Fixture, terminal: TerminalId, marker: &str) {
    let deadline = Instant::now() + WAIT;
    'attach: loop {
        let (snapshot, mut output) = f.terminals.attach(terminal).await.unwrap();
        let mut tail = snapshot.bytes.to_vec();
        loop {
            tail.drain(..tail.len().saturating_sub(64));
            if String::from_utf8_lossy(&tail).contains(marker) {
                return;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(left, output.next()).await {
                Ok(Some(Ok(bytes))) => tail.extend_from_slice(&bytes),
                Ok(Some(Err(TerminalError::Detached(_)))) => continue 'attach,
                other => panic!("no {marker:?} in the output: {other:?}"),
            }
        }
    }
}

#[tokio::test]
async fn a_hook_applied_after_its_session_left_the_live_map_writes_nothing() {
    // As for a hook that waited on the body lock while the start failed and dropped the
    // session: it must not write a record for a session the caller was told failed.
    let f = Fixture::new("- wait_input\n").await;
    let id = f.start().await;
    let before = f.record(id).unwrap();
    let records = session_records(&f.store, f.me);
    let live = f.sessions.inner.live(id).unwrap();
    lock(&f.sessions.inner.live).remove(&id);

    let state = f
        .sessions
        .inner
        .event(&live, Event::Hook(state::Hook::UserPromptSubmit))
        .await
        .unwrap();

    assert_eq!(state, SessionState::Starting);
    assert_eq!(session_records(&f.store, f.me), records);
    assert_eq!(f.record(id).unwrap(), before);
}

#[tokio::test]
async fn a_follower_the_holder_cuts_off_attaches_again_and_still_sees_the_exit() {
    // More output than the holder keeps for one client that does not read.
    let big = "x".repeat(6 * 1024 * 1024);
    let f = Fixture::new(&format!(
        "- wait_input\n- print: {big}\n- print: AFTER-BIG\n- wait_input\n- exit: 0\n"
    ))
    .await;
    let id = f.start().await;
    let terminal = f.record(id).unwrap().body.terminal;
    let live = f.sessions.inner.live(id).unwrap();

    // While the session's body is held, the follower blocks on the first output it reports and
    // reads no more, so its backlog in the holder passes the limit and the holder cuts it off.
    let held = live.body.lock().await;
    f.enter(id).await;
    until_printed(&f, terminal, "AFTER-BIG").await;
    drop(held);

    f.enter(id).await;

    f.until_state(id, SessionState::Finished).await;
    until("its files to go", || f.files_gone(id)).await;
}

#[tokio::test]
async fn a_clear_does_not_end_the_session() {
    // Claude Code sends SessionEnd with reason "clear", then SessionStart with source "clear",
    // and keeps running.
    let f = Fixture::new(
        "
- hook: {event: SessionStart}
- hook: {event: UserPromptSubmit}
- hook: {event: SessionEnd, reason: clear}
- hook: {event: SessionStart, source: clear}
- hook: {event: UserPromptSubmit}
- print: DONE
- wait_input
",
    )
    .await;
    let id = f.start().await;
    let terminal = f.record(id).unwrap().body.terminal;
    let (snapshot, mut output) = f.terminals.attach(terminal).await.unwrap();

    // Every hook is posted and answered before the agent prints.
    read_until(&snapshot.bytes, &mut output, |seen| seen.contains("DONE")).await;

    assert_eq!(f.state(id), Some(SessionState::Working));
}

#[tokio::test]
async fn a_resume_does_not_end_the_session() {
    // Claude Code's /resume sends SessionEnd with reason "resume", then SessionStart with source
    // "resume", and keeps running.
    let f = Fixture::new(
        "
- hook: {event: SessionStart}
- hook: {event: UserPromptSubmit}
- hook: {event: SessionEnd, reason: resume}
- hook: {event: SessionStart, source: resume}
- hook: {event: UserPromptSubmit}
- print: DONE
- wait_input
",
    )
    .await;
    let id = f.start().await;
    let terminal = f.record(id).unwrap().body.terminal;
    let (snapshot, mut output) = f.terminals.attach(terminal).await.unwrap();

    // Every hook is posted and answered before the agent prints.
    read_until(&snapshot.bytes, &mut output, |seen| seen.contains("DONE")).await;

    assert_eq!(f.state(id), Some(SessionState::Working));
}

#[tokio::test]
async fn a_session_end_that_does_not_continue_still_ends_it() {
    let f = Fixture::new("- wait_input\n").await;
    let id = f.start().await;
    let body = r#"{"hook_event_name":"SessionEnd","reason":"logout"}"#;

    assert_eq!(post(f.url(id), Some(f.token(id)), body).await, 204);

    assert_eq!(f.state(id), Some(SessionState::Finished));
}

#[tokio::test]
async fn closing_the_terminal_ends_the_session_and_removes_its_files() {
    let f = Fixture::new("- wait_input\n").await;
    let id = f.start().await;
    let terminal = f.record(id).unwrap().body.terminal;

    f.terminals.close(terminal).await.unwrap();

    until("the session to end", || {
        matches!(
            f.state(id),
            Some(SessionState::Failed | SessionState::Finished)
        )
    })
    .await;
    assert_eq!(f.state(id), Some(SessionState::Failed));
    until("its files to go", || f.files_gone(id)).await;
}

#[tokio::test]
async fn two_starts_at_once_both_get_their_worktree_trusted() {
    let f = Fixture::new("- wait_input\n").await;
    let other = tempfile::tempdir().unwrap();

    let (a, b) = tokio::join!(
        f.sessions
            .start(WorkspaceId::random(), f.worktree.path(), MODEL),
        f.sessions.start(WorkspaceId::random(), other.path(), MODEL),
    );
    a.unwrap();
    b.unwrap();

    let claude: serde_json::Value =
        serde_json::from_slice(&std::fs::read(f.claude_json()).unwrap()).unwrap();
    for worktree in [f.worktree.path(), other.path()] {
        let key = std::fs::canonicalize(worktree).unwrap();
        assert_eq!(
            claude["projects"][key.to_str().unwrap()]["hasTrustDialogAccepted"],
            true,
            "{key:?}"
        );
    }
}

#[tokio::test]
async fn another_sessions_token_is_refused_and_changes_nothing() {
    let f = Fixture::new("- wait_input\n").await;
    let a = f.start().await;
    let b = f.start().await;
    let (before_a, before_b) = (f.record(a).unwrap(), f.record(b).unwrap());
    let (token_a, token_b) = (f.token(a), f.token(b));
    assert_ne!(token_a, token_b);
    let body = r#"{"hook_event_name":"UserPromptSubmit"}"#;

    assert_eq!(post(f.url(b), Some(token_a), body).await, 403);
    assert_eq!(post(f.url(a), Some(token_b), body).await, 403);

    assert_eq!(f.record(a).unwrap(), before_a);
    assert_eq!(f.record(b).unwrap(), before_b);
}

#[tokio::test]
async fn a_start_the_caller_stops_waiting_for_still_finishes() {
    let f = Fixture::new(
        "
- wait_input
- hook: {event: SessionStart}
- wait_input
",
    )
    .await;
    let settings_dir = f.data.path().join("sessions");

    // The caller gives up as soon as the settings file exists, which is before the terminal.
    tokio::select! {
        biased;
        () = async {
            while std::fs::read_dir(&settings_dir).map_or(0, |d| d.flatten().count()) == 0 {
                tokio::task::yield_now().await;
            }
        } => {}
        started = f.sessions.start(WorkspaceId::random(), f.worktree.path(), MODEL) => {
            panic!("the start finished before the caller gave up: {started:?}");
        }
    }

    until("the session's record", || {
        session_records(&f.store, f.me) > 0
    })
    .await;
    let held = {
        let store = f.store.lock().unwrap();
        store
            .since(f.me, 0)
            .unwrap()
            .into_iter()
            .find_map(|r| match r {
                AnyRecord::Session(r) => Some(r),
                _ => None,
            })
            .unwrap()
    };
    assert!(f.terminals.attach(held.body.terminal).await.is_ok());

    f.enter(held.body.id).await;
    f.until_state(held.body.id, SessionState::Idle).await;
}

#[tokio::test]
async fn hooks_reach_the_daemon_when_the_data_dir_has_a_space_and_a_quote_in_its_path() {
    let f = Fixture::with_prefix("- hook: {event: SessionStart}\n- wait_input\n", "f s'q ").await;
    let id = f.start().await;

    f.until_state(id, SessionState::Idle).await;
}

#[tokio::test]
async fn the_hook_token_is_in_a_private_curl_config_and_not_in_the_settings_or_the_command() {
    let f = Fixture::new("- wait_input\n").await;
    let id = f.start().await;
    let token = f.token(id);

    let config = std::fs::read_to_string(f.curl_file(id)).unwrap();
    assert!(config.contains(&format!("header = \"Authorization: Bearer {token}\"")));
    assert!(config.contains(&format!("url = \"{}\"", f.url(id))));
    assert_eq!(
        std::fs::metadata(f.curl_file(id))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let settings = std::fs::read_to_string(f.settings_file(id)).unwrap();
    assert!(
        !settings.contains(&token) && !settings.contains("Bearer"),
        "{settings}"
    );
    assert!(
        settings.contains(&f.curl_file(id).display().to_string()),
        "{settings}"
    );
}

#[tokio::test]
async fn a_sessions_brief_names_its_first_prompt_and_its_newest_step_with_its_secrets_hidden() {
    let f = Fixture::new("- wait_input\n").await;
    let id = f.start().await;
    let other = f.start().await;
    assert_eq!(f.sessions.briefs().get(&id), Some(&brief::Brief::default()));

    let token = f.token(other);
    let prompt = serde_json::json!({
        "hook_event_name": "UserPromptSubmit",
        "prompt": format!("fix it with {} and {token}", f.key),
    });
    let prompt: &'static str = Box::leak(prompt.to_string().into_boxed_str());
    assert_eq!(loopback_hook(&f, id, prompt).await, StatusCode::NO_CONTENT);
    let tool =
        r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"ls"}}"#;
    assert_eq!(loopback_hook(&f, id, tool).await, StatusCode::NO_CONTENT);

    let briefs = f.sessions.briefs();
    let brief = &briefs[&id];
    assert_eq!(brief.title.as_deref(), Some("fix it with ••• and •••"));
    assert_eq!(brief.now.as_ref().map(|s| s.text.as_str()), Some("$ ls"));
    assert_eq!(briefs[&other], brief::Brief::default());
    // The trail is served on its own: the briefs carry none of it.
    assert!(brief.trail.is_empty(), "{:?}", brief.trail);

    let trail = f.sessions.trail(id).expect("a trail for a live session");
    let texts: Vec<&str> = trail.iter().map(|m| m.text.as_str()).collect();
    assert_eq!(texts, ["fix it with ••• and •••", "$ ls"]);
    assert!(trail.iter().all(|m| m.at > 0), "{trail:?}");
    assert_eq!(f.sessions.trail(other), Some(vec![]));
    assert_eq!(f.sessions.trail(SessionId::random()), None);
}

#[tokio::test]
async fn a_refused_hook_leaves_the_brief_alone() {
    let f = Fixture::new("- wait_input\n").await;
    let id = f.start().await;
    let body = r#"{"hook_event_name":"UserPromptSubmit","prompt":"sneaky"}"#;
    assert_eq!(post(f.url(id), Some("wrong".into()), body).await, 403);
    assert_eq!(f.sessions.briefs()[&id], brief::Brief::default());
}
