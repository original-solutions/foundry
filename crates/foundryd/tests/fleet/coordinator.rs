//! Fleet tests of the coordinator role moving between machines, and of shared settings edits:
//! numbered by the acting machine, forwarded to it, and merged when a split heals.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use foundry_proto::ids::{MachineId, ProjectId, WorkspaceId};
use foundry_proto::messages::{ANSWERS_SINCE, EDITS_SINCE, PROTOCOL, PeerMessage};
use foundry_proto::record::AnyRecord;
use foundry_proto::settings::{
    Edit, EditId, Limits, Outcome, Overwritten, Project, SettingKey, SettingValue, SharedSettings,
    Stamp, StampKey,
};
use foundry_proto::state::TerminalState;
use foundryd::coordinator::{Coordinator, CoordinatorError};
use foundryd::sync::{Conn, OFFLINE_AFTER, RETRY_REFUSED};
use foundryd::terminals::Terminals;
use futures::StreamExt as _;
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::*;

const WAIT: Duration = Duration::from_secs(60);

/// The spec's bound: the next machine in the order holds the role within 15 seconds of the
/// coordinator disappearing.
const ROLE_MOVES_WITHIN: Duration = Duration::from_secs(15);

/// How often a test looks again at who acts.
const POLL: Duration = Duration::from_millis(50);

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

/// Waits until `coordinator` names `want` as acting, checking every [`POLL`], and returns how
/// long that took. Fails when a check past `limit` still names another machine.
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
        tokio::time::sleep(POLL).await;
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
    let cut_at = Instant::now();
    let at_cut = ticks();
    let took = acting_becomes(&fleet.daemons[1].coordinator(), second, ROLE_MOVES_WITHIN).await;
    let left = ROLE_MOVES_WITHIN.saturating_sub(cut_at.elapsed());
    acting_becomes(&fleet.daemons[2].coordinator(), second, left).await;
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

/// The priority route, served from a machine that does not act, sets B first, and every
/// machine names B as acting within the spec's 15 seconds.
#[tokio::test]
async fn setting_b_first_from_the_priority_route_moves_the_role_to_b_in_time() {
    let fleet = Fleet::start_coordinated(3);
    let b = fleet.daemons[1].id;
    settled(&fleet, fleet.daemons[0].id).await;

    // Daemon 2 does not act, so the edit is forwarded.
    let daemon = &fleet.daemons[2];
    let app = foundryd::api::machine_priority::router(daemon.coordinator(), daemon.store.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let url = format!("http://{addr}/api/machines/priority");
    let body = serde_json::json!({ "order": [b.to_string()] });
    let sent = Instant::now();
    let (status, answer) = tokio::task::spawn_blocking(move || {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(WAIT))
            .build()
            .into();
        let mut response = agent.post(&url).send_json(body).unwrap();
        let status = response.status().as_u16();
        (status, response.body_mut().read_to_string().unwrap())
    })
    .await
    .unwrap();
    assert_eq!(status, 204, "{answer}");

    for daemon in &fleet.daemons {
        let left = ROLE_MOVES_WITHIN.saturating_sub(sent.elapsed());
        acting_becomes(&daemon.coordinator(), b, left).await;
    }
    server.abort();
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
    let returned = fleet.daemons[0]
        .coordinator()
        .edit(set_project(7, "p"))
        .await
        .unwrap();
    let settings = agreed(&fleet, &[0, 1, 2], "the edit reaches every machine").await;
    assert_eq!(project_named(&settings, 7).as_deref(), Some("p"));
    // Numbered by the acting machine, as the newest number every machine holds.
    let stamp = settings.stamps[&StampKey::Setting(SettingKey::Project(project(7, "p").id))];
    assert_eq!(returned, stamp);
    assert_eq!(stamp.by, first);
    assert_eq!(stamp.seq, settings.seq);
    assert_eq!(settings.seen, [(first, stamp.seq)].into());
}

#[tokio::test(start_paused = true)]
async fn an_edit_sent_to_a_machine_that_is_not_acting_is_forwarded() {
    let fleet = Fleet::start_coordinated(3);
    let first = fleet.daemons[0].id;
    settled(&fleet, first).await;
    // It returns once the acting machine has numbered it, with the stamp it was given.
    let returned = fleet.daemons[2]
        .coordinator()
        .edit(set_project(7, "from the last machine"))
        .await
        .unwrap();
    assert_eq!(returned.by, first);
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
        settings.stamps[&StampKey::Setting(SettingKey::Project(project(7, "p").id))],
        returned
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

/// The UI's add goes through the machine that serves it; the acting machine numbers it.
#[tokio::test]
async fn a_project_added_through_a_machine_that_is_not_acting_shows_on_the_acting_machine() {
    let fleet = Fleet::start_coordinated(2);
    let first = fleet.daemons[0].id;
    settled(&fleet, first).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/api/projects", listener.local_addr().unwrap());
    let app = foundryd::api::projects::router(
        fleet.daemons[1].coordinator(),
        fleet.daemons[1].store.clone(),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let (status, body) = tokio::task::spawn_blocking(move || {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(WAIT))
            .build()
            .into();
        let mut response = agent
            .post(&url)
            .send_json(serde_json::json!({"name": "foundry", "repo": "/srv/foundry.git"}))
            .unwrap();
        let status = response.status().as_u16();
        (status, response.body_mut().read_to_string().unwrap())
    })
    .await
    .unwrap();
    assert_eq!(status, 201, "{body}");
    let id: serde_json::Value = serde_json::from_str(&body).unwrap();
    let id = ProjectId(id["id"].as_str().unwrap().parse().unwrap());

    until(WAIT, "the project reaches the acting machine", || {
        fleet.daemons[0]
            .settings()
            .projects
            .iter()
            .any(|p| p.id == id && p.name == "foundry" && p.repo == "/srv/foundry.git")
    })
    .await;
    let settings = agreed(&fleet, &[0, 1], "both machines agree").await;
    assert_eq!(
        settings.stamps[&StampKey::Setting(SettingKey::Project(id))].by,
        first
    );
    server.abort();
}

#[tokio::test(start_paused = true)]
async fn an_edit_that_cannot_be_written_is_an_error_not_sent() {
    let fleet = Fleet::start_coordinated(2);
    settled(&fleet, fleet.daemons[0].id).await;
    let edit = Edit::SetLimits {
        machine: fleet.daemons[1].id,
        limits: Limits {
            max_sessions: 2,
            max_load_per_core: f64::NAN,
            min_free_disk: 1,
        },
    };
    let error = fleet.daemons[1].coordinator().edit(edit).await.unwrap_err();
    assert!(error.to_string().contains("not sent"), "{error}");
}

#[tokio::test(start_paused = true)]
async fn an_acting_machine_on_the_older_protocol_is_sent_no_edit_and_no_settings() {
    let older = MachineId::random();
    let fleet = Fleet::start_coordinated_with(1, &[older]);
    let daemon = &fleet.daemons[0];
    // The older machine is first in the order. It sends no settings, so this copy says so.
    let mut held = daemon.settings();
    held.seq += 1;
    held.priority = vec![older, daemon.id];
    lock(&daemon.store).apply_settings(held).unwrap();
    let (conn, said) = recorded(raw_link_speaking(daemon, older, EDITS_SINCE - 1).await);
    let _beating = beating(conn);
    acting_becomes(&daemon.coordinator(), older, WAIT).await;

    let error = daemon
        .coordinator()
        .edit(set_project(7, "p"))
        .await
        .unwrap_err();
    assert!(error.to_string().contains(&older.to_string()), "{error}");
    assert!(error.to_string().contains("protocol"), "{error}");
    tokio::time::sleep(Duration::from_secs(5)).await;
    let said = lock(&said);
    assert!(!said.is_empty(), "the link sent nothing");
    for message in said.iter() {
        assert_eq!(message["type"], "records", "{message}");
        assert_eq!(message["protocol"], EDITS_SINCE - 1, "{message}");
    }
}

#[tokio::test(start_paused = true)]
async fn an_acting_machine_numbers_at_once_with_a_peer_on_the_older_protocol_online() {
    let older = MachineId::random();
    let fleet = Fleet::start_coordinated_with(1, &[older]);
    let daemon = &fleet.daemons[0];
    let conn = drained(raw_link_speaking(daemon, older, EDITS_SINCE - 1).await);
    let _beating = beating(conn);
    until(WAIT, "the older peer is online", || {
        daemon.peers.online().contains(&older)
    })
    .await;
    assert_eq!(daemon.coordinator().acting().await.unwrap(), daemon.id);

    // The older peer never sends settings, so there is no copy of its to wait for.
    tokio::time::timeout(
        Duration::from_secs(2),
        daemon.coordinator().edit(set_project(7, "p")),
    )
    .await
    .expect("the edit waited for settings the older peer cannot send")
    .unwrap();
    assert_eq!(project_named(&daemon.settings(), 7).as_deref(), Some("p"));
}

/// A daemon that takes `older`, linked only in the older protocol, to act, and a peer on this
/// build that has sent its settings, both kept online by heartbeats. Returns the fleet, the
/// older machine's heartbeat, the end the test speaks on as the other peer, and what the daemon
/// sends to that peer.
async fn behind_an_older_acting_machine() -> (
    Fleet,
    tokio::task::JoinHandle<()>,
    mpsc::Sender<String>,
    Arc<Mutex<Vec<serde_json::Value>>>,
) {
    let (older, peer) = (MachineId::random(), MachineId::random());
    let fleet = Fleet::start_coordinated_with(1, &[older, peer]);
    let daemon = &fleet.daemons[0];
    let mut held = daemon.settings();
    held.seq += 1;
    held.priority = vec![older, daemon.id, peer];
    lock(&daemon.store).apply_settings(held).unwrap();
    let older_beat = beating(drained(
        raw_link_speaking(daemon, older, EDITS_SINCE - 1).await,
    ));
    let (conn, said) = recorded(raw_link(daemon, peer).await);
    say(&conn, &settings_message(daemon.settings())).await;
    beating(conn.clone());
    acting_becomes(&daemon.coordinator(), older, WAIT).await;
    (fleet, older_beat, conn, said)
}

#[tokio::test(start_paused = true)]
async fn a_forwarded_edit_the_older_acting_machine_cannot_take_waits_for_the_role_to_move() {
    let (fleet, older_beat, peer, _said) = behind_an_older_acting_machine().await;
    let daemon = &fleet.daemons[0];
    say(&peer, &edit_message(set_project(7, "kept"))).await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    // The older machine goes quiet, so the role moves here before the edit is given up.
    older_beat.abort();
    holds_project(daemon, 7, OFFLINE_AFTER + Duration::from_secs(5)).await;
}

#[tokio::test(start_paused = true)]
async fn a_forwarded_edit_still_unsendable_after_the_keep_limit_is_dropped_and_the_next_goes_on() {
    let (fleet, older_beat, peer, said) = behind_an_older_acting_machine().await;
    let daemon = &fleet.daemons[0];
    let start = Instant::now();
    let dropped = EditId::random();
    say(&peer, &edit_with(dropped, set_project(7, "dropped"))).await;
    tokio::time::sleep(Duration::from_secs(20)).await;
    say(&peer, &edit_message(set_project(8, "next"))).await;
    // The older machine is heard until now, so it acts past the first edit's keep limit
    // (twice OFFLINE_AFTER from its arrival), and the role moves only after that.
    older_beat.abort();
    let settings = holds_project(daemon, 8, OFFLINE_AFTER * 2).await;
    assert!(
        start.elapsed() > OFFLINE_AFTER * 2,
        "numbered before the older machine lost the role"
    );
    assert_eq!(project_named(&settings, 7), None, "kept past its limit");
    // The sender is told it was dropped, so it does not wait for a number that never comes.
    let answers = answers_sent(&said);
    let told = answers.iter().find(|(id, _)| *id == dropped);
    assert!(
        matches!(told, Some((_, Outcome::Refused { .. }))),
        "{answers:?}"
    );
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
    let cut_edit = Stamp {
        seq: fleet.daemons[0].settings().seq,
        by: fleet.daemons[0].id,
    };
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
                lost_stamp: cut_edit,
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
    raw_link_speaking(daemon, machine, PROTOCOL).await
}

/// As [`raw_link`], with the test's hello naming `protocol`.
async fn raw_link_speaking(daemon: &Daemon, machine: MachineId, protocol: u16) -> Conn {
    let (mut mine, theirs) = Conn::pair();
    let peers = daemon.peers.clone();
    tokio::spawn(async move { peers.accept(theirs).await });
    send(
        &mine,
        &PeerMessage::Hello {
            protocol,
            machine,
            versions: Default::default(),
            factory_versions: Default::default(),
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
/// daemon sends.
fn beat(conn: Conn) -> tokio::task::JoinHandle<()> {
    beating(drained(conn))
}

/// Sends a heartbeat on `to` every second until the returned task is aborted or the link
/// closes.
fn beating(to: mpsc::Sender<String>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let text = serde_json::to_string(&heartbeat()).unwrap();
        loop {
            if to.send(text.clone()).await.is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    })
}

/// Keeps every message the daemon sends on `conn`, and returns the end the test sends on.
fn recorded(conn: Conn) -> (mpsc::Sender<String>, Arc<Mutex<Vec<serde_json::Value>>>) {
    let Conn { send, mut recv } = conn;
    let said = Arc::new(Mutex::new(Vec::new()));
    tokio::spawn({
        let said = said.clone();
        async move {
            while let Some(text) = recv.recv().await {
                lock(&said).push(serde_json::from_str(&text).unwrap());
            }
        }
    });
    (send, said)
}

/// `held` as a peer that numbered one edit to the priority order, putting `order` first.
fn ordered_by(held: &SharedSettings, peer: MachineId, order: Vec<MachineId>) -> SharedSettings {
    let mut theirs = held.clone();
    theirs.seq += 5;
    theirs.priority = order;
    theirs.stamps.insert(
        StampKey::Setting(SettingKey::Priority),
        Stamp {
            seq: theirs.seq,
            by: peer,
        },
    );
    theirs.seen.insert(peer, theirs.seq);
    theirs
}

fn settings_message(settings: SharedSettings) -> PeerMessage {
    PeerMessage::Settings {
        protocol: PROTOCOL,
        settings,
    }
}

fn edit_message(edit: Edit) -> PeerMessage {
    edit_with(EditId::random(), edit)
}

fn edit_with(id: EditId, edit: Edit) -> PeerMessage {
    PeerMessage::Edit {
        protocol: PROTOCOL,
        id: Some(id),
        edit,
    }
}

fn answer(id: EditId, outcome: Outcome) -> PeerMessage {
    PeerMessage::Answer {
        protocol: PROTOCOL,
        id,
        outcome,
    }
}

/// The key of project `n`'s setting.
fn project_key(n: u128) -> StampKey {
    StampKey::Setting(SettingKey::Project(project(n, "").id))
}

/// Every edit the daemon sent in `said`, with its id, in order.
fn edits_sent(said: &Mutex<Vec<serde_json::Value>>) -> Vec<(Option<EditId>, Edit)> {
    lock(said)
        .iter()
        .filter(|m| m["type"] == "edit")
        .map(|m| match serde_json::from_value(m.clone()).unwrap() {
            PeerMessage::Edit { id, edit, .. } => (id, edit),
            other => panic!("not an edit: {other:?}"),
        })
        .collect()
}

/// Every answer the daemon sent in `said`, in order.
fn answers_sent(said: &Mutex<Vec<serde_json::Value>>) -> Vec<(EditId, Outcome)> {
    lock(said)
        .iter()
        .filter(|m| m["type"] == "answer")
        .map(|m| match serde_json::from_value(m.clone()).unwrap() {
            PeerMessage::Answer { id, outcome, .. } => (id, outcome),
            other => panic!("not an answer: {other:?}"),
        })
        .collect()
}

/// Waits until `daemon` holds project `n`, and returns its settings.
async fn holds_project(daemon: &Daemon, n: u128, within: Duration) -> SharedSettings {
    until(within, "the project is numbered", || {
        project_named(&daemon.settings(), n).is_some()
    })
    .await;
    daemon.settings()
}

#[tokio::test(start_paused = true)]
async fn an_edit_forwarded_by_the_machine_this_one_takes_to_act_waits_while_the_role_settles() {
    let peer = MachineId::random();
    let fleet = Fleet::start_coordinated_with(1, &[peer]);
    let daemon = &fleet.daemons[0];
    let (conn, said) = recorded(raw_link(daemon, peer).await);
    let theirs = ordered_by(&daemon.settings(), peer, vec![peer, daemon.id]);
    say(&conn, &settings_message(theirs.clone())).await;
    let _beating = beating(conn.clone());
    until(WAIT, "the peer is online", || {
        daemon.peers.online().contains(&peer)
    })
    .await;
    assert_eq!(daemon.coordinator().acting().await.unwrap(), peer);

    // The peer forwards an edit to it, though this machine takes the peer to act.
    say(&conn, &edit_message(set_project(7, "forwarded"))).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        project_named(&daemon.settings(), 7),
        None,
        "numbered while not acting"
    );

    // The peer's next copy hands the role to this machine, which then numbers the edit.
    let back = ordered_by(&theirs, peer, vec![daemon.id, peer]);
    say(&conn, &settings_message(back)).await;
    let settings = holds_project(daemon, 7, Duration::from_secs(2)).await;
    assert_eq!(
        settings.stamps[&StampKey::Setting(SettingKey::Project(project(7, "").id))].by,
        daemon.id
    );
    let sent_back = lock(&said).iter().any(|m| m["type"] == "edit");
    assert!(
        !sent_back,
        "the edit went back to the machine that forwarded it"
    );
}

#[tokio::test(start_paused = true)]
async fn forwarded_edits_wait_for_catch_up_without_stalling_the_link_and_keep_their_order() {
    let peer = MachineId::random();
    let fleet = Fleet::start_coordinated_with(1, &[peer]);
    let daemon = &fleet.daemons[0];
    let conn = drained(raw_link(daemon, peer).await);
    say(&conn, &heartbeat()).await;
    let _beating = beating(conn.clone());
    until(WAIT, "the peer is online", || {
        daemon.peers.online().contains(&peer)
    })
    .await;

    // Two edits to one setting arrive before the peer's copy does, half a look apart, so an
    // edit that looked for itself would see the copy first in the later one's turn.
    let me = daemon.id;
    say(&conn, &edit_message(set_limits(me, 4))).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    say(&conn, &edit_message(set_limits(me, 6))).await;
    tokio::time::sleep(Duration::from_millis(2960)).await;
    assert!(
        !daemon.settings().limits.contains_key(&me),
        "numbered before catching up"
    );

    // The link goes on reading, so the copy that lets them be numbered gets through.
    say(&conn, &settings_message(daemon.settings())).await;
    until(Duration::from_secs(2), "both edits are numbered", || {
        daemon.settings().seen.get(&me) == Some(&(daemon.settings().seq))
            && daemon.settings().limits.contains_key(&me)
    })
    .await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let settings = daemon.settings();
    assert_eq!(settings.limits[&me].max_sessions, 6, "the later edit holds");
    assert_eq!(
        settings.stamps[&StampKey::Setting(SettingKey::Limits(me))].seq,
        settings.seq
    );
}

#[tokio::test(start_paused = true)]
async fn a_restarted_machine_waits_for_a_listed_machines_settings_before_numbering() {
    let peer = MachineId::random();
    let fleet = Fleet::start_coordinated_with(1, &[peer]);
    let daemon = &fleet.daemons[0];
    // It hears from no one yet, so it acts, but the listed peer may hold newer settings.
    let editing = tokio::spawn({
        let coordinator = daemon.coordinator();
        async move { coordinator.edit(set_project(7, "after catching up")).await }
    });
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        project_named(&daemon.settings(), 7),
        None,
        "numbered on a stale copy"
    );

    let conn = drained(raw_link(daemon, peer).await);
    let mut theirs = daemon.settings();
    theirs.seq = 5;
    theirs.projects.push(project(8, "newer"));
    theirs.stamps.insert(
        StampKey::Setting(SettingKey::Project(project(8, "").id)),
        Stamp { seq: 5, by: peer },
    );
    theirs.seen.insert(peer, 5);
    say(&conn, &settings_message(theirs)).await;
    tokio::time::timeout(Duration::from_secs(2), editing)
        .await
        .expect("numbered once the listed peer's copy is in")
        .unwrap()
        .unwrap();
    let settings = daemon.settings();
    assert_eq!(project_named(&settings, 8).as_deref(), Some("newer"));
    assert!(
        settings.stamps[&StampKey::Setting(SettingKey::Project(project(7, "").id))].seq > 5,
        "{settings:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_forwarded_edit_outlives_the_acting_machines_closed_links() {
    let (old, sender) = (MachineId::random(), MachineId::random());
    let fleet = Fleet::start_coordinated_with(1, &[old, sender]);
    let daemon = &fleet.daemons[0];
    let theirs = ordered_by(&daemon.settings(), old, vec![old, daemon.id, sender]);
    let old_conn = drained(raw_link(daemon, old).await);
    let sender_conn = drained(raw_link(daemon, sender).await);
    say(&old_conn, &settings_message(theirs.clone())).await;
    say(&sender_conn, &settings_message(theirs)).await;
    let old_beat = beating(old_conn.clone());
    let _sender_beat = beating(sender_conn.clone());
    acting_becomes(&daemon.coordinator(), old, WAIT).await;

    // The old coordinator's links close; it stays online here until it has been silent for
    // the offline limit.
    old_beat.abort();
    drop(old_conn);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(daemon.coordinator().acting().await.unwrap(), old);
    say(&sender_conn, &edit_message(set_project(7, "outlived"))).await;

    let settings = holds_project(daemon, 7, OFFLINE_AFTER * 2).await;
    assert_eq!(project_named(&settings, 7).as_deref(), Some("outlived"));
    assert_eq!(
        settings.stamps[&StampKey::Setting(SettingKey::Project(project(7, "").id))].by,
        daemon.id
    );
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
        StampKey::Setting(SettingKey::Project(project(8, "").id)),
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
        settings.stamps[&StampKey::Setting(SettingKey::Project(project(7, "").id))].seq,
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

    // The peer's copy is in, so this machine, which acts, numbers at once.
    send(&conn, &settings_message(daemon.settings())).await;
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
                let settings: SharedSettings =
                    serde_json::from_value(message["settings"].clone()).unwrap();
                if project_named(&settings, 7).is_some() {
                    return settings;
                }
            }
        }
    })
    .await
    .expect("the changed settings are sent");
    assert_eq!(sent, daemon.settings());
}

#[tokio::test(start_paused = true)]
async fn an_edit_forwarded_by_the_machine_that_still_acts_once_the_role_settles_goes_back_to_it() {
    let peer = MachineId::random();
    let fleet = Fleet::start_coordinated_with(1, &[peer]);
    let daemon = &fleet.daemons[0];
    let (conn, said) = recorded(raw_link(daemon, peer).await);
    let theirs = ordered_by(&daemon.settings(), peer, vec![peer, daemon.id]);
    say(&conn, &settings_message(theirs)).await;
    let _beating = beating(conn.clone());
    acting_becomes(&daemon.coordinator(), peer, WAIT).await;

    // The peer forwarded it before it saw that it acts, and it goes on acting.
    let (id, edit) = (EditId::random(), set_project(7, "handed back"));
    say(&conn, &edit_with(id, edit.clone())).await;
    let start = Instant::now();
    until(OFFLINE_AFTER * 2, "the edit goes back to the peer", || {
        lock(&said).iter().any(|m| m["type"] == "edit")
    })
    .await;
    assert!(
        start.elapsed() >= OFFLINE_AFTER - POLL,
        "sent back before the role settled"
    );
    // It goes back under the id the peer gave it.
    assert_eq!(edits_sent(&said), [(Some(id), edit)]);
    assert_eq!(
        project_named(&daemon.settings(), 7),
        None,
        "numbered while not acting"
    );
}

#[tokio::test(start_paused = true)]
async fn forwarded_edits_that_wait_together_are_each_numbered_within_the_offline_limit() {
    let peer = MachineId::random();
    let fleet = Fleet::start_coordinated_with(1, &[peer]);
    let daemon = &fleet.daemons[0];
    let conn = drained(raw_link(daemon, peer).await);
    say(&conn, &heartbeat()).await;
    let _beating = beating(conn.clone());
    until(WAIT, "the peer is online", || {
        daemon.peers.online().contains(&peer)
    })
    .await;

    // The peer never sends its copy, so each edit waits out the offline limit.
    say(&conn, &edit_message(set_limits(daemon.id, 4))).await;
    say(&conn, &edit_message(set_project(7, "second"))).await;
    tokio::time::sleep(OFFLINE_AFTER + Duration::from_secs(1)).await;
    let settings = daemon.settings();
    assert_eq!(settings.limits[&daemon.id].max_sessions, 4);
    assert_eq!(project_named(&settings, 7).as_deref(), Some("second"));
}

#[tokio::test(start_paused = true)]
async fn a_machine_running_past_the_offline_limit_numbers_at_once_though_a_listed_machine_never_came()
 {
    let peer = MachineId::random();
    let fleet = Fleet::start_coordinated_with(1, &[peer]);
    let daemon = &fleet.daemons[0];
    tokio::time::sleep(OFFLINE_AFTER + Duration::from_secs(1)).await;
    let start = Instant::now();
    daemon
        .coordinator()
        .edit(set_project(7, "p"))
        .await
        .unwrap();
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "waited {:?}",
        start.elapsed()
    );
    let settings = daemon.settings();
    assert_eq!(
        settings.stamps[&StampKey::Setting(SettingKey::Project(project(7, "").id))].by,
        daemon.id
    );
}

#[tokio::test(start_paused = true)]
async fn an_edit_on_a_link_that_refused_a_message_is_still_taken() {
    let peer = MachineId::random();
    let fleet = Fleet::start_coordinated_with(1, &[peer]);
    let daemon = &fleet.daemons[0];
    let conn = drained(raw_link(daemon, peer).await);
    say(&conn, &settings_message(daemon.settings())).await;
    until(WAIT, "the peer is online", || {
        daemon.peers.online().contains(&peer)
    })
    .await;
    // A message this machine cannot read: the link takes no more records from the peer.
    conn.send("not a message".to_owned()).await.unwrap();
    say(&conn, &edit_message(set_project(7, "after a refusal"))).await;
    let settings = holds_project(daemon, 7, Duration::from_secs(2)).await;
    assert_eq!(
        project_named(&settings, 7).as_deref(),
        Some("after a refusal")
    );
}

#[tokio::test(start_paused = true)]
async fn a_link_that_refused_a_message_takes_settings_and_the_edit_that_arrives_as_it_closes() {
    let peer = MachineId::random();
    let fleet = Fleet::start_coordinated_with(1, &[peer]);
    let daemon = &fleet.daemons[0];
    let conn = drained(raw_link(daemon, peer).await);
    say(&conn, &settings_message(daemon.settings())).await;
    until(WAIT, "the peer is online", || {
        daemon.peers.online().contains(&peer)
    })
    .await;
    conn.send("not a message".to_owned()).await.unwrap();

    // A copy that holds an edit the peer numbered is still merged.
    let mut theirs = daemon.settings();
    theirs.seq += 5;
    theirs.projects.push(project(8, "from the peer"));
    theirs.stamps.insert(
        StampKey::Setting(SettingKey::Project(project(8, "").id)),
        Stamp {
            seq: theirs.seq,
            by: peer,
        },
    );
    theirs.seen.insert(peer, theirs.seq);
    say(&conn, &settings_message(theirs)).await;
    holds_project(daemon, 8, Duration::from_secs(2)).await;

    // The first message past the retry limit closes the link, and is an edit: it is taken.
    let beat = beating(conn.clone());
    tokio::time::sleep(RETRY_REFUSED - Duration::from_millis(500)).await;
    beat.abort();
    tokio::time::sleep(Duration::from_secs(1)).await;
    say(&conn, &edit_message(set_project(7, "as it closes"))).await;
    let settings = holds_project(daemon, 7, Duration::from_secs(2)).await;
    assert_eq!(project_named(&settings, 7).as_deref(), Some("as it closes"));
}

#[tokio::test(start_paused = true)]
async fn an_edit_forwarded_by_the_machine_that_acts_goes_back_once_its_next_copy_arrives() {
    let peer = MachineId::random();
    let fleet = Fleet::start_coordinated_with(1, &[peer]);
    let daemon = &fleet.daemons[0];
    let (conn, said) = recorded(raw_link(daemon, peer).await);
    let theirs = ordered_by(&daemon.settings(), peer, vec![peer, daemon.id]);
    say(&conn, &settings_message(theirs.clone())).await;
    let _beating = beating(conn.clone());
    acting_becomes(&daemon.coordinator(), peer, WAIT).await;

    let edit = set_project(7, "handed back");
    say(&conn, &edit_message(edit.clone())).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        !lock(&said).iter().any(|m| m["type"] == "edit"),
        "sent back before the peer's views were known"
    );
    // The peer's next copy, taken after the edit, shows it still acts.
    let mut next = theirs;
    next.seq += 1;
    say(&conn, &settings_message(next)).await;
    until(
        Duration::from_secs(2),
        "the edit goes back to the peer",
        || lock(&said).iter().any(|m| m["type"] == "edit"),
    )
    .await;
    assert_eq!(
        project_named(&daemon.settings(), 7),
        None,
        "numbered while not acting"
    );
}

// --- answers ---

/// A daemon that acts, and a peer on this build that has sent its settings and keeps sending
/// heartbeats. Returns the fleet, the end the test speaks on as the peer, and what the daemon
/// sends to the peer.
async fn acting_with_a_peer() -> (
    Fleet,
    MachineId,
    mpsc::Sender<String>,
    Arc<Mutex<Vec<serde_json::Value>>>,
) {
    let peer = MachineId::random();
    let fleet = Fleet::start_coordinated_with(1, &[peer]);
    let daemon = &fleet.daemons[0];
    let (conn, said) = recorded(raw_link(daemon, peer).await);
    say(&conn, &settings_message(daemon.settings())).await;
    beating(conn.clone());
    until(WAIT, "the peer is online", || {
        daemon.peers.online().contains(&peer)
    })
    .await;
    assert_eq!(daemon.coordinator().acting().await.unwrap(), daemon.id);
    (fleet, peer, conn, said)
}

/// Gives the acting `daemon` project `n` named `name`, as an add it numbered would.
fn holding_project(daemon: &Daemon, n: u128, name: &str) {
    let mut settings = daemon.settings();
    settings.seq += 1;
    settings.projects.push(project(n, name));
    settings.stamps.insert(
        project_key(n),
        Stamp {
            seq: settings.seq,
            by: daemon.id,
        },
    );
    settings.seen.insert(daemon.id, settings.seq);
    daemon.set_settings(settings);
}

/// The one answer the daemon sent, once it has sent one.
async fn the_answer(said: &Mutex<Vec<serde_json::Value>>) -> (EditId, Outcome) {
    until(Duration::from_secs(2), "the edit is answered", || {
        !answers_sent(said).is_empty()
    })
    .await;
    let answers = answers_sent(said);
    assert_eq!(answers.len(), 1, "{answers:?}");
    answers[0].clone()
}

#[tokio::test(start_paused = true)]
async fn an_add_of_a_held_name_forwarded_by_a_machine_that_has_not_heard_of_it_is_refused() {
    let (fleet, _, conn, said) = acting_with_a_peer().await;
    let daemon = &fleet.daemons[0];
    holding_project(daemon, 1, "Foundry");
    let before = daemon.settings();
    // The peer's copy predates the first add, and it sends one of its own under that name.
    let id = EditId::random();
    say(&conn, &edit_with(id, set_project(2, " fOUNDRY "))).await;
    let (answered, outcome) = the_answer(&said).await;
    assert_eq!(answered, id);
    let Outcome::Refused { reason, conflict } = outcome else {
        panic!("not refused: {outcome:?}");
    };
    assert!(conflict, "{reason}");
    for value in ["oundry", "OUNDRY", "example.invalid"] {
        assert!(!reason.contains(value), "{value} in {reason}");
    }
    assert_eq!(daemon.settings(), before);
}

#[tokio::test(start_paused = true)]
async fn a_remove_forwarded_while_the_acting_machine_holds_a_live_workspace_of_it_is_refused() {
    let (fleet, _, conn, said) = acting_with_a_peer().await;
    let daemon = &fleet.daemons[0];
    holding_project(daemon, 1, "foundry");
    // A workspace of project 1, made on the acting machine, whose records the peer lacks.
    daemon.write(workspace(WorkspaceId::random(), false));
    let id = EditId::random();
    let remove = Edit::RemoveProject {
        id: project(1, "").id,
    };
    say(&conn, &edit_with(id, remove)).await;
    let (answered, outcome) = the_answer(&said).await;
    assert_eq!(answered, id);
    let Outcome::Refused { reason, conflict } = outcome else {
        panic!("not refused: {outcome:?}");
    };
    assert!(conflict, "{reason}");
    for value in ["foundry", "fix login", "example.invalid"] {
        assert!(!reason.contains(value), "{value} in {reason}");
    }
    assert_eq!(
        project_named(&daemon.settings(), 1).as_deref(),
        Some("foundry")
    );
}

#[tokio::test(start_paused = true)]
async fn the_acting_machine_answers_a_forwarded_edit_with_its_id_and_stamp() {
    let (fleet, _, conn, said) = acting_with_a_peer().await;
    let daemon = &fleet.daemons[0];
    let id = EditId::random();
    say(&conn, &edit_with(id, set_project(7, "answered"))).await;
    until(Duration::from_secs(2), "the edit is answered", || {
        !answers_sent(&said).is_empty()
    })
    .await;
    let settings = daemon.settings();
    let stamp = settings.stamps[&project_key(7)];
    assert_eq!(stamp.by, daemon.id);
    assert_eq!(answers_sent(&said), [(id, Outcome::Numbered { stamp })]);
    assert_eq!(settings.numbered[&id], stamp);
}

#[tokio::test(start_paused = true)]
async fn an_edit_sent_again_after_its_answer_was_lost_is_numbered_once_and_answered_again() {
    let (fleet, _, conn, said) = acting_with_a_peer().await;
    let daemon = &fleet.daemons[0];
    let id = EditId::random();
    let edit = edit_with(id, set_project(7, "once"));
    say(&conn, &edit).await;
    until(Duration::from_secs(2), "the edit is answered", || {
        answers_sent(&said).len() == 1
    })
    .await;
    let numbered = daemon.settings();

    say(&conn, &edit).await;
    until(
        Duration::from_secs(2),
        "the edit sent again is answered",
        || answers_sent(&said).len() == 2,
    )
    .await;
    let answers = answers_sent(&said);
    assert_eq!(answers[0], answers[1]);
    assert_eq!(daemon.settings(), numbered, "numbered twice");
}

#[tokio::test(start_paused = true)]
async fn a_store_error_on_the_acting_machine_reports_the_edit_refused_to_the_forwarding_machine() {
    let fleet = Fleet::start_coordinated(2);
    let first = fleet.daemons[0].id;
    settled(&fleet, first).await;
    // The acting machine's store refuses every write to the settings from now on.
    let _refusing = refuse_settings_writes(&fleet.daemons[0].db);

    let error = fleet.daemons[1]
        .coordinator()
        .edit(set_project(7, "secret-name"))
        .await
        .unwrap_err();
    assert!(matches!(error, CoordinatorError::Refused { .. }), "{error}");
    let text = error.to_string();
    assert!(text.contains(&first.to_string()), "{text}");
    // The reason names no setting's value, and nothing the store said.
    for value in ["secret-name", "example.invalid", "disk full"] {
        assert!(!text.contains(value), "{value} in {text}");
    }
    assert_eq!(project_named(&fleet.daemons[1].settings(), 7), None);
}

#[tokio::test(start_paused = true)]
async fn the_acting_machine_restarting_while_a_forwarded_edit_waits_to_catch_up_still_numbers_it() {
    let quiet = MachineId::random();
    let mut fleet = Fleet::start_coordinated_with(2, &[quiet]);
    let (first, second) = (fleet.daemons[0].id, fleet.daemons[1].id);
    // `quiet` is online at the acting machine and never sends its settings, so an edit waits
    // there to catch up.
    let quiet_beat = beat(raw_link(&fleet.daemons[0], quiet).await);
    until(WAIT, "every machine is heard", || {
        let (zero, one) = (
            fleet.daemons[0].peers.online(),
            fleet.daemons[1].peers.online(),
        );
        zero.contains(&second) && zero.contains(&quiet) && one.contains(&first)
    })
    .await;
    acting_becomes(&fleet.daemons[1].coordinator(), first, WAIT).await;
    let before = fleet.daemons[1].settings().seq;

    let editing = tokio::spawn({
        let coordinator = fleet.daemons[1].coordinator();
        async move { coordinator.edit(set_project(7, "survived")).await }
    });
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        project_named(&fleet.daemons[0].settings(), 7),
        None,
        "numbered before catching up"
    );

    // The acting machine restarts, and forgets the edit it held.
    quiet_beat.abort();
    fleet.restart(0).await;

    let stamp = tokio::time::timeout(OFFLINE_AFTER * 2, editing)
        .await
        .expect("an outcome within the keep limit")
        .unwrap()
        .expect("numbered");
    let settings = agreed(&fleet, &[0, 1], "the edit reaches both machines").await;
    assert_eq!(project_named(&settings, 7).as_deref(), Some("survived"));
    assert_eq!(settings.stamps[&project_key(7)], stamp);
    assert_eq!(
        stamp,
        Stamp {
            seq: before + 1,
            by: first
        },
        "numbered more than once"
    );
}

#[tokio::test(start_paused = true)]
async fn a_link_closed_with_the_edit_still_queued_leads_to_a_resend_that_numbers_it_once() {
    let fleet = Fleet::start_coordinated(2);
    let first = fleet.daemons[0].id;
    settled(&fleet, first).await;
    let before = fleet.daemons[1].settings().seq;

    // The acting machine's links close. Until it is silent for the offline limit it still
    // acts, and the only link to it is one the test holds, which takes the edit and never
    // passes it on.
    fleet.cut(0);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (conn, said) = recorded(raw_link(&fleet.daemons[1], first).await);
    let held_beat = beating(conn.clone());
    let editing = tokio::spawn({
        let coordinator = fleet.daemons[1].coordinator();
        async move { coordinator.edit(set_project(7, "resent")).await }
    });
    until(Duration::from_secs(2), "the edit is queued", || {
        !edits_sent(&said).is_empty()
    })
    .await;
    let (id, _) = edits_sent(&said)[0].clone();

    // That link closes with the edit unread, and the acting machine links again.
    held_beat.abort();
    drop(conn);
    fleet.heal(0);

    let stamp = tokio::time::timeout(OFFLINE_AFTER * 2, editing)
        .await
        .expect("an outcome within the keep limit")
        .unwrap()
        .expect("numbered");
    let settings = agreed(&fleet, &[0, 1], "the edit reaches both machines").await;
    assert_eq!(project_named(&settings, 7).as_deref(), Some("resent"));
    assert_eq!(
        stamp,
        Stamp {
            seq: before + 1,
            by: first
        },
        "numbered more than once"
    );
    assert_eq!(settings.numbered[&id.unwrap()], stamp);
}

/// A daemon on which `peer`, on this build, acts and has sent its settings, and the end the
/// test speaks on as `peer`, with what the daemon sends it.
async fn behind_a_peer_that_acts() -> (
    Fleet,
    MachineId,
    SharedSettings,
    mpsc::Sender<String>,
    Arc<Mutex<Vec<serde_json::Value>>>,
) {
    let peer = MachineId::random();
    let fleet = Fleet::start_coordinated_with(1, &[peer]);
    let daemon = &fleet.daemons[0];
    let (conn, said) = recorded(raw_link(daemon, peer).await);
    let theirs = ordered_by(&daemon.settings(), peer, vec![peer, daemon.id]);
    say(&conn, &settings_message(theirs.clone())).await;
    beating(conn.clone());
    acting_becomes(&daemon.coordinator(), peer, WAIT).await;
    (fleet, peer, theirs, conn, said)
}

#[tokio::test(start_paused = true)]
async fn an_edit_with_neither_an_answer_nor_its_stamp_is_a_no_answer_error_after_the_keep_limit() {
    let (fleet, peer, _, _conn, said) = behind_a_peer_that_acts().await;
    let start = Instant::now();
    let error = fleet.daemons[0]
        .coordinator()
        .edit(set_project(7, "unanswered"))
        .await
        .unwrap_err();
    let took = start.elapsed();
    assert!(
        matches!(error, CoordinatorError::NoAnswer { .. }),
        "{error}"
    );
    assert!(error.to_string().contains(&peer.to_string()), "{error}");
    assert!(
        took >= OFFLINE_AFTER * 2 - POLL && took <= OFFLINE_AFTER * 2 + Duration::from_secs(1),
        "gave up after {took:?}"
    );
    // The link stayed open and the peer went on acting, so it was sent once.
    assert_eq!(edits_sent(&said).len(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_copy_of_the_settings_holding_the_edits_stamp_settles_it_with_no_answer() {
    let (fleet, peer, theirs, conn, said) = behind_a_peer_that_acts().await;
    let editing = tokio::spawn({
        let coordinator = fleet.daemons[0].coordinator();
        async move { coordinator.edit(set_project(7, "settled by a copy")).await }
    });
    until(Duration::from_secs(2), "the edit is sent", || {
        !edits_sent(&said).is_empty()
    })
    .await;
    let (id, edit) = edits_sent(&said)[0].clone();

    // The peer numbers it and sends its copy, but no answer.
    let mut next = theirs;
    next.seq += 1;
    let stamp = Stamp {
        seq: next.seq,
        by: peer,
    };
    let Edit::SetProject { project } = edit else {
        panic!("{edit:?}")
    };
    next.projects.push(project);
    next.stamps.insert(project_key(7), stamp);
    next.seen.insert(peer, next.seq);
    next.numbered.insert(id.unwrap(), stamp);
    say(&conn, &settings_message(next)).await;

    let returned = tokio::time::timeout(Duration::from_secs(2), editing)
        .await
        .expect("settled once the copy is in")
        .unwrap()
        .unwrap();
    assert_eq!(returned, stamp);
}

#[tokio::test(start_paused = true)]
async fn an_edit_the_acting_machine_refuses_returns_its_reason() {
    let (fleet, _, _, conn, said) = behind_a_peer_that_acts().await;
    let editing = tokio::spawn({
        let coordinator = fleet.daemons[0].coordinator();
        async move { coordinator.edit(set_project(7, "refused")).await }
    });
    until(Duration::from_secs(2), "the edit is sent", || {
        !edits_sent(&said).is_empty()
    })
    .await;
    let (id, _) = edits_sent(&said)[0].clone();
    let reason = "it could not be stored".to_owned();
    say(
        &conn,
        &answer(
            id.unwrap(),
            Outcome::Refused {
                reason: reason.clone(),
                conflict: false,
            },
        ),
    )
    .await;
    let error = tokio::time::timeout(Duration::from_secs(2), editing)
        .await
        .expect("an outcome once answered")
        .unwrap()
        .unwrap_err();
    match &error {
        CoordinatorError::Refused { reason: given } => assert_eq!(given, &reason),
        other => panic!("{other}"),
    }
}

#[tokio::test(start_paused = true)]
async fn when_the_role_moves_before_an_answer_the_edit_goes_again_to_the_machine_that_then_acts() {
    let (p1, p2) = (MachineId::random(), MachineId::random());
    let fleet = Fleet::start_coordinated_with(1, &[p1, p2]);
    let daemon = &fleet.daemons[0];
    let theirs = ordered_by(&daemon.settings(), p1, vec![p1, p2, daemon.id]);
    let (c1, said1) = recorded(raw_link(daemon, p1).await);
    let (c2, said2) = recorded(raw_link(daemon, p2).await);
    say(&c1, &settings_message(theirs.clone())).await;
    say(&c2, &settings_message(theirs)).await;
    let beat1 = beating(c1.clone());
    let _beat2 = beating(c2.clone());
    acting_becomes(&daemon.coordinator(), p1, WAIT).await;

    let editing = tokio::spawn({
        let coordinator = daemon.coordinator();
        async move { coordinator.edit(set_project(7, "moved")).await }
    });
    until(Duration::from_secs(2), "the edit goes to the first", || {
        !edits_sent(&said1).is_empty()
    })
    .await;
    let (id, edit) = edits_sent(&said1)[0].clone();

    // The first goes quiet, so the role moves to the second, which is sent the same edit.
    beat1.abort();
    until(OFFLINE_AFTER * 2, "the edit goes to the second", || {
        !edits_sent(&said2).is_empty()
    })
    .await;
    assert_eq!(edits_sent(&said2), [(id, edit)]);
    let stamp = Stamp { seq: 10, by: p2 };
    say(&c2, &answer(id.unwrap(), Outcome::Numbered { stamp })).await;
    let returned = tokio::time::timeout(Duration::from_secs(2), editing)
        .await
        .expect("an outcome once answered")
        .unwrap()
        .unwrap();
    assert_eq!(returned, stamp);
}

#[tokio::test(start_paused = true)]
async fn a_machine_that_passes_a_forwarded_edit_on_passes_its_answer_back() {
    let (acting, sender) = (MachineId::random(), MachineId::random());
    let fleet = Fleet::start_coordinated_with(1, &[acting, sender]);
    let daemon = &fleet.daemons[0];
    let theirs = ordered_by(&daemon.settings(), acting, vec![acting, daemon.id, sender]);
    let (ca, said_a) = recorded(raw_link(daemon, acting).await);
    let (cs, said_s) = recorded(raw_link(daemon, sender).await);
    say(&ca, &settings_message(theirs.clone())).await;
    say(&cs, &settings_message(theirs)).await;
    let _beat_a = beating(ca.clone());
    let _beat_s = beating(cs.clone());
    acting_becomes(&daemon.coordinator(), acting, WAIT).await;

    let id = EditId::random();
    say(&cs, &edit_with(id, set_project(7, "passed on"))).await;
    until(Duration::from_secs(2), "the edit is passed on", || {
        !edits_sent(&said_a).is_empty()
    })
    .await;
    assert_eq!(edits_sent(&said_a)[0].0, Some(id));
    let refused = Outcome::Refused {
        reason: format!("{acting} could not store it"),
        conflict: false,
    };
    say(&ca, &answer(id, refused.clone())).await;
    until(Duration::from_secs(2), "the answer is passed back", || {
        !answers_sent(&said_s).is_empty()
    })
    .await;
    assert_eq!(answers_sent(&said_s), [(id, refused)]);
}

#[tokio::test(start_paused = true)]
async fn an_acting_machine_on_the_protocol_before_answers_is_sent_no_edit() {
    let older = MachineId::random();
    let fleet = Fleet::start_coordinated_with(1, &[older]);
    let daemon = &fleet.daemons[0];
    let (conn, said) = recorded(raw_link_speaking(daemon, older, ANSWERS_SINCE - 1).await);
    let theirs = ordered_by(&daemon.settings(), older, vec![older, daemon.id]);
    say(&conn, &settings_message(theirs)).await;
    let _beating = beating(conn.clone());
    acting_becomes(&daemon.coordinator(), older, WAIT).await;

    let error = daemon
        .coordinator()
        .edit(set_project(7, "p"))
        .await
        .unwrap_err();
    assert!(error.to_string().contains(&older.to_string()), "{error}");
    assert!(error.to_string().contains("protocol"), "{error}");
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(edits_sent(&said).is_empty());
}

#[tokio::test(start_paused = true)]
async fn an_edit_forwarded_on_a_link_before_answers_is_numbered_and_never_answered() {
    let older = MachineId::random();
    let fleet = Fleet::start_coordinated_with(1, &[older]);
    let daemon = &fleet.daemons[0];
    let (conn, said) = recorded(raw_link_speaking(daemon, older, ANSWERS_SINCE - 1).await);
    say(&conn, &settings_message(daemon.settings())).await;
    let _beating = beating(conn.clone());
    until(WAIT, "the peer is online", || {
        daemon.peers.online().contains(&older)
    })
    .await;
    say(
        &conn,
        &PeerMessage::Edit {
            protocol: ANSWERS_SINCE - 1,
            id: None,
            edit: set_project(7, "from an older machine"),
        },
    )
    .await;
    holds_project(daemon, 7, Duration::from_secs(2)).await;
    // An edit that carries an id on such a link is numbered and not answered either: the link
    // is too old to carry an answer.
    say(
        &conn,
        &PeerMessage::Edit {
            protocol: ANSWERS_SINCE - 1,
            id: Some(EditId::random()),
            edit: set_project(8, "from an older machine, with an id"),
        },
    )
    .await;
    holds_project(daemon, 8, Duration::from_secs(2)).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(answers_sent(&said).is_empty());
}

// --- forwarded edits: refusals, copies, ids and read errors ---

#[tokio::test(start_paused = true)]
async fn a_peer_that_cannot_carry_numbered_ids_does_not_keep_raising_the_number() {
    let peer = MachineId::random();
    let fleet = Fleet::start_coordinated_with(1, &[peer]);
    let daemon = &fleet.daemons[0];
    let Conn { send, mut recv } = raw_link_speaking(daemon, peer, ANSWERS_SINCE - 1).await;
    // A machine before answers sends back every copy it is sent, without what it cannot hold.
    tokio::spawn({
        let send = send.clone();
        async move {
            while let Some(text) = recv.recv().await {
                let mut json: serde_json::Value = serde_json::from_str(&text).unwrap();
                if json["type"] == "settings" {
                    json["settings"].as_object_mut().unwrap().remove("numbered");
                    if send.send(json.to_string()).await.is_err() {
                        return;
                    }
                }
            }
        }
    });
    say(&send, &settings_message(daemon.settings())).await;
    let _beating = beating(send.clone());
    until(WAIT, "the peer is online", || {
        daemon.peers.online().contains(&peer)
    })
    .await;
    assert_eq!(daemon.coordinator().acting().await.unwrap(), daemon.id);

    daemon
        .coordinator()
        .edit(set_project(7, "numbered"))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_secs(5)).await;
    let seq = daemon.settings().seq;
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert_eq!(daemon.settings().seq, seq, "the number keeps rising");
}

#[tokio::test(start_paused = true)]
async fn a_forwarded_edit_sent_twice_while_it_waits_to_catch_up_is_answered_once() {
    let (peer, quiet) = (MachineId::random(), MachineId::random());
    let fleet = Fleet::start_coordinated_with(1, &[peer, quiet]);
    let daemon = &fleet.daemons[0];
    let (conn, said) = recorded(raw_link(daemon, peer).await);
    say(&conn, &settings_message(daemon.settings())).await;
    let _beating = beating(conn.clone());
    // `quiet` is online and never sends its settings, so an edit waits to catch up.
    let _quiet = beat(raw_link(daemon, quiet).await);
    until(WAIT, "both peers are online", || {
        let online = daemon.peers.online();
        online.contains(&peer) && online.contains(&quiet)
    })
    .await;
    let _refusing = refuse_settings_writes(&daemon.db);

    let edit = edit_with(EditId::random(), set_project(7, "twice"));
    say(&conn, &edit).await;
    say(&conn, &edit).await;
    tokio::time::sleep(OFFLINE_AFTER + Duration::from_secs(10)).await;
    let answers = answers_sent(&said);
    assert_eq!(answers.len(), 1, "{answers:?}");
    assert!(
        matches!(answers[0].1, Outcome::Refused { .. }),
        "{answers:?}"
    );
    assert_eq!(project_named(&daemon.settings(), 7), None);
}

#[tokio::test(start_paused = true)]
async fn a_read_error_after_the_edit_was_sent_does_not_end_the_wait() {
    let (fleet, _, _, conn, said) = behind_a_peer_that_acts().await;
    let daemon = &fleet.daemons[0];
    let editing = tokio::spawn({
        let coordinator = daemon.coordinator();
        async move { coordinator.edit(set_project(7, "read later")).await }
    });
    until(Duration::from_secs(2), "the edit is sent", || {
        !edits_sent(&said).is_empty()
    })
    .await;
    let (id, _) = edits_sent(&said)[0].clone();

    // The settings cannot be read for a moment, then can.
    let db = rusqlite::Connection::open(&daemon.db).unwrap();
    let held: String = db
        .query_row("SELECT settings FROM settings", [], |row| row.get(0))
        .unwrap();
    db.execute("UPDATE settings SET settings = 'unreadable'", [])
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    db.execute("UPDATE settings SET settings = ?1", [held])
        .unwrap();

    let stamp = Stamp {
        seq: 99,
        by: MachineId::random(),
    };
    say(&conn, &answer(id.unwrap(), Outcome::Numbered { stamp })).await;
    let returned = tokio::time::timeout(Duration::from_secs(2), editing)
        .await
        .expect("an outcome once answered")
        .unwrap()
        .expect("a read error is not a refusal");
    assert_eq!(returned, stamp);
}

#[tokio::test(start_paused = true)]
async fn an_edit_without_an_id_goes_on_to_an_acting_machine_before_answers() {
    let (acting, forwarder) = (MachineId::random(), MachineId::random());
    let fleet = Fleet::start_coordinated_with(1, &[acting, forwarder]);
    let daemon = &fleet.daemons[0];
    let theirs = ordered_by(
        &daemon.settings(),
        acting,
        vec![acting, daemon.id, forwarder],
    );
    let (ca, said_a) = recorded(raw_link_speaking(daemon, acting, ANSWERS_SINCE - 1).await);
    let cf = drained(raw_link_speaking(daemon, forwarder, ANSWERS_SINCE - 1).await);
    say(&ca, &settings_message(theirs.clone())).await;
    say(&cf, &settings_message(theirs)).await;
    let _beat_a = beating(ca.clone());
    let _beat_f = beating(cf.clone());
    acting_becomes(&daemon.coordinator(), acting, WAIT).await;

    say(
        &cf,
        &PeerMessage::Edit {
            protocol: ANSWERS_SINCE - 1,
            id: None,
            edit: set_project(7, "no id"),
        },
    )
    .await;
    until(Duration::from_secs(2), "the edit is passed on", || {
        lock(&said_a).iter().any(|m| m["type"] == "edit")
    })
    .await;
    let sent = lock(&said_a)
        .iter()
        .find(|m| m["type"] == "edit")
        .cloned()
        .unwrap();
    assert!(sent.get("id").is_none(), "{sent}");
    assert_eq!(sent["protocol"], ANSWERS_SINCE - 1);

    // It is passed on once: the acting machine's link closes and a new one opens, and nothing
    // is sent again, as nobody would answer it.
    _beat_a.abort();
    drop(ca);
    tokio::time::sleep(Duration::from_secs(2)).await;
    let (ca, said_again) = recorded(raw_link_speaking(daemon, acting, ANSWERS_SINCE - 1).await);
    say(&ca, &settings_message(daemon.settings())).await;
    let _beat_again = beating(ca.clone());
    tokio::time::sleep(Duration::from_secs(5)).await;
    let count = |said: &Mutex<Vec<serde_json::Value>>| {
        lock(said).iter().filter(|m| m["type"] == "edit").count()
    };
    assert_eq!(count(&said_a) + count(&said_again), 1);
}

#[tokio::test(start_paused = true)]
async fn a_forwarded_edit_waiting_for_its_answer_does_not_hold_back_the_next() {
    let (acting, sender) = (MachineId::random(), MachineId::random());
    let fleet = Fleet::start_coordinated_with(1, &[acting, sender]);
    let daemon = &fleet.daemons[0];
    let theirs = ordered_by(&daemon.settings(), acting, vec![acting, daemon.id, sender]);
    let (ca, said_a) = recorded(raw_link(daemon, acting).await);
    let cs = drained(raw_link(daemon, sender).await);
    say(&ca, &settings_message(theirs.clone())).await;
    say(&cs, &settings_message(theirs)).await;
    let _beat_a = beating(ca.clone());
    let _beat_s = beating(cs.clone());
    acting_becomes(&daemon.coordinator(), acting, WAIT).await;

    // The acting machine never answers the first.
    say(&cs, &edit_message(set_project(7, "first"))).await;
    say(&cs, &edit_message(set_project(8, "second"))).await;
    until(Duration::from_secs(2), "both are passed on", || {
        edits_sent(&said_a).len() == 2
    })
    .await;
    let names: Vec<_> = edits_sent(&said_a)
        .into_iter()
        .map(|(_, edit)| match edit {
            Edit::SetProject { project } => project.name,
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(names, ["first", "second"], "in the order they arrived");
}

#[tokio::test(start_paused = true)]
async fn an_answer_on_a_link_that_refused_a_message_is_still_taken() {
    let (fleet, _, _, conn, said) = behind_a_peer_that_acts().await;
    let editing = tokio::spawn({
        let coordinator = fleet.daemons[0].coordinator();
        async move { coordinator.edit(set_project(7, "refused link")).await }
    });
    until(Duration::from_secs(2), "the edit is sent", || {
        !edits_sent(&said).is_empty()
    })
    .await;
    let (id, _) = edits_sent(&said)[0].clone();
    conn.send("not a message".to_owned()).await.unwrap();
    say(
        &conn,
        &answer(
            id.unwrap(),
            Outcome::Refused {
                reason: "no".into(),
                conflict: false,
            },
        ),
    )
    .await;
    let error = tokio::time::timeout(Duration::from_secs(5), editing)
        .await
        .expect("an outcome once answered")
        .unwrap()
        .unwrap_err();
    assert!(matches!(error, CoordinatorError::Refused { .. }), "{error}");
}

#[tokio::test(start_paused = true)]
async fn an_id_refused_is_refused_again_when_it_is_sent_again() {
    let (fleet, _, conn, said) = acting_with_a_peer().await;
    let daemon = &fleet.daemons[0];
    let db = refuse_settings_writes(&daemon.db);
    let edit = edit_with(EditId::random(), set_project(7, "refused once"));
    say(&conn, &edit).await;
    until(Duration::from_secs(2), "the edit is refused", || {
        answers_sent(&said).len() == 1
    })
    .await;

    // The store takes writes again, but the sender was told the edit is refused.
    db.execute_batch("DROP TRIGGER refuse").unwrap();
    say(&conn, &edit).await;
    until(
        Duration::from_secs(2),
        "the edit sent again is answered",
        || answers_sent(&said).len() == 2,
    )
    .await;
    let answers = answers_sent(&said);
    assert!(
        matches!(answers[0].1, Outcome::Refused { .. }),
        "{answers:?}"
    );
    assert_eq!(answers[0], answers[1]);
    assert_eq!(project_named(&daemon.settings(), 7), None);
}

#[tokio::test(start_paused = true)]
async fn a_failed_write_after_the_edit_was_sent_does_not_end_the_wait_once_the_role_moves_here() {
    let peer = MachineId::random();
    let fleet = Fleet::start_coordinated_with(1, &[peer]);
    let daemon = &fleet.daemons[0];
    let (conn, said) = recorded(raw_link(daemon, peer).await);
    let theirs = ordered_by(&daemon.settings(), peer, vec![peer, daemon.id]);
    say(&conn, &settings_message(theirs)).await;
    let beat = beating(conn.clone());
    acting_becomes(&daemon.coordinator(), peer, WAIT).await;
    let editing = tokio::spawn({
        let coordinator = daemon.coordinator();
        async move { coordinator.edit(set_project(7, "written later")).await }
    });
    until(Duration::from_secs(2), "the edit is sent", || {
        !edits_sent(&said).is_empty()
    })
    .await;

    // The peer goes quiet, so the role moves here while the store refuses writes.
    beat.abort();
    let db = refuse_settings_writes(&daemon.db);
    tokio::time::sleep(OFFLINE_AFTER + Duration::from_secs(2)).await;
    assert!(!editing.is_finished(), "ended by the failed write");
    db.execute_batch("DROP TRIGGER refuse").unwrap();
    tokio::time::timeout(Duration::from_secs(2), editing)
        .await
        .expect("numbered once the store takes writes")
        .unwrap()
        .expect("a failed write after the send is not the end");
    assert_eq!(
        project_named(&daemon.settings(), 7).as_deref(),
        Some("written later")
    );
}

#[tokio::test(start_paused = true)]
async fn an_own_add_sent_before_the_role_moves_here_is_not_refused_by_the_rules_here() {
    let peer = MachineId::random();
    let fleet = Fleet::start_coordinated_with(1, &[peer]);
    let daemon = &fleet.daemons[0];
    let (conn, said) = recorded(raw_link(daemon, peer).await);
    let theirs = ordered_by(&daemon.settings(), peer, vec![peer, daemon.id]);
    say(&conn, &settings_message(theirs)).await;
    let beat = beating(conn.clone());
    acting_becomes(&daemon.coordinator(), peer, WAIT).await;
    holding_project(daemon, 1, "foundry");
    let editing = tokio::spawn({
        let coordinator = daemon.coordinator();
        async move { coordinator.edit(set_project(7, "Foundry")).await }
    });
    until(Duration::from_secs(2), "the edit is sent", || {
        !edits_sent(&said).is_empty()
    })
    .await;
    let id = edits_sent(&said)[0].0.expect("sent with an id");

    // The peer goes quiet, so the role moves here, where the add breaks the name rule. The
    // peer may have numbered it, so it waits.
    beat.abort();
    acting_becomes(&daemon.coordinator(), daemon.id, WAIT).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(!editing.is_finished(), "ended by the rule here");

    // The peer comes back on a new link with a copy that holds its stamp for the id, which
    // settles it.
    let (conn, _) = recorded(raw_link(daemon, peer).await);
    let mut numbered = daemon.settings();
    numbered.seq += 5;
    numbered.projects.push(project(7, "Foundry"));
    let stamp = Stamp {
        seq: numbered.seq,
        by: peer,
    };
    numbered.stamps.insert(project_key(7), stamp);
    numbered.seen.insert(peer, numbered.seq);
    numbered.numbered.insert(id, stamp);
    say(&conn, &settings_message(numbered)).await;
    let made = tokio::time::timeout(Duration::from_secs(2), editing)
        .await
        .expect("settled by the stamp")
        .unwrap();
    assert_eq!(made.unwrap(), stamp);
}

#[tokio::test(start_paused = true)]
async fn the_same_id_from_two_machines_is_answered_to_each() {
    let (p1, p2, quiet) = (
        MachineId::random(),
        MachineId::random(),
        MachineId::random(),
    );
    let fleet = Fleet::start_coordinated_with(1, &[p1, p2, quiet]);
    let daemon = &fleet.daemons[0];
    let (c1, said1) = recorded(raw_link(daemon, p1).await);
    let (c2, said2) = recorded(raw_link(daemon, p2).await);
    say(&c1, &settings_message(daemon.settings())).await;
    say(&c2, &settings_message(daemon.settings())).await;
    let _b1 = beating(c1.clone());
    let _b2 = beating(c2.clone());
    // `quiet` never sends its settings, so edits wait to catch up.
    let _quiet = beat(raw_link(daemon, quiet).await);
    until(WAIT, "every peer is online", || {
        let online = daemon.peers.online();
        [p1, p2, quiet].iter().all(|p| online.contains(p))
    })
    .await;

    let edit = edit_with(EditId::random(), set_project(7, "from two"));
    say(&c1, &edit).await;
    say(&c2, &edit).await;
    tokio::time::sleep(OFFLINE_AFTER + Duration::from_secs(5)).await;
    let (a1, a2) = (answers_sent(&said1), answers_sent(&said2));
    assert_eq!(a1.len(), 1, "{a1:?}");
    assert_eq!(a2.len(), 1, "{a2:?}");
    assert_eq!(a1, a2, "one id, one number");
}

#[tokio::test(start_paused = true)]
async fn a_forwarded_edit_whose_settings_cannot_be_read_for_a_moment_is_still_numbered() {
    let (peer, quiet) = (MachineId::random(), MachineId::random());
    let fleet = Fleet::start_coordinated_with(1, &[peer, quiet]);
    let daemon = &fleet.daemons[0];
    let (conn, said) = recorded(raw_link(daemon, peer).await);
    say(&conn, &settings_message(daemon.settings())).await;
    let _beating = beating(conn.clone());
    let _quiet = beat(raw_link(daemon, quiet).await);
    until(WAIT, "both peers are online", || {
        let online = daemon.peers.online();
        online.contains(&peer) && online.contains(&quiet)
    })
    .await;
    let id = EditId::random();
    say(&conn, &edit_with(id, set_project(7, "read later"))).await;
    tokio::time::sleep(Duration::from_secs(2)).await;

    let db = rusqlite::Connection::open(&daemon.db).unwrap();
    let held: String = db
        .query_row("SELECT settings FROM settings", [], |row| row.get(0))
        .unwrap();
    db.execute("UPDATE settings SET settings = 'unreadable'", [])
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    db.execute("UPDATE settings SET settings = ?1", [held])
        .unwrap();

    tokio::time::sleep(OFFLINE_AFTER).await;
    let answers = answers_sent(&said);
    assert!(
        matches!(answers[..], [(got, Outcome::Numbered { .. })] if got == id),
        "{answers:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn an_edit_a_machine_that_acted_before_numbered_is_answered_with_its_stamp_and_not_numbered_again()
 {
    let (before, forwarder) = (MachineId::random(), MachineId::random());
    let fleet = Fleet::start_coordinated_with(1, &[before, forwarder]);
    let daemon = &fleet.daemons[0];
    let cb = drained(raw_link(daemon, before).await);
    let (cf, said) = recorded(raw_link(daemon, forwarder).await);
    let id = EditId::random();
    let stamp = Stamp {
        seq: daemon.settings().seq + 1,
        by: before,
    };
    let mut theirs = daemon.settings();
    theirs.seq += 1;
    theirs.numbered.insert(id, stamp);
    say(&cb, &settings_message(theirs)).await;
    say(&cf, &settings_message(daemon.settings())).await;
    let _bb = beating(cb.clone());
    let _bf = beating(cf.clone());
    until(WAIT, "the copy with the id is taken", || {
        daemon.settings().numbered.contains_key(&id)
    })
    .await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(daemon.coordinator().acting().await.unwrap(), daemon.id);
    let seq = daemon.settings().seq;

    say(&cf, &edit_with(id, set_project(7, "numbered before"))).await;
    until(Duration::from_secs(2), "the edit is answered", || {
        !answers_sent(&said).is_empty()
    })
    .await;
    assert_eq!(answers_sent(&said), [(id, Outcome::Numbered { stamp })]);
    assert_eq!(daemon.settings().seq, seq, "numbered again");
    assert_eq!(project_named(&daemon.settings(), 7), None);
}

#[tokio::test(start_paused = true)]
async fn a_refusal_from_a_second_machine_is_not_final_for_an_edit_sent_to_two() {
    let (p1, p2) = (MachineId::random(), MachineId::random());
    let fleet = Fleet::start_coordinated_with(1, &[p1, p2]);
    let daemon = &fleet.daemons[0];
    let theirs = ordered_by(&daemon.settings(), p1, vec![p1, p2, daemon.id]);
    let (c1, said1) = recorded(raw_link(daemon, p1).await);
    let (c2, said2) = recorded(raw_link(daemon, p2).await);
    say(&c1, &settings_message(theirs.clone())).await;
    say(&c2, &settings_message(theirs)).await;
    let beat1 = beating(c1.clone());
    let _beat2 = beating(c2.clone());
    acting_becomes(&daemon.coordinator(), p1, WAIT).await;
    let editing = tokio::spawn({
        let coordinator = daemon.coordinator();
        async move { coordinator.edit(set_project(7, "sent twice")).await }
    });
    until(Duration::from_secs(2), "the edit goes to the first", || {
        !edits_sent(&said1).is_empty()
    })
    .await;
    // The first goes quiet and may have numbered it; the second, which then acts, refuses it.
    beat1.abort();
    until(OFFLINE_AFTER * 2, "the edit goes to the second", || {
        !edits_sent(&said2).is_empty()
    })
    .await;
    let (id, _) = edits_sent(&said2)[0].clone();
    let refused = Outcome::Refused {
        reason: "no".into(),
        conflict: false,
    };
    say(&c2, &answer(id.unwrap(), refused)).await;
    let error = tokio::time::timeout(OFFLINE_AFTER * 3, editing)
        .await
        .expect("an outcome by the limit")
        .unwrap()
        .unwrap_err();
    assert!(
        matches!(error, CoordinatorError::NoAnswer { .. }),
        "{error}"
    );
}

#[tokio::test(start_paused = true)]
async fn an_edit_sent_and_never_answered_is_no_answer_though_the_settings_cannot_be_read() {
    let (fleet, _, _, _conn, said) = behind_a_peer_that_acts().await;
    let daemon = &fleet.daemons[0];
    let editing = tokio::spawn({
        let coordinator = daemon.coordinator();
        async move { coordinator.edit(set_project(7, "unread")).await }
    });
    until(Duration::from_secs(2), "the edit is sent", || {
        !edits_sent(&said).is_empty()
    })
    .await;
    tokio::time::sleep(Duration::from_secs(20)).await;
    rusqlite::Connection::open(&daemon.db)
        .unwrap()
        .execute("UPDATE settings SET settings = 'unreadable'", [])
        .unwrap();
    let error = tokio::time::timeout(OFFLINE_AFTER * 3, editing)
        .await
        .expect("an outcome by the limit")
        .unwrap()
        .unwrap_err();
    assert!(
        matches!(error, CoordinatorError::NoAnswer { .. }),
        "{error}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_stamp_from_the_second_machine_replaces_the_first_machines_refusal() {
    let (p1, p2) = (MachineId::random(), MachineId::random());
    let fleet = Fleet::start_coordinated_with(1, &[p1, p2]);
    let daemon = &fleet.daemons[0];
    let theirs = ordered_by(&daemon.settings(), p1, vec![p1, p2, daemon.id]);
    let (c1, said1) = recorded(raw_link(daemon, p1).await);
    let (c2, said2) = recorded(raw_link(daemon, p2).await);
    say(&c1, &settings_message(theirs.clone())).await;
    say(&c2, &settings_message(theirs)).await;
    let beat1 = beating(c1.clone());
    let _beat2 = beating(c2.clone());
    acting_becomes(&daemon.coordinator(), p1, WAIT).await;
    let editing = tokio::spawn({
        let coordinator = daemon.coordinator();
        async move {
            coordinator
                .edit(set_project(7, "refused then numbered"))
                .await
        }
    });
    until(Duration::from_secs(2), "the edit goes to the first", || {
        !edits_sent(&said1).is_empty()
    })
    .await;
    beat1.abort();
    until(OFFLINE_AFTER * 2, "the edit goes to the second", || {
        !edits_sent(&said2).is_empty()
    })
    .await;
    let (id, _) = edits_sent(&said2)[0].clone();
    let id = id.unwrap();
    let refused = Outcome::Refused {
        reason: "no".into(),
        conflict: false,
    };
    // An answer is for an id, whichever link carries it.
    say(&c2, &answer(id, refused)).await;
    let stamp = Stamp { seq: 50, by: p2 };
    say(&c2, &answer(id, Outcome::Numbered { stamp })).await;
    let returned = tokio::time::timeout(Duration::from_secs(2), editing)
        .await
        .expect("the stamp settles it")
        .unwrap()
        .unwrap();
    assert_eq!(returned, stamp);
}

/// Makes the store at `db` refuse every write to the settings, until the returned connection
/// runs `DROP TRIGGER refuse`.
fn refuse_settings_writes(db: &Path) -> rusqlite::Connection {
    let db = rusqlite::Connection::open(db).unwrap();
    db.execute_batch(
        "CREATE TRIGGER refuse BEFORE UPDATE ON settings \
         BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
    )
    .unwrap();
    db
}

/// A daemon on which `acting`, on this build, acts, and that `sender` forwards edits to, both
/// linked and sending heartbeats. Returns the ends the test speaks on as each, with what the
/// daemon sends them, and the heartbeat task of `acting`.
async fn relaying() -> (
    Fleet,
    (MachineId, SharedSettings),
    (mpsc::Sender<String>, Arc<Mutex<Vec<serde_json::Value>>>),
    (mpsc::Sender<String>, Arc<Mutex<Vec<serde_json::Value>>>),
    tokio::task::JoinHandle<()>,
) {
    let (acting, sender) = (MachineId::random(), MachineId::random());
    let fleet = Fleet::start_coordinated_with(1, &[acting, sender]);
    let daemon = &fleet.daemons[0];
    let theirs = ordered_by(&daemon.settings(), acting, vec![acting, daemon.id, sender]);
    let (ca, said_a) = recorded(raw_link(daemon, acting).await);
    let (cs, said_s) = recorded(raw_link(daemon, sender).await);
    say(&ca, &settings_message(theirs.clone())).await;
    say(&cs, &settings_message(theirs.clone())).await;
    let beat_a = beating(ca.clone());
    beating(cs.clone());
    acting_becomes(&daemon.coordinator(), acting, WAIT).await;
    (fleet, (acting, theirs), (ca, said_a), (cs, said_s), beat_a)
}

#[tokio::test(start_paused = true)]
async fn a_relayed_edit_is_not_refused_when_a_write_here_fails_once_the_role_moves_here() {
    let (fleet, _, (_ca, said_a), (cs, said_s), beat_a) = relaying().await;
    let daemon = &fleet.daemons[0];
    say(&cs, &edit_message(set_project(7, "relayed"))).await;
    until(Duration::from_secs(2), "the edit is passed on", || {
        edits_sent(&said_a).len() == 1
    })
    .await;

    // The acting machine goes quiet, and may have numbered the edit. The role moves here,
    // where the store refuses writes.
    beat_a.abort();
    let _refusing = refuse_settings_writes(&daemon.db);
    acting_becomes(&daemon.coordinator(), daemon.id, WAIT).await;
    tokio::time::sleep(OFFLINE_AFTER * 2).await;
    assert_eq!(
        answers_sent(&said_s),
        [],
        "an edit another machine may have numbered is not refused"
    );
}

#[tokio::test(start_paused = true)]
async fn an_edit_relayed_and_answered_is_answered_with_its_stamp_again_once_the_role_moves_here() {
    let (fleet, (acting, theirs), (ca, said_a), (cs, said_s), beat_a) = relaying().await;
    let daemon = &fleet.daemons[0];
    let id = EditId::random();
    let edit = edit_with(id, set_project(7, "numbered there"));
    say(&cs, &edit).await;
    until(Duration::from_secs(2), "the edit is passed on", || {
        edits_sent(&said_a).len() == 1
    })
    .await;
    let stamp = Stamp {
        seq: theirs.seq + 1,
        by: acting,
    };
    say(&ca, &answer(id, Outcome::Numbered { stamp })).await;
    until(Duration::from_secs(2), "the answer is passed back", || {
        answers_sent(&said_s).len() == 1
    })
    .await;

    // The answer is lost on its way to the sender, and the acting machine goes quiet before
    // its copy holding the stamp reaches here. The role moves here, where the store refuses
    // writes, and the sender sends the edit again.
    beat_a.abort();
    let _refusing = refuse_settings_writes(&daemon.db);
    acting_becomes(&daemon.coordinator(), daemon.id, WAIT).await;
    say(&cs, &edit).await;
    tokio::time::sleep(OFFLINE_AFTER * 2).await;
    assert_eq!(
        answers_sent(&said_s),
        [
            (id, Outcome::Numbered { stamp }),
            (id, Outcome::Numbered { stamp })
        ],
        "the edit sent again is answered with the stamp this machine saw, not refused"
    );
}

// On real time: the first link's writer fails at once, while the link stays listed until its
// next heartbeat fails, a second later. A paused clock could reach that heartbeat while the
// daemon reads its store.
#[tokio::test]
async fn a_forwarded_edit_goes_only_on_the_link_the_one_before_it_went_on() {
    goes_after_the_edit_before_it(false, false).await;
}

#[tokio::test]
async fn an_edit_without_an_id_goes_only_on_the_link_the_one_before_it_went_on() {
    goes_after_the_edit_before_it(false, true).await;
}

#[tokio::test]
async fn an_edit_goes_only_on_the_link_an_edit_without_an_id_before_it_went_on() {
    goes_after_the_edit_before_it(true, true).await;
}

/// A sender forwards edit "a" and then "b" to project 7. "a" goes on the first of two links to
/// the acting machine, which then stops taking what the daemon sends. "b" must not overtake it
/// on the second link. An edit `without_id` comes with no id, on a link the sender also opened
/// in the protocol before answers. "a" with an id is sent again on the second link, before "b";
/// "a" with none is never sent again, so "b" goes on the second link only once the first is
/// closed.
async fn goes_after_the_edit_before_it(first_without_id: bool, second_without_id: bool) {
    let (acting, sender) = (MachineId::random(), MachineId::random());
    let fleet = Fleet::start_coordinated_with(1, &[acting, sender]);
    let daemon = &fleet.daemons[0];
    let theirs = ordered_by(&daemon.settings(), acting, vec![acting, daemon.id, sender]);
    let Conn {
        send: first,
        recv: mut first_said,
    } = raw_link(daemon, acting).await;
    let (second, said_second) = recorded(raw_link(daemon, acting).await);
    let cs = drained(raw_link(daemon, sender).await);
    let older = drained(raw_link_speaking(daemon, sender, ANSWERS_SINCE - 1).await);
    say(&first, &settings_message(theirs.clone())).await;
    say(&cs, &settings_message(theirs)).await;
    let _beat_first = beating(first.clone());
    let _beat_second = beating(second.clone());
    let _beat_s = beating(cs.clone());
    acting_becomes(&daemon.coordinator(), acting, WAIT).await;

    let older_edit = |name: &str| PeerMessage::Edit {
        protocol: ANSWERS_SINCE - 1,
        id: None,
        edit: set_project(7, name),
    };
    if first_without_id {
        say(&older, &older_edit("a")).await;
    } else {
        say(&cs, &edit_message(set_project(7, "a"))).await;
    }
    tokio::time::timeout(WAIT, async {
        while let Some(text) = first_said.recv().await {
            if text.contains("\"type\":\"edit\"") {
                return;
            }
        }
    })
    .await
    .expect("the first edit goes on the first link");
    // The first link stops taking what the daemon sends, with the first edit unread, and the
    // next edit arrives.
    drop(first_said);
    if second_without_id {
        say(&older, &older_edit("b")).await;
    } else {
        say(&cs, &edit_message(set_project(7, "b"))).await;
    }
    let want = if first_without_id { 1 } else { 2 };
    let mut first_closed = false;
    until(WAIT, "the edits go on the second link", || {
        first_closed = first.is_closed();
        edits_sent(&said_second).len() == want
    })
    .await;
    if first_without_id {
        assert!(
            first_closed,
            "\"b\" went on the second link while the first was open"
        );
    }
    let names: Vec<_> = edits_sent(&said_second)
        .into_iter()
        .map(|(_, edit)| match edit {
            Edit::SetProject { project } => project.name,
            other => panic!("{other:?}"),
        })
        .collect();
    let want: &[&str] = if first_without_id {
        &["b"]
    } else {
        &["a", "b"]
    };
    assert_eq!(names, want, "in the order they arrived");
}
