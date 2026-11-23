use std::collections::BTreeMap;

use foundry_proto::factory::{
    FactoryBody, FactoryRecord, FactoryStatus, MachineHeld, RunEvent, RunId, SessionTurn,
};
use foundry_proto::ids::{MachineId, ProjectId, SessionId, TerminalId, WorkspaceId};
use foundry_proto::record::{AnyRecord, MachineSample, Record, Session, Terminal, Workspace};
use foundry_proto::settings::{Limits, Machine, SharedSettings};
use foundry_proto::state::{SessionState, TerminalState};
use tempfile::TempDir;

use super::*;

fn open() -> (TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("foundry.db")).unwrap();
    (dir, store)
}

fn workspace(owner: MachineId, version: u64) -> AnyRecord {
    AnyRecord::Workspace(Record {
        owner,
        version,
        deleted: false,
        body: Workspace {
            id: WorkspaceId(uuid::Uuid::from_u128(1)),
            project: ProjectId(uuid::Uuid::from_u128(2)),
            name: "fix login".into(),
            branch: "fix-login".into(),
            base: "main".into(),
            setup: None,
        },
    })
}

fn terminal(owner: MachineId, version: u64) -> AnyRecord {
    AnyRecord::Terminal(Record {
        owner,
        version,
        deleted: false,
        body: Terminal {
            id: TerminalId(uuid::Uuid::from_u128(3)),
            workspace: WorkspaceId(uuid::Uuid::from_u128(1)),
            state: TerminalState::Running,
            exit_code: None,
        },
    })
}

fn session(owner: MachineId, version: u64) -> AnyRecord {
    AnyRecord::Session(Record {
        owner,
        version,
        deleted: false,
        body: Session {
            id: SessionId(uuid::Uuid::from_u128(4)),
            workspace: WorkspaceId(uuid::Uuid::from_u128(1)),
            terminal: TerminalId(uuid::Uuid::from_u128(3)),
            model: "claude-opus-5-5".into(),
            state: SessionState::Working,
        },
    })
}

fn sample(owner: MachineId, version: u64) -> AnyRecord {
    AnyRecord::MachineSample(Record {
        owner,
        version,
        deleted: false,
        body: MachineSample {
            load_per_core: 0.5,
            free_memory: 1 << 30,
            free_disk: 1 << 35,
            sessions: 2,
            update_refused: None,
        },
    })
}

fn every_kind(owner: MachineId, version: u64) -> Vec<AnyRecord> {
    vec![
        workspace(owner, version),
        terminal(owner, version),
        session(owner, version),
        sample(owner, version),
    ]
}

fn deleted(record: AnyRecord) -> AnyRecord {
    match record {
        AnyRecord::Workspace(r) => AnyRecord::Workspace(Record { deleted: true, ..r }),
        AnyRecord::Terminal(r) => AnyRecord::Terminal(Record { deleted: true, ..r }),
        AnyRecord::Session(r) => AnyRecord::Session(Record { deleted: true, ..r }),
        AnyRecord::MachineSample(r) => AnyRecord::MachineSample(Record { deleted: true, ..r }),
    }
}

fn everything(store: &Store) -> Vec<AnyRecord> {
    let mut all = Vec::new();
    for owner in store.versions().unwrap().into_keys() {
        all.extend(store.since(owner, 0).unwrap());
    }
    all
}

fn settings(seq: u64) -> SharedSettings {
    let mac = MachineId(uuid::Uuid::from_u128(10));
    SharedSettings {
        seq,
        machines: vec![Machine {
            id: mac,
            name: "trilogy-mac".into(),
            address: "100.64.0.1".into(),
            key: String::new(),
        }],
        priority: vec![mac],
        projects: vec![],
        limits: [(
            mac,
            Limits {
                max_sessions: 6,
                max_load_per_core: 1.0,
                min_free_disk: 1 << 34,
            },
        )]
        .into(),
        ..SharedSettings::default()
    }
}

#[test]
fn a_record_from_its_owner_is_stored_for_every_kind() {
    let (_dir, mut store) = open();
    let owner = MachineId::random();
    for record in every_kind(owner, 1) {
        assert_eq!(
            store.apply(&record, owner).unwrap(),
            Applied::Stored,
            "{record:?}"
        );
    }
    assert_eq!(everything(&store).len(), 4);
}

#[test]
fn a_record_from_a_machine_that_is_not_its_owner_is_not_owner_and_stores_nothing() {
    let (_dir, mut store) = open();
    let owner = MachineId::random();
    let other = MachineId::random();
    for record in every_kind(owner, 1) {
        assert_eq!(
            store.apply(&record, other).unwrap(),
            Applied::NotOwner,
            "{record:?}"
        );
    }
    assert!(everything(&store).is_empty());
    assert!(store.versions().unwrap().is_empty());
}

#[test]
fn a_record_that_takes_over_another_owners_record_is_not_owner_and_changes_nothing() {
    let (_dir, mut store) = open();
    let owner = MachineId::random();
    let thief = MachineId::random();
    for record in [workspace(owner, 3), terminal(owner, 3), session(owner, 3)] {
        store.apply(&record, owner).unwrap();
    }
    let before = everything(&store);
    for record in [workspace(thief, 9), terminal(thief, 9), session(thief, 9)] {
        assert_eq!(
            store.apply(&record, thief).unwrap(),
            Applied::NotOwner,
            "{record:?}"
        );
    }
    assert_eq!(everything(&store), before);
    assert!(!store.versions().unwrap().contains_key(&thief));
}

#[test]
fn an_equal_or_lower_version_is_stale_and_changes_nothing() {
    let (_dir, mut store) = open();
    let owner = MachineId::random();
    for record in every_kind(owner, 5) {
        store.apply(&record, owner).unwrap();
    }
    let before = everything(&store);
    for version in [5, 4, 0] {
        for record in every_kind(owner, version) {
            let changed = deleted(record);
            assert_eq!(
                store.apply(&changed, owner).unwrap(),
                Applied::Stale,
                "{changed:?}"
            );
        }
    }
    assert_eq!(everything(&store), before);
}

#[test]
fn a_higher_version_replaces_the_stored_one() {
    let (_dir, mut store) = open();
    let owner = MachineId::random();
    store.apply(&workspace(owner, 1), owner).unwrap();
    let newer = deleted(workspace(owner, 2));
    assert_eq!(store.apply(&newer, owner).unwrap(), Applied::Stored);
    assert_eq!(store.since(owner, 0).unwrap(), vec![newer]);
}

#[test]
fn applying_the_same_record_twice_changes_nothing() {
    let (_dir, mut store) = open();
    let owner = MachineId::random();
    for record in every_kind(owner, 2) {
        assert_eq!(store.apply(&record, owner).unwrap(), Applied::Stored);
        let once = everything(&store);
        let versions = store.versions().unwrap();
        assert_eq!(store.apply(&record, owner).unwrap(), Applied::Stale);
        assert_eq!(everything(&store), once);
        assert_eq!(store.versions().unwrap(), versions);
    }
}

#[test]
fn a_deleted_record_stays_deleted_when_an_older_live_version_arrives() {
    let (_dir, mut store) = open();
    let owner = MachineId::random();
    for record in every_kind(owner, 1) {
        store.apply(&record, owner).unwrap();
    }
    let tombstones: Vec<_> = every_kind(owner, 8).into_iter().map(deleted).collect();
    for record in &tombstones {
        assert_eq!(store.apply(record, owner).unwrap(), Applied::Stored);
    }
    for record in every_kind(owner, 7) {
        assert_eq!(store.apply(&record, owner).unwrap(), Applied::Stale);
    }
    let mut kept = store.since(owner, 0).unwrap();
    kept.sort_by_key(|r| format!("{r:?}"));
    let mut expected = tombstones;
    expected.sort_by_key(|r| format!("{r:?}"));
    assert_eq!(kept, expected);
}

#[test]
fn a_deleted_record_that_arrives_first_is_kept_so_an_older_live_one_cannot_revive_it() {
    let (_dir, mut store) = open();
    let owner = MachineId::random();
    let tombstone = deleted(workspace(owner, 8));
    assert_eq!(store.apply(&tombstone, owner).unwrap(), Applied::Stored);
    assert_eq!(
        store.apply(&workspace(owner, 3), owner).unwrap(),
        Applied::Stale
    );
    assert_eq!(store.since(owner, 0).unwrap(), vec![tombstone]);
}

#[test]
fn since_gives_one_owners_records_above_a_version_in_version_order() {
    let (_dir, mut store) = open();
    let owner = MachineId::random();
    let other = MachineId::random();
    // Applied out of version order: the order out must come from the version alone.
    store.apply(&session(owner, 9), owner).unwrap();
    store.apply(&workspace(owner, 4), owner).unwrap();
    store.apply(&terminal(owner, 6), owner).unwrap();
    store.apply(&sample(other, 50), other).unwrap();
    // Without the index the rows come back in the order they went in (9, 4, 6), so only
    // the query itself can put them in version order.
    store
        .conn
        .execute("DROP INDEX records_by_owner", [])
        .unwrap();

    assert_eq!(
        store.since(owner, 0).unwrap(),
        vec![workspace(owner, 4), terminal(owner, 6), session(owner, 9)]
    );
    assert_eq!(
        store.since(owner, 4).unwrap(),
        vec![terminal(owner, 6), session(owner, 9)]
    );
    assert!(store.since(owner, 9).unwrap().is_empty());
    assert!(store.since(owner, u64::MAX).unwrap().is_empty());
    assert_eq!(store.since(other, 0).unwrap(), vec![sample(other, 50)]);
    assert!(store.since(MachineId::random(), 0).unwrap().is_empty());
}

#[test]
fn since_limited_gives_at_most_limit_records_lowest_versions_first() {
    let (_dir, mut store) = open();
    let owner = MachineId::random();
    store.apply(&session(owner, 9), owner).unwrap();
    store.apply(&workspace(owner, 4), owner).unwrap();
    store.apply(&terminal(owner, 6), owner).unwrap();

    assert_eq!(
        store.since_limited(owner, 0, 2).unwrap(),
        vec![workspace(owner, 4), terminal(owner, 6)]
    );
    assert_eq!(
        store.since_limited(owner, 6, 2).unwrap(),
        vec![session(owner, 9)]
    );
    assert!(store.since_limited(owner, 0, 0).unwrap().is_empty());
    assert!(store.since_limited(owner, 9, 2).unwrap().is_empty());
}

/// A row this build cannot read past the limit does not stop the rows below it.
#[test]
fn since_limited_reads_only_the_rows_it_gives() {
    let (_dir, mut store) = open();
    let owner = MachineId::random();
    store.apply(&workspace(owner, 1), owner).unwrap();
    store.apply(&terminal(owner, 2), owner).unwrap();
    insert_unreadable(&store, owner, 3, "printer", "{}");

    assert_eq!(
        store.since_limited(owner, 0, 2).unwrap(),
        vec![workspace(owner, 1), terminal(owner, 2)]
    );
    assert!(matches!(
        store.since_limited(owner, 2, 2),
        Err(StoreError::Json(_))
    ));
}

/// One workspace is read by its key: a row this build cannot read elsewhere does not stop it.
#[test]
fn workspace_gives_one_workspace_by_its_id_deleted_or_not() {
    let (_dir, mut store) = open();
    let owner = MachineId::random();
    let AnyRecord::Workspace(live) = workspace(owner, 1) else {
        unreachable!()
    };
    let id = live.body.id;
    assert_eq!(store.workspace(id).unwrap(), None);

    store.apply(&workspace(owner, 1), owner).unwrap();
    // A terminal under the same id is another record.
    let mut same_id = terminal(owner, 2);
    if let AnyRecord::Terminal(r) = &mut same_id {
        r.body.id = TerminalId(id.0);
    }
    store.apply(&same_id, owner).unwrap();
    insert_unreadable(&store, owner, 3, "printer", "{}");
    assert_eq!(store.workspace(id).unwrap(), Some(live.clone()));

    store.apply(&deleted(workspace(owner, 4)), owner).unwrap();
    let held = store.workspace(id).unwrap().unwrap();
    assert!(held.deleted);
    assert_eq!(held.version, 4);
    assert_eq!(
        store
            .workspace(WorkspaceId(uuid::Uuid::from_u128(99)))
            .unwrap(),
        None
    );
}

/// One terminal is read by its key: a row this build cannot read elsewhere does not stop it.
#[test]
fn terminal_gives_one_terminal_by_its_id_deleted_or_not() {
    let (_dir, mut store) = open();
    let owner = MachineId::random();
    let AnyRecord::Terminal(live) = terminal(owner, 2) else {
        unreachable!()
    };
    let id = live.body.id;
    assert_eq!(store.terminal(id).unwrap(), None);

    // A workspace under the same id is another record. It goes in first, so a read that
    // ignored the kind would find it before the terminal.
    let mut same_id = workspace(owner, 1);
    if let AnyRecord::Workspace(r) = &mut same_id {
        r.body.id = WorkspaceId(id.0);
    }
    store.apply(&same_id, owner).unwrap();
    store.apply(&terminal(owner, 2), owner).unwrap();
    insert_unreadable(&store, owner, 3, "printer", "{}");
    assert_eq!(store.terminal(id).unwrap(), Some(live.clone()));

    store.apply(&deleted(terminal(owner, 4)), owner).unwrap();
    let held = store.terminal(id).unwrap().unwrap();
    assert!(held.deleted);
    assert_eq!(held.version, 4);
    assert_eq!(
        store
            .terminal(TerminalId(uuid::Uuid::from_u128(99)))
            .unwrap(),
        None
    );

    // The one row this build cannot read is an error, not an absent terminal.
    store
        .conn
        .execute(
            "UPDATE records SET record = '{}' WHERE kind = 'terminal' AND id = ?1",
            params![id.to_string()],
        )
        .unwrap();
    assert!(matches!(store.terminal(id), Err(StoreError::Json(_))));
}

/// The terminals of one workspace are read without the owner's other records, deleted ones
/// included, lowest version first.
#[test]
fn terminals_in_gives_one_workspaces_terminals_from_one_owner() {
    let (_dir, mut store) = open();
    let owner = MachineId::random();
    let other = MachineId::random();
    let in_workspace = |id: u128, workspace: u128, owner: MachineId, version: u64| {
        let AnyRecord::Terminal(mut r) = terminal(owner, version) else {
            unreachable!()
        };
        r.body.id = TerminalId(uuid::Uuid::from_u128(id));
        r.body.workspace = WorkspaceId(uuid::Uuid::from_u128(workspace));
        r
    };
    let first = in_workspace(10, 1, owner, 1);
    let elsewhere = in_workspace(11, 2, owner, 2);
    let mut second = in_workspace(12, 1, owner, 3);
    second.deleted = true;
    let not_owned = in_workspace(13, 1, other, 1);
    for r in [&first, &elsewhere, &second] {
        store.apply(&AnyRecord::Terminal(r.clone()), owner).unwrap();
    }
    store
        .apply(&AnyRecord::Terminal(not_owned.clone()), other)
        .unwrap();
    store.apply(&workspace(owner, 4), owner).unwrap();
    insert_unreadable(&store, owner, 5, "printer", "{}");

    let one = WorkspaceId(uuid::Uuid::from_u128(1));
    assert_eq!(store.terminals_in(owner, one).unwrap(), vec![first, second]);
    assert_eq!(store.terminals_in(other, one).unwrap(), vec![not_owned]);
    let none = WorkspaceId(uuid::Uuid::from_u128(98));
    assert!(store.terminals_in(owner, none).unwrap().is_empty());
}

/// The workspaces not deleted, from every owner, read without the other kinds: a row of
/// another kind this build cannot read does not stop it.
#[test]
fn live_workspaces_gives_every_owners_workspaces_not_deleted_and_reads_no_other_kind() {
    let (_dir, mut store) = open();
    let owner = MachineId::random();
    let other = MachineId::random();
    let numbered = |id: u128, owner: MachineId, version: u64| {
        let AnyRecord::Workspace(mut r) = workspace(owner, version) else {
            unreachable!()
        };
        r.body.id = WorkspaceId(uuid::Uuid::from_u128(id));
        r
    };
    assert!(store.live_workspaces().unwrap().is_empty());

    let mine = numbered(20, owner, 1);
    let mut archived = numbered(21, owner, 2);
    archived.deleted = true;
    let theirs = numbered(22, other, 3);
    for r in [&mine, &archived] {
        store
            .apply(&AnyRecord::Workspace(r.clone()), owner)
            .unwrap();
    }
    store
        .apply(&AnyRecord::Workspace(theirs.clone()), other)
        .unwrap();
    store.apply(&terminal(owner, 4), owner).unwrap();
    store.apply(&session(other, 5), other).unwrap();
    insert_unreadable(&store, owner, 6, "printer", "{}");

    assert_eq!(store.live_workspaces().unwrap(), vec![mine, theirs]);
}

#[test]
fn versions_gives_the_highest_version_held_per_owner() {
    let (_dir, mut store) = open();
    assert!(store.versions().unwrap().is_empty());
    let a = MachineId::random();
    let b = MachineId::random();
    store.apply(&workspace(a, 3), a).unwrap();
    store.apply(&session(a, 11), a).unwrap();
    store.apply(&sample(b, 2), b).unwrap();
    store.apply(&sample(b, 5), b).unwrap();
    assert_eq!(store.versions().unwrap(), BTreeMap::from([(a, 11), (b, 5)]));
}

#[test]
fn one_machine_sample_is_kept_per_owner() {
    let (_dir, mut store) = open();
    let a = MachineId::random();
    let b = MachineId::random();
    store.apply(&sample(a, 1), a).unwrap();
    store.apply(&sample(b, 1), b).unwrap();
    store.apply(&sample(a, 2), a).unwrap();
    assert_eq!(store.since(a, 0).unwrap(), vec![sample(a, 2)]);
    assert_eq!(store.since(b, 0).unwrap(), vec![sample(b, 1)]);
}

#[test]
fn records_of_different_kinds_with_the_same_id_are_kept_apart() {
    let (_dir, mut store) = open();
    let owner = MachineId::random();
    let id = uuid::Uuid::from_u128(77);
    let AnyRecord::Workspace(mut w) = workspace(owner, 1) else {
        unreachable!()
    };
    w.body.id = WorkspaceId(id);
    let AnyRecord::Terminal(mut t) = terminal(owner, 2) else {
        unreachable!()
    };
    t.body.id = TerminalId(id);
    store.apply(&AnyRecord::Workspace(w), owner).unwrap();
    store.apply(&AnyRecord::Terminal(t), owner).unwrap();
    assert_eq!(store.since(owner, 0).unwrap().len(), 2);
}

#[test]
fn records_survive_reopening_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("foundry.db");
    let owner = MachineId::random();
    {
        let mut store = Store::open(&path).unwrap();
        store.apply(&workspace(owner, 4), owner).unwrap();
        assert!(store.apply_settings(settings(3)).unwrap());
    }
    let store = Store::open(&path).unwrap();
    assert_eq!(store.since(owner, 0).unwrap(), vec![workspace(owner, 4)]);
    assert_eq!(store.settings().unwrap(), settings(3));
}

#[test]
fn own_next_version_only_goes_up_including_across_reopening() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("foundry.db");
    let mut seen = Vec::new();
    {
        let mut store = Store::open(&path).unwrap();
        seen.push(store.own_next_version().unwrap());
        seen.push(store.own_next_version().unwrap());
    }
    let mut store = Store::open(&path).unwrap();
    seen.push(store.own_next_version().unwrap());
    assert!(seen[0] > 0, "{seen:?}");
    assert!(seen.windows(2).all(|w| w[1] > w[0]), "{seen:?}");
}

#[test]
fn settings_start_empty() {
    let (_dir, store) = open();
    assert_eq!(store.settings().unwrap(), SharedSettings::default());
}

#[test]
fn settings_with_a_higher_seq_are_accepted() {
    let (_dir, mut store) = open();
    assert!(store.apply_settings(settings(1)).unwrap());
    assert_eq!(store.settings().unwrap(), settings(1));
    assert!(store.apply_settings(settings(2)).unwrap());
    assert_eq!(store.settings().unwrap(), settings(2));
}

#[test]
fn settings_with_a_lower_or_equal_seq_are_refused() {
    let (_dir, mut store) = open();
    assert!(
        !store.apply_settings(settings(0)).unwrap(),
        "seq 0 on an empty store"
    );
    assert_eq!(store.settings().unwrap(), SharedSettings::default());
    assert!(store.apply_settings(settings(5)).unwrap());
    for seq in [5, 4, 0] {
        let mut older = settings(seq);
        older.projects.clear();
        older.priority.clear();
        assert!(!store.apply_settings(older).unwrap(), "seq {seq}");
    }
    assert_eq!(store.settings().unwrap(), settings(5));
}

#[test]
fn a_version_sqlite_cannot_hold_is_an_error_that_stores_nothing() {
    let (_dir, mut store) = open();
    let owner = MachineId::random();
    let error = store.apply(&workspace(owner, u64::MAX), owner).unwrap_err();
    assert!(error.to_string().contains(&u64::MAX.to_string()), "{error}");
    assert!(everything(&store).is_empty());
    let error = store.apply_settings(settings(u64::MAX)).unwrap_err();
    assert!(error.to_string().contains(&u64::MAX.to_string()), "{error}");
    assert_eq!(store.settings().unwrap(), SharedSettings::default());
}

#[test]
fn migrations_run_on_an_empty_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("foundry.db");
    std::fs::File::create(&path).unwrap();
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.schema_version().unwrap(), MIGRATIONS.len());
    let owner = MachineId::random();
    assert_eq!(
        store.apply(&workspace(owner, 1), owner).unwrap(),
        Applied::Stored
    );
    assert!(store.apply_settings(settings(1)).unwrap());
}

#[test]
fn migrations_run_on_every_earlier_schema_and_keep_what_it_held() {
    for earlier in 0..MIGRATIONS.len() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("foundry.db");
        {
            let mut conn = rusqlite::Connection::open(&path).unwrap();
            migrate(&mut conn, earlier).unwrap();
            assert_eq!(user_version(&conn).unwrap(), earlier);
            conn.execute("CREATE TABLE kept_by_the_test (x INTEGER)", [])
                .unwrap();
            conn.execute("INSERT INTO kept_by_the_test VALUES (42)", [])
                .unwrap();
        }
        let mut store = Store::open(&path).unwrap();
        assert_eq!(
            store.schema_version().unwrap(),
            MIGRATIONS.len(),
            "from {earlier}"
        );
        let kept: i64 = store
            .conn
            .query_row("SELECT x FROM kept_by_the_test", [], |row| row.get(0))
            .unwrap();
        assert_eq!(kept, 42, "from {earlier}");
        let owner = MachineId::random();
        assert_eq!(
            store.apply(&sample(owner, 1), owner).unwrap(),
            Applied::Stored
        );
    }
}

#[test]
fn reopening_a_current_file_runs_no_migration_again() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("foundry.db");
    let owner = MachineId::random();
    Store::open(&path)
        .unwrap()
        .apply(&workspace(owner, 2), owner)
        .unwrap();
    let store = Store::open(&path).unwrap();
    assert_eq!(store.schema_version().unwrap(), MIGRATIONS.len());
    assert_eq!(store.since(owner, 0).unwrap(), vec![workspace(owner, 2)]);
}

#[test]
fn a_file_a_newer_build_migrated_opens_and_keeps_working() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("foundry.db");
    let newer = MIGRATIONS.len() + 1;
    let owner = MachineId::random();
    Store::open(&path)
        .unwrap()
        .apply(&workspace(owner, 1), owner)
        .unwrap();
    {
        // What a newer build's extra step leaves: a table of its own, a nullable column on
        // every existing table and a higher count.
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE added_by_a_newer_build (x INTEGER);
             ALTER TABLE records ADD COLUMN added_later TEXT;
             ALTER TABLE own_version ADD COLUMN added_later TEXT;
             ALTER TABLE settings ADD COLUMN added_later TEXT;",
        )
        .unwrap();
        conn.execute("INSERT INTO added_by_a_newer_build VALUES (42)", [])
            .unwrap();
        conn.pragma_update(None, "user_version", newer as i64)
            .unwrap();
    }
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.schema_version().unwrap(), newer);
    let added: i64 = store
        .conn
        .query_row("SELECT x FROM added_by_a_newer_build", [], |row| row.get(0))
        .unwrap();
    assert_eq!(added, 42);
    assert_eq!(
        store.apply(&terminal(owner, 2), owner).unwrap(),
        Applied::Stored
    );
    assert_eq!(
        store.since(owner, 0).unwrap(),
        vec![workspace(owner, 1), terminal(owner, 2)]
    );
    assert_eq!(store.own_next_version().unwrap(), 1);
    assert!(store.apply_settings(settings(1)).unwrap());
    assert_eq!(store.settings().unwrap(), settings(1));
}

/// The rule on [`MIGRATIONS`], checked: every step after the first only adds, and nothing it
/// adds refuses a write an older build makes or asks a newer build to count on a backfill.
#[test]
fn every_later_migration_step_only_adds() {
    for (at, step) in MIGRATIONS.iter().enumerate().skip(1) {
        if let Err(statement) = adds_only(step) {
            panic!("step {} breaks the rule on MIGRATIONS: {statement}", at + 1);
        }
    }
}

#[test]
fn the_add_only_check_refuses_what_an_older_build_cannot_live_with() {
    for step in [
        "DROP TABLE settings",
        "DROP INDEX records_by_owner",
        "ALTER TABLE records RENAME TO kept",
        "ALTER TABLE records RENAME COLUMN record TO body",
        "ALTER TABLE records DROP COLUMN record",
        "ALTER TABLE records ADD COLUMN origin TEXT NOT NULL DEFAULT ''",
        "ALTER TABLE records ADD COLUMN origin TEXT UNIQUE",
        "ALTER TABLE records ADD COLUMN origin TEXT CHECK (origin <> '')",
        "ALTER TABLE records ADD COLUMN origin TEXT REFERENCES machines (id)",
        "ALTER TABLE records ADD COLUMN origin TEXT PRIMARY KEY",
        "CREATE UNIQUE INDEX one_per_version ON records (owner, version)",
        "CREATE TRIGGER no_old_writes BEFORE INSERT ON records BEGIN SELECT 1; END",
        "UPDATE records SET version = version + 1",
        "DELETE FROM records",
        "CREATE INDEX records_by_kind ON records (kind); UPDATE records SET kind = 'x'",
    ] {
        assert!(adds_only(step).is_err(), "{step}");
    }
    for step in [
        "CREATE TABLE notes (id TEXT NOT NULL PRIMARY KEY, body TEXT NOT NULL)",
        "CREATE INDEX records_by_kind ON records (kind)",
        "alter table records add column origin text",
        "ALTER TABLE records ADD origin TEXT DEFAULT ''",
        "CREATE TABLE notes (id TEXT);\n  CREATE INDEX notes_by_id ON notes (id);",
    ] {
        assert_eq!(adds_only(step), Ok(()), "{step}");
    }
}

/// Reads a step as statements split on `;` and allows only the three the rule names: a new
/// table, a non-unique index and a nullable column with no constraint. Gives back the first
/// statement that is anything else.
fn adds_only(step: &str) -> Result<(), String> {
    for statement in step.split(';') {
        let statement = statement
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_uppercase();
        let column = statement
            .strip_prefix("ALTER TABLE ")
            .and_then(|rest| rest.split_once(" ADD "))
            .map(|(_, column)| column);
        let allowed = statement.is_empty()
            || statement.starts_with("CREATE TABLE ")
            || statement.starts_with("CREATE INDEX ")
            || column.is_some_and(|column| {
                !["NOT NULL", "UNIQUE", "CHECK", "REFERENCES", "PRIMARY KEY"]
                    .iter()
                    .any(|constraint| column.contains(constraint))
            });
        if !allowed {
            return Err(statement);
        }
    }
    Ok(())
}

/// Puts a row this build cannot read into the store, as a newer build would have left it.
fn insert_unreadable(store: &Store, owner: MachineId, version: u64, kind: &str, body: &str) {
    store
        .conn
        .execute(
            "INSERT INTO records (kind, id, owner, version, record) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                kind,
                format!("unreadable-{version}"),
                owner.to_string(),
                version as i64,
                format!(
                    r#"{{"kind":"{kind}","owner":"{owner}","version":{version},"deleted":false,"body":{body}}}"#
                )
            ],
        )
        .unwrap();
}

/// A row this build cannot read can only come from a newer build before a rollback. `since`
/// says so with an error instead of giving the owner's records with a version missing.
/// Reading such rows after a rollback belongs to sync (task 12) and rollback (task 18).
#[test]
fn since_fails_on_a_kind_this_build_does_not_know() {
    let (_dir, mut store) = open();
    let owner = MachineId::random();
    store.apply(&workspace(owner, 1), owner).unwrap();
    insert_unreadable(&store, owner, 2, "printer", "{}");
    store.apply(&session(owner, 3), owner).unwrap();
    assert!(matches!(store.since(owner, 0), Err(StoreError::Json(_))));
    assert!(matches!(store.since(owner, 1), Err(StoreError::Json(_))));
    assert_eq!(store.since(owner, 2).unwrap(), vec![session(owner, 3)]);
}

#[test]
fn since_fails_on_a_known_kind_with_a_state_this_build_does_not_know() {
    let (_dir, mut store) = open();
    let owner = MachineId::random();
    store.apply(&workspace(owner, 1), owner).unwrap();
    let mut body = serde_json::to_value(match session(owner, 2) {
        AnyRecord::Session(r) => r.body,
        _ => unreachable!(),
    })
    .unwrap();
    body["state"] = serde_json::json!("dreaming");
    insert_unreadable(&store, owner, 2, "session", &body.to_string());
    store.apply(&terminal(owner, 3), owner).unwrap();
    assert!(matches!(store.since(owner, 0), Err(StoreError::Json(_))));
    assert_eq!(store.since(owner, 2).unwrap(), vec![terminal(owner, 3)]);
}

/// Review focus 3: clocks on two machines can disagree by minutes, so nothing in the
/// schema holds a time a query could order by. Records order only by their version.
#[test]
fn no_column_in_the_schema_holds_a_time() {
    let (_dir, store) = open();
    let mut tables = store
        .conn
        .prepare("SELECT name FROM sqlite_schema WHERE type = 'table'")
        .unwrap();
    let tables: Vec<String> = tables
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(!tables.is_empty());
    for table in tables {
        let mut columns = store
            .conn
            .prepare(&format!(
                "SELECT name, type FROM pragma_table_info('{table}')"
            ))
            .unwrap();
        let columns: Vec<(String, String)> = columns
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        for (name, kind) in columns {
            let name = name.to_lowercase();
            let kind = kind.to_lowercase();
            for word in ["time", "date", "_at", "created", "updated", "clock"] {
                assert!(!name.contains(word), "{table}.{name} looks like a time");
                assert!(!kind.contains(word), "{table}.{name} is typed {kind}");
            }
        }
    }
}

/// Review focus 3, checked in the queries themselves: every `ORDER BY` in `mod.rs` orders
/// by a version. It reads only `mod.rs`, which holds every query of the store today.
#[test]
fn every_query_that_orders_orders_by_version() {
    // mod.rs holds P1's queries; the factory table's are in factory.rs, checked below.
    let source = include_str!("mod.rs").to_uppercase();
    let mut found = 0;
    for (at, _) in source.match_indices("ORDER BY") {
        found += 1;
        let rest = source[at + "ORDER BY".len()..].trim_start();
        assert!(
            rest.starts_with("VERSION"),
            "orders by {}",
            &rest[..rest.len().min(40)]
        );
    }
    assert!(found > 0, "since() orders its result");
}

#[test]
fn store_opens_with_full_sync_and_temp_tables_in_memory() {
    let (_dir, store) = open();
    // FULL, not NORMAL: with the rollback journal a power cut under NORMAL can corrupt the file.
    let sync_mode: i64 = store
        .conn
        .pragma_query_value(None, "synchronous", |row| row.get(0))
        .unwrap();
    let temp_store: i64 = store
        .conn
        .pragma_query_value(None, "temp_store", |row| row.get(0))
        .unwrap();
    assert_eq!(sync_mode, 2); // FULL
    assert_eq!(temp_store, 2); // MEMORY
}

#[test]
fn settings_seq_tracks_applied_settings_without_reading_full_body() {
    let (_dir, mut store) = open();
    assert_eq!(store.settings_seq().unwrap(), 0);
    assert!(store.apply_settings(settings(4)).unwrap());
    assert_eq!(store.settings_seq().unwrap(), 4);
    assert!(!store.apply_settings(settings(2)).unwrap());
    assert_eq!(store.settings_seq().unwrap(), 4);
}

#[test]
fn settings_since_gives_the_settings_only_when_their_seq_is_not_the_one_seen() {
    let (_dir, mut store) = open();
    assert_eq!(store.settings_since(None).unwrap().map(|s| s.seq), Some(0));
    assert_eq!(store.settings_since(Some(0)).unwrap(), None);
    assert!(store.apply_settings(settings(4)).unwrap());
    assert_eq!(store.settings_since(Some(0)).unwrap(), Some(settings(4)));
    assert_eq!(store.settings_since(Some(4)).unwrap(), None);
}

#[test]
fn live_session_count_counts_only_live_sessions_of_that_owner() {
    let (_dir, mut store) = open();
    let me = MachineId::random();
    let peer = MachineId::random();
    let make_session =
        |id: u128, owner: MachineId, version: u64, state: SessionState, del: bool| {
            AnyRecord::Session(Record {
                owner,
                version,
                deleted: del,
                body: Session {
                    id: SessionId(uuid::Uuid::from_u128(id)),
                    workspace: WorkspaceId(uuid::Uuid::from_u128(1)),
                    terminal: TerminalId(uuid::Uuid::from_u128(3)),
                    model: "opus".into(),
                    state,
                },
            })
        };
    store.apply(&workspace(me, 1), me).unwrap();
    store.apply(&terminal(me, 2), me).unwrap();
    store.apply(&sample(me, 3), me).unwrap();
    for (idx, state) in [
        SessionState::Starting,
        SessionState::Working,
        SessionState::WaitingForYou,
        SessionState::Idle,
        SessionState::Stuck,
        SessionState::Finished,
        SessionState::Failed,
    ]
    .into_iter()
    .enumerate()
    {
        store
            .apply(
                &make_session(100 + idx as u128, me, 10 + idx as u64, state, false),
                me,
            )
            .unwrap();
    }
    store
        .apply(&make_session(200, me, 20, SessionState::Working, true), me)
        .unwrap();
    store
        .apply(
            &make_session(201, peer, 1, SessionState::Working, false),
            peer,
        )
        .unwrap();
    // Unreadable row of another kind does not affect live_session_count.
    insert_unreadable(&store, me, 30, "printer", "{}");

    assert_eq!(store.live_session_count(me).unwrap(), 5);
    assert_eq!(store.live_session_count(peer).unwrap(), 1);
    assert_eq!(store.live_session_count(MachineId::random()).unwrap(), 0);
}

/// One owner's sessions are read without its other records: a session row this build cannot
/// read is left out, and a row of another kind it cannot read is never parsed.
#[test]
fn sessions_of_gives_one_owners_sessions_not_deleted_and_leaves_out_a_row_it_cannot_read() {
    let (_dir, mut store) = open();
    let me = MachineId::random();
    let peer = MachineId::random();
    let numbered = |id: u128, owner: MachineId, version: u64| {
        let AnyRecord::Session(mut r) = session(owner, version) else {
            unreachable!()
        };
        r.body.id = SessionId(uuid::Uuid::from_u128(id));
        r
    };
    assert!(store.sessions_of(me).unwrap().is_empty());

    let first = numbered(40, me, 1);
    let mut gone = numbered(41, me, 2);
    gone.deleted = true;
    let mut failed = numbered(42, me, 3);
    failed.body.state = SessionState::Failed;
    for r in [&first, &gone, &failed] {
        store.apply(&AnyRecord::Session(r.clone()), me).unwrap();
    }
    store
        .apply(&AnyRecord::Session(numbered(43, peer, 1)), peer)
        .unwrap();
    store.apply(&terminal(me, 4), me).unwrap();
    insert_unreadable(
        &store,
        me,
        5,
        "session",
        r#"{"state":"from-a-newer-build"}"#,
    );
    insert_unreadable(&store, me, 6, "printer", "{}");

    assert_eq!(store.sessions_of(me).unwrap(), vec![first, failed]);
}

#[test]
fn apply_batch_stores_valid_prefix_skips_stale_and_reports_not_owner() {
    let (_dir, mut store) = open();
    let owner = MachineId::random();
    let other = MachineId::random();

    // Seed workspace at version 2 so version 1 is stale.
    store.apply(&workspace(owner, 2), owner).unwrap();

    let mut refused = Vec::new();
    let batch = [
        workspace(owner, 1),      // stale
        terminal(other, 1),       // not owner
        terminal(owner, 3),       // stored
        session(owner, u64::MAX), // error mid-batch
        sample(owner, 4),         // must not be stored after error
    ];
    let err = store
        .apply_batch(&batch, owner, |r| refused.push(r.owner()))
        .unwrap_err();
    assert!(matches!(err, StoreError::TooLarge { .. }), "{err}");
    assert_eq!(refused, vec![other]);
    assert_eq!(
        store.since(owner, 0).unwrap(),
        vec![workspace(owner, 2), terminal(owner, 3)]
    );
}

// Factory records.

fn run_id(repo: &str, issue: u64, attempt: u32) -> RunId {
    RunId::new(repo, issue, attempt).unwrap()
}

fn fr(owner: MachineId, version: u64, body: FactoryBody) -> FactoryRecord {
    FactoryRecord {
        owner,
        version,
        body,
    }
}

/// A run event whose `ts` and `ms` are given, so a test can make the times disagree with the
/// versions.
fn run_event(
    owner: MachineId,
    version: u64,
    run: &RunId,
    generation: u64,
    ms: u64,
) -> FactoryRecord {
    fr(
        owner,
        version,
        FactoryBody::RunEvent(RunEvent {
            run: run.clone(),
            generation,
            ts: format!("2026-10-04T10:00:{:02}.000Z", ms % 60),
            ms,
            event: "step_changed".into(),
            data: serde_json::json!({"step": "red"}),
            cause: None,
        }),
    )
}

fn status_body(project: ProjectId, pass: u64, generation: u64) -> FactoryStatus {
    FactoryStatus {
        project,
        pass,
        holds: true,
        generation,
        not_holding: None,
        idle: None,
        owner_ignored: false,
        ready: vec![],
        feedback_waiting: vec![],
        runs: vec![],
        held_up: vec![],
        gate_infra: vec![],
        finished: vec![],
        foreign: vec![],
    }
}

fn status(
    owner: MachineId,
    version: u64,
    project: ProjectId,
    pass: u64,
    generation: u64,
) -> FactoryRecord {
    fr(
        owner,
        version,
        FactoryBody::Status(status_body(project, pass, generation)),
    )
}

fn held_body(logins: &[&str]) -> MachineHeld {
    MachineHeld {
        logins: logins.iter().map(|l| (*l).to_owned()).collect(),
        skill: true,
    }
}

fn held(owner: MachineId, version: u64, logins: &[&str]) -> FactoryRecord {
    fr(owner, version, FactoryBody::Held(held_body(logins)))
}

fn turn_body(session: SessionId, turn: u64) -> SessionTurn {
    SessionTurn {
        session,
        workspace: WorkspaceId(uuid::Uuid::from_u128(1)),
        project: ProjectId(uuid::Uuid::from_u128(2)),
        turn,
        prompts: turn,
        question: false,
    }
}

fn turn_record(owner: MachineId, version: u64, session: SessionId, turn: u64) -> FactoryRecord {
    fr(owner, version, FactoryBody::Turn(turn_body(session, turn)))
}

fn every_factory_kind(owner: MachineId, version: u64) -> Vec<FactoryRecord> {
    vec![
        run_event(owner, version, &run_id("a/b", 1, 1), 1, 5),
        status(owner, version, ProjectId(uuid::Uuid::from_u128(2)), 1, 1),
        held(owner, version, &["tomkaczocha"]),
        turn_record(owner, version, SessionId(uuid::Uuid::from_u128(4)), 1),
    ]
}

fn factory_rows(store: &Store) -> i64 {
    store
        .conn
        .query_row("SELECT COUNT(*) FROM factory_records", [], |row| row.get(0))
        .unwrap()
}

#[test]
fn apply_factory_stores_every_kind_from_its_owner() {
    let (_dir, mut store) = open();
    let owner = MachineId::random();
    for record in every_factory_kind(owner, 1) {
        assert_eq!(
            store.apply_factory(&record, owner).unwrap(),
            Applied::Stored,
            "{record:?}"
        );
    }
    assert_eq!(factory_rows(&store), 4);
    assert_eq!(
        store.factory_since(owner, 0, usize::MAX).unwrap(),
        every_factory_kind(owner, 1)
    );
}

#[test]
fn apply_factory_refuses_a_record_from_a_machine_that_is_not_its_owner() {
    let (_dir, mut store) = open();
    let owner = MachineId::random();
    let other = MachineId::random();
    for record in every_factory_kind(owner, 1) {
        assert_eq!(
            store.apply_factory(&record, other).unwrap(),
            Applied::NotOwner
        );
    }
    assert_eq!(factory_rows(&store), 0);
}

#[test]
fn apply_factory_refuses_to_take_a_key_another_owner_holds() {
    let (_dir, mut store) = open();
    let first = MachineId::random();
    let second = MachineId::random();
    let session = SessionId::random();
    let theirs = turn_record(first, 1, session, 1);
    assert_eq!(
        store.apply_factory(&theirs, first).unwrap(),
        Applied::Stored
    );
    let claim = turn_record(second, 9, session, 2);
    assert_eq!(
        store.apply_factory(&claim, second).unwrap(),
        Applied::NotOwner
    );
    assert_eq!(factory_rows(&store), 1);
    assert_eq!(
        store.session_turn(session).unwrap(),
        Some(turn_body(session, 1))
    );
    assert!(store.factory_since(second, 0, 10).unwrap().is_empty());
}

#[test]
fn apply_factory_takes_only_a_higher_version() {
    let (_dir, mut store) = open();
    let owner = MachineId::random();
    let project = ProjectId::random();
    let session = SessionId::random();
    let status_at = |version, pass| status(owner, version, project, pass, 1);
    let turn_at = |version, turn| turn_record(owner, version, session, turn);

    assert_eq!(
        store.apply_factory(&status_at(5, 10), owner).unwrap(),
        Applied::Stored
    );
    assert_eq!(
        store.apply_factory(&turn_at(6, 1), owner).unwrap(),
        Applied::Stored
    );
    for version in [5, 4, 1] {
        assert_eq!(
            store.apply_factory(&status_at(version, 99), owner).unwrap(),
            Applied::Stale
        );
        assert_eq!(
            store
                .apply_factory(&turn_at(version + 1, 99), owner)
                .unwrap(),
            Applied::Stale
        );
    }
    assert_eq!(store.statuses(project).unwrap()[0].1.pass, 10);
    assert_eq!(store.session_turn(session).unwrap().unwrap().turn, 1);

    assert_eq!(
        store.apply_factory(&status_at(6, 11), owner).unwrap(),
        Applied::Stored
    );
    assert_eq!(
        store.apply_factory(&turn_at(7, 2), owner).unwrap(),
        Applied::Stored
    );
    assert_eq!(store.statuses(project).unwrap()[0].1.pass, 11);
    assert_eq!(store.session_turn(session).unwrap().unwrap().turn, 2);
    assert_eq!(factory_rows(&store), 2);
}

#[test]
fn applying_a_factory_record_twice_changes_nothing() {
    let (_dir, mut store) = open();
    let owner = MachineId::random();
    for record in every_factory_kind(owner, 3) {
        assert_eq!(
            store.apply_factory(&record, owner).unwrap(),
            Applied::Stored
        );
        assert_eq!(store.apply_factory(&record, owner).unwrap(), Applied::Stale);
    }
    assert_eq!(factory_rows(&store), 4);
    assert_eq!(
        store.factory_since(owner, 0, usize::MAX).unwrap(),
        every_factory_kind(owner, 3)
    );
}

#[test]
fn apply_factory_refuses_a_version_above_what_the_store_can_hold() {
    let (_dir, mut store) = open();
    let owner = MachineId::random();
    let record = held(owner, 1u64 << 63, &["a"]);
    assert!(matches!(
        store.apply_factory(&record, owner),
        Err(StoreError::TooLarge { .. })
    ));
    assert_eq!(factory_rows(&store), 0);
}

#[test]
fn own_next_factory_version_rises_and_survives_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("foundry.db");
    let mut store = Store::open(&path).unwrap();
    let first = store.own_next_factory_version().unwrap();
    let second = store.own_next_factory_version().unwrap();
    assert!(second > first);
    drop(store);
    let mut store = Store::open(&path).unwrap();
    assert!(store.own_next_factory_version().unwrap() > second);
}

#[test]
fn the_factory_version_and_the_p1_version_count_apart() {
    let (_dir, mut store) = open();
    let p1: Vec<u64> = (0..3).map(|_| store.own_next_version().unwrap()).collect();
    assert_eq!(p1, [1, 2, 3]);
    let first = store.own_next_factory_version().unwrap();
    assert_eq!(store.own_next_factory_version().unwrap(), first + 1);
    // The factory counter did not move P1's.
    assert_eq!(store.own_next_version().unwrap(), 4);
    let next = store.own_next_factory_version().unwrap();
    assert_eq!(next, first + 2);
}

#[test]
fn factory_since_gives_one_owners_records_above_a_version_lowest_first_up_to_a_limit() {
    let (_dir, mut store) = open();
    let owner = MachineId::random();
    let other = MachineId::random();
    for version in [4, 2, 5, 3, 1] {
        let record = held(owner, version, &["x"]);
        store.apply_factory(&record, owner).unwrap();
    }
    let project = ProjectId::random();
    store
        .apply_factory(&status(other, 2, project, 1, 1), other)
        .unwrap();
    // One key per owner for `held`, so only the last stored version of it remains.
    assert_eq!(
        store.factory_since(owner, 0, 10).unwrap(),
        vec![held(owner, 5, &["x"])]
    );

    let run = run_id("a/b", 1, 1);
    for version in [10, 8, 9, 7] {
        let record = run_event(owner, version, &run, 1, 1);
        store.apply_factory(&record, owner).unwrap();
    }
    let versions =
        |records: Vec<FactoryRecord>| -> Vec<u64> { records.iter().map(|r| r.version).collect() };
    assert_eq!(
        versions(store.factory_since(owner, 0, 100).unwrap()),
        [5, 7, 8, 9, 10]
    );
    assert_eq!(
        versions(store.factory_since(owner, 7, 100).unwrap()),
        [8, 9, 10]
    );
    assert_eq!(versions(store.factory_since(owner, 0, 2).unwrap()), [5, 7]);
    assert_eq!(versions(store.factory_since(other, 0, 100).unwrap()), [2]);
    assert!(store.factory_since(owner, 10, 100).unwrap().is_empty());
}

#[test]
fn factory_since_fails_on_a_row_of_a_kind_it_does_not_know() {
    let (_dir, mut store) = open();
    let owner = MachineId::random();
    store.apply_factory(&held(owner, 1, &["a"]), owner).unwrap();
    store
        .conn
        .execute(
            "INSERT INTO factory_records (kind, id, owner, version, record)
             VALUES ('mystery', 'x', ?1, 2, ?2)",
            params![
                owner.to_string(),
                format!(r#"{{"owner":"{owner}","version":2,"kind":"mystery","body":{{}}}}"#)
            ],
        )
        .unwrap();
    assert!(matches!(
        store.factory_since(owner, 0, 10),
        Err(StoreError::Json(_))
    ));
    // A limit that stops before the row does not read it.
    assert_eq!(store.factory_since(owner, 0, 1).unwrap().len(), 1);
}

#[test]
fn factory_versions_gives_the_highest_per_owner_from_the_factory_table_only() {
    let (_dir, mut store) = open();
    let one = MachineId::random();
    let two = MachineId::random();
    let three = MachineId::random();
    assert!(store.factory_versions().unwrap().is_empty());
    store.apply(&workspace(three, 77), three).unwrap();
    store.apply_factory(&held(one, 3, &["a"]), one).unwrap();
    store
        .apply_factory(&run_event(one, 8, &run_id("a/b", 1, 1), 1, 1), one)
        .unwrap();
    store.apply_factory(&held(two, 2, &["a"]), two).unwrap();
    let versions = store.factory_versions().unwrap();
    assert_eq!(versions, BTreeMap::from([(one, 8), (two, 2)]));
    // And P1's `versions` does not show the factory owners.
    assert_eq!(store.versions().unwrap(), BTreeMap::from([(three, 77)]));
}

#[test]
fn p1_reads_never_return_or_count_a_factory_record() {
    let (_dir, mut store) = open();
    let owner = MachineId::random();
    let session_id = SessionId(uuid::Uuid::from_u128(4));
    store.apply(&session(owner, 1), owner).unwrap();
    let before_since = store.since(owner, 0).unwrap();
    let before_versions = store.versions().unwrap();
    let before_live = store.live_session_count(owner).unwrap();
    assert_eq!(before_live, 1);

    let factory_owner = MachineId::random();
    for record in every_factory_kind(owner, 50) {
        store.apply_factory(&record, owner).unwrap();
    }
    // A turn for the owner's session from a second machine is refused, not stored.
    store
        .apply_factory(
            &turn_record(factory_owner, 60, session_id, 3),
            factory_owner,
        )
        .unwrap();
    assert_eq!(store.since(owner, 0).unwrap(), before_since);
    assert!(store.since(factory_owner, 0).unwrap().is_empty());
    assert_eq!(store.versions().unwrap(), before_versions);
    assert_eq!(store.live_session_count(owner).unwrap(), before_live);
    assert_eq!(store.live_session_count(factory_owner).unwrap(), 0);
    assert!(store.sessions_of(factory_owner).unwrap().is_empty());
}

#[test]
fn run_events_read_back_in_generation_order_whatever_the_versions_and_times() {
    let (_dir, mut store) = open();
    let first = MachineId::random();
    let second = MachineId::random();
    let run = run_id("a/b", 1, 1);
    // Generation 2 has the lower version and the earlier time.
    let late = run_event(first, 9, &run, 1, 9_000);
    let early = run_event(second, 1, &run, 2, 1_000);
    store.apply_factory(&early, second).unwrap();
    store.apply_factory(&late, first).unwrap();
    let events = store.run_events(&run).unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(
        events.iter().map(|e| e.generation).collect::<Vec<_>>(),
        [1, 2]
    );
    assert_eq!(events[0].ms, 9_000);
}

#[test]
fn run_events_of_one_generation_come_back_in_version_order() {
    let (_dir, mut store) = open();
    let owner = MachineId::random();
    let run = run_id("a/b", 1, 1);
    // Applied out of order, with times that run the other way.
    for (version, ms) in [(3, 100), (1, 300), (2, 200)] {
        let record = run_event(owner, version, &run, 4, ms);
        assert_eq!(
            store.apply_factory(&record, owner).unwrap(),
            Applied::Stored
        );
    }
    let events = store.run_events(&run).unwrap();
    assert_eq!(
        events.iter().map(|e| e.ms).collect::<Vec<_>>(),
        [300, 200, 100]
    );
}

#[test]
fn run_events_hold_one_run_and_only_events() {
    let (_dir, mut store) = open();
    let owner = MachineId::random();
    let run = run_id("a/b", 1, 1);
    let longer = run_id("a/b", 1, 10);
    assert_eq!(longer.to_string(), format!("{run}0"));
    let mut version = 0;
    let mut put = |store: &mut Store, record: FactoryRecord| {
        store.apply_factory(&record, owner).unwrap();
    };
    for (run, generation) in [(&run, 1), (&longer, 1), (&run, 2)] {
        version += 1;
        put(&mut store, run_event(owner, version, run, generation, 1));
    }
    put(&mut store, held(owner, 10, &["a"]));
    put(&mut store, status(owner, 11, ProjectId::random(), 1, 1));
    let events = store.run_events(&run).unwrap();
    assert_eq!(events.len(), 2);
    assert!(events.iter().all(|e| e.run == run));
    let longer_events = store.run_events(&longer).unwrap();
    assert_eq!(longer_events.len(), 1);
    assert!(store.run_events(&run_id("a/b", 2, 1)).unwrap().is_empty());
}

#[test]
fn runs_of_gives_each_run_of_the_repo_once_in_order() {
    let (_dir, mut store) = open();
    let owner = MachineId::random();
    let other = MachineId::random();
    let mut version = 0;
    let mut put = |store: &mut Store, from: MachineId, run: &RunId, generation| {
        version += 1;
        store
            .apply_factory(&run_event(from, version, run, generation, 1), from)
            .unwrap();
    };
    let two = run_id("Rawphp/SoftFac", 2, 1);
    let ten = run_id("rawphp/softfac", 10, 1);
    put(&mut store, owner, &two, 1);
    put(&mut store, owner, &two, 1);
    put(&mut store, other, &two, 2);
    put(&mut store, owner, &ten, 1);
    put(&mut store, owner, &run_id("rawphp/softfac-extra", 3, 1), 1);
    put(&mut store, owner, &run_id("rawphp/soft", 4, 1), 1);
    put(&mut store, owner, &run_id("other/softfac", 5, 1), 1);
    let expected = vec![two.clone(), ten.clone()];
    assert_eq!(store.runs_of("rawphp/softfac").unwrap(), expected);
    assert_eq!(store.runs_of("RawPHP/SoftFac").unwrap(), expected);
    assert!(store.runs_of("rawphp/none").unwrap().is_empty());
}

#[test]
fn statuses_gives_every_writers_status_of_the_project_by_machine() {
    let (_dir, mut store) = open();
    let project = ProjectId::random();
    let elsewhere = ProjectId::random();
    let mut machines = [
        MachineId::random(),
        MachineId::random(),
        MachineId::random(),
    ];
    for (at, machine) in machines.iter().enumerate() {
        let record = status(*machine, 1, project, at as u64, 1);
        store.apply_factory(&record, *machine).unwrap();
    }
    store
        .apply_factory(&status(machines[0], 2, elsewhere, 7, 1), machines[0])
        .unwrap();
    let statuses = store.statuses(project).unwrap();
    machines.sort();
    assert_eq!(
        statuses.iter().map(|(m, _)| *m).collect::<Vec<_>>(),
        machines
    );
    assert!(statuses.iter().all(|(_, s)| s.project == project));
    assert_eq!(store.statuses(elsewhere).unwrap().len(), 1);
    assert!(store.statuses(ProjectId::random()).unwrap().is_empty());
}

#[test]
fn highest_generation_is_the_highest_any_writer_has_for_the_project() {
    let (_dir, mut store) = open();
    let project = ProjectId::random();
    let elsewhere = ProjectId::random();
    let one = MachineId::random();
    let two = MachineId::random();
    assert_eq!(store.highest_generation(project).unwrap(), 0);
    store
        .apply_factory(&status(one, 1, project, 1, 3), one)
        .unwrap();
    store
        .apply_factory(&status(two, 1, project, 1, 7), two)
        .unwrap();
    store
        .apply_factory(&status(one, 2, elsewhere, 1, 90), one)
        .unwrap();
    assert_eq!(store.highest_generation(project).unwrap(), 7);
    assert_eq!(store.highest_generation(elsewhere).unwrap(), 90);
    // A run event's generation is not a status's.
    store
        .apply_factory(&run_event(one, 3, &run_id("a/b", 1, 1), 99, 1), one)
        .unwrap();
    assert_eq!(store.highest_generation(project).unwrap(), 7);
}

#[test]
fn held_and_session_turn_are_read_by_their_key() {
    let (_dir, mut store) = open();
    let owner = MachineId::random();
    let other = MachineId::random();
    let session = SessionId::random();
    assert_eq!(store.held(owner).unwrap(), None);
    assert_eq!(store.session_turn(session).unwrap(), None);
    store
        .apply_factory(&held(owner, 1, &["tomkaczocha", "rawphp"]), owner)
        .unwrap();
    store
        .apply_factory(&turn_record(owner, 2, session, 6), owner)
        .unwrap();
    assert_eq!(
        store.held(owner).unwrap(),
        Some(held_body(&["tomkaczocha", "rawphp"]))
    );
    assert_eq!(store.held(other).unwrap(), None);
    assert_eq!(
        store.session_turn(session).unwrap(),
        Some(turn_body(session, 6))
    );
    assert_eq!(store.session_turn(SessionId::random()).unwrap(), None);
}

#[test]
fn the_factory_migration_runs_on_an_empty_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("foundry.db");
    std::fs::File::create(&path).unwrap();
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.schema_version().unwrap(), MIGRATIONS.len());
    assert!(MIGRATIONS.len() >= 2);
    let owner = MachineId::random();
    assert_eq!(
        store.apply_factory(&held(owner, 1, &[]), owner).unwrap(),
        Applied::Stored
    );
    assert_eq!(store.own_next_factory_version().unwrap(), 1);
}

#[test]
fn the_factory_migration_runs_on_a_p1_database_and_leaves_records_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("foundry.db");
    let owner = MachineId::random();
    let snapshot = |conn: &rusqlite::Connection| -> Vec<(String, String, String, i64, String)> {
        let mut query = conn
            .prepare("SELECT kind, id, owner, version, record FROM records ORDER BY kind, id")
            .unwrap();
        query
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    let before;
    {
        let mut conn = rusqlite::Connection::open(&path).unwrap();
        migrate(&mut conn, 1).unwrap();
        let mut p1 = Store { conn };
        for record in every_kind(owner, 5) {
            assert_eq!(p1.apply(&record, owner).unwrap(), Applied::Stored);
        }
        assert_eq!(p1.own_next_version().unwrap(), 1);
        before = snapshot(&p1.conn);
        assert_eq!(before.len(), 4);
        assert_eq!(user_version(&p1.conn).unwrap(), 1);
    }
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.schema_version().unwrap(), MIGRATIONS.len());
    assert_eq!(snapshot(&store.conn), before);
    assert_eq!(store.own_next_version().unwrap(), 2);
    assert_eq!(store.since(owner, 0).unwrap(), every_kind(owner, 5));
    for record in every_factory_kind(owner, 1) {
        assert_eq!(
            store.apply_factory(&record, owner).unwrap(),
            Applied::Stored
        );
    }
    assert_eq!(snapshot(&store.conn), before);
}

#[test]
fn the_factory_queries_order_only_by_a_generation_a_version_or_an_owner() {
    let source = include_str!("factory.rs");
    assert!(!source.contains("$.body.ts"));
    assert!(!source.contains("$.body.ms"));
    let mut found = 0;
    for (at, _) in source.match_indices("ORDER BY") {
        found += 1;
        let rest = &source[at + "ORDER BY".len()..];
        let clause = rest.split(['"', ';']).next().unwrap();
        // A string continued with `\` runs on to the next line.
        let clause = clause.replace("\\\n", " ");
        for term in clause.split(',') {
            let term = term.trim();
            let allowed = [
                "version",
                "owner",
                "json_extract(record, '$.body.generation')",
            ];
            assert!(allowed.contains(&term), "orders by {term:?}");
        }
    }
    assert!(found > 0, "run_events orders its result");
}
