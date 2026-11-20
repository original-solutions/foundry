//! Fleet tests of the coordinator role moving between machines, and of shared settings edits:
//! numbered by the acting machine, forwarded to it, and merged when a split heals.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use foundry_proto::ids::{MachineId, ProjectId, WorkspaceId};
use foundry_proto::messages::{PROTOCOL, PeerMessage};
use foundry_proto::record::AnyRecord;
use foundry_proto::settings::{
    Edit, Limits, Overwritten, Project, SettingKey, SettingValue, SharedSettings, Stamp,
};
use foundry_proto::state::TerminalState;
use foundryd::coordinator::Coordinator;
use foundryd::sync::{Conn, OFFLINE_AFTER};
use foundryd::terminals::Terminals;
use futures::StreamExt as _;
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::*;

const WAIT: Duration = Duration::from_secs(60);

fn project(n: u128, name: &str) -> Project {
    Project {
        id: ProjectId(uuid::Uuid::from_u128(n)),
        name: name.into(),
        repo: format!("https://example.invalid/{n}.git"),
    }
}

fn set_project(n: u128, name: &str) -> Edit {
    Edit::SetProject {
        project: project(n, name),
    }
}

fn set_limits(machine: MachineId, max_sessions: u32) -> Edit {
    Edit::SetLimits {
        machine,
        limits: Limits {
            max_sessions,
            max_load_per_core: 1.0,
            min_free_disk: 1,
        },
    }
}

fn project_named(settings: &SharedSettings, n: u128) -> Option<String> {
    settings
        .projects
        .iter()
        .find(|p| p.id.0 == uuid::Uuid::from_u128(n))
        .map(|p| p.name.clone())
}

/// Waits until `coordinator` names `want` as acting, checking every 100 ms, and returns how
/// long that took. Fails past `limit`.
async fn acting_becomes(coordinator: &Coordinator, want: MachineId, limit: Duration) -> Duration {
    let start = Instant::now();
    loop {
        if coordinator.acting().await.unwrap() == want {
            return start.elapsed();
        }
        assert!(
            start.elapsed() <= limit,
            "{want} did not take the role within {limit:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Waits until every daemon names `want` as acting and is heard from by the others.
async fn settled(fleet: &Fleet, want: MachineId) {
    let n = fleet.daemons.len();
    until(WAIT, "every machine online everywhere", || {
        fleet.daemons.iter().all(|d| d.peers.online().len() == n)
    })
    .await;
    for daemon in &fleet.daemons {
        acting_becomes(&daemon.coordinator(), want, WAIT).await;
    }
}

/// Waits until every daemon in `which` holds the same settings, and returns them.
async fn agreed(fleet: &Fleet, which: &[usize], what: &str) -> SharedSettings {
    until(WAIT, what, || {
        let first = fleet.daemons[which[0]].settings();
        which.iter().all(|i| fleet.daemons[*i].settings() == first)
    })
    .await;
    fleet.daemons[which[0]].settings()
}

/// Cuts daemon 0 off and waits until daemon 1 acts on the other side.
async fn split(fleet: &Fleet) {
    fleet.cut(0);
    let one = fleet.daemons[1].id;
    acting_becomes(&fleet.daemons[1].coordinator(), one, OFFLINE_AFTER * 2).await;
    acting_becomes(&fleet.daemons[2].coordinator(), one, OFFLINE_AFTER * 2).await;
}

// --- the role ---

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

/// Acceptance 8: the coordinator stops, the next machine in the order takes the role within
/// 15 seconds, and a terminal on another machine prints all the while.
#[tokio::test]
async fn stopping_the_coordinator_moves_the_role_in_time_while_a_session_elsewhere_prints() {
    let fleet = Fleet::start_coordinated(3);
    let (first, second) = (fleet.daemons[0].id, fleet.daemons[1].id);
    settled(&fleet, first).await;

    // A terminal on machine 2 printing a line every 100 ms.
    let data = tempfile::Builder::new()
        .prefix("fc")
        .tempdir_in("/tmp")
        .unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let terminals = Terminals::new(
        fleet.daemons[2].id,
        fleet.daemons[2].store.clone(),
        data.path(),
        holder(),
    );
    let argv: Vec<String> = ["sh", "-c", "while :; do echo tick; sleep 0.1; done"]
        .iter()
        .map(|a| (*a).to_owned())
        .collect();
    let terminal = terminals
        .start(WorkspaceId::random(), cwd.path(), &argv, &[])
        .await
        .unwrap();
    let (_, mut output) = terminals.attach(terminal).await.unwrap();
    let printed = Arc::new(Mutex::new(String::new()));
    let reader = tokio::spawn({
        let printed = printed.clone();
        async move {
            while let Some(Ok(bytes)) = output.next().await {
                lock(&printed).push_str(&String::from_utf8_lossy(&bytes));
            }
        }
    });
    let ticks = || lock(&printed).matches("tick").count();
    until(WAIT, "the terminal prints", || ticks() > 0).await;

    fleet.cut(0);
    let at_cut = ticks();
    let took = acting_becomes(&fleet.daemons[1].coordinator(), second, OFFLINE_AFTER).await;
    acting_becomes(
        &fleet.daemons[2].coordinator(),
        second,
        Duration::from_secs(1),
    )
    .await;
    let at_move = ticks();
    // Printing went on through the change of role, and goes on after it.
    assert!(
        at_move >= at_cut + (took.as_millis() / 200) as usize,
        "{} ticks in {took:?}",
        at_move - at_cut
    );
    until(Duration::from_secs(5), "the terminal prints on", || {
        ticks() > at_move + 5
    })
    .await;
    let record =
        fleet.daemons[2].held(|r| matches!(r, AnyRecord::Terminal(t) if t.body.id == terminal));
    match record {
        Some(AnyRecord::Terminal(t)) => assert_eq!(t.body.state, TerminalState::Running),
        other => panic!("the terminal's record: {other:?}"),
    }

    terminals.close(terminal).await.unwrap();
    reader.abort();
}

#[tokio::test(start_paused = true)]
async fn a_returning_higher_priority_machine_takes_the_role_back() {
    let fleet = Fleet::start_coordinated(3);
    let first = fleet.daemons[0].id;
    settled(&fleet, first).await;
    split(&fleet).await;
    fleet.heal(0);
    settled(&fleet, first).await;
}

// --- edits ---

#[tokio::test(start_paused = true)]
async fn an_edit_on_the_acting_machine_is_numbered_by_it_and_reaches_every_machine() {
    let fleet = Fleet::start_coordinated(3);
    let first = fleet.daemons[0].id;
    settled(&fleet, first).await;
    fleet.daemons[0]
        .coordinator()
        .edit(set_project(7, "p"))
        .await
        .unwrap();
    let settings = agreed(&fleet, &[0, 1, 2], "the edit reaches every machine").await;
    assert_eq!(project_named(&settings, 7).as_deref(), Some("p"));
    assert_eq!(settings.seq, 2);
    assert_eq!(
        settings.stamps[&SettingKey::Project(project(7, "p").id)].by,
        first
    );
}

#[tokio::test(start_paused = true)]
async fn an_edit_sent_to_a_machine_that_is_not_acting_is_forwarded() {
    let fleet = Fleet::start_coordinated(3);
    let first = fleet.daemons[0].id;
    settled(&fleet, first).await;
    fleet.daemons[2]
        .coordinator()
        .edit(set_project(7, "from the last machine"))
        .await
        .unwrap();
    until(WAIT, "the forwarded edit reaches every machine", || {
        fleet
            .daemons
            .iter()
            .all(|d| project_named(&d.settings(), 7).as_deref() == Some("from the last machine"))
    })
    .await;
    let settings = agreed(&fleet, &[0, 1, 2], "every machine agrees").await;
    // The acting machine numbered it.
    assert_eq!(
        settings.stamps[&SettingKey::Project(project(7, "p").id)].by,
        first
    );
    assert_eq!(settings.seen.keys().copied().collect::<Vec<_>>(), [first]);
}

#[tokio::test(start_paused = true)]
async fn an_edit_with_no_open_link_to_the_acting_machine_is_an_error() {
    let fleet = Fleet::start_coordinated(2);
    let first = fleet.daemons[0].id;
    settled(&fleet, first).await;
    fleet.cut(0);
    // Its links are closed, but it is not yet silent for long enough to lose the role.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        fleet.daemons[1].coordinator().acting().await.unwrap(),
        first
    );
    let error = fleet.daemons[1]
        .coordinator()
        .edit(set_project(7, "p"))
        .await
        .unwrap_err();
    assert!(error.to_string().contains(&first.to_string()), "{error}");
}

// --- splits ---

#[tokio::test(start_paused = true)]
async fn after_a_split_with_one_edit_on_each_side_the_later_one_holds_and_an_item_shows_both() {
    let fleet = Fleet::start_coordinated(3);
    settled(&fleet, fleet.daemons[0].id).await;
    split(&fleet).await;
    fleet.daemons[0]
        .coordinator()
        .edit(set_project(7, "from the cut side"))
        .await
        .unwrap();
    // The other side edits something else first, so its edit to the project is the later one.
    let other = fleet.daemons[1].coordinator();
    other
        .edit(set_limits(fleet.daemons[2].id, 4))
        .await
        .unwrap();
    other.edit(set_project(7, "from the rest")).await.unwrap();
    agreed(&fleet, &[1, 2], "the rest agree").await;

    fleet.heal(0);
    let settings = agreed(&fleet, &[0, 1, 2], "the sides rejoin").await;
    assert_eq!(
        project_named(&settings, 7).as_deref(),
        Some("from the rest")
    );
    assert_eq!(
        settings.overwritten,
        [(
            SettingKey::Project(project(7, "p").id),
            Overwritten {
                kept: Some(SettingValue::Project(project(7, "from the rest"))),
                lost: Some(SettingValue::Project(project(7, "from the cut side"))),
            }
        )]
        .into()
    );
    assert_eq!(settings.limits[&fleet.daemons[2].id].max_sessions, 4);
}

#[tokio::test(start_paused = true)]
async fn after_a_split_with_edits_to_different_settings_both_hold_and_no_item_is_raised() {
    let fleet = Fleet::start_coordinated(3);
    settled(&fleet, fleet.daemons[0].id).await;
    split(&fleet).await;
    fleet.daemons[0]
        .coordinator()
        .edit(set_project(7, "from the cut side"))
        .await
        .unwrap();
    fleet.daemons[1]
        .coordinator()
        .edit(set_project(8, "from the rest"))
        .await
        .unwrap();
    fleet.heal(0);
    let settings = agreed(&fleet, &[0, 1, 2], "the sides rejoin").await;
    assert_eq!(
        project_named(&settings, 7).as_deref(),
        Some("from the cut side")
    );
    assert_eq!(
        project_named(&settings, 8).as_deref(),
        Some("from the rest")
    );
    assert!(settings.overwritten.is_empty(), "{settings:?}");
}

#[tokio::test(start_paused = true)]
async fn a_machine_that_takes_over_and_edits_what_the_old_coordinator_edited_raises_no_item() {
    let fleet = Fleet::start_coordinated(3);
    settled(&fleet, fleet.daemons[0].id).await;
    fleet.daemons[0]
        .coordinator()
        .edit(set_project(7, "by the first"))
        .await
        .unwrap();
    agreed(&fleet, &[0, 1, 2], "the first edit reaches everyone").await;
    split(&fleet).await;
    fleet.daemons[1]
        .coordinator()
        .edit(set_project(7, "by the second"))
        .await
        .unwrap();
    fleet.heal(0);
    let settings = agreed(&fleet, &[0, 1, 2], "the old coordinator catches up").await;
    assert_eq!(
        project_named(&settings, 7).as_deref(),
        Some("by the second")
    );
    assert!(settings.overwritten.is_empty(), "{settings:?}");
}

#[tokio::test(start_paused = true)]
async fn a_setting_removed_on_one_side_stays_removed_unless_the_other_side_edited_it_later() {
    let fleet = Fleet::start_coordinated(3);
    settled(&fleet, fleet.daemons[0].id).await;
    let first = fleet.daemons[0].coordinator();
    first.edit(set_project(7, "seven")).await.unwrap();
    first.edit(set_project(8, "eight")).await.unwrap();
    agreed(&fleet, &[0, 1, 2], "both projects reach everyone").await;
    split(&fleet).await;
    for n in [7, 8] {
        first
            .edit(Edit::RemoveProject {
                id: project(n, "").id,
            })
            .await
            .unwrap();
    }
    // The other side leaves 7 alone, and edits 8 after more edits than the cut side made.
    let other = fleet.daemons[1].coordinator();
    for max in [1, 2] {
        other
            .edit(set_limits(fleet.daemons[2].id, max))
            .await
            .unwrap();
    }
    other.edit(set_project(8, "eight again")).await.unwrap();
    fleet.heal(0);
    let settings = agreed(&fleet, &[0, 1, 2], "the sides rejoin").await;
    assert_eq!(project_named(&settings, 7), None);
    assert_eq!(project_named(&settings, 8).as_deref(), Some("eight again"));
    assert_eq!(
        settings.overwritten[&SettingKey::Project(project(8, "").id)].lost,
        None
    );
}

#[tokio::test(start_paused = true)]
async fn a_dismissed_item_stays_dismissed_after_it_reaches_a_machine_that_still_held_it() {
    let fleet = Fleet::start_coordinated(3);
    settled(&fleet, fleet.daemons[0].id).await;
    split(&fleet).await;
    fleet.daemons[0]
        .coordinator()
        .edit(set_project(7, "a"))
        .await
        .unwrap();
    fleet.daemons[1]
        .coordinator()
        .edit(set_project(7, "b"))
        .await
        .unwrap();
    fleet.heal(0);
    let key = SettingKey::Project(project(7, "").id);
    let settings = agreed(&fleet, &[0, 1, 2], "the sides rejoin").await;
    assert!(settings.overwritten.contains_key(&key));
    settled(&fleet, fleet.daemons[0].id).await;

    // Machine 2 still holds the item while it is dismissed elsewhere.
    fleet.cut(2);
    tokio::time::sleep(OFFLINE_AFTER * 2).await;
    fleet.daemons[0]
        .coordinator()
        .edit(Edit::Dismiss {
            setting: key.clone(),
        })
        .await
        .unwrap();
    agreed(&fleet, &[0, 1], "the dismissal reaches machine 1").await;
    assert!(fleet.daemons[2].settings().overwritten.contains_key(&key));
    fleet.heal(2);
    let settings = agreed(&fleet, &[0, 1, 2], "machine 2 rejoins").await;
    assert!(settings.overwritten.is_empty(), "{settings:?}");
    tokio::time::sleep(Duration::from_secs(5)).await;
    let settings = agreed(&fleet, &[0, 1, 2], "it stays dismissed").await;
    assert!(settings.overwritten.is_empty(), "{settings:?}");
}

// --- catching up ---

/// A link to `daemon` that the test speaks on as `machine`: the hello is sent, and the
/// daemon's hello read.
async fn raw_link(daemon: &Daemon, machine: MachineId) -> Conn {
    let (mut mine, theirs) = Conn::pair();
    let peers = daemon.peers.clone();
    tokio::spawn(async move { peers.accept(theirs).await });
    send(
        &mine,
        &PeerMessage::Hello {
            protocol: PROTOCOL,
            machine,
            versions: Default::default(),
        },
    )
    .await;
    let hello = mine.recv.recv().await.expect("the daemon's hello");
    assert!(hello.contains("\"hello\""), "{hello}");
    mine
}

async fn send(conn: &Conn, message: &PeerMessage) {
    say(&conn.send, message).await;
}

async fn say(to: &mpsc::Sender<String>, message: &PeerMessage) {
    let text = serde_json::to_string(message).unwrap();
    to.send(text).await.expect("the link is open");
}

/// Reads and drops what the daemon sends on `conn`, and returns the end the test sends on.
fn drained(conn: Conn) -> mpsc::Sender<String> {
    let Conn { send, mut recv } = conn;
    tokio::spawn(async move { while recv.recv().await.is_some() {} });
    send
}

fn heartbeat() -> PeerMessage {
    PeerMessage::Records {
        protocol: PROTOCOL,
        records: Vec::new(),
    }
}

/// Keeps `conn` open as a peer that sends only heartbeats, reading and dropping what the
/// daemon sends, until the returned task is dropped.
fn beat(conn: Conn) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let Conn { send, mut recv } = conn;
        tokio::spawn(async move { while recv.recv().await.is_some() {} });
        let text = serde_json::to_string(&heartbeat()).unwrap();
        loop {
            if send.send(text.clone()).await.is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    })
}

#[tokio::test(start_paused = true)]
async fn a_returning_machine_numbers_nothing_until_it_has_every_online_machines_settings() {
    let peer = MachineId::random();
    let fleet = Fleet::start_coordinated_with(1, &[peer]);
    let daemon = &fleet.daemons[0];
    let held = daemon.settings();
    let conn = drained(raw_link(daemon, peer).await);
    say(&conn, &heartbeat()).await;
    until(WAIT, "the peer is online", || {
        daemon.peers.online().contains(&peer)
    })
    .await;
    assert_eq!(daemon.coordinator().acting().await.unwrap(), daemon.id);

    let editing = tokio::spawn({
        let coordinator = daemon.coordinator();
        async move { coordinator.edit(set_project(7, "waited")).await }
    });
    // The peer is online and has sent no settings: the edit waits.
    for _ in 0..3 {
        say(&conn, &heartbeat()).await;
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert_eq!(daemon.settings(), held, "numbered before catching up");

    // The peer's copy holds an edit this machine has not seen; it is taken first.
    let mut theirs = held.clone();
    theirs.seq = 5;
    theirs.projects.push(project(8, "from the peer"));
    theirs.stamps.insert(
        SettingKey::Project(project(8, "").id),
        Stamp { seq: 5, by: peer },
    );
    theirs.seen.insert(peer, 5);
    say(
        &conn,
        &PeerMessage::Settings {
            protocol: PROTOCOL,
            settings: theirs,
        },
    )
    .await;
    tokio::time::timeout(Duration::from_secs(2), editing)
        .await
        .expect("numbered once caught up")
        .unwrap()
        .unwrap();
    let settings = daemon.settings();
    assert_eq!(project_named(&settings, 7).as_deref(), Some("waited"));
    assert_eq!(
        project_named(&settings, 8).as_deref(),
        Some("from the peer")
    );
    assert_eq!(
        settings.stamps[&SettingKey::Project(project(7, "").id)].seq,
        settings.seq
    );
    assert!(settings.seq > 5);
}

#[tokio::test(start_paused = true)]
async fn an_edit_that_waits_to_catch_up_is_numbered_after_the_offline_limit_not_dropped() {
    let peer = MachineId::random();
    let fleet = Fleet::start_coordinated_with(1, &[peer]);
    let daemon = &fleet.daemons[0];
    let conn = raw_link(daemon, peer).await;
    send(&conn, &heartbeat()).await;
    until(WAIT, "the peer is online", || {
        daemon.peers.online().contains(&peer)
    })
    .await;
    let _beating = beat(conn);

    let start = Instant::now();
    daemon
        .coordinator()
        .edit(set_project(7, "late"))
        .await
        .unwrap();
    let took = start.elapsed();
    assert!(
        daemon.peers.online().contains(&peer),
        "the peer stayed online"
    );
    assert!(
        took >= OFFLINE_AFTER - Duration::from_secs(1) && took <= OFFLINE_AFTER * 2,
        "numbered after {took:?}"
    );
    assert_eq!(
        project_named(&daemon.settings(), 7).as_deref(),
        Some("late")
    );
}

#[tokio::test(start_paused = true)]
async fn a_link_sends_the_settings_after_the_hello_and_again_when_they_change() {
    let peer = MachineId::random();
    let fleet = Fleet::start_coordinated_with(1, &[peer]);
    let daemon = &fleet.daemons[0];
    let mut conn = raw_link(daemon, peer).await;
    let first: serde_json::Value = serde_json::from_str(&conn.recv.recv().await.unwrap()).unwrap();
    assert_eq!(first["type"], "settings", "{first}");
    assert_eq!(first["settings"]["seq"], 1);

    // No coordinator reachable but this one: it acts, and numbers at once.
    daemon
        .coordinator()
        .edit(set_project(7, "p"))
        .await
        .unwrap();
    let sent = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            send(&conn, &heartbeat()).await;
            let text = conn.recv.recv().await.unwrap();
            let message: serde_json::Value = serde_json::from_str(&text).unwrap();
            if message["type"] == "settings" {
                return message;
            }
        }
    })
    .await
    .expect("the changed settings are sent");
    assert_eq!(sent["settings"]["seq"], 2);
}
