//! Load, memory and disk samples, and the machine limits.
//!
//! Every 5 seconds this machine writes one [`MachineSample`] record of its own: the whole
//! machine's load per core, its free memory, the free disk on the volume holding the data
//! directory, and its live session count. The record's version is the sample's `at_version`.
//! [`over_limits`] says which of a machine's limits a sample is at or over, one [`Reason`]
//! per limit.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use foundry_proto::ids::MachineId;
use foundry_proto::record::{AnyRecord, MachineSample, Record};
use foundry_proto::settings::Limits;
use foundry_proto::state::SessionState;
use nix::sys::statvfs::statvfs;
use sysinfo::{CpuRefreshKind, LoadAvg, MemoryRefreshKind, RefreshKind, System};
use tokio::time::MissedTickBehavior;

use crate::store::{Store, StoreError};

/// How often this machine writes its sample.
pub const SAMPLE_EVERY: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum MachineError {
    #[error("machine: the load average could not be read: {0}")]
    Load(sysinfo::Error),
    #[error("machine: memory and cores could not be read: {0}")]
    System(sysinfo::Error),
    #[error("machine: free disk could not be read for {path}: {source}")]
    Disk { path: PathBuf, source: nix::Error },
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// A limit a machine is at or over.
#[derive(Clone, Debug, PartialEq)]
pub struct Reason {
    pub machine: MachineId,
    pub over: Over,
}

/// Which limit, with the sample's value and the limit's.
#[derive(Clone, Debug, PartialEq)]
pub enum Over {
    Sessions { sessions: u32, limit: u32 },
    Load { load_per_core: f64, limit: f64 },
    Disk { free_disk: u64, limit: u64 },
}

impl Reason {
    /// The reason as the UI shows it, such as `trilogy-mac: load 1.8 over limit 1.0`.
    pub fn text(&self, machine_name: &str) -> String {
        let what = match self.over {
            Over::Sessions { sessions, limit } => {
                format!(
                    "sessions {sessions} {} limit {limit}",
                    at_or_over(sessions == limit)
                )
            }
            Over::Load {
                load_per_core,
                limit,
            } => {
                let (value, shown_limit) = apart(load_per_core, limit);
                format!(
                    "load {value} {} limit {shown_limit}",
                    at_or_over(load_per_core == limit)
                )
            }
            Over::Disk { free_disk, limit } => {
                let (value, shown_limit) = apart(gigabytes(free_disk), gigabytes(limit));
                format!("free disk {value} GB under limit {shown_limit} GB")
            }
        };
        format!("{machine_name}: {what}")
    }
}

fn at_or_over(at: bool) -> &'static str {
    if at { "at" } else { "over" }
}

/// Decimal gigabytes, as disk sizes are sold and shown.
fn gigabytes(bytes: u64) -> f64 {
    bytes as f64 / 1e9
}

/// `value` and `limit` at the fewest decimals, two at least, that print them apart, so a
/// reason never reads `load 1.0 over limit 1.0` or shows a value across the limit it is on
/// the other side of. Trailing zeros are dropped down to one decimal: `1.8`, `1.0`, `2.25`.
fn apart(value: f64, limit: f64) -> (String, String) {
    // Byte counts in GB differ by 1e-9 at least, and 17 decimals reach past what an f64 load holds.
    for places in 2..=17 {
        let shown = (format!("{value:.places$}"), format!("{limit:.places$}"));
        if shown.0 != shown.1 {
            return (trimmed(shown.0), trimmed(shown.1));
        }
    }
    let equal = trimmed(format!("{value:.2}"));
    (equal.clone(), equal)
}

/// `text`, a number with decimals, without trailing zeros but with at least one decimal.
fn trimmed(text: String) -> String {
    let text = text.trim_end_matches('0');
    if text.ends_with('.') {
        format!("{text}0")
    } else {
        text.to_string()
    }
}

/// The 1-minute load divided by the logical cores.
pub fn load_per_core(load: &LoadAvg, logical_cores: usize) -> f64 {
    load.one / logical_cores.max(1) as f64
}

/// Bytes free to an unprivileged writer on the volume holding `path`.
pub fn free_disk(path: &Path) -> Result<u64, MachineError> {
    let stat = statvfs(path).map_err(|source| MachineError::Disk {
        path: path.to_path_buf(),
        source,
    })?;
    // The field types differ by platform: u32 or u64 on macOS, u64 on Linux.
    #[allow(clippy::useless_conversion)]
    let (blocks, block_size) = (
        u64::from(stat.blocks_available()),
        u64::from(stat.fragment_size()),
    );
    Ok(blocks.saturating_mul(block_size))
}

/// Reads this machine's load, memory and the free disk for `data_dir`, with `sessions`.
pub fn read(data_dir: &Path, sessions: u32) -> Result<MachineSample, MachineError> {
    let system = System::new_with_specifics(
        RefreshKind::nothing()
            .with_cpu(CpuRefreshKind::nothing())
            .with_memory(MemoryRefreshKind::nothing().with_ram()),
    )
    .map_err(MachineError::System)?;
    let load = System::load_average().map_err(MachineError::Load)?;
    Ok(MachineSample {
        load_per_core: load_per_core(&load, system.cpus().len()),
        free_memory: system.available_memory(),
        free_disk: free_disk(data_dir)?,
        sessions,
        update_refused: None,
    })
}

/// Reads a sample, counting the live sessions `me` owns in `store`, and stores it as `me`'s
/// sample record under its next own version. Returns the record written.
///
/// The machine is read before the store is taken, so a slow volume holds up no other user
/// of the store. It blocks, so async callers run it on the blocking pool, as [`run`] does.
pub fn write_sample(
    store: &Mutex<Store>,
    me: MachineId,
    data_dir: &Path,
) -> Result<Record<MachineSample>, MachineError> {
    let machine = read(data_dir, 0)?;
    // A task that panicked while holding the store leaves it consistent: SQLite rolls back
    // a transaction that was not committed. Stopping here would freeze this machine's last
    // sample while it still looks online.
    let mut store = store.lock().unwrap_or_else(PoisonError::into_inner);
    let body = MachineSample {
        sessions: live_sessions(&store, me)?,
        ..machine
    };
    let record = Record {
        owner: me,
        version: store.own_next_version()?,
        deleted: false,
        body,
    };
    store.apply(&AnyRecord::MachineSample(record.clone()), me)?;
    Ok(record)
}

/// Sessions `me` owns that are not removed and have not finished or failed.
fn live_sessions(store: &Store, me: MachineId) -> Result<u32, MachineError> {
    let live = store
        .since(me, 0)?
        .into_iter()
        .filter(|record| match record {
            AnyRecord::Session(r) => {
                !r.deleted && !matches!(r.body.state, SessionState::Finished | SessionState::Failed)
            }
            _ => false,
        })
        .count();
    Ok(u32::try_from(live).unwrap_or(u32::MAX))
}

/// Writes a sample every [`SAMPLE_EVERY`], the first at once, each on the blocking pool so a
/// slow disk read or a busy store never stalls an async worker. A sample that cannot be read
/// or stored is logged and the next one is tried on time.
pub async fn run(store: Arc<Mutex<Store>>, me: MachineId, data_dir: PathBuf) {
    let mut every = tokio::time::interval(SAMPLE_EVERY);
    every.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        every.tick().await;
        let (store, data_dir) = (store.clone(), data_dir.clone());
        match tokio::task::spawn_blocking(move || write_sample(&store, me, &data_dir)).await {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => tracing::warn!(%error, "machine sample not written"),
            Err(error) => tracing::warn!(%error, "machine sample not written"),
        }
    }
}

/// One reason for each of `limits` that `sample` is at or over, in the order sessions, load,
/// disk. A machine qualifies for a new session only when this is empty.
pub fn over_limits(sample: &Record<MachineSample>, limits: &Limits) -> Vec<Reason> {
    let body = &sample.body;
    let mut over = Vec::new();
    if body.sessions >= limits.max_sessions {
        over.push(Over::Sessions {
            sessions: body.sessions,
            limit: limits.max_sessions,
        });
    }
    if body.load_per_core >= limits.max_load_per_core {
        over.push(Over::Load {
            load_per_core: body.load_per_core,
            limit: limits.max_load_per_core,
        });
    }
    if body.free_disk < limits.min_free_disk {
        over.push(Over::Disk {
            free_disk: body.free_disk,
            limit: limits.min_free_disk,
        });
    }
    over.into_iter()
        .map(|over| Reason {
            machine: sample.owner,
            over,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use foundry_proto::ids::{SessionId, TerminalId, WorkspaceId};
    use foundry_proto::record::Session;

    fn sample(sessions: u32, load_per_core: f64, free_disk: u64) -> Record<MachineSample> {
        Record {
            owner: MachineId::random(),
            version: 9,
            deleted: false,
            body: MachineSample {
                load_per_core,
                free_memory: 8_000_000_000,
                free_disk,
                sessions,
                update_refused: None,
            },
        }
    }

    fn limits() -> Limits {
        Limits {
            max_sessions: 4,
            max_load_per_core: 1.0,
            min_free_disk: 10_000_000_000,
        }
    }

    fn texts(sample: &Record<MachineSample>, limits: &Limits) -> Vec<String> {
        over_limits(sample, limits)
            .iter()
            .map(|r| r.text("trilogy-mac"))
            .collect()
    }

    #[test]
    fn load_per_core_is_the_one_minute_load_over_logical_cores() {
        let load = LoadAvg {
            one: 3.6,
            five: 8.0,
            fifteen: 12.0,
        };
        assert!((load_per_core(&load, 2) - 1.8).abs() < 1e-9);
        assert!((load_per_core(&load, 8) - 0.45).abs() < 1e-9);
    }

    #[test]
    fn free_disk_is_read_for_the_volume_holding_the_data_directory() {
        let dir = tempfile::tempdir().unwrap();
        let ours = free_disk(dir.path()).unwrap();

        // `df -P` reports the same volume's space available to unprivileged writers, in
        // 1024-byte blocks. Other programs write meanwhile, so allow some drift.
        let out = std::process::Command::new("df")
            .arg("-Pk")
            .arg(dir.path())
            .output()
            .unwrap();
        assert!(out.status.success());
        let text = String::from_utf8(out.stdout).unwrap();
        let line = text.lines().nth(1).expect("df prints one volume");
        let available: u64 = line.split_whitespace().nth(3).unwrap().parse().unwrap();
        let df = available * 1024;
        assert!(
            ours.abs_diff(df) < 512 * 1024 * 1024,
            "free_disk {ours} and df {df} disagree"
        );
    }

    #[test]
    fn free_disk_fails_for_a_missing_directory() {
        let dir = tempfile::tempdir().unwrap();
        let err = free_disk(&dir.path().join("missing")).unwrap_err();
        assert!(err.to_string().contains("missing"), "{err}");
    }

    #[test]
    fn read_gives_this_machines_numbers() {
        let dir = tempfile::tempdir().unwrap();
        let sample = read(dir.path(), 3).unwrap();
        assert!(sample.load_per_core.is_finite() && sample.load_per_core >= 0.0);
        assert!(sample.free_memory > 0);
        assert!(sample.free_disk > 0);
        assert_eq!(sample.sessions, 3);
    }

    #[test]
    fn read_divides_the_load_by_this_machines_logical_cores() {
        let dir = tempfile::tempdir().unwrap();
        let cores = std::thread::available_parallelism().unwrap().get() as f64;
        let one_minute = System::load_average().unwrap().one;
        let sample = read(dir.path(), 0).unwrap();
        // Compared in whole-machine load, so a wrong divisor shows even on an idle machine.
        // The load average moves a little between the two reads.
        assert!(
            (sample.load_per_core * cores - one_minute).abs() < 0.25,
            "load per core {} times {cores} cores is not the 1-minute load {one_minute}",
            sample.load_per_core
        );
    }

    #[test]
    fn under_every_limit_gives_no_reason() {
        assert!(over_limits(&sample(3, 0.9, 10_000_000_000), &limits()).is_empty());
    }

    #[test]
    fn each_limit_gives_its_own_reason_text() {
        assert_eq!(
            texts(&sample(0, 1.8, 50_000_000_000), &limits()),
            ["trilogy-mac: load 1.8 over limit 1.0"]
        );
        assert_eq!(
            texts(&sample(0, 1.837, 50_000_000_000), &limits()),
            ["trilogy-mac: load 1.84 over limit 1.0"]
        );
        assert_eq!(
            texts(&sample(5, 0.2, 50_000_000_000), &limits()),
            ["trilogy-mac: sessions 5 over limit 4"]
        );
        assert_eq!(
            texts(&sample(0, 0.2, 4_200_000_000), &limits()),
            ["trilogy-mac: free disk 4.2 GB under limit 10.0 GB"]
        );
    }

    #[test]
    fn a_value_never_prints_equal_to_a_limit_it_is_over_or_under() {
        assert_eq!(
            texts(&sample(0, 1.004, 50_000_000_000), &limits()),
            ["trilogy-mac: load 1.004 over limit 1.0"]
        );
        assert_eq!(
            texts(&sample(0, 0.2, 9_999_000_000), &limits()),
            ["trilogy-mac: free disk 9.999 GB under limit 10.0 GB"]
        );
        // A 10-core Mac's load moves in steps of 1/2048: 20481/2048 over 10 cores.
        assert_eq!(
            texts(
                &sample(0, 20481.0 / 2048.0 / 10.0, 50_000_000_000),
                &limits()
            ),
            ["trilogy-mac: load 1.00005 over limit 1.0"]
        );
        // One 4096-byte block under the limit.
        assert_eq!(
            texts(&sample(0, 0.2, 9_999_995_904), &limits()),
            ["trilogy-mac: free disk 9.999996 GB under limit 10.0 GB"]
        );
        // One byte under the limit.
        assert_eq!(
            texts(&sample(0, 0.2, 9_999_999_999), &limits()),
            ["trilogy-mac: free disk 9.999999999 GB under limit 10.0 GB"]
        );
        // Both numbers are widened, so the value never reads above a limit it is under.
        let gib_limit = Limits {
            min_free_disk: 21_474_836_480,
            ..limits()
        };
        assert_eq!(
            texts(&sample(0, 0.2, 21_470_500_000), &gib_limit),
            ["trilogy-mac: free disk 21.471 GB under limit 21.475 GB"]
        );
    }

    #[test]
    fn a_maximum_is_reached_at_its_value() {
        // A machine at its maximum sessions has no room for another.
        assert_eq!(
            texts(&sample(4, 1.0, 10_000_000_000), &limits()),
            [
                "trilogy-mac: sessions 4 at limit 4",
                "trilogy-mac: load 1.0 at limit 1.0",
            ]
        );
    }

    #[test]
    fn every_limit_over_gives_every_reason_for_that_machine() {
        let sample = sample(6, 2.25, 1_500_000_000);
        let reasons = over_limits(&sample, &limits());
        assert_eq!(
            reasons,
            [
                Reason {
                    machine: sample.owner,
                    over: Over::Sessions {
                        sessions: 6,
                        limit: 4
                    },
                },
                Reason {
                    machine: sample.owner,
                    over: Over::Load {
                        load_per_core: 2.25,
                        limit: 1.0
                    },
                },
                Reason {
                    machine: sample.owner,
                    over: Over::Disk {
                        free_disk: 1_500_000_000,
                        limit: 10_000_000_000
                    },
                },
            ]
        );
        assert_eq!(
            texts(&sample, &limits()),
            [
                "trilogy-mac: sessions 6 over limit 4",
                "trilogy-mac: load 2.25 over limit 1.0",
                "trilogy-mac: free disk 1.5 GB under limit 10.0 GB",
            ]
        );
    }

    fn put_session(store: &mut Store, owner: MachineId, state: SessionState, deleted: bool) {
        let version = store.own_next_version().unwrap();
        let record = AnyRecord::Session(Record {
            owner,
            version,
            deleted,
            body: Session {
                id: SessionId::random(),
                workspace: WorkspaceId::random(),
                terminal: TerminalId::random(),
                model: "claude-opus-5-5".into(),
                state,
            },
        });
        store.apply(&record, owner).unwrap();
    }

    fn held_sample(store: &Store, me: MachineId) -> Option<Record<MachineSample>> {
        store
            .since(me, 0)
            .unwrap()
            .into_iter()
            .find_map(|r| match r {
                AnyRecord::MachineSample(r) => Some(r),
                _ => None,
            })
    }

    #[test]
    fn write_sample_stores_this_machines_sample_with_its_live_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("foundry.db")).unwrap();
        let me = MachineId::random();
        let peer = MachineId::random();
        for state in [
            SessionState::Starting,
            SessionState::Working,
            SessionState::WaitingForYou,
            SessionState::Idle,
            SessionState::Stuck,
            SessionState::Finished,
            SessionState::Failed,
        ] {
            put_session(&mut store, me, state, false);
        }
        put_session(&mut store, me, SessionState::Working, true);
        put_session(&mut store, peer, SessionState::Working, false);
        let last_session_version = store.versions().unwrap()[&me];
        let store = Mutex::new(store);

        let written = write_sample(&store, me, dir.path()).unwrap();

        assert_eq!(written.owner, me);
        assert!(!written.deleted);
        assert!(written.version > last_session_version);
        assert_eq!(written.body.sessions, 5);
        assert!(written.body.free_disk > 0);
        assert!(written.body.free_memory > 0);
        assert_eq!(
            held_sample(&store.lock().unwrap(), me),
            Some(written.clone())
        );

        // The sample just stored is this machine's record too, and is not a session.
        let again = write_sample(&store, me, dir.path()).unwrap();
        assert!(again.version > written.version);
        assert_eq!(again.body.sessions, 5);
    }

    #[test]
    fn write_sample_reads_the_machine_before_taking_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Mutex::new(
            Store::open(dir.path().join("foundry.db")).unwrap(),
        ));
        let (held, release) = (std::sync::mpsc::channel(), std::sync::mpsc::channel::<()>());
        let holder = store.clone();
        let holding = std::thread::spawn(move || {
            let _guard = holder.lock().unwrap();
            held.0.send(()).unwrap();
            release.1.recv().unwrap();
        });
        held.1.recv().unwrap();

        // A missing data directory fails the disk read. Read first, that fails at once;
        // with the store taken first, it would wait for the holder.
        let (done, result) = std::sync::mpsc::channel();
        let missing = dir.path().join("missing");
        let writer = store.clone();
        std::thread::spawn(move || {
            done.send(write_sample(&writer, MachineId::random(), &missing).is_err())
                .unwrap();
        });
        let failed = result.recv_timeout(Duration::from_secs(5));

        release.0.send(()).unwrap();
        holding.join().unwrap();
        assert_eq!(failed, Ok(true), "the disk read waited for the store");
    }

    async fn settle() {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
    }

    /// Each pass runs on the blocking pool, so wait in real time, up to 5 s, for the sample
    /// held to differ from `before`, and give what is held then.
    async fn sample_after(
        store: &Arc<Mutex<Store>>,
        me: MachineId,
        before: Option<u64>,
    ) -> Option<u64> {
        for _ in 0..1_000 {
            let held = held_version(store, me);
            if held != before {
                return held;
            }
            tokio::task::yield_now().await;
            std::thread::sleep(Duration::from_millis(5));
        }
        held_version(store, me)
    }

    fn held_version(store: &Arc<Mutex<Store>>, me: MachineId) -> Option<u64> {
        let store = store.lock().unwrap_or_else(PoisonError::into_inner);
        held_sample(&store, me).map(|r| r.version)
    }

    #[tokio::test(start_paused = true)]
    async fn run_writes_a_sample_every_5_seconds() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Mutex::new(
            Store::open(dir.path().join("foundry.db")).unwrap(),
        ));
        let me = MachineId::random();
        let task = tokio::spawn(run(store.clone(), me, dir.path().to_path_buf()));

        let first = sample_after(&store, me, None)
            .await
            .expect("a sample at once");

        tokio::time::advance(Duration::from_millis(4_900)).await;
        settle().await;
        assert_eq!(held_version(&store, me), Some(first));

        tokio::time::advance(Duration::from_millis(100)).await;
        let second = sample_after(&store, me, Some(first))
            .await
            .expect("still a sample");
        assert!(second > first, "{second} after {first}");

        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn run_writes_one_sample_not_a_burst_after_a_stall() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Mutex::new(
            Store::open(dir.path().join("foundry.db")).unwrap(),
        ));
        let me = MachineId::random();
        let task = tokio::spawn(run(store.clone(), me, dir.path().to_path_buf()));
        let first = sample_after(&store, me, None)
            .await
            .expect("a sample at once");

        // Three samples late, as after the machine sleeps. Each write takes one version.
        tokio::time::advance(SAMPLE_EVERY * 3).await;
        assert_eq!(sample_after(&store, me, Some(first)).await, Some(first + 1));
        assert_eq!(
            sample_after(&store, me, Some(first + 1)).await,
            Some(first + 1)
        );

        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn run_keeps_sampling_after_a_failed_read() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Mutex::new(
            Store::open(dir.path().join("foundry.db")).unwrap(),
        ));
        let me = MachineId::random();
        let data = dir.path().join("data");
        let task = tokio::spawn(run(store.clone(), me, data.clone()));

        // The first pass fails on the missing directory. Waiting out the 5 s also lets it end.
        assert_eq!(sample_after(&store, me, None).await, None);

        std::fs::create_dir(&data).unwrap();
        tokio::time::advance(SAMPLE_EVERY).await;
        assert!(sample_after(&store, me, None).await.is_some());

        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn run_keeps_sampling_after_another_task_panics_holding_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Mutex::new(
            Store::open(dir.path().join("foundry.db")).unwrap(),
        ));
        let holder = store.clone();
        std::thread::spawn(move || {
            let _guard = holder.lock().unwrap();
            panic!("a bug elsewhere, holding the store");
        })
        .join()
        .unwrap_err();
        assert!(store.is_poisoned());

        let me = MachineId::random();
        let task = tokio::spawn(run(store.clone(), me, dir.path().to_path_buf()));
        let first = sample_after(&store, me, None)
            .await
            .expect("a sample at once");

        tokio::time::advance(SAMPLE_EVERY).await;
        assert!(sample_after(&store, me, Some(first)).await > Some(first));

        task.abort();
    }

    #[tokio::test]
    async fn run_leaves_the_runtime_free_while_the_store_is_busy() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Mutex::new(
            Store::open(dir.path().join("foundry.db")).unwrap(),
        ));
        let ((held, holding_now), holds) = (std::sync::mpsc::channel(), Duration::from_secs(1));
        let holder = store.clone();
        let holding = std::thread::spawn(move || {
            let _guard = holder.lock().unwrap();
            held.send(()).unwrap();
            std::thread::sleep(holds);
        });
        holding_now.recv().unwrap();

        let task = tokio::spawn(run(
            store.clone(),
            MachineId::random(),
            dir.path().to_path_buf(),
        ));
        // This test's runtime has one thread. A pass that waits for the store on it stalls
        // this 20 ms sleep until the holder lets go.
        let started = std::time::Instant::now();
        tokio::time::sleep(Duration::from_millis(20)).await;
        let slept = started.elapsed();

        holding.join().unwrap();
        task.abort();
        assert!(
            slept < holds / 2,
            "the runtime stalled {slept:?} while the sampler waited for the store"
        );
    }
}
