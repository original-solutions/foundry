//! The ledger: each run's events, read back, listed and exported.
//!
//! A port of the runner's `src/ledger/` (`ledger.ts`, `time.ts` and `limits.ts` here, with
//! `events.ts`, `read.ts` and `listRuns.ts` in the modules below). The runner appends each
//! event as one line of a run file; here each event is a `run_event` factory record of the
//! machine that wrote it, so the store, which syncs them, replaces the files
//! (`appendFile.ts` and `machineLog.ts` are not ported).
//!
//! A run may be written by more than one machine over its life, one lease generation after
//! another. Its events are read in order of the generation they were written under, then of
//! the writer's factory version (F6). The wall-clock time each event carries is what the
//! runner records and what `limits.minutes` counts from; it never orders anything, so a
//! machine whose clock is behind cannot move its events before an earlier holder's.
//!
//! Before an event is stored, every text in its data and cause goes through the ledger's
//! [`Hide`], which hides the secrets this machine holds, and only then is it cut: each text of
//! the data to its last [`OUTPUT_LIMIT`] bytes, and the cause's as the runner cuts them.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use foundry_proto::factory::{FactoryBody, FactoryRecord, RunEvent, RunId};
use foundry_proto::ids::MachineId;
use serde_json::Value;

use crate::store::{Applied, Store, StoreError};

use events::{Cause, Event, Outcome, PrEnd, names};

pub mod events;
pub mod export;
pub mod list;
pub mod read;

/// The most output an event keeps, in bytes: a gate or setup tail is cut to its last 64 KB.
pub const OUTPUT_LIMIT: usize = 64 * 1024;

/// Hides the secrets this machine holds in a text. The daemon's must hide every `github:*`
/// token and gateway key in its secret store at the time of the call, and URL passwords, as
/// task 7's `secrets::hide_known` will; the ledger is given it when it is made.
pub type Hide = Arc<dyn Fn(&str) -> String + Send + Sync>;

/// The wall-clock time as Unix epoch milliseconds: what an event is stamped with.
pub type WallClock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// This machine's wall-clock time as Unix epoch milliseconds; 0 for a clock set before 1970.
pub fn system_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

/// Why the ledger did not write an event.
#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    /// The run has its `finished` event; nothing more is written to it.
    #[error("run {run} is already finished")]
    AlreadyFinished { run: RunId },
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Writes a run's events as this machine's `run_event` records, under the lease generation it
/// was given. Cheap to clone: every clone writes to the same store, and shares what the ledger
/// has read of each run.
#[derive(Clone)]
pub struct Ledger {
    store: Arc<Mutex<Store>>,
    me: MachineId,
    generation: u64,
    hide: Hide,
    wall: WallClock,
    tallies: Arc<Mutex<BTreeMap<RunId, RunTally>>>,
}

/// What a ledger has read of one run's events: all a write needs, so the run's history is read
/// once and each later write reads only what was stored since.
///
/// A stored run event never changes or goes (its key holds its writer and version), and each
/// writer's records are stored in the order of their versions, so what was read stays true and
/// the highest version held from a writer is where its next read starts. The writers are
/// looked for again on every read, so a `finished` event that another machine wrote, synced in
/// since, is seen. Once the run is finished nothing more is kept of it, since nothing more is
/// written to it.
#[derive(Default)]
struct RunTally {
    /// The version each writer's next read of the run starts above: the highest version held
    /// from it, of any record, when it was last read.
    read_to: BTreeMap<MachineId, u64>,
    /// The run has its `finished` event, whatever its data.
    finished: bool,
    /// The run's first event, by generation, then version, then writer, as the store orders
    /// them: that order and the event's time.
    first: Option<((u64, u64, String), u64)>,
    /// The distinct attempts of the run's `gate_command` events that read whole.
    gate_attempts: BTreeSet<u32>,
}

impl RunTally {
    /// Takes in `event`, which `writer` stored at `version`. Nothing is kept of a finished run
    /// but that it is finished.
    fn take(&mut self, writer: MachineId, version: u64, event: &RunEvent) {
        if event.event == names::FINISHED {
            self.finished = true;
        }
        if self.finished {
            self.forget_all_but_finished();
            return;
        }
        self.read_to_at_least(writer, version);
        let order = (event.generation, version, writer.to_string());
        if self.first.as_ref().is_none_or(|(first, _)| order < *first) {
            self.first = Some((order, event.ms));
        }
        if let Some(attempt) = gate_attempt(event) {
            self.gate_attempts.insert(attempt);
        }
    }

    /// Keeps only that the run is finished: nothing more is written to it, so nothing more of
    /// it is needed.
    fn forget_all_but_finished(&mut self) {
        *self = Self {
            finished: true,
            ..Self::default()
        };
    }

    /// Moves `writer`'s next read to start above `version`, if it does not already.
    fn read_to_at_least(&mut self, writer: MachineId, version: u64) {
        let read_to = self.read_to.entry(writer).or_insert(version);
        *read_to = (*read_to).max(version);
    }

    /// Reads into this what `store` holds of `run` that has not been read yet: every event of
    /// a writer not read before, and each other writer's events above where its last read
    /// ended. A row this build cannot read is an error, and nothing of its writer is taken.
    ///
    /// Each writer's next read then starts above the highest version held from it, so the
    /// records it stores of other runs meanwhile are not visited again by every later write.
    /// `store` is the locked store, so nothing of a writer is stored between its two reads.
    fn catch_up(&mut self, store: &Store, run: &RunId) -> Result<(), StoreError> {
        for writer in store.run_writers(run)? {
            let above = self.read_to.get(&writer).copied();
            for (version, event) in store.run_events_by(run, writer, above)? {
                self.take(writer, version, &event);
            }
            if self.finished {
                return Ok(());
            }
            if let Some(highest) = store.highest_factory_version(writer)? {
                self.read_to_at_least(writer, highest);
            }
        }
        Ok(())
    }
}

impl Ledger {
    /// A ledger of machine `me` that writes at lease `generation`, hiding text with `hide` and
    /// stamping events with the system's wall clock.
    pub fn new(store: Arc<Mutex<Store>>, me: MachineId, generation: u64, hide: Hide) -> Self {
        Self {
            store,
            me,
            generation,
            hide,
            wall: Arc::new(system_ms),
            tallies: Arc::default(),
        }
    }

    /// The same ledger, stamping events with `wall`.
    pub fn with_wall(self, wall: WallClock) -> Self {
        Self { wall, ..self }
    }

    /// The same ledger at lease `generation`.
    pub fn at(&self, generation: u64) -> Self {
        Self {
            generation,
            ..self.clone()
        }
    }

    /// The lease generation this ledger writes at.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The store, locked. A task that panicked while holding it leaves it consistent: SQLite
    /// rolls back what it did not commit.
    fn lock(&self) -> MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Writes `event` as the next event of `run`, stamped with this ledger's generation and the
    /// time, and gives back what was stored. A `finished` event gets the run's own `total_ms`,
    /// the time from its first event (never negative), and `gate_attempts`, the distinct
    /// attempts among its `gate_command` events; see [`finish`](Self::finish). Nothing is
    /// written to a run that has its `finished` event, whoever wrote it.
    pub fn write(
        &self,
        run: &RunId,
        event: &Event,
        cause: &Cause,
    ) -> Result<RunEvent, LedgerError> {
        let (name, mut data) = event.parts();
        // Hiding reads the secret store, which can be slow, so it is done before the store is
        // locked. Nothing it hides depends on what the store holds.
        hide_texts(&mut data, &*self.hide);
        let cause = serde_json::to_value(cause.cleaned_with(&*self.hide))
            .expect("a cause is an object of strings and a number");
        // The lock is held from the read to the write, so this machine's own writes of one
        // run never cross. What the ledger has read of the run is locked inside it, always in
        // that order.
        let mut store = self.lock();
        let mut tallies = self.tallies.lock().unwrap_or_else(PoisonError::into_inner);
        let tally = tallies.entry(run.clone()).or_default();
        // Only the events stored since the ledger last read the run are read, so a write does
        // not read the run's whole history again. A run once finished stays finished, so it is
        // not read again at all.
        if !tally.finished {
            tally.catch_up(&store, run)?;
        }
        // A `finished` event ends the run even when its data cannot be read.
        if tally.finished {
            return Err(LedgerError::AlreadyFinished { run: run.clone() });
        }
        let ms = (self.wall)();
        // Two numbers, which need no hiding.
        if let (Event::Finished { .. }, Value::Object(fields)) = (event, &mut data) {
            let first = tally.first.as_ref().map_or(ms, |(_, first)| *first);
            let attempts = u32::try_from(tally.gate_attempts.len()).unwrap_or(u32::MAX);
            fields.insert("total_ms".into(), ms.saturating_sub(first).into());
            fields.insert("gate_attempts".into(), attempts.into());
        }
        let written = RunEvent {
            run: run.clone(),
            generation: self.generation,
            ts: format_ts(ms),
            ms,
            event: name.to_owned(),
            data,
            cause: Some(cause),
        };
        let version = store.own_next_factory_version()?;
        let record = FactoryRecord {
            owner: self.me,
            version,
            body: FactoryBody::RunEvent(written.clone()),
        };
        if store.apply_factory(&record, self.me)? == Applied::Stored {
            tally.take(self.me, version, &written);
        }
        Ok(written)
    }

    /// Writes the `finished` event that ends `run`. The ledger fills in the total time since
    /// the run's first event and the number of distinct gate attempts. `pr_end` is how the
    /// pull request ended, for a run that ended because it did.
    pub fn finish(
        &self,
        run: &RunId,
        outcome: Outcome,
        review_cycles: Option<u32>,
        pr_url: Option<&str>,
        cause: &Cause,
        pr_end: Option<PrEnd>,
    ) -> Result<RunEvent, LedgerError> {
        let event = Event::Finished {
            outcome,
            total_ms: 0,
            gate_attempts: 0,
            review_cycles,
            pr_url: pr_url.map(str::to_owned),
            pr_end,
        };
        self.write(run, &event, cause)
    }
}

/// The attempt of `event` when it is a `gate_command` that reads whole as its typed event, as
/// only one the runner's reader reads whole counts: an attempt that is a u32, a command, an exit
/// code that is an i32 and a duration that is a u64.
fn gate_attempt(event: &RunEvent) -> Option<u32> {
    if event.event != names::GATE_COMMAND {
        return None;
    }
    match Event::from_parts(&event.event, &event.data)? {
        Event::GateCommand { attempt, .. } => Some(attempt),
        _ => None,
    }
}

/// Every text in `value` through `hide`, then cut to its last [`OUTPUT_LIMIT`] bytes. Hiding
/// comes first, so a cut can never leave the end of a secret that the hiding would no longer
/// recognise. A text is kept as it is, its lines and control characters too, unless `hide`
/// finds a secret only once the control characters that are not white space are gone: then it
/// is kept without them, with that secret hidden. Line breaks and tabs stay, so two lines are
/// never joined into something that looks like a secret.
///
/// So a known token split by white space (a line break, a tab, a space) is not hidden here, and
/// is stored as it came. Only `hide` knows the values it hides, so only it can tell a split
/// token from two lines that merely join into something that looks like one.
fn hide_texts(value: &mut Value, hide: &(dyn Fn(&str) -> String + Send + Sync)) {
    match value {
        Value::String(text) => {
            let hidden = hide(text);
            let stripped = events::strip_inline(&hidden);
            let again = hide(&stripped);
            let kept = if again == stripped { hidden } else { again };
            *text = cut_output(&kept).to_owned();
        }
        Value::Array(items) => {
            for item in items {
                hide_texts(item, hide);
            }
        }
        Value::Object(fields) => {
            for item in fields.values_mut() {
                hide_texts(item, hide);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// The last [`OUTPUT_LIMIT`] bytes of `text`, cut on a character boundary: the whole text when
/// it is no longer than that.
pub fn cut_output(text: &str) -> &str {
    let mut at = text.len().saturating_sub(OUTPUT_LIMIT);
    while !text.is_char_boundary(at) {
        at += 1;
    }
    &text[at..]
}

// The runner's `time.ts`.

/// Unix epoch milliseconds as RFC 3339 UTC with millisecond precision, as the runner writes
/// an event's `ts`.
pub fn format_ts(ms: u64) -> String {
    // Every u64 of milliseconds is under 2^64 / 86 400 000 days, which fits an i64 with room.
    let ms = i64::try_from(ms).unwrap_or(i64::MAX);
    let secs = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let of_day = secs.rem_euclid(86_400);
    let (hour, minute, second) = (of_day / 3_600, of_day % 3_600 / 60, of_day % 60);
    // Days since 1970-01-01 to a civil date (Howard Hinnant's `civil_from_days`).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Unix epoch milliseconds from RFC 3339 UTC as [`format_ts`] writes it,
/// `YYYY-MM-DDTHH:MM:SSZ` with a fraction of one to three digits before the `Z` when there is
/// one. Anything else, such as an offset in place of the `Z`, a date that is not on the
/// calendar, a trailing newline or a time before the epoch, is `None`.
pub fn parse_ts(text: &str) -> Option<u64> {
    let body = text.strip_suffix('Z')?;
    let (whole, fraction) = match body.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (body, None),
    };
    let bytes = whole.as_bytes();
    if bytes.len() != 19 {
        return None;
    }
    let shape_ok = bytes.iter().enumerate().all(|(i, b)| match i {
        4 | 7 => *b == b'-',
        10 => *b == b'T',
        13 | 16 => *b == b':',
        _ => b.is_ascii_digit(),
    });
    if !shape_ok {
        return None;
    }
    let number = |from: usize, to: usize| -> i64 {
        whole[from..to]
            .bytes()
            .fold(0, |n, b| n * 10 + i64::from(b - b'0'))
    };
    let (year, month, day) = (number(0, 4), number(5, 7), number(8, 10));
    let (hour, minute, second) = (number(11, 13), number(14, 16), number(17, 19));
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let month_days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return None,
    };
    if day < 1 || day > month_days || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let millis = match fraction {
        None => 0,
        Some(digits) => {
            if digits.is_empty() || digits.len() > 3 || !digits.bytes().all(|b| b.is_ascii_digit())
            {
                return None;
            }
            // `.1` is a tenth of a second: pad on the right to three digits.
            format!("{digits:0<3}").parse::<i64>().ok()?
        }
    };
    // A civil date to days since 1970-01-01 (Howard Hinnant's `days_from_civil`).
    let y = year - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let year_of_era = y - era * 400;
    let mp = (month + 9) % 12;
    let day_of_year = (153 * mp + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    let ms = (days * 86_400 + hour * 3_600 + minute * 60 + second) * 1000 + millis;
    u64::try_from(ms).ok()
}

/// A length of time as the runner prints it: `12s`, `5m`, `2h05m` or `1d01h`, by the largest
/// unit that fits.
pub fn duration(ms: u64) -> String {
    let secs = ms / 1000;
    match secs {
        0..60 => format!("{secs}s"),
        60..3_600 => format!("{}m", secs / 60),
        3_600..86_400 => format!("{}h{:02}m", secs / 3_600, secs % 3_600 / 60),
        _ => format!("{}d{:02}h", secs / 86_400, secs % 86_400 / 3_600),
    }
}

/// What the ledger's tests share: a store, clocks the test moves, and the runner's samples.
#[cfg(test)]
pub(crate) mod testing {
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use foundry_proto::factory::{FactoryBody, FactoryRecord, RunEvent, RunId};
    use foundry_proto::ids::MachineId;
    use serde_json::Value;
    use tempfile::TempDir;

    use super::events::{Cause, CauseStep, Event};
    use super::{Hide, Ledger, WallClock};
    use crate::store::Store;

    /// 2026-10-04T10:13:16.123Z
    pub const T0: u64 = 1_791_108_796_123;

    /// A wall clock that starts at `start` and moves on by `step` ms every time it is read.
    pub fn ticking(start: u64, step: u64) -> WallClock {
        let now = Arc::new(AtomicU64::new(start));
        Arc::new(move || now.fetch_add(step, Ordering::SeqCst))
    }

    /// A wall clock that gives `times` in order, and panics when it is read more often.
    pub fn scripted(times: &[u64]) -> WallClock {
        let times = times.to_vec();
        let next = Arc::new(AtomicUsize::new(0));
        Arc::new(move || {
            let at = next.fetch_add(1, Ordering::SeqCst);
            *times.get(at).expect("clock read too often")
        })
    }

    /// Hides nothing.
    pub fn shown() -> Hide {
        Arc::new(|text: &str| text.to_owned())
    }

    /// A store of its own in a directory that lives as long as the returned guard.
    pub fn open_store() -> (TempDir, Arc<Mutex<Store>>) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("foundry.db")).unwrap();
        (dir, Arc::new(Mutex::new(store)))
    }

    /// A ledger of `me` at `generation` that hides nothing.
    pub fn ledger(
        store: &Arc<Mutex<Store>>,
        me: MachineId,
        generation: u64,
        wall: WallClock,
    ) -> Ledger {
        Ledger::new(store.clone(), me, generation, shown()).with_wall(wall)
    }

    /// Issue `issue` of `rawphp/softfac`, attempt 1.
    pub fn run(issue: u64) -> RunId {
        RunId::new("rawphp/softfac", issue, 1).unwrap()
    }

    /// The cause of an event a test writes only to have something in the ledger.
    pub fn test_cause() -> Cause {
        Cause::new(CauseStep::Pass, "claimed")
    }

    pub fn step(name: &str) -> Event {
        Event::StepChanged { step: name.into() }
    }

    pub fn gate(attempt: u32, command: &str) -> Event {
        Event::GateCommand {
            attempt,
            command: command.into(),
            exit_code: 0,
            duration_ms: 10,
        }
    }

    pub fn stuck(reason: &str) -> Event {
        Event::Stuck {
            reason: reason.into(),
            branch: None,
            commits_ahead: None,
        }
    }

    /// The events of `run` the store holds, in order.
    pub fn held(store: &Arc<Mutex<Store>>, run: &RunId) -> Vec<RunEvent> {
        store.lock().unwrap().run_events(run).unwrap()
    }

    /// A run event as another writer, `owner`, left it at `version`: whatever its event and
    /// data, as a newer or older build may write them.
    #[allow(clippy::too_many_arguments)]
    pub fn put(
        store: &Arc<Mutex<Store>>,
        owner: MachineId,
        version: u64,
        run: &RunId,
        generation: u64,
        ms: u64,
        event: &str,
        data: Value,
        cause: Option<Value>,
    ) {
        let record = FactoryRecord {
            owner,
            version,
            body: FactoryBody::RunEvent(RunEvent {
                run: run.clone(),
                generation,
                ts: super::format_ts(ms),
                ms,
                event: event.into(),
                data,
                cause,
            }),
        };
        store.lock().unwrap().apply_factory(&record, owner).unwrap();
    }

    /// Copies every factory record `from` holds of its owner `owner` into `to`, as a link does.
    pub fn sync(from: &Arc<Mutex<Store>>, owner: MachineId, to: &Arc<Mutex<Store>>) {
        let records = from
            .lock()
            .unwrap()
            .factory_since(owner, 0, usize::MAX)
            .unwrap();
        let mut to = to.lock().unwrap();
        for record in records {
            to.apply_factory(&record, owner).unwrap();
        }
    }

    const SHA: &str = "4f1c2d3e4a5b60718293a4b5c6d7e8f901234567";
    const BASE_SHA: &str = "1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d";

    /// Every event type, with the exact `data` the runner writes for it, in its key order.
    /// `finished` is last, as written as the only event of a run: the ledger fills in its
    /// summary.
    pub fn samples() -> Vec<(Event, String)> {
        use super::events::{CheckRead, ErrorKind, ModelOption, OptionValue, Outcome, ResumedFrom};
        use crate::factory::lifecycle::State;

        vec![
            (
                Event::Claimed {
                    machine: "mac-mini".into(),
                    github_user: "rawphp".into(),
                },
                r#"{"machine":"mac-mini","github_user":"rawphp"}"#.into(),
            ),
            (
                Event::TriageResult {
                    passed: false,
                    reasons: vec!["no acceptance criteria".into(), "too vague".into()],
                },
                r#"{"passed":false,"reasons":["no acceptance criteria","too vague"]}"#.into(),
            ),
            (
                Event::TriageResult {
                    passed: true,
                    reasons: vec![],
                },
                r#"{"passed":true,"reasons":[]}"#.into(),
            ),
            (
                Event::QuestionsPosted {
                    questions: "Which table?\nAnd \"why\"? café".into(),
                },
                r#"{"questions":"Which table?\nAnd \"why\"? café"}"#.into(),
            ),
            (
                Event::AnswerReceived {
                    comment_id: None,
                    body_edited: true,
                },
                r#"{"comment_id":null,"body_edited":true}"#.into(),
            ),
            (
                Event::AnswerReceived {
                    comment_id: Some("42".into()),
                    body_edited: false,
                },
                r#"{"comment_id":"42","body_edited":false}"#.into(),
            ),
            (
                Event::SessionLaunched {
                    thread_id: "thr_1".into(),
                    worktree: Some("/tmp/wt".into()),
                    branch: Some("issue-8".into()),
                    model_instance: Some("claudeAgent".into()),
                    model: Some("claude-opus-5-5".into()),
                    model_options: Some(vec![ModelOption {
                        id: "reasoningEffort".into(),
                        value: OptionValue::Text("high".into()),
                    }]),
                    resumed_from: Some(ResumedFrom {
                        run: "rawphp__softfac__8__1".into(),
                        branch: "t3code/abc".into(),
                    }),
                    base: None,
                    title: None,
                },
                r#"{"thread_id":"thr_1","worktree":"/tmp/wt","branch":"issue-8","model_instance":"claudeAgent","model":"claude-opus-5-5","model_options":[{"id":"reasoningEffort","value":"high"}],"resumed_from":{"run":"rawphp__softfac__8__1","branch":"t3code/abc"}}"#.into(),
            ),
            (
                Event::SessionLaunched {
                    thread_id: "thr_2".into(),
                    worktree: None,
                    branch: None,
                    model_instance: None,
                    model: None,
                    model_options: None,
                    resumed_from: None,
                    base: Some("main".into()),
                    title: Some("feat: the run ledger".into()),
                },
                r#"{"thread_id":"thr_2","worktree":null,"branch":null,"model_instance":null,"model":null,"model_options":null,"resumed_from":null,"base":"main","title":"feat: the run ledger"}"#.into(),
            ),
            (
                Event::CommentForwarded {
                    comment_id: "42".into(),
                },
                r#"{"comment_id":"42"}"#.into(),
            ),
            (step("review"), r#"{"step":"review"}"#.into()),
            (
                Event::PrOpened {
                    pr: 12,
                    url: "https://github.com/rawphp/softfac/pull/12".into(),
                },
                r#"{"pr":12,"url":"https://github.com/rawphp/softfac/pull/12"}"#.into(),
            ),
            (
                Event::PrLeft {
                    pr: 12,
                    url: "https://github.com/rawphp/softfac/pull/12".into(),
                },
                r#"{"pr":12,"url":"https://github.com/rawphp/softfac/pull/12"}"#.into(),
            ),
            (
                Event::GateCommand {
                    attempt: 1,
                    command: "cargo test".into(),
                    exit_code: -1,
                    duration_ms: 4200,
                },
                r#"{"attempt":1,"command":"cargo test","exit_code":-1,"duration_ms":4200}"#.into(),
            ),
            (
                Event::StateChanged {
                    from: State::Building,
                    to: State::NeedsInfo,
                },
                r#"{"from":"building","to":"needs-info"}"#.into(),
            ),
            (
                stuck("gate failed 3 times"),
                r#"{"reason":"gate failed 3 times"}"#.into(),
            ),
            (
                Event::GateSentBack {
                    attempt: 2,
                    run_ordinal: 7,
                },
                r#"{"attempt":2,"run_ordinal":7}"#.into(),
            ),
            (
                Event::GateCheckout {
                    attempt: 2,
                    dir: "/data/gate/rawphp__softfac__8__1/2".into(),
                    sha: SHA.into(),
                    base_sha: BASE_SHA.into(),
                },
                format!(
                    r#"{{"attempt":2,"dir":"/data/gate/rawphp__softfac__8__1/2","sha":"{SHA}","base_sha":"{BASE_SHA}"}}"#
                ),
            ),
            (
                Event::GateFinished {
                    attempt: 2,
                    total_ms: 754_321,
                },
                r#"{"attempt":2,"total_ms":754321}"#.into(),
            ),
            (
                Event::GateRegate {
                    sha: SHA.into(),
                    base_sha: "9f8e7d6c5b4a39281706f5e4d3c2b1a098765432".into(),
                    previous_base_sha: BASE_SHA.into(),
                    conflict: false,
                },
                format!(
                    r#"{{"sha":"{SHA}","base_sha":"9f8e7d6c5b4a39281706f5e4d3c2b1a098765432","previous_base_sha":"{BASE_SHA}"}}"#
                ),
            ),
            (
                Event::GateRegate {
                    sha: SHA.into(),
                    base_sha: BASE_SHA.into(),
                    previous_base_sha: BASE_SHA.into(),
                    conflict: true,
                },
                format!(
                    r#"{{"sha":"{SHA}","base_sha":"{BASE_SHA}","previous_base_sha":"{BASE_SHA}","conflict":true}}"#
                ),
            ),
            (
                Event::RegateRelaunching {
                    attempt: 1,
                    branch: "t3code/abc".into(),
                    message: "The factory gate failed.\nCommand: cargo test".into(),
                },
                r#"{"attempt":1,"branch":"t3code/abc","message":"The factory gate failed.\nCommand: cargo test"}"#.into(),
            ),
            (
                Event::FeedbackRelaunching {
                    n: 1,
                    branch: "t3code/abc".into(),
                    items: vec!["review:11".into(), "issue-comment:42".into()],
                    message: "The factory forwards new feedback.\nFeedback 1 of 2".into(),
                },
                r#"{"n":1,"branch":"t3code/abc","items":["review:11","issue-comment:42"],"message":"The factory forwards new feedback.\nFeedback 1 of 2"}"#.into(),
            ),
            (
                Event::SessionInterrupted {
                    resumes: 1,
                    relaunch: false,
                    run_ordinal: Some(3),
                    branch: Some("t3code/abc".into()),
                    t3_error: Some("T3 Code is not reachable: timed out".into()),
                    ended_in_pass: 2,
                },
                r#"{"resumes":1,"relaunch":false,"run_ordinal":3,"branch":"t3code/abc","t3_error":"T3 Code is not reachable: timed out","ended_in_pass":2}"#.into(),
            ),
            (
                Event::SessionInterrupted {
                    resumes: 2,
                    relaunch: true,
                    run_ordinal: None,
                    branch: None,
                    t3_error: None,
                    ended_in_pass: 1,
                },
                r#"{"resumes":2,"relaunch":true,"run_ordinal":null,"branch":null,"t3_error":null,"ended_in_pass":1}"#.into(),
            ),
            (
                Event::SessionRateLimited {
                    run_ordinal: 3,
                    branch: Some("t3code/abc".into()),
                    resumes: 1,
                    model: Some("claude-opus-5-5".into()),
                },
                r#"{"run_ordinal":3,"branch":"t3code/abc","resumes":1,"model":"claude-opus-5-5"}"#.into(),
            ),
            (
                Event::SessionPaused {
                    run_ordinal: Some(3),
                },
                r#"{"run_ordinal":3}"#.into(),
            ),
            (
                Event::StopRequested {
                    by: "operator".into(),
                },
                r#"{"by":"operator"}"#.into(),
            ),
            (
                Event::FeedbackForwarding {
                    n: 1,
                    run_ordinal: Some(3),
                    items: vec!["pr-comment:7".into(), "review:11".into()],
                    message: "The factory forwards new feedback.\nFeedback 1 of 2".into(),
                },
                r#"{"n":1,"run_ordinal":3,"items":["pr-comment:7","review:11"],"message":"The factory forwards new feedback.\nFeedback 1 of 2"}"#.into(),
            ),
            (
                Event::PrAdopted {
                    pr: 12,
                    branch: "t3code/abc".into(),
                    run: "rawphp__softfac__8__1".into(),
                },
                r#"{"pr":12,"branch":"t3code/abc","run":"rawphp__softfac__8__1"}"#.into(),
            ),
            (
                Event::BranchBound {
                    pr: 12,
                    branch: "t3code/abc".into(),
                },
                r#"{"pr":12,"branch":"t3code/abc"}"#.into(),
            ),
            (Event::PrSetDraft { pr: 12 }, r#"{"pr":12}"#.into()),
            (
                Event::PrReadyOutside {
                    pr: 12,
                    by: Some("alice".into()),
                },
                r#"{"pr":12,"by":"alice"}"#.into(),
            ),
            (
                Event::PrReadyOutside { pr: 12, by: None },
                r#"{"pr":12}"#.into(),
            ),
            (
                Event::GateCleanupFailed {
                    dir: "/data/gate/rawphp__softfac__8__1/2".into(),
                    error: "could not remove the checkout: permission denied".into(),
                },
                r#"{"dir":"/data/gate/rawphp__softfac__8__1/2","error":"could not remove the checkout: permission denied"}"#.into(),
            ),
            (
                Event::WorktreeRemoved {
                    path: "/t3/worktrees/softfac/t3-01ac8cf3".into(),
                },
                r#"{"path":"/t3/worktrees/softfac/t3-01ac8cf3"}"#.into(),
            ),
            (
                Event::WorktreeRemoveFailed {
                    path: "/t3/worktrees/softfac/t3-01ac8cf3".into(),
                    error: "`git worktree remove --force -- /t3/worktrees/softfac/t3-01ac8cf3` exited with status 128: fatal: cannot remove the worktree".into(),
                },
                r#"{"path":"/t3/worktrees/softfac/t3-01ac8cf3","error":"`git worktree remove --force -- /t3/worktrees/softfac/t3-01ac8cf3` exited with status 128: fatal: cannot remove the worktree"}"#.into(),
            ),
            (
                Event::ChecksRead {
                    sha: SHA.into(),
                    checks: vec![
                        CheckRead {
                            name: "gate".into(),
                            state: "passed".into(),
                        },
                        CheckRead {
                            name: "lint".into(),
                            state: "pending".into(),
                        },
                    ],
                },
                format!(
                    r#"{{"sha":"{SHA}","checks":[{{"name":"gate","state":"passed"}},{{"name":"lint","state":"pending"}}]}}"#
                ),
            ),
            (
                Event::Stuck {
                    reason: "the run passed its time limit".into(),
                    branch: Some("t3code/abc".into()),
                    commits_ahead: Some(2),
                },
                r#"{"reason":"the run passed its time limit","branch":"t3code/abc","commits_ahead":2}"#.into(),
            ),
            (
                Event::Error {
                    step: CauseStep::Gate,
                    kind: ErrorKind::Github,
                    message: "the pull request could not be read".into(),
                },
                r#"{"step":"gate","kind":"github","message":"the pull request could not be read"}"#.into(),
            ),
            (
                Event::Finished {
                    outcome: Outcome::PrOpen,
                    total_ms: 0,
                    gate_attempts: 0,
                    review_cycles: None,
                    pr_url: None,
                    pr_end: None,
                },
                r#"{"outcome":"pr-open","total_ms":0,"gate_attempts":0,"review_cycles":null,"pr_url":null,"pr_end":null}"#.into(),
            ),
        ]
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use foundry_proto::factory::{PrEnd, RunEvent, RunId};
    use foundry_proto::ids::MachineId;
    use serde_json::{Value, json};

    use super::events::{Cause, CauseStep, Event, FIELDS, Outcome};
    use super::testing::*;
    use super::*;

    fn cause_read() -> Option<Value> {
        Some(json!({"step": "pass", "reason": "claimed"}))
    }

    /// An event as the ledger stores it for run 8 at generation 1 with the test cause.
    fn stored(at_ms: u64, event: &str, data: Value) -> RunEvent {
        RunEvent {
            run: run(8),
            generation: 1,
            ts: format_ts(at_ms),
            ms: at_ms,
            event: event.into(),
            data,
            cause: cause_read(),
        }
    }

    fn finished_data(outcome: &str, total_ms: u64, gate_attempts: u32) -> Value {
        json!({
            "outcome": outcome,
            "total_ms": total_ms,
            "gate_attempts": gate_attempts,
            "review_cycles": null,
            "pr_url": null,
            "pr_end": null,
        })
    }

    fn last(store: &std::sync::Arc<std::sync::Mutex<crate::store::Store>>) -> RunEvent {
        held(store, &run(8)).pop().unwrap()
    }

    // The runner's `time.test.ts`.

    #[test]
    fn format_ts_epoch() {
        assert_eq!(format_ts(0), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn format_ts_known_instants() {
        assert_eq!(format_ts(1_791_108_796_123), "2026-10-04T10:13:16.123Z");
        assert_eq!(format_ts(946_684_799_999), "1999-12-31T23:59:59.999Z");
        assert_eq!(format_ts(4_107_542_401_005), "2100-03-01T00:00:01.005Z");
    }

    #[test]
    fn format_ts_leap_days() {
        assert_eq!(format_ts(1_709_210_096_789), "2024-02-29T12:34:56.789Z");
        assert_eq!(format_ts(951_782_400_000), "2000-02-29T00:00:00.000Z");
    }

    #[test]
    fn parse_ts_is_the_inverse_of_format_ts() {
        for ms in [
            0,
            1_791_108_796_123,
            946_684_799_999,
            4_107_542_401_005,
            1_709_210_096_789,
            951_782_400_000,
        ] {
            assert_eq!(parse_ts(&format_ts(ms)), Some(ms));
        }
    }

    #[test]
    fn parse_ts_takes_no_fraction_or_a_short_one() {
        assert_eq!(parse_ts("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_ts("2026-10-04T10:13:16Z"), Some(1_791_108_796_000));
        assert_eq!(parse_ts("2026-10-04T10:13:16.1Z"), Some(1_791_108_796_100));
        assert_eq!(parse_ts("2026-10-04T10:13:16.12Z"), Some(1_791_108_796_120));
        // A leap day: 2000 is a leap year, being divisible by 400.
        assert_eq!(format_ts(951_782_400_000), "2000-02-29T00:00:00.000Z");
        assert_eq!(parse_ts("2000-02-29T00:00:00Z"), Some(951_782_400_000));
    }

    #[test]
    fn parse_ts_refuses_what_format_ts_never_writes() {
        for text in [
            "",
            "garbage",
            "2026-10-04T10:13:16+10:00",
            "2026-10-04T10:13:16.123+00:00",
            "2026-10-04T10:13:16",
            "2026-10-04 10:13:16Z",
            "2026-10-04T10:13:16.Z",
            "2026-10-04T10:13:16.1234Z",
            "2026-13-04T10:13:16Z",
            "2026-10-32T10:13:16Z",
            "2026-10-04T24:13:16Z",
            "2026-10-04T10:60:16Z",
            "2026-10-04T10:13:16Z\n",
            // 2100 is a century year not divisible by 400, and 2026 is a common year.
            "2100-02-29T00:00:00Z",
            "2026-02-29T00:00:00Z",
            // Not ASCII digits, or a sign where a digit belongs.
            "2026-10-04T10:13:1٦Z",
            "+026-10-04T10:13:16Z",
            // Before the epoch is not a Unix millisecond count.
            "1969-12-31T23:59:59Z",
        ] {
            assert_eq!(parse_ts(text), None, "{text:?}");
        }
    }

    #[test]
    fn duration_says_seconds_under_a_minute() {
        assert_eq!(duration(0), "0s");
        assert_eq!(duration(999), "0s");
        assert_eq!(duration(59_999), "59s");
    }

    #[test]
    fn duration_says_whole_minutes_under_an_hour() {
        assert_eq!(duration(60_000), "1m");
        assert_eq!(duration(3_599_999), "59m");
    }

    #[test]
    fn duration_says_hours_and_two_digit_minutes_under_a_day() {
        assert_eq!(duration(3_600_000), "1h00m");
        assert_eq!(duration(2 * 3_600_000), "2h00m");
        assert_eq!(duration(2 * 3_600_000 + 5 * 60_000), "2h05m");
    }

    #[test]
    fn duration_says_days_and_two_digit_hours_from_a_day_on() {
        assert_eq!(duration(86_400_000), "1d00h");
        assert_eq!(duration(90_000_000), "1d01h");
    }

    // The runner's `ledger.test.ts`, against the store. The cases about the run file itself
    // (its modes, syncs, symlinks, cut-off lines and failed writes) are the store's now.

    #[test]
    fn create_now_stamps_events_with_the_system_clock() {
        let (_dir, store) = open_store();
        let ledger = Ledger::new(store.clone(), MachineId::random(), 1, shown());
        let before = system_ms();
        let written = ledger.write(&run(8), &step("red"), &test_cause()).unwrap();
        let after = system_ms();
        assert!(
            before <= written.ms && written.ms <= after,
            "{before} <= {} <= {after}",
            written.ms
        );
        assert_eq!(written.ts, format_ts(written.ms));
    }

    #[test]
    fn append_writes_one_exact_line_per_event_type() {
        let (_dir, store) = open_store();
        let ledger = ledger(&store, MachineId::random(), 1, ticking(T0, 0));
        let mut seen = BTreeSet::new();
        for (i, (event, data)) in samples().into_iter().enumerate() {
            let id = RunId::new("rawphp/softfac", 8, u32::try_from(i).unwrap() + 1).unwrap();
            let written = ledger.write(&id, &event, &test_cause()).unwrap();
            let name = written.event.clone();
            let expected = RunEvent {
                run: id.clone(),
                generation: 1,
                ts: "2026-10-04T10:13:16.123Z".into(),
                ms: T0,
                event: name.clone(),
                data: serde_json::from_str(&data).unwrap(),
                cause: cause_read(),
            };
            assert_eq!(written, expected, "{name}");
            assert_eq!(held(&store, &id), vec![expected], "{name}");
            // The store keeps `data` as an object whose keys are sorted; the runner's line, in
            // its key order, is made from it again.
            let line = format!(
                r#"{{"ts":"2026-10-04T10:13:16.123Z","ms":1791108796123,"event":"{name}","data":{data},"cause":{{"step":"pass","reason":"claimed"}}}}"#
            );
            assert_eq!(read::runner_line(&held(&store, &id)[0]), line, "{name}");
            seen.insert(name);
        }
        // Every one of the 33 event types has at least one exact line.
        let all: BTreeSet<String> = FIELDS.iter().map(|(name, _)| (*name).to_owned()).collect();
        assert_eq!(seen, all);
        assert_eq!(seen.len(), 33);
    }

    #[test]
    fn append_adds_lines_without_changing_earlier_bytes() {
        let (_dir, store) = open_store();
        let ledger = ledger(&store, MachineId::random(), 1, ticking(T0, 1000));
        let steps = [
            (
                Event::Claimed {
                    machine: "m".into(),
                    github_user: "u".into(),
                },
                stored(T0, "claimed", json!({"machine": "m", "github_user": "u"})),
            ),
            (
                step("red"),
                stored(T0 + 1000, "step_changed", json!({"step": "red"})),
            ),
            (
                stuck("r"),
                stored(T0 + 2000, "stuck", json!({"reason": "r"})),
            ),
        ];
        let mut expected = Vec::new();
        for (event, line) in steps {
            ledger.write(&run(8), &event, &test_cause()).unwrap();
            expected.push(line);
            assert_eq!(held(&store, &run(8)), expected);
        }
    }

    #[test]
    fn append_cleans_the_cause_actor_command_attempt_and_label_come_in_that_order_and_only_when_set()
     {
        let (_dir, store) = open_store();
        let ledger = ledger(&store, MachineId::random(), 1, ticking(T0, 0));
        let causes = [
            (
                Cause::new(CauseStep::Start, "approved"),
                r#"{"step":"start","reason":"approved"}"#,
            ),
            (
                Cause::new(CauseStep::Gate, "gate-command-ran")
                    .label("factory:ready")
                    .attempt(1)
                    .command("cargo test")
                    .actor("alice"),
                r#"{"step":"gate","reason":"gate-command-ran","actor":"alice","command":"cargo test","attempt":1,"label":"factory:ready"}"#,
            ),
            (
                Cause::new(CauseStep::Gate, "gate-regated").attempt(0),
                r#"{"step":"gate","reason":"gate-regated","attempt":0}"#,
            ),
            (
                Cause::new(CauseStep::Start, "new-attempt-started").label("factory:ready"),
                r#"{"step":"start","reason":"new-attempt-started","label":"factory:ready"}"#,
            ),
        ];
        for (cause, line) in &causes {
            // In the runner's key order, and only the keys that are set.
            assert_eq!(serde_json::to_string(&cause.cleaned()).unwrap(), *line);
            ledger.write(&run(8), &step("s"), cause).unwrap();
        }
        let read = held(&store, &run(8));
        for (event, (_, line)) in read.iter().zip(&causes) {
            assert!(
                read::runner_line(event).ends_with(&format!(r#","cause":{line}}}"#)),
                "{line}"
            );
        }
        let read: Vec<Option<Value>> = read.into_iter().map(|event| event.cause).collect();
        let expected: Vec<Option<Value>> = causes
            .iter()
            .map(|(_, line)| Some(serde_json::from_str(line).unwrap()))
            .collect();
        assert_eq!(read, expected);
    }

    #[test]
    fn append_cleans_a_hand_built_cause_on_the_way_to_the_file() {
        let (_dir, store) = open_store();
        let ledger = ledger(&store, MachineId::random(), 1, ticking(T0, 1000));
        let cause = Cause {
            step: CauseStep::Start,
            reason: "approved".into(),
            actor: Some(format!("al\u{202e}i{}", "x".repeat(150))),
            command: Some(format!("a\nb{}", "c".repeat(1500))),
            attempt: None,
            label: Some("la\u{0007}bel\u{2066}".into()),
        };
        let written = ledger.write(&run(8), &step("s"), &cause).unwrap();
        assert_eq!(
            written.cause,
            Some(json!({
                "step": "start",
                "reason": "approved",
                "actor": format!("ali{}", "x".repeat(97)),
                "command": format!("ab{}", "c".repeat(998)),
                "label": "label",
            }))
        );
    }

    #[test]
    fn open_rebuilds_start_time_and_gate_attempts_from_the_file() {
        let (_dir, store) = open_store();
        let me = MachineId::random();
        let first = ledger(&store, me, 1, ticking(T0, 1000));
        first.write(&run(8), &step("red"), &test_cause()).unwrap();
        first
            .write(&run(8), &gate(1, "cargo test"), &test_cause())
            .unwrap();
        drop(first);

        let second = ledger(&store, me, 1, ticking(T0 + 60_000, 1000));
        second
            .write(&run(8), &gate(2, "cargo test"), &test_cause())
            .unwrap();
        second
            .finish(&run(8), Outcome::PrOpen, None, None, &test_cause(), None)
            .unwrap();
        let end = last(&store);
        assert_eq!(end.ts, "2026-10-04T10:14:17.123Z");
        assert_eq!(end.data, finished_data("pr-open", 61_000, 2));
    }

    #[test]
    fn open_a_finished_run_then_append_is_an_error_and_writes_nothing() {
        let (_dir, store) = open_store();
        let me = MachineId::random();
        let first = ledger(&store, me, 1, ticking(T0, 1000));
        first
            .finish(&run(8), Outcome::Stuck, None, None, &test_cause(), None)
            .unwrap();
        let before = held(&store, &run(8));

        let second = ledger(&store, me, 1, ticking(T0, 1000));
        let error = second
            .write(&run(8), &stuck("r"), &test_cause())
            .unwrap_err();
        assert!(
            matches!(error, LedgerError::AlreadyFinished { .. }),
            "{error}"
        );
        assert_eq!(held(&store, &run(8)), before);
    }

    #[test]
    fn a_finished_line_this_code_cannot_read_still_ends_the_run_after_open() {
        let (_dir, store) = open_store();
        let newer = MachineId::random();
        put(
            &store,
            newer,
            1,
            &run(8),
            1,
            T0,
            "step_changed",
            json!({"step": "red"}),
            None,
        );
        put(
            &store,
            newer,
            2,
            &run(8),
            1,
            T0 + 1000,
            "finished",
            json!({"outcome": "merged", "total_ms": "soon"}),
            None,
        );
        let before = held(&store, &run(8));
        let ledger = ledger(&store, MachineId::random(), 1, ticking(T0, 1000));
        assert!(matches!(
            ledger.write(&run(8), &step("green"), &test_cause()),
            Err(LedgerError::AlreadyFinished { .. })
        ));
        assert!(matches!(
            ledger.finish(&run(8), Outcome::Stuck, None, None, &test_cause(), None),
            Err(LedgerError::AlreadyFinished { .. })
        ));
        assert_eq!(held(&store, &run(8)), before);
    }

    #[test]
    fn a_gate_command_line_that_does_not_parse_is_not_counted() {
        let (_dir, store) = open_store();
        let other = MachineId::random();
        for version in [1, 2] {
            put(
                &store,
                other,
                version,
                &run(8),
                1,
                T0,
                "gate_command",
                json!({"attempt": "two", "command": "c"}),
                None,
            );
        }
        // The earlier holder wrote at generation 1; within a generation only one machine writes.
        let ledger = ledger(&store, MachineId::random(), 2, ticking(T0 + 5000, 1000));
        ledger
            .write(&run(8), &gate(1, "cargo test"), &test_cause())
            .unwrap();
        ledger
            .finish(&run(8), Outcome::Stuck, None, None, &test_cause(), None)
            .unwrap();
        let end = last(&store);
        assert_eq!(end.ts, "2026-10-04T10:13:22.123Z");
        assert_eq!(end.data, finished_data("stuck", 6000, 1));
    }

    #[test]
    fn a_gate_command_line_counts_only_when_every_field_parses() {
        let (_dir, store) = open_store();
        let other = MachineId::random();
        let lines = [
            // No exit_code.
            json!({"attempt": 2, "command": "c", "duration_ms": 5}),
            // A string duration_ms.
            json!({"attempt": 3, "command": "c", "exit_code": 0, "duration_ms": "5"}),
            // A non-integer exit_code.
            json!({"attempt": 4, "command": "c", "exit_code": 1.5, "duration_ms": 5}),
            // A negative duration_ms is not a u64.
            json!({"attempt": 5, "command": "c", "exit_code": 0, "duration_ms": -1}),
            // An exit_code outside i32.
            json!({"attempt": 6, "command": "c", "exit_code": 2_147_483_648_u64, "duration_ms": 5}),
            // A missing command.
            json!({"attempt": 7, "exit_code": 0, "duration_ms": 5}),
            // An attempt below or above u32.
            json!({"attempt": -1, "command": "c", "exit_code": 0, "duration_ms": 5}),
            json!({"attempt": 4_294_967_296_u64, "command": "c", "exit_code": 0, "duration_ms": 5}),
        ];
        for (version, data) in (1..).zip(lines) {
            put(
                &store,
                other,
                version,
                &run(8),
                1,
                T0,
                "gate_command",
                data,
                None,
            );
        }
        // The earlier holder wrote at generation 1; within a generation only one machine writes.
        let ledger = ledger(&store, MachineId::random(), 2, ticking(T0 + 5000, 1000));
        ledger
            .write(&run(8), &gate(1, "cargo test"), &test_cause())
            .unwrap();
        ledger
            .finish(&run(8), Outcome::Stuck, None, None, &test_cause(), None)
            .unwrap();
        assert_eq!(last(&store).data["gate_attempts"], json!(1));
    }

    #[test]
    fn a_handle_refuses_to_append_after_another_handle_finished_the_run() {
        let (_a_dir, a_store) = open_store();
        let (_b_dir, b_store) = open_store();
        let (a, b) = (MachineId::random(), MachineId::random());
        let first = ledger(&a_store, a, 1, ticking(T0, 1000));
        let second = ledger(&b_store, b, 2, ticking(T0, 1000));
        first
            .finish(&run(8), Outcome::Stuck, None, None, &test_cause(), None)
            .unwrap();
        sync(&a_store, a, &b_store);
        let before = held(&b_store, &run(8));
        assert!(matches!(
            second.write(&run(8), &step("green"), &test_cause()),
            Err(LedgerError::AlreadyFinished { .. })
        ));
        assert!(matches!(
            second.finish(&run(8), Outcome::Stuck, None, None, &test_cause(), None),
            Err(LedgerError::AlreadyFinished { .. })
        ));
        assert_eq!(held(&b_store, &run(8)), before);
    }

    #[test]
    fn a_handles_finish_counts_the_lines_another_handle_wrote_after_it_opened() {
        let (_dir, store) = open_store();
        let me = MachineId::random();
        let first = ledger(&store, me, 1, scripted(&[T0, T0 + 1000]));
        let second = ledger(&store, me, 1, scripted(&[T0 + 2000, T0 + 9000]));
        first.write(&run(8), &step("red"), &test_cause()).unwrap();
        first
            .write(&run(8), &gate(1, "cargo test"), &test_cause())
            .unwrap();
        second
            .write(&run(8), &gate(2, "cargo test"), &test_cause())
            .unwrap();
        second
            .finish(&run(8), Outcome::Stuck, None, None, &test_cause(), None)
            .unwrap();
        assert_eq!(last(&store).data, finished_data("stuck", 9000, 2));
    }

    #[test]
    fn a_created_ledger_and_a_reopened_one_interleave_appends_without_overwriting() {
        let (_dir, store) = open_store();
        let me = MachineId::random();
        let first = ledger(&store, me, 1, ticking(T0, 10_000));
        let second = ledger(&store, me, 1, ticking(T0 + 1000, 10_000));
        first.write(&run(8), &step("red"), &test_cause()).unwrap();
        second
            .write(&run(8), &step("green"), &test_cause())
            .unwrap();
        first
            .write(&run(8), &step("refactor"), &test_cause())
            .unwrap();
        second.write(&run(8), &step("ship"), &test_cause()).unwrap();
        assert_eq!(
            held(&store, &run(8)),
            vec![
                stored(T0, "step_changed", json!({"step": "red"})),
                stored(T0 + 1000, "step_changed", json!({"step": "green"})),
                stored(T0 + 10_000, "step_changed", json!({"step": "refactor"})),
                stored(T0 + 11_000, "step_changed", json!({"step": "ship"})),
            ]
        );
    }

    #[test]
    fn finish_writes_the_summary_line_with_total_time_and_zero_gate_attempts() {
        let (_dir, store) = open_store();
        let ledger = ledger(&store, MachineId::random(), 1, scripted(&[T0, T0 + 90_000]));
        ledger
            .write(
                &run(8),
                &Event::Claimed {
                    machine: "m".into(),
                    github_user: "u".into(),
                },
                &test_cause(),
            )
            .unwrap();
        ledger
            .finish(
                &run(8),
                Outcome::PrOpen,
                Some(2),
                Some("https://github.com/rawphp/softfac/pull/12"),
                &test_cause(),
                None,
            )
            .unwrap();
        assert_eq!(
            held(&store, &run(8)),
            vec![
                stored(T0, "claimed", json!({"machine": "m", "github_user": "u"})),
                stored(
                    T0 + 90_000,
                    "finished",
                    json!({
                        "outcome": "pr-open",
                        "total_ms": 90_000,
                        "gate_attempts": 0,
                        "review_cycles": 2,
                        "pr_url": "https://github.com/rawphp/softfac/pull/12",
                        "pr_end": null,
                    })
                ),
            ]
        );
        assert_eq!(held(&store, &run(8))[1].ts, "2026-10-04T10:14:46.123Z");
    }

    #[test]
    fn finish_counts_commands_in_one_attempt_as_one_attempt() {
        let (_dir, store) = open_store();
        let ledger = ledger(&store, MachineId::random(), 1, ticking(T0, 1000));
        for command in ["cargo fmt --check", "cargo clippy", "cargo test"] {
            ledger
                .write(&run(8), &gate(1, command), &test_cause())
                .unwrap();
        }
        ledger
            .finish(&run(8), Outcome::PrOpen, None, None, &test_cause(), None)
            .unwrap();
        assert_eq!(
            last(&store),
            stored(T0 + 3000, "finished", finished_data("pr-open", 3000, 1))
        );
    }

    #[test]
    fn finish_counts_distinct_gate_attempts() {
        let (_dir, store) = open_store();
        let ledger = ledger(&store, MachineId::random(), 1, ticking(T0, 1000));
        for (attempt, command) in [
            (1, "cargo fmt --check"),
            (1, "cargo test"),
            (2, "cargo fmt --check"),
            (2, "cargo test"),
            (3, "cargo test"),
        ] {
            ledger
                .write(&run(8), &gate(attempt, command), &test_cause())
                .unwrap();
        }
        ledger
            .finish(&run(8), Outcome::Stuck, None, None, &test_cause(), None)
            .unwrap();
        assert_eq!(
            last(&store),
            stored(T0 + 5000, "finished", finished_data("stuck", 5000, 3))
        );
    }

    #[test]
    fn a_gate_checkout_line_is_not_a_gate_attempt_by_itself() {
        let (_dir, store) = open_store();
        let ledger = ledger(&store, MachineId::random(), 1, ticking(T0, 1000));
        ledger
            .write(
                &run(8),
                &Event::GateCheckout {
                    attempt: 1,
                    dir: "/d".into(),
                    sha: "a".repeat(40),
                    base_sha: "b".repeat(40),
                },
                &test_cause(),
            )
            .unwrap();
        ledger
            .write(
                &run(8),
                &Event::GateCleanupFailed {
                    dir: "/d".into(),
                    error: "busy".into(),
                },
                &test_cause(),
            )
            .unwrap();
        ledger
            .finish(&run(8), Outcome::Stuck, None, None, &test_cause(), None)
            .unwrap();
        assert_eq!(
            last(&store),
            stored(T0 + 2000, "finished", finished_data("stuck", 2000, 0))
        );
    }

    #[test]
    fn a_gate_sent_back_line_is_not_a_gate_attempt_by_itself() {
        let (_dir, store) = open_store();
        let ledger = ledger(&store, MachineId::random(), 1, ticking(T0, 1000));
        ledger
            .write(&run(8), &gate(1, "cargo test"), &test_cause())
            .unwrap();
        for attempt in [1, 2, 3] {
            ledger
                .write(
                    &run(8),
                    &Event::GateSentBack {
                        attempt,
                        run_ordinal: 1,
                    },
                    &test_cause(),
                )
                .unwrap();
        }
        ledger
            .finish(&run(8), Outcome::Stuck, None, None, &test_cause(), None)
            .unwrap();
        assert_eq!(
            last(&store),
            stored(T0 + 4000, "finished", finished_data("stuck", 4000, 1))
        );
    }

    #[test]
    fn finish_counts_distinct_attempt_numbers_not_the_highest() {
        let (_dir, store) = open_store();
        let ledger = ledger(&store, MachineId::random(), 1, ticking(T0, 1000));
        ledger
            .write(&run(8), &gate(1, "cargo test"), &test_cause())
            .unwrap();
        ledger
            .write(&run(8), &gate(3, "cargo test"), &test_cause())
            .unwrap();
        ledger
            .finish(
                &run(8),
                Outcome::NeedsInfo,
                Some(0),
                None,
                &test_cause(),
                None,
            )
            .unwrap();
        assert_eq!(
            last(&store),
            stored(
                T0 + 2000,
                "finished",
                json!({
                    "outcome": "needs-info",
                    "total_ms": 2000,
                    "gate_attempts": 2,
                    "review_cycles": 0,
                    "pr_url": null,
                    "pr_end": null,
                })
            )
        );
    }

    #[test]
    fn finish_as_the_only_line_has_zero_total_time() {
        let (_dir, store) = open_store();
        let ledger = ledger(&store, MachineId::random(), 1, ticking(T0, 1000));
        ledger
            .finish(&run(8), Outcome::Stuck, None, None, &test_cause(), None)
            .unwrap();
        assert_eq!(
            held(&store, &run(8)),
            vec![stored(T0, "finished", finished_data("stuck", 0, 0))]
        );
    }

    #[test]
    fn finish_total_time_never_goes_negative_if_the_clock_steps_back() {
        let (_dir, store) = open_store();
        let ledger = ledger(&store, MachineId::random(), 1, scripted(&[T0, T0 - 5000]));
        ledger.write(&run(8), &step("red"), &test_cause()).unwrap();
        ledger
            .finish(&run(8), Outcome::Stuck, None, None, &test_cause(), None)
            .unwrap();
        assert_eq!(last(&store).data["total_ms"], json!(0));
    }

    #[test]
    fn append_computes_the_summary_even_if_the_caller_filled_it_in() {
        let (_dir, store) = open_store();
        let ledger = ledger(&store, MachineId::random(), 1, ticking(T0, 1000));
        ledger
            .write(&run(8), &gate(1, "cargo test"), &test_cause())
            .unwrap();
        ledger
            .write(
                &run(8),
                &Event::Finished {
                    outcome: Outcome::Stuck,
                    total_ms: 999_999,
                    gate_attempts: 99,
                    review_cycles: None,
                    pr_url: None,
                    pr_end: None,
                },
                &test_cause(),
            )
            .unwrap();
        assert_eq!(
            last(&store),
            stored(T0 + 1000, "finished", finished_data("stuck", 1000, 1))
        );
    }

    #[test]
    fn finished_reads_back_as_a_finished_event() {
        let (_dir, store) = open_store();
        let ledger = ledger(&store, MachineId::random(), 1, ticking(T0, 1000));
        ledger
            .write(&run(8), &gate(1, "cargo test"), &test_cause())
            .unwrap();
        ledger
            .finish(
                &run(8),
                Outcome::PrOpen,
                Some(1),
                Some("https://x/pull/1"),
                &test_cause(),
                None,
            )
            .unwrap();
        let read = ledger.read(&run(8)).unwrap();
        assert_eq!(
            read[1],
            stored(
                T0 + 1000,
                "finished",
                json!({
                    "outcome": "pr-open",
                    "total_ms": 1000,
                    "gate_attempts": 1,
                    "review_cycles": 1,
                    "pr_url": "https://x/pull/1",
                    "pr_end": null,
                })
            )
        );
        assert_eq!(
            serde_json::from_value::<Event>(json!({"event": read[1].event, "data": read[1].data}))
                .unwrap(),
            Event::Finished {
                outcome: Outcome::PrOpen,
                total_ms: 1000,
                gate_attempts: 1,
                review_cycles: Some(1),
                pr_url: Some("https://x/pull/1".into()),
                pr_end: None,
            }
        );
    }

    #[test]
    fn append_after_finished_is_an_error_and_writes_nothing() {
        let (_dir, store) = open_store();
        let ledger = ledger(&store, MachineId::random(), 1, ticking(T0, 1000));
        ledger
            .finish(&run(8), Outcome::Stuck, None, None, &test_cause(), None)
            .unwrap();
        let before = held(&store, &run(8));
        let errors = [
            ledger
                .write(&run(8), &stuck("r"), &test_cause())
                .unwrap_err(),
            ledger
                .finish(&run(8), Outcome::Stuck, None, None, &test_cause(), None)
                .unwrap_err(),
        ];
        for error in errors {
            match &error {
                LedgerError::AlreadyFinished { run: id } => assert_eq!(*id, run(8)),
                other => panic!("{other}"),
            }
            assert_eq!(
                error.to_string(),
                "run rawphp__softfac__8__1 is already finished"
            );
        }
        assert_eq!(held(&store, &run(8)), before);
    }

    #[test]
    fn a_finish_with_a_pr_end_writes_it_last_and_reads_back() {
        for pr_end in [PrEnd::Merged, PrEnd::Closed] {
            let (_dir, store) = open_store();
            let ledger = ledger(&store, MachineId::random(), 1, ticking(T0, 1000));
            ledger
                .finish(
                    &run(8),
                    Outcome::PrOpen,
                    None,
                    Some("https://x/pull/1"),
                    &test_cause(),
                    Some(pr_end),
                )
                .unwrap();
            let name = serde_json::to_value(pr_end).unwrap();
            assert_eq!(
                ledger.read(&run(8)).unwrap()[0].data,
                json!({
                    "outcome": "pr-open",
                    "total_ms": 0,
                    "gate_attempts": 0,
                    "review_cycles": null,
                    "pr_url": "https://x/pull/1",
                    "pr_end": name,
                })
            );
            // Written last, as the runner writes it.
            let event = Event::Finished {
                outcome: Outcome::PrOpen,
                total_ms: 0,
                gate_attempts: 0,
                review_cycles: None,
                pr_url: Some("https://x/pull/1".into()),
                pr_end: Some(pr_end),
            };
            assert!(
                serde_json::to_string(&event).unwrap().ends_with(&format!(
                    r#""pr_url":"https://x/pull/1","pr_end":{name}}}}}"#
                )),
                "{pr_end:?}"
            );
        }
    }

    #[test]
    fn a_plain_finish_writes_a_null_pr_end() {
        let (_dir, store) = open_store();
        let ledger = ledger(&store, MachineId::random(), 1, ticking(T0, 1000));
        ledger
            .finish(&run(8), Outcome::Stuck, None, None, &test_cause(), None)
            .unwrap();
        assert_eq!(last(&store).data["pr_end"], Value::Null);
        assert_eq!(last(&store).data["pr_url"], Value::Null);
    }

    #[test]
    fn append_keeps_the_pr_end_when_it_fills_in_the_summary() {
        let (_dir, store) = open_store();
        let ledger = ledger(&store, MachineId::random(), 1, ticking(T0, 1000));
        ledger
            .write(
                &run(8),
                &Event::Finished {
                    outcome: Outcome::PrOpen,
                    total_ms: 0,
                    gate_attempts: 0,
                    review_cycles: None,
                    pr_url: None,
                    pr_end: Some(PrEnd::Closed),
                },
                &test_cause(),
            )
            .unwrap();
        assert_eq!(last(&store).data["pr_end"], json!("closed"));
    }

    // What the port adds: the lease generation, two writers, known secrets and the 64 KB cut.

    #[test]
    fn every_write_is_stamped_with_the_lease_generation_the_ledger_is_given() {
        let (_dir, store) = open_store();
        let me = MachineId::random();
        let at_3 = ledger(&store, me, 3, ticking(T0, 1000));
        assert_eq!(at_3.generation(), 3);
        at_3.write(&run(8), &step("red"), &test_cause()).unwrap();
        let at_4 = at_3.at(4);
        assert_eq!(at_4.generation(), 4);
        at_4.write(&run(8), &step("green"), &test_cause()).unwrap();
        let generations: Vec<u64> = held(&store, &run(8))
            .iter()
            .map(|event| event.generation)
            .collect();
        assert_eq!(generations, [3, 4]);
    }

    #[test]
    fn a_write_is_a_factory_record_of_this_machine_under_its_next_factory_version() {
        let (_dir, store) = open_store();
        let me = MachineId::random();
        let ledger = ledger(&store, me, 1, ticking(T0, 1000));
        ledger.write(&run(8), &step("red"), &test_cause()).unwrap();
        ledger
            .write(&run(9), &step("green"), &test_cause())
            .unwrap();
        let records = store
            .lock()
            .unwrap()
            .factory_since(me, 0, usize::MAX)
            .unwrap();
        let versions: Vec<u64> = records.iter().map(|record| record.version).collect();
        assert_eq!(versions, [1, 2]);
        assert!(records.iter().all(|record| record.owner == me));
    }

    #[test]
    fn events_written_by_two_machines_at_generations_1_and_2_read_in_that_order_with_the_second_machines_clock_an_hour_behind()
     {
        let (_a_dir, a_store) = open_store();
        let (_b_dir, b_store) = open_store();
        let (a, b) = (MachineId::random(), MachineId::random());
        // The first holder writes a few events, so its versions are above the second's.
        let first = ledger(&a_store, a, 1, ticking(T0, 1000));
        for name in ["a1", "a2", "a3"] {
            first.write(&run(8), &step(name), &test_cause()).unwrap();
        }
        sync(&a_store, a, &b_store);
        // The lease moves to a machine whose clock is an hour behind.
        let second = ledger(&b_store, b, 2, ticking(T0 - 3_600_000, 1000));
        for name in ["b1", "b2"] {
            second.write(&run(8), &step(name), &test_cause()).unwrap();
        }
        sync(&b_store, b, &a_store);
        for (store, reader) in [(&a_store, &first), (&b_store, &second)] {
            let steps: Vec<Value> = reader
                .read(&run(8))
                .unwrap()
                .into_iter()
                .map(|event| event.data["step"].clone())
                .collect();
            assert_eq!(
                steps,
                [
                    json!("a1"),
                    json!("a2"),
                    json!("a3"),
                    json!("b1"),
                    json!("b2")
                ]
            );
            assert_eq!(held(store, &run(8)).len(), 5);
        }
        // Its finish counts no negative time from the first holder's start.
        second
            .finish(&run(8), Outcome::Stuck, None, None, &test_cause(), None)
            .unwrap();
        assert_eq!(last(&b_store).data["total_ms"], json!(0));
    }

    #[test]
    fn an_events_output_over_64_kb_is_cut_to_its_tail() {
        let (_dir, store) = open_store();
        let ledger = ledger(&store, MachineId::random(), 1, ticking(T0, 1000));
        let output = format!("{}{}", "h".repeat(10_000), "t".repeat(OUTPUT_LIMIT));
        let written = ledger
            .write(&run(8), &stuck(&output), &test_cause())
            .unwrap();
        assert_eq!(written.data["reason"], json!("t".repeat(OUTPUT_LIMIT)));
        assert_eq!(
            held(&store, &run(8))[0].data["reason"],
            written.data["reason"]
        );
        // Output of exactly 64 KB is kept whole, and every text in the event is cut alike.
        let exact = "x".repeat(OUTPUT_LIMIT);
        let written = ledger
            .write(
                &run(8),
                &Event::FeedbackRelaunching {
                    n: 1,
                    branch: "b".into(),
                    items: vec![format!("{}{}", "h", exact)],
                    message: exact.clone(),
                },
                &test_cause(),
            )
            .unwrap();
        assert_eq!(written.data["message"], json!(exact));
        assert_eq!(written.data["items"], json!([exact]));
    }

    #[test]
    fn a_cut_never_splits_a_character() {
        // A three-byte character straddles the place a cut by bytes would make.
        let text = format!("€{}", "t".repeat(OUTPUT_LIMIT - 1));
        assert_eq!(text.len(), OUTPUT_LIMIT + 2);
        assert_eq!(cut_output(&text), "t".repeat(OUTPUT_LIMIT - 1));
        let text = "€".repeat(OUTPUT_LIMIT / 3 + 1);
        assert_eq!(text.len(), OUTPUT_LIMIT + 2);
        let cut = cut_output(&text);
        assert!(cut.len() <= OUTPUT_LIMIT);
        assert_eq!(cut, "€".repeat(OUTPUT_LIMIT / 3));
        assert_eq!(cut_output("short"), "short");
    }

    #[test]
    fn a_secret_across_the_cut_of_a_cause_text_is_hidden_before_the_text_is_cut() {
        let (_dir, store) = open_store();
        let token = "ghp_0123456789abcdefghij";
        let hide: Hide = std::sync::Arc::new(move |text: &str| text.replace(token, "***"));
        let ledger =
            Ledger::new(store.clone(), MachineId::random(), 1, hide).with_wall(ticking(T0, 1000));
        // Each text holds the secret across the place its cut falls: 100 characters for the
        // reason, actor and label, 1000 for the command.
        let short = |fill: &str| format!("{}{token}", fill.repeat(95));
        let cause = Cause::new(CauseStep::Gate, short("r"))
            .actor(&short("a"))
            .command(&format!("{}{token}", "c".repeat(990)))
            .label(&short("l"));
        let written = ledger.write(&run(8), &step("s"), &cause).unwrap();
        let stored = serde_json::to_string(&held(&store, &run(8))).unwrap();
        for text in [serde_json::to_string(&written).unwrap(), stored] {
            assert!(!text.contains("ghp_"), "{text}");
        }
        assert_eq!(
            written.cause,
            Some(json!({
                "step": "gate",
                "reason": format!("{}***", "r".repeat(95)),
                "actor": format!("{}***", "a".repeat(95)),
                "command": format!("{}***", "c".repeat(990)),
                "label": format!("{}***", "l".repeat(95)),
            }))
        );
        // A control character inside a secret cannot hide it from the hiding.
        let split = "ghp_0123456789\u{0007}abcdefghij";
        let written = ledger
            .write(
                &run(8),
                &step("t"),
                &Cause::new(CauseStep::Gate, "gate-command-ran").command(split),
            )
            .unwrap();
        assert_eq!(written.cause.unwrap()["command"], json!("***"));
    }

    #[test]
    fn a_secret_split_by_a_control_character_is_hidden_in_the_data_too() {
        let (_dir, store) = open_store();
        let token = "ghp_0123456789abcdefghij";
        let hide: Hide = std::sync::Arc::new(move |text: &str| text.replace(token, "***"));
        let ledger =
            Ledger::new(store.clone(), MachineId::random(), 1, hide).with_wall(ticking(T0, 1000));
        let written = ledger
            .write(
                &run(8),
                &stuck("setup failed:\nexport T=ghp_0123456789\u{0007}abcdefghij\ndone"),
                &test_cause(),
            )
            .unwrap();
        let stored = serde_json::to_string(&held(&store, &run(8))).unwrap();
        for text in [serde_json::to_string(&written).unwrap(), stored] {
            assert!(!text.contains("ghp_"), "{text}");
            assert!(!text.contains("abcdefghij"), "{text}");
        }
        // The same for DEL, a C1 control and the bidi controls, and the lines stay.
        for split in ['\u{007f}', '\u{0090}', '\u{202e}', '\u{2066}'] {
            let written = ledger
                .write(
                    &run(9),
                    &stuck(&format!("a\nT=ghp_0123456789{split}abcdefghij\nb")),
                    &test_cause(),
                )
                .unwrap();
            assert_eq!(written.data["reason"], json!("a\nT=***\nb"), "{split:?}");
        }
        // Output with no secret in it keeps its lines and control characters.
        let written = ledger
            .write(&run(8), &stuck("one\ntwo\u{0007}"), &test_cause())
            .unwrap();
        assert_eq!(written.data["reason"], json!("one\ntwo\u{0007}"));
    }

    #[test]
    fn output_whose_lines_only_join_into_a_url_password_keeps_its_lines() {
        // A Hide that hides URL passwords as `secrets::redact` does, beside a known token.
        let (_dir, store) = open_store();
        let hide: Hide = std::sync::Arc::new(|text: &str| {
            crate::secrets::redact(text).replace("ghp_0123456789abcdefghij", "***")
        });
        let ledger =
            Ledger::new(store.clone(), MachineId::random(), 1, hide).with_wall(ticking(T0, 1000));
        let output =
            "fetching https://registry.example.com\nadmin@example.com wrote\r\n\tok\u{0007}";
        let written = ledger
            .write(&run(8), &stuck(output), &test_cause())
            .unwrap();
        assert_eq!(written.data["reason"], json!(output));
        // A secret split by a control character that is not white space is still hidden, and
        // the text keeps its lines.
        let written = ledger
            .write(
                &run(8),
                &stuck("a\nT=ghp_0123456789\u{0007}abcdefghij\nb"),
                &test_cause(),
            )
            .unwrap();
        assert_eq!(written.data["reason"], json!("a\nT=***\nb"));
    }

    #[test]
    fn a_write_hides_its_texts_before_it_takes_the_store_lock() {
        // The daemon's Hide reads the secret store, which can be slow; the store lock is
        // shared by the whole daemon, so no Hide call may run while it is held.
        let (_dir, store) = open_store();
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let locked = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hide: Hide = {
            let (store, calls, locked) = (store.clone(), calls.clone(), locked.clone());
            std::sync::Arc::new(move |text: &str| {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if store.try_lock().is_err() {
                    locked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
                text.to_owned()
            })
        };
        let ledger =
            Ledger::new(store.clone(), MachineId::random(), 1, hide).with_wall(ticking(T0, 1000));
        let cause = Cause::new(CauseStep::Gate, "gate-command-ran")
            .actor("someone")
            .command("make check")
            .label("factory:building");
        ledger.write(&run(8), &stuck("output"), &cause).unwrap();
        ledger
            .finish(&run(8), Outcome::Stuck, None, None, &cause, None)
            .unwrap();
        assert!(calls.load(std::sync::atomic::Ordering::SeqCst) > 0);
        assert_eq!(locked.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn a_write_holds_the_store_lock_from_its_finished_check_to_its_store() {
        // The clock is read between the finished check and the store, so it sees the lock held
        // there. A write that let the lock go between the two could let another clone's
        // `finished` in between, and store an event after it.
        let (_dir, store) = open_store();
        let (reads, unlocked) = (
            std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        );
        let wall: WallClock = {
            let (store, reads, unlocked) = (store.clone(), reads.clone(), unlocked.clone());
            std::sync::Arc::new(move || {
                reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if store.try_lock().is_ok() {
                    unlocked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
                T0
            })
        };
        let ledger = Ledger::new(store.clone(), MachineId::random(), 1, shown()).with_wall(wall);
        ledger.write(&run(8), &step("s"), &test_cause()).unwrap();
        ledger
            .finish(&run(8), Outcome::Stuck, None, None, &test_cause(), None)
            .unwrap();
        assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(unlocked.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn the_ledger_lists_and_counts_the_runs_its_store_holds() {
        let (_dir, store) = open_store();
        let ledger = ledger(&store, MachineId::random(), 1, ticking(T0, 1000));
        let second = RunId::new("rawphp/softfac", 8, 2).unwrap();
        ledger.write(&run(8), &step("red"), &test_cause()).unwrap();
        ledger.write(&second, &step("red"), &test_cause()).unwrap();
        ledger.write(&run(9), &step("red"), &test_cause()).unwrap();
        ledger
            .finish(&run(9), Outcome::Stuck, None, None, &test_cause(), None)
            .unwrap();
        let ids = |runs: Vec<list::RunInfo>| -> Vec<RunId> {
            runs.into_iter().map(|run| run.id).collect()
        };
        assert_eq!(
            ids(ledger.runs("RawPHP/softfac").unwrap()),
            [run(8), second.clone()]
        );
        assert_eq!(
            ids(ledger.issue_runs("rawphp/softfac", 9).unwrap()),
            Vec::<RunId>::new()
        );
        assert_eq!(ledger.next_attempt("rawphp/softfac", 8).unwrap(), 3);
        let summaries = ledger.summaries("rawphp/softfac").unwrap();
        assert_eq!(summaries.len(), 3);
        assert!(summaries[2].finished);
    }

    #[test]
    fn known_secrets_are_hidden_in_the_data_and_the_cause_before_the_output_is_cut() {
        let (_dir, store) = open_store();
        let hide: Hide = std::sync::Arc::new(|text: &str| text.replace("s3cr3t-token", "***"));
        let ledger =
            Ledger::new(store.clone(), MachineId::random(), 1, hide).with_wall(ticking(T0, 1000));
        // The secret sits across the place the cut would make if it came before the hiding.
        let output = format!(
            "{}s3cr3t-token{}",
            "h".repeat(100),
            "t".repeat(OUTPUT_LIMIT - 6)
        );
        let written = ledger
            .write(
                &run(8),
                &Event::FeedbackForwarding {
                    n: 1,
                    run_ordinal: None,
                    items: vec!["review:1 s3cr3t-token".into()],
                    message: output,
                },
                &Cause::new(CauseStep::Gate, "gate-command-ran")
                    .command("curl -H 's3cr3t-token'")
                    .actor("s3cr3t-token"),
            )
            .unwrap();
        let text = serde_json::to_string(&written).unwrap();
        assert!(!text.contains("s3cr3t"), "{text}");
        assert!(
            !text.contains("token"),
            "a cut left part of the secret: {text}"
        );
        assert_eq!(written.data["items"], json!(["review:1 ***"]));
        let message = written.data["message"].as_str().unwrap();
        assert_eq!(message.len(), OUTPUT_LIMIT);
        assert!(message.ends_with(&"t".repeat(OUTPUT_LIMIT - 6)));
        assert_eq!(
            written.cause.as_ref().unwrap()["command"],
            json!("curl -H '***'")
        );
        assert_eq!(written.cause.as_ref().unwrap()["actor"], json!("***"));
        let stored = serde_json::to_string(&held(&store, &run(8))).unwrap();
        assert!(!stored.contains("s3cr3t"), "{stored}");
    }

    /// A run of 1,000 events another machine wrote at generation 1: a step, then gate commands
    /// of attempts 1 to 3 every tenth event, the first at `T0` and a second apart.
    fn a_thousand_events(
        store: &std::sync::Arc<std::sync::Mutex<crate::store::Store>>,
    ) -> MachineId {
        let other = MachineId::random();
        for version in 1..=1000_u64 {
            let ms = T0 + (version - 1) * 1000;
            let (name, data) = if version % 10 == 0 {
                let attempt = version / 10 % 3 + 1;
                (
                    "gate_command",
                    json!({"attempt": attempt, "command": "cargo test", "exit_code": 0, "duration_ms": 5}),
                )
            } else {
                ("step_changed", json!({"step": "red"}))
            };
            put(store, other, version, &run(8), 1, ms, name, data, None);
        }
        other
    }

    /// A run event row of `run` this build cannot read, as a newer build may leave one.
    fn put_unreadable(dir: &tempfile::TempDir, run: &RunId) {
        let owner = MachineId::random();
        let conn = rusqlite::Connection::open(dir.path().join("foundry.db")).unwrap();
        conn.execute(
            "INSERT INTO factory_records (kind, id, owner, version, record)
             VALUES ('run_event', ?1, ?2, 1, '{\"kind\":\"run_event\",\"later\":true}')",
            rusqlite::params![format!("{run}/{owner}/1"), owner.to_string()],
        )
        .unwrap();
    }

    #[test]
    fn the_write_of_event_1001_reads_one_writer_id_and_no_event() {
        let (_dir, store) = open_store();
        let ledger = ledger(&store, MachineId::random(), 1, ticking(T0, 1000));
        for _ in 0..1000 {
            ledger.write(&run(8), &step("red"), &test_cause()).unwrap();
        }
        let before = Store::run_rows_read();
        ledger
            .write(&run(8), &step("green"), &test_cause())
            .unwrap();
        // This machine's id, the run's only writer, and no event: nothing was stored since.
        assert_eq!(Store::run_rows_read() - before, 1);
        assert_eq!(held(&store, &run(8)).len(), 1001);
    }

    #[test]
    fn a_ledger_reads_a_runs_history_once_then_only_what_was_stored_since() {
        let (_dir, store) = open_store();
        let writer = a_thousand_events(&store);
        let ledger = ledger(
            &store,
            MachineId::random(),
            2,
            ticking(T0 + 2_000_000, 1000),
        );
        let rows = || Store::run_rows_read();

        // The first write reads the other writer's id and its 1,000 events.
        let before = rows();
        ledger
            .write(&run(8), &step("green"), &test_cause())
            .unwrap();
        assert_eq!(rows() - before, 1 + 1000);

        // The next reads the two writers' ids and nothing else.
        let before = rows();
        ledger
            .write(&run(8), &step("refactor"), &test_cause())
            .unwrap();
        assert_eq!(rows() - before, 2);

        // An event the other writer stores is the only event the write after it reads.
        put(
            &store,
            writer,
            1001,
            &run(8),
            1,
            T0,
            "gate_command",
            json!({"attempt": 7, "command": "c", "exit_code": 0, "duration_ms": 5}),
            None,
        );
        let before = rows();
        ledger
            .finish(&run(8), Outcome::Stuck, None, None, &test_cause(), None)
            .unwrap();
        assert_eq!(rows() - before, 2 + 1);
        // From the other writer's first event at T0, with attempts 1 to 3 and 7.
        assert_eq!(last(&store).data, finished_data("stuck", 2_002_000, 4));
    }

    #[test]
    fn a_finish_after_1000_events_reads_no_event_again() {
        let (_dir, store) = open_store();
        a_thousand_events(&store);
        let ledger = ledger(
            &store,
            MachineId::random(),
            2,
            ticking(T0 + 2_000_000, 1000),
        );
        ledger
            .write(&run(8), &step("green"), &test_cause())
            .unwrap();
        let before = Store::run_rows_read();
        ledger
            .finish(&run(8), Outcome::Stuck, None, None, &test_cause(), None)
            .unwrap();
        // The two writers' ids.
        assert_eq!(Store::run_rows_read() - before, 2);
        assert_eq!(last(&store).data, finished_data("stuck", 2_001_000, 3));
    }

    #[test]
    fn a_finished_event_synced_in_after_the_ledger_read_the_run_still_stops_its_next_write() {
        let (_a_dir, a_store) = open_store();
        let (_b_dir, b_store) = open_store();
        let (a, b) = (MachineId::random(), MachineId::random());
        let first = ledger(&a_store, a, 1, ticking(T0, 1000));
        let second = ledger(&b_store, b, 2, ticking(T0, 1000));
        first.write(&run(8), &step("a1"), &test_cause()).unwrap();
        sync(&a_store, a, &b_store);
        // The second machine reads the run, the first machine's event included.
        second.write(&run(8), &step("b1"), &test_cause()).unwrap();
        // The first machine writes more, then finishes, and its events sync in.
        first.write(&run(8), &step("a2"), &test_cause()).unwrap();
        first
            .finish(&run(8), Outcome::Stuck, None, None, &test_cause(), None)
            .unwrap();
        sync(&a_store, a, &b_store);
        let before = held(&b_store, &run(8));
        assert!(matches!(
            second.write(&run(8), &step("b2"), &test_cause()),
            Err(LedgerError::AlreadyFinished { .. })
        ));
        assert!(matches!(
            second.finish(&run(8), Outcome::Stuck, None, None, &test_cause(), None),
            Err(LedgerError::AlreadyFinished { .. })
        ));
        assert_eq!(held(&b_store, &run(8)), before);
    }

    #[test]
    fn a_clone_and_a_second_ledger_of_one_store_each_see_the_others_writes() {
        let (_dir, store) = open_store();
        let me = MachineId::random();
        let first = ledger(&store, me, 1, scripted(&[T0, T0 + 1000, T0 + 9000]));
        let clone = first.clone();
        let second = ledger(&store, me, 1, scripted(&[T0 + 2000, T0 + 3000]));
        first.write(&run(8), &step("red"), &test_cause()).unwrap();
        second
            .write(&run(8), &gate(1, "cargo test"), &test_cause())
            .unwrap();
        clone
            .write(&run(8), &gate(2, "cargo test"), &test_cause())
            .unwrap();
        second
            .finish(&run(8), Outcome::Stuck, None, None, &test_cause(), None)
            .unwrap();
        assert_eq!(last(&store).data, finished_data("stuck", 3000, 2));
        assert!(matches!(
            first.write(&run(8), &step("late"), &test_cause()),
            Err(LedgerError::AlreadyFinished { .. })
        ));
    }

    #[test]
    fn a_write_to_a_run_holding_a_row_this_build_cannot_read_fails_and_writes_nothing() {
        let me = MachineId::random();
        let mine = |store: &std::sync::Arc<std::sync::Mutex<crate::store::Store>>| {
            store
                .lock()
                .unwrap()
                .factory_since(me, 0, usize::MAX)
                .unwrap()
                .len()
        };
        // Held before the ledger first reads the run.
        let (dir, store) = open_store();
        put_unreadable(&dir, &run(8));
        let cold = ledger(&store, me, 1, ticking(T0, 1000));
        assert!(matches!(
            cold.write(&run(8), &step("red"), &test_cause()),
            Err(LedgerError::Store(_))
        ));
        assert!(matches!(
            cold.finish(&run(8), Outcome::Stuck, None, None, &test_cause(), None),
            Err(LedgerError::Store(_))
        ));
        assert_eq!(mine(&store), 0);

        // Stored after the ledger read the run.
        let (dir, store) = open_store();
        let warm = ledger(&store, me, 1, ticking(T0, 1000));
        warm.write(&run(9), &step("red"), &test_cause()).unwrap();
        put_unreadable(&dir, &run(9));
        assert!(matches!(
            warm.write(&run(9), &step("green"), &test_cause()),
            Err(LedgerError::Store(_))
        ));
        assert!(matches!(
            warm.finish(&run(9), Outcome::Stuck, None, None, &test_cause(), None),
            Err(LedgerError::Store(_))
        ));
        assert_eq!(mine(&store), 1);
    }

    /// What `ledger` has read of `run`, as (whether it is finished, the highest version read of
    /// each writer).
    fn tally(ledger: &Ledger, run: &RunId) -> (bool, std::collections::BTreeMap<MachineId, u64>) {
        let tallies = ledger.tallies.lock().unwrap();
        let tally = &tallies[run];
        (tally.finished, tally.read_to.clone())
    }

    #[test]
    fn a_writers_cursor_moves_to_its_highest_version_so_its_other_records_are_not_visited_again() {
        let (_dir, store) = open_store();
        let (me, other) = (MachineId::random(), MachineId::random());
        for version in 1..=3 {
            put(
                &store,
                other,
                version,
                &run(8),
                1,
                T0,
                "step_changed",
                json!({}),
                None,
            );
        }
        // The other writer carries on with another run.
        for version in 4..=1003 {
            put(
                &store,
                other,
                version,
                &run(9),
                1,
                T0,
                "step_changed",
                json!({}),
                None,
            );
        }
        let ledger = ledger(&store, me, 2, ticking(T0, 1000));
        ledger.write(&run(8), &step("red"), &test_cause()).unwrap();
        assert_eq!(tally(&ledger, &run(8)).1[&other], 1003);
        // And again once more is stored, read from above where it was last seen.
        put(
            &store,
            other,
            1004,
            &run(9),
            1,
            T0,
            "step_changed",
            json!({}),
            None,
        );
        ledger
            .write(&run(8), &step("green"), &test_cause())
            .unwrap();
        assert_eq!(tally(&ledger, &run(8)).1[&other], 1004);
    }

    #[test]
    fn a_finished_event_read_before_a_later_event_of_its_writer_still_ends_the_run() {
        let (_dir, store) = open_store();
        let other = MachineId::random();
        put(
            &store,
            other,
            1,
            &run(8),
            1,
            T0,
            "finished",
            json!({"outcome": "stuck"}),
            None,
        );
        put(
            &store,
            other,
            2,
            &run(8),
            1,
            T0,
            "step_changed",
            json!({"step": "late"}),
            None,
        );
        let cold = ledger(&store, MachineId::random(), 2, ticking(T0, 1000));
        let before = held(&store, &run(8));
        assert!(matches!(
            cold.write(&run(8), &step("red"), &test_cause()),
            Err(LedgerError::AlreadyFinished { .. })
        ));
        assert!(matches!(
            cold.finish(&run(8), Outcome::Stuck, None, None, &test_cause(), None),
            Err(LedgerError::AlreadyFinished { .. })
        ));
        assert_eq!(held(&store, &run(8)), before);

        // The same when the `finished` and the later event are stored after the ledger first
        // read the run, and read in one catch-up.
        let (_dir, store) = open_store();
        let warm = ledger(&store, MachineId::random(), 2, ticking(T0, 1000));
        warm.write(&run(9), &step("red"), &test_cause()).unwrap();
        put(
            &store,
            other,
            1,
            &run(9),
            1,
            T0,
            "finished",
            json!({"outcome": "stuck"}),
            None,
        );
        put(
            &store,
            other,
            2,
            &run(9),
            1,
            T0,
            "step_changed",
            json!({"step": "late"}),
            None,
        );
        let before = held(&store, &run(9));
        assert!(matches!(
            warm.write(&run(9), &step("green"), &test_cause()),
            Err(LedgerError::AlreadyFinished { .. })
        ));
        assert_eq!(held(&store, &run(9)), before);
    }

    #[test]
    fn the_first_event_is_the_lowest_by_generation_then_version_whatever_order_it_is_read_in() {
        let (_dir, store) = open_store();
        let other = MachineId::random();
        let ledger = ledger(
            &store,
            MachineId::random(),
            2,
            scripted(&[T0 + 5000, T0 + 9000]),
        );
        ledger.write(&run(8), &step("red"), &test_cause()).unwrap();
        // The earlier holder's events sync in after: generation 1, at versions above the
        // ledger's own, the first of them at T0.
        put(
            &store,
            other,
            50,
            &run(8),
            1,
            T0,
            "step_changed",
            json!({}),
            None,
        );
        put(
            &store,
            other,
            51,
            &run(8),
            1,
            T0 + 1000,
            "step_changed",
            json!({}),
            None,
        );
        ledger
            .finish(&run(8), Outcome::Stuck, None, None, &test_cause(), None)
            .unwrap();
        assert_eq!(last(&store).data, finished_data("stuck", 9000, 0));
    }

    #[test]
    fn a_finished_runs_tally_keeps_only_that_it_is_finished_and_reads_nothing_again() {
        let (_dir, store) = open_store();
        let ledger = ledger(&store, MachineId::random(), 1, ticking(T0, 1000));
        ledger
            .write(&run(8), &gate(1, "cargo test"), &test_cause())
            .unwrap();
        ledger
            .finish(&run(8), Outcome::Stuck, None, None, &test_cause(), None)
            .unwrap();
        let before = Store::run_rows_read();
        assert!(matches!(
            ledger.write(&run(8), &step("late"), &test_cause()),
            Err(LedgerError::AlreadyFinished { .. })
        ));
        assert_eq!(Store::run_rows_read() - before, 0);
        assert_eq!(tally(&ledger, &run(8)), (true, Default::default()));
        let tallies = ledger.tallies.lock().unwrap();
        assert!(tallies[&run(8)].first.is_none());
        assert!(tallies[&run(8)].gate_attempts.is_empty());
    }

    #[test]
    fn a_finished_run_holding_a_row_this_build_cannot_read_is_a_store_error_whatever_its_key_order()
    {
        let (low, high) = (
            MachineId(uuid::Uuid::from_u128(1)),
            MachineId(uuid::Uuid::from_u128(u128::MAX)),
        );
        for (finisher, damaged) in [(low, high), (high, low)] {
            let (dir, store) = open_store();
            put(
                &store,
                finisher,
                1,
                &run(8),
                1,
                T0,
                "finished",
                json!({"outcome": "stuck"}),
                None,
            );
            let conn = rusqlite::Connection::open(dir.path().join("foundry.db")).unwrap();
            conn.execute(
                "INSERT INTO factory_records (kind, id, owner, version, record)
                 VALUES ('run_event', ?1, ?2, 1, '{\"kind\":\"run_event\",\"later\":true}')",
                rusqlite::params![format!("{}/{damaged}/1", run(8)), damaged.to_string()],
            )
            .unwrap();
            let ledger = ledger(&store, MachineId::random(), 2, ticking(T0, 1000));
            assert!(
                matches!(
                    ledger.write(&run(8), &step("red"), &test_cause()),
                    Err(LedgerError::Store(_))
                ),
                "finished by {finisher}"
            );
        }
    }
}
