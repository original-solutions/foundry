//! Fleet tests of sync: every record reaches every peer, a returning machine catches up, and a
//! silent machine reads as offline with its sessions unreachable.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use foundry_proto::ids::MachineId;
use foundry_proto::messages::{PROTOCOL, PeerMessage};
use foundry_proto::record::AnyRecord;
use foundry_proto::settings::{Machine, SharedSettings};
use foundry_proto::state::{SessionState, TerminalState};
use foundryd::store::Store;
use foundryd::sync::ws::{WsDialer, router};
use foundryd::sync::{Conn, OFFLINE_AFTER, Peers};
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
    PeerMessage::Hello {
        protocol,
        machine,
        versions: BTreeMap::new(),
    }
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
        let state = daemon.peers.state().unwrap();
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
    let state = fleet.daemons[0].peers.state().unwrap();
    assert!(state.iter().all(|record| match record {
        AnyRecord::Session(r) => r.body.state == SessionState::Working,
        _ => true,
    }));
}

#[tokio::test(start_paused = true)]
async fn a_workspace_archived_while_a_returning_peer_holds_its_old_copy_stays_archived() {
    let stray = MachineId::random();
    let fleet = Fleet::start_with(3, &[stray]);
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

    // A peer that still sends the old copy changes nothing.
    let mut conn = raw_link(&a.peers);
    send(&conn, &hello(PROTOCOL, stray)).await;
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
    let fleet = Fleet::start(1);
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
    let settings = SharedSettings {
        seq: 1,
        machines: ids
            .iter()
            .enumerate()
            .map(|(i, id)| Machine {
                id: *id,
                name: format!("machine-{i}"),
                address: "127.0.0.1".into(),
            })
            .collect(),
        priority: ids.to_vec(),
        ..SharedSettings::default()
    };

    let mut daemons = Vec::new();
    for (i, listener) in listeners.into_iter().enumerate() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("foundry.db")).unwrap();
        store.apply_settings(settings.clone()).unwrap();
        let store = Arc::new(Mutex::new(store));
        // Each daemon dials the port the other one serves on.
        let dialer = WsDialer { port: ports[1 - i] };
        let peers = Peers::new(ids[i], store.clone(), Arc::new(dialer));
        tokio::spawn(axum::serve(listener, router(peers.clone())).into_future());
        let running = peers.clone();
        tokio::spawn(async move { running.run().await });
        daemons.push(Daemon {
            id: ids[i],
            store,
            peers,
            _dir: dir,
        });
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
