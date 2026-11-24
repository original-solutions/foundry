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
use rusqlite::{OptionalExtension, Transaction, params};

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

fn parse_machine(index: usize, text: &str) -> rusqlite::Result<MachineId> {
    text.parse()
        .map(MachineId)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(index, Type::Text, Box::new(e)))
}

/// Counts the run events a read of this thread hands back, for tests that show a read is
/// bounded. Counted by thread, since each test runs on a thread of its own.
#[cfg(test)]
mod counted {
    use std::cell::Cell;

    thread_local! {
        static READ: Cell<usize> = const { Cell::new(0) };
    }

    pub(super) fn add(events: usize) {
        READ.with(|read| read.set(read.get() + events));
    }

    pub(super) fn read() -> usize {
        READ.with(Cell::get)
    }
}

#[cfg(not(test))]
mod counted {
    pub(super) fn add(_events: usize) {}
}

/// The run events stored under the JSON of `rows`, in their order. A row that holds another
/// kind of record is an error.
fn run_events_of(
    rows: impl Iterator<Item = rusqlite::Result<String>>,
) -> Result<Vec<RunEvent>, StoreError> {
    let mut events = Vec::new();
    for json in rows {
        let record: FactoryRecord = serde_json::from_str(&json?)?;
        match record.body {
            FactoryBody::RunEvent(event) => events.push(event),
            other => return Err(wrong_kind("run_event", &other)),
        }
    }
    counted::add(events.len());
    Ok(events)
}

/// The run event keys of `repo` (`owner/name`, in any case), as the repo a run id folds it to
/// and a range of ids that holds every one of them; `None` for a repo that is not a legal one.
///
/// A run event's id is `<run>/<owner>/<version>`, and every run of the repo starts with
/// `<owner>__<name>__`, so the keys lie in [prefix, prefix with its last `_` raised to the
/// next character, a backtick). The range is not exact (a repo name may hold `__`, so `a/b`
/// also reaches the runs of `a/b__c`), so the repo of each run read is still checked.
fn repo_range(repo: &str) -> Option<(String, String, String)> {
    // Normalise as a run id does, so the argument folds exactly as stored repos did.
    let probe = RunId::new(repo, 0, 0).ok()?;
    let prefix = probe.to_string();
    let prefix = &prefix[..prefix.len() - "0__0".len()];
    let to = format!("{}`", &prefix[..prefix.len() - 1]);
    Some((probe.repo().to_owned(), prefix.to_owned(), to))
}

/// The run ids among `rows`, each the text of a run event id up to its first `/`, that are of
/// `repo`, once each and in order.
fn runs_among(
    rows: impl Iterator<Item = rusqlite::Result<String>>,
    repo: &str,
) -> Result<Vec<RunId>, StoreError> {
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
    pub fn factory_versions(&self) -> Result<BTreeMap<MachineId, u64>, StoreError> {
        let mut query = self
            .conn
            .prepare_cached("SELECT owner, MAX(version) FROM factory_records GROUP BY owner")?;
        let rows = query.query_map([], |row| {
            let owner: String = row.get(0)?;
            Ok((parse_machine(0, &owner)?, from_sql(row.get(1)?)))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
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
        run_events_of(rows)
    }

    /// The events of `run` named `event`, from every writer, in the order of
    /// [`run_events`](Self::run_events). Only those rows are handed back and parsed.
    pub fn run_events_named(&self, run: &RunId, event: &str) -> Result<Vec<RunEvent>, StoreError> {
        let (from, to) = range(&run.to_string());
        let mut query = self.conn.prepare_cached(
            "SELECT record FROM factory_records WHERE kind = 'run_event' AND id >= ?1 AND id < ?2 \
             AND json_extract(record, '$.body.event') IS ?3 \
             ORDER BY json_extract(record, '$.body.generation'), version, owner",
        )?;
        let rows = query.query_map(params![from, to, event], |row| row.get::<_, String>(0))?;
        run_events_of(rows)
    }

    /// The first event of `run` in the order of [`run_events`](Self::run_events), or `None`
    /// for a run with none. Only that row is handed back and parsed.
    pub fn first_run_event(&self, run: &RunId) -> Result<Option<RunEvent>, StoreError> {
        let (from, to) = range(&run.to_string());
        let mut query = self.conn.prepare_cached(
            "SELECT record FROM factory_records WHERE kind = 'run_event' AND id >= ?1 AND id < ?2 \
             ORDER BY json_extract(record, '$.body.generation'), version, owner LIMIT 1",
        )?;
        let rows = query.query_map(params![from, to], |row| row.get::<_, String>(0))?;
        Ok(run_events_of(rows)?.pop())
    }

    /// Whether any writer has stored a `finished` event of `run`, whatever its data. No event
    /// is handed back: the store answers yes or no.
    pub fn run_finished(&self, run: &RunId) -> Result<bool, StoreError> {
        let (from, to) = range(&run.to_string());
        let mut query = self.conn.prepare_cached(
            "SELECT EXISTS (SELECT 1 FROM factory_records WHERE kind = 'run_event' \
             AND id >= ?1 AND id < ?2 AND json_extract(record, '$.body.event') IS 'finished')",
        )?;
        Ok(query.query_row(params![from, to], |row| row.get(0))?)
    }

    /// Every run of `repo` (`owner/name`, in any case) that has an event, once each, in order.
    /// A repo that is not a legal one has no runs.
    pub fn runs_of(&self, repo: &str) -> Result<Vec<RunId>, StoreError> {
        let Some((repo, from, to)) = repo_range(repo) else {
            return Ok(Vec::new());
        };
        // A run id holds no `/`, so the run is the id up to its first `/`: no record is parsed.
        let mut query = self.conn.prepare_cached(
            "SELECT DISTINCT substr(id, 1, instr(id, '/') - 1) FROM factory_records
             WHERE kind = 'run_event' AND id >= ?1 AND id < ?2",
        )?;
        let rows = query.query_map(params![from, to], |row| row.get::<_, String>(0))?;
        runs_among(rows, &repo)
    }

    /// The runs of `repo` that [`runs_of`](Self::runs_of) gives, less those with a `finished`
    /// event from any writer, in order. No event is handed back, so a finished run's history is
    /// never read; a run none of whose rows this build can read as `finished` is kept.
    pub fn unfinished_runs_of(&self, repo: &str) -> Result<Vec<RunId>, StoreError> {
        let Some((repo, from, to)) = repo_range(repo) else {
            return Ok(Vec::new());
        };
        let mut query = self.conn.prepare_cached(
            "SELECT substr(id, 1, instr(id, '/') - 1) AS run FROM factory_records
             WHERE kind = 'run_event' AND id >= ?1 AND id < ?2
             GROUP BY run HAVING SUM(json_extract(record, '$.body.event') IS 'finished') = 0",
        )?;
        let rows = query.query_map(params![from, to], |row| row.get::<_, String>(0))?;
        runs_among(rows, &repo)
    }

    /// How many run events the reads of this thread have handed back so far.
    #[cfg(test)]
    pub fn run_events_read() -> usize {
        counted::read()
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
    use serde_json::{Value, json};
    use tempfile::TempDir;

    use super::{EVENTS_OF_WRITER, EVENTS_OF_WRITER_ABOVE, HAS_EVENT, WRITER_PROBE};
    use crate::store::Store;

    fn open() -> (TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("foundry.db")).unwrap();
        (dir, store)
    }

    fn run(repo: &str, issue: u64, attempt: u32) -> RunId {
        RunId::new(repo, issue, attempt).unwrap()
    }

    /// Stores the event `event` of `run` as `owner` left it at `version` and `generation`.
    fn put(
        store: &mut Store,
        owner: MachineId,
        version: u64,
        run: &RunId,
        generation: u64,
        event: &str,
        data: Value,
    ) {
        let record = FactoryRecord {
            owner,
            version,
            body: FactoryBody::RunEvent(RunEvent {
                run: run.clone(),
                generation,
                ts: "2026-10-04T10:00:00.000Z".into(),
                ms: version,
                event: event.into(),
                data,
                cause: None,
            }),
        };
        store.apply_factory(&record, owner).unwrap();
    }

    /// A run event row of `run` that holds `record` as it is, as a newer build or a damaged
    /// file may leave one.
    fn put_raw(dir: &TempDir, run: &RunId, record: &str) {
        let owner = MachineId::random();
        let conn = rusqlite::Connection::open(dir.path().join("foundry.db")).unwrap();
        conn.execute(
            "INSERT INTO factory_records (kind, id, owner, version, record)
             VALUES ('run_event', ?1, ?2, 1, ?3)",
            rusqlite::params![format!("{run}/{owner}/1"), owner.to_string(), record],
        )
        .unwrap();
    }

    fn versions(events: &[(u64, RunEvent)]) -> Vec<u64> {
        events.iter().map(|(version, _)| *version).collect()
    }

    #[test]
    fn a_run_has_an_event_once_any_writer_stored_one_of_that_name() {
        let (_dir, mut store) = open();
        let (a, b) = (MachineId::random(), MachineId::random());
        let eight = run("rawphp/softfac", 8, 1);
        assert!(!store.run_has_event(&eight, "finished").unwrap());
        put(&mut store, a, 1, &eight, 1, "step_changed", json!({}));
        assert!(!store.run_has_event(&eight, "finished").unwrap());
        assert!(store.run_has_event(&eight, "step_changed").unwrap());
        // Another run whose id starts with this one's is not this run.
        let eighty = run("rawphp/softfac", 80, 1);
        put(&mut store, a, 2, &eighty, 1, "finished", json!({}));
        assert!(!store.run_has_event(&eight, "finished").unwrap());
        // A `finished` whose data this build cannot read is still a `finished`.
        put(
            &mut store,
            b,
            1,
            &eight,
            2,
            "finished",
            json!({"total_ms": "soon"}),
        );
        assert!(store.run_has_event(&eight, "finished").unwrap());
    }

    #[test]
    fn a_row_this_build_cannot_read_names_no_event_and_one_that_is_not_json_is_an_error() {
        let (dir, store) = open();
        let eight = run("rawphp/softfac", 8, 1);
        put_raw(&dir, &eight, r#"{"kind":"run_event","later":true}"#);
        assert!(!store.run_has_event(&eight, "finished").unwrap());
        let nine = run("rawphp/softfac", 9, 1);
        put_raw(&dir, &nine, "not json");
        assert!(store.run_has_event(&nine, "finished").is_err());
    }

    #[test]
    fn the_writers_of_a_run_are_read_from_its_keys_once_each_in_order() {
        let (dir, mut store) = open();
        let (a, b, c) = (
            MachineId::random(),
            MachineId::random(),
            MachineId::random(),
        );
        let eight = run("rawphp/softfac", 8, 1);
        assert_eq!(store.run_writers(&eight).unwrap(), []);
        for version in 1..=3 {
            put(&mut store, a, version, &eight, 1, "step_changed", json!({}));
            put(&mut store, b, version, &eight, 2, "step_changed", json!({}));
        }
        put(
            &mut store,
            c,
            1,
            &run("rawphp/softfac", 80, 1),
            1,
            "step_changed",
            json!({}),
        );
        // A row whose record cannot be read still names its writer by its key.
        put_raw(&dir, &eight, "not json");
        let writers = store.run_writers(&eight).unwrap();
        assert_eq!(writers.len(), 3);
        assert!(writers.contains(&a) && writers.contains(&b));
        assert!(!writers.contains(&c));
        let texts: Vec<String> = writers.iter().map(ToString::to_string).collect();
        let mut sorted = texts.clone();
        sorted.sort();
        assert_eq!(texts, sorted);
    }

    #[test]
    fn a_writers_events_of_a_run_are_read_by_version_from_the_start_or_above_one() {
        let (_dir, mut store) = open();
        let (a, b) = (MachineId::random(), MachineId::random());
        let eight = run("rawphp/softfac", 8, 1);
        let eighty = run("rawphp/softfac", 80, 1);
        for version in [3, 1, 12, 2] {
            put(&mut store, a, version, &eight, 1, "step_changed", json!({}));
        }
        put(&mut store, a, 4, &eighty, 1, "step_changed", json!({}));
        put(&mut store, b, 5, &eight, 2, "step_changed", json!({}));
        let all = store.run_events_by(&eight, a, None).unwrap();
        assert_eq!(versions(&all), [1, 2, 3, 12]);
        assert!(all.iter().all(|(_, event)| event.run == eight));
        assert_eq!(
            versions(&store.run_events_by(&eight, a, Some(2)).unwrap()),
            [3, 12]
        );
        assert_eq!(
            versions(&store.run_events_by(&eight, a, Some(12)).unwrap()),
            Vec::<u64>::new()
        );
        assert_eq!(
            versions(&store.run_events_by(&eight, b, None).unwrap()),
            [5]
        );
        // A version 0 is read from the start, and not above 0.
        put(&mut store, b, 0, &eight, 2, "step_changed", json!({}));
        assert_eq!(
            versions(&store.run_events_by(&eight, b, None).unwrap()),
            [0, 5]
        );
        assert_eq!(
            versions(&store.run_events_by(&eight, b, Some(0)).unwrap()),
            [5]
        );
    }

    #[test]
    fn a_writers_row_this_build_cannot_read_is_an_error_never_skipped() {
        let (dir, store) = open();
        let eight = run("rawphp/softfac", 8, 1);
        put_raw(&dir, &eight, r#"{"kind":"run_event","later":true}"#);
        let writer = store.run_writers(&eight).unwrap()[0];
        assert!(store.run_events_by(&eight, writer, None).is_err());
    }

    /// What SQLite says it does for `sql`, one line per step.
    fn plan(store: &Store, sql: &str) -> Vec<String> {
        let mut query = store
            .conn
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap();
        let rows = query.query_map([], |row| row.get::<_, String>(3)).unwrap();
        rows.map(Result::unwrap).collect()
    }

    #[test]
    fn the_reads_of_a_writes_catch_up_seek_an_index_and_never_scan_the_table() {
        let (_dir, store) = open();
        for sql in [
            WRITER_PROBE,
            EVENTS_OF_WRITER,
            EVENTS_OF_WRITER_ABOVE,
            HAS_EVENT,
        ] {
            let steps = plan(&store, sql);
            assert!(
                steps
                    .iter()
                    .any(|step| step.starts_with("SEARCH factory_records USING")),
                "{sql}: {steps:?}"
            );
            assert!(
                !steps.iter().any(|step| step.starts_with("SCAN")),
                "{sql}: {steps:?}"
            );
        }
        // Above a version, only the writer's records stored since are visited, whatever the
        // run's history holds.
        let steps = plan(&store, EVENTS_OF_WRITER_ABOVE);
        assert!(
            steps
                .iter()
                .any(|step| step.contains("factory_records_by_owner (owner=? AND version>?)")),
            "{steps:?}"
        );
    }

    #[test]
    fn the_reads_of_a_runs_rows_count_the_rows_they_hand_back() {
        let (_dir, mut store) = open();
        let (a, b) = (MachineId::random(), MachineId::random());
        let eight = run("rawphp/softfac", 8, 1);
        for version in 1..=3 {
            put(&mut store, a, version, &eight, 1, "gate_command", json!({}));
        }
        put(&mut store, b, 1, &eight, 2, "finished", json!({}));
        let read = Store::run_rows_read;
        let before = read();
        store.run_events(&eight).unwrap();
        assert_eq!(read() - before, 4);
        let before = read();
        store.run_writers(&eight).unwrap();
        assert_eq!(read() - before, 2);
        let before = read();
        store.run_events_by(&eight, a, Some(1)).unwrap();
        assert_eq!(read() - before, 2);
        let before = read();
        store.runs_of("rawphp/softfac").unwrap();
        assert_eq!(read() - before, 1);
        // A yes or no hands back no row of the run.
        let before = read();
        store.run_has_event(&eight, "finished").unwrap();
        assert_eq!(read() - before, 0);
    }
}
