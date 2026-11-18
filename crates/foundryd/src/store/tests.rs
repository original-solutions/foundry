use std::collections::BTreeMap;

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

/// A rollback starts the build before a newer one that already migrated the file, so a
/// file with steps this build does not know still opens and works.
#[test]
fn a_file_a_newer_build_migrated_still_opens() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("foundry.db");
    let owner = MachineId::random();
    {
        let mut store = Store::open(&path).unwrap();
        store.apply(&workspace(owner, 1), owner).unwrap();
        store
            .conn
            .pragma_update(None, "user_version", MIGRATIONS.len() as i64 + 1)
            .unwrap();
    }
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.schema_version().unwrap(), MIGRATIONS.len() + 1);
    assert_eq!(
        store.apply(&workspace(owner, 2), owner).unwrap(),
        Applied::Stored
    );
    assert_eq!(store.since(owner, 0).unwrap(), vec![workspace(owner, 2)]);
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

/// Review focus 3, checked in the queries themselves: every `ORDER BY` in this module
/// orders by a version.
#[test]
fn every_query_that_orders_orders_by_version() {
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
