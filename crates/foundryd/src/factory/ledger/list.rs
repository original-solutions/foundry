//! Listing the runs in the ledger.
//!
//! The runner's `src/ledger/listRuns.ts`, and `nextAttempt` of its `runId.ts`, over the
//! store. A run is listed once it has an event. The runner lists runs oldest first by the
//! wall-clock time of each run's first line; nothing here orders by wall-clock time, so runs
//! are listed by run id: the repo, then the issue and the attempt as numbers. A run whose
//! events the store holds but this build cannot read is still listed, flagged `unreadable`,
//! after the others.
//!
//! The runner's progress read (`runner/progress.ts`, `Seen`) is not ported yet; the few facts
//! of it a listing needs are read here, by the runner's rules.

use foundry_proto::factory::{RunEvent, RunId};

use super::Ledger;
use super::events::{PrEnd, names};
use super::read::{events, field, text};
use crate::factory::lifecycle::State;
use crate::store::{Store, StoreError};

/// The step the runner writes when it approves a run: `approved <when>`.
const APPROVED: &str = "approved ";

/// The step written before a launch is sent.
const LAUNCH_SENDING: &str = "launch-sending";

/// The step written before a launch that carries on from an earlier branch is sent.
const LAUNCH_SENDING_RESUMED: &str = "launch-sending resumed-from ";

impl Ledger {
    /// [`runs`] of the ledger's store.
    pub fn runs(&self, repo: &str) -> Result<Vec<RunInfo>, StoreError> {
        runs(&self.lock(), repo)
    }

    /// [`issue_runs`] of the ledger's store.
    pub fn issue_runs(&self, repo: &str, issue: u64) -> Result<Vec<RunInfo>, StoreError> {
        issue_runs(&self.lock(), repo, issue)
    }

    /// [`next_attempt`] of the ledger's store.
    pub fn next_attempt(&self, repo: &str, issue: u64) -> Result<u32, StoreError> {
        next_attempt(&self.lock(), repo, issue)
    }

    /// [`summaries`] of the ledger's store.
    pub fn summaries(&self, repo: &str) -> Result<Vec<RunSummary>, StoreError> {
        summaries(&self.lock(), repo)
    }
}

/// The longest reason a summary carries, in characters.
pub const REASON_MAX_CHARS: usize = 300;

/// One unfinished run as the pass sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunInfo {
    pub id: RunId,
    /// The `to` of the last `state_changed`; `None` before any, or for a state this build does
    /// not know.
    pub state: Option<State>,
    /// The run's events could not be read, so the rest is not known.
    pub unreadable: bool,
    /// The run records what approved it or the session it launched. A run with neither can
    /// never launch one.
    pub confirmed: bool,
    /// A session may be running for it: the run records a launched session, or a launch that
    /// was sent.
    pub session: bool,
    /// The session reported its pull request since the run last entered `building`: its work
    /// is done and waits for the gate.
    pub pr_reported: bool,
    /// The base branch the run's newest launch that names one recorded.
    pub base: Option<String>,
}

/// Whether a run holds one of the machine's run slots: it is confirmed, not finished and not
/// waiting on a pull request (`pr-open`, or reported and waiting for the gate). A run whose
/// events cannot be read counts. Finished runs are not listed.
pub fn holds_slot(run: &RunInfo) -> bool {
    run.unreadable || (run.confirmed && run.state != Some(State::PrOpen) && !run.pr_reported)
}

/// What a listing reads of a run's events, by the runner's progress rules.
#[derive(Default)]
struct Facts {
    state: Option<State>,
    approved: bool,
    thread: bool,
    launch_sent: bool,
    pr_reported: bool,
    base: Option<String>,
    finished: bool,
}

impl Facts {
    fn of(events: &[RunEvent]) -> Self {
        let mut facts = Facts::default();
        for event in events {
            let data = &event.data;
            match event.event.as_str() {
                names::STEP_CHANGED => {
                    let Some(step) = text(data, "step") else {
                        continue;
                    };
                    if step.starts_with(APPROVED) {
                        facts.approved = true;
                    }
                    if step == LAUNCH_SENDING || step.starts_with(LAUNCH_SENDING_RESUMED) {
                        facts.launch_sent = true;
                    }
                }
                names::STATE_CHANGED => {
                    let to = text(data, "to");
                    facts.state = to.and_then(State::from_name);
                    if to == Some(State::Building.name()) {
                        facts.pr_reported = false;
                    }
                }
                names::SESSION_LAUNCHED => {
                    if text(data, "thread_id").is_some() {
                        facts.thread = true;
                    }
                    if let Some(base) = text(data, "base") {
                        facts.base = Some(base.to_owned());
                    }
                }
                names::PR_OPENED => {
                    if field(data, "pr").is_some_and(|pr| pr.is_number())
                        && text(data, "url").is_some()
                    {
                        facts.pr_reported = true;
                    }
                }
                names::GATE_SENT_BACK => facts.pr_reported = false,
                names::FINISHED => facts.finished = true,
                _ => {}
            }
        }
        facts
    }

    fn info(self, id: RunId) -> RunInfo {
        RunInfo {
            id,
            state: self.state,
            unreadable: false,
            confirmed: self.approved || self.thread,
            session: self.thread || self.launch_sent,
            pr_reported: self.pr_reported,
            base: self.base,
        }
    }
}

/// A run whose events could not be read.
fn unreadable(id: RunId) -> RunInfo {
    RunInfo {
        id,
        state: None,
        unreadable: true,
        confirmed: false,
        session: false,
        pr_reported: false,
        base: None,
    }
}

/// The unfinished runs among `ids`, unreadable ones last.
///
/// Each run's events are read whole, finished runs' too, since the store has no read of a
/// run's last event yet. The runner reads only a finished file's tail; a read of the runs of a
/// repo that have a `finished` event, in the store, would let this skip them the same way.
fn unfinished(store: &Store, ids: Vec<RunId>) -> Vec<RunInfo> {
    let (mut readable, mut rest) = (Vec::new(), Vec::new());
    for id in ids {
        match events(store, &id) {
            Ok(events) => {
                let facts = Facts::of(&events);
                if !facts.finished {
                    readable.push(facts.info(id));
                }
            }
            Err(error) => {
                tracing::warn!(run = %id, %error, "a run whose events cannot be read is listed as unreadable");
                rest.push(unreadable(id));
            }
        }
    }
    readable.extend(rest);
    readable
}

/// Every run of `repo` (`owner/name`, in any case) that is not finished, in run id order, with
/// any whose events cannot be read last.
pub fn runs(store: &Store, repo: &str) -> Result<Vec<RunInfo>, StoreError> {
    Ok(unfinished(store, store.runs_of(repo)?))
}

/// The unfinished runs of `issue` in `repo`, as [`runs`] lists them. Only that issue's runs
/// are read.
pub fn issue_runs(store: &Store, repo: &str, issue: u64) -> Result<Vec<RunInfo>, StoreError> {
    let ids = store
        .runs_of(repo)?
        .into_iter()
        .filter(|id| id.issue() == issue)
        .collect();
    Ok(unfinished(store, ids))
}

/// The attempt number for a new run of `issue` in `repo`: one past the highest attempt the
/// ledger holds, never above the largest, or 1 when there is none.
pub fn next_attempt(store: &Store, repo: &str, issue: u64) -> Result<u32, StoreError> {
    let highest = store
        .runs_of(repo)?
        .into_iter()
        .filter(|id| id.issue() == issue)
        .map(|id| id.attempt())
        .max();
    Ok(highest.map_or(1, |attempt| attempt.saturating_add(1)))
}

/// A run in one row, as the runner's `runs list` shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunSummary {
    pub id: RunId,
    /// The first event's `ts`.
    pub started_ts: Option<String>,
    /// The name of the last event.
    pub last_event: Option<String>,
    /// The issue title the run's first `session_launched` recorded: `None` when that event has
    /// none, or before the run launched.
    pub title: Option<String>,
    /// Why the run waits for a person: the `reason` of the newest `stuck` while its last state
    /// is `stuck`, the `questions` of the newest `questions_posted` while it is `needs-info`.
    /// Cut to [`REASON_MAX_CHARS`] characters.
    pub reason: Option<String>,
    /// The `to` of the last `state_changed`, when this build knows it.
    pub state: Option<State>,
    /// The `step` of the last `step_changed`; `None` before any and for a finished run.
    pub step: Option<String>,
    /// The `finished` event's pull request URL, else the last `pr_opened` URL.
    pub pr_url: Option<String>,
    /// How the run's pull request ended, from the `finished` event.
    pub pr_end: Option<PrEnd>,
    pub finished: bool,
    /// The `finished` event's `total_ms`.
    pub total_ms: Option<u64>,
    /// The run's events could not be read; every other field is empty.
    pub unreadable: bool,
}

impl RunSummary {
    fn unreadable(id: RunId) -> Self {
        Self {
            id,
            started_ts: None,
            last_event: None,
            title: None,
            reason: None,
            state: None,
            step: None,
            pr_url: None,
            pr_end: None,
            finished: false,
            total_ms: None,
            unreadable: true,
        }
    }

    fn of(id: RunId, events: &[RunEvent]) -> Self {
        let facts = Facts::of(events);
        let last = |name: &str| events.iter().rev().find(|event| event.event == name);
        let end = last(names::FINISHED);
        let reason_source = match facts.state {
            Some(State::Stuck) => Some((names::STUCK, "reason")),
            Some(State::NeedsInfo) => Some((names::QUESTIONS_POSTED, "questions")),
            _ => None,
        };
        let reason = reason_source
            .and_then(|(name, key)| text(&last(name)?.data, key))
            .map(cap_reason);
        let last_pr = events
            .iter()
            .rev()
            .filter(|event| event.event == names::PR_OPENED)
            .filter(|event| field(&event.data, "pr").is_some_and(|pr| pr.is_number()))
            .find_map(|event| text(&event.data, "url"));
        let step = (!facts.finished)
            .then(|| last(names::STEP_CHANGED).and_then(|event| text(&event.data, "step")))
            .flatten();
        Self {
            started_ts: events.first().map(|event| event.ts.clone()),
            last_event: events.last().map(|event| event.event.clone()),
            title: events
                .iter()
                .find(|event| event.event == names::SESSION_LAUNCHED)
                .and_then(|event| text(&event.data, "title"))
                .map(str::to_owned),
            reason,
            state: facts.state,
            step: step.map(str::to_owned),
            pr_url: end
                .and_then(|end| text(&end.data, "pr_url"))
                .or(last_pr)
                .map(str::to_owned),
            pr_end: end
                .and_then(|end| field(&end.data, "pr_end"))
                .and_then(|pr_end| serde_json::from_value(pr_end.clone()).ok()),
            finished: facts.finished,
            total_ms: end.and_then(|end| field(&end.data, "total_ms")?.as_u64()),
            unreadable: false,
            id,
        }
    }
}

/// `reason` cut to [`REASON_MAX_CHARS`] characters, ending in an ellipsis when cut.
fn cap_reason(reason: &str) -> String {
    if reason.chars().count() <= REASON_MAX_CHARS {
        return reason.to_owned();
    }
    let mut capped: String = reason.chars().take(REASON_MAX_CHARS - 1).collect();
    capped.push('\u{2026}');
    capped
}

/// One summary per run of `repo` (`owner/name`, in any case), finished runs included, in run
/// id order with any whose events cannot be read last.
pub fn summaries(store: &Store, repo: &str) -> Result<Vec<RunSummary>, StoreError> {
    let (mut readable, mut rest) = (Vec::new(), Vec::new());
    for id in store.runs_of(repo)? {
        match events(store, &id) {
            Ok(events) => readable.push(RunSummary::of(id, &events)),
            Err(error) => {
                tracing::warn!(run = %id, %error, "a run whose events cannot be read is listed as unreadable");
                rest.push(RunSummary::unreadable(id));
            }
        }
    }
    readable.extend(rest);
    Ok(readable)
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use foundry_proto::factory::{PrEnd, RunId};
    use foundry_proto::ids::MachineId;
    use serde_json::json;

    use super::*;
    use crate::factory::ledger::Ledger;
    use crate::factory::ledger::events::{Event, Outcome};
    use crate::factory::ledger::testing::*;
    use crate::factory::lifecycle::State;
    use crate::store::Store;

    fn rid(issue: u64, repo: &str, attempt: u32) -> RunId {
        RunId::new(repo, issue, attempt).unwrap()
    }

    fn softfac(issue: u64) -> RunId {
        rid(issue, "rawphp/softfac", 1)
    }

    fn claimed() -> Event {
        Event::Claimed {
            machine: "m".into(),
            github_user: "u".into(),
        }
    }

    fn moved(from: State, to: State) -> Event {
        Event::StateChanged { from, to }
    }

    fn launched(title: Option<&str>, base: Option<&str>) -> Event {
        Event::SessionLaunched {
            thread_id: "t-1".into(),
            worktree: Some("/wt".into()),
            branch: Some("b".into()),
            model_instance: Some("m".into()),
            model: Some("x".into()),
            model_options: None,
            resumed_from: None,
            base: base.map(str::to_owned),
            title: title.map(str::to_owned),
        }
    }

    fn asked(questions: &str) -> Event {
        Event::QuestionsPosted {
            questions: questions.into(),
        }
    }

    /// A machine's ledger over `store` whose clock starts at `start` and ticks a second a read.
    fn at(store: &Arc<Mutex<Store>>, start: u64) -> Ledger {
        ledger(store, MachineId::random(), 1, ticking(start, 1000))
    }

    /// A run started at `start_ms` whose last state is `to`, finished when `finish`.
    fn run_at(
        store: &Arc<Mutex<Store>>,
        id: &RunId,
        start_ms: u64,
        to: Option<State>,
        finish: bool,
        approved: bool,
    ) -> Ledger {
        let ledger = at(store, start_ms);
        if approved {
            ledger
                .write(id, &step("approved 2026-10-04T10:00:00Z"), &test_cause())
                .unwrap();
        }
        ledger.write(id, &claimed(), &test_cause()).unwrap();
        if let Some(to) = to {
            ledger
                .write(id, &moved(State::Ready, to), &test_cause())
                .unwrap();
        }
        if finish {
            ledger
                .finish(id, Outcome::Stuck, None, None, &test_cause(), None)
                .unwrap();
        }
        ledger
    }

    /// A run event row this build cannot read, as a newer build may leave one.
    fn unreadable_run(dir: &Path, id: &RunId) {
        let owner = MachineId::random();
        let conn = rusqlite::Connection::open(dir.join("foundry.db")).unwrap();
        conn.execute(
            "INSERT INTO factory_records (kind, id, owner, version, record)
             VALUES ('run_event', ?1, ?2, 1, '{\"kind\":\"run_event\",\"later\":true}')",
            rusqlite::params![format!("{id}/{owner}/1"), owner.to_string()],
        )
        .unwrap();
    }

    fn info(id: RunId, state: Option<State>, confirmed: bool) -> RunInfo {
        RunInfo {
            id,
            state,
            unreadable: false,
            confirmed,
            session: false,
            pr_reported: false,
            base: None,
        }
    }

    fn listed(store: &Arc<Mutex<Store>>, repo: &str) -> Vec<RunInfo> {
        runs(&store.lock().unwrap(), repo).unwrap()
    }

    fn summaries_of(store: &Arc<Mutex<Store>>, repo: &str) -> Vec<RunSummary> {
        summaries(&store.lock().unwrap(), repo).unwrap()
    }

    // The runner's `listRuns.test.ts`: `listUnfinishedRuns` is `runs`. The cases about the
    // runs directory and reading a file from its ends are the store's now.

    #[test]
    fn an_empty_data_dir_has_no_runs() {
        let (_dir, store) = open_store();
        assert_eq!(listed(&store, "rawphp/softfac"), []);
    }

    #[test]
    fn lists_the_unfinished_runs_in_run_id_order_with_their_state_and_without_the_finished_ones() {
        // The runner's "oldest first", by each run's first wall-clock time. Nothing here orders
        // records by wall-clock time: runs are listed in run id order, the issue and attempt as
        // numbers, so issue 1, started last, is listed first.
        let (_dir, store) = open_store();
        run_at(
            &store,
            &softfac(3),
            T0 + 20_000,
            Some(State::Building),
            false,
            true,
        );
        run_at(
            &store,
            &softfac(1),
            T0 + 90_000,
            Some(State::Triage),
            false,
            true,
        );
        run_at(
            &store,
            &softfac(2),
            T0 + 10_000,
            Some(State::PrOpen),
            true,
            true,
        );
        run_at(&store, &softfac(4), T0 + 30_000, None, false, true);
        assert_eq!(
            listed(&store, "rawphp/softfac"),
            [
                info(softfac(1), Some(State::Triage), true),
                info(softfac(3), Some(State::Building), true),
                info(softfac(4), None, true),
            ]
        );
    }

    #[test]
    fn a_run_that_ended_in_needs_info_is_not_listed() {
        let (_dir, store) = open_store();
        run_at(&store, &softfac(5), T0, Some(State::NeedsInfo), true, true);
        assert_eq!(listed(&store, "rawphp/softfac"), []);
    }

    #[test]
    fn a_run_whose_file_cannot_be_read_is_still_listed_flagged_unreadable_after_the_others() {
        let (dir, store) = open_store();
        run_at(
            &store,
            &softfac(7),
            T0 + 5_000,
            Some(State::Building),
            false,
            true,
        );
        unreadable_run(dir.path(), &softfac(6));
        assert_eq!(
            listed(&store, "rawphp/softfac"),
            [
                info(softfac(7), Some(State::Building), true),
                RunInfo {
                    unreadable: true,
                    ..info(softfac(6), None, false)
                },
            ]
        );
    }

    #[test]
    fn a_run_with_an_unknown_last_state_is_listed_with_no_state() {
        let (_dir, store) = open_store();
        put(
            &store,
            MachineId::random(),
            1,
            &softfac(9),
            1,
            T0,
            "state_changed",
            json!({"from": "ready", "to": "later"}),
            None,
        );
        assert_eq!(
            listed(&store, "rawphp/softfac"),
            [info(softfac(9), None, false)]
        );
    }

    #[test]
    fn a_run_with_no_approval_and_no_session_is_listed_as_not_confirmed() {
        let (_dir, store) = open_store();
        run_at(&store, &softfac(8), T0, Some(State::Triage), false, false);
        assert_eq!(
            listed(&store, "rawphp/softfac"),
            [info(softfac(8), Some(State::Triage), false)]
        );
    }

    #[test]
    fn with_one_issue_named_lists_only_that_issues_runs_whatever_the_repos_case() {
        let (dir, store) = open_store();
        run_at(&store, &softfac(1), T0, Some(State::Triage), false, true);
        run_at(
            &store,
            &rid(1, "rawphp/softfac", 2),
            T0 + 1_000,
            Some(State::Building),
            false,
            true,
        );
        run_at(
            &store,
            &softfac(2),
            T0 + 2_000,
            Some(State::Triage),
            false,
            true,
        );
        run_at(
            &store,
            &rid(1, "rawphp/other", 1),
            T0 + 3_000,
            Some(State::Triage),
            false,
            true,
        );
        // A run of another issue that cannot be read is not read, so it is not listed.
        unreadable_run(dir.path(), &softfac(3));
        // A run of this issue that cannot be read is listed last, so it still holds its slot.
        unreadable_run(dir.path(), &rid(1, "rawphp/softfac", 3));
        assert_eq!(
            issue_runs(&store.lock().unwrap(), "RawPhp/Softfac", 1).unwrap(),
            [
                info(softfac(1), Some(State::Triage), true),
                info(rid(1, "rawphp/softfac", 2), Some(State::Building), true),
                RunInfo {
                    unreadable: true,
                    ..info(rid(1, "rawphp/softfac", 3), None, false)
                },
            ]
        );
    }

    #[test]
    fn the_runs_of_another_repo_are_not_listed_and_the_repo_is_matched_in_any_case() {
        let (_dir, store) = open_store();
        run_at(&store, &softfac(1), T0, Some(State::Triage), false, true);
        run_at(
            &store,
            &rid(1, "rawphp/softfac__x", 1),
            T0,
            Some(State::Triage),
            false,
            true,
        );
        assert_eq!(
            listed(&store, "RawPHP/SoftFac"),
            [info(softfac(1), Some(State::Triage), true)]
        );
        assert_eq!(listed(&store, "not a repo"), []);
    }

    #[test]
    fn is_set_by_a_launched_session_or_a_launch_that_was_sent_and_not_by_the_claim_alone() {
        let (_dir, store) = open_store();
        run_at(&store, &softfac(1), T0, Some(State::Triage), false, true);
        run_at(
            &store,
            &softfac(2),
            T0 + 10_000,
            Some(State::Triage),
            false,
            true,
        );
        run_at(
            &store,
            &softfac(3),
            T0 + 20_000,
            Some(State::Triage),
            false,
            true,
        );
        run_at(
            &store,
            &softfac(4),
            T0 + 20_000,
            Some(State::Triage),
            false,
            false,
        );
        let later = at(&store, T0 + 30_000);
        later
            .write(&softfac(2), &step("launch-sending"), &test_cause())
            .unwrap();
        later
            .write(&softfac(3), &launched(None, None), &test_cause())
            .unwrap();
        later
            .write(
                &softfac(4),
                &step("launch-sending resumed-from fac/4-1"),
                &test_cause(),
            )
            .unwrap();
        let sessions: Vec<(u64, bool, bool)> = listed(&store, "rawphp/softfac")
            .into_iter()
            .map(|run| (run.id.issue(), run.session, run.confirmed))
            .collect();
        // A launched session confirms a run that recorded no approval; a launch sent does not.
        assert_eq!(
            sessions,
            [
                (1, false, true),
                (2, true, true),
                (3, true, true),
                (4, true, false)
            ]
        );
    }

    #[test]
    fn a_run_is_listed_with_the_base_its_newest_launch_recorded() {
        let (_dir, store) = open_store();
        let ledger = run_at(&store, &softfac(1), T0, Some(State::Building), false, false);
        ledger
            .write(&softfac(1), &launched(None, Some("release")), &test_cause())
            .unwrap();
        ledger
            .write(&softfac(1), &launched(None, None), &test_cause())
            .unwrap();
        assert_eq!(
            listed(&store, "rawphp/softfac"),
            [RunInfo {
                session: true,
                base: Some("release".into()),
                ..info(softfac(1), Some(State::Building), true)
            }]
        );
    }

    #[test]
    fn is_listed_from_pr_opened_until_the_run_is_building_again() {
        let (_dir, store) = open_store();
        let opened = run_at(&store, &softfac(1), T0, Some(State::Building), false, true);
        opened
            .write(
                &softfac(1),
                &Event::PrOpened {
                    pr: 12,
                    url: "https://x/pull/12".into(),
                },
                &test_cause(),
            )
            .unwrap();
        let reported = |store: &Arc<Mutex<Store>>| -> Vec<bool> {
            listed(store, "rawphp/softfac")
                .into_iter()
                .map(|run| run.pr_reported)
                .collect()
        };
        assert_eq!(reported(&store), [true]);
        opened
            .write(
                &softfac(1),
                &moved(State::Building, State::PrOpen),
                &test_cause(),
            )
            .unwrap();
        opened
            .write(
                &softfac(1),
                &moved(State::PrOpen, State::Building),
                &test_cause(),
            )
            .unwrap();
        assert_eq!(reported(&store), [false]);
        // A gate that sends the run back puts its session to work again.
        opened
            .write(
                &softfac(1),
                &Event::PrOpened {
                    pr: 12,
                    url: "https://x/pull/12".into(),
                },
                &test_cause(),
            )
            .unwrap();
        assert_eq!(reported(&store), [true]);
        opened
            .write(
                &softfac(1),
                &Event::GateSentBack {
                    attempt: 1,
                    run_ordinal: 2,
                },
                &test_cause(),
            )
            .unwrap();
        assert_eq!(reported(&store), [false]);
    }

    #[test]
    fn every_confirmed_unfinished_run_holds_a_slot_but_one_waiting_in_pr_open() {
        assert!(holds_slot(&info(softfac(1), Some(State::Building), true)));
        assert!(holds_slot(&info(softfac(1), Some(State::Triage), true)));
        assert!(holds_slot(&info(softfac(1), None, true)));
        assert!(!holds_slot(&info(softfac(1), Some(State::PrOpen), true)));
    }

    #[test]
    fn a_run_that_is_not_confirmed_holds_no_slot_it_never_launches_a_session() {
        assert!(!holds_slot(&info(softfac(1), Some(State::Triage), false)));
        assert!(!holds_slot(&info(softfac(1), None, false)));
    }

    #[test]
    fn a_building_run_whose_session_reported_its_pull_request_holds_no_slot_it_waits_for_the_gate()
    {
        assert!(!holds_slot(&RunInfo {
            pr_reported: true,
            ..info(softfac(1), Some(State::Building), true)
        }));
    }

    #[test]
    fn a_run_whose_file_cannot_be_read_holds_a_slot() {
        assert!(holds_slot(&RunInfo {
            unreadable: true,
            ..info(softfac(1), None, false)
        }));
        assert!(holds_slot(&RunInfo {
            unreadable: true,
            ..info(softfac(1), Some(State::PrOpen), false)
        }));
    }

    // `listRuns`: one summary per run, finished runs included.

    fn summary(id: RunId) -> RunSummary {
        RunSummary {
            id,
            started_ts: None,
            last_event: None,
            title: None,
            reason: None,
            state: None,
            step: None,
            pr_url: None,
            pr_end: None,
            finished: false,
            total_ms: None,
            unreadable: false,
        }
    }

    #[test]
    fn an_empty_ledger_lists_no_runs() {
        let (_dir, store) = open_store();
        assert_eq!(summaries_of(&store, "rawphp/softfac"), []);
    }

    #[test]
    fn lists_one_summary_per_run_in_run_id_order_finished_runs_included() {
        // The runner's "oldest first"; here in run id order, so issue 8, started an hour after
        // issue 12, is listed first.
        let (_dir, store) = open_store();
        run_at(&store, &softfac(12), T0 + 60_000, None, false, true);
        run_at(&store, &softfac(8), T0 + 3_600_000, None, true, true);
        assert_eq!(
            summaries_of(&store, "rawphp/softfac"),
            [
                RunSummary {
                    started_ts: Some("2026-10-04T11:13:16.123Z".into()),
                    last_event: Some("finished".into()),
                    finished: true,
                    total_ms: Some(2000),
                    ..summary(softfac(8))
                },
                RunSummary {
                    started_ts: Some("2026-10-04T10:14:16.123Z".into()),
                    last_event: Some("claimed".into()),
                    step: Some("approved 2026-10-04T10:00:00Z".into()),
                    ..summary(softfac(12))
                },
            ]
        );
    }

    #[test]
    fn reads_the_state_from_the_last_state_change() {
        let (_dir, store) = open_store();
        let ledger = at(&store, T0);
        ledger
            .write(
                &softfac(8),
                &moved(State::Triage, State::Building),
                &test_cause(),
            )
            .unwrap();
        ledger
            .write(
                &softfac(8),
                &moved(State::Building, State::PrOpen),
                &test_cause(),
            )
            .unwrap();
        let read: Vec<(Option<State>, Option<String>)> = summaries_of(&store, "rawphp/softfac")
            .into_iter()
            .map(|run| (run.state, run.last_event))
            .collect();
        assert_eq!(read, [(Some(State::PrOpen), Some("state_changed".into()))]);
    }

    #[test]
    fn reads_the_step_from_the_last_step_changed_of_a_run_read_whole_null_before_any() {
        let (_dir, store) = open_store();
        let ledger = at(&store, T0);
        ledger
            .write(&softfac(8), &claimed(), &test_cause())
            .unwrap();
        assert_eq!(summaries_of(&store, "rawphp/softfac")[0].step, None);
        ledger
            .write(&softfac(8), &step("launch-sending"), &test_cause())
            .unwrap();
        ledger
            .write(&softfac(8), &step("gate-passed"), &test_cause())
            .unwrap();
        ledger
            .write(&softfac(8), &claimed(), &test_cause())
            .unwrap();
        assert_eq!(
            summaries_of(&store, "rawphp/softfac")[0].step.as_deref(),
            Some("gate-passed")
        );
    }

    #[test]
    fn a_finished_run_has_no_step_read_whole_or_from_the_ends_of_its_file() {
        let (_dir, store) = open_store();
        let ledger = at(&store, T0);
        ledger
            .write(&softfac(8), &step("gate-passed"), &test_cause())
            .unwrap();
        ledger
            .finish(&softfac(8), Outcome::Stuck, None, None, &test_cause(), None)
            .unwrap();
        let read = &summaries_of(&store, "rawphp/softfac")[0];
        assert!(read.finished);
        assert_eq!(read.step, None);
    }

    #[test]
    fn reads_the_pull_request_url_from_the_last_pr_opened_and_from_the_finished_line_over_it() {
        let (_dir, store) = open_store();
        let ledger = at(&store, T0);
        for pr in [1, 2] {
            ledger
                .write(
                    &softfac(8),
                    &Event::PrOpened {
                        pr,
                        url: format!("https://example.test/pull/{pr}"),
                    },
                    &test_cause(),
                )
                .unwrap();
        }
        assert_eq!(
            summaries_of(&store, "rawphp/softfac")[0].pr_url.as_deref(),
            Some("https://example.test/pull/2")
        );
        ledger
            .finish(
                &softfac(8),
                Outcome::PrOpen,
                None,
                Some("https://example.test/pull/3"),
                &test_cause(),
                Some(PrEnd::Merged),
            )
            .unwrap();
        assert_eq!(
            summaries_of(&store, "rawphp/softfac")[0].pr_url.as_deref(),
            Some("https://example.test/pull/3")
        );
    }

    #[test]
    fn a_pr_open_run_reads_finished_once_its_pull_request_ended_and_how_it_ended() {
        let (_dir, store) = open_store();
        let ledger = at(&store, T0);
        ledger
            .write(
                &softfac(8),
                &moved(State::Building, State::PrOpen),
                &test_cause(),
            )
            .unwrap();
        assert!(!summaries_of(&store, "rawphp/softfac")[0].finished);
        ledger
            .finish(
                &softfac(8),
                Outcome::PrOpen,
                None,
                Some("https://github.com/rawphp/softfac/pull/12"),
                &test_cause(),
                Some(PrEnd::Closed),
            )
            .unwrap();
        let read = &summaries_of(&store, "rawphp/softfac")[0];
        assert_eq!(read.state, Some(State::PrOpen));
        assert!(read.finished);
        assert_eq!(
            read.pr_url.as_deref(),
            Some("https://github.com/rawphp/softfac/pull/12")
        );
        assert_eq!(read.pr_end, Some(PrEnd::Closed));
    }

    #[test]
    fn an_unreadable_run_file_is_listed_with_no_record_and_the_others_still_are() {
        let (dir, store) = open_store();
        run_at(&store, &softfac(8), T0, Some(State::Building), false, true);
        unreadable_run(dir.path(), &softfac(2));
        let read = summaries_of(&store, "rawphp/softfac");
        let flags: Vec<(u64, bool)> = read
            .iter()
            .map(|run| (run.id.issue(), run.unreadable))
            .collect();
        assert_eq!(flags, [(8, false), (2, true)]);
        assert_eq!(
            read[1],
            RunSummary {
                unreadable: true,
                ..summary(softfac(2))
            }
        );
    }

    #[test]
    fn orders_runs_with_the_same_start_by_repo_then_issue_and_attempt_as_numbers() {
        let (_dir, store) = open_store();
        for id in [
            rid(10, "rawphp/softfac", 1),
            rid(9, "rawphp/softfac", 2),
            rid(9, "rawphp/softfac", 10),
            rid(9, "rawphp/softfac", 1),
        ] {
            run_at(&store, &id, T0, None, false, true);
        }
        let order: Vec<(u64, u32)> = summaries_of(&store, "rawphp/softfac")
            .into_iter()
            .map(|run| (run.id.issue(), run.id.attempt()))
            .collect();
        assert_eq!(order, [(9, 1), (9, 2), (9, 10), (10, 1)]);
    }

    /// A finished run of `id` with `events` after the claim.
    fn finished_run(store: &Arc<Mutex<Store>>, id: &RunId, events: &[Event], outcome: Outcome) {
        let ledger = at(store, T0 + id.issue() * 60_000);
        ledger.write(id, &claimed(), &test_cause()).unwrap();
        for event in events {
            ledger.write(id, event, &test_cause()).unwrap();
        }
        ledger
            .finish(id, outcome, None, None, &test_cause(), None)
            .unwrap();
    }

    fn title_and_reason(store: &Arc<Mutex<Store>>) -> Vec<(u64, Option<String>, Option<String>)> {
        summaries_of(store, "rawphp/softfac")
            .into_iter()
            .map(|run| (run.id.issue(), run.title, run.reason))
            .collect()
    }

    #[test]
    fn reads_the_title_of_the_first_launch_and_the_reason_the_last_state_asks_for_as_the_whole_read_does()
     {
        let (_dir, store) = open_store();
        // The first titled launch wins; the stuck line before the state change gives the reason.
        finished_run(
            &store,
            &softfac(1),
            &[
                launched(Some("Old title"), None),
                launched(Some("New title"), None),
                stuck("older"),
                stuck("gate failed"),
                moved(State::Building, State::Stuck),
            ],
            Outcome::Stuck,
        );
        // Needs info: the newest questions, whichever side of the state change they sit on.
        finished_run(
            &store,
            &softfac(2),
            &[
                asked("first?"),
                moved(State::Triage, State::NeedsInfo),
                asked("which db?"),
            ],
            Outcome::NeedsInfo,
        );
        // The first launch has no title, so the run has none, whatever a later launch carries.
        finished_run(
            &store,
            &softfac(3),
            &[
                launched(None, None),
                launched(Some("Later title"), None),
                moved(State::Ready, State::Building),
            ],
            Outcome::PrOpen,
        );
        // No title anywhere: none. A stuck line that the run has since left says nothing.
        finished_run(
            &store,
            &softfac(4),
            &[
                launched(None, None),
                stuck("old"),
                moved(State::Stuck, State::Building),
            ],
            Outcome::PrOpen,
        );
        // Stuck with no stuck line: no reason.
        finished_run(
            &store,
            &softfac(5),
            &[moved(State::Building, State::Stuck)],
            Outcome::Stuck,
        );
        assert_eq!(
            title_and_reason(&store),
            [
                (1, Some("Old title".into()), Some("gate failed".into())),
                (2, None, Some("which db?".into())),
                (3, None, None),
                (4, None, None),
                (5, None, None),
            ]
        );
    }

    #[test]
    fn reads_the_title_and_reason_of_a_run_that_is_not_finished() {
        let (_dir, store) = open_store();
        let ledger = at(&store, T0);
        ledger
            .write(&softfac(1), &claimed(), &test_cause())
            .unwrap();
        ledger
            .write(
                &softfac(1),
                &launched(Some("Open run"), None),
                &test_cause(),
            )
            .unwrap();
        ledger
            .write(&softfac(1), &stuck("gate failed"), &test_cause())
            .unwrap();
        ledger
            .write(
                &softfac(1),
                &moved(State::Building, State::Stuck),
                &test_cause(),
            )
            .unwrap();
        assert_eq!(
            title_and_reason(&store),
            [(1, Some("Open run".into()), Some("gate failed".into()))]
        );
    }

    #[test]
    fn caps_a_long_reason_at_300_characters_on_a_character_boundary_in_both_reads() {
        let (_dir, store) = open_store();
        let long = "\u{1F600}".repeat(400);
        finished_run(
            &store,
            &softfac(1),
            &[stuck(&long), moved(State::Building, State::Stuck)],
            Outcome::Stuck,
        );
        finished_run(
            &store,
            &softfac(2),
            &[asked("short?"), moved(State::Triage, State::NeedsInfo)],
            Outcome::NeedsInfo,
        );
        let ledger = at(&store, T0);
        ledger
            .write(&softfac(3), &claimed(), &test_cause())
            .unwrap();
        ledger
            .write(&softfac(3), &stuck(&long), &test_cause())
            .unwrap();
        ledger
            .write(
                &softfac(3),
                &moved(State::Building, State::Stuck),
                &test_cause(),
            )
            .unwrap();
        let reasons: Vec<String> = summaries_of(&store, "rawphp/softfac")
            .into_iter()
            .map(|run| run.reason.unwrap_or_default())
            .collect();
        let capped = format!("{}\u{2026}", "\u{1F600}".repeat(299));
        for reason in [&reasons[0], &reasons[2]] {
            assert_eq!(reason.chars().count(), REASON_MAX_CHARS);
            assert_eq!(*reason, capped);
        }
        assert_eq!(reasons[1], "short?");
    }

    #[test]
    fn leaves_a_reason_of_exactly_300_characters_whole_and_cuts_one_of_301_to_299_and_an_ellipsis()
    {
        let (_dir, store) = open_store();
        let exact = "\u{1F600}".repeat(300);
        finished_run(
            &store,
            &softfac(1),
            &[stuck(&exact), moved(State::Building, State::Stuck)],
            Outcome::Stuck,
        );
        finished_run(
            &store,
            &softfac(2),
            &[
                stuck(&format!("{exact}\u{1F600}")),
                moved(State::Building, State::Stuck),
            ],
            Outcome::Stuck,
        );
        let reasons: Vec<Option<String>> = summaries_of(&store, "rawphp/softfac")
            .into_iter()
            .map(|run| run.reason)
            .collect();
        assert_eq!(
            reasons,
            [
                Some(exact),
                Some(format!("{}\u{2026}", "\u{1F600}".repeat(299)))
            ]
        );
    }

    // The runner's `runId.test.ts`: the run id itself is `foundry_proto::factory::RunId`, whose
    // tests are its own. The next attempt is read from the store.

    #[test]
    fn next_attempt_is_one_when_there_are_no_runs() {
        let (_dir, store) = open_store();
        assert_eq!(
            next_attempt(&store.lock().unwrap(), "rawphp/softfac", 8).unwrap(),
            1
        );
    }

    #[test]
    fn next_attempt_is_one_past_the_highest_for_that_repo_and_issue() {
        let (_dir, store) = open_store();
        for id in [
            rid(8, "rawphp/softfac", 1),
            rid(8, "rawphp/softfac", 2),
            rid(8, "rawphp/softfac", 4),
            rid(9, "rawphp/softfac", 7),
            rid(8, "rawphp/my_app", 3),
            rid(8, "rawphp/my__app", 6),
            rid(8, "rawphp/other", 9),
            rid(8, "someone/softfac", 9),
        ] {
            run_at(&store, &id, T0, None, false, true);
        }
        let store = store.lock().unwrap();
        assert_eq!(next_attempt(&store, "rawphp/softfac", 8).unwrap(), 5);
        assert_eq!(next_attempt(&store, "rawphp/softfac", 9).unwrap(), 8);
        assert_eq!(next_attempt(&store, "rawphp/softfac", 10).unwrap(), 1);
        assert_eq!(next_attempt(&store, "rawphp/other", 8).unwrap(), 10);
        assert_eq!(next_attempt(&store, "rawphp/my_app", 8).unwrap(), 4);
        assert_eq!(next_attempt(&store, "Rawphp/My__App", 8).unwrap(), 7);
        assert_eq!(next_attempt(&store, "RAWPHP/SoftFac", 8).unwrap(), 5);
        assert_eq!(next_attempt(&store, "../x", 8).unwrap(), 1);
    }

    #[test]
    fn next_attempt_matches_a_linear_identifier_in_any_case_and_never_passes_the_largest_attempt() {
        // Foundry's issues are GitHub numbers, so no Linear identifier can be written: the repo
        // is what is matched in any case. At the largest attempt the answer stays there, as the
        // runner's `Math.min(highest + 1, U32_MAX)` has it.
        let (_dir, store) = open_store();
        run_at(
            &store,
            &rid(8, "rawphp/softfac", u32::MAX),
            T0,
            None,
            false,
            true,
        );
        assert_eq!(
            next_attempt(&store.lock().unwrap(), "RawPHP/SoftFac", 8).unwrap(),
            u32::MAX
        );
    }
}
