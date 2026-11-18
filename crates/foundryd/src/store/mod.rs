//! SQLite schema, migrations and record upsert.
//!
//! One database per machine holds every machine's records and the shared settings. A record
//! is taken only from its owner and only when its version is higher than the copy held, so
//! a record arriving late or twice changes nothing. Records are ordered by version, never by
//! a time: two machines' clocks may disagree.

use std::collections::BTreeMap;
use std::path::Path;

use foundry_proto::ids::{MachineId, WorkspaceId};
use foundry_proto::record::{AnyRecord, Record, Terminal, Workspace};
use foundry_proto::settings::SharedSettings;
use rusqlite::types::Type;
use rusqlite::{Connection, OptionalExtension, params};

#[cfg(test)]
mod tests;

/// The schema, one step per entry. A file's `user_version` counts the steps it has had, so
/// a step once released is never edited: a change is a new step at the end. What a build
/// does with a file a newer build migrated belongs to rollback (task 18).
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
];

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
        migrate(&mut conn, MIGRATIONS.len())?;
        Ok(Self { conn })
    }

    /// Stores `record`, received from machine `from`, when `from` owns it and its version is
    /// higher than the copy held. A deleted record is kept, so an older live copy that
    /// arrives later is [`Applied::Stale`] and cannot bring it back.
    pub fn apply(&mut self, record: &AnyRecord, from: MachineId) -> Result<Applied, StoreError> {
        let owner = record.owner();
        if owner != from {
            return Ok(Applied::NotOwner);
        }
        let version = to_sql("version", record.version())?;
        let (kind, id) = key(record);
        let json = serde_json::to_string(record)?;

        let tx = self.conn.transaction()?;
        let held: Option<(String, i64)> = tx
            .query_row(
                "SELECT owner, version FROM records WHERE kind = ?1 AND id = ?2",
                params![kind, id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        match held {
            Some((held_owner, _)) if held_owner != owner.to_string() => {
                return Ok(Applied::NotOwner);
            }
            Some((_, held_version)) if version <= held_version => return Ok(Applied::Stale),
            _ => {}
        }
        tx.execute(
            "INSERT INTO records (kind, id, owner, version, record) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (kind, id) DO UPDATE SET version = excluded.version, record = excluded.record",
            params![kind, id, owner.to_string(), version, json],
        )?;
        tx.commit()?;
        Ok(Applied::Stored)
    }

    /// The version for this machine's next write of one of its own records. Each call gives
    /// a higher number than the last, across restarts too.
    pub fn own_next_version(&mut self) -> Result<u64, StoreError> {
        let version: i64 = self.conn.query_row(
            "UPDATE own_version SET version = version + 1 RETURNING version",
            [],
            |row| row.get(0),
        )?;
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
        let mut query = self.conn.prepare(
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
        let json: Option<String> = self
            .conn
            .query_row(
                "SELECT record FROM records WHERE kind = 'workspace' AND id = ?1",
                params![id.to_string()],
                |row| row.get(0),
            )
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

    /// The terminal records held from `owner` in workspace `workspace`, deleted ones included,
    /// lowest version first. Only those rows are parsed.
    pub fn terminals_in(
        &self,
        owner: MachineId,
        workspace: WorkspaceId,
    ) -> Result<Vec<Record<Terminal>>, StoreError> {
        let mut query = self.conn.prepare(
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

    /// The highest version held from each owner.
    pub fn versions(&self) -> Result<BTreeMap<MachineId, u64>, StoreError> {
        let mut query = self
            .conn
            .prepare("SELECT owner, MAX(version) FROM records GROUP BY owner")?;
        let rows = query.query_map([], |row| {
            let owner: String = row.get(0)?;
            let owner = owner.parse().map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(0, Type::Text, Box::new(e))
            })?;
            Ok((MachineId(owner), from_sql(row.get(1)?)))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The shared settings held, or empty settings with `seq` 0 before any arrive.
    pub fn settings(&self) -> Result<SharedSettings, StoreError> {
        let json: Option<String> = self
            .conn
            .query_row("SELECT settings FROM settings", [], |row| row.get(0))
            .optional()?;
        match json {
            Some(json) => Ok(serde_json::from_str(&json)?),
            None => Ok(SharedSettings::default()),
        }
    }

    /// Stores `settings` when their `seq` is higher than the held one's, and says whether it
    /// did. Settings with an equal or lower `seq` change nothing.
    pub fn apply_settings(&mut self, settings: SharedSettings) -> Result<bool, StoreError> {
        let seq = to_sql("settings seq", settings.seq)?;
        let json = serde_json::to_string(&settings)?;

        let tx = self.conn.transaction()?;
        let held: i64 = tx
            .query_row("SELECT seq FROM settings", [], |row| row.get(0))
            .optional()?
            .unwrap_or(0);
        if seq <= held {
            return Ok(false);
        }
        tx.execute(
            "INSERT INTO settings (only, seq, settings) VALUES (1, ?1, ?2)
             ON CONFLICT (only) DO UPDATE SET seq = excluded.seq, settings = excluded.settings",
            params![seq, json],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// How many steps of [`MIGRATIONS`] the file has had.
    #[cfg(test)]
    fn schema_version(&self) -> Result<usize, StoreError> {
        user_version(&self.conn)
    }
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
