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

/// The machine id `text` spells, only in the spelling this build writes one. `apply_factory`
/// keys every record under its owner's id in that spelling, and the reads look an owner's rows
/// up by it, so an id spelt otherwise (a newer build's or a damaged file's) names rows those
/// reads would never find, and is an error rather than taken as the id it parses to.
fn parse_machine(index: usize, text: &str) -> rusqlite::Result<MachineId> {
    let unreadable = |e: Box<dyn std::error::Error + Send + Sync>| {
        rusqlite::Error::FromSqlConversionFailure(index, Type::Text, e)
    };
    let id = text
        .parse()
        .map(MachineId)
        .map_err(|e| unreadable(Box::new(e)))?;
    if id.to_string() != text {
        return Err(unreadable(
            "a machine id not spelt as this build writes one".into(),
        ));
    }
    Ok(id)
}

/// Counts the run event rows the reads of this thread hand back, as events or as ids, for tests
/// that show a read is bounded. Counted by thread, since each test runs on a thread of its own.
#[cfg(test)]
mod counted {
    use std::cell::Cell;

    thread_local! {
        static READ: Cell<usize> = const { Cell::new(0) };
    }

    pub(super) fn add(rows: usize) {
        READ.with(|read| read.set(read.get() + rows));
    }

    pub(super) fn read() -> usize {
        READ.with(Cell::get)
    }
}

#[cfg(not(test))]
mod counted {
    pub(super) fn add(_rows: usize) {}
}

/// The lowest run event key in [`?1`, `?2`), or NULL with none: one seek of the primary key,
/// which finds the next writer of a run without visiting the rows of the writer before it.
const WRITER_PROBE: &str = "SELECT MIN(id) FROM factory_records WHERE kind = 'run_event' \
     AND id >= ?1 AND id < ?2";

/// The run events in [`?1`, `?2`), one writer's keys of one run, lowest version first.
const EVENTS_OF_WRITER: &str = "SELECT version, record FROM factory_records \
     WHERE kind = 'run_event' AND id >= ?1 AND id < ?2 ORDER BY version";

/// The run events in [`?1`, `?2`) that `?3` stored above version `?4`, lowest version first.
/// Read by the writer's index, so only the records `?3` stored above `?4`, of any run or kind,
/// are visited, never the rows of the run below it.
const EVENTS_OF_WRITER_ABOVE: &str = "SELECT version, record FROM factory_records \
     INDEXED BY factory_records_by_owner \
     WHERE owner = ?3 AND version > ?4 AND kind = 'run_event' AND id >= ?1 AND id < ?2 \
     ORDER BY version";

/// Whether a run event in [`?1`, `?2`) is named `?3`. No index holds the event name, so SQLite
/// reads the rows of the range in key order until one is named `?3`, all of them when none is;
/// only the yes or no comes back. A row that is not JSON names no event, so the answer does not
/// depend on whether such a row comes before the one named `?3`.
const HAS_EVENT: &str = "SELECT EXISTS (SELECT 1 FROM factory_records WHERE kind = 'run_event' \
     AND id >= ?1 AND id < ?2 \
     AND CASE WHEN json_valid(record) THEN json_extract(record, '$.body.event') END IS ?3)";

/// The highest version held from owner `?1`, of any kind of record, or NULL with none: one seek
/// of the owner's index.
const HIGHEST_VERSION: &str = "SELECT MAX(version) FROM factory_records WHERE owner = ?1";

/// The run event stored as `json`. A row that holds another kind of record is an error.
fn run_event(json: &str) -> Result<RunEvent, StoreError> {
    let record: FactoryRecord = serde_json::from_str(json)?;
    counted::add(1);
    match record.body {
        FactoryBody::RunEvent(event) => Ok(event),
        other => Err(wrong_kind("run_event", &other)),
    }
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
    /// An owner is read only as this build writes it: text that is a machine id in the
    /// spelling `apply_factory` stores and `factory_since` looks it up by. Any other owner cell
    /// (a blob, text that is not UTF-8 or not a machine id, or a machine id spelt otherwise),
    /// which only a corrupt file or a newer build's id format leaves, is left out: one such row
    /// must not make the machine's hello say it holds no factory record, which would have every
    /// peer send it all of theirs again on every link, and it must not stand in for the version
    /// of an owner it may spell. Each such owner is warned of once while the store is open
    /// (`warn_unreadable_owner`).
    pub fn factory_versions(&self) -> Result<BTreeMap<MachineId, u64>, StoreError> {
        let mut query = self.conn.prepare_cached(
            "SELECT owner, MAX(version) FROM factory_records GROUP BY owner ORDER BY owner",
        )?;
        // Read to the end before warning, so the warning's own writes never run inside the read.
        let rows = query
            .query_map([], |row| {
                let owner = row.get_ref(0)?;
                let id = owner.as_str().ok().and_then(|text| {
                    text.parse()
                        .ok()
                        .map(MachineId)
                        .filter(|id| id.to_string() == text)
                });
                // The column is TEXT and NOT NULL, so text and blobs are all a cell holds.
                let owner = id.ok_or_else(|| owner.as_bytes().unwrap_or_default().to_vec());
                Ok((owner, from_sql(row.get(1)?)))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut versions = BTreeMap::new();
        for (owner, version) in rows {
            match owner {
                Ok(id) => {
                    versions.insert(id, version);
                }
                Err(cell) => warn_unreadable_owner(&self.conn, &cell),
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
            events.push(run_event(&json?)?);
        }
        Ok(events)
    }

    /// The machines that wrote an event of `run`, each once, in the order of their ids. Read
    /// from the keys alone, one seek per writer, so a run's history is never visited and a row
    /// whose record cannot be read still names its writer. A key whose writer is not a machine
    /// id in the spelling this build writes is an error (`parse_machine`).
    pub fn run_writers(&self, run: &RunId) -> Result<Vec<MachineId>, StoreError> {
        let run = run.to_string();
        let (mut from, to) = range(&run);
        let mut query = self.conn.prepare_cached(WRITER_PROBE)?;
        let mut writers = Vec::new();
        while let Some(id) =
            query.query_row(params![from, to], |row| row.get::<_, Option<String>>(0))?
        {
            counted::add(1);
            // `<run>/<owner>/<version>`: the writer is the part after the run's `/`, up to the
            // next one. The next writer's keys start above `<run>/<owner>0`, since `0` is the
            // character after `/`.
            let rest = &id[run.len() + 1..];
            let owner = rest.split('/').next().unwrap_or(rest);
            writers.push(parse_machine(0, owner)?);
            from = format!("{run}/{owner}0");
        }
        Ok(writers)
    }

    /// The events of `run` that `writer` stored, each with its version, lowest version first:
    /// all of them when `above` is `None`, else only those above version `above`.
    ///
    /// Above a version, the read goes by the writer's index, so it visits only the records the
    /// writer stored since that version and never the run's history below it. A writer's
    /// records are stored in the order of their versions, on its own machine and on each one
    /// it syncs to, so none of this run's events can arrive later below `above`. A row this
    /// build cannot parse is an error, never skipped.
    pub fn run_events_by(
        &self,
        run: &RunId,
        writer: MachineId,
        above: Option<u64>,
    ) -> Result<Vec<(u64, RunEvent)>, StoreError> {
        let (from, to) = range(&format!("{run}/{writer}"));
        let row = |row: &rusqlite::Row<'_>| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?));
        let rows: Vec<(i64, String)> = match above {
            None => self
                .conn
                .prepare_cached(EVENTS_OF_WRITER)?
                .query_map(params![from, to], row)?
                .collect::<Result<_, _>>()?,
            Some(version) => {
                // Nothing stored is above i64::MAX, so a larger `version` correctly finds nothing.
                let version = i64::try_from(version).unwrap_or(i64::MAX);
                self.conn
                    .prepare_cached(EVENTS_OF_WRITER_ABOVE)?
                    .query_map(params![from, to, writer.to_string(), version], row)?
                    .collect::<Result<_, _>>()?
            }
        };
        rows.into_iter()
            .map(|(version, json)| Ok((from_sql(version), run_event(&json)?)))
            .collect()
    }

    /// Whether any writer has stored an event of `run` named `event`, whatever its data. The
    /// store answers yes or no and hands back no row of the run, though it reads the run's rows
    /// up to the first one so named ([`HAS_EVENT`]). A row this build cannot read as a run
    /// event, JSON or not, names no event.
    pub fn run_has_event(&self, run: &RunId, event: &str) -> Result<bool, StoreError> {
        let (from, to) = range(&run.to_string());
        let mut query = self.conn.prepare_cached(HAS_EVENT)?;
        Ok(query.query_row(params![from, to, event], |row| row.get(0))?)
    }

    /// The highest version of any factory record held from `owner`, or `None` with none. A
    /// writer's records are stored in the order of their versions, so none it wrote can arrive
    /// later below this.
    pub fn highest_factory_version(&self, owner: MachineId) -> Result<Option<u64>, StoreError> {
        let mut query = self.conn.prepare_cached(HIGHEST_VERSION)?;
        let highest: Option<i64> = query.query_row(params![owner.to_string()], |row| row.get(0))?;
        Ok(highest.map(from_sql))
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
            counted::add(1);
            let run: RunId = text.parse().map_err(|e: String| {
                rusqlite::Error::FromSqlConversionFailure(0, Type::Text, e.into())
            })?;
            if run.repo() == repo {
                runs.insert(run);
            }
        }
        Ok(runs.into_iter().collect())
    }

    /// How many run event rows the reads of this thread have handed back so far, as events
    /// or as ids. A yes or no, such as [`run_has_event`](Self::run_has_event), counts none.
    #[cfg(test)]
    pub fn run_rows_read() -> usize {
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

/// Warns that a factory row whose owner this build cannot read is left out of the versions
/// held, once per owner cell (its bytes) while `conn` is open, which is once a start: the owners
/// warned of are kept in a table of the connection's own, which goes when it closes. That table
/// holds at most one row per unreadable owner the store holds, and no peer can add one, since a
/// record is stored under the machine id it came from, in this build's spelling. Should that
/// table fail, the warning goes anyway. The line holds nothing of the row: not the owner and not
/// its version.
fn warn_unreadable_owner(conn: &Connection, owner: &[u8]) {
    let inserted = conn
        .execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS factory_owners_warned (owner BLOB PRIMARY KEY)",
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
mod run_read_tests {
    use foundry_proto::factory::{FactoryBody, FactoryRecord, RunEvent, RunId};
    use foundry_proto::ids::MachineId;
    use serde_json::{Value, json};
    use tempfile::TempDir;

    use super::{
        EVENTS_OF_WRITER, EVENTS_OF_WRITER_ABOVE, HAS_EVENT, HIGHEST_VERSION, WRITER_PROBE,
    };
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
        put_raw_by(dir, run, MachineId::random(), record);
    }

    /// [`put_raw`] as `owner`.
    fn put_raw_by(dir: &TempDir, run: &RunId, owner: MachineId, record: &str) {
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
    fn a_row_this_build_cannot_read_names_no_event_whether_or_not_it_is_json() {
        let (dir, mut store) = open();
        let eight = run("rawphp/softfac", 8, 1);
        put_raw(&dir, &eight, r#"{"kind":"run_event","later":true}"#);
        assert!(!store.run_has_event(&eight, "finished").unwrap());
        let nine = run("rawphp/softfac", 9, 1);
        put_raw(&dir, &nine, "not json");
        assert!(!store.run_has_event(&nine, "finished").unwrap());
        // A `finished` is found whether a row that is not JSON comes before it by key or after.
        let (low, high) = (
            MachineId(uuid::Uuid::from_u128(1)),
            MachineId(uuid::Uuid::from_u128(u128::MAX)),
        );
        for (issue, damaged, writer) in [(10, low, high), (11, high, low)] {
            let run = run("rawphp/softfac", issue, 1);
            put_raw_by(&dir, &run, damaged, "not json");
            put(&mut store, writer, issue, &run, 1, "finished", json!({}));
            assert!(store.run_has_event(&run, "finished").unwrap(), "{run}");
        }
    }

    #[test]
    fn a_writers_highest_version_is_its_highest_record_of_any_kind_or_run() {
        let (_dir, mut store) = open();
        let (a, b) = (MachineId::random(), MachineId::random());
        assert_eq!(store.highest_factory_version(a).unwrap(), None);
        put(
            &mut store,
            a,
            3,
            &run("rawphp/softfac", 8, 1),
            1,
            "step_changed",
            json!({}),
        );
        put(
            &mut store,
            a,
            70,
            &run("rawphp/other", 1, 1),
            1,
            "step_changed",
            json!({}),
        );
        put(
            &mut store,
            b,
            900,
            &run("rawphp/softfac", 8, 1),
            1,
            "step_changed",
            json!({}),
        );
        assert_eq!(store.highest_factory_version(a).unwrap(), Some(70));
        assert_eq!(store.highest_factory_version(b).unwrap(), Some(900));
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

    #[test]
    fn a_writer_whose_key_spells_its_id_otherwise_than_this_build_writes_it_is_an_error() {
        let a = MachineId::random();
        let eight = run("rawphp/softfac", 8, 1);
        let finished = serde_json::to_string(&FactoryRecord {
            owner: a,
            version: 1,
            body: FactoryBody::RunEvent(RunEvent {
                run: eight.clone(),
                generation: 1,
                ts: "2026-10-04T10:00:00.000Z".into(),
                ms: 1,
                event: "finished".into(),
                data: json!({}),
                cause: None,
            }),
        })
        .unwrap();
        // Each spelling parses as a's id, but a's reads look its rows up under the spelling
        // this build writes, so taken as a, this row would never be read.
        for spelt in [
            a.to_string().to_uppercase(),
            format!("{{{a}}}"),
            a.0.simple().to_string(),
        ] {
            let (dir, store) = open();
            let conn = rusqlite::Connection::open(dir.path().join("foundry.db")).unwrap();
            conn.execute(
                "INSERT INTO factory_records (kind, id, owner, version, record)
                 VALUES ('run_event', ?1, ?2, 1, ?3)",
                rusqlite::params![format!("{eight}/{spelt}/1"), a.to_string(), finished],
            )
            .unwrap();
            assert!(store.run_has_event(&eight, "finished").unwrap(), "{spelt}");
            assert!(store.run_writers(&eight).is_err(), "{spelt}");
        }
    }

    /// What SQLite says it does for `sql`, one line per step.
    fn plan(store: &Store, sql: &str) -> Vec<String> {
        let mut query = store
            .conn
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap();
        let unbound = vec![rusqlite::types::Null; query.parameter_count()];
        let rows = query
            .query_map(rusqlite::params_from_iter(unbound), |row| {
                row.get::<_, String>(3)
            })
            .unwrap();
        rows.map(Result::unwrap).collect()
    }

    #[test]
    fn the_reads_of_a_runs_rows_seek_an_index_and_never_scan_the_table() {
        let (_dir, store) = open();
        for sql in [
            WRITER_PROBE,
            EVENTS_OF_WRITER,
            EVENTS_OF_WRITER_ABOVE,
            HAS_EVENT,
            HIGHEST_VERSION,
        ] {
            let steps = plan(&store, sql);
            assert!(
                steps
                    .iter()
                    .any(|step| step.starts_with("SEARCH factory_records USING")),
                "{sql}: {steps:?}"
            );
            assert!(
                !steps
                    .iter()
                    .any(|step| step.starts_with("SCAN factory_records")),
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
    fn a_read_above_a_version_keeps_to_the_writers_index_where_the_planner_would_not() {
        // Statistics under which one writer holds every row and a run's key range looks
        // narrow: left to itself, SQLite would then read the run's rows by key, its whole
        // history below the version included.
        let (_dir, store) = open();
        store
            .conn
            .execute_batch(
                "ANALYZE;
                 DELETE FROM sqlite_stat1;
                 INSERT INTO sqlite_stat1 (tbl, idx, stat) VALUES
                     ('factory_records', 'factory_records_by_owner', '1000000 1000000 1'),
                     ('factory_records', 'sqlite_autoindex_factory_records_1', '1000000 250000 1');
                 ANALYZE sqlite_schema;",
            )
            .unwrap();
        let unhinted = EVENTS_OF_WRITER_ABOVE.replace("INDEXED BY factory_records_by_owner", "");
        let steps = plan(&store, &unhinted);
        assert!(
            !steps
                .iter()
                .any(|step| step.contains("factory_records_by_owner")),
            "the statistics must turn the planner away from the writer's index: {steps:?}"
        );
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

    /// Makes the owner cell of `owner`'s row of `version` hold `cell`, an SQL expression in
    /// which `?1` is the owner as this build writes it, and gives the cell's SQL type.
    fn respell(store: &Store, owner: MachineId, version: u64, cell: &str) -> String {
        let changed = store
            .conn
            .execute(
                &format!(
                    "UPDATE factory_records SET owner = {cell} WHERE owner = ?1 AND version = ?2"
                ),
                params![owner.to_string(), i64::try_from(version).unwrap()],
            )
            .unwrap();
        assert_eq!(changed, 1);
        store
            .conn
            .query_row(
                "SELECT typeof(owner) FROM factory_records WHERE version = ?1",
                params![i64::try_from(version).unwrap()],
                |row| row.get(0),
            )
            .unwrap()
    }

    #[test]
    fn factory_versions_leaves_out_an_owner_stored_as_a_blob_or_as_text_that_is_not_utf_8() {
        let (_dir, mut store) = open();
        let (a, blob, not_utf_8) = (
            MachineId::random(),
            MachineId::random(),
            MachineId::random(),
        );
        store_event(&mut store, a, 1, "a-1");
        store_event(&mut store, blob, 2, "blob-2");
        store_event(&mut store, not_utf_8, 3, "not-utf-8-3");
        assert_eq!(respell(&store, blob, 2, "x'00ff'"), "blob");
        assert_eq!(respell(&store, not_utf_8, 3, "CAST(x'ff' AS TEXT)"), "text");

        let logged = Logged::default();
        let _capture = logged.capture();
        assert_eq!(
            store.factory_versions().unwrap(),
            [(a, 1)].into(),
            "the readable owner's version, and no error"
        );
        assert_eq!(logged.warnings().len(), 2, "{:?}", logged.warnings());
    }

    #[test]
    fn a_row_whose_owner_is_spelt_otherwise_than_this_build_writes_it_never_lowers_its_version() {
        let (_dir, mut store) = open();
        let a = MachineId::random();
        for version in 1..=3 {
            store_event(&mut store, a, version, &format!("a-{version}"));
        }
        // Two of a's rows, below its highest, with its id spelt as this build never writes it.
        // Both sort after a's own spelling, so a read that took either as a's would lower it.
        assert_eq!(respell(&store, a, 1, "CAST(?1 AS BLOB)"), "blob");
        assert_eq!(respell(&store, a, 2, "'{' || ?1 || '}'"), "text");

        let logged = Logged::default();
        let _capture = logged.capture();
        assert_eq!(store.factory_versions().unwrap(), [(a, 3)].into());
        assert_eq!(logged.warnings().len(), 2, "{:?}", logged.warnings());
    }

    #[test]
    fn each_owner_that_cannot_be_read_is_warned_of_once_and_one_found_later_then() {
        let (_dir, mut store) = open();
        let (a, first, last, later) = (
            MachineId::random(),
            MachineId::random(),
            MachineId::random(),
            MachineId::random(),
        );
        store_event(&mut store, a, 1, "a-1");
        store_event(&mut store, first, 2, "first-2");
        store_event(&mut store, last, 3, "last-3");
        spoil_owner(&store, first, UNREADABLE_FIRST);
        spoil_owner(&store, last, UNREADABLE);

        let logged = Logged::default();
        let _capture = logged.capture();
        for _ in 0..3 {
            assert_eq!(store.factory_versions().unwrap(), [(a, 1)].into());
        }
        assert_eq!(
            logged.warnings().len(),
            2,
            "once for each owner: {:?}",
            logged.warnings()
        );

        // An owner first found unreadable while the store is open is warned of then, once.
        store_event(&mut store, later, 4, "later-4");
        spoil_owner(&store, later, "owner-found-later");
        for _ in 0..3 {
            assert_eq!(store.factory_versions().unwrap(), [(a, 1)].into());
        }
        assert_eq!(logged.warnings().len(), 3, "{:?}", logged.warnings());
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
        // The query reads owners in their order: one bad owner comes before both readable ones,
        // and one after, so a read that stops at a bad owner rather than going past it fails
        // here.
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
