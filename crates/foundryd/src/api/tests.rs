//! The API against a real store, holder, fake agent and local bare repo, served on a loopback
//! port and called the way the UI calls it: plain HTTP and WebSockets.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use foundry_proto::ids::{MachineId, ProjectId, SessionId, TerminalId, WorkspaceId};
use foundry_proto::messages::UiMessage;
use foundry_proto::record::AnyRecord;
use foundry_proto::settings::{Machine, Project};
use foundry_proto::state::{SessionState, TerminalState};
use futures::future::BoxFuture;
use futures::{SinkExt as _, StreamExt as _};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio_tungstenite::tungstenite::{self, Message};

use super::*;
use crate::sessions::{self, Gateway, Sessions};
use crate::store::Store;
use crate::sync::{Conn, Dialer, Peers};
use crate::terminals::Terminals;
use crate::workspaces::Workspaces;

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

/// A peer link that never opens: the API reads state, it does not sync.
struct NoDial;

impl Dialer for NoDial {
    fn dial(&self, _: &Machine) -> BoxFuture<'static, std::io::Result<Conn>> {
        Box::pin(async { Err(std::io::Error::other("no peers in these tests")) })
    }
}

/// Runs git in `dir` with an identity of its own.
fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=Foundry Test",
            "-c",
            "user.email=test@foundry.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// One machine's daemon: the store, terminals, workspaces, sessions started with the fake
/// agent, the hook endpoint, and the API served on a loopback port. Dropping it closes every
/// holder left under the data directory.
struct Daemon {
    data: TempDir,
    remotes: TempDir,
    _home: TempDir,
    me: MachineId,
    key: String,
    store: Arc<Mutex<Store>>,
    terminals: Arc<Terminals>,
    addr: SocketAddr,
    servers: Vec<tokio::task::JoinHandle<()>>,
}

impl Daemon {
    /// A daemon whose sessions act out the fake agent `script`.
    async fn new(script: &str) -> Self {
        // A Unix socket path is at most about 100 bytes, which the system's own temporary
        // folder on macOS would take most of.
        let data = tempfile::Builder::new()
            .prefix("fa")
            .tempdir_in("/tmp")
            .unwrap();
        let script_path = data.path().join("script.yaml");
        std::fs::write(&script_path, script).unwrap();
        let me = MachineId::random();
        let store = Arc::new(Mutex::new(
            Store::open(data.path().join("foundry.db")).unwrap(),
        ));
        let terminals = Arc::new(Terminals::new(
            me,
            store.clone(),
            data.path(),
            built("foundry-holder"),
        ));
        let workspaces = Arc::new(Workspaces::new(
            me,
            store.clone(),
            data.path(),
            terminals.clone(),
        ));
        let hooks = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let home = tempfile::tempdir().unwrap();
        let key = format!("gw-key-{}", uuid::Uuid::new_v4());
        let config = sessions::Config {
            claude: vec![
                built("foundry-fake-agent").to_string_lossy().into_owned(),
                "--script".into(),
                script_path.to_string_lossy().into_owned(),
            ],
            claude_json: home.path().join(".claude.json"),
            hook_port: hooks.local_addr().unwrap().port(),
            gateway: Gateway {
                base_url: "http://127.0.0.1:9/gateway".into(),
                key: key.clone(),
            },
        };
        let sessions = Arc::new(Sessions::new(
            me,
            store.clone(),
            terminals.clone(),
            data.path(),
            config,
        ));
        let hook_router = sessions.router();
        let peers = Peers::new(me, store.clone(), Arc::new(NoDial));
        let api = Api {
            store: store.clone(),
            peers,
            terminals: terminals.clone(),
            workspaces,
            sessions,
        };
        let listener = bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let servers = vec![
            tokio::spawn(async move {
                axum::serve(
                    hooks,
                    hook_router.into_make_service_with_connect_info::<SocketAddr>(),
                )
                .await
                .unwrap();
            }),
            tokio::spawn(async move {
                axum::serve(listener, router(api)).await.unwrap();
            }),
        ];
        Daemon {
            data,
            remotes: tempfile::tempdir().unwrap(),
            _home: home,
            me,
            key,
            store,
            terminals,
            addr,
            servers,
        }
    }

    /// A project in the shared settings whose remote, a local bare repo, has one commit on
    /// `main`.
    fn project(&self) -> ProjectId {
        let project = ProjectId::random();
        let root = self.remotes.path().join(project.to_string());
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q", "--bare", "remote.git"]);
        git(&root, &["clone", "-q", "remote.git", "src"]);
        let src = root.join("src");
        std::fs::write(src.join("README"), "demo\n").unwrap();
        git(&src, &["add", "README"]);
        git(&src, &["commit", "-q", "-m", "first"]);
        git(&src, &["push", "-q", "origin", "main"]);
        self.add_project(Project {
            id: project,
            name: "demo".into(),
            repo: root.join("remote.git").to_string_lossy().into_owned(),
        });
        project
    }

    fn add_project(&self, project: Project) {
        let mut store = self.store.lock().unwrap();
        let mut settings = store.settings().unwrap();
        settings.seq += 1;
        settings.projects.push(project);
        assert!(store.apply_settings(settings).unwrap());
    }

    fn host(&self) -> String {
        self.addr.to_string()
    }

    fn ws_url(&self, path: &str) -> String {
        format!("ws://{}{path}", self.addr)
    }

    /// `method path` with `body` as JSON, from a same-site caller with no `Origin`.
    async fn call(&self, method: &str, path: &str, body: Option<serde_json::Value>) -> Reply {
        let host = self.host();
        self.raw(method, path, &[("Host", host.as_str())], body)
            .await
    }

    /// `method path` with exactly `headers`, plus the body's own.
    async fn raw(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<serde_json::Value>,
    ) -> Reply {
        let mut headers = headers.to_vec();
        if body.is_some() {
            headers.push(("Content-Type", "application/json"));
        }
        let body = body.map(|b| b.to_string()).unwrap_or_default();
        self.send(method, path, &headers, &body).await
    }

    /// `method path` with exactly `headers` and `body`, and a Content-Length of the body's.
    async fn send(&self, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Reply {
        let mut stream = tokio::net::TcpStream::connect(self.addr).await.unwrap();
        let mut request = format!("{method} {path} HTTP/1.1\r\nConnection: close\r\n");
        for (name, value) in headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        request.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(WAIT, stream.read_to_end(&mut response))
            .await
            .expect("the API answers in time")
            .unwrap();
        let response = String::from_utf8(response).unwrap();
        let (head, body) = response.split_once("\r\n\r\n").unwrap();
        let status = head.split(' ').nth(1).unwrap().parse().unwrap();
        Reply {
            status,
            body: body.to_owned(),
        }
    }

    async fn create_workspace(&self, project: ProjectId, name: &str) -> WorkspaceId {
        let reply = self
            .call(
                "POST",
                "/api/workspaces",
                Some(serde_json::json!({"project": project, "base": "main", "name": name})),
            )
            .await;
        assert_eq!(reply.status, 201, "{}", reply.body);
        reply.id()
    }

    /// Opens the events socket and reads its first message, the full state.
    async fn events(&self) -> (Socket, Vec<AnyRecord>) {
        let (mut socket, _) = tokio_tungstenite::connect_async(self.ws_url("/api/events"))
            .await
            .unwrap();
        match next_message(&mut socket).await {
            UiMessage::State { records, .. } => (socket, records),
            other => panic!("the first message is not the state: {other:?}"),
        }
    }

    fn records(&self) -> Vec<AnyRecord> {
        self.store.lock().unwrap().since(self.me, 0).unwrap()
    }

    /// The hook token of every session started, read from the curl configs that hold them.
    fn hook_tokens(&self) -> Vec<String> {
        let dir = self.data.path().join("sessions");
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "curl"))
            .map(|e| {
                let text = std::fs::read_to_string(e.path()).unwrap();
                let token = text
                    .split("Bearer ")
                    .nth(1)
                    .expect("the curl config holds a bearer token");
                token.split(['"', '\n']).next().unwrap().to_owned()
            })
            .collect()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        use std::io::Write as _;
        for server in &self.servers {
            server.abort();
        }
        let Ok(entries) = std::fs::read_dir(self.data.path().join("terminals")) else {
            return;
        };
        for entry in entries.flatten() {
            if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(entry.path()) {
                let _ = stream.write_all(&foundry_holder::protocol::Request::Close.encode());
            }
        }
    }
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct Reply {
    status: u16,
    body: String,
}

impl Reply {
    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("{e}: {}", self.body))
    }

    fn id<T: serde::de::DeserializeOwned>(&self) -> T {
        serde_json::from_value(self.json()["id"].clone()).unwrap()
    }
}

/// The next text frame on an events socket, as sent.
async fn next_text(socket: &mut Socket) -> String {
    loop {
        let frame = tokio::time::timeout(WAIT, socket.next())
            .await
            .expect("an event arrives in time")
            .expect("the socket stays open")
            .unwrap();
        if let Message::Text(text) = frame {
            return text.as_str().to_owned();
        }
    }
}

/// The next frame on an events socket, as a UI message.
async fn next_message(socket: &mut Socket) -> UiMessage {
    serde_json::from_str(&next_text(socket).await).unwrap()
}

/// Reads events until `done` says the records seen so far are enough, and returns them all.
async fn until_seen(socket: &mut Socket, done: impl Fn(&[AnyRecord]) -> bool) -> Vec<AnyRecord> {
    let mut seen = Vec::new();
    while !done(&seen) {
        match next_message(socket).await {
            UiMessage::Record { record, .. } => seen.push(record),
            UiMessage::State { records, .. } => seen.extend(records),
            UiMessage::Settings { .. } => {}
        }
    }
    seen
}

fn session_states(records: &[AnyRecord], id: SessionId) -> Vec<SessionState> {
    let mut states: Vec<SessionState> = records
        .iter()
        .filter_map(|r| match r {
            AnyRecord::Session(s) if s.body.id == id => Some(s.body.state),
            _ => None,
        })
        .collect();
    states.dedup();
    states
}

/// A session that reports a prompt, then the turn's end, a second apart, and waits for input.
const TURN: &str = "
- sleep_ms: 1000
- hook: {event: UserPromptSubmit}
- sleep_ms: 1000
- hook: {event: Stop}
- wait_input
";

// --- binding ---

#[test]
fn only_loopback_and_tailscale_addresses_are_allowed() {
    let allowed_ips = [
        "127.0.0.1",
        "127.255.0.9",
        "::1",
        "100.64.0.1",
        "100.127.255.255",
        "fd7a:115c:a1e0::1",
        "fd7a:115c:a1e0:ffff::9",
        "::ffff:127.0.0.1",
        "::ffff:100.100.1.1",
    ];
    for ip in allowed_ips {
        assert!(allowed(ip.parse().unwrap()), "{ip} is refused");
    }
    let refused_ips = [
        "0.0.0.0",
        "::",
        "10.0.0.1",
        "192.168.1.20",
        "100.63.255.255",
        "100.128.0.0",
        "8.8.8.8",
        "fd7a:115c:a1e1::1",
        "fe80::1",
        "::ffff:8.8.8.8",
    ];
    for ip in refused_ips {
        assert!(!allowed(ip.parse().unwrap()), "{ip} is allowed");
    }
}

#[tokio::test]
async fn binding_to_every_address_fails_naming_the_rule() {
    for addr in ["0.0.0.0:0", "[::]:0", "192.168.1.20:0"] {
        let error = bind(addr.parse().unwrap())
            .await
            .expect_err("bound to an address that is not allowed")
            .to_string();
        for part in [
            "loopback",
            "Tailscale",
            "100.64.0.0/10",
            "fd7a:115c:a1e0::/48",
        ] {
            assert!(error.contains(part), "{error:?} does not name {part}");
        }
        let ip = addr.parse::<SocketAddr>().unwrap().ip();
        assert!(
            error.contains(&ip.to_string()),
            "{error:?} does not name {ip}"
        );
    }
}

#[tokio::test]
async fn binding_to_loopback_serves() {
    let listener = bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
    assert_eq!(
        listener.local_addr().unwrap().ip(),
        "127.0.0.1".parse::<IpAddr>().unwrap()
    );
}

// --- callers ---

#[test]
fn a_host_is_trusted_only_when_it_names_this_machine_on_loopback_or_tailscale() {
    for host in [
        "127.0.0.1:7433",
        "[::1]:7433",
        "localhost:7433",
        "LOCALHOST",
        "100.100.1.2:7433",
        "[fd7a:115c:a1e0::5]:7433",
        "trilogy-mac:7433",
        "trilogy-mac.tail1234.ts.net:7433",
        "trilogy-mac.tail1234.ts.net.",
    ] {
        assert!(trusted_host(host), "{host} is refused");
    }
    for host in [
        "evil.example:7433",
        "evil.example",
        "8.8.8.8:7433",
        "[2001:db8::1]:7433",
        "ts.net.evil.example",
        "",
    ] {
        assert!(!trusted_host(host), "{host} is trusted");
    }
}

#[tokio::test]
async fn a_request_from_another_site_is_refused() {
    let daemon = Daemon::new(TURN).await;
    let host = daemon.host();
    let same = format!("http://{host}");

    let reply = daemon
        .raw(
            "GET",
            "/api/state",
            &[("Host", &host), ("Origin", &same)],
            None,
        )
        .await;
    assert_eq!(reply.status, 200, "{}", reply.body);

    for origin in ["http://evil.example", "null", "http://127.0.0.1:1"] {
        let reply = daemon
            .raw(
                "GET",
                "/api/state",
                &[("Host", &host), ("Origin", origin)],
                None,
            )
            .await;
        assert_eq!(reply.status, 403, "Origin {origin}: {}", reply.body);
        assert!(reply.body.contains("site"), "{}", reply.body);
    }
    let project = daemon.project();
    let reply = daemon
        .raw(
            "POST",
            "/api/workspaces",
            &[("Host", &host), ("Origin", "http://evil.example")],
            Some(serde_json::json!({"project": project, "base": "main", "name": "x"})),
        )
        .await;
    assert_eq!(reply.status, 403, "{}", reply.body);
    assert!(
        daemon.records().is_empty(),
        "a refused create made a workspace"
    );
}

#[tokio::test]
async fn a_request_naming_another_host_is_refused() {
    // What a page whose name was rebound to this machine's address sends.
    let daemon = Daemon::new(TURN).await;
    let reply = daemon
        .raw(
            "GET",
            "/api/state",
            &[
                ("Host", "evil.example:7433"),
                ("Origin", "http://evil.example:7433"),
            ],
            None,
        )
        .await;
    assert_eq!(reply.status, 403, "{}", reply.body);
    assert!(reply.body.contains("evil.example"), "{}", reply.body);
    let reply = daemon.raw("GET", "/api/state", &[], None).await;
    assert_eq!(reply.status, 403, "no Host: {}", reply.body);
}

#[tokio::test]
async fn a_socket_opened_from_another_site_is_refused() {
    let daemon = Daemon::new(TURN).await;
    let mut request =
        tungstenite::client::IntoClientRequest::into_client_request(daemon.ws_url("/api/events"))
            .unwrap();
    request
        .headers_mut()
        .insert("Origin", "http://evil.example".parse().unwrap());
    match tokio_tungstenite::connect_async(request).await {
        Err(tungstenite::Error::Http(response)) => assert_eq!(response.status(), 403),
        other => panic!("the socket was not refused: {other:?}"),
    }
}

// --- state and events ---

#[tokio::test]
async fn state_is_every_record_and_the_settings() {
    let daemon = Daemon::new(TURN).await;
    let project = daemon.project();
    let workspace = daemon.create_workspace(project, "fix login").await;

    let reply = daemon.call("GET", "/api/state", None).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    match serde_json::from_str::<UiMessage>(&reply.body).unwrap() {
        UiMessage::State {
            records, settings, ..
        } => {
            assert!(settings.projects.iter().any(|p| p.id == project));
            assert!(records.iter().any(
                |r| matches!(r, AnyRecord::Workspace(w) if w.body.id == workspace && w.owner == daemon.me)
            ));
        }
        other => panic!("not the state: {other:?}"),
    }
}

#[tokio::test]
async fn events_show_a_workspace_appear_and_its_session_change_state() {
    let daemon = Daemon::new(TURN).await;
    let project = daemon.project();
    let (mut socket, first) = daemon.events().await;
    assert!(first.is_empty(), "a new machine holds records: {first:?}");

    let workspace = daemon.create_workspace(project, "fix login").await;
    until_seen(&mut socket, |seen| {
        seen.iter()
            .any(|r| matches!(r, AnyRecord::Workspace(w) if w.body.id == workspace))
    })
    .await;

    let reply = daemon
        .call(
            "POST",
            &format!("/api/workspaces/{workspace}/sessions"),
            Some(serde_json::json!({"model": MODEL})),
        )
        .await;
    assert_eq!(reply.status, 201, "{}", reply.body);
    let session: SessionId = reply.id();
    let seen = until_seen(&mut socket, |seen| {
        session_states(seen, session).ends_with(&[SessionState::Idle])
    })
    .await;
    let states = session_states(&seen, session);
    let working = states.iter().position(|s| *s == SessionState::Working);
    assert!(
        working.is_some_and(|w| states[w..].contains(&SessionState::Idle)),
        "the session went {states:?}, not working then idle"
    );
}

#[tokio::test]
async fn events_send_the_settings_when_they_change() {
    let daemon = Daemon::new(TURN).await;
    let (mut socket, _) = daemon.events().await;
    let project = daemon.project();
    loop {
        if let UiMessage::Settings { settings, .. } = next_message(&mut socket).await {
            assert!(settings.projects.iter().any(|p| p.id == project));
            break;
        }
    }
}

#[tokio::test]
async fn events_send_a_deleted_record() {
    let daemon = Daemon::new(TURN).await;
    let project = daemon.project();
    let workspace = daemon.create_workspace(project, "short lived").await;
    let (mut socket, _) = daemon.events().await;
    let reply = daemon
        .call(
            "POST",
            &format!("/api/workspaces/{workspace}/archive"),
            Some(serde_json::json!({"force": true})),
        )
        .await;
    assert_eq!(reply.status, 204, "{}", reply.body);
    until_seen(&mut socket, |seen| {
        seen.iter()
            .any(|r| matches!(r, AnyRecord::Workspace(w) if w.body.id == workspace && w.deleted))
    })
    .await;
}

// --- terminals ---

async fn cat(daemon: &Daemon) -> TerminalId {
    let cwd = daemon.data.path().to_path_buf();
    daemon
        .terminals
        .start(WorkspaceId::random(), &cwd, &["cat".to_owned()], &[])
        .await
        .unwrap()
}

/// Reads binary frames until their bytes together contain `wanted`.
async fn read_until(socket: &mut Socket, wanted: &str) -> String {
    let mut got = Vec::new();
    while !String::from_utf8_lossy(&got).contains(wanted) {
        let frame = tokio::time::timeout(WAIT, socket.next())
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "{wanted:?} did not arrive; got {:?}",
                    String::from_utf8_lossy(&got)
                )
            })
            .expect("the socket stays open")
            .unwrap();
        if let Message::Binary(bytes) = frame {
            got.extend_from_slice(&bytes);
        }
    }
    String::from_utf8_lossy(&got).into_owned()
}

#[tokio::test]
async fn a_terminal_socket_echoes_typed_input_from_cat() {
    let daemon = Daemon::new(TURN).await;
    let id = cat(&daemon).await;
    let (mut socket, _) =
        tokio_tungstenite::connect_async(daemon.ws_url(&format!("/api/terminals/{id}")))
            .await
            .unwrap();
    socket
        .send(Message::text(r#"{"type":"resize","cols":100,"rows":30}"#))
        .await
        .unwrap();
    socket
        .send(Message::binary(&b"hello foundry\n"[..]))
        .await
        .unwrap();
    // The PTY echoes the line as it is typed, then cat writes it back.
    let text = read_until(&mut socket, "hello foundry").await;
    let text = if text.matches("hello foundry").count() < 2 {
        format!("{text}{}", read_until(&mut socket, "hello foundry").await)
    } else {
        text
    };
    assert!(text.matches("hello foundry").count() >= 2, "{text:?}");
}

#[tokio::test]
async fn a_terminal_socket_starts_with_the_earlier_output() {
    let daemon = Daemon::new(TURN).await;
    let id = cat(&daemon).await;
    daemon
        .terminals
        .input(id, bytes::Bytes::from_static(b"before attach\n"))
        .await
        .unwrap();
    // Until the holder has it, a snapshot may not.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (mut socket, _) =
        tokio_tungstenite::connect_async(daemon.ws_url(&format!("/api/terminals/{id}")))
            .await
            .unwrap();
    let first = tokio::time::timeout(WAIT, socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    match first {
        Message::Binary(bytes) => assert!(
            String::from_utf8_lossy(&bytes).contains("before attach"),
            "the snapshot is {bytes:?}"
        ),
        other => panic!("the first frame is not the snapshot: {other:?}"),
    }
}

#[tokio::test]
async fn a_terminal_socket_closes_once_the_child_exits() {
    let daemon = Daemon::new(TURN).await;
    let id = cat(&daemon).await;
    let (mut socket, _) =
        tokio_tungstenite::connect_async(daemon.ws_url(&format!("/api/terminals/{id}")))
            .await
            .unwrap();
    // Ctrl-D at the start of a line ends cat.
    socket.send(Message::binary(&b"\x04"[..])).await.unwrap();
    loop {
        let frame = tokio::time::timeout(WAIT, socket.next())
            .await
            .expect("the socket closes in time");
        match frame {
            Some(Ok(Message::Close(Some(close)))) => {
                assert_eq!(
                    close.code,
                    tungstenite::protocol::frame::coding::CloseCode::Normal
                );
                break;
            }
            Some(Ok(Message::Close(None))) | None => panic!("closed with no reason"),
            Some(Ok(_)) => {}
            Some(Err(e)) => panic!("{e}"),
        }
    }
}

#[tokio::test]
async fn an_unknown_terminal_socket_is_not_found() {
    let daemon = Daemon::new(TURN).await;
    for id in [TerminalId::random().to_string(), "not-an-id".to_owned()] {
        match tokio_tungstenite::connect_async(daemon.ws_url(&format!("/api/terminals/{id}"))).await
        {
            Err(tungstenite::Error::Http(response)) => assert_eq!(response.status(), 404),
            other => panic!("{id}: the socket was not refused: {other:?}"),
        }
    }
}

// --- actions ---

#[tokio::test]
async fn a_shell_is_started_in_the_worktree_and_closed() {
    let daemon = Daemon::new(TURN).await;
    let project = daemon.project();
    let workspace = daemon.create_workspace(project, "shells").await;

    let reply = daemon
        .call("POST", &format!("/api/workspaces/{workspace}/shells"), None)
        .await;
    assert_eq!(reply.status, 201, "{}", reply.body);
    let shell: TerminalId = reply.id();
    let running = daemon.records().into_iter().any(|r| {
        matches!(r, AnyRecord::Terminal(t)
            if t.body.id == shell && t.body.workspace == workspace
                && t.body.state == TerminalState::Running && !t.deleted)
    });
    assert!(running, "no running terminal {shell} in {workspace}");

    let (mut socket, _) =
        tokio_tungstenite::connect_async(daemon.ws_url(&format!("/api/terminals/{shell}")))
            .await
            .unwrap();
    socket.send(Message::binary(&b"pwd\n"[..])).await.unwrap();
    read_until(&mut socket, "shells").await;

    let reply = daemon
        .call("POST", &format!("/api/terminals/{shell}/close"), None)
        .await;
    assert_eq!(reply.status, 204, "{}", reply.body);
    let deleted = daemon
        .records()
        .into_iter()
        .any(|r| matches!(r, AnyRecord::Terminal(t) if t.body.id == shell && t.deleted));
    assert!(deleted, "the closed terminal's record is not deleted");
}

#[tokio::test]
async fn archive_with_uncommitted_work_is_refused_listing_it() {
    let daemon = Daemon::new(TURN).await;
    let project = daemon.project();
    let workspace = daemon.create_workspace(project, "dirty").await;
    let worktree = daemon
        .data
        .path()
        .join("worktrees")
        .join(project.to_string())
        .join("dirty");
    std::fs::write(worktree.join("notes.txt"), "work\n").unwrap();

    let path = format!("/api/workspaces/{workspace}/archive");
    let reply = daemon.call("POST", &path, None).await;
    assert_eq!(reply.status, 409, "{}", reply.body);
    assert_eq!(
        reply.json()["uncommitted"],
        serde_json::json!(["notes.txt"])
    );
    assert!(worktree.exists());

    let reply = daemon
        .call("POST", &path, Some(serde_json::json!({"force": true})))
        .await;
    assert_eq!(reply.status, 204, "{}", reply.body);
    assert!(!worktree.exists());
}

#[tokio::test]
async fn an_action_on_what_is_not_here_is_not_found() {
    let daemon = Daemon::new(TURN).await;
    let workspace = WorkspaceId::random();
    for (path, body) in [
        (
            format!("/api/workspaces/{workspace}/sessions"),
            Some(serde_json::json!({"model": MODEL})),
        ),
        (format!("/api/workspaces/{workspace}/shells"), None),
        (format!("/api/workspaces/{workspace}/archive"), None),
        (
            format!("/api/terminals/{}/close", TerminalId::random()),
            None,
        ),
    ] {
        let reply = daemon.call("POST", &path, body).await;
        assert_eq!(reply.status, 404, "{path}: {}", reply.body);
        assert!(reply.json()["error"].is_string(), "{path}: {}", reply.body);
    }
    let reply = daemon
        .call(
            "POST",
            "/api/workspaces",
            Some(serde_json::json!({"project": ProjectId::random(), "base": "main", "name": "x"})),
        )
        .await;
    assert_eq!(reply.status, 422, "{}", reply.body);
}

#[tokio::test]
async fn a_name_already_taken_is_a_conflict() {
    let daemon = Daemon::new(TURN).await;
    let project = daemon.project();
    daemon.create_workspace(project, "twice").await;
    let reply = daemon
        .call(
            "POST",
            "/api/workspaces",
            Some(serde_json::json!({"project": project, "base": "main", "name": "twice"})),
        )
        .await;
    assert_eq!(reply.status, 409, "{}", reply.body);
    assert!(reply.json()["error"].as_str().unwrap().contains("twice"));
}

// --- errors ---

#[tokio::test]
async fn every_error_is_json_with_the_status_of_its_cause() {
    let daemon = Daemon::new(TURN).await;
    let project = daemon.project();
    let workspace = daemon.create_workspace(project, "errors").await;
    let host = daemon.host();
    let host = host.as_str();
    let json = ("Content-Type", "application/json");
    let archive = format!("/api/workspaces/{workspace}/archive");
    let cases: Vec<(&str, Reply, u16)> = vec![
        (
            "a missing field",
            daemon
                .send(
                    "POST",
                    "/api/workspaces",
                    &[("Host", host), json],
                    r#"{"base":"main"}"#,
                )
                .await,
            422,
        ),
        (
            "no Content-Type",
            daemon
                .send(
                    "POST",
                    "/api/workspaces",
                    &[("Host", host)],
                    r#"{"base":"main"}"#,
                )
                .await,
            415,
        ),
        (
            "malformed JSON",
            daemon
                .send(
                    "POST",
                    "/api/workspaces",
                    &[("Host", host), json],
                    "{not json",
                )
                .await,
            400,
        ),
        (
            "a session start with no model",
            daemon
                .send(
                    "POST",
                    &format!("/api/workspaces/{workspace}/sessions"),
                    &[("Host", host), json],
                    "{}",
                )
                .await,
            422,
        ),
        (
            "an archive body that is not JSON",
            daemon
                .send("POST", &archive, &[("Host", host), json], "force")
                .await,
            400,
        ),
        (
            "an unknown route",
            daemon
                .send("GET", "/api/nothing", &[("Host", host)], "")
                .await,
            404,
        ),
        (
            "a wrong method",
            daemon
                .send("GET", "/api/workspaces", &[("Host", host)], "")
                .await,
            405,
        ),
        (
            "the events path without an upgrade",
            daemon
                .send("GET", "/api/events", &[("Host", host)], "")
                .await,
            400,
        ),
        (
            "a terminal path without an upgrade",
            daemon
                .send(
                    "GET",
                    &format!("/api/terminals/{}", cat(&daemon).await),
                    &[("Host", host)],
                    "",
                )
                .await,
            400,
        ),
    ];
    for (what, reply, status) in cases {
        assert_eq!(reply.status, status, "{what}: {}", reply.body);
        let body: serde_json::Value = serde_json::from_str(&reply.body)
            .unwrap_or_else(|e| panic!("{what}: the body is not JSON ({e}): {:?}", reply.body));
        assert!(body["error"].is_string(), "{what}: {}", reply.body);
    }
}

#[tokio::test]
async fn an_archive_with_no_body_is_archived_whatever_its_content_type() {
    let daemon = Daemon::new(TURN).await;
    let project = daemon.project();
    let workspace = daemon.create_workspace(project, "bodiless").await;
    let host = daemon.host();
    let reply = daemon
        .send(
            "POST",
            &format!("/api/workspaces/{workspace}/archive"),
            &[
                ("Host", host.as_str()),
                ("Content-Type", "application/json"),
            ],
            "",
        )
        .await;
    assert_eq!(reply.status, 204, "{}", reply.body);
}

#[tokio::test]
async fn a_field_a_request_does_not_know_is_refused() {
    let daemon = Daemon::new(TURN).await;
    let project = daemon.project();
    let reply = daemon
        .call(
            "POST",
            "/api/workspaces",
            Some(serde_json::json!({
                "project": project, "base": "main", "name": "x", "machine": "elsewhere"
            })),
        )
        .await;
    assert_eq!(reply.status, 422, "{}", reply.body);
    assert!(reply.json()["error"].is_string(), "{}", reply.body);
    assert!(
        daemon.records().is_empty(),
        "a refused create made a workspace"
    );
}

// --- secrets ---

#[test]
fn credentials_in_a_url_are_hidden() {
    for (text, secret) in [
        (
            "fatal: unable to access 'https://x-access-token:ghp_s3cret@github.com/o/r.git/'",
            "ghp_s3cret",
        ),
        ("https://ghp_t0ken@github.com/o/r.git", "ghp_t0ken"),
        ("ssh://deploy:hunter2@host/r.git and more", "hunter2"),
    ] {
        let shown = redact(text);
        assert!(!shown.contains(secret), "{shown}");
        assert!(
            shown.contains("github.com") || shown.contains("host/r.git"),
            "{shown}"
        );
    }
    assert_eq!(redact("git@github.com:o/r.git"), "git@github.com:o/r.git");
    assert_eq!(redact("no url here"), "no url here");
}

#[tokio::test]
async fn no_response_contains_a_secret() {
    let daemon = Daemon::new(TURN).await;
    let project = daemon.project();
    let repo_secret = format!("repo-secret-{}", uuid::Uuid::new_v4());
    daemon.add_project(Project {
        id: ProjectId::random(),
        name: "private".into(),
        repo: format!("https://x-access-token:{repo_secret}@example.invalid/o/r.git"),
    });
    let (mut socket, _) = tokio_tungstenite::connect_async(daemon.ws_url("/api/events"))
        .await
        .unwrap();
    let mut said = Vec::new();

    let reply = daemon
        .call(
            "POST",
            "/api/workspaces",
            Some(serde_json::json!({"project": project, "base": "main", "name": "keys"})),
        )
        .await;
    said.push(reply.body.clone());
    let workspace: WorkspaceId = reply.id();
    let reply = daemon
        .call(
            "POST",
            &format!("/api/workspaces/{workspace}/sessions"),
            Some(serde_json::json!({"model": MODEL})),
        )
        .await;
    said.push(reply.body.clone());
    let session: SessionId = reply.id();
    // Read now: the curl configs go when the session ends.
    let tokens = daemon.hook_tokens();
    assert!(!tokens.is_empty(), "no hook token found to check");
    said.push(daemon.call("GET", "/api/state", None).await.body);
    // Every frame the events socket sent, up to the session going idle.
    let mut seen = Vec::new();
    while !session_states(&seen, session).contains(&SessionState::Idle) {
        let text = next_text(&mut socket).await;
        match serde_json::from_str(&text).unwrap() {
            UiMessage::Record { record, .. } => seen.push(record),
            UiMessage::State { records, .. } => seen.extend(records),
            UiMessage::Settings { .. } => {}
        }
        said.push(text);
    }
    for (method, path) in [
        ("POST", format!("/api/workspaces/{workspace}/archive")),
        ("POST", "/api/workspaces".to_owned()),
        ("GET", "/api/nothing".to_owned()),
    ] {
        said.push(daemon.call(method, &path, None).await.body);
    }

    let said = said.join("\n");
    for secret in tokens
        .iter()
        .map(String::as_str)
        .chain([daemon.key.as_str(), repo_secret.as_str()])
    {
        assert!(!said.contains(secret), "a response holds a secret");
    }
}
