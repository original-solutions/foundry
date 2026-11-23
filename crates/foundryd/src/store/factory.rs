//! The factory table, its versions and its reads.
//!
//! Factory records are kept apart from P1's `records`, under the same rules: one is taken only
//! from its owner and only when its version is higher than the copy held. The versions are the
//! owner's own factory counter, which is not P1's. Nothing here reads or orders by a time:
//! a run's events are ordered by the lease generation they were written under, then by version.

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

fn parse_machine(index: usize, text: &str) -> rusqlite::Result<MachineId> {
    text.parse()
        .map(MachineId)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(index, Type::Text, Box::new(e)))
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
        let mut events = Vec::new();
        for json in rows {
            let record: FactoryRecord = serde_json::from_str(&json?)?;
            if let FactoryBody::RunEvent(event) = record.body {
                events.push(event);
            }
        }
        Ok(events)
    }

    /// Every run of `repo` (`owner/name`, in any case) that has an event, once each, in order.
    pub fn runs_of(&self, repo: &str) -> Result<Vec<RunId>, StoreError> {
        let repo = repo.to_lowercase();
        let mut query = self.conn.prepare_cached(
            "SELECT DISTINCT json_extract(record, '$.body.run') FROM factory_records
             WHERE kind = 'run_event'",
        )?;
        let rows = query.query_map([], |row| row.get::<_, String>(0))?;
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
    pub fn statuses(
        &self,
        project: ProjectId,
    ) -> Result<Vec<(MachineId, FactoryStatus)>, StoreError> {
        let (from, to) = range(&project.to_string());
        let mut query = self.conn.prepare_cached(
            "SELECT record FROM factory_records WHERE kind = 'factory_status' \
             AND id >= ?1 AND id < ?2",
        )?;
        let rows = query.query_map(params![from, to], |row| row.get::<_, String>(0))?;
        let mut statuses = Vec::new();
        for json in rows {
            let record: FactoryRecord = serde_json::from_str(&json?)?;
            if let FactoryBody::Status(status) = record.body {
                statuses.push((record.owner, status));
            }
        }
        statuses.sort_by_key(|(machine, _)| *machine);
        Ok(statuses)
    }

    /// The highest lease generation any writer's status for `project` holds, or `0` with none.
    pub fn highest_generation(&self, project: ProjectId) -> Result<u64, StoreError> {
        Ok(self
            .statuses(project)?
            .iter()
            .map(|(_, status)| status.generation)
            .max()
            .unwrap_or(0))
    }

    /// The logins `machine` holds a token for, read by its key.
    pub fn held(&self, machine: MachineId) -> Result<Option<MachineHeld>, StoreError> {
        Ok(
            match self.factory_by_key("machine_held", &machine.to_string())? {
                Some(FactoryBody::Held(held)) => Some(held),
                _ => None,
            },
        )
    }

    /// The newest turn of `session`, read by its key.
    pub fn session_turn(&self, session: SessionId) -> Result<Option<SessionTurn>, StoreError> {
        Ok(
            match self.factory_by_key("session_turn", &session.to_string())? {
                Some(FactoryBody::Turn(turn)) => Some(turn),
                _ => None,
            },
        )
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
