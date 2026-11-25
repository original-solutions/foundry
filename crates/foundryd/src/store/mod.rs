//! SQLite schema, migrations and record upsert.
//!
//! One database per machine holds every machine's records and the shared settings. A record
//! is taken only from its owner and only when its version is higher than the copy held, so
//! a record arriving late or twice changes nothing. Records are ordered by version, never by
//! a time: two machines' clocks may disagree.

use std::collections::BTreeMap;
use std::path::Path;

use foundry_proto::ids::{MachineId, SessionId, TerminalId, WorkspaceId};
use foundry_proto::record::{AnyRecord, Record, Session, Terminal, Workspace};
use foundry_proto::settings::SharedSettings;
use foundry_proto::state::SessionState;
use rusqlite::types::Type;
use rusqlite::{Connection, OptionalExtension, Transaction, params};

pub mod factory;
#[cfg(test)]
mod tests;

/// The schema, one step per entry. A file's `user_version` counts the steps it has had, so
/// a step once released is never edited: a change is a new step at the end.
///
/// A rollback, or going back by hand with `install`, runs the previous build on the file a
/// newer build migrated. That build skips the steps it does not know and keeps working, so
/// a step may only add tables, non-unique indexes or nullable columns. It must never drop,
/// rename or change the meaning of an existing table or column, and nothing it adds may
/// refuse a write the previous build makes: the previous build still reads and writes them.
/// Rows a newer build stored that the previous build cannot parse are another matter: see
/// [`Store::since`].
///
/// The newer build comes back to the same file, and its step does not run again. Meanwhile
/// the previous build inserted rows with nothing in the added columns, updated rows without
/// touching them, and stored nothing in the added tables. So the newer build treats an added
/// column as empty or out of date and rebuilds it from the columns the previous build
/// writes, and never counts on a step having filled in existing rows. The test
/// `every_later_migration_step_only_adds` checks the add-only part for each step after the
/// first.
const MIGRATIONS: &[&str] = &[
    // 1: records keyed by kind and id, the owner's own version counter, shared settings.
    "CREATE TABLE records (
         kind TEXT NOT NULL,
         id TEXT NOT NULL,
         owner TEXT NOT NULL,
         version INTEGER NOT NULL,
         record TEXT NOT NULL,
         PRIMARY KEY (kind, id)
     );
     CREATE INDEX records_by_owner ON records (owner, version);
     CREATE TABLE own_version (
         only INTEGER PRIMARY KEY CHECK (only = 1),
         version INTEGER NOT NULL
     );
     INSERT INTO own_version (only, version) VALUES (1, 0);
     CREATE TABLE settings (
         only INTEGER PRIMARY KEY CHECK (only = 1),
         seq INTEGER NOT NULL,
         settings TEXT NOT NULL
     );",
    // 2: factory records keyed by kind and id, and the owner's own factory version counter.
    // The counter has no row until the first version is taken.
    "CREATE TABLE factory_records (
         kind TEXT NOT NULL,
         id TEXT NOT NULL,
         owner TEXT NOT NULL,
         version INTEGER NOT NULL,
         record TEXT NOT NULL,
         PRIMARY KEY (kind, id)
     );
     CREATE INDEX factory_records_by_owner ON factory_records (owner, version);
     CREATE TABLE own_factory_version (
         only INTEGER PRIMARY KEY CHECK (only = 1),
         version INTEGER NOT NULL
     );",
    // 3: session rows by their terminal and by their workspace, so one session is found
    // without reading every owner's. SQLite keeps both up to date on every write, the previous
    // build's included.
    "CREATE INDEX records_session_by_terminal
         ON records (json_extract(record, '$.body.terminal'), version) WHERE kind = 'session';
     CREATE INDEX records_session_by_workspace
         ON records (json_extract(record, '$.body.workspace'), version) WHERE kind = 'session';",
];

/// The session row with an id, by the records table's key.
const SESSION_BY_ID: &str =
    "SELECT version, record FROM records WHERE kind = 'session' AND id = ?1";

/// The session rows of a terminal, by `records_session_by_terminal`, highest version first.
const SESSION_ON_TERMINAL: &str = "SELECT version, record FROM records WHERE kind = 'session' \
     AND json_extract(record, '$.body.terminal') = ?1 ORDER BY version DESC";

/// The session rows of a workspace, by `records_session_by_workspace`, highest version first.
const SESSION_IN_WORKSPACE: &str = "SELECT version, record FROM records WHERE kind = 'session' \
     AND json_extract(record, '$.body.workspace') = ?1 ORDER BY version DESC";

/// What [`Store::apply`] did with a record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Applied {
    /// The record was new, or newer than the copy held, and is now stored.
    Stored,
    /// The copy held has an equal or higher version. Nothing changed.
    Stale,
    /// The record came from a machine that is not its owner, or claims a record another
    /// machine owns. Nothing changed.
    NotOwner,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("store: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("store: a record or the settings could not be written or read as JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error(
        "store: {what} {value} is larger than the store can hold ({})",
        i64::MAX
    )]
    TooLarge { what: &'static str, value: u64 },
}

/// One machine's SQLite database.
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Opens or creates the database at `path` and brings its schema up to date.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let mut conn = Connection::open(path)?;
        // synchronous stays at SQLite's FULL: with the rollback journal, NORMAL lets a power
        // cut corrupt the file.
        conn.execute_batch("PRAGMA temp_store = MEMORY;")?;
        migrate(&mut conn, MIGRATIONS.len())?;
        Ok(Self { conn })
    }

    /// Stores `record`, received from machine `from`, when `from` owns it and its version is
    /// higher than the copy held. A deleted record is kept, so an older live copy that
    /// arrives later is [`Applied::Stale`] and cannot bring it back.
    pub fn apply(&mut self, record: &AnyRecord, from: MachineId) -> Result<Applied, StoreError> {
        if record.owner() != from {
            return Ok(Applied::NotOwner);
        }
        let tx = self.conn.transaction()?;
        let applied = apply_in_tx(&tx, record, from)?;
        if applied == Applied::Stored {
            tx.commit()?;
        }
        Ok(applied)
    }

    /// Stores a slice of records from `from` in one SQLite transaction, calling `on_not_owner`
    /// for any record `from` does not own. On error, commits the prefix already applied so
    /// nothing before the failing record is lost.
    pub fn apply_batch(
        &mut self,
        records: &[AnyRecord],
        from: MachineId,
        mut on_not_owner: impl FnMut(&AnyRecord),
    ) -> Result<(), StoreError> {
        if records.is_empty() {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        let mut stored_any = false;
        for record in records {
            match apply_in_tx(&tx, record, from) {
                Ok(Applied::Stored) => stored_any = true,
                Ok(Applied::Stale) => {}
                Ok(Applied::NotOwner) => on_not_owner(record),
                Err(error) => {
                    if stored_any {
                        tx.commit()?;
                    }
                    return Err(error);
                }
            }
        }
        if stored_any {
            tx.commit()?;
        }
        Ok(())
    }

    /// The version for this machine's next write of one of its own records. Each call gives
    /// a higher number than the last, across restarts too.
    pub fn own_next_version(&mut self) -> Result<u64, StoreError> {
        let mut stmt = self
            .conn
            .prepare_cached("UPDATE own_version SET version = version + 1 RETURNING version")?;
        let version: i64 = stmt.query_row([], |row| row.get(0))?;
        Ok(from_sql(version))
    }

    /// Every record held from `owner` with a version above `version`, lowest version first.
    ///
    /// A stored record this build cannot parse, one a newer build left before a rollback, is
    /// an error: `since` never silently skips a stored row. Reading such rows after a
    /// rollback belongs to sync (task 12) and rollback (task 18).
    pub fn since(&self, owner: MachineId, version: u64) -> Result<Vec<AnyRecord>, StoreError> {
        self.since_limited(owner, version, usize::MAX)
    }

    /// The first `limit` records [`since`](Self::since) would give, read without the rest, so
    /// a long catch-up can be read one batch at a time. Only the rows it gives are parsed.
    pub fn since_limited(
        &self,
        owner: MachineId,
        version: u64,
        limit: usize,
    ) -> Result<Vec<AnyRecord>, StoreError> {
        // Nothing stored is above i64::MAX, so a larger `version` correctly finds nothing.
        let version = i64::try_from(version).unwrap_or(i64::MAX);
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut query = self.conn.prepare_cached(
            "SELECT record FROM records WHERE owner = ?1 AND version > ?2 ORDER BY version \
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

    /// The workspace record with `id`, deleted or not, read by its key without the rest.
    pub fn workspace(&self, id: WorkspaceId) -> Result<Option<Record<Workspace>>, StoreError> {
        let mut query = self
            .conn
            .prepare_cached("SELECT record FROM records WHERE kind = 'workspace' AND id = ?1")?;
        let json: Option<String> = query
            .query_row(params![id.to_string()], |row| row.get(0))
            .optional()?;
        let Some(json) = json else {
            return Ok(None);
        };
        match serde_json::from_str(&json)? {
            AnyRecord::Workspace(record) => Ok(Some(record)),
            // Rows are stored under their own kind, so a workspace row holds a workspace.
            _ => Ok(None),
        }
    }

    /// The terminal record with `id`, deleted or not, read by its key without the rest.
    pub fn terminal(&self, id: TerminalId) -> Result<Option<Record<Terminal>>, StoreError> {
        let mut query = self
            .conn
            .prepare_cached("SELECT record FROM records WHERE kind = 'terminal' AND id = ?1")?;
        let json: Option<String> = query
            .query_row(params![id.to_string()], |row| row.get(0))
            .optional()?;
        let Some(json) = json else {
            return Ok(None);
        };
        match serde_json::from_str(&json)? {
            AnyRecord::Terminal(record) => Ok(Some(record)),
            // Rows are stored under their own kind, so a terminal row holds a terminal.
            _ => Ok(None),
        }
    }

    /// The workspace records not deleted, from every owner, lowest version first. Only
    /// workspace rows are parsed, so a row of another kind this build cannot read does not stop
    /// it.
    pub fn live_workspaces(&self) -> Result<Vec<Record<Workspace>>, StoreError> {
        let mut query = self.conn.prepare_cached(
            "SELECT record FROM records WHERE kind = 'workspace' ORDER BY version",
        )?;
        let rows = query.query_map([], |row| row.get::<_, String>(0))?;
        let mut records = Vec::new();
        for json in rows {
            if let AnyRecord::Workspace(record) = serde_json::from_str(&json?)?
                && !record.deleted
            {
                records.push(record);
            }
        }
        Ok(records)
    }

    /// The terminal records held from `owner` in workspace `workspace`, deleted ones included,
    /// lowest version first. Only those rows are parsed.
    pub fn terminals_in(
        &self,
        owner: MachineId,
        workspace: WorkspaceId,
    ) -> Result<Vec<Record<Terminal>>, StoreError> {
        let mut query = self.conn.prepare_cached(
            "SELECT record FROM records WHERE owner = ?1 AND kind = 'terminal' \
             AND json_extract(record, '$.body.workspace') = ?2 ORDER BY version",
        )?;
        let rows = query.query_map(params![owner.to_string(), workspace.to_string()], |row| {
            row.get::<_, String>(0)
        })?;
        let mut records = Vec::new();
        for json in rows {
            if let AnyRecord::Terminal(record) = serde_json::from_str(&json?)? {
                records.push(record);
            }
        }
        Ok(records)
    }

    /// Live sessions owned by `owner`: not deleted and neither finished nor failed. Only
    /// session rows of `owner` are read.
    pub fn live_session_count(&self, owner: MachineId) -> Result<u32, StoreError> {
        let mut query = self
            .conn
            .prepare_cached("SELECT record FROM records WHERE owner = ?1 AND kind = 'session'")?;
        let rows = query.query_map(params![owner.to_string()], |row| row.get::<_, String>(0))?;
        let mut live: usize = 0;
        for json in rows {
            if let AnyRecord::Session(r) = serde_json::from_str(&json?)?
                && !r.deleted
                && !matches!(r.body.state, SessionState::Finished | SessionState::Failed)
            {
                live += 1;
            }
        }
        Ok(u32::try_from(live).unwrap_or(u32::MAX))
    }

    /// The session records `owner` wrote that are not deleted, lowest version first. Only
    /// session rows of `owner` are read, and one this build cannot parse, such as a newer
    /// build's, is left out with a warning, so it does not hide the rest.
    pub fn sessions_of(&self, owner: MachineId) -> Result<Vec<Record<Session>>, StoreError> {
        let mut query = self.conn.prepare_cached(
            "SELECT version, record FROM records WHERE owner = ?1 AND kind = 'session' \
             ORDER BY version",
        )?;
        let rows = query.query_map(params![owner.to_string()], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut records = Vec::new();
        for row in rows {
            let (version, json) = row?;
            match serde_json::from_str(&json) {
                Ok(AnyRecord::Session(record)) if !record.deleted => records.push(record),
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(%owner, version, %error, "a session record this build cannot read is left out");
                }
            }
        }
        Ok(records)
    }

    /// The session record with `id`, deleted or not, with its owner, read by its key without the
    /// rest. A row this build cannot read is none, as [`sessions_of`](Self::sessions_of) leaves
    /// it out.
    pub fn session(&self, id: SessionId) -> Result<Option<Record<Session>>, StoreError> {
        self.first_session(SESSION_BY_ID, id.to_string())
    }

    /// The session record that runs in terminal `id`, deleted or not, with its owner, read
    /// through an index without the other sessions. Should two name the terminal, the one with
    /// the highest version. A row this build cannot read is left out.
    pub fn session_on(&self, id: TerminalId) -> Result<Option<Record<Session>>, StoreError> {
        self.first_session(SESSION_ON_TERMINAL, id.to_string())
    }

    /// The session record of workspace `id` with the highest version, deleted or not, with its
    /// owner, read through an index without the other sessions. A workspace may have had several
    /// sessions, as a start after an end makes, and only the newest is given. A row this build
    /// cannot read is left out, and the next newest answers.
    pub fn session_in(&self, id: WorkspaceId) -> Result<Option<Record<Session>>, StoreError> {
        self.first_session(SESSION_IN_WORKSPACE, id.to_string())
    }

    /// The first session record `sql` gives for `key` that this build can read. The rows after
    /// it are never read.
    fn first_session(&self, sql: &str, key: String) -> Result<Option<Record<Session>>, StoreError> {
        let mut query = self.conn.prepare_cached(sql)?;
        let mut rows = query.query(params![key])?;
        while let Some(row) = rows.next()? {
            let (version, json): (i64, String) = (row.get(0)?, row.get(1)?);
            match serde_json::from_str(&json) {
                Ok(AnyRecord::Session(record)) => return Ok(Some(record)),
                // Rows are stored under their own kind, so a session row holds a session.
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(version, %error, "a session record this build cannot read is left out");
                }
            }
        }
        Ok(None)
    }

    /// The highest version held from each owner.
    pub fn versions(&self) -> Result<BTreeMap<MachineId, u64>, StoreError> {
        let mut query = self
            .conn
            .prepare_cached("SELECT owner, MAX(version) FROM records GROUP BY owner")?;
        let rows = query.query_map([], |row| {
            let owner: String = row.get(0)?;
            let owner = owner.parse().map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(0, Type::Text, Box::new(e))
            })?;
            Ok((MachineId(owner), from_sql(row.get(1)?)))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The `seq` of the held shared settings, or `0` before any arrive, without parsing JSON.
    pub fn settings_seq(&self) -> Result<u64, StoreError> {
        let mut query = self.conn.prepare_cached("SELECT seq FROM settings")?;
        let seq: Option<i64> = query.query_row([], |row| row.get(0)).optional()?;
        Ok(seq.map_or(0, from_sql))
    }

    /// The shared settings held, or empty settings with `seq` 0 before any arrive.
    pub fn settings(&self) -> Result<SharedSettings, StoreError> {
        let mut query = self.conn.prepare_cached("SELECT settings FROM settings")?;
        let json: Option<String> = query.query_row([], |row| row.get(0)).optional()?;
        match json {
            Some(json) => Ok(serde_json::from_str(&json)?),
            None => Ok(SharedSettings::default()),
        }
    }

    /// The shared settings held when their `seq` is not `seen`, else `None`, so a caller that
    /// keeps the last `seq` it read parses the settings only after they change.
    pub fn settings_since(&self, seen: Option<u64>) -> Result<Option<SharedSettings>, StoreError> {
        if seen == Some(self.settings_seq()?) {
            return Ok(None);
        }
        self.settings().map(Some)
    }

    /// Stores `settings` when their `seq` is higher than the held one's, and says whether it
    /// did. Settings with an equal or lower `seq` change nothing.
    pub fn apply_settings(&mut self, settings: SharedSettings) -> Result<bool, StoreError> {
        let seq = to_sql("settings seq", settings.seq)?;

        let tx = self.conn.transaction()?;
        let held: i64 = tx
            .prepare_cached("SELECT seq FROM settings")?
            .query_row([], |row| row.get(0))
            .optional()?
            .unwrap_or(0);
        if seq <= held {
            return Ok(false);
        }
        let json = serde_json::to_string(&settings)?;
        tx.prepare_cached(
            "INSERT INTO settings (only, seq, settings) VALUES (1, ?1, ?2)
             ON CONFLICT (only) DO UPDATE SET seq = excluded.seq, settings = excluded.settings",
        )?
        .execute(params![seq, json])?;
        tx.commit()?;
        Ok(true)
    }

    /// How many steps of [`MIGRATIONS`] the file has had.
    #[cfg(test)]
    fn schema_version(&self) -> Result<usize, StoreError> {
        user_version(&self.conn)
    }
}

fn apply_in_tx(
    tx: &Transaction<'_>,
    record: &AnyRecord,
    from: MachineId,
) -> Result<Applied, StoreError> {
    let owner = record.owner();
    if owner != from {
        return Ok(Applied::NotOwner);
    }
    let version = to_sql("version", record.version())?;
    let (kind, id) = key(record);
    let owner_str = owner.to_string();

    let held: Option<(String, i64)> = tx
        .prepare_cached("SELECT owner, version FROM records WHERE kind = ?1 AND id = ?2")?
        .query_row(params![kind, id], |row| Ok((row.get(0)?, row.get(1)?)))
        .optional()?;
    match held {
        Some((held_owner, _)) if held_owner != owner_str => {
            return Ok(Applied::NotOwner);
        }
        Some((_, held_version)) if version <= held_version => return Ok(Applied::Stale),
        _ => {}
    }
    let json = serde_json::to_string(record)?;
    tx.prepare_cached(
        "INSERT INTO records (kind, id, owner, version, record) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (kind, id) DO UPDATE SET version = excluded.version, record = excluded.record",
    )?
    .execute(params![kind, id, owner_str, version, json])?;
    Ok(Applied::Stored)
}

/// Runs the steps of [`MIGRATIONS`] the file has not had, up to step `to`, each in its own
/// transaction with the step count it brings the file to.
fn migrate(conn: &mut Connection, to: usize) -> Result<(), StoreError> {
    let done = user_version(conn)?;
    for (step, sql) in MIGRATIONS.iter().enumerate().take(to).skip(done) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", step as i64 + 1)?;
        tx.commit()?;
    }
    Ok(())
}

fn user_version(conn: &Connection) -> Result<usize, StoreError> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    Ok(usize::try_from(version).unwrap_or(0))
}

/// The kind and id a record is stored under. A machine sample is one per owner.
fn key(record: &AnyRecord) -> (&'static str, String) {
    match record {
        AnyRecord::Workspace(r) => ("workspace", r.body.id.to_string()),
        AnyRecord::Terminal(r) => ("terminal", r.body.id.to_string()),
        AnyRecord::Session(r) => ("session", r.body.id.to_string()),
        AnyRecord::MachineSample(r) => ("machine_sample", r.owner.to_string()),
    }
}

/// SQLite integers are signed, so a value above `i64::MAX` is refused rather than wrapped.
fn to_sql(what: &'static str, value: u64) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|_| StoreError::TooLarge { what, value })
}

/// Every stored version went in through [`to_sql`] or counts up from 0, so it is not negative.
fn from_sql(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}
