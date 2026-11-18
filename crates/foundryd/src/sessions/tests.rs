//! Sessions against a real holder, the fake agent and `curl`, with the hook endpoint served on
//! a loopback port.

use std::io::Write as _;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Path as UrlPath, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use bytes::Bytes;
use foundry_holder::protocol::Request;
use foundry_proto::ids::{MachineId, SessionId, WorkspaceId};
use foundry_proto::record::{AnyRecord, Record, Session};
use foundry_proto::settings::Machine;
use foundry_proto::state::SessionState;
use futures::StreamExt as _;
use futures::future::BoxFuture;
use tempfile::TempDir;

use super::*;
use crate::store::Store;
use crate::sync::{Conn, Dialer, Peers};
use crate::terminals::Output;

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
        // A Unix socket path is at most about 100 bytes, which the system's own temporary
        // folder on macOS would take most of.
        let data = tempfile::Builder::new()
            .prefix("fs")
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

    /// The session's hook token, as the settings file hands it to Claude Code.
    fn token(&self, id: SessionId) -> String {
        let settings: serde_json::Value =
            serde_json::from_slice(&std::fs::read(self.settings_file(id)).unwrap()).unwrap();
        let command = settings["hooks"]["SessionStart"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        let after = command.split("Bearer ").nth(1).unwrap();
        after.split('\'').next().unwrap().to_string()
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
async fn read_until(
    snapshot: &[u8],
    stream: &mut Output,
    done: impl Fn(&str) -> bool,
) -> String {
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
    until("the exit to be seen", || !f.settings_file(id).exists()).await;
    assert_eq!(f.state(id), Some(SessionState::Finished));
}

#[tokio::test]
async fn a_child_that_exits_non_zero_is_failed() {
    let f = Fixture::new("- exit: 3\n").await;
    let id = f.start().await;

    f.until_state(id, SessionState::Failed).await;

    until("the exit to be seen", || !f.settings_file(id).exists()).await;
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
    assert_eq!(post(f.url("not-a-uuid"), Some(token.clone()), body).await, 403);

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
    assert_eq!(post(f.url(id), Some(token.clone()), r#"{"a":1}"#).await, 400);
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
    assert_eq!(call("[::ffff:127.0.0.1]:40000").await, StatusCode::NO_CONTENT);
    f.until_state(id, SessionState::Working).await;
}

#[tokio::test]
async fn ten_minutes_of_silence_while_working_is_stuck_and_one_byte_of_output_clears_it() {
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

    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(9 * 60)).await;
    for _ in 0..100 {
        tokio::task::yield_now().await;
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(f.state(id), Some(SessionState::Working), "after 9 minutes");

    tokio::time::advance(Duration::from_secs(60 + 1)).await;
    until_paused("the session to be stuck", || {
        f.state(id) == Some(SessionState::Stuck)
    })
    .await;
    tokio::time::resume();

    // The terminal echoes the typed byte: output, and no hook.
    f.terminals
        .input(terminal, Bytes::from_static(b"x"))
        .await
        .unwrap();
    f.until_state(id, SessionState::Working).await;
}

#[tokio::test]
async fn the_gateway_key_is_in_the_childs_environment_and_in_no_log_line_or_file() {
    let logs = logs();
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

    let (snapshot, mut output) = f.terminals.attach(held.body.terminal).await.unwrap();
    let want_key = format!("ANTHROPIC_AUTH_TOKEN={}", f.key);
    read_until(&snapshot.bytes, &mut output, |seen| {
        seen.contains(&want_key) && seen.contains("ANTHROPIC_BASE_URL=http://127.0.0.1:9/gateway")
    })
    .await;

    // A refused hook and a failed start, the two ways a key could leak into a log.
    assert_eq!(
        post(f.url(id), Some("wrong".into()), r#"{"hook_event_name":"Stop"}"#).await,
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

    let logged = String::from_utf8_lossy(&logs.lock().unwrap()).into_owned();
    assert!(
        logged.contains("session started") && logged.contains(&id.to_string()),
        "the capture works: {logged}"
    );
    assert!(!logged.contains(&f.key), "the gateway key is logged");
    assert!(!logged.contains(&token), "the hook token is logged");
    let settings = std::fs::read_to_string(f.settings_file(id)).unwrap();
    assert!(!settings.contains(&f.key), "the gateway key is in the settings file");
    let gateway = Gateway {
        base_url: "http://x".into(),
        key: f.key.clone(),
    };
    let shown = format!("{gateway:?}");
    assert!(!shown.contains(&f.key) && shown.contains("<redacted>"), "{shown}");
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

    assert!(matches!(error, SessionError::ClaudeState { .. }), "{error:?}");
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
    f.sessions = Sessions::new(f.me, f.store.clone(), f.terminals.clone(), f.data.path(), config);

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

/// The text every tracing event is written to, once a test has called [`logs`].
fn logs() -> Arc<Mutex<Vec<u8>>> {
    static LOGS: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();
    struct Writer(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Writer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    LOGS.get_or_init(|| {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let writer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(move || Writer(writer.clone()))
            .finish();
        tracing::subscriber::set_global_default(subscriber)
            .expect("no other test installs a global tracing subscriber");
        buffer
    })
    .clone()
}
