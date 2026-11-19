//! The API against a real store, holder, fake agent and local bare repo, served on a loopback
//! port and called the way the UI calls it: plain HTTP and WebSockets.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use foundry_proto::ids::{MachineId, ProjectId, SessionId, TerminalId, WorkspaceId};
use foundry_proto::messages::{PROTOCOL, PeerMessage, UiMessage};
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
    api: Api,
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
        let api = Api::new(
            store.clone(),
            peers.clone(),
            terminals.clone(),
            workspaces.clone(),
            sessions,
            names.iter().map(|n| (*n).to_owned()).collect(),
        );
        let listener = bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let served = api.clone();
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
                axum::serve(listener, router(served)).await.unwrap();
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
            api,
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

/// Starts `work` on `workspace` under its lifecycle lock, from a caller that hangs up (is
/// aborted) once the work has the lock. The work runs until `release` is sent.
async fn hung_up_holding(api: &Api, workspace: WorkspaceId) -> tokio::sync::oneshot::Sender<()> {
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let (holding, held) = tokio::sync::oneshot::channel::<()>();
    let caller = tokio::spawn({
        let api = api.clone();
        async move {
            let work = async move {
                let _ = holding.send(());
                let _ = released.await;
                Ok(())
            };
            let _ = locked(&api, workspace, work).await;
        }
    });
    tokio::time::timeout(WAIT, held)
        .await
        .expect("the work takes the lock in time")
        .expect("the work runs");
    caller.abort();
    let _ = caller.await;
    release
}

#[tokio::test]
async fn a_caller_hanging_up_leaves_its_workspace_held_until_the_work_ends() {
    let daemon = Daemon::new(TURN).await;
    let workspace = WorkspaceId::random();
    let release = hung_up_holding(&daemon.api, workspace).await;
    let next = locked(&daemon.api, workspace, async { Ok(()) });
    tokio::pin!(next);
    assert!(
        tokio::time::timeout(Duration::from_millis(500), &mut next)
            .await
            .is_err(),
        "a second piece of work on the workspace ran while the first still held it"
    );
    release.send(()).unwrap();
    let ran = tokio::time::timeout(WAIT, next)
        .await
        .expect("the second piece of work runs once the first ends");
    assert!(ran.is_ok());
}

#[tokio::test]
async fn work_on_one_workspace_does_not_wait_for_another() {
    let daemon = Daemon::new(TURN).await;
    let release = hung_up_holding(&daemon.api, WorkspaceId::random()).await;
    let ran = tokio::time::timeout(
        WAIT,
        locked(&daemon.api, WorkspaceId::random(), async { Ok(()) }),
    )
    .await
    .expect("work on another workspace does not wait");
    assert!(ran.is_ok());
    release.send(()).unwrap();
    // Once nothing holds or waits for a workspace, its lock is forgotten.
    tokio::time::timeout(WAIT, async {
        while !daemon.api.lifecycle.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("every lock no one holds is forgotten");
}

#[tokio::test]
async fn work_queued_behind_finished_work_still_holds_the_workspace() {
    let daemon = Daemon::new(TURN).await;
    let workspace = WorkspaceId::random();
    let release_first = hung_up_holding(&daemon.api, workspace).await;
    let (release_second, second_released) = tokio::sync::oneshot::channel::<()>();
    let (second_holding, second_held) = tokio::sync::oneshot::channel::<()>();
    let second = tokio::spawn({
        let api = daemon.api.clone();
        async move {
            let work = async move {
                let _ = second_holding.send(());
                let _ = second_released.await;
                Ok(())
            };
            locked(&api, workspace, work).await.is_ok()
        }
    });
    // The second is queued; the first ends and hands it the lock.
    tokio::time::sleep(Duration::from_millis(100)).await;
    release_first.send(()).unwrap();
    tokio::time::timeout(WAIT, second_held)
        .await
        .expect("the second piece of work takes the lock in time")
        .unwrap();
    let third = locked(&daemon.api, workspace, async { Ok(()) });
    tokio::pin!(third);
    assert!(
        tokio::time::timeout(Duration::from_millis(500), &mut third)
            .await
            .is_err(),
        "a third piece of work ran while the second held the workspace"
    );
    release_second.send(()).unwrap();
    assert!(second.await.unwrap());
    let ran = tokio::time::timeout(WAIT, third)
        .await
        .expect("the third piece of work runs once the second ends");
    assert!(ran.is_ok());
}

#[tokio::test]
async fn work_that_panics_leaves_its_workspace_free_and_forgotten() {
    let daemon = Daemon::new(TURN).await;
    let workspace = WorkspaceId::random();
    let api = daemon.api.clone();
    let panicked = tokio::spawn(async move {
        let work = async { panic!("the work fails") };
        let _: Result<(), ApiError> = locked(&api, workspace, work).await;
    })
    .await
    .unwrap_err();
    assert!(panicked.is_panic());
    assert!(
        daemon.api.lifecycle.lock().unwrap().is_empty(),
        "the lock of work that panicked is still in the map"
    );
    let ran = tokio::time::timeout(WAIT, locked(&daemon.api, workspace, async { Ok(()) }))
        .await
        .expect("the workspace is free after a panic");
    assert!(ran.is_ok());
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
    let routes: Vec<(&str, &str, Option<serde_json::Value>, bool)> = vec![
        ("GET", "/api/state", None, false),
        ("GET", "/api/events", None, true),
        ("GET", &socket, None, true),
        ("POST", &close, None, false),
        ("POST", "/api/workspaces", Some(create), false),
        ("POST", &archive, Some(force), false),
        ("POST", &sessions, Some(model), false),
        ("POST", &shells, None, false),
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
    let (mut socket, _) = tokio_tungstenite::connect_async(daemon.ws_url(&socket))
        .await
        .unwrap();
    socket
        .send(Message::binary(&b"still here\n"[..]))
        .await
        .unwrap();
    read_until(&mut socket, "still here").await;
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

#[tokio::test]
async fn events_send_the_state_again_when_a_peer_comes_online() {
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
    // Going offline takes OFFLINE_AFTER (15 s) of real time, which is not waited for here.
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
    let id = daemon
        .terminals
        .start(
            WorkspaceId::random(),
            &cwd,
            &[
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                "head -c 30000000 /dev/zero | tr '\\0' x; sleep 60".to_owned(),
            ],
            &[],
        )
        .await
        .unwrap();
    let (mut socket, _) =
        tokio_tungstenite::connect_async(daemon.ws_url(&format!("/api/terminals/{id}")))
            .await
            .unwrap();
    // Not reading lets this client's backlog in the holder pass its limit.
    tokio::time::sleep(Duration::from_secs(3)).await;
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
