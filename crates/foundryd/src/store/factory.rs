//! The factory table, its versions and its reads.
//!
//! Factory records are kept apart from P1's `records`, under the same rules: one is taken only
//! from its owner and only when its version is higher than the copy held. The versions are the
//! owner's own factory counter, which is not P1's. Nothing here reads or orders by a time:
//! a run's events are ordered by the lease generation they were written under, then by version.
//!
//! Every row is written under [`FactoryRecord::key`], and the reads below spell out its kind
//! names and id layouts (`<run>/<owner>/<version>`, `<project>/<owner>`, the machine,
//! `<session>/<owner>`) in their queries. Each read's tests store records through
//! `apply_factory`, which keys them by `FactoryRecord::key`, and read them back here, so a
//! change to a kind name or an id layout in `foundry-proto` fails those tests rather than
//! leaving a read empty.

use std::collections::{BTreeMap, BTreeSet};

use foundry_proto::factory::{
    FactoryBody, FactoryRecord, FactoryStatus, MachineHeld, RunEvent, RunId, SessionTurn,
};
use foundry_proto::ids::{MachineId, ProjectId, SessionId};
use rusqlite::types::Type;
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use super::{Applied, Store, StoreError, from_sql, to_sql};

/// The first key above every id that starts with `prefix/`: `0` is the character after `/`.
/// A run or project id is read as a range of the key, never with `LIKE`, whose `_` matches
/// any character.
fn range(prefix: &str) -> (String, String) {
    (format!("{prefix}/"), format!("{prefix}0"))
}

/// The error for a row stored under `kind` whose record is another kind. `apply_factory` never
/// writes one, since it keys each record by its own kind.
fn wrong_kind(kind: &str, body: &FactoryBody) -> StoreError {
    StoreError::Sqlite(rusqlite::Error::FromSqlConversionFailure(
        0,
        Type::Text,
        format!("a {kind} row holds a {} record", body.kind()).into(),
    ))
}

impl Store {
    /// Stores `record`, received from machine `from`, when `from` owns it, no other machine
    /// holds its key and its version is higher than the copy held. Same rules as
    /// [`apply`](Self::apply).
    pub fn apply_factory(
        &mut self,
        record: &FactoryRecord,
        from: MachineId,
    ) -> Result<Applied, StoreError> {
        if record.owner != from {
            return Ok(Applied::NotOwner);
        }
        let tx = self.conn.transaction()?;
        let applied = apply_factory_in_tx(&tx, record)?;
        if applied == Applied::Stored {
            tx.commit()?;
        }
        Ok(applied)
    }

    /// The version for this machine's next write of a factory record. Each call gives a
    /// higher number than the last, across restarts too, and the count is not P1's.
    pub fn own_next_factory_version(&mut self) -> Result<u64, StoreError> {
        let mut stmt = self.conn.prepare_cached(
            "INSERT INTO own_factory_version (only, version) VALUES (1, 1)
             ON CONFLICT (only) DO UPDATE SET version = version + 1 RETURNING version",
        )?;
        let version: i64 = stmt.query_row([], |row| row.get(0))?;
        Ok(from_sql(version))
    }

    /// The first `limit` factory records held from `owner` with a version above `version`,
    /// lowest version first. A stored row this build cannot parse is an error, never skipped.
    pub fn factory_since(
        &self,
        owner: MachineId,
        version: u64,
        limit: usize,
    ) -> Result<Vec<FactoryRecord>, StoreError> {
        // Nothing stored is above i64::MAX, so a larger `version` correctly finds nothing.
        let version = i64::try_from(version).unwrap_or(i64::MAX);
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut query = self.conn.prepare_cached(
            "SELECT record FROM factory_records WHERE owner = ?1 AND version > ?2 ORDER BY version \
             LIMIT ?3",
        )?;
        let rows = query.query_map(params![owner.to_string(), version, limit], |row| {
            row.get::<_, String>(0)
        })?;
        let mut records = Vec::new();
        for json in rows {
            records.push(serde_json::from_str(&json?)?);
        }
        Ok(records)
    }

    /// The highest factory version held from each owner.
    ///
    /// An owner this build cannot read as a machine id, which only a corrupt file or a newer
    /// build's id format leaves, is left out: one such row must not make the machine's hello
    /// say it holds no factory record, which would have every peer send it all of theirs again
    /// on every link. Each such owner is warned of once while the store is open
    /// (`warn_unreadable_owner`).
    pub fn factory_versions(&self) -> Result<BTreeMap<MachineId, u64>, StoreError> {
        let mut query = self
            .conn
            .prepare_cached("SELECT owner, MAX(version) FROM factory_records GROUP BY owner")?;
        let rows = query.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, from_sql(row.get(1)?)))
        })?;
        let mut versions = BTreeMap::new();
        for row in rows {
            let (owner, version) = row?;
            match owner.parse() {
                Ok(id) => {
                    versions.insert(MachineId(id), version);
                }
                Err(_) => warn_unreadable_owner(&self.conn, &owner),
            }
        }
        Ok(versions)
    }

    /// The events of `run`, from every writer, by the lease generation they were written under,
    /// then by version, then by owner so that equal versions read the same everywhere.
    pub fn run_events(&self, run: &RunId) -> Result<Vec<RunEvent>, StoreError> {
        let (from, to) = range(&run.to_string());
        let mut query = self.conn.prepare_cached(
            "SELECT record FROM factory_records WHERE kind = 'run_event' AND id >= ?1 AND id < ?2 \
             ORDER BY json_extract(record, '$.body.generation'), version, owner",
        )?;
        let rows = query.query_map(params![from, to], |row| row.get::<_, String>(0))?;
        let mut events = Vec::new();
        for json in rows {
            let record: FactoryRecord = serde_json::from_str(&json?)?;
            match record.body {
                FactoryBody::RunEvent(event) => events.push(event),
                other => return Err(wrong_kind("run_event", &other)),
            }
        }
        Ok(events)
    }

    /// Every run of `repo` (`owner/name`, in any case) that has an event, once each, in order.
    /// A repo that is not a legal one has no runs.
    pub fn runs_of(&self, repo: &str) -> Result<Vec<RunId>, StoreError> {
        // Normalise as a run id does, so the argument folds exactly as stored repos did.
        let Ok(probe) = RunId::new(repo, 0, 0) else {
            return Ok(Vec::new());
        };
        let repo = probe.repo();
        // A run event's id is `<run>/<owner>/<version>`, and a run id holds no `/`, so the run
        // is the id up to its first `/`: no record is parsed. Every run of the repo starts with
        // `<owner>__<name>__`, so the keys lie in [prefix, prefix with its last `_` raised to
        // the next character, a backtick). The range is not exact (a repo name may hold `__`,
        // so `a/b` also reaches the runs of `a/b__c`), so the repo of each run is still checked.
        let prefix = probe.to_string();
        let prefix = &prefix[..prefix.len() - "0__0".len()];
        let from = prefix.to_owned();
        let to = format!("{}`", &prefix[..prefix.len() - 1]);
        let mut query = self.conn.prepare_cached(
            "SELECT DISTINCT substr(id, 1, instr(id, '/') - 1) FROM factory_records
             WHERE kind = 'run_event' AND id >= ?1 AND id < ?2",
        )?;
        let rows = query.query_map(params![from, to], |row| row.get::<_, String>(0))?;
        let mut runs = BTreeSet::new();
        for text in rows {
            let text = text?;
            let run: RunId = text.parse().map_err(|e: String| {
                rusqlite::Error::FromSqlConversionFailure(0, Type::Text, e.into())
            })?;
            if run.repo() == repo {
                runs.insert(run);
            }
        }
        Ok(runs.into_iter().collect())
    }

    /// Every writer's factory status for `project`, by machine.
    ///
    /// A status this build cannot read, one a newer build left before a rollback, is left out
    /// with a warning, as [`sessions_of`](Self::sessions_of) does, so one writer's status
    /// never hides the others'; so is a status row that holds another kind of record.
    /// [`highest_generation`](Self::highest_generation) still counts its generation. The other
    /// reads give an error for such a row instead: [`run_events`](Self::run_events) because a
    /// run with an event left out reads as another run, and [`held`](Self::held) and
    /// [`session_turn`](Self::session_turn) because they read one record, which would then
    /// read as absent.
    pub fn statuses(
        &self,
        project: ProjectId,
    ) -> Result<Vec<(MachineId, FactoryStatus)>, StoreError> {
        let (from, to) = range(&project.to_string());
        let mut query = self.conn.prepare_cached(
            "SELECT owner, version, record FROM factory_records WHERE kind = 'factory_status' \
             AND id >= ?1 AND id < ?2",
        )?;
        let rows = query.query_map(params![from, to], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        let mut statuses = Vec::new();
        for row in rows {
            let (owner, version, json) = row?;
            match serde_json::from_str::<FactoryRecord>(&json) {
                Ok(FactoryRecord {
                    owner,
                    body: FactoryBody::Status(status),
                    ..
                }) => statuses.push((owner, status)),
                Ok(record) => {
                    let kind = record.body.kind();
                    tracing::warn!(%project, %owner, version, kind, "a factory status row that holds another kind of record is left out");
                }
                Err(error) => {
                    tracing::warn!(%project, %owner, version, %error, "a factory status this build cannot read is left out");
                }
            }
        }
        statuses.sort_by_key(|(machine, _)| *machine);
        Ok(statuses)
    }

    /// The highest lease generation any writer's status for `project` holds, or `0` with none.
    ///
    /// Only the generation of each status row is read, so a status
    /// [`statuses`](Self::statuses) leaves out still counts: a lease checked against a lower
    /// generation could be taken from the writer that holds the higher one. A row whose
    /// generation is not a whole number from 0 up is an error, since the highest is then not
    /// known.
    pub fn highest_generation(&self, project: ProjectId) -> Result<u64, StoreError> {
        let (from, to) = range(&project.to_string());
        let mut query = self.conn.prepare_cached(
            "SELECT json_extract(record, '$.body.generation') FROM factory_records \
             WHERE kind = 'factory_status' AND id >= ?1 AND id < ?2",
        )?;
        let rows = query.query_map(params![from, to], |row| {
            let generation: i64 = row.get(0)?;
            u64::try_from(generation).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(0, Type::Integer, Box::new(e))
            })
        })?;
        let mut highest = 0;
        for generation in rows {
            highest = highest.max(generation?);
        }
        Ok(highest)
    }

    /// The logins `machine` holds a token for, read by its key.
    pub fn held(&self, machine: MachineId) -> Result<Option<MachineHeld>, StoreError> {
        match self.factory_by_key("machine_held", &machine.to_string())? {
            Some(FactoryBody::Held(held)) => Ok(Some(held)),
            Some(other) => Err(wrong_kind("machine_held", &other)),
            None => Ok(None),
        }
    }

    /// The newest turn of `session` written by the machine that owns P1's record of it, read
    /// by its key. A turn from any other machine is never the session's. A turn and P1's
    /// session record sync apart, so a turn held before the record is stored and reads as
    /// `None` until the record says whose session it is.
    pub fn session_turn(&self, session: SessionId) -> Result<Option<SessionTurn>, StoreError> {
        let owner: Option<String> = self
            .conn
            .prepare_cached("SELECT owner FROM records WHERE kind = 'session' AND id = ?1")?
            .query_row(params![session.to_string()], |row| row.get(0))
            .optional()?;
        let Some(owner) = owner else {
            return Ok(None);
        };
        match self.factory_by_key("session_turn", &format!("{session}/{owner}"))? {
            Some(FactoryBody::Turn(turn)) => Ok(Some(turn)),
            Some(other) => Err(wrong_kind("session_turn", &other)),
            None => Ok(None),
        }
    }

    fn factory_by_key(&self, kind: &str, id: &str) -> Result<Option<FactoryBody>, StoreError> {
        let mut query = self
            .conn
            .prepare_cached("SELECT record FROM factory_records WHERE kind = ?1 AND id = ?2")?;
        let json: Option<String> = query
            .query_row(params![kind, id], |row| row.get(0))
            .optional()?;
        let Some(json) = json else {
            return Ok(None);
        };
        let record: FactoryRecord = serde_json::from_str(&json)?;
        Ok(Some(record.body))
    }
}

/// Warns that a factory row whose owner this build cannot read is left out of the versions
/// held, once per owner while `conn` is open, which is once a start: the owners warned of are
/// kept in a table of the connection's own, which goes when it closes. Should that table fail,
/// the warning goes anyway. The line holds nothing of the row: not the owner, not its version,
/// and not the parse error, which quotes the owner.
fn warn_unreadable_owner(conn: &Connection, owner: &str) {
    let inserted = conn
        .execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS factory_owners_warned (owner TEXT PRIMARY KEY)",
        )
        .and_then(|()| {
            conn.prepare_cached(
                "INSERT OR IGNORE INTO temp.factory_owners_warned (owner) VALUES (?1)",
            )?
            .execute(params![owner])
        });
    if !matches!(inserted, Ok(0)) {
        tracing::warn!(
            "a factory row whose owner this build cannot read is left out of the factory \
             versions held"
        );
    }
}

fn apply_factory_in_tx(
    tx: &Transaction<'_>,
    record: &FactoryRecord,
) -> Result<Applied, StoreError> {
    let version = to_sql("version", record.version)?;
    // A generation is read back from the stored JSON as a whole number SQLite holds (by
    // `highest_generation` and the order of `run_events`), so one it cannot hold is refused
    // here, as a version is.
    match &record.body {
        FactoryBody::RunEvent(RunEvent { generation, .. })
        | FactoryBody::Status(FactoryStatus { generation, .. }) => {
            to_sql("generation", *generation)?;
        }
        FactoryBody::Held(_) | FactoryBody::Turn(_) => {}
    }
    let key = record.key();
    let owner = record.owner.to_string();

    let held: Option<(String, i64)> = tx
        .prepare_cached("SELECT owner, version FROM factory_records WHERE kind = ?1 AND id = ?2")?
        .query_row(params![key.kind, key.id], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .optional()?;
    match held {
        Some((held_owner, _)) if held_owner != owner => return Ok(Applied::NotOwner),
        Some((_, held_version)) if version <= held_version => return Ok(Applied::Stale),
        _ => {}
    }
    let json = serde_json::to_string(record)?;
    tx.prepare_cached(
        "INSERT INTO factory_records (kind, id, owner, version, record)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (kind, id) DO UPDATE SET version = excluded.version, record = excluded.record",
    )?
    .execute(params![key.kind, key.id, owner, version, json])?;
    Ok(Applied::Stored)
}

#[cfg(test)]
mod tests {
    use foundry_proto::factory::{FactoryBody, FactoryRecord, RunEvent, RunId};
    use foundry_proto::ids::MachineId;
    use rusqlite::params;
    use tempfile::TempDir;

    use crate::store::{Applied, Store};
    use crate::test_log::Logged;

    /// An owner this build cannot read, as a newer build's id format or a corrupt file leaves.
    /// It sorts after every machine id, so the owners read before it.
    const UNREADABLE: &str = "owner-from-a-newer-build";

    /// Another, that sorts before every machine id, so the owners read after it.
    const UNREADABLE_FIRST: &str = "-owner-of-a-corrupt-file";

    /// The whole warning line, so a field added to it fails the test.
    const WARNING: &str = "foundryd::store::factory: a factory row whose owner this build cannot read \
                           is left out of the factory versions held";

    fn open() -> (TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("foundry.db")).unwrap();
        (dir, store)
    }

    fn store_event(store: &mut Store, owner: MachineId, version: u64, event: &str) {
        let record = FactoryRecord {
            owner,
            version,
            body: FactoryBody::RunEvent(RunEvent {
                run: RunId::new("original-solutions/foundry", 192, 1).unwrap(),
                generation: 1,
                ts: "2026-11-24T05:16:30.000Z".into(),
                ms: 1_795_497_390_000,
                event: event.into(),
                data: serde_json::json!({ "step": event }),
                cause: None,
            }),
        };
        assert_eq!(
            store.apply_factory(&record, owner).unwrap(),
            Applied::Stored
        );
    }

    /// Makes every factory row of `owner` one whose owner reads as `unreadable`.
    fn spoil_owner(store: &Store, owner: MachineId, unreadable: &str) {
        let changed = store
            .conn
            .execute(
                "UPDATE factory_records SET owner = ?1 WHERE owner = ?2",
                params![unreadable, owner.to_string()],
            )
            .unwrap();
        assert!(changed > 0);
    }

    #[test]
    fn factory_versions_leaves_out_an_owner_that_cannot_be_read_and_gives_the_others() {
        let (_dir, mut store) = open();
        let (a, b) = (MachineId::random(), MachineId::random());
        let (first, last) = (MachineId::random(), MachineId::random());
        store_event(&mut store, a, 1, "a-1");
        store_event(&mut store, a, 2, "a-2");
        store_event(&mut store, b, 3, "b-3");
        store_event(&mut store, first, 4, "first-4");
        store_event(&mut store, last, 5, "last-5");
        // Owners are read in their order: one bad owner comes before both readable ones, and
        // one after, so a read that stops at a bad owner rather than going past it fails here.
        spoil_owner(&store, first, UNREADABLE_FIRST);
        spoil_owner(&store, last, UNREADABLE);

        assert_eq!(
            store.factory_versions().unwrap(),
            [(a, 2), (b, 3)].into(),
            "the readable owners' versions, and no error"
        );
    }

    #[test]
    fn an_owner_that_cannot_be_read_is_warned_of_once_per_start_with_nothing_of_its_row() {
        let (dir, mut store) = open();
        let (a, spoiled) = (MachineId::random(), MachineId::random());
        store_event(&mut store, a, 1, "a-1");
        store_event(&mut store, spoiled, 4242, "a-step-only-the-row-holds");
        spoil_owner(&store, spoiled, UNREADABLE);

        let logged = Logged::default();
        let _capture = logged.capture();
        for _ in 0..3 {
            assert_eq!(store.factory_versions().unwrap(), [(a, 1)].into());
        }
        let warnings = logged.warnings();
        assert_eq!(warnings.len(), 1, "once, not once per call: {warnings:?}");
        assert!(
            warnings[0].ends_with(WARNING),
            "the line holds only its message: {warnings:?}"
        );
        for row_data in [
            UNREADABLE,
            "4242",
            "a-step-only-the-row-holds",
            &spoiled.to_string(),
        ] {
            assert!(
                !warnings[0].contains(row_data),
                "the warning holds {row_data:?}: {warnings:?}"
            );
        }

        // The next start warns again, once.
        drop(store);
        let store = Store::open(dir.path().join("foundry.db")).unwrap();
        for _ in 0..3 {
            assert_eq!(store.factory_versions().unwrap(), [(a, 1)].into());
        }
        assert_eq!(logged.warnings().len(), 2, "{:?}", logged.warnings());
    }
}
