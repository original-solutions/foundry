//! Adding a machine by name: machine A gives a code for B through `POST /api/machines/add`,
//! B uses it with `join_at`, as `foundryd join` does, and A lists B and links with it. Each
//! machine's key travels in its machine-list entry, so machines joined through A link with
//! each other too.

use std::collections::BTreeMap;
use std::io;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use foundry_proto::ids::MachineId;
use foundry_proto::messages::{PROTOCOL, PeerMessage};
use foundry_proto::settings::{Machine, SharedSettings};
use foundryd::api::machine_names::{Entry, Listing};
use foundryd::api::{machine_add, machine_remove};
use foundryd::coordinator::Coordinator;
use foundryd::join::{self, KeyState, KeyedDialer, Keys, RECHECK};
use foundryd::secrets::FileSecrets;
use foundryd::store::Store;
use foundryd::sync::{Conn, Dialer, Peers};
use futures::future::BoxFuture;
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::relay::{Keyed, Nowhere, ServedBy, admit, keys};
use crate::{lock, until};

/// Dials through B's keys once B runs, and fails until then.
#[derive(Clone, Default)]
struct Later(Arc<Mutex<Option<ServedBy>>>);

impl Dialer for Later {
    fn dial(&self, peer: &Machine) -> BoxFuture<'static, io::Result<Conn>> {
        match lock(&self.0).as_ref() {
            Some(served) => served.dial(peer),
            None => Box::pin(async { Err(io::Error::other("B is not running yet")) }),
        }
    }
}

#[tokio::test]
async fn a_machine_added_by_name_is_listed_under_that_name_once_it_joins_and_links() {
    let (a_dir, b_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let later = Later::default();
    let a_keys = keys(&a_dir);
    let dialer = KeyedDialer::new(Arc::new(later.clone()), a_keys.clone());
    // A lists itself as `machine-0` and acts alone. It is listed at a loopback address other
    // than `localhost`'s, as a fleet run on one machine is, so it may add `localhost`. Nothing
    // dials A at it here.
    let a = Keyed::new(&[], a_keys, a_dir, Arc::new(dialer));
    {
        let mut store = lock(&a.store);
        let mut held = store.settings().unwrap();
        held.seq += 1;
        for listed in &mut held.machines {
            if listed.id == a.keys.me() {
                listed.address = "127.0.0.2".into();
            }
        }
        store.apply_settings(held).unwrap();
    }
    let coordinator = Arc::new(Coordinator::new(
        a.keys.me(),
        a.store.clone(),
        a.peers.clone(),
    ));
    a.peers.coordinate(coordinator.clone());
    tokio::spawn({
        let peers = a.peers.clone();
        async move { peers.run().await }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let lister = machine_add::Lister::new(coordinator, a.store.clone());
    let app = join::router(a.keys.clone(), lister)
        .merge(machine_add::router(a.keys.clone(), a.store.clone()));
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap()
    });

    // In A's UI: add machine `localhost`.
    let added: Value = tokio::task::spawn_blocking(move || {
        let mut answer = ureq::post(format!("http://127.0.0.1:{port}/api/machines/add"))
            .send_json(json!({ "name": "localhost" }))
            .unwrap();
        assert_eq!(answer.status(), 201);
        answer.body_mut().read_json().unwrap()
    })
    .await
    .unwrap();
    let code = added["code"].as_str().unwrap().to_owned();
    assert_eq!(added["command"], format!("foundryd join 127.0.0.2 {code}"));

    // On B: `foundryd join`, then its daemon starts.
    let secrets = FileSecrets::new(b_dir.path().join("secrets"));
    let joined = tokio::task::spawn_blocking(move || {
        join::join_at("127.0.0.1", port, &code, &secrets, None)
    })
    .await
    .unwrap()
    .unwrap();
    let b = Keyed::new(&[], keys(&b_dir), b_dir, Arc::new(Nowhere));
    assert_eq!(b.keys.me(), joined);
    tokio::spawn({
        let peers = b.peers.clone();
        async move { peers.run().await }
    });
    *lock(&later.0) = Some(ServedBy {
        keys: b.keys.clone(),
        peers: b.peers.clone(),
    });

    let a_settings = || lock(&a.store).settings().unwrap();
    until(Duration::from_secs(10), "B listed on A", || {
        a_settings()
            .machines
            .iter()
            .any(|m| m.id == joined && m.name == "localhost")
    })
    .await;
    let listed = a_settings()
        .machines
        .into_iter()
        .find(|m| m.id == joined)
        .unwrap();
    assert!(
        listed
            .address
            .parse::<std::net::IpAddr>()
            .unwrap()
            .is_loopback(),
        "{listed:?}"
    );
    until(Duration::from_secs(10), "A and B linked", || {
        a.peers.online().contains(&joined) && b.peers.online().contains(&a.keys.me())
    })
    .await;
}

// ---- keys carried in the machine list ----

/// Links dialed to whichever machine runs under the peer's id, through its keys; a machine not
/// running yet cannot be reached.
#[derive(Clone, Default)]
struct Net(Arc<Mutex<BTreeMap<MachineId, ServedBy>>>);

impl Dialer for Net {
    fn dial(&self, peer: &Machine) -> BoxFuture<'static, io::Result<Conn>> {
        match lock(&self.0).get(&peer.id) {
            Some(served) => served.dial(peer),
            None => Box::pin(async { Err(io::Error::other("not running")) }),
        }
    }
}

/// One machine's daemon, as far as joining goes: its keys, store, links and coordinator, with
/// its links and the keys its list carries followed until the test ends.
struct Member {
    keys: Keys,
    store: Arc<Mutex<Store>>,
    peers: Peers,
    coordinator: Arc<Coordinator>,
}

impl Member {
    /// Starts the daemon in `dir` on `net`, and lists it as `own`, as its Tailscale watch does.
    async fn start(net: &Net, dir: &TempDir, own: &Entry) -> Self {
        let keys = keys(dir);
        let store = Arc::new(Mutex::new(
            Store::open(dir.path().join("foundry.db")).unwrap(),
        ));
        let dialer = KeyedDialer::new(Arc::new(net.clone()), keys.clone());
        let peers = Peers::new(keys.me(), store.clone(), Arc::new(dialer));
        let coordinator = Arc::new(Coordinator::new(keys.me(), store.clone(), peers.clone()));
        peers.coordinate(coordinator.clone());
        list_itself(&keys, &coordinator, &store, own).await;
        tokio::spawn({
            let peers = peers.clone();
            async move { peers.run().await }
        });
        tokio::spawn({
            let (keys, store) = (keys.clone(), store.clone());
            async move { keys.follow(store).await }
        });
        lock(&net.0).insert(
            keys.me(),
            ServedBy {
                keys: keys.clone(),
                peers: peers.clone(),
            },
        );
        Self {
            keys,
            store,
            peers,
            coordinator,
        }
    }

    /// Serves the join and add routes on a loopback port, and gives the port.
    async fn serve(&self) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let lister = machine_add::Lister::new(self.coordinator.clone(), self.store.clone());
        let app = join::router(self.keys.clone(), lister)
            .merge(machine_add::router(self.keys.clone(), self.store.clone()))
            .merge(machine_remove::router(
                self.keys.me(),
                self.coordinator.clone(),
                self.store.clone(),
            ));
        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            .unwrap()
        });
        port
    }

    /// The numbers of the record links to `machine` that are open. `Peers` names a link by
    /// number, from 0, one per link ever opened, and has no read of which are open, so this
    /// asks after the first 64: far more than a three or four machine test opens.
    fn open_links(&self, machine: MachineId) -> Vec<u64> {
        (0..64)
            .filter(|link| self.peers.is_open(machine, *link))
            .collect()
    }

    /// Whether any link to `machine` is open, asking after the links numbered up to `through`
    /// and 64 more.
    fn linked_after(&self, machine: MachineId, through: u64) -> bool {
        (0..=through + 64).any(|link| self.peers.is_open(machine, link))
    }

    fn settings(&self) -> SharedSettings {
        lock(&self.store).settings().unwrap()
    }
}

/// Lists the machine with `keys` as `own` through `coordinator`, as the daemon's listing does
/// with no wait for peers, and waits until its list holds it.
async fn list_itself(
    keys: &Keys,
    coordinator: &Arc<Coordinator>,
    store: &Arc<Mutex<Store>>,
    own: &Entry,
) {
    let mut listing = Listing::new(
        keys.me(),
        keys.public_key().hex().to_owned(),
        coordinator.clone(),
        store.clone(),
        Duration::ZERO,
        Arc::new(AtomicBool::new(false)),
    );
    let me = keys.me();
    let listed = || {
        lock(store)
            .settings()
            .unwrap()
            .machines
            .iter()
            .any(|m| m.id == me)
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !listed() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "{me} did not list itself"
        );
        listing.list(Ok(own.clone())).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// What a machine lists itself under, at a loopback address of its own.
fn own(name: &str, address: &str) -> Entry {
    Entry {
        name: name.into(),
        address: address.into(),
    }
}

/// A's own entry: listed at a loopback address other than `localhost`'s, as a fleet run on
/// one machine is, so it may add `localhost`.
fn a_entry() -> Entry {
    own("a", "127.0.0.2")
}

/// The code of an add of `localhost` on the machine serving `port`, as A's UI makes it.
async fn add_localhost(port: u16) -> String {
    tokio::task::spawn_blocking(move || {
        let mut answer = ureq::post(format!("http://127.0.0.1:{port}/api/machines/add"))
            .send_json(json!({ "name": "localhost" }))
            .unwrap();
        assert_eq!(answer.status(), 201);
        let added: Value = answer.body_mut().read_json().unwrap();
        added["code"].as_str().unwrap().to_owned()
    })
    .await
    .unwrap()
}

/// `foundryd join 127.0.0.1 <code>` on the machine in `dir`, sending `own`.
async fn join_with(
    dir: &TempDir,
    port: u16,
    code: String,
    own: Entry,
) -> Result<MachineId, join::JoinError> {
    let secrets = FileSecrets::new(dir.path().join("secrets"));
    tokio::task::spawn_blocking(move || {
        join::join_at("127.0.0.1", port, &code, &secrets, Some(&own))
    })
    .await
    .unwrap()
}

/// Joins the machine in `dir`, listing itself as `own`, through the machine serving `port`,
/// then starts its daemon.
async fn joined(net: &Net, dir: &TempDir, port: u16, own: Entry) -> Member {
    let code = add_localhost(port).await;
    join_with(dir, port, code, own.clone()).await.unwrap();
    Member::start(net, dir, &own).await
}

#[tokio::test]
async fn machines_joined_through_one_machine_link_with_each_other() {
    let net = Net::default();
    let dirs: Vec<TempDir> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let a = Member::start(&net, &dirs[0], &a_entry()).await;
    let port = a.serve().await;

    let b = joined(&net, &dirs[1], port, own("b", "127.0.0.3")).await;
    let c = joined(&net, &dirs[2], port, own("c", "127.0.0.4")).await;

    let (b_id, c_id) = (b.keys.me(), c.keys.me());
    until(Duration::from_secs(30), "B and C linked", || {
        b.peers.online().contains(&c_id) && c.peers.online().contains(&b_id)
    })
    .await;
    assert_eq!(a.keys.state(b_id), KeyState::Active);
}

#[tokio::test]
async fn a_machine_that_listed_itself_before_it_joined_raises_no_setting_overwritten() {
    let net = Net::default();
    let (a_dir, b_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = Member::start(&net, &a_dir, &a_entry()).await;
    let port = a.serve().await;
    let b_own = own("b", "127.0.0.3");
    // B's daemon ran before the join and listed itself, then stops for `foundryd join`.
    {
        let b_keys = keys(&b_dir);
        let store = Arc::new(Mutex::new(
            Store::open(b_dir.path().join("foundry.db")).unwrap(),
        ));
        let peers = Peers::new(b_keys.me(), store.clone(), Arc::new(Nowhere));
        let coordinator = Arc::new(Coordinator::new(b_keys.me(), store.clone(), peers.clone()));
        peers.coordinate(coordinator.clone());
        list_itself(&b_keys, &coordinator, &store, &b_own).await;
    }

    let b = joined(&net, &b_dir, port, b_own.clone()).await;

    let b_id = b.keys.me();
    let entry = Machine {
        id: b_id,
        name: b_own.name.clone(),
        address: b_own.address.clone(),
        key: b.keys.public_key().hex().to_owned(),
    };
    until(
        Duration::from_secs(30),
        "A and B hold the same settings",
        || {
            let (on_a, on_b) = (a.settings(), b.settings());
            on_a.machines.contains(&entry)
                && on_a.machines.len() == 2
                && on_a.seq == on_b.seq
                && on_a.machines == on_b.machines
        },
    )
    .await;
    for (machine, settings) in [("A", a.settings()), ("B", b.settings())] {
        assert!(
            settings.overwritten.is_empty(),
            "{machine}: {:?}",
            settings.overwritten
        );
    }
}

#[tokio::test]
async fn a_redeem_whose_listing_fails_is_503_and_the_same_code_works_once_it_can_be_listed() {
    let net = Net::default();
    let (a_dir, b_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = Member::start(&net, &a_dir, &a_entry()).await;
    let port = a.serve().await;
    // X acts for A's list, is heard from, and its link drops: A's edit has nowhere to go.
    let x = MachineId::random();
    {
        let mut store = lock(&a.store);
        let mut held = store.settings().unwrap();
        held.seq += 1;
        held.machines.push(Machine {
            id: x,
            name: "x".into(),
            address: "100.64.0.9".into(),
            key: String::new(),
        });
        held.priority = vec![x, a.keys.me()];
        store.apply_settings(held).unwrap();
    }
    hear(&a.peers, x).await;
    let code = add_localhost(port).await;

    let refused = join_with(&b_dir, port, code.clone(), own("b", "127.0.0.3")).await;

    let error = refused.unwrap_err().to_string();
    assert!(error.contains(&x.to_string()), "{error}");
    assert!(error.contains("run the same command again"), "{error}");
    assert!(!error.contains(&code), "{error}");
    assert_eq!(keys(&b_dir).state(a.keys.me()), KeyState::NoKey);
    // Neither side keeps the other's key from a join that was not listed.
    assert_eq!(a.keys.state(keys(&b_dir).me()), KeyState::NoKey);

    // Once X has been silent long enough, A acts, and the same command succeeds.
    until(Duration::from_secs(30), "X offline on A", || {
        !a.peers.online().contains(&x)
    })
    .await;
    let b_id = join_with(&b_dir, port, code, own("b", "127.0.0.3"))
        .await
        .unwrap();
    assert!(
        a.settings()
            .machines
            .iter()
            .any(|m| m.id == b_id && m.name == "b")
    );
    assert_eq!(keys(&b_dir).state(a.keys.me()), KeyState::Active);
    assert_eq!(a.keys.state(b_id), KeyState::Active);
}

#[tokio::test]
async fn a_listed_key_never_admits_a_revoked_machine_or_replaces_a_key_held() {
    let net = Net::default();
    let dirs: Vec<TempDir> = (0..4).map(|_| tempfile::tempdir().unwrap()).collect();
    let a = Member::start(&net, &dirs[0], &a_entry()).await;
    let (revoked, held, newcomer) = (keys(&dirs[1]), keys(&dirs[2]), keys(&dirs[3]));
    let stranger = keys(&tempfile::tempdir().unwrap());
    admit(&a.keys, &revoked);
    a.keys.revoke(revoked.me()).unwrap();
    admit(&a.keys, &held);
    admit(&held, &a.keys);

    {
        let mut store = lock(&a.store);
        let mut settings = store.settings().unwrap();
        settings.seq += 1;
        for (id, key, name) in [
            (revoked.me(), revoked.public_key(), "revoked"),
            (held.me(), stranger.public_key(), "held"),
            (newcomer.me(), newcomer.public_key(), "newcomer"),
        ] {
            settings.machines.push(Machine {
                id,
                name: name.into(),
                address: "100.64.0.9".into(),
                key: key.hex().to_owned(),
            });
        }
        store.apply_settings(settings).unwrap();
    }

    until(Duration::from_secs(10), "the newcomer admitted", || {
        a.keys.state(newcomer.me()) == KeyState::Active
    })
    .await;
    assert_eq!(a.keys.state(revoked.me()), KeyState::Revoked);
    // A still holds the held machine's own key: a link it proves with that key opens.
    let dialer = KeyedDialer::new(
        Arc::new(ServedBy {
            keys: a.keys.clone(),
            peers: a.peers.clone(),
        }),
        held.clone(),
    );
    let a_listed = a
        .settings()
        .machines
        .into_iter()
        .find(|m| m.id == a.keys.me())
        .unwrap();
    let conn = dialer.dial(&a_listed).await.unwrap();
    let hello = PeerMessage::Hello {
        protocol: PROTOCOL,
        machine: held.me(),
        versions: BTreeMap::new(),
    };
    let nothing = PeerMessage::Records {
        protocol: PROTOCOL,
        records: Vec::new(),
    };
    for message in [hello, nothing] {
        conn.send
            .send(serde_json::to_string(&message).unwrap())
            .await
            .unwrap();
    }
    until(Duration::from_secs(10), "the held machine linked", || {
        a.peers.online().contains(&held.me())
    })
    .await;
}

/// `peer` says hello on a link to `peers` and sends nothing, so it is heard from, then its link
/// drops.
async fn hear(peers: &Peers, peer: MachineId) {
    let (ours, theirs) = Conn::pair();
    let accepting = tokio::spawn({
        let peers = peers.clone();
        async move { peers.accept(ours).await }
    });
    let hello = PeerMessage::Hello {
        protocol: PROTOCOL,
        machine: peer,
        versions: BTreeMap::new(),
    };
    let nothing = PeerMessage::Records {
        protocol: PROTOCOL,
        records: Vec::new(),
    };
    for message in [hello, nothing] {
        theirs
            .send
            .send(serde_json::to_string(&message).unwrap())
            .await
            .unwrap();
    }
    until(Duration::from_secs(10), "the peer heard", || {
        peers.online().contains(&peer)
    })
    .await;
    drop(theirs);
    accepting.await.unwrap();
}

/// How often a link sends new records and settings: `TICK` in `foundryd::sync`, which is
/// private. One sync round.
const SYNC_ROUND: Duration = Duration::from_secs(1);

/// Scheduling slack on the bounds below, and nothing else: it covers the polling and a busy
/// machine, not any wait the code is allowed.
const SLACK: Duration = Duration::from_millis(500);

/// [`until`], polling every 20 ms so a bound is measured to that.
async fn until_fast(limit: Duration, what: &str, mut done: impl FnMut() -> bool) {
    let start = tokio::time::Instant::now();
    while !done() {
        assert!(
            start.elapsed() <= limit,
            "{what}: not done within {limit:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// POSTs the remove of `machine` to the machine serving `port`: the status.
async fn post_remove(port: u16, machine: MachineId) -> u16 {
    tokio::task::spawn_blocking(move || {
        ureq::post(format!(
            "http://127.0.0.1:{port}/api/machines/{machine}/remove"
        ))
        .send_empty()
        .unwrap()
        .status()
        .as_u16()
    })
    .await
    .unwrap()
}

/// Starts the guarded listing the daemon runs, with no wait for peers, on `member`'s machine:
/// `looks` looks, a few milliseconds apart.
async fn list_guarded(member: &Member, looks: usize) {
    let mut listing = Listing::new(
        member.keys.me(),
        member.keys.public_key().hex().to_owned(),
        member.coordinator.clone(),
        member.store.clone(),
        Duration::ZERO,
        Arc::new(AtomicBool::new(false)),
    );
    let mut warned = false;
    for _ in 0..looks {
        foundryd::daemon::list_unless_removed(
            &mut listing,
            &member.store,
            member.keys.me(),
            &mut warned,
            Ok(own("c", "127.0.0.4")),
        )
        .await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn holds_workspace_of(
    member: &Member,
    owner: MachineId,
    id: foundry_proto::ids::WorkspaceId,
) -> bool {
    lock(&member.store)
        .since(owner, 0)
        .unwrap()
        .iter()
        .any(|r| matches!(r, foundry_proto::record::AnyRecord::Workspace(w) if w.body.id == id))
}

#[tokio::test]
async fn a_machine_removed_through_a_is_revoked_by_every_machine_and_its_links_close() {
    let net = Net::default();
    let dirs: Vec<TempDir> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let a = Member::start(&net, &dirs[0], &a_entry()).await;
    let port = a.serve().await;
    let b = joined(&net, &dirs[1], port, own("b", "127.0.0.3")).await;
    let c = joined(&net, &dirs[2], port, own("c", "127.0.0.4")).await;
    let (a_id, b_id, c_id) = (a.keys.me(), b.keys.me(), c.keys.me());
    until(Duration::from_secs(30), "all three linked", || {
        !a.open_links(b_id).is_empty()
            && !a.open_links(c_id).is_empty()
            && !b.open_links(a_id).is_empty()
            && !b.open_links(c_id).is_empty()
            && !c.open_links(a_id).is_empty()
            && !c.open_links(b_id).is_empty()
    })
    .await;
    // C has a record of its own, and it has reached A.
    let workspace_id = foundry_proto::ids::WorkspaceId::random();
    {
        let mut store = lock(&c.store);
        let version = store.own_next_version().unwrap();
        store
            .apply(&crate::workspace(workspace_id, false)(c_id, version), c_id)
            .unwrap();
    }
    until(Duration::from_secs(10), "C's record on A", || {
        holds_workspace_of(&a, c_id, workspace_id)
    })
    .await;
    // The links with C that are open now: the probe is shown to see them.
    let (on_a, on_b) = (a.open_links(c_id), b.open_links(c_id));
    assert!(!on_a.is_empty() && !on_b.is_empty(), "{on_a:?} {on_b:?}");
    let highest = on_a.iter().chain(&on_b).copied().max().unwrap();

    // In A's UI: remove C. A revokes at its next list check and closes the link at once. B hears
    // the removal in one sync round, then revokes at its next list check.
    assert_eq!(post_remove(port, c_id).await, 204);
    let removed = tokio::time::Instant::now();
    let bound = RECHECK + SYNC_ROUND + SLACK;
    until_fast(bound, "C revoked and unlinked on A and B", || {
        a.keys.state(c_id) == KeyState::Revoked
            && b.keys.state(c_id) == KeyState::Revoked
            && !a.linked_after(c_id, highest)
            && !b.linked_after(c_id, highest)
    })
    .await;
    eprintln!("closed {:?} after the 204", removed.elapsed());
    assert!(on_a.iter().all(|l| !a.peers.is_open(c_id, *l)));
    assert!(on_b.iter().all(|l| !b.peers.is_open(c_id, *l)));
    assert_eq!(a.keys.state(b_id), KeyState::Active);
    assert_eq!(b.keys.state(a_id), KeyState::Active);
    assert_eq!(a.keys.state(a_id), KeyState::Active);
    assert!(a.settings().machines.iter().all(|m| m.id != c_id));
    // What `GET /api/state` reads: A still holds C's records.
    let state = a.peers.state().await.unwrap();
    assert!(
        state.iter().any(|r| r.owner() == c_id),
        "A lost C's records"
    );
    assert!(holds_workspace_of(&a, c_id, workspace_id));
}

#[tokio::test]
async fn a_removed_machine_never_lists_itself_again_and_one_that_missed_the_removal_revokes_it() {
    let net = Net::default();
    let dirs: Vec<TempDir> = (0..4).map(|_| tempfile::tempdir().unwrap()).collect();
    let a = Member::start(&net, &dirs[0], &a_entry()).await;
    let port = a.serve().await;
    let b = joined(&net, &dirs[1], port, own("b", "127.0.0.3")).await;
    let c = joined(&net, &dirs[2], port, own("c", "127.0.0.4")).await;
    let (b_id, c_id) = (b.keys.me(), c.keys.me());
    // D joins through A, then starts on a net of its own that reaches nobody, with the list A
    // holds now, so it holds C's key and is out of touch while C is removed.
    let d_code = add_localhost(port).await;
    let d_own = own("d", "127.0.0.5");
    let d_id = join_with(&dirs[3], port, d_code, d_own.clone())
        .await
        .unwrap();
    until(Duration::from_secs(30), "D listed on A, B and C", || {
        [&a, &b, &c]
            .iter()
            .all(|m| m.settings().machines.iter().any(|x| x.id == d_id))
            && c.keys.state(d_id) == KeyState::Active
            && b.keys.state(d_id) == KeyState::Active
    })
    .await;
    {
        let mut seeded = Store::open(dirs[3].path().join("foundry.db")).unwrap();
        seeded.apply_settings(a.settings()).unwrap();
    }
    let d_net = Net::default();
    let d = Member::start(&d_net, &dirs[3], &d_own).await;
    until(Duration::from_secs(10), "D holds C's key", || {
        d.keys.state(c_id) == KeyState::Active
    })
    .await;
    until(Duration::from_secs(30), "A, B and C linked", || {
        !a.open_links(c_id).is_empty() && !c.open_links(b_id).is_empty()
    })
    .await;

    assert_eq!(post_remove(port, c_id).await, 204);
    // Today the removed machine is not sent the copy over a link: a link to a machine that left
    // the list closes before the send (sync/link.rs), so C usually never holds the removal. The
    // copy is handed to C here to cover the case where it does hold it, which is when the guard
    // acts.
    until(Duration::from_secs(10), "A holds the removal", || {
        join::is_removed(&a.settings(), c_id)
    })
    .await;
    // False when the copy had already arrived over the link.
    lock(&c.store).apply_settings(a.settings()).unwrap();
    assert!(join::is_removed(&c.settings(), c_id));

    // C acts alone now and runs its listing, as its daemon does, for several looks.
    list_guarded(&c, 10).await;
    assert!(
        c.settings().machines.iter().all(|m| m.id != c_id),
        "C listed itself again"
    );

    // D, which never heard of the removal, links with C first. It takes C's copy, which still
    // has C removed, and revokes it.
    lock(&d_net.0).insert(
        c_id,
        ServedBy {
            keys: c.keys.clone(),
            peers: c.peers.clone(),
        },
    );
    until(Duration::from_secs(30), "D revokes C", || {
        d.keys.state(c_id) == KeyState::Revoked
    })
    .await;
    assert!(d.settings().machines.iter().all(|m| m.id != c_id));
    assert!(c.settings().machines.iter().all(|m| m.id != c_id));
}
