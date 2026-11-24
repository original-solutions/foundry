//! `foundryd factory export <dir>`: every run this machine holds, written in the runner's layout.
//!
//! The runner keeps each run as a file, `runs/<owner>__<repo>__<N>__<attempt>.jsonl`, of one
//! line per event, and its `retro` reads a folder of them. The export writes each run the store
//! holds, from every writer, as that file: its events in the ledger's order (the lease
//! generation each was written under, then its writer's factory version, never a time), each
//! as [`runner_line`] writes it, with the runner's keys in the runner's order. The generation
//! is not written: the runner's lines have none.
//!
//! The export only reads the store. Each run's file is written whole beside its place and then
//! moved into it, so a `retro` reading the folder meanwhile never finds half a run. Files in
//! `runs/` that are not a run of this store are left as they are.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use foundry_proto::factory::{FactoryBody, RunId};

use super::read::{events, runner_line};
use crate::store::{Store, StoreError};

/// The folder under the export's directory that holds the run files, as in the runner's.
pub const RUNS: &str = "runs";

/// How many factory records one read of the store hands back while the runs are looked for.
const PAGE: usize = 512;

/// Why an export stopped.
#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error("reading the store: {0}")]
    Store(#[from] StoreError),
    #[error("writing {}: {source}", path.display())]
    Write { path: PathBuf, source: io::Error },
}

/// `foundryd factory export <dir>`: writes every run this machine's store holds into `dir`.
/// A machine with no store yet holds no run: `dir/runs` is made empty, and no store is made.
pub fn command(dir: &Path) -> ExitCode {
    let Some(home) = crate::daemon::home() else {
        eprintln!("foundryd factory export: set FOUNDRY_HOME or HOME");
        return ExitCode::FAILURE;
    };
    let path = home.join("store.sqlite");
    let written = if path.exists() {
        Store::open(&path)
            .map_err(ExportError::from)
            .and_then(|store| export(&store, dir))
    } else {
        make_dir(&dir.join(RUNS)).map(|()| 0)
    };
    match written {
        Ok(count) => {
            let runs = if count == 1 { "run" } else { "runs" };
            println!(
                "foundryd factory export: {count} {runs} written to {}",
                dir.join(RUNS).display()
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("foundryd factory export: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Writes every run `store` holds as `dir/runs/<run>.jsonl`, each line an event as the runner
/// writes it, in the ledger's order. Returns how many runs it wrote.
pub fn export(store: &Store, dir: &Path) -> Result<usize, ExportError> {
    let runs_dir = dir.join(RUNS);
    make_dir(&runs_dir)?;
    let runs = runs_by_page(store, PAGE)?;
    for run in &runs {
        let mut text = String::new();
        for event in events(store, run)? {
            text.push_str(&runner_line(&event));
            text.push('\n');
        }
        write_whole(&runs_dir, &format!("{run}.jsonl"), &text)?;
    }
    Ok(runs.len())
}

/// Every run that `store` holds an event of, from any writer, once each, in order. Each
/// writer's records are read `page` at a time, so the store is never read whole at once.
fn runs_by_page(store: &Store, page: usize) -> Result<Vec<RunId>, StoreError> {
    let mut runs = BTreeSet::new();
    for owner in store.factory_versions()?.into_keys() {
        let mut above = 0;
        loop {
            let records = store.factory_since(owner, above, page)?;
            let Some(last) = records.last() else {
                break;
            };
            above = last.version;
            for record in records {
                if let FactoryBody::RunEvent(event) = record.body {
                    runs.insert(event.run);
                }
            }
        }
    }
    Ok(runs.into_iter().collect())
}

fn make_dir(dir: &Path) -> Result<(), ExportError> {
    fs::create_dir_all(dir).map_err(|source| ExportError::Write {
        path: dir.to_owned(),
        source,
    })
}

/// Writes `text` as `dir/name`: whole into a file beside it first, then moved into place, so
/// the file is either as it was or all of `text`.
fn write_whole(dir: &Path, name: &str, text: &str) -> Result<(), ExportError> {
    let path = dir.join(name);
    let part = dir.join(format!(".{name}.part"));
    let failed = |source| ExportError::Write {
        path: path.clone(),
        source,
    };
    if let Err(error) = fs::write(&part, text).and_then(|()| fs::rename(&part, &path)) {
        // Best effort: what is left of the part file is no run file, whatever is in it.
        let _ = fs::remove_file(&part);
        return Err(failed(error));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::{Arc, Mutex};

    use foundry_proto::factory::RunId;
    use foundry_proto::ids::MachineId;
    use serde_json::{Value, json};

    use super::*;
    use crate::factory::ledger::read::{events, runner_line};
    use crate::factory::ledger::testing::{
        T0, ledger, open_store, put, run, step, sync, test_cause, ticking,
    };
    use crate::store::Store;

    const HOUR: u64 = 60 * 60 * 1000;

    fn machine(n: u128) -> MachineId {
        MachineId(uuid::Uuid::from_u128(n))
    }

    /// The text of `run`'s file under `dir`.
    fn file(dir: &Path, run: &RunId) -> String {
        fs::read_to_string(dir.join("runs").join(format!("{run}.jsonl"))).unwrap()
    }

    /// The file names in `dir/runs`, sorted.
    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir.join("runs"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    fn exported(store: &Arc<Mutex<Store>>, dir: &Path) -> usize {
        export(&store.lock().unwrap(), dir).unwrap()
    }

    #[test]
    fn a_run_written_by_two_machines_exports_in_generation_order_whatever_their_clocks() {
        let (_a_dir, a) = open_store();
        let (_b_dir, b) = open_store();
        let (first, second) = (machine(1), machine(2));
        // The second machine's clock is an hour behind the first's, and it has stored many
        // more records than the first, so neither time nor version puts its events first.
        for n in 0..5 {
            put(
                &b,
                second,
                n + 1,
                &run(9),
                1,
                T0,
                "step_changed",
                json!({"step": "other"}),
                None,
            );
        }
        let earlier = ledger(&a, first, 1, ticking(T0 + HOUR, 1));
        earlier
            .write(&run(7), &step("claimed"), &test_cause())
            .unwrap();
        earlier
            .write(&run(7), &step("launched"), &test_cause())
            .unwrap();
        sync(&a, first, &b);
        let later = ledger(&b, second, 2, ticking(T0, 1));
        later
            .write(&run(7), &step("resumed"), &test_cause())
            .unwrap();
        later.write(&run(7), &step("gated"), &test_cause()).unwrap();
        let out = tempfile::tempdir().unwrap();

        exported(&b, out.path());

        let text = file(out.path(), &run(7));
        let steps: Vec<String> = text
            .lines()
            .map(|line| {
                let line: Value = serde_json::from_str(line).unwrap();
                line["data"]["step"].as_str().unwrap().to_owned()
            })
            .collect();
        assert_eq!(steps, ["claimed", "launched", "resumed", "gated"]);
        let held = events(&b.lock().unwrap(), &run(7)).unwrap();
        let expected: String = held.iter().map(|e| runner_line(e) + "\n").collect();
        assert_eq!(text, expected);
    }

    #[test]
    fn an_exported_line_holds_the_runner_keys_in_the_runner_order_and_no_generation() {
        let (_dir, store) = open_store();
        put(
            &store,
            machine(1),
            1,
            &run(7),
            3,
            T0,
            "gate_command",
            json!({"exit_code": 0, "duration_ms": 10, "command": "cargo test", "attempt": 1}),
            Some(
                json!({"attempt": 1, "command": "cargo test", "reason": "gate-command-ran", "step": "gate"}),
            ),
        );
        let out = tempfile::tempdir().unwrap();

        exported(&store, out.path());

        assert_eq!(
            file(out.path(), &run(7)),
            "{\"ts\":\"2026-10-04T10:13:16.123Z\",\"ms\":1791108796123,\"event\":\"gate_command\",\
             \"data\":{\"attempt\":1,\"command\":\"cargo test\",\"exit_code\":0,\"duration_ms\":10},\
             \"cause\":{\"step\":\"gate\",\"reason\":\"gate-command-ran\",\"command\":\"cargo test\",\
             \"attempt\":1}}\n"
        );
    }

    #[test]
    fn every_run_of_every_repo_and_writer_gets_a_file_named_for_its_run() {
        let (_dir, store) = open_store();
        let other = RunId::new("Original-Solutions/Foundry", 179, 2).unwrap();
        put(
            &store,
            machine(1),
            1,
            &run(7),
            1,
            T0,
            "step_changed",
            json!({"step": "a"}),
            None,
        );
        put(
            &store,
            machine(2),
            1,
            &other,
            1,
            T0,
            "step_changed",
            json!({"step": "b"}),
            None,
        );
        put(
            &store,
            machine(2),
            2,
            &run(8),
            1,
            T0,
            "step_changed",
            json!({"step": "c"}),
            None,
        );
        put(
            &store,
            machine(1),
            2,
            &run(7),
            1,
            T0,
            "step_changed",
            json!({"step": "d"}),
            None,
        );
        let out = tempfile::tempdir().unwrap();

        let count = exported(&store, out.path());

        assert_eq!(count, 3);
        assert_eq!(
            names(out.path()),
            [
                "original-solutions__foundry__179__2.jsonl",
                "rawphp__softfac__7__1.jsonl",
                "rawphp__softfac__8__1.jsonl",
            ]
        );
        assert_eq!(file(out.path(), &run(7)).lines().count(), 2);
    }

    #[test]
    fn the_runs_are_found_however_many_pages_the_records_take() {
        let (_dir, store) = open_store();
        for n in 1..=7 {
            put(
                &store,
                machine(1),
                n,
                &run(n),
                1,
                T0,
                "step_changed",
                json!({"step": "a"}),
                None,
            );
        }
        put(
            &store,
            machine(2),
            1,
            &run(20),
            1,
            T0,
            "step_changed",
            json!({"step": "a"}),
            None,
        );

        let found = runs_by_page(&store.lock().unwrap(), 2).unwrap();

        let expected: Vec<RunId> = [1, 2, 20, 3, 4, 5, 6, 7].into_iter().map(run).collect();
        let mut expected = expected;
        expected.sort();
        assert_eq!(found, expected);
    }

    #[test]
    fn a_store_with_no_run_exports_an_empty_runs_folder() {
        let (_dir, store) = open_store();
        let out = tempfile::tempdir().unwrap();

        let count = exported(&store, out.path());

        assert_eq!(count, 0);
        assert!(names(out.path()).is_empty());
    }

    #[test]
    fn an_export_replaces_its_runs_files_and_leaves_every_other_file_as_it_was() {
        let (_dir, store) = open_store();
        put(
            &store,
            machine(1),
            1,
            &run(7),
            1,
            T0,
            "step_changed",
            json!({"step": "a"}),
            None,
        );
        let out = tempfile::tempdir().unwrap();
        let runs = out.path().join("runs");
        fs::create_dir_all(&runs).unwrap();
        fs::write(
            runs.join("rawphp__softfac__7__1.jsonl"),
            "stale\nstale\nstale\n",
        )
        .unwrap();
        fs::write(runs.join("someone__else__1__1.jsonl"), "theirs\n").unwrap();

        exported(&store, out.path());

        assert_eq!(file(out.path(), &run(7)).lines().count(), 1);
        assert!(!file(out.path(), &run(7)).contains("stale"));
        assert_eq!(
            fs::read_to_string(runs.join("someone__else__1__1.jsonl")).unwrap(),
            "theirs\n"
        );
        assert_eq!(
            names(out.path()),
            ["rawphp__softfac__7__1.jsonl", "someone__else__1__1.jsonl"]
        );
    }

    #[test]
    fn an_export_into_a_path_that_is_a_file_fails_and_names_it() {
        let (_dir, store) = open_store();
        put(
            &store,
            machine(1),
            1,
            &run(7),
            1,
            T0,
            "step_changed",
            json!({"step": "a"}),
            None,
        );
        let out = tempfile::tempdir().unwrap();
        let blocked = out.path().join("blocked");
        fs::write(&blocked, "a file").unwrap();

        let error = export(&store.lock().unwrap(), &blocked).unwrap_err();

        assert!(error.to_string().contains("blocked"), "error: {error}");
    }
}
