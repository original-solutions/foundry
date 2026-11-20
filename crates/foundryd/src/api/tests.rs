//! The API against a real store, holder, fake agent and local bare repo, served on a loopback
//! port and called the way the UI calls it: plain HTTP and WebSockets.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use foundry_proto::ids::{MachineId, ProjectId, SessionId, TerminalId, WorkspaceId};
use foundry_proto::messages::{PROTOCOL, PeerMessage, UiMessage};
use foundry_proto::record::{AnyRecord, Record, Session};
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
use crate::sync::{Conn, Dialer, OFFLINE_AFTER, Peers};
use crate::terminals::Terminals;
use crate::workspaces::{Dirty, Workspaces};

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
    workspaces: Arc<Workspaces>,
    peers: Peers,
    addr: SocketAddr,
    servers: Vec<tokio::task::JoinHandle<()>>,
}

impl Daemon {
    /// A daemon whose sessions act out the fake agent `script`.
    async fn new(script: &str) -> Self {
        Self::start(script, built("foundry-holder"), &[]).await
    }

    /// A daemon that answers to the DNS names `names` besides `localhost`.
    async fn named(names: &[&str]) -> Self {
        Self::start(TURN, built("foundry-holder"), names).await
    }

    /// A daemon whose terminals start the holder at `holder`.
    async fn start(script: &str, holder: PathBuf, names: &[&str]) -> Self {
        Self::start_with(script, holder, names, None, None).await
    }

    /// A daemon with no gateway, whose session starts here are refused with `said`.
    async fn refusing_sessions(said: &str) -> Self {
        Self::start_with(TURN, built("foundry-holder"), &[], Some(said), None).await
    }

    async fn start_with(
        script: &str,
        holder: PathBuf,
        names: &[&str],
        refused: Option<&str>,
        gh: Option<&Path>,
    ) -> Self {
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
        let terminals = Arc::new(Terminals::new(me, store.clone(), data.path(), holder));
        let mut workspaces = Workspaces::new(me, store.clone(), data.path(), terminals.clone());
        if let Some(gh) = gh {
            workspaces = workspaces.with_gh(gh);
        }
        let workspaces = Arc::new(workspaces);
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
        let mut api = Api::new(
            store.clone(),
            peers.clone(),
            terminals.clone(),
            workspaces.clone(),
            sessions,
            names.iter().map(|n| (*n).to_owned()).collect(),
        );
        api.sessions_refused = refused.map(str::to_owned);
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
            workspaces,
            peers,
            addr,
            servers,
        }
    }

    /// A project in the shared settings whose remote, a local bare repo, has one commit on
    /// `main`.
    fn project(&self) -> ProjectId {
        self.project_with(&[])
    }

    /// [`Self::project`] whose commit also holds `files`, as (path, text).
    fn project_with(&self, files: &[(&str, &str)]) -> ProjectId {
        let project = ProjectId::random();
        let root = self.remotes.path().join(project.to_string());
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q", "--bare", "remote.git"]);
        git(&root, &["clone", "-q", "remote.git", "src"]);
        let src = root.join("src");
        std::fs::write(src.join("README"), "demo\n").unwrap();
        for (path, text) in files {
            let path = src.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, text).unwrap();
        }
        git(&src, &["add", "."]);
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

    /// A machine in the shared settings, so a link from it is accepted.
    fn add_machine(&self) -> MachineId {
        let id = MachineId::random();
        let mut store = self.store.lock().unwrap();
        let mut settings = store.settings().unwrap();
        settings.seq += 1;
        settings.machines.push(Machine {
            id,
            name: "peer".into(),
            address: "100.64.0.9".into(),
        });
        assert!(store.apply_settings(settings).unwrap());
        id
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
fn a_host_is_trusted_only_when_it_names_this_machine() {
    let names = vec![
        "trilogy-mac".to_owned(),
        "trilogy-mac.tail1234.ts.net".to_owned(),
    ];
    for host in [
        "127.0.0.1:7433",
        "[::1]:7433",
        "localhost:7433",
        "LOCALHOST",
        "100.100.1.2:7433",
        "[fd7a:115c:a1e0::5]:7433",
        "trilogy-mac:7433",
        "TRILOGY-MAC.:7433",
        "trilogy-mac.tail1234.ts.net:7433",
        "trilogy-mac.tail1234.ts.net.",
    ] {
        assert!(trusted_host(host, &names), "{host} is refused");
    }
    for host in [
        "evil.example:7433",
        "evil.example",
        "foo:7433",
        "vps.tail1234.ts.net",
        "other.tail1234.ts.net:7433",
        "8.8.8.8:7433",
        "[2001:db8::1]:7433",
        "ts.net.evil.example",
        "",
    ] {
        assert!(!trusted_host(host, &names), "{host} is trusted");
    }
    assert!(trusted_host("localhost", &[]));
    assert!(!trusted_host("trilogy-mac", &[]), "no names are listed");
}

#[tokio::test]
async fn only_a_name_the_machine_lists_is_trusted_in_a_host() {
    let alone = Daemon::new(TURN).await;
    let named = Daemon::named(&["foo", "vps.tail1234.ts.net"]).await;
    for host in ["foo:7433", "vps.tail1234.ts.net"] {
        let origin = format!("http://{host}");
        let reply = alone
            .raw(
                "GET",
                "/api/state",
                &[("Host", host), ("Origin", &origin)],
                None,
            )
            .await;
        assert_eq!(reply.status, 403, "{host}: {}", reply.body);
        let reply = named
            .raw(
                "GET",
                "/api/state",
                &[("Host", host), ("Origin", &origin)],
                None,
            )
            .await;
        assert_eq!(reply.status, 200, "{host}: {}", reply.body);
    }
    let reply = named
        .raw("GET", "/api/state", &[("Host", "FOO.:7433")], None)
        .await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let reply = named
        .raw("GET", "/api/state", &[("Host", "bar:7433")], None)
        .await;
    assert_eq!(reply.status, 403, "{}", reply.body);
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
async fn the_ui_learns_which_machine_serves_it() {
    let daemon = Daemon::new(TURN).await;
    let reply = daemon.call("GET", "/api/machine", None).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let body: serde_json::Value = serde_json::from_str(&reply.body).unwrap();
    assert_eq!(body, serde_json::json!({"id": daemon.me.to_string()}));
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

/// Reads binary frames until `done` says their bytes together are enough, and returns them.
async fn read_while(socket: &mut Socket, wanted: &str, done: impl Fn(&str) -> bool) -> String {
    let mut got = Vec::new();
    while !done(&String::from_utf8_lossy(&got)) {
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

/// Reads binary frames until their bytes together contain `wanted`.
async fn read_until(socket: &mut Socket, wanted: &str) -> String {
    read_while(socket, wanted, |got| got.contains(wanted)).await
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
    let text = read_while(&mut socket, "hello foundry twice", |got| {
        got.matches("hello foundry").count() >= 2
    })
    .await;
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
    // Until the holder has the input, a snapshot may not hold it: attach again until it does.
    let url = daemon.ws_url(&format!("/api/terminals/{id}"));
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let (mut socket, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        let first = tokio::time::timeout(WAIT, socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let Message::Binary(bytes) = first else {
            panic!("the first frame is not the snapshot: {first:?}");
        };
        if String::from_utf8_lossy(&bytes).contains("before attach") {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the snapshot never held the output: {bytes:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
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
async fn a_file_sent_to_a_terminal_is_kept_beside_it_under_a_plain_name_and_its_path_returned() {
    let daemon = Daemon::new(TURN).await;
    let project = daemon.project();
    let workspace = daemon.create_workspace(project, "files").await;
    let reply = daemon
        .call("POST", &format!("/api/workspaces/{workspace}/shells"), None)
        .await;
    let shell: TerminalId = reply.id();
    let host = daemon.host();
    let send = |name: &str, body: &str| {
        let path = format!("/api/terminals/{shell}/files?name={name}");
        let (host, body) = (host.clone(), body.to_owned());
        let daemon = &daemon;
        async move {
            daemon
                .send("POST", &path, &[("Host", host.as_str())], &body)
                .await
        }
    };

    let reply = send("my%20shot%20(1).png", "first").await;
    assert_eq!(reply.status, 201, "{}", reply.body);
    let first = PathBuf::from(reply.json()["path"].as_str().unwrap());
    let files = daemon
        .data
        .path()
        .join("terminals")
        .join(format!("{shell}.files"));
    assert_eq!(first, files.join("my-shot-1.png"));
    assert_eq!(std::fs::read_to_string(&first).unwrap(), "first");
    use std::os::unix::fs::PermissionsExt as _;
    let mode = std::fs::metadata(&files).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o700, "only this user reads what was sent");

    // A clipboard image is always "image.png": a second one never overwrites a path already typed.
    let again = send("my%20shot%20(1).png", "second").await;
    assert_eq!(
        again.json()["path"],
        files.join("my-shot-1-2.png").to_str().unwrap()
    );
    assert_eq!(std::fs::read_to_string(&first).unwrap(), "first");

    // No name, or one that is all path and dots, is still kept.
    let unnamed = send("..%2F..%2F", "x").await;
    assert_eq!(unnamed.json()["path"], files.join("file").to_str().unwrap());

    let unknown = daemon
        .send(
            "POST",
            &format!("/api/terminals/{}/files?name=a.txt", TerminalId::random()),
            &[("Host", host.as_str())],
            "x",
        )
        .await;
    assert_eq!(unknown.status, 404, "{}", unknown.body);

    let big = "x".repeat(super::files::MAX_FILE + 1);
    let refused = send("big.bin", &big).await;
    assert_eq!(refused.status, 413, "{}", refused.body);
    assert!(
        refused.json()["error"].as_str().unwrap().contains("25 MB"),
        "{}",
        refused.body
    );

    let reply = daemon
        .call("POST", &format!("/api/terminals/{shell}/close"), None)
        .await;
    assert_eq!(reply.status, 204, "{}", reply.body);
    assert!(!files.exists(), "the files go with the terminal");
}

#[tokio::test]
async fn archive_with_uncommitted_work_is_refused_listing_it() {
    let daemon = Daemon::new(TURN).await;
    let project = daemon.project();
    let workspace = daemon.create_workspace(project, "dirty").await;
    let worktree = daemon.workspaces.worktree(workspace).await.unwrap();
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
async fn a_start_racing_an_archive_leaves_no_live_terminal_in_the_archived_workspace() {
    let daemon = Daemon::new(TURN).await;
    let project = daemon.project();
    for round in 0..10 {
        let workspace = daemon
            .create_workspace(project, &format!("race {round}"))
            .await;
        let shells = format!("/api/workspaces/{workspace}/shells");
        let sessions = format!("/api/workspaces/{workspace}/sessions");
        let archive = format!("/api/workspaces/{workspace}/archive");
        let model = Some(serde_json::json!({"model": MODEL}));
        let (a, b, c, archived) = tokio::join!(
            daemon.call("POST", &shells, None),
            daemon.call("POST", &shells, None),
            daemon.call("POST", &sessions, model),
            daemon.call("POST", &archive, Some(serde_json::json!({"force": true}))),
        );
        assert_eq!(archived.status, 204, "round {round}: {}", archived.body);
        for reply in [a, b, c] {
            assert!(
                [201, 404].contains(&reply.status),
                "round {round}: {}",
                reply.body
            );
        }
        let live: Vec<_> = daemon
            .records()
            .into_iter()
            .filter_map(|r| match r {
                AnyRecord::Terminal(t)
                    if t.body.workspace == workspace
                        && !t.deleted
                        && t.body.state != TerminalState::Exited =>
                {
                    Some(t.body.id)
                }
                _ => None,
            })
            .collect();
        assert!(
            live.is_empty(),
            "round {round}: terminals {live:?} are live in an archived workspace"
        );
    }
}

#[tokio::test]
async fn a_session_starts_with_a_first_prompt_and_one_that_cannot_be_an_argument_is_refused() {
    let daemon = Daemon::new(TURN).await;
    let project = daemon.project();
    let workspace = daemon.create_workspace(project, "first prompt").await;
    let sessions = format!("/api/workspaces/{workspace}/sessions");

    let reply = daemon
        .call(
            "POST",
            &sessions,
            Some(serde_json::json!({"model": MODEL, "prompt": "fix the login"})),
        )
        .await;
    assert_eq!(reply.status, 201, "{}", reply.body);

    for prompt in ["a\u{0}b".to_string(), "x".repeat(20_001)] {
        let reply = daemon
            .call(
                "POST",
                &sessions,
                Some(serde_json::json!({"model": MODEL, "prompt": prompt})),
            )
            .await;
        assert_eq!(reply.status, 422, "{}", reply.body);
        assert!(reply.json()["error"].is_string(), "{}", reply.body);
    }
    let started = daemon
        .records()
        .into_iter()
        .filter(|r| matches!(r, AnyRecord::Session(s) if s.body.workspace == workspace))
        .count();
    assert_eq!(started, 1, "only the first start made a session");
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
async fn a_workspace_whose_setup_did_not_start_is_still_answered_with_its_id() {
    // Terminals cannot start: there is no holder to run.
    let daemon = Daemon::start(TURN, PathBuf::from("/nonexistent/foundry-holder"), &[]).await;
    let project = daemon.project_with(&[(
        ".dw/config.yml",
        "factory:\n  provision:\n    - echo provisioned\n",
    )]);
    let reply = daemon
        .call(
            "POST",
            "/api/workspaces",
            Some(serde_json::json!({"project": project, "base": "main", "name": "setup"})),
        )
        .await;
    assert_eq!(reply.status, 201, "{}", reply.body);
    assert!(reply.json()["error"].is_string(), "{}", reply.body);
    let workspace: WorkspaceId = reply.id();
    let state = daemon.call("GET", "/api/state", None).await;
    match serde_json::from_str::<UiMessage>(&state.body).unwrap() {
        UiMessage::State { records, .. } => assert!(
            records.iter().any(
                |r| matches!(r, AnyRecord::Workspace(w) if w.body.id == workspace && !w.deleted)
            ),
            "the workspace {workspace} is not in the state"
        ),
        other => panic!("not the state: {other:?}"),
    }
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

/// An archive body that is JSON but not an archive's, a `force` that is not a boolean or a
/// field misspelled, is refused and archives nothing.
#[tokio::test]
async fn an_archive_body_of_the_wrong_shape_is_refused_and_archives_nothing() {
    let daemon = Daemon::new(TURN).await;
    let project = daemon.project();
    let workspace = daemon.create_workspace(project, "shapes").await;
    let worktree = daemon.workspaces.worktree(workspace).await.unwrap();
    for body in [r#"{"force":"yes"}"#, r#"{"forse":true}"#] {
        let reply = daemon
            .call(
                "POST",
                &format!("/api/workspaces/{workspace}/archive"),
                Some(serde_json::from_str(body).unwrap()),
            )
            .await;
        assert_eq!(reply.status, 422, "{body}: {}", reply.body);
        assert!(reply.json()["error"].is_string(), "{body}: {}", reply.body);
        assert!(worktree.exists(), "{body} archived the workspace");
        assert_eq!(
            daemon.workspaces.worktree(workspace).await.unwrap(),
            worktree
        );
    }
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

// --- callers on every route ---

/// The headers a browser sends to open a WebSocket.
const UPGRADE: [(&str, &str); 4] = [
    ("Connection", "Upgrade"),
    ("Upgrade", "websocket"),
    ("Sec-WebSocket-Version", "13"),
    ("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ=="),
];

#[tokio::test]
async fn every_route_refuses_another_site_and_another_host_and_does_nothing() {
    let daemon = Daemon::new(TURN).await;
    let project = daemon.project();
    let workspace = daemon.create_workspace(project, "guarded").await;
    let worktree = daemon.workspaces.worktree(workspace).await.unwrap();
    let live = cat(&daemon).await;
    let records_before = daemon.records();
    let host = daemon.host();
    let same_site = format!("http://{host}");
    let archive = format!("/api/workspaces/{workspace}/archive");
    let sessions = format!("/api/workspaces/{workspace}/sessions");
    let shells = format!("/api/workspaces/{workspace}/shells");
    let socket = format!("/api/terminals/{live}");
    let close = format!("/api/terminals/{live}/close");
    let create = serde_json::json!({"project": project, "base": "main", "name": "intruder"});
    let force = serde_json::json!({"force": true});
    let model = serde_json::json!({"model": MODEL});
    let trail = format!("/api/sessions/{}/trail", SessionId::random());
    let changes = format!("/api/workspaces/{workspace}/changes");
    let shipping = format!("/api/workspaces/{workspace}/pr");
    let commit = format!("/api/workspaces/{workspace}/commits/0000000");
    let files = format!("/api/terminals/{live}/files?name=x.txt");
    let routes: Vec<(&str, &str, Option<serde_json::Value>, bool)> = vec![
        ("GET", "/api/state", None, false),
        ("GET", "/api/events", None, true),
        ("GET", &socket, None, true),
        ("POST", &close, None, false),
        ("POST", &files, Some(serde_json::json!("sent")), false),
        ("POST", "/api/workspaces", Some(create), false),
        ("POST", &archive, Some(force), false),
        ("POST", &sessions, Some(model), false),
        ("POST", &shells, None, false),
        ("GET", "/api/briefs", None, false),
        ("GET", "/api/machine", None, false),
        ("GET", "/api/machines", None, false),
        ("GET", &trail, None, false),
        ("GET", &changes, None, false),
        ("GET", &commit, None, false),
        ("GET", &shipping, None, false),
    ];
    for (method, path, body, upgrade) in routes {
        for (name, value) in [
            ("Origin", "http://evil.example"),
            ("Host", "evil.example:7433"),
        ] {
            let mut headers = vec![("Host", host.as_str()), ("Origin", same_site.as_str())];
            headers.retain(|(n, _)| *n != name);
            headers.push((name, value));
            // A rebound page's Origin is its own name, whatever else is wrong.
            if name == "Host" {
                headers.retain(|(n, _)| *n != "Origin");
            }
            if upgrade {
                headers.extend(UPGRADE);
            }
            let reply = daemon.raw(method, path, &headers, body.clone()).await;
            assert_eq!(
                reply.status, 403,
                "{method} {path} with {name} {value}: {}",
                reply.body
            );
        }
    }
    assert_eq!(
        daemon.records(),
        records_before,
        "a refused request changed a record"
    );
    assert!(worktree.exists(), "a refused archive removed the worktree");
    let sent = daemon
        .data
        .path()
        .join("terminals")
        .join(format!("{live}.files"));
    assert!(!sent.exists(), "a refused file was kept");
    let (mut socket, _) = tokio_tungstenite::connect_async(daemon.ws_url(&socket))
        .await
        .unwrap();
    socket
        .send(Message::binary(&b"still here\n"[..]))
        .await
        .unwrap();
    read_until(&mut socket, "still here").await;
}

// --- briefs ---

#[tokio::test]
async fn briefs_name_each_sessions_first_prompt_and_newest_step_with_url_passwords_hidden() {
    let daemon = Daemon::new(
        "
- hook: {event: UserPromptSubmit, prompt: \"fetch https://me:hunter2@example.com/a\"}
- hook: {event: PreToolUse, tool_name: Bash, tool_input: {command: \"git push https://me:hunter2@example.com/r\"}}
- wait_input
",
    )
    .await;
    let project = daemon.project();
    let workspace = daemon.create_workspace(project, "briefed").await;
    let reply = daemon
        .call(
            "POST",
            &format!("/api/workspaces/{workspace}/sessions"),
            Some(serde_json::json!({"model": MODEL})),
        )
        .await;
    assert_eq!(reply.status, 201, "{}", reply.body);
    let session: SessionId = reply.id();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let brief = loop {
        let reply = daemon.call("GET", "/api/briefs", None).await;
        assert_eq!(reply.status, 200, "{}", reply.body);
        let briefs: serde_json::Value = serde_json::from_str(&reply.body).unwrap();
        let brief = briefs[session.to_string()].clone();
        if brief["now"]["kind"] == "tool" {
            break brief;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no tool step in {briefs}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(
        brief,
        serde_json::json!({
            "title": "fetch https://***@example.com/a",
            "now": {"kind": "tool", "text": "$ git push https://***@example.com/r", "tool": "Bash"},
        })
    );
}

#[tokio::test]
async fn a_brief_carries_what_claude_said_at_the_end_of_its_turn_with_url_passwords_hidden() {
    let daemon = Daemon::new(
        "
- hook: {event: UserPromptSubmit, prompt: \"push it\"}
- hook: {event: Stop, last_assistant_message: \"Pushed to https://me:hunter2@example.com/r\\n\\nAll done.\"}
- wait_input
",
    )
    .await;
    let project = daemon.project();
    let workspace = daemon.create_workspace(project, "answered").await;
    let reply = daemon
        .call(
            "POST",
            &format!("/api/workspaces/{workspace}/sessions"),
            Some(serde_json::json!({"model": MODEL})),
        )
        .await;
    assert_eq!(reply.status, 201, "{}", reply.body);
    let session: SessionId = reply.id();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let brief = loop {
        let reply = daemon.call("GET", "/api/briefs", None).await;
        assert_eq!(reply.status, 200, "{}", reply.body);
        let briefs: serde_json::Value = serde_json::from_str(&reply.body).unwrap();
        let brief = briefs[session.to_string()].clone();
        if !brief["said"].is_null() {
            break brief;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "nothing said in {briefs}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(
        brief,
        serde_json::json!({
            "title": "push it",
            "now": null,
            "said": "Pushed to https://***@example.com/r\n\nAll done.",
        })
    );
}

#[tokio::test]
async fn a_sessions_trail_lists_its_steps_with_url_passwords_hidden_and_others_are_not_found() {
    let daemon = Daemon::new(
        "
- hook: {event: UserPromptSubmit, prompt: \"fetch https://me:hunter2@example.com/a\"}
- hook: {event: PreToolUse, tool_name: Read, tool_input: {file_path: \"notes.md\"}}
- hook: {event: Stop}
- wait_input
",
    )
    .await;
    let project = daemon.project();
    let workspace = daemon.create_workspace(project, "trailed").await;
    let reply = daemon
        .call(
            "POST",
            &format!("/api/workspaces/{workspace}/sessions"),
            Some(serde_json::json!({"model": MODEL})),
        )
        .await;
    assert_eq!(reply.status, 201, "{}", reply.body);
    let session: SessionId = reply.id();

    let path = format!("/api/sessions/{session}/trail");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let steps = loop {
        let reply = daemon.call("GET", &path, None).await;
        assert_eq!(reply.status, 200, "{}", reply.body);
        let body: serde_json::Value = serde_json::from_str(&reply.body).unwrap();
        let steps = body["steps"].as_array().cloned().unwrap_or_default();
        if steps.len() == 3 {
            break steps;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "not three steps in {body}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let shown: Vec<(&str, &str)> = steps
        .iter()
        .map(|s| (s["kind"].as_str().unwrap(), s["text"].as_str().unwrap()))
        .collect();
    assert_eq!(
        shown,
        [
            ("prompt", "fetch https://***@example.com/a"),
            ("tool", "Read notes.md"),
            ("done", ""),
        ]
    );
    assert!(
        steps.iter().all(|s| s["at"].as_u64() > Some(0)),
        "{steps:?}"
    );

    for other in [SessionId::random().to_string(), "nonsense".to_string()] {
        let reply = daemon
            .call("GET", &format!("/api/sessions/{other}/trail"), None)
            .await;
        assert_eq!(reply.status, 404, "{}", reply.body);
    }
}

// --- events ---

/// Every frame an events socket sends within `span`.
async fn frames_within(socket: &mut Socket, span: Duration) -> Vec<UiMessage> {
    let mut frames = Vec::new();
    let end = tokio::time::Instant::now() + span;
    while let Ok(Some(frame)) = tokio::time::timeout_at(end, socket.next()).await {
        if let Message::Text(text) = frame.unwrap() {
            frames.push(serde_json::from_str(text.as_str()).unwrap());
        }
    }
    frames
}

/// A session owned by `owner`, at `version`, in `state`.
fn peer_session(owner: MachineId, version: u64, state: SessionState) -> AnyRecord {
    AnyRecord::Session(Record {
        owner,
        version,
        deleted: false,
        body: Session {
            id: SessionId::random(),
            workspace: WorkspaceId::random(),
            terminal: TerminalId::random(),
            model: MODEL.into(),
            state,
        },
    })
}

/// A peer that comes online, writes a record and goes quiet: the UI gets the state again with
/// the peer online, then its record, then the state again with its session unreachable.
#[tokio::test]
async fn events_follow_a_peer_that_comes_online_writes_and_goes_offline() {
    let daemon = Daemon::new(TURN).await;
    let peer = daemon.add_machine();
    let (mut socket, _) = daemon.events().await;
    // Settings changed before the socket opened are already in its state.
    assert!(
        frames_within(&mut socket, 3 * EVENTS_EVERY)
            .await
            .is_empty()
    );

    let (mine, theirs) = Conn::pair();
    let peers = daemon.peers.clone();
    tokio::spawn(async move { peers.accept(theirs).await });
    let hello = PeerMessage::Hello {
        protocol: PROTOCOL,
        machine: peer,
        versions: Default::default(),
    };
    mine.send
        .send(serde_json::to_string(&hello).unwrap())
        .await
        .unwrap();
    // A message the daemon takes is hearing from the peer.
    let heartbeat = PeerMessage::Records {
        protocol: PROTOCOL,
        records: Vec::new(),
    };
    mine.send
        .send(serde_json::to_string(&heartbeat).unwrap())
        .await
        .unwrap();
    loop {
        if matches!(next_message(&mut socket).await, UiMessage::State { .. }) {
            break;
        }
    }
    assert!(daemon.peers.online().contains(&peer));

    let session = peer_session(peer, 1, SessionState::Working);
    let records = PeerMessage::Records {
        protocol: PROTOCOL,
        records: vec![session.clone()],
    };
    mine.send
        .send(serde_json::to_string(&records).unwrap())
        .await
        .unwrap();
    match next_message(&mut socket).await {
        UiMessage::Record { record, .. } => assert_eq!(record, session),
        other => panic!("the peer's record did not come next: {other:?}"),
    }

    // The peer says nothing more, so it is offline OFFLINE_AFTER (15 s) after its record.
    let id = match &session {
        AnyRecord::Session(s) => s.body.id,
        _ => unreachable!(),
    };
    let records = tokio::time::timeout(OFFLINE_AFTER + WAIT, async {
        loop {
            if let UiMessage::State { records, .. } = next_message(&mut socket).await {
                return records;
            }
        }
    })
    .await
    .expect("the state comes again once the peer is offline");
    assert!(!daemon.peers.online().contains(&peer));
    assert_eq!(
        session_states(&records, id),
        vec![SessionState::Unreachable]
    );
    drop(mine);
}

/// Puts a row this build cannot read into the daemon's store, as a newer build on `owner`
/// would have left it before a rollback.
fn insert_unreadable(daemon: &Daemon, owner: MachineId, version: u64) {
    let conn = rusqlite::Connection::open(daemon.data.path().join("foundry.db")).unwrap();
    conn.execute(
        "INSERT INTO records (kind, id, owner, version, record) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            "printer",
            format!("unreadable-{version}"),
            owner.to_string(),
            version as i64,
            format!(
                r#"{{"kind":"printer","owner":"{owner}","version":{version},"deleted":false,"body":{{}}}}"#
            )
        ],
    )
    .unwrap();
}

/// A record the store cannot read leaves its owner's changes up to it out, as the state does,
/// and the socket goes on sending every other change and the owner's later ones.
#[tokio::test]
async fn events_go_on_past_a_record_this_build_cannot_read() {
    let daemon = Daemon::new(TURN).await;
    let (mut socket, _) = daemon.events().await;
    let other = MachineId::random();
    insert_unreadable(&daemon, other, 1);
    // The socket meets the row within a few reads.
    let frames = frames_within(&mut socket, 3 * EVENTS_EVERY).await;
    assert!(frames.is_empty(), "{frames:?}");

    daemon.add_machine();
    loop {
        if matches!(next_message(&mut socket).await, UiMessage::Settings { .. }) {
            break;
        }
    }
    let later = peer_session(other, 2, SessionState::Idle);
    assert!(matches!(
        daemon.store.lock().unwrap().apply(&later, other).unwrap(),
        crate::store::Applied::Stored
    ));
    match next_message(&mut socket).await {
        UiMessage::Record { record, .. } => assert_eq!(record, later),
        other => panic!("the owner's later record did not come next: {other:?}"),
    }
}

#[tokio::test]
async fn events_send_a_change_once() {
    let daemon = Daemon::new(TURN).await;
    let project = daemon.project();
    let (mut socket, _) = daemon.events().await;
    let workspace = daemon.create_workspace(project, "once").await;
    let seen = until_seen(&mut socket, |seen| {
        seen.iter()
            .any(|r| matches!(r, AnyRecord::Workspace(w) if w.body.id == workspace))
    })
    .await;
    let version = seen
        .iter()
        .find_map(|r| match r {
            AnyRecord::Workspace(w) if w.body.id == workspace => Some(w.version),
            _ => None,
        })
        .unwrap();
    for frame in frames_within(&mut socket, 3 * EVENTS_EVERY).await {
        match frame {
            UiMessage::Record { record, .. } => assert!(
                !matches!(&record, AnyRecord::Workspace(w) if w.body.id == workspace),
                "the workspace record was sent again at version {version}: {record:?}"
            ),
            UiMessage::Settings { .. } => {}
            UiMessage::State { .. } => panic!("the state was sent again"),
        }
    }
}

#[tokio::test]
async fn events_send_a_settings_change_once() {
    let daemon = Daemon::new(TURN).await;
    let (mut socket, _) = daemon.events().await;
    let project = daemon.project();
    loop {
        if let UiMessage::Settings { settings, .. } = next_message(&mut socket).await {
            assert!(settings.projects.iter().any(|p| p.id == project));
            break;
        }
    }
    for frame in frames_within(&mut socket, 3 * EVENTS_EVERY).await {
        assert!(
            !matches!(frame, UiMessage::Settings { .. } | UiMessage::State { .. }),
            "the settings were sent again: {frame:?}"
        );
    }
}

// --- terminals, more ---

/// The next text frame of a terminal socket, past the binary output.
async fn next_control(socket: &mut Socket) -> serde_json::Value {
    loop {
        let frame = tokio::time::timeout(WAIT, socket.next())
            .await
            .expect("a control frame arrives in time")
            .expect("the socket stays open")
            .unwrap();
        if let Message::Text(text) = frame {
            return serde_json::from_str(text.as_str()).unwrap();
        }
    }
}

#[tokio::test]
async fn a_resize_reaches_the_terminal() {
    let daemon = Daemon::new(TURN).await;
    let project = daemon.project();
    let workspace = daemon.create_workspace(project, "resize").await;
    let shell: TerminalId = daemon
        .call("POST", &format!("/api/workspaces/{workspace}/shells"), None)
        .await
        .id();
    let (mut socket, _) =
        tokio_tungstenite::connect_async(daemon.ws_url(&format!("/api/terminals/{shell}")))
            .await
            .unwrap();
    socket
        .send(Message::text(r#"{"type":"resize","cols":100,"rows":30}"#))
        .await
        .unwrap();
    socket
        .send(Message::binary(&b"stty size\n"[..]))
        .await
        .unwrap();
    // The echo of the command holds no digits; only the answer does.
    read_until(&mut socket, "30 100").await;
}

#[tokio::test]
async fn input_the_terminal_refuses_is_an_error_frame_and_the_socket_stays_open() {
    let daemon = Daemon::new(TURN).await;
    let id = cat(&daemon).await;
    let (mut socket, _) =
        tokio_tungstenite::connect_async(daemon.ws_url(&format!("/api/terminals/{id}")))
            .await
            .unwrap();
    let flood = vec![b'x'; foundry_holder::protocol::MAX_PENDING_INPUT + 1];
    socket.send(Message::binary(flood)).await.unwrap();
    let control = next_control(&mut socket).await;
    assert_eq!(control["type"], "error", "{control}");
    assert!(control["error"].is_string(), "{control}");
    socket
        .send(Message::binary(&b"still open\n"[..]))
        .await
        .unwrap();
    read_until(&mut socket, "still open").await;
}

#[tokio::test]
async fn a_client_cut_off_for_falling_behind_is_closed_with_1013() {
    let daemon = Daemon::new(TURN).await;
    let cwd = daemon.data.path().to_path_buf();
    let written = cwd.join("written");
    let id = daemon
        .terminals
        .start(
            WorkspaceId::random(),
            &cwd,
            &[
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                "head -c 30000000 /dev/zero | tr '\\0' x; touch written; sleep 60".to_owned(),
            ],
            &[],
        )
        .await
        .unwrap();
    let (mut socket, _) =
        tokio_tungstenite::connect_async(daemon.ws_url(&format!("/api/terminals/{id}")))
            .await
            .unwrap();
    // Not reading until the child has written all of it lets this client's backlog in the
    // holder pass its limit, however slowly the child writes: what lies between the holder and
    // this client holds far less than the 30 MB.
    tokio::time::timeout(WAIT, async {
        while !written.exists() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the child writes all its output in time");
    loop {
        let frame = tokio::time::timeout(WAIT, socket.next())
            .await
            .expect("the socket closes once the holder cuts the client off");
        match frame {
            Some(Ok(Message::Close(Some(close)))) => {
                assert_eq!(u16::from(close.code), 1013, "{close:?}");
                break;
            }
            Some(Ok(_)) => {}
            other => panic!("the socket ended without a close frame: {other:?}"),
        }
    }
}

/// Stops every process whose command line names `pattern` until this is dropped.
struct Stopped(Vec<nix::unistd::Pid>);

impl Stopped {
    fn matching(pattern: &str) -> Self {
        let out = Command::new("pgrep")
            .args(["-f", pattern])
            .output()
            .unwrap();
        let pids: Vec<_> = String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .map(|pid| nix::unistd::Pid::from_raw(pid.parse().unwrap()))
            .collect();
        assert!(!pids.is_empty(), "no process names {pattern}");
        for pid in &pids {
            nix::sys::signal::kill(*pid, nix::sys::signal::Signal::SIGSTOP).unwrap();
        }
        Stopped(pids)
    }
}

impl Drop for Stopped {
    fn drop(&mut self) {
        for pid in &self.0 {
            let _ = nix::sys::signal::kill(*pid, nix::sys::signal::Signal::SIGCONT);
        }
    }
}

/// Input the holder never answers, because it is stopped, ends the socket with 1011 and the
/// reason, not with 1000 or 1013.
#[tokio::test]
async fn input_the_holder_never_answers_closes_the_socket_with_1011_and_the_reason() {
    let daemon = Daemon::new(TURN).await;
    let id = cat(&daemon).await;
    let (mut socket, _) =
        tokio_tungstenite::connect_async(daemon.ws_url(&format!("/api/terminals/{id}")))
            .await
            .unwrap();
    socket
        .send(Message::binary(&b"before\n"[..]))
        .await
        .unwrap();
    read_until(&mut socket, "before").await;

    let stopped = Stopped::matching(&format!("{id}.sock"));
    socket.send(Message::binary(&b"after\n"[..])).await.unwrap();
    loop {
        let frame = tokio::time::timeout(2 * WAIT, socket.next())
            .await
            .expect("the socket closes once the holder does not answer");
        match frame {
            Some(Ok(Message::Close(Some(close)))) => {
                assert_eq!(u16::from(close.code), 1011, "{close:?}");
                assert!(close.reason.contains(&id.to_string()), "{close:?}");
                break;
            }
            Some(Ok(_)) => {}
            other => panic!("the socket ended without a close frame: {other:?}"),
        }
    }
    drop(stopped);
}

/// A close reason is redacted before it is cut to the 123 bytes a close frame carries, and the
/// cut never splits a character.
#[test]
fn a_close_reason_is_redacted_then_cut_on_a_character_boundary() {
    assert_eq!(terminals::close_reason("exited"), "exited");
    // Cut before redacting, the first would keep `user:secret-tok` with no `@` to hide it.
    let url = format!("{}https://user:secret-token@host/r.git", "x".repeat(100));
    let reason = terminals::close_reason(&url);
    assert!(!reason.contains("secret"), "{reason}");
    assert!(reason.len() <= 123, "{reason}");
    // One of these two puts byte 123 inside a two-byte character.
    for pad in ["", "x"] {
        let long = format!("terminal: https://u:t@h/{pad}{}", "é".repeat(80));
        let reason = terminals::close_reason(&long);
        assert!(reason.len() <= 123 && reason.len() >= 122, "{reason}");
        assert!(redact(&long).starts_with(&reason), "{reason}");
        assert!(!reason.contains("u:t"), "{reason}");
    }
}

#[tokio::test]
async fn ids_that_do_not_parse_and_a_base_that_is_not_there_are_answered_in_json() {
    let daemon = Daemon::new(TURN).await;
    let project = daemon.project();
    for path in [
        "/api/workspaces/not-an-id/shells",
        "/api/terminals/not-an-id/close",
    ] {
        let reply = daemon.call("POST", path, None).await;
        assert_eq!(reply.status, 404, "{path}: {}", reply.body);
        assert!(reply.json()["error"].is_string(), "{path}: {}", reply.body);
    }
    let reply = daemon
        .call(
            "POST",
            "/api/workspaces",
            Some(serde_json::json!({"project": project, "base": "nope", "name": "x"})),
        )
        .await;
    assert_eq!(reply.status, 422, "{}", reply.body);
    assert!(reply.json()["error"].is_string(), "{}", reply.body);
}

// --- secrets ---

#[test]
fn a_projects_repo_inside_an_overwritten_item_is_shown_redacted() {
    use foundry_proto::settings::{Overwritten, SettingKey, SettingValue, SharedSettings, Stamp};
    let project = |name: &str, secret: &str| Project {
        id: ProjectId(uuid::Uuid::from_u128(7)),
        name: name.into(),
        repo: format!("https://x-access-token:{secret}@example.invalid/o/{name}.git"),
    };
    let settings = SharedSettings {
        projects: vec![project("kept", "kept-secret")],
        overwritten: [
            (
                SettingKey::Project(ProjectId(uuid::Uuid::from_u128(7))),
                Overwritten {
                    kept: Some(SettingValue::Project(project("kept", "kept-secret"))),
                    lost: Some(SettingValue::Project(project("lost", "lost-secret"))),
                    lost_stamp: Stamp::NONE,
                },
            ),
            (
                SettingKey::Project(ProjectId(uuid::Uuid::from_u128(8))),
                Overwritten {
                    kept: None,
                    lost: Some(SettingValue::Project(project("gone", "gone-secret"))),
                    lost_stamp: Stamp::NONE,
                },
            ),
        ]
        .into(),
        ..SharedSettings::default()
    };
    let said = serde_json::to_string(&shown(settings)).unwrap();
    for secret in ["kept-secret", "lost-secret", "gone-secret"] {
        assert!(!said.contains(secret), "{said}");
    }
    for name in ["kept", "lost", "gone"] {
        assert!(
            said.contains(&format!("example.invalid/o/{name}.git")),
            "{said}"
        );
    }
}

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

/// The status and body `error` answers with.
async fn answered(error: ApiError) -> (u16, String) {
    let response = error.into_response();
    let status = response.status().as_u16();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

#[tokio::test]
async fn an_error_response_hides_the_credentials_in_every_text_it_holds() {
    let url = "https://x-access-token:SECRET@host/r.git";
    let (status, body) = answered(ApiError::Workspace(WorkspaceError::Git {
        command: "fetch",
        stderr: format!("fatal: unable to access '{url}/'"),
    }))
    .await;
    assert_eq!(status, 500);
    assert!(body.contains("host/r.git"), "{body}");
    assert!(!body.contains("SECRET"), "{body}");

    let (status, body) = answered(ApiError::Workspace(WorkspaceError::Dirty(Dirty {
        uncommitted: vec![format!("notes {url}")],
        unpushed: vec![format!("abc123 push to {url}")],
        submodules: vec![format!("sub {url}")],
    })))
    .await;
    assert_eq!(status, 409);
    assert!(!body.contains("SECRET"), "{body}");
    let body: serde_json::Value = serde_json::from_str(&body).unwrap();
    for list in ["uncommitted", "unpushed", "submodules"] {
        let items = body[list].as_array().unwrap();
        assert_eq!(items.len(), 1, "{list}");
        assert!(items[0].as_str().unwrap().contains("***@host"), "{list}");
    }
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
    // Its first frame, the state, is read here; it holds the first secret project only.
    let (mut socket, _) = daemon.events().await;
    let mut said = Vec::new();
    // A project added once the socket is open reaches it as a Settings frame.
    let late_secret = format!("late-secret-{}", uuid::Uuid::new_v4());
    daemon.add_project(Project {
        id: ProjectId::random(),
        name: "late".into(),
        repo: format!("https://deploy:{late_secret}@example.invalid/o/late.git"),
    });

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
    let mut settings_frames = 0;
    while !session_states(&seen, session).contains(&SessionState::Idle) {
        let text = next_text(&mut socket).await;
        match serde_json::from_str(&text).unwrap() {
            UiMessage::Record { record, .. } => seen.push(record),
            UiMessage::State { records, .. } => seen.extend(records),
            UiMessage::Settings { settings, .. } => {
                settings_frames += 1;
                assert!(settings.projects.iter().any(|p| p.name == "late"));
            }
        }
        said.push(text);
    }
    assert!(
        settings_frames > 0,
        "no Settings frame carried the late project"
    );
    // The read routes, before the archive takes the workspace away.
    for path in [
        "/api/briefs".to_owned(),
        format!("/api/sessions/{session}/trail"),
        format!("/api/workspaces/{workspace}/changes"),
        format!("/api/workspaces/{workspace}/pr"),
    ] {
        let reply = daemon.call("GET", &path, None).await;
        assert_eq!(reply.status, 200, "{path}: {}", reply.body);
        said.push(reply.body);
    }
    for (method, path) in [
        ("POST", format!("/api/workspaces/{workspace}/archive")),
        ("POST", "/api/workspaces".to_owned()),
        ("GET", "/api/nothing".to_owned()),
    ] {
        said.push(daemon.call(method, &path, None).await.body);
    }

    let said = said.join("\n");
    for secret in tokens.iter().map(String::as_str).chain([
        daemon.key.as_str(),
        repo_secret.as_str(),
        late_secret.as_str(),
    ]) {
        assert!(!said.contains(secret), "a response holds a secret");
    }
}

#[tokio::test]
async fn changes_list_each_file_with_its_hunks_and_hide_credentials_in_urls() {
    let daemon = Daemon::new(TURN).await;
    let project = daemon.project();
    let workspace = daemon.create_workspace(project, "changed").await;
    let worktree = daemon.workspaces.worktree(workspace).await.unwrap();
    std::fs::write(
        worktree.join("notes.txt"),
        "clone https://bot:hunter2@example.com/r.git\n",
    )
    .unwrap();
    std::fs::write(worktree.join("mirror.txt"), "m\n").unwrap();
    git(&worktree, &["add", "mirror.txt"]);
    git(
        &worktree,
        &[
            "commit",
            "-q",
            "-m",
            "mirror to https://bot:hunter2@example.com/r.git",
        ],
    );

    let path = format!("/api/workspaces/{workspace}/changes");
    let reply = daemon.call("GET", &path, None).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let json = reply.json();
    assert_eq!(json["base"], "main");
    let files = json["files"].as_array().unwrap();
    let notes = files.iter().find(|f| f["path"] == "notes.txt").unwrap();
    assert_eq!(notes["status"], "untracked");
    assert_eq!(notes["added"], 1);
    assert_eq!(notes["uncommitted"], true);
    let patch = notes["patch"].as_str().unwrap();
    assert!(
        patch.starts_with("@@ -0,0 +1,1 @@\n+clone https://"),
        "{patch}"
    );
    let subject = json["commits"][0]["subject"].as_str().unwrap();
    assert_eq!(subject, "mirror to https://***@example.com/r.git");
    assert!(!reply.body.contains("hunter2"), "{}", reply.body);

    let reply = daemon
        .call(
            "GET",
            &format!("/api/workspaces/{}/changes", WorkspaceId::random()),
            None,
        )
        .await;
    assert_eq!(reply.status, 404, "{}", reply.body);
}

#[tokio::test]
async fn a_commit_read_shows_its_own_change_and_hides_credentials_in_urls() {
    let daemon = Daemon::new(TURN).await;
    let project = daemon.project();
    let workspace = daemon.create_workspace(project, "stepped").await;
    let worktree = daemon.workspaces.worktree(workspace).await.unwrap();
    std::fs::write(
        worktree.join("mirror.txt"),
        "clone https://bot:hunter2@example.com/r.git\n",
    )
    .unwrap();
    git(&worktree, &["add", "mirror.txt"]);
    git(
        &worktree,
        &[
            "commit",
            "-q",
            "-m",
            "mirror to https://bot:hunter2@example.com/r.git",
        ],
    );
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(&worktree)
        .output()
        .unwrap();
    let hash = String::from_utf8(out.stdout).unwrap();
    let hash = hash.trim();

    let reply = daemon
        .call(
            "GET",
            &format!("/api/workspaces/{workspace}/commits/{hash}"),
            None,
        )
        .await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let json = reply.json();
    assert_eq!(json["commits"][0]["hash"], hash);
    assert_eq!(
        json["commits"][0]["subject"],
        "mirror to https://***@example.com/r.git"
    );
    assert_eq!(json["files"][0]["path"], "mirror.txt");
    assert!(!reply.body.contains("hunter2"), "{}", reply.body);

    let reply = daemon
        .call(
            "GET",
            &format!("/api/workspaces/{workspace}/commits/0000000"),
            None,
        )
        .await;
    assert_eq!(reply.status, 404, "{}", reply.body);
}

#[tokio::test]
async fn a_workspaces_pr_read_says_what_is_only_on_this_machine() {
    let daemon = Daemon::new(TURN).await;
    let project = daemon.project();
    let workspace = daemon.create_workspace(project, "only here").await;
    let worktree = daemon.workspaces.worktree(workspace).await.unwrap();
    git(&worktree, &["commit", "-q", "--allow-empty", "-m", "one"]);

    let reply = daemon
        .call("GET", &format!("/api/workspaces/{workspace}/pr"), None)
        .await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let json = reply.json();
    assert_eq!(json["branch"], "only-here");
    assert_eq!(json["pushed"], false);
    assert_eq!(json["unpushed"], 1);
    assert_eq!(json["pr"], serde_json::Value::Null);
    assert_eq!(json["unreachable"], false);

    let reply = daemon
        .call(
            "GET",
            &format!("/api/workspaces/{}/pr", WorkspaceId::random()),
            None,
        )
        .await;
    assert_eq!(reply.status, 404, "{}", reply.body);
}

#[tokio::test]
async fn a_pull_request_read_hides_credentials_in_its_title_and_link() {
    use std::os::unix::fs::PermissionsExt as _;
    let gh = tempfile::tempdir().unwrap();
    let answer = gh.path().join("answer.json");
    let stub = gh.path().join("gh");
    std::fs::write(&stub, format!("#!/bin/sh\ncat '{}'\n", answer.display())).unwrap();
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    let daemon = Daemon::start_with(TURN, built("foundry-holder"), &[], None, Some(&stub)).await;
    let project = daemon.project();
    let workspace = daemon.create_workspace(project, "mirrored").await;
    let worktree = daemon.workspaces.worktree(workspace).await.unwrap();
    git(
        &worktree,
        &["commit", "-q", "--allow-empty", "-m", "mirror"],
    );
    git(&worktree, &["push", "-q", "origin", "HEAD"]);
    let out = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&worktree)
        .output()
        .unwrap();
    let head = String::from_utf8(out.stdout).unwrap();
    std::fs::write(
        &answer,
        format!(
            r#"[{{"number":7,"title":"mirror to https://bot:hunter2@example.com/r.git","url":"https://bot:hunter2@github.com/o/r/pull/7","state":"OPEN","isDraft":false,"reviewDecision":"","statusCheckRollup":[],"headRefOid":"{}"}}]"#,
            head.trim()
        ),
    )
    .unwrap();

    let reply = daemon
        .call("GET", &format!("/api/workspaces/{workspace}/pr"), None)
        .await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let json = reply.json();
    assert_eq!(json["pr"]["number"], 7, "{}", reply.body);
    assert_eq!(
        json["pr"]["title"],
        "mirror to https://***@example.com/r.git"
    );
    assert_eq!(json["pr"]["url"], "https://***@github.com/o/r/pull/7");
    assert!(!reply.body.contains("hunter2"), "{}", reply.body);
}

// --- calls carried to another machine ---

/// A workspace of `owner`, held here as a peer's record is.
fn peer_workspace(daemon: &Daemon, owner: MachineId) -> WorkspaceId {
    let id = WorkspaceId::random();
    let record = AnyRecord::Workspace(Record {
        owner,
        version: 1,
        deleted: false,
        body: foundry_proto::record::Workspace {
            id,
            project: ProjectId::random(),
            name: "far work".into(),
            branch: "far-work".into(),
            base: "main".into(),
        },
    });
    daemon.store.lock().unwrap().apply(&record, owner).unwrap();
    id
}

#[tokio::test]
async fn machines_lists_this_machine_and_each_listed_one_with_whether_it_is_online() {
    let daemon = Daemon::new(TURN).await;
    let peer = daemon.add_machine();
    let reply = daemon.call("GET", "/api/machines", None).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(
        reply.json(),
        serde_json::json!({
            "me": daemon.me,
            "machines": [{"id": peer, "name": "peer", "online": false}],
        })
    );
}

#[tokio::test]
async fn a_create_naming_this_machine_makes_the_workspace_here() {
    let daemon = Daemon::new(TURN).await;
    let project = daemon.project();
    let body = serde_json::json!({"project": project, "base": "main", "name": "here", "machine": daemon.me});
    let reply = daemon.call("POST", "/api/workspaces", Some(body)).await;
    assert_eq!(reply.status, 201, "{}", reply.body);
    let id: WorkspaceId = reply.id();
    assert!(
        daemon
            .records()
            .iter()
            .any(|r| matches!(r, AnyRecord::Workspace(w) if w.body.id == id))
    );
}

#[tokio::test]
async fn a_create_naming_a_machine_not_listed_is_404_and_makes_nothing() {
    let daemon = Daemon::new(TURN).await;
    let project = daemon.project();
    let stranger = MachineId::random();
    let body =
        serde_json::json!({"project": project, "base": "main", "name": "far", "machine": stranger});
    let reply = daemon.call("POST", "/api/workspaces", Some(body)).await;
    assert_eq!(reply.status, 404, "{}", reply.body);
    assert!(
        reply.json()["error"]
            .as_str()
            .unwrap()
            .contains(&stranger.to_string())
    );
    assert!(
        daemon
            .records()
            .iter()
            .all(|r| !matches!(r, AnyRecord::Workspace(_)))
    );
}

#[tokio::test]
async fn a_create_or_action_for_an_offline_machine_is_503_naming_it_and_changes_nothing() {
    let daemon = Daemon::new(TURN).await;
    let project = daemon.project();
    let peer = daemon.add_machine();
    let body =
        serde_json::json!({"project": project, "base": "main", "name": "far", "machine": peer});
    let reply = daemon.call("POST", "/api/workspaces", Some(body)).await;
    assert_eq!(reply.status, 503, "{}", reply.body);
    assert_eq!(reply.json()["error"], "peer is offline");

    let workspace = peer_workspace(&daemon, peer);
    for (path, body) in [
        ("sessions", Some(serde_json::json!({"model": MODEL}))),
        ("shells", None),
        ("archive", Some(serde_json::json!({"force": true}))),
    ] {
        let path = format!("/api/workspaces/{workspace}/{path}");
        let reply = daemon.call("POST", &path, body).await;
        assert_eq!(reply.status, 503, "{path}: {}", reply.body);
        assert_eq!(reply.json()["error"], "peer is offline", "{path}");
    }
    assert!(daemon.records().iter().all(|r| !matches!(
        r,
        AnyRecord::Workspace(_) | AnyRecord::Terminal(_) | AnyRecord::Session(_)
    )));
}

#[tokio::test]
async fn a_close_of_an_offline_machines_terminal_is_503_naming_it() {
    let daemon = Daemon::new(TURN).await;
    let peer = daemon.add_machine();
    let terminal = TerminalId::random();
    let record = AnyRecord::Terminal(Record {
        owner: peer,
        version: 1,
        deleted: false,
        body: foundry_proto::record::Terminal {
            id: terminal,
            workspace: WorkspaceId::random(),
            state: TerminalState::Running,
            exit_code: None,
        },
    });
    daemon.store.lock().unwrap().apply(&record, peer).unwrap();
    let reply = daemon
        .call("POST", &format!("/api/terminals/{terminal}/close"), None)
        .await;
    assert_eq!(reply.status, 503, "{}", reply.body);
    assert_eq!(reply.json()["error"], "peer is offline");
}

/// `method path` with `body`, marked as a call carried here from another machine.
async fn carried(daemon: &Daemon, path: &str, body: Option<serde_json::Value>) -> Reply {
    let host = daemon.host();
    let headers = [("Host", host.as_str()), (crate::sync::call::CARRIED, "1")];
    daemon.raw("POST", path, &headers, body).await
}

#[tokio::test]
async fn a_carried_create_naming_another_machine_is_refused_and_never_carried_on() {
    let daemon = Daemon::new(TURN).await;
    let project = daemon.project();
    let peer = daemon.add_machine();
    let body =
        serde_json::json!({"project": project, "base": "main", "name": "far", "machine": peer});
    let reply = carried(&daemon, "/api/workspaces", Some(body)).await;
    assert_eq!(reply.status, 421, "{}", reply.body);
    assert!(
        daemon
            .records()
            .iter()
            .all(|r| !matches!(r, AnyRecord::Workspace(_)))
    );

    let body = serde_json::json!({"project": project, "base": "main", "name": "unnamed"});
    let reply = carried(&daemon, "/api/workspaces", Some(body)).await;
    assert_eq!(
        reply.status, 421,
        "a carried create names its owner: {}",
        reply.body
    );
    assert!(
        daemon
            .records()
            .iter()
            .all(|r| !matches!(r, AnyRecord::Workspace(_)))
    );

    let body = serde_json::json!({"project": project, "base": "main", "name": "here", "machine": daemon.me});
    let reply = carried(&daemon, "/api/workspaces", Some(body)).await;
    assert_eq!(reply.status, 201, "{}", reply.body);
}

#[tokio::test]
async fn a_carried_action_on_another_machines_workspace_runs_only_here_and_is_404() {
    let daemon = Daemon::new(TURN).await;
    let peer = daemon.add_machine();
    let workspace = peer_workspace(&daemon, peer);
    let path = format!("/api/workspaces/{workspace}/shells");
    let reply = carried(&daemon, &path, None).await;
    assert_eq!(reply.status, 404, "{}", reply.body);
}

#[tokio::test]
async fn a_session_start_with_no_gateway_is_refused_here_but_one_for_a_peers_workspace_is_carried()
{
    let daemon = Daemon::refusing_sessions("set FOUNDRY_GATEWAY_URL").await;
    let project = daemon.project();
    let here = daemon.create_workspace(project, "here").await;
    let reply = daemon
        .call(
            "POST",
            &format!("/api/workspaces/{here}/sessions"),
            Some(serde_json::json!({"model": MODEL})),
        )
        .await;
    assert_eq!(reply.status, 503, "{}", reply.body);
    assert_eq!(reply.json()["error"], "set FOUNDRY_GATEWAY_URL");
    let shell = daemon
        .call("POST", &format!("/api/workspaces/{here}/shells"), None)
        .await;
    assert_eq!(shell.status, 201, "{}", shell.body);

    let peer = daemon.add_machine();
    let far = peer_workspace(&daemon, peer);
    let reply = daemon
        .call(
            "POST",
            &format!("/api/workspaces/{far}/sessions"),
            Some(serde_json::json!({"model": MODEL})),
        )
        .await;
    assert_eq!(reply.status, 503, "{}", reply.body);
    assert_eq!(reply.json()["error"], "peer is offline");
}

#[tokio::test]
async fn a_create_from_another_site_naming_another_machine_is_refused_before_it_is_carried() {
    let daemon = Daemon::new(TURN).await;
    let project = daemon.project();
    let peer = daemon.add_machine();
    let host = daemon.host();
    let headers = [("Host", host.as_str()), ("Origin", "http://evil.example")];
    let body =
        serde_json::json!({"project": project, "base": "main", "name": "far", "machine": peer});
    let reply = daemon
        .raw("POST", "/api/workspaces", &headers, Some(body))
        .await;
    assert_eq!(reply.status, 403, "{}", reply.body);
    assert!(!reply.body.contains("offline"), "{}", reply.body);
}

#[tokio::test]
async fn a_peer_on_an_older_link_shows_online_and_a_call_for_it_is_503_saying_it_runs_an_older_foundry()
 {
    let daemon = Daemon::new(TURN).await;
    let project = daemon.project();
    let peer = daemon.add_machine();
    let (mine, theirs) = Conn::pair();
    let peers = daemon.peers.clone();
    tokio::spawn(async move { peers.accept(theirs).await });
    for message in [
        PeerMessage::Hello {
            protocol: 2,
            machine: peer,
            versions: Default::default(),
        },
        PeerMessage::Records {
            protocol: 2,
            records: Vec::new(),
        },
    ] {
        mine.send
            .send(serde_json::to_string(&message).unwrap())
            .await
            .unwrap();
    }
    let started = tokio::time::Instant::now();
    while !daemon.peers.online().contains(&peer) {
        assert!(started.elapsed() < WAIT, "the peer is never heard from");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let reply = daemon.call("GET", "/api/machines", None).await;
    assert_eq!(
        reply.json()["machines"][0]["online"],
        true,
        "{}",
        reply.body
    );

    let body =
        serde_json::json!({"project": project, "base": "main", "name": "far", "machine": peer});
    let reply = daemon.call("POST", "/api/workspaces", Some(body)).await;
    assert_eq!(reply.status, 503, "{}", reply.body);
    assert_eq!(
        reply.json()["error"],
        "peer runs an older Foundry, which cannot take this call"
    );
    assert!(
        daemon
            .records()
            .iter()
            .all(|r| !matches!(r, AnyRecord::Workspace(_)))
    );
    drop(mine);
}
