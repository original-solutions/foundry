//! Fleet tests of a terminal opened through another machine: machine A's terminal socket
//! carries a terminal machine B owns over a relay link from A to B.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use foundry_proto::ids::{MachineId, TerminalId, WorkspaceId};
use foundry_proto::messages::{PROTOCOL, PeerMessage};
use foundry_proto::record::AnyRecord;
use foundry_proto::settings::Machine;
use foundryd::api::{self, Api};
use foundryd::join::{KeyedDialer, Keys};
use foundryd::secrets::FileSecrets;
use foundryd::sessions::{self, Gateway, Sessions};
use foundryd::store::Store;
use foundryd::sync::relay::{Closed, Event, RelayError, Remote};
use foundryd::sync::{Conn, Dialer, OFFLINE_AFTER, Peers};
use foundryd::terminals::{Snapshot, Terminals};
use foundryd::workspaces::Workspaces;
use futures::future::BoxFuture;
use futures::{SinkExt as _, StreamExt as _};
use tempfile::TempDir;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::{self, Message};

use crate::*;

const WAIT: Duration = Duration::from_secs(20);

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// `cargo test --workspace` builds the holder beside the test binary's `deps` folder.
fn holder() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let path = exe
        .parent()
        .and_then(Path::parent)
        .unwrap()
        .join("foundry-holder");
    assert!(
        path.exists(),
        "{} is not built: run `cargo test --workspace`",
        path.display()
    );
    path
}

/// One machine's terminals, under a data directory short enough for a socket path. Dropping
/// it closes every holder left there, so no test leaves a process behind.
struct Held {
    terminals: Arc<Terminals>,
    data: TempDir,
    cwd: TempDir,
}

impl Held {
    fn new(me: MachineId, store: Arc<Mutex<Store>>) -> Self {
        // A Unix socket path is at most about 100 bytes, which the system's own temporary
        // folder on macOS would take most of.
        let data = tempfile::Builder::new()
            .prefix("fr")
            .tempdir_in("/tmp")
            .unwrap();
        let terminals = Arc::new(Terminals::new(me, store, data.path(), holder()));
        Self {
            terminals,
            data,
            cwd: tempfile::tempdir().unwrap(),
        }
    }

    /// A terminal running `cat`.
    async fn cat(&self) -> TerminalId {
        self.start(&["cat"]).await
    }

    /// A terminal running `argv` in this machine's working directory.
    async fn start(&self, argv: &[&str]) -> TerminalId {
        let argv: Vec<String> = argv.iter().map(|a| (*a).to_owned()).collect();
        self.terminals
            .start(WorkspaceId::random(), self.cwd.path(), &argv, &[])
            .await
            .unwrap()
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        use std::io::Write as _;
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

/// A daemon's API served on a loopback port, over its store and peers, with terminals of its
/// own that hold nothing.
struct Served {
    addr: SocketAddr,
    server: JoinHandle<()>,
    _held: Held,
    _home: TempDir,
}

impl Drop for Served {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl Served {
    async fn start(daemon: &Daemon) -> Self {
        let held = Held::new(daemon.id, daemon.store.clone());
        let home = tempfile::tempdir().unwrap();
        let workspaces = Arc::new(Workspaces::new(
            daemon.id,
            daemon.store.clone(),
            held.data.path(),
            held.terminals.clone(),
        ));
        let sessions = Arc::new(Sessions::new(
            daemon.id,
            daemon.store.clone(),
            held.terminals.clone(),
            held.data.path(),
            sessions::Config {
                claude: vec!["true".into()],
                claude_json: home.path().join(".claude.json"),
                hook_port: 9,
                gateway: Gateway {
                    base_url: "http://127.0.0.1:9/gateway".into(),
                    key: "unused".into(),
                },
            },
        ));
        let api = Api::new(
            daemon.store.clone(),
            daemon.peers.clone(),
            held.terminals.clone(),
            workspaces,
            sessions,
            Vec::new(),
        );
        let listener = api::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, api::router(api)).await.unwrap();
        });
        Self {
            addr,
            server,
            _held: held,
            _home: home,
        }
    }

    fn url(&self, id: TerminalId) -> String {
        format!("ws://{}/api/terminals/{id}", self.addr)
    }

    /// Opens terminal `id`'s socket and reads its first frame, the snapshot.
    async fn attach(&self, id: TerminalId) -> (Socket, Vec<u8>) {
        let (mut socket, _) = tokio_tungstenite::connect_async(self.url(id))
            .await
            .unwrap();
        let first = timeout(WAIT, socket.next())
            .await
            .expect("the snapshot within the wait")
            .expect("the socket is open")
            .unwrap();
        let Message::Binary(bytes) = first else {
            panic!("the first frame is not the snapshot: {first:?}");
        };
        (socket, bytes.to_vec())
    }

    /// The status and body of an attach refused before the upgrade.
    async fn refused(&self, id: TerminalId) -> (u16, String) {
        match tokio_tungstenite::connect_async(self.url(id)).await {
            Err(tungstenite::Error::Http(response)) => (
                response.status().as_u16(),
                String::from_utf8_lossy(response.body().as_deref().unwrap_or_default())
                    .into_owned(),
            ),
            Ok(_) => panic!("the attach was upgraded"),
            Err(other) => panic!("the attach failed another way: {other}"),
        }
    }
}

/// Reads binary frames until their bytes together contain `wanted` `times` times.
async fn read_until(socket: &mut Socket, wanted: &str, times: usize) -> String {
    let mut got = String::new();
    timeout(WAIT, async {
        while got.matches(wanted).count() < times {
            match socket.next().await {
                Some(Ok(Message::Binary(bytes))) => got.push_str(&String::from_utf8_lossy(&bytes)),
                Some(Ok(_)) => {}
                other => panic!("the socket ended first: {other:?}; read {got:?}"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{wanted:?} not read {times} times; read {got:?}"));
    got
}

/// Waits until daemon `a` holds the record of terminal `id`.
async fn synced(a: &Daemon, id: TerminalId) {
    until(WAIT, "the terminal's record reaches A", || {
        a.held(|r| matches!(r, AnyRecord::Terminal(t) if t.body.id == id))
            .is_some()
    })
    .await;
}

/// The next close frame on `socket`, skipping any output still on its way.
async fn closed(socket: &mut Socket) -> (u16, String) {
    timeout(WAIT, async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Close(Some(close)))) => {
                    return (u16::from(close.code), close.reason.to_string());
                }
                Some(Ok(Message::Binary(_) | Message::Text(_))) => {}
                other => panic!("the socket ended without a close frame: {other:?}"),
            }
        }
    })
    .await
    .expect("the socket closes within the wait")
}

#[tokio::test]
async fn input_typed_through_a_reaches_cat_on_b_and_its_output_returns() {
    let fleet = Fleet::start(2);
    let (a, b) = (&fleet.daemons[0], &fleet.daemons[1]);
    let on_b = Held::new(b.id, b.store.clone());
    b.peers.serve_terminals(on_b.terminals.clone());
    let id = on_b.cat().await;
    synced(a, id).await;
    let api = Served::start(a).await;

    let (mut socket, _) = api.attach(id).await;
    socket
        .send(Message::text(r#"{"type":"resize","cols":100,"rows":30}"#))
        .await
        .unwrap();
    socket
        .send(Message::binary(&b"through a\n"[..]))
        .await
        .unwrap();
    // The PTY echoes the line as it is typed, then cat writes it back.
    read_until(&mut socket, "through a", 2).await;
}

#[tokio::test]
async fn cutting_the_link_closes_the_socket_with_1013_and_attach_is_503_until_it_heals() {
    let fleet = Fleet::start(2);
    let (a, b) = (&fleet.daemons[0], &fleet.daemons[1]);
    let on_b = Held::new(b.id, b.store.clone());
    b.peers.serve_terminals(on_b.terminals.clone());
    let id = on_b.cat().await;
    synced(a, id).await;
    let api = Served::start(a).await;

    let (mut socket, _) = api.attach(id).await;
    socket
        .send(Message::binary(&b"before the cut\n"[..]))
        .await
        .unwrap();
    read_until(&mut socket, "before the cut", 2).await;

    fleet.cut(1);
    let (code, reason) = closed(&mut socket).await;
    assert_eq!(code, 1013, "{reason}");
    assert_eq!(reason, "the link to machine-1 is down");

    let (status, body) = api.refused(id).await;
    assert_eq!(status, 503, "{body}");
    assert!(body.contains("the link to machine-1 is down"), "{body}");

    fleet.heal(1);
    let (_socket, snapshot) = api.attach(id).await;
    let snapshot = String::from_utf8_lossy(&snapshot);
    assert!(snapshot.contains("before the cut"), "{snapshot:?}");
}

#[tokio::test]
async fn a_relay_to_a_machine_serving_no_terminals_closes_with_a_reason() {
    let fleet = Fleet::start(2);
    let (a, b) = (&fleet.daemons[0], &fleet.daemons[1]);
    match a.peers.open_terminal(b.id, TerminalId::random()).await {
        Err(RelayError::Refused(reason)) => {
            assert!(reason.contains("serves no terminals"), "{reason}");
        }
        Err(other) => panic!("refused another way: {other}"),
        Ok(_) => panic!("a relay opened on a machine serving no terminals"),
    }
}

#[tokio::test]
async fn a_relay_for_a_terminal_b_does_not_hold_closes_with_a_reason() {
    let fleet = Fleet::start(2);
    let (a, b) = (&fleet.daemons[0], &fleet.daemons[1]);
    let on_b = Held::new(b.id, b.store.clone());
    b.peers.serve_terminals(on_b.terminals.clone());
    let unknown = TerminalId::random();
    match a.peers.open_terminal(b.id, unknown).await {
        Err(RelayError::Refused(reason)) => {
            assert!(reason.contains(&unknown.to_string()), "{reason}");
        }
        Err(other) => panic!("refused another way: {other}"),
        Ok(_) => panic!("a relay opened for a terminal B does not hold"),
    }
}

/// One machine with keys of its own, a store listing `listed`, and its peer links.
struct Keyed {
    keys: Keys,
    peers: Peers,
    store: Arc<Mutex<Store>>,
    _dir: TempDir,
}

fn keys(dir: &TempDir) -> Keys {
    Keys::open(Arc::new(FileSecrets::new(dir.path().join("secrets")))).unwrap()
}

impl Keyed {
    fn new(listed: &[MachineId], keys: Keys, dir: TempDir, dialer: Arc<dyn Dialer>) -> Self {
        let mut store = Store::open(dir.path().join("foundry.db")).unwrap();
        let mut all = vec![keys.me()];
        all.extend(listed);
        assert!(store.apply_settings(settings_for(&all)).unwrap());
        let store = Arc::new(Mutex::new(store));
        let peers = Peers::new(keys.me(), store.clone(), dialer);
        Self {
            keys,
            peers,
            store,
            _dir: dir,
        }
    }
}

/// `host` admits `peer` with a fresh code, as `foundryd join` does.
fn admit(host: &Keys, peer: &Keys) {
    let code = host.new_code().unwrap().to_string();
    host.redeem(&code, peer.me(), &peer.public_key()).unwrap();
}

/// Dials nobody.
struct Nowhere;

impl Dialer for Nowhere {
    fn dial(&self, _: &Machine) -> BoxFuture<'static, std::io::Result<Conn>> {
        Box::pin(async { Err(std::io::Error::other("no links in this test")) })
    }
}

/// Links served by another machine's [`Keys::accept`], as the daemon serves every peer link.
struct ServedBy {
    keys: Keys,
    peers: Peers,
}

impl Dialer for ServedBy {
    fn dial(&self, _: &Machine) -> BoxFuture<'static, std::io::Result<Conn>> {
        let (near, far) = Conn::pair();
        let (keys, peers) = (self.keys.clone(), self.peers.clone());
        tokio::spawn(async move { keys.accept(&peers, far).await });
        Box::pin(async move { Ok(near) })
    }
}

/// Machine B serving a `cat` terminal behind its keys, and a machine that dials B through a
/// [`KeyedDialer`]. `b_admits` says whether B holds the dialer's key; the dialer always holds
/// B's.
struct KeyedPair {
    b: Keyed,
    a: Keyed,
    terminal: TerminalId,
    held: Held,
}

async fn keyed_pair(b_admits: bool) -> KeyedPair {
    let (a_dir, b_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (a_keys, b_keys) = (keys(&a_dir), keys(&b_dir));
    admit(&a_keys, &b_keys);
    if b_admits {
        admit(&b_keys, &a_keys);
    }
    let b = Keyed::new(&[a_keys.me()], b_keys, b_dir, Arc::new(Nowhere));
    let held = Held::new(b.keys.me(), b.store.clone());
    b.peers.serve_terminals(held.terminals.clone());
    let terminal = held.cat().await;
    let dialer = KeyedDialer::new(
        Arc::new(ServedBy {
            keys: b.keys.clone(),
            peers: b.peers.clone(),
        }),
        a_keys.clone(),
    );
    let a = Keyed::new(&[b.keys.me()], a_keys, a_dir, Arc::new(dialer));
    KeyedPair {
        b,
        a,
        terminal,
        held,
    }
}

/// The relay's output until it holds `wanted` `times` times.
async fn remote_until(remote: &mut Remote, wanted: &str, times: usize) -> String {
    let mut got = String::new();
    timeout(WAIT, async {
        while got.matches(wanted).count() < times {
            match remote.next().await {
                Event::Output(bytes) => got.push_str(&String::from_utf8_lossy(&bytes)),
                Event::Refused(reason) => panic!("input refused: {reason}"),
                Event::Closed(closed) => panic!("the relay closed: {closed:?}; read {got:?}"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{wanted:?} not read {times} times; read {got:?}"));
    got
}

#[tokio::test]
async fn a_relay_through_keys_accept_carries_a_terminals_bytes_both_ways() {
    let pair = keyed_pair(true).await;
    let (_snapshot, mut remote): (Snapshot, Remote) = pair
        .a
        .peers
        .open_terminal(pair.b.keys.me(), pair.terminal)
        .await
        .unwrap();
    remote
        .input(Bytes::from_static(b"keyed relay\n"))
        .await
        .unwrap();
    remote_until(&mut remote, "keyed relay", 2).await;
}

#[tokio::test]
async fn a_relay_from_a_machine_whose_key_is_not_admitted_is_refused_before_any_byte() {
    let pair = keyed_pair(false).await;
    let opened = pair
        .a
        .peers
        .open_terminal(pair.b.keys.me(), pair.terminal)
        .await;
    match opened {
        Err(RelayError::Refused(reason)) => assert!(reason.contains("refused"), "{reason}"),
        Err(other) => panic!("refused another way: {other}"),
        Ok(_) => panic!("a relay opened with no key admitted"),
    }
}

#[tokio::test]
async fn a_relay_from_a_revoked_machine_is_refused_and_an_open_one_closes() {
    let pair = keyed_pair(true).await;
    let (_snapshot, mut remote) = pair
        .a
        .peers
        .open_terminal(pair.b.keys.me(), pair.terminal)
        .await
        .unwrap();
    pair.b.keys.revoke(pair.a.keys.me()).unwrap();
    let closed = timeout(WAIT, async {
        loop {
            if let Event::Closed(closed) = remote.next().await {
                return closed;
            }
        }
    })
    .await
    .expect("the open relay closes once its machine is revoked");
    assert_eq!(closed.code, 1013, "{closed:?}");
    let opened = pair
        .a
        .peers
        .open_terminal(pair.b.keys.me(), pair.terminal)
        .await;
    match opened {
        Err(RelayError::Refused(reason)) => assert!(reason.contains("refused"), "{reason}"),
        Err(other) => panic!("refused another way: {other}"),
        Ok(_) => panic!("a relay opened from a revoked machine"),
    }
}

/// Reads a relay to its end and returns how it closed.
async fn remote_closed(remote: &mut Remote) -> Closed {
    timeout(WAIT, async {
        loop {
            if let Event::Closed(closed) = remote.next().await {
                return closed;
            }
        }
    })
    .await
    .expect("the relay closes within the wait")
}

#[tokio::test]
async fn a_keyed_relay_for_a_terminal_b_does_not_hold_is_refused_with_bs_reason() {
    let pair = keyed_pair(true).await;
    let unknown = TerminalId::random();
    match pair.a.peers.open_terminal(pair.b.keys.me(), unknown).await {
        Err(RelayError::Refused(reason)) => {
            assert!(reason.contains(&unknown.to_string()), "{reason}");
        }
        Err(other) => panic!("refused another way: {other}"),
        Ok(_) => panic!("a relay opened for a terminal B does not hold"),
    }
}

#[tokio::test]
async fn a_keyed_relay_whose_child_exits_closes_with_1000() {
    let pair = keyed_pair(true).await;
    let id = pair.held.start(&["sh", "-c", "read x"]).await;
    let (_snapshot, mut remote) = pair
        .a
        .peers
        .open_terminal(pair.b.keys.me(), id)
        .await
        .unwrap();
    remote.input(Bytes::from_static(b"bye\n")).await.unwrap();
    assert_eq!(remote_closed(&mut remote).await, Closed::exited());
}

#[tokio::test]
async fn a_relay_naming_another_machine_than_the_one_proved_is_refused() {
    let dirs = [(); 3].map(|()| tempfile::tempdir().unwrap());
    let [a_keys, b_keys, c_keys] = [&dirs[0], &dirs[1], &dirs[2]].map(keys);
    admit(&a_keys, &b_keys);
    admit(&b_keys, &a_keys);
    admit(&b_keys, &c_keys);
    let [a_dir, b_dir, _] = dirs;
    let b = Keyed::new(
        &[a_keys.me(), c_keys.me()],
        b_keys,
        b_dir,
        Arc::new(Nowhere),
    );
    let held = Held::new(b.keys.me(), b.store.clone());
    b.peers.serve_terminals(held.terminals.clone());
    let terminal = held.cat().await;
    let _a_dir = a_dir;
    let dialer = KeyedDialer::new(
        Arc::new(ServedBy {
            keys: b.keys.clone(),
            peers: b.peers.clone(),
        }),
        a_keys,
    );
    let to_b = Machine {
        id: b.keys.me(),
        name: "b".into(),
        address: "127.0.0.1".into(),
    };
    let mut conn = dialer.dial(&to_b).await.expect("both keys are proved");
    let relay = PeerMessage::Relay {
        protocol: PROTOCOL,
        machine: c_keys.me(),
        terminal,
    };
    conn.send
        .send(serde_json::to_string(&relay).unwrap())
        .await
        .unwrap();
    let ended = timeout(WAIT, async {
        while let Some(text) = conn.recv.recv().await {
            assert!(!text.contains("snapshot"), "B relayed for C: {text}");
        }
    })
    .await;
    assert!(ended.is_ok(), "the link stays open");
}

/// A link into `peers` that opens with a relay naming `machine`.
async fn raw_relay(peers: &Peers, machine: MachineId, terminal: TerminalId) -> Conn {
    let (near, far) = Conn::pair();
    let peers = peers.clone();
    tokio::spawn(async move { peers.accept(far).await });
    let relay = PeerMessage::Relay {
        protocol: PROTOCOL,
        machine,
        terminal,
    };
    near.send
        .send(serde_json::to_string(&relay).unwrap())
        .await
        .unwrap();
    near
}

/// The next frame on `conn`, or `None` once it is closed.
async fn frame(conn: &mut Conn, within: Duration) -> Option<serde_json::Value> {
    let text = timeout(within, conn.recv.recv())
        .await
        .expect("a frame or a close within the wait")?;
    Some(serde_json::from_str(&text).unwrap())
}

#[tokio::test]
async fn a_relay_naming_a_machine_not_listed_or_the_owner_itself_closes_with_a_reason() {
    let fleet = Fleet::start(2);
    let b = &fleet.daemons[1];
    let on_b = Held::new(b.id, b.store.clone());
    b.peers.serve_terminals(on_b.terminals.clone());
    let id = on_b.cat().await;
    for machine in [MachineId::random(), b.id] {
        let mut conn = raw_relay(&b.peers, machine, id).await;
        let first = frame(&mut conn, WAIT).await.expect("a close, not silence");
        assert_eq!(first["type"], "closed", "{first}");
        assert_eq!(first["code"], 1011, "{first}");
        let reason = first["reason"].as_str().unwrap();
        assert!(reason.contains("not a peer"), "{reason}");
    }
}

#[tokio::test]
async fn the_owner_beats_on_an_idle_relay_and_ends_one_its_opener_went_silent_on() {
    let fleet = Fleet::start(2);
    let (a, b) = (&fleet.daemons[0], &fleet.daemons[1]);
    let on_b = Held::new(b.id, b.store.clone());
    b.peers.serve_terminals(on_b.terminals.clone());
    let id = on_b.cat().await;
    let mut conn = raw_relay(&b.peers, a.id, id).await;
    let first = frame(&mut conn, WAIT).await.expect("the snapshot");
    assert_eq!(first["type"], "snapshot", "{first}");
    let beat = frame(&mut conn, Duration::from_secs(3))
        .await
        .expect("a beat");
    assert_eq!(beat["type"], "beat", "{beat}");
    // This end sends nothing, so the owner takes the link as down and ends it.
    let start = tokio::time::Instant::now();
    while frame(&mut conn, OFFLINE_AFTER * 2).await.is_some() {}
    assert!(start.elapsed() >= OFFLINE_AFTER - Duration::from_secs(3));
}

/// An owner that answers a relay with `answer`, each frame after its delay, then holds the
/// link open and silent, or closes it when `hold` is false. What the opener sends after its
/// relay goes to `heard`.
struct FakeOwner {
    answer: Vec<(Duration, String)>,
    hold: bool,
    heard: Option<mpsc::UnboundedSender<String>>,
}

impl FakeOwner {
    /// Answers with `frames` at once and holds the link.
    fn answering(frames: &[&str]) -> Self {
        Self {
            answer: frames
                .iter()
                .map(|f| (Duration::ZERO, (*f).to_owned()))
                .collect(),
            hold: true,
            heard: None,
        }
    }
}

impl Dialer for FakeOwner {
    fn dial(&self, _: &Machine) -> BoxFuture<'static, std::io::Result<Conn>> {
        let (near, mut far) = Conn::pair();
        let (answer, hold, heard) = (self.answer.clone(), self.hold, self.heard.clone());
        tokio::spawn(async move {
            let _relay = far.recv.recv().await;
            for (wait, text) in answer {
                tokio::time::sleep(wait).await;
                let _ = far.send.send(text).await;
            }
            if !hold {
                return;
            }
            while let Some(text) = far.recv.recv().await {
                if let Some(heard) = &heard {
                    let _ = heard.send(text);
                }
            }
        });
        Box::pin(async move { Ok(near) })
    }
}

/// Machine A dialing its one peer, named machine-1, through `owner`.
fn opener(owner: FakeOwner) -> (Peers, MachineId, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let (me, peer) = (MachineId::random(), MachineId::random());
    let mut store = Store::open(dir.path().join("foundry.db")).unwrap();
    assert!(store.apply_settings(settings_for(&[me, peer])).unwrap());
    let peers = Peers::new(me, Arc::new(Mutex::new(store)), Arc::new(owner));
    (peers, peer, dir)
}

/// Makes `peers` hear from `owner` over a record link, so it shows online, and returns the
/// link to keep it open.
async fn hear_from(peers: &Peers, owner: MachineId) -> Conn {
    let (near, far) = Conn::pair();
    let served = peers.clone();
    tokio::spawn(async move { served.accept(far).await });
    for message in [
        PeerMessage::Hello {
            protocol: PROTOCOL,
            machine: owner,
            versions: Default::default(),
        },
        PeerMessage::Records {
            protocol: PROTOCOL,
            records: Vec::new(),
        },
    ] {
        near.send
            .send(serde_json::to_string(&message).unwrap())
            .await
            .unwrap();
    }
    until(WAIT, "A hears from the owner", || {
        peers.online().contains(&owner)
    })
    .await;
    near
}

const EMPTY_SNAPSHOT: &str = r#"{"type":"snapshot","start":0,"bytes":"","last":true}"#;

#[tokio::test(start_paused = true)]
async fn an_owner_silent_past_the_offline_limit_closes_the_relay_with_1013() {
    let (peers, owner, _dir) = opener(FakeOwner::answering(&[EMPTY_SNAPSHOT]));
    let (_snapshot, mut remote) = peers
        .open_terminal(owner, TerminalId::random())
        .await
        .unwrap();
    let start = tokio::time::Instant::now();
    let closed = remote_closed(&mut remote).await;
    assert_eq!(closed.code, 1013, "{closed:?}");
    assert_eq!(closed.reason, "the link to machine-1 is down");
    assert!(start.elapsed() >= OFFLINE_AFTER);
}

#[tokio::test(start_paused = true)]
async fn the_owners_close_codes_pass_through_and_any_other_reads_as_1011() {
    for (sent, read) in [(1013, 1013), (1000, 1000), (1011, 1011), (4000, 1011)] {
        let closing = format!(r#"{{"type":"closed","code":{sent},"reason":"odd"}}"#);
        let (peers, owner, _dir) = opener(FakeOwner::answering(&[EMPTY_SNAPSHOT, &closing]));
        let (_snapshot, mut remote) = peers
            .open_terminal(owner, TerminalId::random())
            .await
            .unwrap();
        let closed = remote_closed(&mut remote).await;
        assert_eq!(
            closed,
            Closed {
                code: read,
                reason: "odd".into()
            },
            "sent {sent}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn an_owner_heard_from_that_closes_a_relay_without_answering_has_refused_it() {
    let (peers, owner, _dir) = opener(FakeOwner {
        answer: Vec::new(),
        hold: false,
        heard: None,
    });
    let _records = hear_from(&peers, owner).await;
    match peers.open_terminal(owner, TerminalId::random()).await {
        Err(RelayError::Refused(reason)) => assert!(reason.contains("older build"), "{reason}"),
        Err(other) => panic!("refused another way: {other}"),
        Ok(_) => panic!("a relay opened with no answer"),
    }
}

#[tokio::test(start_paused = true)]
async fn a_relay_closed_by_an_owner_not_heard_from_is_down() {
    let (peers, owner, _dir) = opener(FakeOwner {
        answer: Vec::new(),
        hold: false,
        heard: None,
    });
    match peers.open_terminal(owner, TerminalId::random()).await {
        Err(RelayError::Down(name)) => assert_eq!(name, "machine-1"),
        Err(other) => panic!("refused another way: {other}"),
        Ok(_) => panic!("a relay opened with no answer"),
    }
}

#[tokio::test(start_paused = true)]
async fn a_link_that_drops_mid_snapshot_is_down() {
    let (peers, owner, _dir) = opener(FakeOwner {
        answer: vec![(
            Duration::ZERO,
            r#"{"type":"snapshot","start":0,"bytes":"61","last":false}"#.to_owned(),
        )],
        hold: false,
        heard: None,
    });
    let _records = hear_from(&peers, owner).await;
    match peers.open_terminal(owner, TerminalId::random()).await {
        Err(RelayError::Down(name)) => assert_eq!(name, "machine-1"),
        Err(other) => panic!("refused another way: {other}"),
        Ok(_) => panic!("a relay opened on half a snapshot"),
    }
}

#[tokio::test(start_paused = true)]
async fn a_snapshot_in_pieces_slower_in_total_than_the_offline_limit_arrives_whole() {
    let gap = OFFLINE_AFTER * 2 / 3;
    let piece = |hex: &str, last: bool| {
        (
            gap,
            format!(r#"{{"type":"snapshot","start":4,"bytes":"{hex}","last":{last}}}"#),
        )
    };
    let (peers, owner, _dir) = opener(FakeOwner {
        answer: vec![piece("61", false), piece("62", false), piece("63", true)],
        hold: true,
        heard: None,
    });
    let (snapshot, _remote) = peers
        .open_terminal(owner, TerminalId::random())
        .await
        .unwrap();
    assert_eq!(snapshot.start, 4);
    assert_eq!(&snapshot.bytes[..], b"abc");
}

#[tokio::test(start_paused = true)]
async fn the_opener_beats_on_an_idle_relay() {
    let (heard, mut sent) = mpsc::unbounded_channel();
    let (peers, owner, _dir) = opener(FakeOwner {
        heard: Some(heard),
        ..FakeOwner::answering(&[EMPTY_SNAPSHOT])
    });
    let (_snapshot, _remote) = peers
        .open_terminal(owner, TerminalId::random())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;
    let mut frames = Vec::new();
    while let Ok(text) = sent.try_recv() {
        frames.push(text);
    }
    assert!(
        frames.iter().any(|f| f == r#"{"type":"beat"}"#),
        "{frames:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn an_input_larger_than_one_frame_goes_in_pieces_only_the_last_marked() {
    let (heard, mut sent) = mpsc::unbounded_channel();
    let (peers, owner, _dir) = opener(FakeOwner {
        heard: Some(heard),
        ..FakeOwner::answering(&[EMPTY_SNAPSHOT])
    });
    let (_snapshot, remote) = peers
        .open_terminal(owner, TerminalId::random())
        .await
        .unwrap();
    remote
        .input(Bytes::from(vec![b'x'; 1_500_000]))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    let mut pieces = Vec::new();
    while let Ok(text) = sent.try_recv() {
        let frame: serde_json::Value = serde_json::from_str(&text).unwrap();
        if frame["type"] == "input" {
            assert!(
                text.len() < 2 * 1024 * 1024 + 100,
                "a frame of {}",
                text.len()
            );
            pieces.push(frame);
        }
    }
    assert!(pieces.len() >= 2, "{} pieces", pieces.len());
    let lasts: Vec<bool> = pieces.iter().map(|p| p["last"] == true).collect();
    assert_eq!(lasts.iter().filter(|l| **l).count(), 1);
    assert_eq!(lasts.last(), Some(&true));
    let total: usize = pieces
        .iter()
        .map(|p| p["bytes"].as_str().unwrap().len() / 2)
        .sum();
    assert_eq!(total, 1_500_000);
}

#[tokio::test]
async fn input_b_refuses_is_reported_and_the_relay_stays_open() {
    let fleet = Fleet::start(2);
    let (a, b) = (&fleet.daemons[0], &fleet.daemons[1]);
    let on_b = Held::new(b.id, b.store.clone());
    b.peers.serve_terminals(on_b.terminals.clone());
    let id = on_b.start(&["sh", "-c", "stty -echo; sleep 60"]).await;
    let (_snapshot, mut remote) = a.peers.open_terminal(b.id, id).await.unwrap();
    let too_much = foundry_holder::protocol::MAX_PENDING_INPUT + 1;
    remote
        .input(Bytes::from(vec![b'x'; too_much]))
        .await
        .unwrap();
    let event = timeout(WAIT, async {
        loop {
            match remote.next().await {
                Event::Output(_) => {}
                other => return other,
            }
        }
    })
    .await
    .expect("an answer to the input");
    assert!(matches!(event, Event::Refused(_)), "{event:?}");
    remote.input(Bytes::from_static(b"y")).await.unwrap();
}

#[tokio::test]
async fn a_screen_larger_than_one_frame_crosses_a_relay_in_pieces_and_arrives_whole() {
    let fleet = Fleet::start(2);
    let (a, b) = (&fleet.daemons[0], &fleet.daemons[1]);
    let on_b = Held::new(b.id, b.store.clone());
    b.peers.serve_terminals(on_b.terminals.clone());
    let script = r#"head -c 1500000 /dev/zero | tr "\0" x; echo END; cat"#;
    let id = on_b.start(&["sh", "-c", script]).await;
    // Until the holder has all the output, a snapshot may not hold it: attach again until it does.
    let deadline = tokio::time::Instant::now() + WAIT;
    let snapshot = loop {
        let (snapshot, _remote) = a.peers.open_terminal(b.id, id).await.unwrap();
        if String::from_utf8_lossy(&snapshot.bytes).contains("END") {
            break snapshot;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "END never reached B's screen"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(snapshot.bytes.len() > 1_500_000, "{}", snapshot.bytes.len());

    let mut conn = raw_relay(&b.peers, a.id, id).await;
    let mut lasts = Vec::new();
    loop {
        let piece = frame(&mut conn, WAIT).await.expect("a snapshot piece");
        assert_eq!(piece["type"], "snapshot", "{piece}");
        let bytes = piece["bytes"].as_str().unwrap().len();
        assert!(bytes <= 2 * 1024 * 1024, "a piece of {bytes} hex digits");
        let last = piece["last"] == true;
        lasts.push(last);
        if last {
            break;
        }
    }
    assert!(lasts.len() >= 2, "{} pieces", lasts.len());
}

#[tokio::test]
async fn a_child_that_exits_on_b_closes_as_socket_with_1000() {
    let fleet = Fleet::start(2);
    let (a, b) = (&fleet.daemons[0], &fleet.daemons[1]);
    let on_b = Held::new(b.id, b.store.clone());
    b.peers.serve_terminals(on_b.terminals.clone());
    let id = on_b.start(&["sh", "-c", "read x"]).await;
    synced(a, id).await;
    let api = Served::start(a).await;
    let (mut socket, _) = api.attach(id).await;
    socket.send(Message::binary(&b"bye\n"[..])).await.unwrap();
    assert_eq!(closed(&mut socket).await, (1000, "exited".to_owned()));
}

#[tokio::test]
async fn a_resize_through_a_reaches_the_terminal_on_b() {
    let fleet = Fleet::start(2);
    let (a, b) = (&fleet.daemons[0], &fleet.daemons[1]);
    let on_b = Held::new(b.id, b.store.clone());
    b.peers.serve_terminals(on_b.terminals.clone());
    let id = on_b.start(&["sh", "-c", "read x; stty size; read y"]).await;
    synced(a, id).await;
    let api = Served::start(a).await;
    let (mut socket, _) = api.attach(id).await;
    socket
        .send(Message::text(r#"{"type":"resize","cols":100,"rows":30}"#))
        .await
        .unwrap();
    socket.send(Message::binary(&b"go\n"[..])).await.unwrap();
    read_until(&mut socket, "30 100", 1).await;
}

#[tokio::test]
async fn an_input_larger_than_one_frame_reaches_b_whole() {
    let fleet = Fleet::start(2);
    let (a, b) = (&fleet.daemons[0], &fleet.daemons[1]);
    let on_b = Held::new(b.id, b.store.clone());
    b.peers.serve_terminals(on_b.terminals.clone());
    let script = "stty raw -echo; echo ready; head -c 2500000 > typed; echo done";
    let id = on_b.start(&["sh", "-c", script]).await;
    let (_snapshot, mut remote) = a.peers.open_terminal(b.id, id).await.unwrap();
    remote_until(&mut remote, "ready", 1).await;
    remote
        .input(Bytes::from(vec![b'x'; 2_500_000]))
        .await
        .unwrap();
    remote_until(&mut remote, "done", 1).await;
    let typed = std::fs::metadata(on_b.cwd.path().join("typed")).unwrap();
    assert_eq!(typed.len(), 2_500_000);
}

#[tokio::test]
async fn an_attach_b_refuses_is_502_with_bs_reason() {
    let fleet = Fleet::start(2);
    let (a, b) = (&fleet.daemons[0], &fleet.daemons[1]);
    let on_b = Held::new(b.id, b.store.clone());
    let id = on_b.cat().await;
    synced(a, id).await;
    let api = Served::start(a).await;
    let (status, body) = api.refused(id).await;
    assert_eq!(status, 502, "{body}");
    assert!(body.contains("serves no terminals"), "{body}");
}

#[tokio::test]
async fn an_attach_to_a_terminal_b_closed_is_404() {
    let fleet = Fleet::start(2);
    let (a, b) = (&fleet.daemons[0], &fleet.daemons[1]);
    let on_b = Held::new(b.id, b.store.clone());
    b.peers.serve_terminals(on_b.terminals.clone());
    let id = on_b.cat().await;
    synced(a, id).await;
    on_b.terminals.close(id).await.unwrap();
    until(WAIT, "the terminal's deletion reaches A", || {
        a.held(|r| matches!(r, AnyRecord::Terminal(t) if t.body.id == id && t.deleted))
            .is_some()
    })
    .await;
    let api = Served::start(a).await;
    let (status, body) = api.refused(id).await;
    assert_eq!(status, 404, "{body}");
}
