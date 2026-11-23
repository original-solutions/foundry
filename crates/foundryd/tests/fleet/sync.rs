//! Fleet tests of sync: every record reaches every peer, a returning machine catches up, and a
//! silent machine reads as offline with its sessions unreachable.

use std::collections::BTreeMap;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use foundry_proto::ids::{MachineId, WorkspaceId};
use foundry_proto::messages::{PROTOCOL, PeerMessage};
use foundry_proto::record::AnyRecord;
use foundry_proto::settings::Machine;
use foundry_proto::state::{SessionState, TerminalState};
use foundryd::sync::ws::{WsDialer, router};
use foundryd::sync::{BATCH, Conn, Dialer, OFFLINE_AFTER, Peers, RETRY_REFUSED};
use futures::future::BoxFuture;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::time::{Instant, timeout};

use crate::*;

/// Opens a link to `peers` that the test speaks on as another machine.
fn raw_link(peers: &Peers) -> Conn {
    let (mine, theirs) = Conn::pair();
    let peers = peers.clone();
    tokio::spawn(async move { peers.accept(theirs).await });
    mine
}

async fn send(conn: &Conn, message: &PeerMessage) {
    let text = serde_json::to_string(message).unwrap();
    conn.send.send(text).await.expect("the link is open");
}

/// The next message on `conn`, or `None` when the link closed.
async fn next(conn: &mut Conn) -> Option<serde_json::Value> {
    let text = timeout(OFFLINE_AFTER * 2, conn.recv.recv())
        .await
        .expect("a message or a close within the offline limit")?;
    Some(serde_json::from_str(&text).unwrap())
}

fn hello(protocol: u16, machine: MachineId) -> PeerMessage {
    hello_holding(protocol, machine, BTreeMap::new())
}

fn hello_holding(
    protocol: u16,
    machine: MachineId,
    versions: BTreeMap<MachineId, u64>,
) -> PeerMessage {
    PeerMessage::Hello {
        protocol,
        machine,
        versions,
        factory_versions: Default::default(),
    }
}

/// An empty records message: a peer's heartbeat.
fn heartbeat() -> String {
    serde_json::to_string(&PeerMessage::Records {
        protocol: PROTOCOL,
        records: Vec::new(),
    })
    .unwrap()
}

/// The versions of the records in the next `messages` messages on `conn`, in the order sent.
async fn versions_sent(conn: &mut Conn, messages: usize) -> Vec<u64> {
    let mut versions = Vec::new();
    for _ in 0..messages {
        let message = next(conn).await.expect("the link stays open");
        if message["type"] == "records" {
            let records: Vec<AnyRecord> =
                serde_json::from_value(message["records"].clone()).unwrap();
            versions.extend(records.iter().map(AnyRecord::version));
        }
    }
    versions
}

/// Talks on `conn` every second for `long`, reading whatever arrives, and says whether the
/// link stayed open the whole time.
async fn keep_talking(conn: &mut Conn, long: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < long {
        if conn.send.send(heartbeat()).await.is_err() {
            return false;
        }
        loop {
            match conn.recv.try_recv() {
                Ok(_) => {}
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return false,
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    true
}

fn populated(fleet: &Fleet, state: SessionState) -> Vec<Ids> {
    fleet
        .daemons
        .iter()
        .map(|daemon| {
            let ids = Ids::random();
            populate(daemon, ids, state);
            ids
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn three_daemons_converge_on_the_same_state() {
    let fleet = Fleet::start(3);
    let ids = populated(&fleet, SessionState::Idle);

    until(Duration::from_secs(10), "catch-up", || fleet.converged()).await;
    assert_eq!(fleet.daemons[0].dump().len(), 12, "four records from each");

    // A change made once the links are up reaches every peer.
    let a = &fleet.daemons[0];
    let changed = a.write(session(
        ids[0].session,
        ids[0].workspace,
        ids[0].terminal,
        SessionState::Working,
    ));
    let changed = serde_json::to_string(&changed).unwrap();
    until(Duration::from_secs(5), "live send", || {
        fleet.daemons.iter().all(|d| d.dump().contains(&changed))
    })
    .await;
    assert!(fleet.converged());
}

#[tokio::test(start_paused = true)]
async fn a_daemon_cut_off_for_two_hours_has_the_full_state_within_30_seconds_of_healing() {
    let fleet = Fleet::start(3);
    let ids = populated(&fleet, SessionState::Idle);
    until(Duration::from_secs(10), "first sync", || fleet.converged()).await;

    fleet.cut(2);
    let states = [
        SessionState::Working,
        SessionState::WaitingForYou,
        SessionState::Idle,
    ];
    let away = Instant::now();
    let mut minute = 0;
    while away.elapsed() < Duration::from_secs(2 * 60 * 60) {
        for (daemon, ids) in fleet.daemons.iter().zip(&ids) {
            let state = states[minute % states.len()];
            daemon.write(session(ids.session, ids.workspace, ids.terminal, state));
            daemon.write(sample(minute as u32));
        }
        if minute % 10 == 0 {
            for daemon in &fleet.daemons {
                populate(daemon, Ids::random(), SessionState::Working);
            }
        }
        minute += 1;
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
    assert!(
        !fleet.converged(),
        "the cut-off daemon heard nothing while it was away"
    );

    fleet.heal(2);
    until(Duration::from_secs(30), "catch-up after two hours", || {
        fleet.converged()
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_cut_off_machine_shows_offline_on_the_others_within_15_seconds_with_its_sessions_unreachable()
 {
    let fleet = Fleet::start(3);
    populated(&fleet, SessionState::Working);
    until(Duration::from_secs(10), "first sync", || fleet.converged()).await;
    until(Duration::from_secs(10), "every machine online", || {
        fleet.daemons.iter().all(|d| d.peers.online().len() == 3)
    })
    .await;

    let gone = fleet.daemons[2].id;
    fleet.cut(2);
    until(OFFLINE_AFTER, "offline on the others", || {
        fleet.daemons[..2]
            .iter()
            .all(|d| !d.peers.online().contains(&gone))
    })
    .await;

    for daemon in &fleet.daemons[..2] {
        let state = daemon.peers.state().await.unwrap();
        let mut seen = 0;
        for record in &state {
            match record {
                AnyRecord::Session(r) if r.owner == gone => {
                    assert_eq!(r.body.state, SessionState::Unreachable);
                    seen += 1;
                }
                AnyRecord::Terminal(r) if r.owner == gone => {
                    assert_eq!(r.body.state, TerminalState::Unreachable);
                    seen += 1;
                }
                AnyRecord::Workspace(r) if r.owner == gone => seen += 1,
                AnyRecord::Session(r) => assert_eq!(r.body.state, SessionState::Working),
                AnyRecord::Terminal(r) => assert_eq!(r.body.state, TerminalState::Running),
                _ => {}
            }
        }
        assert_eq!(
            seen, 3,
            "the offline machine's records are shown, not hidden"
        );
    }
    let cut_off = &fleet.daemons[2];
    assert_eq!(
        cut_off.peers.online(),
        [gone].into(),
        "it sees itself alone"
    );

    fleet.heal(2);
    until(Duration::from_secs(5), "back online", || {
        fleet.daemons.iter().all(|d| d.peers.online().len() == 3)
    })
    .await;
    let state = fleet.daemons[0].peers.state().await.unwrap();
    assert!(state.iter().all(|record| match record {
        AnyRecord::Session(r) => r.body.state == SessionState::Working,
        _ => true,
    }));
}

#[tokio::test(start_paused = true)]
async fn a_workspace_archived_while_a_returning_peer_holds_its_old_copy_stays_archived() {
    let fleet = Fleet::start(3);
    let a = &fleet.daemons[0];
    let ids = Ids::random();
    let live = a.write(workspace(ids.workspace, false));
    let is_it = |record: &AnyRecord| matches!(record, AnyRecord::Workspace(w) if w.body.id == ids.workspace);
    until(Duration::from_secs(10), "first sync", || fleet.converged()).await;

    fleet.cut(2);
    let archived = a.write(workspace(ids.workspace, true));
    tokio::time::sleep(Duration::from_secs(2 * 60 * 60)).await;
    assert_eq!(fleet.daemons[2].held(is_it), Some(live.clone()));

    fleet.heal(2);
    until(Duration::from_secs(30), "catch-up", || fleet.converged()).await;
    for daemon in &fleet.daemons {
        assert_eq!(daemon.held(is_it), Some(archived.clone()));
    }

    // The old copy, sent again by its own owner to the returning peer, changes nothing.
    let returned = &fleet.daemons[2];
    let mut conn = raw_link(&returned.peers);
    send(&conn, &hello(PROTOCOL, a.id)).await;
    next(&mut conn).await.expect("a hello back");
    send(
        &conn,
        &PeerMessage::Records {
            protocol: PROTOCOL,
            records: vec![live],
        },
    )
    .await;
    tokio::time::sleep(Duration::from_secs(60)).await;
    for daemon in &fleet.daemons {
        assert_eq!(daemon.held(is_it), Some(archived.clone()));
    }
}

#[tokio::test(start_paused = true)]
async fn a_peer_speaking_an_older_protocol_is_sent_the_older_message_form() {
    let old = MachineId::random();
    let fleet = Fleet::start_with(1, &[old]);
    let a = &fleet.daemons[0];
    populate(a, Ids::random(), SessionState::Idle);
    let older = PROTOCOL - 1;

    let mut conn = raw_link(&a.peers);
    send(&conn, &hello(older, old)).await;
    let mut records = 0;
    for _ in 0..5 {
        let message = next(&mut conn).await.expect("the link stays open");
        assert_eq!(message["protocol"], older, "{message}");
        if message["type"] == "records" {
            records += message["records"].as_array().unwrap().len();
        }
    }
    assert_eq!(records, 4, "the catch-up arrived in the older form");
}

#[tokio::test(start_paused = true)]
async fn a_peer_speaking_a_newer_protocol_is_sent_this_builds_form() {
    let new = MachineId::random();
    let fleet = Fleet::start_with(1, &[new]);
    let a = &fleet.daemons[0];
    populate(a, Ids::random(), SessionState::Idle);

    let mut conn = raw_link(&a.peers);
    send(&conn, &hello(PROTOCOL + 1, new)).await;
    for _ in 0..3 {
        let message = next(&mut conn).await.expect("the link stays open");
        assert_eq!(message["protocol"], PROTOCOL, "{message}");
    }
}

#[tokio::test(start_paused = true)]
async fn a_hello_from_a_machine_not_in_the_list_is_refused() {
    // The list holds another machine, so it is a list to follow: a machine alone in its list
    // takes a link from any machine whose key it holds.
    let fleet = Fleet::start_with(1, &[MachineId::random()]);
    let a = &fleet.daemons[0];
    populate(a, Ids::random(), SessionState::Idle);

    let stranger = MachineId::random();
    let mut conn = raw_link(&a.peers);
    send(&conn, &hello(PROTOCOL, stranger)).await;
    assert_eq!(next(&mut conn).await, None, "closed with nothing sent");
    assert!(!a.peers.online().contains(&stranger));
}

#[tokio::test(start_paused = true)]
async fn a_record_a_peer_does_not_own_is_not_taken_from_it() {
    let liar = MachineId::random();
    let fleet = Fleet::start_with(2, &[liar]);
    let (a, b) = (&fleet.daemons[0], &fleet.daemons[1]);
    let ids = Ids::random();
    // A record b owns, but that a has not been sent yet: the liar claims to relay it.
    let forged = workspace(ids.workspace, false)(b.id, 1);

    let mut conn = raw_link(&a.peers);
    send(&conn, &hello(PROTOCOL, liar)).await;
    next(&mut conn).await.expect("a hello back");
    send(
        &conn,
        &PeerMessage::Records {
            protocol: PROTOCOL,
            records: vec![forged],
        },
    )
    .await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    let is_it = |record: &AnyRecord| matches!(record, AnyRecord::Workspace(w) if w.body.id == ids.workspace);
    assert_eq!(a.held(is_it), None);
}

#[tokio::test]
async fn two_daemons_linked_over_websockets_converge() {
    let listeners = [
        tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap(),
        tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap(),
    ];
    let ports: Vec<u16> = listeners
        .iter()
        .map(|l| l.local_addr().unwrap().port())
        .collect();
    let ids = [MachineId::random(), MachineId::random()];
    let mut settings = settings_for(&ids);
    for machine in &mut settings.machines {
        machine.address = "127.0.0.1".into();
    }

    let mut daemons = Vec::new();
    for (i, listener) in listeners.into_iter().enumerate() {
        // Each daemon dials the port the other one serves on.
        let dialer = WsDialer { port: ports[1 - i] };
        let daemon = Daemon::start(ids[i], &settings, Arc::new(dialer));
        tokio::spawn(axum::serve(listener, router(daemon.peers.clone())).into_future());
        daemons.push(daemon);
    }
    for daemon in &daemons {
        populate(daemon, Ids::random(), SessionState::Idle);
    }

    until(Duration::from_secs(10), "sync over websockets", || {
        let first = daemons[0].dump();
        first.len() == 8 && daemons[1].dump() == first
    })
    .await;
    until(Duration::from_secs(10), "both online", || {
        daemons.iter().all(|d| d.peers.online().len() == 2)
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_link_to_a_peer_silent_for_15_seconds_is_closed() {
    let quiet = MachineId::random();
    let fleet = Fleet::start_with(1, &[quiet]);
    let a = &fleet.daemons[0];

    let mut conn = raw_link(&a.peers);
    send(&conn, &hello(PROTOCOL, quiet)).await;
    let start = Instant::now();
    while next(&mut conn).await.is_some() {
        assert!(
            start.elapsed() <= OFFLINE_AFTER + Duration::from_secs(1),
            "the link is still open after {:?} of silence",
            start.elapsed()
        );
    }
    assert!(start.elapsed() >= OFFLINE_AFTER - Duration::from_secs(1));
}

/// A dialer that hands the test the far end of every link it dials, so the test can answer as
/// the dialed machine. With `hang_first`, its first dial never finishes.
struct TapDialer {
    dials: mpsc::UnboundedSender<(Machine, Conn)>,
    hang_first: AtomicBool,
}

impl Dialer for TapDialer {
    fn dial(&self, peer: &Machine) -> BoxFuture<'static, io::Result<Conn>> {
        if self.hang_first.swap(false, Ordering::SeqCst) {
            return Box::pin(futures::future::pending());
        }
        let (mine, theirs) = Conn::pair();
        let _ = self.dials.send((peer.clone(), theirs));
        Box::pin(async move { Ok(mine) })
    }
}

/// A daemon for the first of `ids` that dials the others through a [`TapDialer`].
fn tapped(
    ids: &[MachineId],
    hang_first: bool,
) -> (Daemon, mpsc::UnboundedReceiver<(Machine, Conn)>) {
    let (dials, tap) = mpsc::unbounded_channel();
    let dialer = TapDialer {
        dials,
        hang_first: AtomicBool::new(hang_first),
    };
    let daemon = Daemon::start(ids[0], &settings_for(ids), Arc::new(dialer));
    (daemon, tap)
}

/// The next link the daemon dials, within `limit`.
async fn dialed(
    tap: &mut mpsc::UnboundedReceiver<(Machine, Conn)>,
    limit: Duration,
) -> (Machine, Conn) {
    timeout(limit, tap.recv())
        .await
        .expect("a dial within the limit")
        .expect("the daemon is running")
}

#[tokio::test(start_paused = true)]
async fn a_dialed_peer_speaking_an_older_protocol_is_sent_the_older_message_form() {
    let (me, old) = (MachineId::random(), MachineId::random());
    let (daemon, mut tap) = tapped(&[me, old], false);
    populate(&daemon, Ids::random(), SessionState::Idle);
    let older = PROTOCOL - 1;

    let (_, mut conn) = dialed(&mut tap, Duration::from_secs(5)).await;
    let first = next(&mut conn).await.expect("the dialer says hello first");
    assert_eq!(first["type"], "hello");
    send(&conn, &hello(older, old)).await;
    let mut records = 0;
    for _ in 0..4 {
        let message = next(&mut conn).await.expect("the link stays open");
        assert_eq!(message["protocol"], older, "{message}");
        if message["type"] == "records" {
            records += message["records"].as_array().unwrap().len();
        }
    }
    assert_eq!(records, 4, "the catch-up arrived in the older form");
}

#[tokio::test(start_paused = true)]
async fn a_dialed_address_that_answers_as_another_machine_is_refused() {
    let (me, dialed_one, other) = (
        MachineId::random(),
        MachineId::random(),
        MachineId::random(),
    );
    let (daemon, mut tap) = tapped(&[me, dialed_one, other], false);
    populate(&daemon, Ids::random(), SessionState::Idle);

    loop {
        let (machine, mut conn) = dialed(&mut tap, Duration::from_secs(5)).await;
        if machine.id != dialed_one {
            continue;
        }
        next(&mut conn).await.expect("the dialer says hello first");
        send(&conn, &hello(PROTOCOL, other)).await;
        assert_eq!(next(&mut conn).await, None, "closed with nothing sent");
        break;
    }
    assert!(!daemon.peers.online().contains(&other));
}

#[tokio::test(start_paused = true)]
async fn a_dial_that_never_finishes_is_given_up_and_tried_again() {
    let (me, peer) = (MachineId::random(), MachineId::random());
    let (_daemon, mut tap) = tapped(&[me, peer], true);

    let (machine, _conn) = dialed(&mut tap, Duration::from_secs(10)).await;
    assert_eq!(machine.id, peer);
}

#[tokio::test(start_paused = true)]
async fn machines_added_to_or_changed_in_the_list_are_dialed_and_old_entries_are_not() {
    let (me, x, y) = (
        MachineId::random(),
        MachineId::random(),
        MachineId::random(),
    );
    let (daemon, mut tap) = tapped(&[me, x], false);
    let (first, _) = dialed(&mut tap, Duration::from_secs(5)).await;
    assert_eq!(first.id, x);
    let old_address = first.address.clone();

    let mut settings = settings_for(&[me, x, y]);
    settings.seq = 2;
    settings.machines[1].address = "100.64.9.9".into();
    daemon.set_settings(settings);
    tokio::time::sleep(Duration::from_secs(3)).await;
    while tap.try_recv().is_ok() {}

    let mut seen = Vec::new();
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        if let Ok(Some((machine, _))) = timeout(Duration::from_secs(1), tap.recv()).await {
            seen.push(machine);
        }
    }
    assert!(
        seen.iter().any(|m| m.id == y),
        "the added machine is dialed"
    );
    assert!(
        seen.iter().any(|m| m.id == x && m.address == "100.64.9.9"),
        "the changed machine is dialed at its new address"
    );
    assert!(
        seen.iter().all(|m| m.address != old_address),
        "the old address is not dialed again: {seen:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_machine_removed_from_the_list_is_dropped_on_every_link() {
    let fleet = Fleet::start(2);
    let (a, b) = (&fleet.daemons[0], &fleet.daemons[1]);
    populated(&fleet, SessionState::Idle);
    until(Duration::from_secs(10), "first sync", || fleet.converged()).await;

    // `a` keeps another machine in its list: alone in it, it would take any link its keys admit.
    let mut settings = settings_for(&[a.id, MachineId::random()]);
    settings.seq = 2;
    a.set_settings(settings);
    tokio::time::sleep(Duration::from_secs(3)).await;
    let later = b.write(sample(9));
    tokio::time::sleep(Duration::from_secs(20)).await;

    let later = serde_json::to_string(&later).unwrap();
    assert!(
        !a.dump().contains(&later),
        "a removed machine's records are not taken"
    );
    assert!(!a.peers.online().contains(&b.id));
}

#[tokio::test(start_paused = true)]
async fn a_machine_with_an_unreadable_record_of_its_own_stays_online() {
    let fleet = Fleet::start(2);
    let (a, b) = (&fleet.daemons[0], &fleet.daemons[1]);
    let ids = populated(&fleet, SessionState::Idle);
    until(Duration::from_secs(10), "first sync", || fleet.converged()).await;

    // A row a newer build wrote, in a state this build does not know, then rolled back.
    let version = lock(&a.store).own_next_version().unwrap();
    let mut row = serde_json::to_value(session(
        ids[0].session,
        ids[0].workspace,
        ids[0].terminal,
        SessionState::Idle,
    )(a.id, version))
    .unwrap();
    row["body"]["state"] = serde_json::json!("dreaming");
    let db = rusqlite::Connection::open(&a.db).unwrap();
    db.execute(
        "UPDATE records SET version = ?1, record = ?2 WHERE kind = 'session' AND id = ?3",
        rusqlite::params![version as i64, row.to_string(), ids[0].session.to_string()],
    )
    .unwrap();

    let start = Instant::now();
    while start.elapsed() < OFFLINE_AFTER * 4 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            b.peers.online().contains(&a.id),
            "a is offline on b {:?} after its row became unreadable",
            start.elapsed()
        );
    }
    let state = a.peers.state().await.expect("the other owners still show");
    assert!(state.iter().any(|record| record.owner() == b.id));
}

#[tokio::test(start_paused = true)]
async fn after_a_batch_it_cannot_read_a_link_takes_nothing_more_and_the_peer_shows_offline() {
    let newer = MachineId::random();
    let fleet = Fleet::start_with(1, &[newer]);
    let a = &fleet.daemons[0];
    let ids = Ids::random();

    let mut conn = raw_link(&a.peers);
    send(&conn, &hello(PROTOCOL, newer)).await;
    next(&mut conn).await.expect("a hello back");
    assert!(keep_talking(&mut conn, Duration::from_secs(2)).await);
    assert!(
        a.peers.online().contains(&newer),
        "online while its messages are taken"
    );

    let mut unreadable = serde_json::to_value(session(
        ids.session,
        ids.workspace,
        ids.terminal,
        SessionState::Idle,
    )(newer, 1))
    .unwrap();
    unreadable["body"]["state"] = serde_json::json!("dreaming");
    let batch =
        serde_json::json!({ "type": "records", "protocol": PROTOCOL, "records": [unreadable] });
    conn.send.send(batch.to_string()).await.unwrap();
    send(
        &conn,
        &PeerMessage::Records {
            protocol: PROTOCOL,
            records: vec![workspace(ids.workspace, false)(newer, 2)],
        },
    )
    .await;

    // The peer keeps talking. The link stays open, so it is not dialed again and its catch-up
    // is not sent again, but nothing it says counts as hearing from it.
    assert!(
        keep_talking(&mut conn, OFFLINE_AFTER * 2).await,
        "the link stays open"
    );
    assert!(
        !a.peers.online().contains(&newer),
        "a peer whose records are refused shows offline"
    );
    let is_newer = |record: &AnyRecord| record.owner() == newer;
    assert_eq!(
        a.held(is_newer),
        None,
        "nothing past the unreadable batch is taken"
    );

    // The refusing link ends a while later, so the next link's hello asks again from what is
    // stored, and a record that can be stored now is taken.
    assert!(
        !keep_talking(&mut conn, RETRY_REFUSED).await,
        "the refusing link ends"
    );
    let mut conn = raw_link(&a.peers);
    send(&conn, &hello(PROTOCOL, newer)).await;
    next(&mut conn).await.expect("a hello back");
    send(
        &conn,
        &PeerMessage::Records {
            protocol: PROTOCOL,
            records: vec![workspace(ids.workspace, false)(newer, 1)],
        },
    )
    .await;
    assert!(keep_talking(&mut conn, Duration::from_secs(2)).await);
    assert!(
        a.held(is_newer).is_some(),
        "the next link takes what can be stored"
    );
    assert!(a.peers.online().contains(&newer));
}

#[tokio::test(start_paused = true)]
async fn a_large_catch_up_is_sent_in_batches() {
    let peer = MachineId::random();
    let fleet = Fleet::start_with(1, &[peer]);
    let a = &fleet.daemons[0];
    let total = BATCH * 2 + 1;
    for _ in 0..total {
        a.write(workspace(WorkspaceId::random(), false));
    }

    let mut conn = raw_link(&a.peers);
    send(&conn, &hello(PROTOCOL, peer)).await;
    let mut received = 0;
    while received < total {
        let message = next(&mut conn).await.expect("the link stays open");
        if message["type"] == "records" {
            let n = message["records"].as_array().unwrap().len();
            assert!(n <= BATCH, "a message of {n} records");
            received += n;
        }
    }
    assert_eq!(received, total);
}

#[tokio::test(start_paused = true)]
async fn a_hello_naming_this_machine_or_no_hello_at_all_is_refused() {
    let fleet = Fleet::start(1);
    let a = &fleet.daemons[0];
    populate(a, Ids::random(), SessionState::Idle);

    let mut conn = raw_link(&a.peers);
    send(&conn, &hello(PROTOCOL, a.id)).await;
    assert_eq!(next(&mut conn).await, None, "a hello as this machine");

    let mut conn = raw_link(&a.peers);
    conn.send.send("{}".into()).await.unwrap();
    assert_eq!(
        next(&mut conn).await,
        None,
        "a first message that is not a hello"
    );

    let mut conn = raw_link(&a.peers);
    assert_eq!(
        next(&mut conn).await,
        None,
        "no hello within the offline limit"
    );
}

#[tokio::test(start_paused = true)]
async fn after_a_record_the_store_cannot_take_a_link_takes_nothing_more_and_the_peer_shows_offline()
{
    let peer = MachineId::random();
    let fleet = Fleet::start_with(1, &[peer]);
    let a = &fleet.daemons[0];
    let (first, refused, after) = (
        WorkspaceId::random(),
        WorkspaceId::random(),
        WorkspaceId::random(),
    );

    let mut conn = raw_link(&a.peers);
    send(&conn, &hello(PROTOCOL, peer)).await;
    next(&mut conn).await.expect("a hello back");
    // The store holds versions only up to i64::MAX, so the middle record cannot be stored.
    send(
        &conn,
        &PeerMessage::Records {
            protocol: PROTOCOL,
            records: vec![
                workspace(first, false)(peer, 1),
                workspace(refused, false)(peer, u64::MAX),
                workspace(after, false)(peer, 2),
            ],
        },
    )
    .await;
    send(
        &conn,
        &PeerMessage::Records {
            protocol: PROTOCOL,
            records: vec![workspace(after, false)(peer, 3)],
        },
    )
    .await;

    assert!(
        keep_talking(&mut conn, OFFLINE_AFTER * 2).await,
        "the link stays open"
    );
    assert!(
        !a.peers.online().contains(&peer),
        "a peer whose records are refused shows offline"
    );
    let held =
        |id| move |record: &AnyRecord| matches!(record, AnyRecord::Workspace(w) if w.body.id == id);
    assert!(
        a.held(held(first)).is_some(),
        "the records before it are kept"
    );
    assert_eq!(a.held(held(after)), None, "nothing after it is taken");
}

#[tokio::test(start_paused = true)]
async fn a_link_to_a_peer_that_stops_reading_is_closed() {
    let deaf = MachineId::random();
    let fleet = Fleet::start_with(1, &[deaf]);
    let a = &fleet.daemons[0];

    let conn = raw_link(&a.peers);
    send(&conn, &hello(PROTOCOL, deaf)).await;
    let empty = PeerMessage::Records {
        protocol: PROTOCOL,
        records: Vec::new(),
    };
    let start = Instant::now();
    // The peer keeps talking, so it is never silent, but it never reads what a sends.
    while !conn.send.is_closed() {
        assert!(
            start.elapsed() < OFFLINE_AFTER + Duration::from_secs(64 + 2),
            "the link is still open"
        );
        let _ = conn.send.send(serde_json::to_string(&empty).unwrap()).await;
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

#[tokio::test(start_paused = true)]
async fn catch_up_sends_only_the_versions_above_the_one_the_peer_holds() {
    let peer = MachineId::random();
    let fleet = Fleet::start_with(1, &[peer]);
    let a = &fleet.daemons[0];
    let written: Vec<u64> = (0..4)
        .map(|_| a.write(workspace(WorkspaceId::random(), false)).version())
        .collect();

    let mut conn = raw_link(&a.peers);
    send(
        &conn,
        &hello_holding(PROTOCOL, peer, [(a.id, written[1])].into()),
    )
    .await;
    assert_eq!(versions_sent(&mut conn, 4).await, written[2..]);
}

#[tokio::test(start_paused = true)]
async fn a_dialed_peer_is_sent_only_the_versions_above_the_one_it_holds() {
    let (me, peer) = (MachineId::random(), MachineId::random());
    let (daemon, mut tap) = tapped(&[me, peer], false);
    let written: Vec<u64> = (0..4)
        .map(|_| {
            daemon
                .write(workspace(WorkspaceId::random(), false))
                .version()
        })
        .collect();

    let (_, mut conn) = dialed(&mut tap, Duration::from_secs(5)).await;
    next(&mut conn).await.expect("the dialer says hello first");
    send(
        &conn,
        &hello_holding(PROTOCOL, peer, [(me, written[1])].into()),
    )
    .await;
    assert_eq!(versions_sent(&mut conn, 4).await, written[2..]);
}

#[tokio::test(start_paused = true)]
async fn a_slow_catch_up_keeps_hearing_the_peer() {
    let slow = MachineId::random();
    let fleet = Fleet::start_with(1, &[slow]);
    let a = &fleet.daemons[0];

    let mut conn = raw_link(&a.peers);
    send(&conn, &hello(PROTOCOL, slow)).await;
    // The peer talks every second but reads nothing for a minute, so the link's buffer toward
    // it fills with a's heartbeats.
    for _ in 0..60 {
        let _ = conn.send.try_send(heartbeat());
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    // Then a has six batches to send and the peer takes one message every 10 seconds, so the
    // catch-up spends far longer than the offline limit waiting on the peer.
    for _ in 0..BATCH * 6 {
        a.write(workspace(WorkspaceId::random(), false));
    }
    for second in 0..60 {
        let _ = conn.send.try_send(heartbeat());
        if second % 10 == 0 {
            let _ = conn.recv.try_recv();
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(
            a.peers.online().contains(&slow),
            "the peer shows offline {second} s into a slow catch-up"
        );
    }
}

/// Runs on real time: a store held by another thread must not stop the runtime's timers.
#[tokio::test]
async fn a_busy_store_never_stalls_the_async_workers() {
    let peer = MachineId::random();
    let fleet = Fleet::start_with(1, &[peer]);
    let a = &fleet.daemons[0];
    let mut conn = raw_link(&a.peers);
    send(&conn, &hello(PROTOCOL, peer)).await;
    next(&mut conn).await.expect("a hello back");

    // Held past several ticks of every loop that reads the store, so a call made on an async
    // worker would stall it for two seconds or more.
    let store = a.store.clone();
    let held = std::thread::spawn(move || {
        let _store = lock(&store);
        std::thread::sleep(Duration::from_millis(3000));
    });
    let start = std::time::Instant::now();
    let mut worst = Duration::ZERO;
    while start.elapsed() < Duration::from_millis(3500) {
        let before = std::time::Instant::now();
        tokio::time::sleep(Duration::from_millis(20)).await;
        worst = worst.max(before.elapsed());
    }
    held.join().unwrap();
    assert!(
        worst < Duration::from_secs(1),
        "a 20 ms sleep took {worst:?} while the store was busy"
    );
}
