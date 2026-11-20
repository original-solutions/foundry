//! Fleet tests of a terminal opened through another machine: machine A's terminal socket
//! carries a terminal machine B owns over a relay link from A to B.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use foundry_proto::ids::{MachineId, TerminalId, WorkspaceId};
use foundry_proto::record::AnyRecord;
use foundry_proto::settings::Machine;
use foundryd::api::{self, Api};
use foundryd::join::{KeyedDialer, Keys};
use foundryd::secrets::FileSecrets;
use foundryd::sessions::{self, Gateway, Sessions};
use foundryd::store::Store;
use foundryd::sync::relay::{Event, RelayError, Remote};
use foundryd::sync::{Conn, Dialer, Peers};
use foundryd::terminals::{Snapshot, Terminals};
use foundryd::workspaces::Workspaces;
use futures::future::BoxFuture;
use futures::{SinkExt as _, StreamExt as _};
use tempfile::TempDir;
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
        self.terminals
            .start(
                WorkspaceId::random(),
                self.cwd.path(),
                &["cat".to_owned()],
                &[],
            )
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
    _held: Held,
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
        _held: held,
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
    assert!(opened.is_err(), "a relay opened with no key admitted");
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
    assert!(opened.is_err(), "a relay opened from a revoked machine");
}
