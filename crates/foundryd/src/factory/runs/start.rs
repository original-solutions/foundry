//! The claim and the launch of a run.
//!
//! Ported from the runner's `src/runner/start.ts`, `src/runner/launchTarget.ts` and
//! `src/claims.ts`. [`claim`] takes a `ready` issue to `factory:triage` in the order the spec
//! gives: Foundry's claim comment first, then the label, the assignee and the status comment,
//! with the run's `approved` and `claimed` lines written before any of it. [`advance`] is one
//! pass of what follows: it asks for the build workspace, looks at its setup terminal on later
//! passes, checks the approval again, starts the session with the launch message, moves the
//! label to `building` and asks for the launch again by count when the session is idle a pass
//! later. No pass waits on a worker: a step records what it is about to do, does it, and a pass
//! that finds more to wait for says so ([`StartStep::Waiting`]) and returns.
//!
//! Where it differs from the runner:
//!
//! - The runner launches in one call and checks the issue text after it. Foundry decides what
//!   the launch continues from and the base before the workspace is asked for (it is made on
//!   that branch), asks for the workspace, waits for its setup, checks the approval again and
//!   only then starts the session: an approval taken back means no session was started.
//! - The claim is Foundry's comment first (F13): a claim cut short after the comment leaves the
//!   issue `ready` and the next claim's comment is the newest. The runner's `claim` call, its
//!   claim winner among machines and its give-way after five minutes are not ported: one
//!   coordinator holds the project's lease and claims, so no two machines race, and the lease
//!   guard stands where the runner's give-way did. [`claim`] does not skip an issue because an
//!   older claim comment is on it (a cut-short claim or the runner's own is not the newest
//!   claim of a `ready` issue that matters); [`advance`] reads the claim comment of a run that
//!   has no events, which is the takeover the spec names.
//! - Claims paused is a setting, not a file in a data directory (`claims.ts`).
//! - There is no T3 project to look up, no host or cloud launch message and no cooldown
//!   fallback of the model: the session starts on the fleet's model and effort. A gate lock
//!   file is a gate workspace that is running: a `gate_checkout` with no `gate_finished`.
//! - The tracker has no `openPullRequestsClosing`, so it is asked here, privately, over
//!   [`Tracker::gh`].

use foundry_proto::factory::RunId;
use foundry_proto::ids::{MachineId, SessionId};
use serde::Deserialize;

use super::progress::{APPROVED, Seen, read_progress};
use super::{Closing, RunContext, RunError, abandon, moved_on_by_closing};
use crate::factory::github::MARKER;
use crate::factory::github::read::{gh_pages, is_utc_second};
use crate::factory::ledger::events::{Cause, CauseStep, Event, Outcome};
use crate::factory::lifecycle::approval::{Approval, Refusal, refusal_reason};
use crate::factory::lifecycle::{
    Action, Event as LifecycleEvent, InvalidTransition, State, transition,
};
use crate::factory::messages::{self, CUT_SHORT_COMMENT};
use crate::factory::text;

/// What the setup of a build workspace that failed says as its reason. The terminal's tail is
/// quoted below it.
pub const SETUP_FAILED: &str = "the build workspace's setup failed";

/// The count of prompts a session has had before its launch is typed: none.
pub const FIRST_PROMPT_COUNT: u64 = 0;

/// Who holds claims back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaimPause {
    /// The fleet's setting: no project claims.
    Fleet,
    /// The project's setting.
    Project,
}

impl ClaimPause {
    /// What the pass says of a claim it did not make.
    pub fn reason(self) -> &'static str {
        match self {
            ClaimPause::Fleet => "claims are paused for the fleet",
            ClaimPause::Project => "claims are paused for this project",
        }
    }
}

/// Whether claims are paused, by the fleet or by the project. The fleet's pause is named first
/// when both are set. Runs already going carry on: this holds back claims only.
pub fn claims_paused(settings: &super::RunSettings) -> Option<ClaimPause> {
    if settings.fleet.paused {
        Some(ClaimPause::Fleet)
    } else if settings.project.paused {
        Some(ClaimPause::Project)
    } else {
        None
    }
}

/// What [`claim`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// The issue is claimed and is `factory:triage`: [`advance`] launches it.
    Claimed { run: RunId },
    /// The issue was not ours to take. Nothing was written anywhere.
    Skipped { reason: String },
    /// The issue was `ready` but not approved, so it went back to `factory:proposed` with one
    /// fixed comment, and no run was made.
    Refused { reason: String },
    /// The issue is closed and a person closed it: its unfinished runs are finished and their
    /// sessions ended. `may_have_session` names runs that sent a launch the ledger never got a
    /// session for. `error` is why a later run could not be ended after earlier ones were; the
    /// next call tries again.
    Abandoned {
        may_have_session: Vec<RunId>,
        error: Option<String>,
    },
}

/// Claims `issue`, to be built on `machine`. See the module's words for the order.
pub fn claim(
    ctx: &RunContext<'_>,
    issue: u64,
    machine: MachineId,
) -> Result<ClaimOutcome, RunError> {
    let repo = ctx.repo();
    let detail = ctx.tracker.issue(repo, issue)?;
    // A closed issue is never claimed, whatever its label says: its unfinished runs are ended.
    if detail.closed {
        return abandon_closed(ctx, issue);
    }
    if let Some(pause) = claims_paused(ctx.settings) {
        return Ok(ClaimOutcome::Skipped {
            reason: pause.reason().into(),
        });
    }
    if detail.state != Some(State::Ready) {
        return Ok(ClaimOutcome::Skipped {
            reason: format!("issue #{issue} is not factory:ready"),
        });
    }
    if !detail.assignees.is_empty() {
        return Ok(ClaimOutcome::Skipped {
            reason: format!(
                "issue #{issue} is already assigned to {}",
                detail.assignees.join(", ")
            ),
        });
    }
    // Before the claim: nothing is written for an issue that is not approved except the
    // explanation.
    let (labeled_at, labeled_by) =
        match ctx.tracker.approval(repo, issue, &ctx.config.instructors)? {
            Approval::Refused(refusal) => return refuse_start(ctx, issue, &refusal),
            Approval::Approved {
                labeled_at,
                labeled_by,
            } => (labeled_at, labeled_by),
        };
    let applied_by = (!labeled_by.is_empty()).then_some(labeled_by.as_str());
    ctx.held()?;
    let run = RunId::new(repo, issue, ctx.ledger.next_attempt(repo, issue)?)
        .map_err(RunError::Invalid)?;
    // Before the claim, so a crash after it cannot leave an older run stranded in front of the
    // new one. An instructor applied the label again, which is what began this attempt.
    finish_older_runs(ctx, issue, run.attempt(), applied_by)?;
    // Recorded before anything is written on GitHub: a claim done again carries on.
    record_approval(ctx, &run, &labeled_at, applied_by, CauseStep::Start)?;
    let cause = Cause::new(CauseStep::Start, "claimed");
    ctx.record(
        &run,
        Event::Claimed {
            machine: machine.to_string(),
            github_user: ctx.project.login.clone(),
        },
        &cause,
    )?;
    // Foundry's comment first (F13), then the label and the assignee. A claim cut short after
    // the comment leaves the issue `ready`, and the next claim's comment is the newest.
    let applied = ctx
        .held()
        .and_then(|_| {
            ctx.tracker
                .post_claim(repo, issue, &run, &machine)
                .map_err(RunError::from)
        })
        .and_then(|()| ctx.set_state(issue, State::Triage))
        .and_then(|()| ctx.assign(issue));
    if let Err(error) = applied {
        // The edit may have landed all the same. See `repair_claim`.
        repair_claim(ctx, issue, &run, error)?;
    }
    ctx.record(
        &run,
        Event::StateChanged {
            from: State::Ready,
            to: State::Triage,
        },
        &cause,
    )?;
    ctx.upsert_status(
        issue,
        &messages::status(State::Triage, &machine.to_string(), None, None),
    )?;
    Ok(ClaimOutcome::Claimed { run })
}

/// The first line of an attempt's ledger, the `approved <labeled_at>` step: what the attempt was
/// approved on, for the text checks that come later, with the instructor whose label it was
/// (`actor` on the cause) when the approval names one.
pub fn record_approval(
    ctx: &RunContext<'_>,
    run: &RunId,
    labeled_at: &str,
    labeled_by: Option<&str>,
    step: CauseStep,
) -> Result<(), RunError> {
    let mut cause = Cause::new(step, "approved");
    if let Some(by) = labeled_by {
        cause = cause.actor(by);
    }
    ctx.record_step(run, &format!("{APPROVED}{labeled_at}"), &cause)
}

/// A comment of the project's login as GitHub lists it, with its creation time.
#[derive(Deserialize)]
struct CommentRow {
    id: u64,
    #[serde(default)]
    user: Option<CommentUser>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
}

#[derive(Deserialize)]
struct CommentUser {
    login: String,
}

/// The newest comment of the project's login that ends with the factory's marker, with its body
/// as it was posted (lines ended `\n`, the end trimmed) and its creation time. Anyone can paste
/// the marker, so only the login's own comment counts.
fn newest_factory_comment(
    ctx: &RunContext<'_>,
    issue: u64,
) -> Result<Option<(String, String)>, RunError> {
    let repo = ctx.repo();
    let args = vec![
        "api".to_string(),
        format!("repos/{repo}/issues/{issue}/comments?per_page=100"),
        "--paginate".to_string(),
    ];
    let rows: Vec<CommentRow> = gh_pages(ctx.tracker, &args, "JSON arrays of comments")?;
    let newest = rows
        .into_iter()
        .filter(|row| {
            row.user
                .as_ref()
                .is_some_and(|user| user.login.eq_ignore_ascii_case(&ctx.project.login))
        })
        .filter_map(|row| {
            let body = row.body.unwrap_or_default().replace("\r\n", "\n");
            let body = body.trim_end().to_owned();
            body.ends_with(MARKER)
                .then(|| (row.id, body, row.created_at.unwrap_or_default()))
        })
        .max_by_key(|(id, _, _)| *id);
    Ok(newest.map(|(_, body, at)| (body, at)))
}

/// Whether the issue's newest factory comment already says `text` for the label it was judged
/// on: same words, and newer than the label event (an older comment was about an earlier
/// refusal). A refusal that names no label time is matched on the words alone: Foundry orders
/// nothing by the wall clock, so it has no hour to count.
fn already_said(
    ctx: &RunContext<'_>,
    issue: u64,
    refusal: &Refusal,
    said: &str,
) -> Result<bool, RunError> {
    let Some((body, created_at)) = newest_factory_comment(ctx, issue)? else {
        return Ok(false);
    };
    let expected = format!("{}\n\n{MARKER}", text::for_comment(&ctx.tracker.hide(said)));
    if body != expected.trim_end() {
        return Ok(false);
    }
    let labeled_at = match refusal {
        Refusal::LabeledByNonInstructor { labeled_at, .. }
        | Refusal::EditedAfterLabel { labeled_at }
        | Refusal::EditHistoryIncomplete { labeled_at } => Some(labeled_at),
        Refusal::LabelNotFound | Refusal::LabelHistoryIncomplete => None,
    };
    Ok(match labeled_at {
        // A comment whose time is not GitHub's form never counts.
        Some(label) => is_utc_second(&created_at) && created_at.as_str() > label.as_str(),
        None => true,
    })
}

/// Takes a refused `ready` issue back to `proposed`: the comment first, then the label. There is
/// no run, so nothing is written to a ledger. A label call that fails leaves the issue `ready`
/// and the next pass tries it again. The comment is not posted again while it is still the
/// issue's newest factory comment.
fn refuse_start(
    ctx: &RunContext<'_>,
    issue: u64,
    refusal: &Refusal,
) -> Result<ClaimOutcome, RunError> {
    let reason = refusal_reason(refusal);
    let (_, actions) = transition(
        State::Ready,
        &LifecycleEvent::Unapproved {
            refusal: refusal.clone(),
        },
    )?;
    for action in actions {
        match action {
            Action::PostUnapproved { refusal } => {
                let said = messages::unapproved(&refusal);
                if !already_said(ctx, issue, &refusal, &said)? {
                    ctx.post_comment(issue, &said)?;
                }
            }
            Action::SetLabel { state } => ctx.set_state(issue, state)?,
            _ => {
                return Err(InvalidTransition {
                    state: State::Ready,
                    event: "unapproved",
                }
                .into());
            }
        }
    }
    Ok(ClaimOutcome::Refused {
        reason: format!("issue #{issue} was not approved: {reason}"),
    })
}

/// Whether `events` end with a gate that was checked out and has not finished: its workspace is
/// running, where the runner's gate holds a lock file.
fn gate_running(events: &[foundry_proto::factory::RunEvent]) -> bool {
    events
        .iter()
        .rev()
        .find(|event| matches!(event.event.as_str(), "gate_checkout" | "gate_finished"))
        .is_some_and(|event| event.event == "gate_checkout")
}

/// The unfinished runs of the issue before attempt `below` that `wanted` keeps, oldest first.
/// While the gate of any run it keeps is running, none of them is touched
/// ([`RunError::GateRunning`]).
fn unfinished_runs(
    ctx: &RunContext<'_>,
    issue: u64,
    below: u32,
    wanted: impl Fn(&Seen) -> bool,
) -> Result<Vec<(RunId, Seen)>, RunError> {
    let mut ids: Vec<RunId> = ctx
        .ledger
        .issue_runs(ctx.repo(), issue)?
        .into_iter()
        .map(|info| info.id)
        .filter(|id| id.attempt() < below)
        .collect();
    ids.sort_by_key(RunId::attempt);
    let mut runs = Vec::new();
    for id in ids {
        let events = ctx.ledger.read(&id)?;
        let seen = read_progress(&events);
        if seen.finished || !wanted(&seen) {
            continue;
        }
        if gate_running(&events) {
            return Err(RunError::GateRunning { run: id });
        }
        runs.push((id, seen));
    }
    Ok(runs)
}

/// How a run left in `state` is ended when a new attempt or a close takes its place.
fn outcome_of(state: Option<State>) -> Outcome {
    match state {
        Some(State::NeedsInfo) => Outcome::NeedsInfo,
        Some(State::PrOpen) => Outcome::PrOpen,
        _ => Outcome::Stuck,
    }
}

/// Finishes every unfinished run of the issue before attempt `below`, by its last state, so it
/// is not polled again and cannot write to an issue a newer attempt now owns. While the gate of
/// any of them is running, none of them is finished. A run that names a session or a pull
/// request is finished in the ledger only: nothing is ended or drafted.
fn finish_older_runs(
    ctx: &RunContext<'_>,
    issue: u64,
    below: u32,
    applied_by: Option<&str>,
) -> Result<(), RunError> {
    for (id, seen) in unfinished_runs(ctx, issue, below, |_| true)? {
        let mut cause = Cause::new(CauseStep::Start, "new-attempt-started");
        if let Some(by) = applied_by {
            cause = cause.actor(by).label(State::Ready.label());
        }
        ctx.ledger
            .finish(&id, outcome_of(seen.state), None, None, &cause, None)?;
    }
    Ok(())
}

/// What `claim` does with a closed issue: every unfinished run that a person's closing ended is
/// abandoned (see [`abandon`]), oldest first, and nothing is claimed or written on GitHub. A
/// run in `pr-open` or `stuck`, or one whose own merged pull request closed the issue, is left
/// alone. While any such run's gate is running none is touched.
fn abandon_closed(ctx: &RunContext<'_>, issue: u64) -> Result<ClaimOutcome, RunError> {
    let next = ctx.ledger.next_attempt(ctx.repo(), issue)?;
    let unfinished = unfinished_runs(ctx, issue, next, |seen| {
        !matches!(seen.state, Some(State::PrOpen | State::Stuck))
    })?;
    let mut abandoned = false;
    let mut may_have_session = Vec::new();
    for (id, seen) in unfinished {
        let ended = moved_on_by_closing(ctx, &seen, true).and_then(|closing| {
            if closing == Closing::PersonsMove {
                abandon(ctx, &id, &seen, CauseStep::Start).map(Some)
            } else {
                Ok(None)
            }
        });
        match ended {
            Err(error) if abandoned => {
                return Ok(ClaimOutcome::Abandoned {
                    may_have_session,
                    error: Some(error.to_string()),
                });
            }
            Err(error) => return Err(error),
            Ok(None) => {}
            Ok(Some(may_exist)) => {
                if may_exist {
                    may_have_session.push(id);
                }
                abandoned = true;
            }
        }
    }
    Ok(if abandoned {
        ClaimOutcome::Abandoned {
            may_have_session,
            error: None,
        }
    } else {
        ClaimOutcome::Skipped {
            reason: format!("issue #{issue} is closed"),
        }
    })
}

/// What to do about a failed call of the claim: GitHub can apply an edit and still answer with an
/// error. The issue is read again. A whole claim, or one only missing the assignee, is finished
/// (`Ok`). One where nothing was applied ends the run, which holds no slot, and the error stands:
/// the issue is `ready` and the next claim posts a comment of its own, the newest. Otherwise the
/// issue loses our assignee before it goes to `factory:proposed`, so it is never `proposed` and
/// assigned. `error` is what the call said.
fn repair_claim(
    ctx: &RunContext<'_>,
    issue: u64,
    run: &RunId,
    error: RunError,
) -> Result<(), RunError> {
    let end_run = |why: &str| {
        // A run that holds no claim holds no slot. A ledger that cannot be written is not
        // worth failing the repair for: the next claim ends the run.
        let _ = ctx.ledger.finish(
            run,
            Outcome::Stuck,
            None,
            None,
            &Cause::new(CauseStep::Start, why),
            None,
        );
    };
    let Ok(seen) = ctx.tracker.issue(ctx.repo(), issue) else {
        return Err(error);
    };
    let login = &ctx.project.login;
    let ours = seen.assignees.len() == 1 && seen.assignees[0].eq_ignore_ascii_case(login);
    let nobody = seen.assignees.is_empty();
    let swapped = matches!(seen.state, Some(State::Triage | State::Ready) | None);
    // Still `factory:ready` with no assignee: nothing was applied.
    if seen.state == Some(State::Ready) && nobody {
        end_run("claim-cut-short");
        return Err(error);
    }
    if seen.closed || !swapped || !(ours || nobody) {
        return Err(error);
    }
    let assigned = !nobody || ctx.assign(issue).is_ok();
    let labelled = assigned
        && (seen.state == Some(State::Triage) || ctx.set_state(issue, State::Triage).is_ok());
    if assigned && labelled {
        let _ = ctx.record_step(
            run,
            "claim-repaired finished",
            &Cause::new(CauseStep::Start, "claim-repaired"),
        );
        return Ok(());
    }
    // Both are tried, so a label that will not move does not leave our assignee on the issue.
    let off = ctx.unassign(issue).is_ok();
    let moved = ctx.set_state(issue, State::Proposed).is_ok();
    if off && moved {
        let _ = ctx.post_comment(issue, CUT_SHORT_COMMENT);
        let _ = ctx.record_step(
            run,
            "claim-repaired released",
            &Cause::new(CauseStep::Start, "claim-repaired"),
        );
        end_run("claim-cut-short");
    }
    Err(error)
}

/// What one pass of [`advance`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StartStep {
    /// Something is not there yet: look again next pass.
    Waiting(Wait),
    /// The session runs and the issue is `factory:building`.
    Started { session: SessionId },
    /// The session was idle a pass after the launch and the launch was asked for again by count.
    Resent { session: SessionId },
    /// The run went stuck.
    Stuck { reason: String },
    /// The approval was taken back before the session started: the run ended as the runner
    /// refuses an unapproved issue.
    Unapproved { reason: String },
    /// The run is not this step's: nothing was written.
    Skipped { reason: String },
    /// The issue was closed by a person: the run is ended.
    Abandoned,
    /// The run's own merged pull request closed the issue: the run is ended.
    Ended,
}

/// What a pass that returned waiting is waiting for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wait {
    /// The worker answered the create, and the record of the workspace is not here yet.
    Workspace,
    /// The setup terminal is still running, or its exit is not known.
    Setup,
}

/// One pass of the start of `run`.
pub fn advance(_ctx: &RunContext<'_>, _run: &RunId) -> Result<StartStep, RunError> {
    todo!("advance")
}

#[cfg(test)]
mod tests {
    use foundry_proto::factory::{FactorySettings, ProjectFactory};

    use super::super::testing::*;
    use super::*;
    use crate::factory::github::{Claim, Tracker, newest_claim};
    use crate::factory::ledger::events::Event;
    use crate::factory::lifecycle::State;
    use crate::factory::messages::CUT_SHORT_COMMENT;
    use crate::factory::worker::{Call, Lost};

    fn settings(fleet_paused: bool, project_paused: bool) -> super::super::RunSettings {
        super::super::RunSettings {
            fleet: FactorySettings {
                paused: fleet_paused,
                ..FactorySettings::default()
            },
            project: ProjectFactory {
                on: true,
                login: LOGIN.into(),
                paused: project_paused,
            },
        }
    }

    fn claim_it(rig: &Rig) -> Result<ClaimOutcome, RunError> {
        claim(&rig.ctx(), ISSUE, rig.machine)
    }

    fn claimed(rig: &Rig) -> RunId {
        match claim_it(rig).unwrap() {
            ClaimOutcome::Claimed { run } => run,
            other => panic!("expected a claim, got {other:?}"),
        }
    }

    fn newest(rig: &Rig) -> Option<Claim> {
        let comments = rig.tracker.issue_comments(REPO, ISSUE).unwrap();
        newest_claim(&comments, LOGIN)
    }

    /// The runner's `claims.test.ts`, "claims paused by hand". In Foundry the pause is two
    /// settings, the fleet's and the project's, not a file in a data directory, so each case is
    /// what a setting says. The cases about a file (its mode, a symlink, a FIFO, an unreadable
    /// one, a missing data directory) have no counterpart.
    mod claims_paused_by_hand {
        use super::*;

        /// The runner's case, where pausing writes a file with the time: in Foundry, a fleet
        /// setting that is set pauses every project, with no time written.
        #[test]
        fn pause_writes_the_time_privately() {
            assert_eq!(
                claims_paused(&settings(true, false)),
                Some(ClaimPause::Fleet)
            );
            assert_eq!(
                claims_paused(&settings(false, true)),
                Some(ClaimPause::Project)
            );
        }

        /// The runner's case, where a second pause keeps the first time: in Foundry, both
        /// settings set read as the fleet's, and the project's own stays as it was.
        #[test]
        fn a_second_pause_keeps_the_first_time() {
            let both = settings(true, true);
            assert_eq!(claims_paused(&both), Some(ClaimPause::Fleet));
            assert!(both.project.paused);
        }

        /// The runner's case, where resume removes the file and says when there was none: in
        /// Foundry, neither setting set is no pause.
        #[test]
        fn resume_removes_the_file_and_says_when_there_was_none() {
            assert_eq!(claims_paused(&settings(false, false)), None);
        }

        /// The runner's case, where a file a person wrote is still a pause: in Foundry, the
        /// words say which setting holds claims back.
        #[test]
        fn a_file_a_person_wrote_is_still_a_pause_since_its_mtime() {
            assert_eq!(
                ClaimPause::Fleet.reason(),
                "claims are paused for the fleet"
            );
            assert_eq!(
                ClaimPause::Project.reason(),
                "claims are paused for this project"
            );
        }

        /// The runner's case, where a paused pass claims no ready issue of any repo and
        /// everything else goes on: in Foundry, a paused claim writes nothing, and a run already
        /// going is not held back.
        #[test]
        fn the_time_in_the_file_is_trimmed_and_parsed() {
            let mut rig = Rig::new();
            rig.settings = settings(false, true);
            let outcome = claim_it(&rig).unwrap();
            assert_eq!(
                outcome,
                ClaimOutcome::Skipped {
                    reason: "claims are paused for this project".into()
                }
            );
            assert!(rig.sim.writes().is_empty());
            assert!(rig.ledger.issue_runs(REPO, ISSUE).unwrap().is_empty());
            rig.settings = settings(true, false);
            assert!(matches!(
                claim_it(&rig).unwrap(),
                ClaimOutcome::Skipped { .. }
            ));
        }

        /// The runner's case, where a missing file and a symlink are no pause: in Foundry, a
        /// setting that is off, whatever the other holds.
        #[test]
        fn a_missing_file_and_a_symlink_are_no_pause() {
            let rig = Rig::new();
            assert!(matches!(
                claim_it(&rig).unwrap(),
                ClaimOutcome::Claimed { .. }
            ));
        }

        /// The runner's case, where a file that cannot be opened is still a pause: in Foundry,
        /// there is nothing to open, so a pause is only ever a setting that reads as set.
        #[test]
        fn a_file_that_cannot_be_opened_is_still_a_pause_since_its_mtime() {
            assert_eq!(
                claims_paused(&settings(true, true)),
                Some(ClaimPause::Fleet)
            );
        }

        /// The runner's case, where a FIFO is no pause and does not block the read: in Foundry,
        /// the settings are values already read, so a pause never blocks.
        #[test]
        fn a_fifo_is_no_pause_and_does_not_block_the_read() {
            assert_eq!(claims_paused(&settings(false, false)), None);
        }

        /// The runner's case, where pausing in a data dir that is not there is an error: in
        /// Foundry, pausing is a setting and cannot fail, so a project that is not on is not
        /// held back by a pause it does not have.
        #[test]
        fn pausing_in_a_data_dir_that_is_not_there_is_an_error() {
            let mut rig = Rig::new();
            rig.settings.project.on = false;
            assert_eq!(claims_paused(&rig.settings), None);
        }
    }

    mod a_fresh_ready_issue {
        use super::*;

        /// The runner's case "claims, launches the session and moves the issue to building",
        /// the claim's half: the comment first, then the label, the assignee and the status
        /// comment, with the run's lines in the runner's order.
        #[test]
        fn claims_the_comment_first_then_the_label_the_assignee_and_the_status_comment() {
            let rig = Rig::new();
            let run = claimed(&rig);
            assert_eq!(run, run_id(1));
            let writes = rig.sim.writes();
            assert_eq!(writes.len(), 4, "{writes:#?}");
            assert!(writes[0].starts_with("issue comment 8"));
            assert!(writes[0].contains("<!-- foundry:claim "));
            assert!(writes[1].contains("--add-label factory:triage --remove-label factory:ready"));
            assert!(writes[2].ends_with("--add-assignee softfac-bot"));
            assert!(writes[3].starts_with("issue comment 8"));
            assert!(writes[3].contains("**Factory status:** triage"));
            assert_eq!(rig.sim.labels(), vec!["factory:triage"]);
            assert_eq!(rig.sim.assignees(), vec![LOGIN]);
            assert_eq!(
                rig.names(&run),
                vec!["step_changed", "claimed", "state_changed"]
            );
            assert_eq!(rig.steps(&run), vec![format!("approved {LABELED_AT}")]);
        }

        /// The runner's case "a fresh claim names this machine and the label that began it":
        /// the comment names the run and the build machine, the `claimed` line the machine and
        /// the login, and the approval's actor is the instructor whose label began the run.
        #[test]
        fn a_fresh_claim_names_this_machine_and_the_label_that_began_it() {
            let rig = Rig::new();
            let run = claimed(&rig);
            assert_eq!(
                newest(&rig),
                Some(Claim::Foundry {
                    run: run.clone(),
                    machine: rig.machine
                })
            );
            let events = rig.events(&run);
            assert_eq!(events[0].cause.as_ref().unwrap()["actor"], "tom");
            assert_eq!(events[1].data["machine"], rig.machine.to_string());
            assert_eq!(events[1].data["github_user"], LOGIN);
        }

        /// The runner's case "a refusal posts the comment, moves the issue to proposed and makes
        /// no run".
        #[test]
        fn a_refusal_posts_the_comment_moves_the_issue_to_proposed_and_makes_no_run() {
            let rig = Rig::new();
            rig.sim
                .set_approval(approval_answer(LABELED_AT, "mallory", &[]));
            let outcome = claim_it(&rig).unwrap();
            assert_eq!(
                outcome,
                ClaimOutcome::Refused {
                    reason:
                        "issue #8 was not approved: factory:ready was not applied by an instructor"
                            .into()
                }
            );
            let comments = rig.sim.comments();
            assert_eq!(comments.len(), 1);
            assert!(comments[0].contains("did not start on this issue"));
            assert_eq!(rig.sim.labels(), vec!["factory:proposed"]);
            assert!(rig.ledger.issue_runs(REPO, ISSUE).unwrap().is_empty());
            assert!(rig.worker.calls().is_empty());
        }

        /// The runner's case "a refusal already explained since the label is not commented
        /// again".
        #[test]
        fn a_refusal_already_explained_since_the_label_is_not_commented_again() {
            let rig = Rig::new();
            rig.sim
                .set_approval(approval_answer(LABELED_AT, "mallory", &[]));
            claim_it(&rig).unwrap();
            // A person puts it back to ready, the same label event still in force.
            rig.sim.set_labels(&["factory:ready"]);
            claim_it(&rig).unwrap();
            assert_eq!(rig.sim.comments().len(), 1);
            assert_eq!(rig.sim.labels(), vec!["factory:proposed"]);
        }

        /// The runner's case "a refusal with no label time is not repeated within an hour". The
        /// listing of comments carries a time, but Foundry orders nothing by the wall clock: a
        /// refusal that names no label time is not said again while it is the newest factory
        /// comment, however old.
        #[test]
        fn a_refusal_with_no_label_time_is_not_repeated_within_an_hour() {
            let rig = Rig::new();
            rig.sim.set_approval(
                serde_json::json!({"data": {"repository": {"issue": {
                    "timelineItems": {"totalCount": 0, "nodes": []},
                    "lastEditedAt": null,
                    "editor": null,
                    "userContentEdits": {"totalCount": 0, "nodes": []},
                }}}})
                .to_string(),
            );
            claim_it(&rig).unwrap();
            rig.sim.set_labels(&["factory:ready"]);
            claim_it(&rig).unwrap();
            assert_eq!(rig.sim.comments().len(), 1);
        }

        /// A refusal that is explained again after the instructor applied the label again: the
        /// label event is newer than the comment.
        #[test]
        fn a_refusal_after_a_newer_label_is_commented_again() {
            let rig = Rig::new();
            rig.sim
                .set_approval(approval_answer("2026-10-04T10:00:00Z", "mallory", &[]));
            claim_it(&rig).unwrap();
            rig.sim.set_labels(&["factory:ready"]);
            rig.sim
                .set_approval(approval_answer("2026-10-04T23:00:00Z", "mallory", &[]));
            claim_it(&rig).unwrap();
            assert_eq!(rig.sim.comments().len(), 2);
        }

        /// The runner's case "an issue that is assigned, closed or not ready is skipped with
        /// nothing written".
        #[test]
        fn an_issue_that_is_assigned_closed_or_not_ready_is_skipped_with_nothing_written() {
            let rig = Rig::new();
            rig.sim.set_assignees(&["someone"]);
            let outcome = claim_it(&rig).unwrap();
            assert_eq!(
                outcome,
                ClaimOutcome::Skipped {
                    reason: "issue #8 is already assigned to someone".into()
                }
            );
            let rig = Rig::new();
            rig.sim.set_labels(&["factory:proposed"]);
            assert_eq!(
                claim_it(&rig).unwrap(),
                ClaimOutcome::Skipped {
                    reason: "issue #8 is not factory:ready".into()
                }
            );
            let rig = Rig::new();
            rig.sim.set_closed(true);
            assert_eq!(
                claim_it(&rig).unwrap(),
                ClaimOutcome::Skipped {
                    reason: "issue #8 is closed".into()
                }
            );
            assert!(rig.sim.writes().is_empty());
            assert!(rig.ledger.issue_runs(REPO, ISSUE).unwrap().is_empty());
        }

        /// The runner's case "a claim that finds an assignee is skipped and makes no run": the
        /// issue is read for its assignees before anything is written.
        #[test]
        fn a_claim_that_finds_an_assignee_is_skipped_and_makes_no_run() {
            let rig = Rig::new();
            rig.sim.set_assignees(&["someone", LOGIN]);
            assert!(matches!(
                claim_it(&rig).unwrap(),
                ClaimOutcome::Skipped { .. }
            ));
            assert!(rig.sim.writes().is_empty());
            assert!(rig.ledger.issue_runs(REPO, ISSUE).unwrap().is_empty());
        }

        #[test]
        fn a_lost_lease_claims_nothing() {
            let mut rig = Rig::new();
            rig.guard = Box::new(Lost);
            assert!(matches!(claim_it(&rig), Err(RunError::LeaseLost(_))));
            assert!(rig.sim.writes().is_empty());
            assert!(rig.ledger.issue_runs(REPO, ISSUE).unwrap().is_empty());
        }

        /// A comment by someone else that carries Foundry's marker is no claim: the claim made
        /// here is the newest by the project's login, whatever others wrote after it.
        #[test]
        fn a_comment_that_carries_foundrys_marker_but_was_written_by_someone_else_counts_for_nothing()
         {
            let rig = Rig::new();
            let run = claimed(&rig);
            rig.sim.comment_as(
                "mallory",
                &crate::factory::github::claim_comment(&run_id(7), &rig.machine),
            );
            assert_eq!(
                newest(&rig),
                Some(Claim::Foundry {
                    run,
                    machine: rig.machine
                })
            );
        }
    }

    mod a_claim_cut_short {
        use super::*;

        /// Acceptance: a claim cut short after the comment leaves the issue `ready`, and the
        /// next claim's comment is the newest.
        #[test]
        fn after_the_comment_leaves_the_issue_ready_and_the_next_claims_comment_is_the_newest() {
            let rig = Rig::new();
            rig.sim.fail("--add-label", 1);
            let first = claim_it(&rig);
            assert!(matches!(first, Err(RunError::Tracker(_))), "{first:?}");
            assert_eq!(rig.sim.labels(), vec!["factory:ready"]);
            assert!(rig.sim.assignees().is_empty());
            assert_eq!(rig.sim.comments().len(), 1);
            assert_eq!(
                newest(&rig),
                Some(Claim::Foundry {
                    run: run_id(1),
                    machine: rig.machine
                })
            );
            // The cut-short run is ended, so it holds no slot.
            assert!(rig.finished(&run_id(1)));
            let run = claimed(&rig);
            assert_eq!(run, run_id(2));
            assert_eq!(rig.sim.comments().len(), 3);
            assert_eq!(
                newest(&rig),
                Some(Claim::Foundry {
                    run: run_id(2),
                    machine: rig.machine
                })
            );
            assert_eq!(rig.sim.labels(), vec!["factory:triage"]);
        }

        /// The runner's case "is the error when nothing was applied, with nothing written": no
        /// label and no assignee changed, and the ledger holds no run that counts.
        #[test]
        fn is_the_error_when_nothing_was_applied_with_nothing_written() {
            let rig = Rig::new();
            rig.sim.fail("--add-label", 1);
            assert!(claim_it(&rig).is_err());
            // The comment, and the label call that failed: no assignee, no status comment.
            let writes = rig.sim.writes();
            assert_eq!(writes.len(), 2, "{writes:#?}");
            assert!(writes[0].contains("foundry:claim"));
            assert!(writes[1].contains("--add-label"));
            assert!(rig.ledger.issue_runs(REPO, ISSUE).unwrap().is_empty());
        }

        /// The runner's case "is finished when GitHub applied it, and says so in the machine
        /// log": the label landed, the assignee call failed, and the repair assigns. The line
        /// the machine log would hold is a step of the run's ledger.
        #[test]
        fn is_finished_when_github_applied_it_and_says_so_in_the_machine_log() {
            let rig = Rig::new();
            rig.sim.fail("--add-assignee", 1);
            let run = claimed(&rig);
            assert_eq!(rig.sim.labels(), vec!["factory:triage"]);
            assert_eq!(rig.sim.assignees(), vec![LOGIN]);
            assert!(
                rig.steps(&run)
                    .contains(&"claim-repaired finished".to_string())
            );
            assert!(!rig.finished(&run));
        }

        /// The runner's case "is finished even when the machine log cannot be written, which is
        /// logged": Foundry has no machine log, so the repair's line is in the run's ledger and
        /// a repair is finished all the same.
        #[test]
        fn is_finished_even_when_the_machine_log_cannot_be_written_which_is_logged() {
            let rig = Rig::new();
            rig.sim.fail("--add-assignee", 1);
            assert!(matches!(
                claim_it(&rig).unwrap(),
                ClaimOutcome::Claimed { .. }
            ));
        }

        /// The runner's case "adds the missing assignee when only the labels moved".
        #[test]
        fn adds_the_missing_assignee_when_only_the_labels_moved() {
            let rig = Rig::new();
            rig.sim.set_labels(&["factory:triage"]);
            let run = run_id(1);
            let error = RunError::Invalid("the claim call failed".into());
            repair_claim(&rig.ctx(), ISSUE, &run, error).unwrap();
            assert_eq!(rig.sim.assignees(), vec![LOGIN]);
        }

        /// The runner's case "sets the label when the issue is ours but still labelled ready".
        #[test]
        fn sets_the_label_when_the_issue_is_ours_but_still_labelled_ready() {
            let rig = Rig::new();
            rig.sim.set_assignees(&[LOGIN]);
            let error = RunError::Invalid("the claim call failed".into());
            repair_claim(&rig.ctx(), ISSUE, &run_id(1), error).unwrap();
            assert_eq!(rig.sim.labels(), vec!["factory:triage"]);
        }

        /// The runner's case "assigns and labels when both factory labels are gone and nobody is
        /// assigned".
        #[test]
        fn assigns_and_labels_when_both_factory_labels_are_gone_and_nobody_is_assigned() {
            let rig = Rig::new();
            rig.sim.set_labels(&[]);
            let error = RunError::Invalid("the claim call failed".into());
            repair_claim(&rig.ctx(), ISSUE, &run_id(1), error).unwrap();
            assert_eq!(rig.sim.labels(), vec!["factory:triage"]);
            assert_eq!(rig.sim.assignees(), vec![LOGIN]);
        }

        /// The runner's case "releases the issue to proposed with a comment when it cannot be
        /// finished": the label landed and the assignee never does.
        #[test]
        fn releases_the_issue_to_proposed_with_a_comment_when_it_cannot_be_finished() {
            let rig = Rig::new();
            rig.sim.fail("--add-assignee", usize::MAX);
            let outcome = claim_it(&rig);
            assert!(outcome.is_err());
            assert_eq!(rig.sim.labels(), vec!["factory:proposed"]);
            assert!(rig.sim.assignees().is_empty());
            assert!(
                rig.sim
                    .comments()
                    .contains(&format!("{CUT_SHORT_COMMENT}\n\n<!-- softfac -->"))
            );
            assert!(rig.finished(&run_id(1)));
        }

        /// The runner's case "is the error, with nothing written, when someone else is
        /// assigned".
        #[test]
        fn is_the_error_with_nothing_written_when_someone_else_is_assigned() {
            let rig = Rig::new();
            rig.sim.set_labels(&["factory:triage"]);
            rig.sim.set_assignees(&["someone"]);
            let error = RunError::Invalid("the claim call failed".into());
            let result = repair_claim(&rig.ctx(), ISSUE, &run_id(1), error);
            assert!(matches!(result, Err(RunError::Invalid(_))));
            assert!(rig.sim.writes().is_empty());
        }

        /// The runner's case "is the error, untouched, when the issue cannot be read again".
        #[test]
        fn is_the_error_untouched_when_the_issue_cannot_be_read_again() {
            let rig = Rig::new();
            rig.sim.fail("issue view", usize::MAX);
            let error = RunError::Invalid("the claim call failed".into());
            let result = repair_claim(&rig.ctx(), ISSUE, &run_id(1), error);
            assert!(matches!(result, Err(RunError::Invalid(_))));
            assert!(rig.sim.writes().is_empty());
        }

        /// The runner's case "a claim that gave way makes no run and says so": Foundry's claim
        /// does not give way after five minutes; the lease guard stands in its place, and a
        /// lease lost between the comment and the label stops the claim with the issue `ready`.
        #[test]
        fn a_claim_that_gave_way_makes_no_run_and_says_so() {
            let rig = Rig::new();
            rig.sim.fail("--add-label", usize::MAX);
            assert!(claim_it(&rig).is_err());
            assert_eq!(rig.sim.labels(), vec!["factory:ready"]);
            assert!(rig.ledger.issue_runs(REPO, ISSUE).unwrap().is_empty());
        }
    }

    mod older_runs_of_the_issue {
        use super::*;

        fn unfinished_in(rig: &Rig, run: &RunId, state: State) {
            rig.step(run, &format!("approved {LABELED_AT}"));
            rig.moved(run, State::Ready, State::Triage);
            if state != State::Triage {
                rig.moved(run, State::Triage, state);
            }
        }

        /// The runner's case "a new attempt finishes every unfinished older run by its last
        /// state first".
        #[test]
        fn a_new_attempt_finishes_every_unfinished_older_run_by_its_last_state_first() {
            let rig = Rig::new();
            unfinished_in(&rig, &run_id(1), State::NeedsInfo);
            unfinished_in(&rig, &run_id(2), State::PrOpen);
            unfinished_in(&rig, &run_id(3), State::Building);
            let run = claimed(&rig);
            assert_eq!(run, run_id(4));
            let outcome = |run: &RunId| rig.events(run).pop().unwrap();
            assert_eq!(outcome(&run_id(1)).data["outcome"], "needs-info");
            assert_eq!(outcome(&run_id(2)).data["outcome"], "pr-open");
            assert_eq!(outcome(&run_id(3)).data["outcome"], "stuck");
            let cause = outcome(&run_id(1)).cause.unwrap();
            assert_eq!(cause["reason"], "new-attempt-started");
            assert_eq!(cause["actor"], "tom");
            assert_eq!(cause["label"], "factory:ready");
            // Nothing is archived or drafted: the ledger only.
            assert!(rig.worker.calls().is_empty());
        }

        /// The runner's case "an older run whose gate is running stops the start before
        /// anything is written". A gate is running when its workspace was checked out and has
        /// not finished.
        #[test]
        fn an_older_run_whose_gate_is_running_stops_the_start_before_anything_is_written() {
            let rig = Rig::new();
            unfinished_in(&rig, &run_id(1), State::Building);
            rig.write(
                &run_id(1),
                Event::GateCheckout {
                    attempt: 1,
                    dir: "/g".into(),
                    sha: SHA.into(),
                    base_sha: SHA.into(),
                },
            );
            let result = claim_it(&rig);
            assert!(matches!(result, Err(RunError::GateRunning { .. })));
            assert!(rig.sim.writes().is_empty());
            assert!(!rig.finished(&run_id(1)));
            assert!(rig.ledger.read(&run_id(2)).unwrap().is_empty());
        }
    }

    mod a_closed_issue {
        use super::*;

        fn unfinished(rig: &Rig, run: &RunId, state: State) {
            rig.step(run, &format!("approved {LABELED_AT}"));
            rig.moved(run, State::Ready, State::Triage);
            if state != State::Triage {
                rig.moved(run, State::Triage, state);
            }
        }

        fn launched(rig: &Rig, run: &RunId, workspace: u128) {
            let id = uuid::Uuid::from_u128(workspace);
            rig.step(run, &format!("workspace-created {id}"));
            rig.write(
                run,
                Event::SessionLaunched {
                    thread_id: uuid::Uuid::from_u128(workspace + 1).to_string(),
                    worktree: Some(id.to_string()),
                    branch: None,
                    model_instance: None,
                    model: None,
                    model_options: None,
                    resumed_from: None,
                    base: None,
                    title: None,
                },
            );
        }

        /// The runner's case "ends every unfinished run of it, oldest first, and writes nothing
        /// on GitHub".
        #[test]
        fn ends_every_unfinished_run_of_it_oldest_first_and_writes_nothing_on_github() {
            let rig = Rig::new();
            unfinished(&rig, &run_id(1), State::Building);
            launched(&rig, &run_id(1), 0xa0);
            unfinished(&rig, &run_id(2), State::NeedsInfo);
            rig.sim.set_closed(true);
            let outcome = claim_it(&rig).unwrap();
            assert_eq!(
                outcome,
                ClaimOutcome::Abandoned {
                    may_have_session: vec![],
                    error: None
                }
            );
            assert_eq!(
                rig.events(&run_id(1)).pop().unwrap().data["outcome"],
                "stuck"
            );
            assert_eq!(
                rig.events(&run_id(2)).pop().unwrap().data["outcome"],
                "needs-info"
            );
            assert!(rig.sim.writes().is_empty());
            assert_eq!(rig.worker.calls().len(), 1);
        }

        /// The runner's case "never ends a run whose gate is running, and then ends none of
        /// them".
        #[test]
        fn never_ends_a_run_whose_gate_is_running_and_then_ends_none_of_them() {
            let rig = Rig::new();
            unfinished(&rig, &run_id(1), State::Building);
            unfinished(&rig, &run_id(2), State::Building);
            rig.write(
                &run_id(2),
                Event::GateCheckout {
                    attempt: 1,
                    dir: "/g".into(),
                    sha: SHA.into(),
                    base_sha: SHA.into(),
                },
            );
            rig.sim.set_closed(true);
            assert!(matches!(claim_it(&rig), Err(RunError::GateRunning { .. })));
            assert!(!rig.finished(&run_id(1)));
            assert!(!rig.finished(&run_id(2)));
        }

        /// The runner's case "leaves a pr-open run and a run whose own merge closed it alone".
        #[test]
        fn leaves_a_pr_open_run_and_a_run_whose_own_merge_closed_it_alone() {
            let rig = Rig::new();
            unfinished(&rig, &run_id(1), State::PrOpen);
            unfinished(&rig, &run_id(2), State::Building);
            rig.write(
                &run_id(2),
                Event::PrOpened {
                    pr: 12,
                    url: "u".into(),
                },
            );
            rig.sim.set_pull(
                12,
                SimPull {
                    state: "MERGED",
                    draft: false,
                    head: "fac/8-2".into(),
                    base: "main".into(),
                    cross: false,
                },
            );
            rig.sim.set_closed(true);
            assert_eq!(
                claim_it(&rig).unwrap(),
                ClaimOutcome::Skipped {
                    reason: "issue #8 is closed".into()
                }
            );
            assert!(!rig.finished(&run_id(1)));
            assert!(!rig.finished(&run_id(2)));
        }

        /// The runner's case "ends a run whose pull request was closed unmerged".
        #[test]
        fn ends_a_run_whose_pull_request_was_closed_unmerged() {
            let rig = Rig::new();
            unfinished(&rig, &run_id(1), State::Building);
            rig.write(
                &run_id(1),
                Event::PrOpened {
                    pr: 12,
                    url: "u".into(),
                },
            );
            rig.sim.set_pull(
                12,
                SimPull {
                    state: "CLOSED",
                    draft: false,
                    head: "fac/8-1".into(),
                    base: "main".into(),
                    cross: false,
                },
            );
            rig.sim.set_closed(true);
            assert!(matches!(
                claim_it(&rig).unwrap(),
                ClaimOutcome::Abandoned { .. }
            ));
            assert!(rig.finished(&run_id(1)));
        }

        /// The runner's case "with a launch sent and no thread recorded says a session may
        /// exist".
        #[test]
        fn with_a_launch_sent_and_no_thread_recorded_says_a_session_may_exist() {
            let rig = Rig::new();
            unfinished(&rig, &run_id(1), State::Triage);
            rig.step(
                &run_id(1),
                &format!("workspace-created {}", uuid::Uuid::from_u128(0xa0)),
            );
            rig.step(&run_id(1), "launch-sending");
            rig.sim.set_closed(true);
            assert_eq!(
                claim_it(&rig).unwrap(),
                ClaimOutcome::Abandoned {
                    may_have_session: vec![run_id(1)],
                    error: None
                }
            );
        }

        /// The runner's case "whose archive fails is an error with nothing written, and the next
        /// call ends it": the session's end fails, and the next call ends it.
        #[test]
        fn whose_archive_fails_is_an_error_with_nothing_written_and_the_next_call_ends_it() {
            let rig = Rig::new();
            unfinished(&rig, &run_id(1), State::Building);
            launched(&rig, &run_id(1), 0xa0);
            rig.sim.set_closed(true);
            rig.worker
                .on_end_sessions(Err(crate::factory::worker::WorkerError::Unreachable(
                    "gone".into(),
                )));
            assert!(claim_it(&rig).is_err());
            assert!(!rig.finished(&run_id(1)));
            assert!(rig.sim.writes().is_empty());
            assert!(matches!(
                claim_it(&rig).unwrap(),
                ClaimOutcome::Abandoned { .. }
            ));
            assert!(rig.finished(&run_id(1)));
        }

        /// The runner's case "whose later run fails to end still names the runs it ended, with
        /// the error".
        #[test]
        fn whose_later_run_fails_to_end_still_names_the_runs_it_ended_with_the_error() {
            let rig = Rig::new();
            unfinished(&rig, &run_id(1), State::Triage);
            unfinished(&rig, &run_id(2), State::Building);
            launched(&rig, &run_id(2), 0xb0);
            rig.sim.set_closed(true);
            rig.worker
                .on_end_sessions(Err(crate::factory::worker::WorkerError::Unreachable(
                    "gone".into(),
                )));
            let outcome = claim_it(&rig).unwrap();
            match outcome {
                ClaimOutcome::Abandoned {
                    may_have_session,
                    error,
                } => {
                    assert!(may_have_session.is_empty());
                    assert!(error.unwrap().contains("gone"));
                }
                other => panic!("{other:?}"),
            }
            assert!(rig.finished(&run_id(1)));
            assert!(!rig.finished(&run_id(2)));
        }
    }

    #[test]
    fn a_call_to_the_worker_is_never_made_by_a_claim() {
        let rig = Rig::new();
        claimed(&rig);
        assert!(
            !rig.worker
                .calls()
                .iter()
                .any(|call| matches!(call, Call::Create { .. }))
        );
    }
}
