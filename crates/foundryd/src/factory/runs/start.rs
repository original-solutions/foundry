//! The claim and the launch of a run.
//!
//! Ported from the runner's `src/runner/start.ts`, `src/runner/launchTarget.ts` and
//! `src/claims.ts`. [`claim`] takes a `ready` issue to `factory:triage` in the order the spec
//! gives: Foundry's claim comment first, then the label, the assignee and the status comment,
//! with the run's `approved` and `claimed` lines written before any of it. [`advance`] is one
//! pass of what follows: it asks for the build workspace, looks at its setup terminal on later
//! passes, checks the text since the approval, starts the session with the launch message,
//! moves the label to `building` and asks for the launch again by count when the session is
//! idle a pass later. No pass waits on a worker: a step records what it is about to do, does
//! it, and a pass that finds more to wait for says so ([`StartStep::Waiting`]) and returns.
//!
//! Where it differs from the runner:
//!
//! - The runner launches in one call and checks the issue text after it. Foundry decides what
//!   the launch continues from and the base before the workspace is asked for (it is made on
//!   that branch), asks for the workspace, waits for its setup, checks the text since the
//!   approval the run recorded and only then starts the session: a text changed since means no
//!   session was started. Who applied the label is not read again: the claim holds the approval,
//!   and its own removal of `factory:ready` is no approval taken back.
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

use foundry_proto::factory::{RunEvent, RunId};
use foundry_proto::ids::{MachineId, SessionId, WorkspaceId};
use foundry_proto::record::{Record, Workspace};
use foundry_proto::state::{SessionState, TerminalState};
use serde::Deserialize;

use super::progress::{
    APPROVAL_RECHECKED, APPROVED, LAUNCH_BASE, LAUNCH_ENDING_SESSIONS, LAUNCH_MODEL,
    LAUNCH_PULL_REQUEST, LAUNCH_RESENDING, LAUNCH_RESUMING, LAUNCH_SENDING, LAUNCH_SENDING_RESUMED,
    Seen, TIME_LIMIT, UNCONFIRMED_CODE, WORKSPACE_CREATED, WORKSPACE_REQUESTED, build_name,
    read_progress,
};
use super::{
    Closing, Progress, RunContext, RunError, Stop, abandon, as_posted, end_own_merge,
    finish_building, go_stuck, moved_on_by_closing, newest_own, progress_from, session_id,
    stop_session, workspace_id,
};
use crate::factory::github::read::{
    gh_json, is_branch_path, is_plain_ref_name, is_safe_branch_name, is_utc_second,
    issue_query_args,
};
use crate::factory::github::{
    Claim, IssueDetail, OpenPullRequest, OtherKind, Tracker, TrackerError, newest_claim,
};
use crate::factory::ledger::events::{
    Cause, CauseStep, Event, ModelOption, OptionValue, Outcome, ResumedFrom, names,
};
use crate::factory::ledger::read::{field, text};
use crate::factory::lifecycle::approval::{Approval, Refusal, TextCheck, refusal_reason};
use crate::factory::lifecycle::base;
use crate::factory::lifecycle::{
    Action, Event as LifecycleEvent, InvalidTransition, State, transition,
};
use crate::factory::messages::{self, CUT_SHORT_COMMENT, Message, ResumeFrom};
use crate::factory::text;
use crate::factory::worker::{CreateWorkspace, WorkerError};
use crate::sessions::factory::{BODY_PATH, FactoryStart, SendRefused};
use crate::workspaces::{Checkout, FactoryTag, Setup};

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
    let Some(newest) = newest_own(ctx, issue)? else {
        return Ok(false);
    };
    if newest.body != as_posted(ctx, said) {
        return Ok(false);
    }
    let created_at = newest.created_at;
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

/// The query for the open pull requests that close an issue, as GitHub links them: onto the
/// default branch only, so a pull request onto another base is found by its branch.
pub const CLOSING_PRS_QUERY: &str = "query($owner: String!, $name: String!, $number: Int!) { repository(owner: $owner, name: $name) { issue(number: $number) { closedByPullRequestsReferences(first: 50, includeClosedPrs: false) { nodes { number url state headRefName baseRefName isCrossRepository author { __typename login } } } } } }";

/// The open pull requests GitHub links to `issue` as closing it, one call. A bot's login is
/// written `app/<login>`, as `gh pr list` writes it.
pub fn open_pull_requests_closing<G: Tracker + ?Sized>(
    tracker: &G,
    repo: &str,
    issue: u64,
) -> Result<Vec<OpenPullRequest>, TrackerError> {
    #[derive(Deserialize)]
    struct Answer {
        data: Data,
    }
    #[derive(Deserialize)]
    struct Data {
        repository: Repository,
    }
    #[derive(Deserialize)]
    struct Repository {
        issue: ClosingIssue,
    }
    #[derive(Deserialize)]
    struct ClosingIssue {
        #[serde(rename = "closedByPullRequestsReferences")]
        closed_by: Nodes,
    }
    #[derive(Deserialize)]
    struct Nodes {
        nodes: Vec<Listed>,
    }
    #[derive(Deserialize)]
    struct Listed {
        number: u64,
        url: String,
        state: String,
        #[serde(rename = "headRefName")]
        head_ref_name: String,
        #[serde(rename = "baseRefName")]
        base_ref_name: String,
        #[serde(rename = "isCrossRepository")]
        is_cross_repository: bool,
        #[serde(default)]
        author: Option<Author>,
    }
    #[derive(Deserialize)]
    struct Author {
        login: String,
        #[serde(rename = "__typename", default)]
        typename: Option<String>,
    }
    let args = issue_query_args(repo, CLOSING_PRS_QUERY, issue)?;
    let answer: Answer = gh_json(
        tracker,
        &args,
        "a GraphQL response with the issue's closing pull requests",
    )?;
    Ok(answer
        .data
        .repository
        .issue
        .closed_by
        .nodes
        .into_iter()
        .filter(|node| node.state == "OPEN")
        .map(|node| OpenPullRequest {
            number: node.number,
            url: node.url,
            head_ref: node.head_ref_name,
            base_ref: node.base_ref_name,
            cross_repository: node.is_cross_repository,
            author: node
                .author
                .filter(|author| !author.login.is_empty())
                .map(|author| {
                    if author.typename.as_deref() == Some("Bot") {
                        format!("app/{}", author.login)
                    } else {
                        author.login
                    }
                }),
        })
        .collect())
}

/// Whether a pull request opened by `author` is the run's own: its author is the project's
/// login, matched without case. The app a cloud workspace opens pull requests as has a login
/// no user has (it holds a `/`), and Foundry has no cloud launch, so it is never the run's own.
pub fn is_own_pull(author: Option<&str>, login: &str) -> bool {
    author.is_some_and(|author| author.eq_ignore_ascii_case(login))
}

/// Whether a session can be launched onto a pull request and told to push to its branch: it is
/// in this repo, merges into `base`, and its head is a name a command can be handed.
pub fn is_resumable_pull(
    cross_repository: bool,
    base_ref: &str,
    head_ref: &str,
    base: &str,
) -> bool {
    !cross_repository && base_ref == base && is_safe_branch_name(head_ref)
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
    /// A withdraw the ledger shows begun was finished: the run ended as the runner refuses an
    /// unapproved issue.
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

/// One pass of the start of `run`. See the module's words for what a pass does; it never waits
/// on a worker.
pub fn advance(ctx: &RunContext<'_>, run: &RunId) -> Result<StartStep, RunError> {
    let issue = run.issue();
    let repo = ctx.repo();
    let events = ctx.ledger.read(run)?;
    let skipped = |reason: String| Ok(StartStep::Skipped { reason });
    if events.is_empty() {
        // A claim comment names a run no event records: a coordinator took over from one that
        // claimed and died before it wrote the run's first line.
        match takeover(ctx, run)? {
            None => {}
            Some(step) => return Ok(step),
        }
    }
    let mut seen = ctx.seen(run)?;
    if seen.finished {
        return skipped(format!("run {run} is finished"));
    }
    if matches!(
        seen.state,
        Some(State::PrOpen | State::NeedsInfo | State::Stuck)
    ) {
        return skipped(format!(
            "run {run} is in state {}, which another step owns",
            seen.state.map_or("none", State::name)
        ));
    }
    let detail = ctx.tracker.issue(repo, issue)?;
    if detail.closed {
        return match moved_on_by_closing(ctx, &seen, true)? {
            Closing::OwnMerge(pull) => {
                end_own_merge(ctx, run, &seen, CauseStep::Start, &pull)?;
                Ok(StartStep::Ended)
            }
            _ => {
                abandon(ctx, run, &seen, CauseStep::Start)?;
                Ok(StartStep::Abandoned)
            }
        };
    }
    // An issue put back to `ready` is the claim step's, which finishes its older runs: a path
    // the ledger shows begun is left alone too, as the runner leaves it, and writes nothing.
    if detail.state == Some(State::Ready) {
        return skipped(format!("issue #{issue} is ready again"));
    }
    // A path the ledger shows begun is finished whatever the issue's label and assignee now
    // are: the label and the assignee move before the ledger's own state line does, so a path
    // cut short leaves the issue where this pass would otherwise skip it.
    if let Some(step) = finish_begun_path(ctx, run, &seen)? {
        return Ok(step);
    }
    if !matches!(detail.state, Some(State::Triage | State::Building)) {
        return skipped(format!("issue #{issue} is not in triage or building"));
    }
    let ours =
        detail.assignees.len() == 1 && detail.assignees[0].eq_ignore_ascii_case(&ctx.project.login);
    // A claim cut short after its label and before its assignee: the login is assigned now, the
    // step recorded first.
    let unassigned_claim = seen.claimed
        && seen.state.is_none()
        && detail.state == Some(State::Triage)
        && detail.assignees.is_empty();
    if unassigned_claim {
        ctx.held()?;
        ctx.record_step(
            run,
            "claim-assigning",
            &Cause::new(CauseStep::Start, "claim-resumed"),
        )?;
        ctx.assign(issue)?;
    } else if !ours {
        return skipped(format!(
            "issue #{issue} is not assigned to {} alone",
            ctx.project.login
        ));
    }
    let cause = Cause::new(CauseStep::Start, "claimed");
    if seen.state.is_none() {
        ctx.record(
            run,
            Event::StateChanged {
                from: State::Ready,
                to: State::Triage,
            },
            &cause,
        )?;
        seen = ctx.seen(run)?;
    }
    let mut at = progress_from(run, CauseStep::Start, &seen, State::Triage);
    // The record is written before any launch: a run with no record of what approved it fails
    // closed, and a session it has is ended by the stuck path.
    if seen.approved_at.is_none() {
        go_stuck(
            ctx,
            issue,
            &mut at,
            UNCONFIRMED_CODE,
            Stop::new(messages::CANNOT_CONFIRM),
            None,
        )?;
        return Ok(StartStep::Stuck {
            reason: messages::CANNOT_CONFIRM.into(),
        });
    }
    if seen.thread_id.is_some() {
        return launched(ctx, run, &seen, &mut at);
    }
    build(ctx, run, &seen, &detail, &mut at)
}

/// Finishes the stuck path or the withdraw the run's ledger shows begun, if it does. `None`
/// when the run has begun neither. A decision to give up that is in the ledger stands.
fn finish_begun_path(
    ctx: &RunContext<'_>,
    run: &RunId,
    seen: &Seen,
) -> Result<Option<StartStep>, RunError> {
    let issue = run.issue();
    let mut at = progress_from(run, CauseStep::Start, seen, State::Triage);
    if let Some(reason) = seen.stuck.clone() {
        go_stuck(
            ctx,
            issue,
            &mut at,
            "stuck-resumed",
            Stop::new(reason.clone()),
            None,
        )?;
        return Ok(Some(StartStep::Stuck { reason }));
    }
    // The decision to give up is in the ledger and its comment may be on the issue: the path
    // goes on with the words it recorded, so the comment is found, not posted twice.
    if let Some(raw) = &seen.stuck_stop {
        let decision: super::StuckStop = serde_json::from_str(raw)
            .map_err(|error| RunError::Invalid(format!("the stuck decision of {run}: {error}")))?;
        let reason = decision.stop.reason.clone();
        let mut stop = decision.stop;
        stop.detail = seen.stuck_detail.clone();
        // The decision holds no setup output; the workspace is kept, so the tail is read again
        // as the first pass read it. A failed read posts the reason alone.
        if decision.code == "setup-failed"
            && let Some(machine) = seen
                .machine
                .as_deref()
                .and_then(|machine| uuid::Uuid::parse_str(machine).ok())
                .map(MachineId)
            && let Some(record) = find_workspace(
                ctx,
                &build_name(run),
                machine,
                seen.workspace.as_deref().map(workspace_id).transpose()?,
            )
            && let Some(terminal) = record.body.setup
            && let Ok(tail) = ctx.tail(terminal)
        {
            stop.session_text = Some(messages::prepare_output_tail(&tail));
            stop.recorded_text = Some(text::sanitize(&tail, true));
        }
        go_stuck(ctx, issue, &mut at, &decision.code, stop, None)?;
        return Ok(Some(StartStep::Stuck { reason }));
    }
    if seen.withdrawn {
        let workspace = seen
            .workspace
            .as_deref()
            .map(workspace_id)
            .transpose()?
            .ok_or_else(|| RunError::Invalid(format!("run {run} withdrew with no workspace")))?;
        // What the approval says now: the explanation is posted once, and only when it still
        // holds.
        let refusal = match ctx
            .tracker
            .approval(ctx.repo(), issue, &ctx.config.instructors)?
        {
            Approval::Refused(refusal) => Some(refusal),
            Approval::Approved { .. } => None,
        };
        return withdraw(ctx, run, &at, refusal.as_ref(), workspace).map(Some);
    }
    Ok(None)
}

/// What a run with no events learns from the claim comment that names it: the machine the
/// build is asked of, and the approval, recorded before anything is done. `None` goes on with
/// the run; a step is a pass that is over.
fn takeover(ctx: &RunContext<'_>, run: &RunId) -> Result<Option<StartStep>, RunError> {
    let issue = run.issue();
    let repo = ctx.repo();
    let skipped = |reason: String| Ok(Some(StartStep::Skipped { reason }));
    let detail = ctx.tracker.issue(repo, issue)?;
    if detail.closed {
        return skipped(format!(
            "issue #{issue} is closed and run {run} has no events"
        ));
    }
    if detail.state == Some(State::Ready) {
        // A claim cut short before its label: the claim step takes the issue again.
        return skipped(format!("issue #{issue} is ready: the claim was cut short"));
    }
    let comments = ctx.tracker.issue_comments(repo, issue)?;
    let machine = match newest_claim(&comments, &ctx.project.login) {
        Some(Claim::Foundry {
            run: named,
            machine,
        }) if named == *run => machine,
        Some(Claim::Foundry { run: named, .. }) => {
            return skipped(format!(
                "the issue's newest claim names run {named}, not {run}"
            ));
        }
        Some(Claim::Foreign { .. }) => {
            return skipped("the issue's newest claim is another factory's".into());
        }
        None => return skipped(format!("no claim comment names run {run}")),
    };
    if let Approval::Approved {
        labeled_at,
        labeled_by,
    } = ctx
        .tracker
        .claimed_approval(repo, issue, &ctx.project.login, &ctx.config.instructors)?
    {
        record_approval(ctx, run, &labeled_at, Some(&labeled_by), CauseStep::Start)?;
    }
    ctx.record(
        run,
        Event::Claimed {
            machine: machine.to_string(),
            github_user: ctx.project.login.clone(),
        },
        &Cause::new(CauseStep::Start, "claim-found"),
    )?;
    Ok(None)
}

/// What a launch continues from.
#[derive(Clone, Debug)]
enum Resumed {
    /// The branch of an earlier attempt that hit its time limit or was relaunched.
    Earlier {
        run: RunId,
        branch: String,
        after: After,
    },
    /// The open pull request an earlier attempt left, on its branch.
    ExistingPr {
        run: RunId,
        branch: String,
        pr_url: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum After {
    TimeLimit,
    Interrupted,
}

impl Resumed {
    fn branch(&self) -> &str {
        match self {
            Resumed::Earlier { branch, .. } | Resumed::ExistingPr { branch, .. } => branch,
        }
    }

    fn pr_url(&self) -> Option<&str> {
        match self {
            Resumed::Earlier { .. } => None,
            Resumed::ExistingPr { pr_url, .. } => Some(pr_url),
        }
    }

    /// The run a `session_launched` line names as `resumed_from`.
    fn run(&self) -> &RunId {
        match self {
            Resumed::Earlier { run, .. } | Resumed::ExistingPr { run, .. } => run,
        }
    }

    fn resume_from(&self) -> ResumeFrom {
        match self {
            Resumed::Earlier {
                run,
                branch,
                after: After::TimeLimit,
            } => ResumeFrom::TimeLimit {
                run: run.clone(),
                branch: branch.clone(),
            },
            Resumed::Earlier {
                run,
                branch,
                after: After::Interrupted,
            } => ResumeFrom::Interrupted {
                run: run.clone(),
                branch: branch.clone(),
            },
            Resumed::ExistingPr { branch, pr_url, .. } => ResumeFrom::ExistingPr {
                branch: branch.clone(),
                pr_url: pr_url.clone(),
            },
        }
    }
}

/// The launch message of a run on `base`, continuing `resumed` when it does.
fn launch_message(
    ctx: &RunContext<'_>,
    run: &RunId,
    base: &str,
    resumed: Option<&Resumed>,
) -> Message {
    let named = (base != ctx.config.base).then_some(base);
    match resumed {
        None => messages::launch(
            ctx.repo(),
            run.issue(),
            &ctx.project.login,
            named,
            BODY_PATH,
        ),
        Some(resumed) => messages::resumed(
            ctx.repo(),
            run.issue(),
            &ctx.project.login,
            &resumed.resume_from(),
            named,
            BODY_PATH,
        ),
    }
}

/// What the ledger recorded a launch was decided on: the base, and what it continues from.
fn recorded_decision(
    ctx: &RunContext<'_>,
    run: &RunId,
    seen: &Seen,
) -> Result<(String, Option<Resumed>), RunError> {
    let base = seen
        .base
        .clone()
        .or_else(|| seen.launch_base.clone())
        .unwrap_or_else(|| ctx.config.base.clone());
    let resumed = match &seen.launch_resumed_from {
        Some(branch) => Some(recorded_resume(
            ctx,
            run,
            branch,
            seen.launch_resumed_pr.as_deref(),
        )?),
        None => None,
    };
    Ok((base, resumed))
}

/// The resume a `launch-resuming <branch>` step decided, for a launch asked for again. The words
/// of the first message are the same as the first's: what the earlier attempt ended with is in
/// its ledger, which does not change, and a pull request decided with the branch is in the
/// `launch-pull-request` step before it.
fn recorded_resume(
    ctx: &RunContext<'_>,
    run: &RunId,
    branch: &str,
    pr_url: Option<&str>,
) -> Result<Resumed, RunError> {
    if let Some(pr_url) = pr_url {
        return Ok(Resumed::ExistingPr {
            run: run.clone(),
            branch: branch.into(),
            pr_url: pr_url.into(),
        });
    }
    let previous = previous_run(ctx, run)?;
    let after = previous
        .as_ref()
        .and_then(|(_, events)| continuation(events))
        .map_or(After::TimeLimit, |found| found.after);
    let earlier = match previous {
        Some((id, _)) => id,
        None => RunId::new(
            run.repo(),
            run.issue(),
            run.attempt().saturating_sub(1).max(1),
        )
        .map_err(RunError::Invalid)?,
    };
    Ok(Resumed::Earlier {
        run: earlier,
        branch: branch.into(),
        after,
    })
}

/// The run before `run` and its events; `None` for a first attempt or an earlier run with no
/// ledger.
fn previous_run(
    ctx: &RunContext<'_>,
    run: &RunId,
) -> Result<Option<(RunId, Vec<RunEvent>)>, RunError> {
    if run.attempt() < 2 {
        return Ok(None);
    }
    let id = RunId::new(run.repo(), run.issue(), run.attempt() - 1).map_err(RunError::Invalid)?;
    let events = ctx.ledger.read(&id)?;
    Ok((!events.is_empty()).then_some((id, events)))
}

/// The runs of the issue before `run` that have a ledger, oldest first, with their events.
fn earlier_runs(
    ctx: &RunContext<'_>,
    run: &RunId,
) -> Result<Vec<(RunId, Vec<RunEvent>)>, RunError> {
    let mut runs = Vec::new();
    for attempt in 1..run.attempt() {
        let id = RunId::new(run.repo(), run.issue(), attempt).map_err(RunError::Invalid)?;
        let events = ctx.ledger.read(&id)?;
        if !events.is_empty() {
            runs.push((id, events));
        }
    }
    Ok(runs)
}

/// What the attempt before the new one left to continue from.
struct Continuation {
    branches: Vec<String>,
    after: After,
}

/// What the attempt before the new one left to continue from: its session's branch, then its own
/// `resumed_from` for when origin lacks the first (a resumed session pushes there). `None`
/// unless it hit its time limit (which wins) or was relaunched, and after `needs-info`: its
/// questions end it. A time-limit branch is the `session_launched` line's, else the `stuck`
/// line's.
fn continuation(events: &[RunEvent]) -> Option<Continuation> {
    let needs_info = events.iter().any(|event| {
        event.event == names::FINISHED
            && field(&event.data, "outcome")
                .and_then(|v| serde_json::from_value::<Outcome>(v.clone()).ok())
                == Some(Outcome::NeedsInfo)
    });
    if needs_info {
        return None;
    }
    let seen = read_progress(events);
    let with_resumed = |branch: Option<String>, after: After| {
        let mut branches: Vec<String> = Vec::new();
        for branch in [branch, seen.resumed_from.clone()].into_iter().flatten() {
            if !branches.contains(&branch) {
                branches.push(branch);
            }
        }
        (!branches.is_empty()).then_some(Continuation { branches, after })
    };
    let branch_on = |name: &str, event: &RunEvent| -> Option<String> {
        (event.event == name)
            .then(|| text(&event.data, "branch").map(str::to_owned))
            .flatten()
    };
    let time_limit = events.iter().any(|event| {
        event.event == names::STUCK
            && event
                .cause
                .as_ref()
                .and_then(|cause| cause.get("reason"))
                .and_then(|reason| reason.as_str())
                == Some(TIME_LIMIT)
    });
    if time_limit {
        let launched = events
            .iter()
            .find_map(|event| branch_on(names::SESSION_LAUNCHED, event));
        let recorded = events
            .iter()
            .rev()
            .find_map(|event| branch_on(names::STUCK, event));
        return with_resumed(launched.or(recorded), After::TimeLimit);
    }
    let relaunch = seen.interruption.as_ref()?;
    if !relaunch.relaunch {
        return None;
    }
    with_resumed(relaunch.branch.clone(), After::Interrupted)
}

/// What the new attempt `run` continues from. First the issue's open pull request, if an earlier
/// attempt left one ([`existing_pull_request`]): it is the work, and a start from `base` beside
/// it would open a second. Then the branch of an attempt that hit its time limit or was
/// relaunched ([`continuation`]). `None` is a plain start from `base`. `origin` is the only
/// source of the work.
fn resume_from(ctx: &RunContext<'_>, run: &RunId, base: &str) -> Result<Option<Resumed>, RunError> {
    let issue = run.issue();
    let earlier = earlier_runs(ctx, run)?;
    if let Some((branch, pr_url)) = existing_pull_request(ctx, &earlier, issue, base)? {
        return Ok(Some(Resumed::ExistingPr {
            run: run.clone(),
            branch,
            pr_url,
        }));
    }
    let Some((previous, events)) = earlier
        .iter()
        .find(|(id, _)| id.attempt() + 1 == run.attempt())
    else {
        return Ok(None);
    };
    let Some(found) = continuation(events) else {
        return Ok(None);
    };
    // The branch goes to GitHub's ref path, to git as a ref, and into the `git push` the
    // launch message tells the session to run: only a plain branch name is resumed from.
    let branches: Vec<&String> = found
        .branches
        .iter()
        .filter(|branch| is_safe_branch_name(branch))
        .collect();
    if branches.is_empty() {
        return Ok(None);
    }
    let Some(approved_at) = read_progress(events).approved_at else {
        return Ok(None);
    };
    if ctx
        .tracker
        .text_edited_since(ctx.repo(), issue, &approved_at)?
    {
        return Ok(None);
    }
    for branch in branches {
        match ctx.tracker.branch_tip(ctx.repo(), branch) {
            Ok(_) => {
                return Ok(Some(Resumed::Earlier {
                    run: previous.clone(),
                    branch: branch.clone(),
                    after: found.after,
                }));
            }
            Err(TrackerError::NotFound { .. }) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(None)
}

/// The open pull request an earlier attempt of the issue left, if any, and its branch. The
/// ledger first (newest `pr_opened`, then `pr_left`, of each earlier run) when GitHub still has
/// it open in this repo onto `base` on a branch a session can push to ([`is_resumable_pull`]).
/// Then the newest open one by the project's login that closes the issue, which GitHub links
/// only onto the default.
fn existing_pull_request(
    ctx: &RunContext<'_>,
    earlier: &[(RunId, Vec<RunEvent>)],
    issue: u64,
    base: &str,
) -> Result<Option<(String, String)>, RunError> {
    for (_, events) in earlier.iter().rev() {
        let seen = read_progress(events);
        // The one the session reported first, then the one a stop found it had left.
        for recorded in [seen.last_pr.as_ref(), seen.left.last()]
            .into_iter()
            .flatten()
        {
            if let Some(pull) = ctx.tracker.pull_request(ctx.repo(), recorded.pr)?
                && pull.open
                && is_resumable_pull(pull.cross_repository, &pull.base_ref, &pull.head_ref, base)
            {
                return Ok(Some((pull.head_ref, recorded.url.clone())));
            }
        }
    }
    let newest = open_pull_requests_closing(ctx.tracker, ctx.repo(), issue)?
        .into_iter()
        .filter(|pull| {
            is_own_pull(pull.author.as_deref(), &ctx.project.login)
                && is_resumable_pull(pull.cross_repository, &pull.base_ref, &pull.head_ref, base)
        })
        .max_by_key(|pull| pull.number);
    Ok(newest.map(|pull| (pull.head_ref, pull.url)))
}

/// The branch the run lands on: the issue's `## Base`, else `factory.base`. A named branch must
/// be a plain ref name and be on `origin` (`branch_tip`); either failing is a refusal with its
/// public reason. A lookup that fails in any other way is an error, and the next pass asks
/// again.
fn run_base(
    ctx: &RunContext<'_>,
    detail: &IssueDetail,
) -> Result<Result<String, String>, RunError> {
    let Some(named) = base::of(&detail.body) else {
        return Ok(Ok(ctx.config.base.clone()));
    };
    if named == ctx.config.base {
        return Ok(Ok(named));
    }
    if !is_plain_ref_name(&named) || !is_branch_path(&named) {
        return Ok(Err(messages::base_not_a_branch(&named)));
    }
    match ctx.tracker.branch_tip(ctx.repo(), &named) {
        Ok(_) => Ok(Ok(named)),
        // A name the tracker refuses is not a branch name either, whatever rule refused it.
        Err(TrackerError::Other {
            kind: OtherKind::InvalidArgument,
            ..
        }) => Ok(Err(messages::base_not_a_branch(&named))),
        Err(TrackerError::NotFound { .. }) => Ok(Err(messages::base_not_on_origin(&named))),
        Err(error) => Err(error.into()),
    }
}

/// The workspace record of the run's build, as the fleet shows it: `id` when the worker
/// answered, else the one named so, on `machine` first.
fn find_workspace(
    ctx: &RunContext<'_>,
    name: &str,
    machine: MachineId,
    id: Option<WorkspaceId>,
) -> Option<Record<Workspace>> {
    let named = ctx.view.workspaces_named(ctx.project.id, name);
    match id {
        Some(id) => named.into_iter().find(|record| record.body.id == id),
        None => named
            .iter()
            .find(|record| record.owner == machine)
            .or(named.first())
            .copied(),
    }
    .cloned()
}

/// Where the run's build stands until its session starts: asks for the workspace, waits for its
/// record and its setup, checks the text since the approval the run recorded, and starts the
/// session.
fn build(
    ctx: &RunContext<'_>,
    run: &RunId,
    seen: &Seen,
    detail: &IssueDetail,
    at: &mut Progress,
) -> Result<StartStep, RunError> {
    let issue = run.issue();
    let machine = seen
        .machine
        .as_deref()
        .and_then(|machine| uuid::Uuid::parse_str(machine).ok())
        .map(MachineId)
        .ok_or_else(|| {
            RunError::Invalid(format!("run {run} has no machine its build is asked of"))
        })?;
    let name = build_name(run);
    let wanted = seen.workspace.as_deref().map(workspace_id).transpose()?;
    let found = find_workspace(ctx, &name, machine, wanted);
    let cause = Cause::new(CauseStep::Start, "launch-decided");
    // What the launch is decided on, once, and recorded before the workspace is asked for.
    let (base, resumed) = if seen.workspace_requested.is_some() {
        recorded_decision(ctx, run, seen)?
    } else if let Some(record) = &found {
        // A workspace of the run's name that no step records: it was made by a coordinator
        // that died, as a plain start from its own base.
        let base = record.body.base.clone();
        record_decision(ctx, run, &base, None, &name, &cause)?;
        (base, None)
    } else {
        let base = match run_base(ctx, detail)? {
            Ok(base) => base,
            Err(reason) => {
                go_stuck(
                    ctx,
                    issue,
                    at,
                    "launch-failed",
                    Stop::new(reason.clone()),
                    None,
                )?;
                return Ok(StartStep::Stuck { reason });
            }
        };
        let resumed = resume_from(ctx, run, &base)?;
        record_decision(ctx, run, &base, resumed.as_ref(), &name, &cause)?;
        (base, resumed)
    };
    let record = match (&seen.workspace, found) {
        (Some(_), Some(record)) => record,
        // The worker answered, and the fleet does not show the record yet: wait, never ask again.
        (Some(_), None) => return Ok(StartStep::Waiting(Wait::Workspace)),
        (None, Some(record)) if record.body.setup.is_some() => {
            ctx.record_step(
                run,
                &format!("{WORKSPACE_CREATED}{}", record.body.id),
                &Cause::new(CauseStep::Start, "workspace-found"),
            )?;
            record
        }
        // A record with no setup terminal is no proof there is no setup: the worker writes the
        // record first and starts the setup after, and a create cut short between leaves one
        // with none. The create is asked again: it answers with the first and starts a setup
        // that is due. The record is taken as having no setup once it has answered.
        (None, found) => {
            let checkout = match &resumed {
                Some(resumed) => Checkout::Branch {
                    name: resumed.branch().to_owned(),
                },
                None => Checkout::NewBranch {
                    base: base.clone(),
                    branch: Some(name.clone()),
                },
            };
            let request = CreateWorkspace {
                project: ctx.project.id,
                name: name.clone(),
                checkout,
                setup: Setup::Own,
                tag: FactoryTag {
                    project: ctx.project.id,
                    run: run.to_string(),
                },
            };
            let cause = if found.is_some() {
                "workspace-found"
            } else {
                "workspace-created"
            };
            // A create that fails with no answer is asked again next pass, and the worker's
            // create answers with the first by tag and name.
            let id = ctx.create(machine, request)?;
            ctx.record_step(
                run,
                &format!("{WORKSPACE_CREATED}{id}"),
                &Cause::new(CauseStep::Start, cause),
            )?;
            return Ok(StartStep::Waiting(Wait::Workspace));
        }
    };
    let workspace = record.body.id;
    at.worktree = Some(workspace.to_string());
    // Its setup terminal: waited for while it runs, the run's end when it failed.
    if let Some(terminal) = record.body.setup {
        match ctx.view.terminal(terminal) {
            Some((TerminalState::Exited, Some(0))) => {}
            // An exit with no code (the holder is gone) is no success either.
            Some((TerminalState::Exited, _)) => {
                // A machine that cannot be reached is asked again; one that answered it has no
                // tail gives the reason alone, which ends the run.
                let stop = match ctx.tail(terminal) {
                    Ok(tail) => Stop {
                        reason: SETUP_FAILED.into(),
                        session_text: Some(messages::prepare_output_tail(&tail)),
                        recorded_text: Some(text::sanitize(&tail, true)),
                        ..Stop::default()
                    },
                    Err(RunError::Worker(WorkerError::Failed(_))) => Stop::new(SETUP_FAILED),
                    Err(error) => return Err(error),
                };
                go_stuck(ctx, issue, at, "setup-failed", stop, None)?;
                return Ok(StartStep::Stuck {
                    reason: SETUP_FAILED.into(),
                });
            }
            _ => return Ok(StartStep::Waiting(Wait::Setup)),
        }
    }
    // A factory session the fleet already shows in the workspace (one with a turn record: a
    // person's session there is no launch of ours) for a launch that may have gone out:
    // adopted, never started twice.
    if let Some(session) = ctx.view.session_of(workspace)
        && ctx.view.session_turn(session.body.id).is_some()
    {
        let model = seen
            .launch_model
            .clone()
            .unwrap_or_else(|| ctx.settings.fleet.model.clone());
        record_launched(
            ctx,
            run,
            detail,
            &record,
            session.body.id,
            &base,
            resumed.as_ref(),
            &model,
        )?;
        at.thread_id = Some(session.body.id.to_string());
        finish_building(ctx, issue, at)?;
        return Ok(StartStep::Started {
            session: session.body.id,
        });
    }
    // Before the session starts, on every pass that is about to start it: the text since the
    // approval the run recorded, as the runner checks it. The claim holds the approval, and the
    // claim's own removal of `factory:ready` is no approval taken back, so who holds the label
    // is not read again here (issue #164, answer 2). A start retried a pass later is checked
    // again.
    let Some(approved_at) = &seen.approved_at else {
        return Err(RunError::Invalid(format!("run {run} records no approval")));
    };
    let check = ctx
        .tracker
        .text_check(ctx.repo(), issue, approved_at, &ctx.config.instructors)?;
    // A history that cannot be judged counts as changed.
    if check != TextCheck::Unchanged {
        go_stuck(
            ctx,
            issue,
            at,
            "text-changed-before-start",
            Stop::new(messages::TEXT_CHANGED),
            None,
        )?;
        return Ok(StartStep::Stuck {
            reason: messages::TEXT_CHANGED.into(),
        });
    }
    // `approval-rechecked` is the runner's name for its check of the text since the approval.
    // Foundry records it here, before the session starts; the runner records it after the launch.
    if !seen.rechecked {
        ctx.record_step(
            run,
            APPROVAL_RECHECKED,
            &Cause::new(CauseStep::Start, "approval-rechecked"),
        )?;
    }
    start_session(ctx, run, detail, at, &record, &base, resumed.as_ref())
}

/// Records what a launch was decided on, in the order the runner's launch steps are read, and
/// then that the workspace is asked for: the model, a base that is not the repo's, a pull
/// request it continues, the branch it resumes from.
fn record_decision(
    ctx: &RunContext<'_>,
    run: &RunId,
    base: &str,
    resumed: Option<&Resumed>,
    name: &str,
    cause: &Cause,
) -> Result<(), RunError> {
    ctx.record_step(
        run,
        &format!("{LAUNCH_MODEL}{}", ctx.settings.fleet.model),
        cause,
    )?;
    if base != ctx.config.base {
        ctx.record_step(run, &format!("{LAUNCH_BASE}{base}"), cause)?;
    }
    if let Some(resumed) = resumed {
        if let Some(pr_url) = resumed.pr_url() {
            ctx.record_step(
                run,
                &format!("{LAUNCH_PULL_REQUEST}{} {pr_url}", resumed.branch()),
                cause,
            )?;
        }
        ctx.record_step(
            run,
            &format!("{LAUNCH_RESUMING}{}", resumed.branch()),
            cause,
        )?;
    }
    ctx.record_step(run, &format!("{WORKSPACE_REQUESTED}{name}"), cause)
}

/// The run ends as the runner refuses an unapproved issue, finishing a withdraw the ledger
/// shows begun (no pass begins one now). The refusal is explained once, the issue goes back to
/// `factory:proposed` without our assignee, the build workspace is archived by force and the run
/// is finished.
fn withdraw(
    ctx: &RunContext<'_>,
    run: &RunId,
    at: &Progress,
    refusal: Option<&Refusal>,
    workspace: WorkspaceId,
) -> Result<StartStep, RunError> {
    let issue = run.issue();
    let cause = Cause::new(CauseStep::Start, "approval-withdrawn");
    let reason = refusal.map_or("the approval was taken back", refusal_reason);
    // A refusal is explained once; a resumed withdraw whose approval reads as given again has
    // nothing to explain.
    if let Some(refusal) = refusal {
        let said = messages::unapproved(refusal);
        if !already_said(ctx, issue, refusal, &said)? {
            ctx.post_comment(issue, &said)?;
        }
    }
    // A start that was sent may have made a session: it ends with the run.
    stop_session(ctx, at)?;
    ctx.unassign(issue)?;
    ctx.set_state(issue, State::Proposed)?;
    ctx.archive(workspace)?;
    ctx.ledger
        .finish(run, Outcome::Stuck, None, None, &cause, None)?;
    Ok(StartStep::Unapproved {
        reason: reason.to_owned(),
    })
}

/// Starts the session with the launch message, records it, and moves the issue to `building`.
fn start_session(
    ctx: &RunContext<'_>,
    run: &RunId,
    detail: &IssueDetail,
    at: &mut Progress,
    record: &Record<Workspace>,
    base: &str,
    resumed: Option<&Resumed>,
) -> Result<StartStep, RunError> {
    let issue = run.issue();
    let workspace = record.body.id;
    let message = launch_message(ctx, run, base, resumed);
    let model = at
        .seen
        .launch_model
        .clone()
        .unwrap_or_else(|| ctx.settings.fleet.model.clone());
    // Written once per run before the first call, so a start that fails on our side is known to
    // have been sent.
    ctx.held()?;
    // A start that was sent and has no session recorded may have been taken whose answer was
    // lost, and the worker's start is not idempotent: the sessions of the workspace are ended,
    // recorded before they are, and only then is it started again.
    let earlier_sent = at.seen.launch_sent;
    if earlier_sent {
        ctx.record_step(
            run,
            LAUNCH_ENDING_SESSIONS,
            &Cause::new(CauseStep::Start, "launch-ending-sessions"),
        )?;
        ctx.end_sessions(workspace)?;
    }
    if !at.seen.launch_sent {
        let step = match resumed {
            None => LAUNCH_SENDING.to_owned(),
            Some(resumed) => format!("{LAUNCH_SENDING_RESUMED}{}", resumed.branch()),
        };
        ctx.record_step(run, &step, &Cause::new(CauseStep::Start, "launch-sending"))?;
        at.seen.launch_sent = true;
    }
    let start = FactoryStart {
        project: ctx.project.id,
        login: ctx.project.login.clone(),
        model: model.clone(),
        effort: ctx.settings.fleet.effort.clone(),
    };
    let session = match ctx.start_session(workspace, start, message) {
        Ok(session) => session,
        // The worker refused: stuck, with the detail in the ledger only. A machine that cannot
        // be reached is no refusal: the next pass starts again.
        Err(RunError::Worker(WorkerError::Failed(what))) => {
            // A first start the worker refused made no session; one after a start that may have
            // been taken may have left one.
            let note = if earlier_sent {
                messages::session_may_exist_note(&what, &format!("foundry:{run}:launch"))
            } else {
                what
            };
            let stop = Stop {
                reason: messages::LAUNCH_FAILED.into(),
                detail: Some(note),
                ..Stop::default()
            };
            go_stuck(ctx, issue, at, "launch-failed", stop, None)?;
            return Ok(StartStep::Stuck {
                reason: messages::LAUNCH_FAILED.into(),
            });
        }
        Err(error) => return Err(error),
    };
    record_launched(ctx, run, detail, record, session, base, resumed, &model)?;
    at.thread_id = Some(session.to_string());
    finish_building(ctx, issue, at)?;
    Ok(StartStep::Started { session })
}

/// The `session_launched` line.
#[allow(clippy::too_many_arguments)]
fn record_launched(
    ctx: &RunContext<'_>,
    run: &RunId,
    detail: &IssueDetail,
    record: &Record<Workspace>,
    session: SessionId,
    base: &str,
    resumed: Option<&Resumed>,
    model: &str,
) -> Result<(), RunError> {
    ctx.record(
        run,
        Event::SessionLaunched {
            thread_id: session.to_string(),
            worktree: Some(record.body.id.to_string()),
            branch: Some(
                resumed.map_or_else(|| record.body.branch.clone(), |r| r.branch().to_owned()),
            ),
            model_instance: None,
            model: Some(model.to_owned()),
            model_options: Some(vec![ModelOption {
                id: "effort".into(),
                value: OptionValue::Text(ctx.settings.fleet.effort.clone()),
            }]),
            resumed_from: resumed.map(|resumed| ResumedFrom {
                run: resumed.run().to_string(),
                branch: resumed.branch().to_owned(),
            }),
            base: Some(base.to_owned()),
            title: Some(detail.title.clone()),
        },
        &Cause::new(
            CauseStep::Start,
            if resumed.is_none() {
                "session-launched"
            } else {
                "resumed-from-branch"
            },
        ),
    )
}

/// A run whose session is launched: the label, and the launch asked for again by count when the
/// session is idle a pass after it started.
fn launched(
    ctx: &RunContext<'_>,
    run: &RunId,
    seen: &Seen,
    at: &mut Progress,
) -> Result<StartStep, RunError> {
    let Some(thread) = &seen.thread_id else {
        return Err(RunError::Invalid(format!("run {run} has no session")));
    };
    let session = session_id(thread)?;
    if at.state != State::Building {
        finish_building(ctx, run.issue(), at)?;
    }
    // The launch is the session's first prompt, so its count before the send is none. A session
    // that is idle with no turn ended, or whose turn record shows no prompt, has not taken it.
    let idle = ctx.view.session_state(session) == Some(SessionState::Idle);
    let untouched = ctx
        .view
        .session_turn(session)
        .is_none_or(|turn| turn.turn == 0 && turn.prompts == FIRST_PROMPT_COUNT);
    if !(idle && untouched) {
        return Ok(StartStep::Started { session });
    }
    let (base, resumed) = recorded_decision(ctx, run, seen)?;
    let message = launch_message(ctx, run, &base, resumed.as_ref());
    ctx.record_step(
        run,
        &format!("{LAUNCH_RESENDING}{FIRST_PROMPT_COUNT}"),
        &Cause::new(CauseStep::Start, "launch-resend"),
    )?;
    match ctx.send(session, message, Some(FIRST_PROMPT_COUNT)) {
        Ok(_) => Ok(StartStep::Resent { session }),
        // The worker decides: its count rose, so the message was typed; or the session is not
        // idle yet, and the next pass looks again.
        Err(RunError::Send(SendRefused::CountRose { .. } | SendRefused::NotIdle)) => {
            Ok(StartStep::Started { session })
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use foundry_proto::factory::{FactorySettings, ProjectFactory};

    use super::super::testing::HeldFor;
    use super::super::testing::*;
    use super::*;
    use crate::factory::github::{Claim, Tracker, newest_claim};
    use crate::factory::ledger::events::Event;
    use crate::factory::lifecycle::State;
    use crate::factory::messages::CUT_SHORT_COMMENT;
    use crate::factory::worker::Lost;

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
    /// settings, the fleet's and the project's, not a file in a data directory. Each case keeps
    /// the runner's name and asserts the rule that name is about, as the settings hold it: a
    /// pause holds claims back, the fleet's is named first, a resume lifts it, nothing can fail
    /// to be read. What a file's mode, a symlink, a FIFO or a missing directory change has no
    /// counterpart.
    mod claims_paused_by_hand {
        use super::*;

        /// The runner's case, where pausing writes the time privately: in Foundry a pause is
        /// one setting that is set, with no time and no file to write, and either setting holds
        /// claims back.
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

        /// The runner's case, where a second pause keeps the first time: in Foundry pausing the
        /// project too changes nothing about a fleet pause that is already set.
        #[test]
        fn a_second_pause_keeps_the_first_time() {
            assert_eq!(
                claims_paused(&settings(true, true)),
                claims_paused(&settings(true, false))
            );
        }

        /// The runner's case, where resume removes the file and says when there was none: in
        /// Foundry a resume is a setting turned off, and the claim that was held back goes
        /// ahead.
        #[test]
        fn resume_removes_the_file_and_says_when_there_was_none() {
            let mut rig = Rig::new();
            rig.settings = settings(true, false);
            assert!(matches!(
                claim_it(&rig).unwrap(),
                ClaimOutcome::Skipped { .. }
            ));
            rig.settings = settings(false, false);
            assert_eq!(claims_paused(&rig.settings), None);
            assert!(matches!(
                claim_it(&rig).unwrap(),
                ClaimOutcome::Claimed { .. }
            ));
        }

        /// The runner's case, where a file a person wrote is still a pause, by its mtime: in
        /// Foundry a pause a person set in the settings is a pause, and the words say which
        /// setting holds claims back.
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
            let mut rig = Rig::new();
            rig.settings = settings(false, true);
            assert_eq!(
                claim_it(&rig).unwrap(),
                ClaimOutcome::Skipped {
                    reason: ClaimPause::Project.reason().into()
                }
            );
        }

        /// The runner's case, where the time in the file is trimmed and parsed: in Foundry
        /// there is no time to read, and a paused claim writes nothing to GitHub or the ledger.
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
        }

        /// The runner's case, where a missing file and a symlink are no pause: in Foundry
        /// settings that were never written are the defaults, which pause nothing.
        #[test]
        fn a_missing_file_and_a_symlink_are_no_pause() {
            assert_eq!(
                claims_paused(&crate::factory::runs::RunSettings::default()),
                None
            );
        }

        /// The runner's case, where a file that cannot be opened is still a pause: in Foundry
        /// the pause does not depend on the project being on, so a project that is off is
        /// still held back by the fleet's pause.
        #[test]
        fn a_file_that_cannot_be_opened_is_still_a_pause_since_its_mtime() {
            let mut held = settings(true, false);
            held.project.on = false;
            assert_eq!(claims_paused(&held), Some(ClaimPause::Fleet));
        }

        /// The runner's case, where a FIFO is no pause and does not block the read: in Foundry
        /// the settings are values already read, and every combination answers at once.
        #[test]
        fn a_fifo_is_no_pause_and_does_not_block_the_read() {
            let table = [
                (false, false, None),
                (true, false, Some(ClaimPause::Fleet)),
                (false, true, Some(ClaimPause::Project)),
                (true, true, Some(ClaimPause::Fleet)),
            ];
            for (fleet, project, wanted) in table {
                assert_eq!(claims_paused(&settings(fleet, project)), wanted);
            }
        }

        /// The runner's case, where pausing in a data dir that is not there is an error: in
        /// Foundry a pause cannot fail, and a claim that is paused is no error either.
        #[test]
        fn pausing_in_a_data_dir_that_is_not_there_is_an_error() {
            let mut rig = Rig::new();
            rig.settings = settings(true, false);
            assert!(claim_it(&rig).is_ok());
        }
    }

    mod a_fresh_ready_issue {
        use super::*;

        /// The claim's half of the runner's case "claims, launches the session and moves the
        /// issue to building" (the whole case is in `the_launch`): the comment first, then the
        /// label, the assignee and the status comment, with the run's lines in the runner's
        /// order.
        #[test]
        fn a_claim_writes_the_comment_the_label_the_assignee_and_the_status_in_order() {
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

        /// Anyone can paste the factory's marker: the refusal's words posted by another login
        /// do not stand in for the factory's explanation.
        #[test]
        fn a_refusal_pasted_by_another_login_is_still_explained() {
            let posted = {
                let rig = Rig::new();
                rig.sim
                    .set_approval(approval_answer(LABELED_AT, "mallory", &[]));
                claim_it(&rig).unwrap();
                rig.sim.comments().remove(0)
            };
            let rig = Rig::new();
            rig.sim
                .set_approval(approval_answer(LABELED_AT, "mallory", &[]));
            rig.sim.comment_as("mallory", &posted);
            claim_it(&rig).unwrap();
            let explained = rig
                .sim
                .comments()
                .iter()
                .filter(|c| c.contains("did not start on this issue"))
                .count();
            assert_eq!(explained, 2, "mallory's and the factory's");
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
            assert!(matches!(
                claim(&rig.ctx(), ISSUE, rig.machine),
                Err(RunError::LeaseLost(_))
            ));
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
            let mut rig = Rig::new();
            rig.guard = Box::new(HeldFor::new(2));
            // The comment landed; the lease is lost before the label.
            assert!(matches!(
                claim(&rig.ctx(), ISSUE, rig.machine),
                Err(RunError::LeaseLost(_))
            ));
            assert_eq!(rig.sim.writes().len(), 1);
            assert_eq!(rig.sim.labels(), vec!["factory:ready"]);
            // The run the cut-short claim made is ended, so it holds no slot.
            assert!(rig.finished(&run_id(1)));
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
        assert!(rig.worker.calls().is_empty());
    }

    /// A lease lost between the claim's own steps stops it there: the ledger lines are written
    /// under the first check, the claim comment under the second, and the label and the
    /// assignee each under their own.
    #[test]
    fn a_lease_lost_between_the_steps_of_a_claim_stops_it_there() {
        // (checks held, GitHub writes that landed, the label left)
        let table = [
            (1, 0, "factory:ready"),
            (2, 1, "factory:ready"),
            (3, 2, "factory:triage"),
            (4, 3, "factory:triage"),
        ];
        for (checks, writes, label) in table {
            let mut rig = Rig::new();
            rig.guard = Box::new(HeldFor::new(checks));
            let result = claim_it(&rig);
            assert!(
                matches!(result, Err(RunError::LeaseLost(_))),
                "{checks}: {result:?}"
            );
            assert_eq!(rig.sim.writes().len(), writes, "{checks}");
            assert_eq!(rig.sim.labels(), vec![label], "{checks}");
            assert!(rig.worker.calls().is_empty());
        }
    }

    /// What one pass of `advance` shows, with the fleet as the test lays it out.
    mod the_launch {
        use foundry_proto::ids::{SessionId, TerminalId, WorkspaceId};
        use foundry_proto::record::{Record, Session, Terminal, Workspace};
        use foundry_proto::state::{SessionState, TerminalState};

        use super::super::super::testing::*;
        use super::super::*;
        use crate::factory::ledger::events::Event;
        use crate::factory::lifecycle::approval::Refusal;
        use crate::factory::messages::{
            CANNOT_CONFIRM, LAUNCH_FAILED, Message, ResumeFrom, TEXT_CHANGED,
        };
        use crate::factory::worker::{Call, FleetView, Lost, WorkerError};
        use crate::sessions::factory::{BODY_PATH, SendRefused};
        use crate::workspaces::{Checkout, FactoryTag, Setup};

        fn ws_id() -> WorkspaceId {
            WorkspaceId(uuid::Uuid::from_u128(0xa11))
        }

        fn setup_terminal() -> TerminalId {
            TerminalId(uuid::Uuid::from_u128(0x7e))
        }

        fn session() -> SessionId {
            SessionId(uuid::Uuid::from_u128(0x5e5))
        }

        fn elsewhere() -> MachineId {
            MachineId(uuid::Uuid::from_u128(0x77))
        }

        /// The workspace record a machine writes once it has made the workspace.
        fn workspace_record(
            owner: MachineId,
            name: &str,
            setup: Option<TerminalId>,
        ) -> Record<Workspace> {
            Record {
                owner,
                version: 1,
                deleted: false,
                body: Workspace {
                    id: ws_id(),
                    project: project_id(),
                    name: name.into(),
                    branch: name.into(),
                    base: "main".into(),
                    setup,
                },
            }
        }

        fn terminal_record(
            owner: MachineId,
            state: TerminalState,
            exit_code: Option<i32>,
        ) -> Record<Terminal> {
            Record {
                owner,
                version: 1,
                deleted: false,
                body: Terminal {
                    id: setup_terminal(),
                    workspace: ws_id(),
                    state,
                    exit_code,
                },
            }
        }

        fn session_record(owner: MachineId, state: SessionState) -> Record<Session> {
            Record {
                owner,
                version: 1,
                deleted: false,
                body: Session {
                    id: session(),
                    workspace: ws_id(),
                    terminal: TerminalId(uuid::Uuid::from_u128(0x5e6)),
                    model: "opus".into(),
                    state,
                    name: None,
                },
            }
        }

        /// The fleet shows the build workspace, with no setup.
        fn show_workspace(rig: &mut Rig) {
            rig.view = FleetView::builder()
                .online(rig.machine)
                .workspace(workspace_record(rig.machine, "fac/8-1", None))
                .build();
        }

        /// The fleet shows the build workspace with a setup terminal in `state`.
        fn show_setup(rig: &mut Rig, state: TerminalState, code: Option<i32>) {
            rig.view = FleetView::builder()
                .online(rig.machine)
                .workspace(workspace_record(
                    rig.machine,
                    "fac/8-1",
                    Some(setup_terminal()),
                ))
                .terminal(terminal_record(rig.machine, state, code))
                .build();
        }

        fn pass(rig: &Rig, run: &RunId) -> StartStep {
            advance(&rig.ctx(), run).unwrap()
        }

        /// A rig whose worker answers a create with [`ws_id`] and a session start with
        /// [`session`], with the issue claimed: the state the first `advance` finds.
        fn claimed_rig() -> (Rig, RunId) {
            let rig = Rig::new();
            rig.worker.on_create(Ok(ws_id()));
            rig.worker.on_start_session(Ok(session()));
            let run = match claim(&rig.ctx(), ISSUE, rig.machine).unwrap() {
                ClaimOutcome::Claimed { run } => run,
                other => panic!("{other:?}"),
            };
            (rig, run)
        }

        /// The run a claim made.
        fn claim_run(rig: &Rig) -> RunId {
            match claim(&rig.ctx(), ISSUE, rig.machine).unwrap() {
                ClaimOutcome::Claimed { run } => run,
                other => panic!("{other:?}"),
            }
        }

        /// Claimed, the workspace asked for and shown, with no setup: the next pass starts.
        fn workspace_shown() -> (Rig, RunId) {
            let (mut rig, run) = claimed_rig();
            assert_eq!(pass(&rig, &run), StartStep::Waiting(Wait::Workspace));
            show_workspace(&mut rig);
            (rig, run)
        }

        /// Claimed and started: the session runs and the issue is building.
        fn started() -> (Rig, RunId) {
            let (rig, run) = workspace_shown();
            assert_eq!(pass(&rig, &run), StartStep::Started { session: session() });
            (rig, run)
        }

        fn calls(rig: &Rig) -> Vec<Call> {
            rig.worker.calls()
        }

        fn creates(rig: &Rig) -> Vec<Call> {
            calls(rig)
                .into_iter()
                .filter(|call| matches!(call, Call::Create { .. }))
                .collect()
        }

        fn start_sessions(rig: &Rig) -> Vec<Call> {
            calls(rig)
                .into_iter()
                .filter(|call| matches!(call, Call::StartSession { .. }))
                .collect()
        }

        /// The comment that says why the run could not be finished.
        fn stuck_comment(rig: &Rig) -> String {
            rig.sim
                .comments()
                .into_iter()
                .find(|comment| comment.contains("could not finish this issue"))
                .expect("a stuck comment")
        }

        /// The runner's case "claims, launches the session and moves the issue to building",
        /// the whole of it: the claim, the one launch with its message, the label, the status
        /// comment and the run's lines through the building state.
        #[test]
        fn claims_launches_the_session_and_moves_the_issue_to_building() {
            let (rig, run) = workspace_shown();
            assert_eq!(rig.sim.labels(), vec!["factory:triage"]);
            assert_eq!(pass(&rig, &run), StartStep::Started { session: session() });
            match &start_sessions(&rig)[..] {
                [Call::StartSession { first, login, .. }] => {
                    assert_eq!(first, &launch_message());
                    assert_eq!(login, LOGIN);
                }
                other => panic!("{other:?}"),
            }
            assert_eq!(rig.sim.labels(), vec!["factory:building"]);
            assert_eq!(rig.sim.assignees(), vec![LOGIN]);
            assert!(
                rig.sim
                    .writes()
                    .iter()
                    .any(|write| write.contains("**Factory status:** building"))
            );
            let names = rig.names(&run);
            let launched = names.iter().position(|n| n == "session_launched").unwrap();
            let building = names.iter().rposition(|n| n == "state_changed").unwrap();
            assert!(launched < building, "{names:?}");
            let last = rig.events(&run).into_iter().last().unwrap();
            assert_eq!(last.data["to"], "building");
            assert!(!rig.finished(&run));
        }

        fn launch_message() -> Message {
            messages::launch(REPO, ISSUE, LOGIN, None, BODY_PATH)
        }

        mod the_build_workspace {
            use super::*;

            /// The spec's launch: the coordinator asks the machine for the build workspace
            /// named from the run, on a new branch of that name from the base, with the
            /// worktree's own provision as setup.
            #[test]
            fn is_asked_for_on_the_machine_the_claim_names_by_the_runs_name() {
                let (rig, run) = claimed_rig();
                assert_eq!(pass(&rig, &run), StartStep::Waiting(Wait::Workspace));
                assert_eq!(
                    calls(&rig),
                    vec![Call::Create {
                        machine: rig.machine,
                        request: crate::factory::worker::CreateWorkspace {
                            project: project_id(),
                            name: "fac/8-1".into(),
                            checkout: Checkout::NewBranch {
                                base: "main".into(),
                                branch: Some("fac/8-1".into()),
                            },
                            setup: Setup::Own,
                            tag: FactoryTag {
                                project: project_id(),
                                run: run.to_string(),
                            },
                        },
                    }]
                );
                assert_eq!(rig.sim.labels(), vec!["factory:triage"]);
            }

            #[test]
            fn is_recorded_before_it_is_asked_for_and_its_answer_after() {
                let rig = Rig::new();
                rig.worker
                    .on_create(Err(WorkerError::Unreachable("down".into())));
                rig.worker.on_create(Ok(ws_id()));
                let run = match claim(&rig.ctx(), ISSUE, rig.machine).unwrap() {
                    ClaimOutcome::Claimed { run } => run,
                    other => panic!("{other:?}"),
                };
                // A create that fails still leaves its request in the ledger, and no answer.
                assert!(advance(&rig.ctx(), &run).is_err());
                let steps = rig.steps(&run);
                assert!(steps.contains(&"workspace-requested fac/8-1".to_string()));
                assert!(!steps.iter().any(|s| s.starts_with("workspace-created")));
                pass(&rig, &run);
                assert!(
                    rig.steps(&run)
                        .contains(&format!("workspace-created {}", ws_id()))
                );
            }

            /// Acceptance: a create whose record arrives a pass later is not asked for twice.
            #[test]
            fn a_create_whose_record_arrives_a_pass_later_is_not_asked_for_twice() {
                let (mut rig, run) = claimed_rig();
                assert_eq!(pass(&rig, &run), StartStep::Waiting(Wait::Workspace));
                // The record has not arrived: another pass, and another.
                assert_eq!(pass(&rig, &run), StartStep::Waiting(Wait::Workspace));
                assert_eq!(pass(&rig, &run), StartStep::Waiting(Wait::Workspace));
                assert_eq!(creates(&rig).len(), 1);
                show_workspace(&mut rig);
                assert_eq!(pass(&rig, &run), StartStep::Started { session: session() });
                assert_eq!(creates(&rig).len(), 1);
            }

            #[test]
            fn a_create_that_failed_with_no_answer_is_asked_again_next_pass() {
                let rig = Rig::new();
                rig.worker
                    .on_create(Err(WorkerError::Unreachable("down".into())));
                let run = match claim(&rig.ctx(), ISSUE, rig.machine).unwrap() {
                    ClaimOutcome::Claimed { run } => run,
                    other => panic!("{other:?}"),
                };
                assert!(matches!(
                    advance(&rig.ctx(), &run),
                    Err(RunError::Worker(WorkerError::Unreachable(_)))
                ));
                assert_eq!(pass(&rig, &run), StartStep::Waiting(Wait::Workspace));
                assert_eq!(creates(&rig).len(), 2);
                // Asked for by the same request both times: the worker's create is idempotent.
                let both = creates(&rig);
                assert_eq!(both[0], both[1]);
            }

            #[test]
            fn one_the_fleet_already_shows_is_found_before_any_create() {
                let (mut rig, run) = claimed_rig();
                show_setup(&mut rig, TerminalState::Exited, Some(0));
                assert_eq!(pass(&rig, &run), StartStep::Started { session: session() });
                assert!(creates(&rig).is_empty());
                assert!(
                    rig.steps(&run)
                        .contains(&format!("workspace-created {}", ws_id()))
                );
            }
        }

        mod a_claim_with_no_run_events {
            use super::*;

            fn taken_over(rig: &Rig, claim_machine: MachineId) -> RunId {
                let run = run_id(1);
                rig.sim.set_labels(&["factory:triage"]);
                rig.sim.set_assignees(&[LOGIN]);
                rig.foundry_claim(&run, claim_machine);
                run
            }

            /// Acceptance: a claim with no workspace is launched on the machine its comment
            /// names.
            #[test]
            fn is_launched_on_the_machine_its_comment_names() {
                let rig = Rig::new();
                rig.worker.on_create(Ok(ws_id()));
                let run = taken_over(&rig, elsewhere());
                assert_eq!(pass(&rig, &run), StartStep::Waiting(Wait::Workspace));
                match &creates(&rig)[..] {
                    [Call::Create { machine, request }] => {
                        assert_eq!(*machine, elsewhere());
                        assert_eq!(request.name, "fac/8-1");
                    }
                    other => panic!("{other:?}"),
                }
            }

            #[test]
            fn records_what_it_learned_before_it_acts() {
                let rig = Rig::new();
                rig.worker
                    .on_create(Err(WorkerError::Unreachable("down".into())));
                let run = taken_over(&rig, elsewhere());
                assert!(advance(&rig.ctx(), &run).is_err());
                let events = rig.events(&run);
                let claimed = events.iter().find(|e| e.event == "claimed").unwrap();
                assert_eq!(claimed.data["machine"], elsewhere().to_string());
                assert_eq!(claimed.data["github_user"], LOGIN);
                assert_eq!(
                    rig.steps(&run)[0],
                    format!("approved {LABELED_AT}"),
                    "{:?}",
                    rig.steps(&run)
                );
                assert!(events.iter().any(|e| e.event == "state_changed"));
            }

            /// The spec: a claim with a build workspace and no event is carried on.
            #[test]
            fn with_a_build_workspace_is_carried_on_without_a_create() {
                let mut rig = Rig::new();
                rig.worker.on_start_session(Ok(session()));
                let run = taken_over(&rig, rig.machine);
                show_setup(&mut rig, TerminalState::Exited, Some(0));
                assert_eq!(pass(&rig, &run), StartStep::Started { session: session() });
                assert!(creates(&rig).is_empty());
            }

            #[test]
            fn a_comment_by_someone_else_is_no_claim() {
                let rig = Rig::new();
                rig.sim.set_labels(&["factory:triage"]);
                rig.sim.set_assignees(&[LOGIN]);
                rig.sim.comment_as(
                    "mallory",
                    &crate::factory::github::claim_comment(&run_id(1), &elsewhere()),
                );
                let step = pass(&rig, &run_id(1));
                assert!(matches!(step, StartStep::Skipped { .. }));
                assert!(calls(&rig).is_empty());
                assert!(rig.ledger.read(&run_id(1)).unwrap().is_empty());
            }

            #[test]
            fn a_claim_that_names_another_run_leaves_this_one_alone() {
                let rig = Rig::new();
                taken_over(&rig, rig.machine);
                rig.foundry_claim(&run_id(2), rig.machine);
                let step = pass(&rig, &run_id(1));
                assert!(matches!(step, StartStep::Skipped { .. }));
                assert!(rig.ledger.read(&run_id(1)).unwrap().is_empty());
                assert!(calls(&rig).is_empty());
            }

            /// F13: a claim without Foundry's marker belongs to another factory.
            #[test]
            fn a_foreign_claim_is_left_alone() {
                let rig = Rig::new();
                rig.sim.set_labels(&["factory:triage"]);
                rig.sim.set_assignees(&[LOGIN]);
                rig.sim
                    .comment_as(LOGIN, "Claimed.\n\n<!-- softfac:claim mac-mini -->");
                let step = pass(&rig, &run_id(1));
                assert!(matches!(step, StartStep::Skipped { .. }));
                assert!(calls(&rig).is_empty());
                assert!(rig.sim.writes().is_empty());
            }

            /// The runner's case "never claims an issue put back to ready: that is the claim
            /// step's": a cut-short claim's comment is on an issue still `ready`.
            #[test]
            fn never_claims_an_issue_put_back_to_ready_that_is_the_claim_step_s() {
                let rig = Rig::new();
                rig.foundry_claim(&run_id(1), rig.machine);
                let step = pass(&rig, &run_id(1));
                assert!(matches!(step, StartStep::Skipped { .. }));
                assert!(rig.sim.writes().is_empty());
                assert!(rig.ledger.read(&run_id(1)).unwrap().is_empty());
            }

            /// The runner's case "with no run goes stuck: the factory cannot confirm what it was
            /// approved on".
            #[test]
            fn with_no_run_goes_stuck_the_factory_cannot_confirm_what_it_was_approved_on() {
                let rig = Rig::new();
                let run = taken_over(&rig, rig.machine);
                rig.sim
                    .set_approval(approval_answer(LABELED_AT, "mallory", &[]));
                assert_eq!(
                    pass(&rig, &run),
                    StartStep::Stuck {
                        reason: CANNOT_CONFIRM.into()
                    }
                );
                assert_eq!(rig.sim.labels(), vec!["factory:stuck"]);
                assert!(calls(&rig).is_empty());
                assert!(rig.finished(&run));
            }

            /// One label event of the timeline: `kind` of `label` at `at` by `login`, who is a
            /// `User` unless `typename` says otherwise.
            fn label_event(
                kind: &str,
                label: &str,
                at: &str,
                typename: &str,
                login: &str,
            ) -> serde_json::Value {
                serde_json::json!({
                    "__typename": kind,
                    "createdAt": at,
                    "actor": {"__typename": typename, "login": login},
                    "label": {"name": label},
                })
            }

            /// The claim's swap of `factory:ready` for `factory:triage` at `at`, as GitHub
            /// records it: the removal and the label, both by the factory's login.
            fn claim_swap(at: &str) -> [serde_json::Value; 2] {
                [
                    label_event("UnlabeledEvent", "factory:ready", at, "User", LOGIN),
                    label_event("LabeledEvent", "factory:triage", at, "User", LOGIN),
                ]
            }

            /// GitHub's answer whose timeline is `nodes`, oldest first, with the body edits of
            /// `edits_by`.
            fn timeline_of(nodes: Vec<serde_json::Value>, edits_by: &[(&str, &str)]) -> String {
                let mut answer: serde_json::Value =
                    serde_json::from_str(&approval_answer(LABELED_AT, "tom", edits_by)).unwrap();
                let timeline = &mut answer["data"]["repository"]["issue"]["timelineItems"];
                timeline["totalCount"] = nodes.len().into();
                timeline["nodes"] = nodes.into();
                answer.to_string()
            }

            /// What GitHub answers once the claim swapped `factory:ready` for `factory:triage`:
            /// the label `by` applied at [`LABELED_AT`], then the claim's swap, so no label is in
            /// force.
            fn removed_by_the_claim(by: &str, edits_by: &[(&str, &str)]) -> String {
                let mut nodes = vec![label_event(
                    "LabeledEvent",
                    "factory:ready",
                    LABELED_AT,
                    "User",
                    by,
                )];
                nodes.extend(claim_swap("2026-10-04T10:05:00Z"));
                timeline_of(nodes, edits_by)
            }

            /// `pass` on `run` goes stuck because the approval cannot be confirmed, and records
            /// no approval.
            fn goes_stuck_unapproved(rig: &Rig, run: &RunId) {
                assert_eq!(
                    pass(rig, run),
                    StartStep::Stuck {
                        reason: CANNOT_CONFIRM.into()
                    }
                );
                assert!(
                    !rig.steps(run).iter().any(|s| s.starts_with("approved ")),
                    "{:?}",
                    rig.steps(run)
                );
                assert!(calls(rig).is_empty());
            }

            /// The `gh api graphql` approval reads among the calls the rig's GitHub was asked.
            fn approval_reads(rig: &Rig) -> usize {
                let query = format!("query={}", crate::factory::github::read::APPROVAL_QUERY);
                rig.sim
                    .calls()
                    .iter()
                    .filter(|call| call[..2] == ["api", "graphql"] && call.contains(&query))
                    .count()
            }

            /// A takeover reads the issue's approval history once, whether it approves or not.
            #[test]
            fn a_takeover_reads_the_approval_history_once() {
                let rig = Rig::new();
                rig.worker.on_create(Ok(ws_id()));
                let run = taken_over(&rig, elsewhere());
                rig.sim.set_approval(removed_by_the_claim("tom", &[]));
                assert_eq!(pass(&rig, &run), StartStep::Waiting(Wait::Workspace));
                assert_eq!(approval_reads(&rig), 1);

                let rig = Rig::new();
                let run = taken_over(&rig, rig.machine);
                rig.sim.set_approval(removed_by_the_claim("mallory", &[]));
                goes_stuck_unapproved(&rig, &run);
                assert_eq!(approval_reads(&rig), 1);
            }

            /// An approval history with a malformed timestamp fails the takeover before it
            /// records or does anything, even where the label is a stranger's: the read checks
            /// every timestamp, and a later pass reads it again.
            #[test]
            fn a_malformed_timestamp_in_the_history_fails_the_takeover_before_it_records_anything()
            {
                let rig = Rig::new();
                rig.worker.on_create(Ok(ws_id()));
                let run = taken_over(&rig, elsewhere());
                let mut nodes = vec![label_event(
                    "LabeledEvent",
                    "factory:ready",
                    "2026-10-04 10:00:00Z",
                    "User",
                    "mallory",
                )];
                nodes.extend(claim_swap("2026-10-04T10:05:00Z"));
                rig.sim.set_approval(timeline_of(nodes, &[]));
                assert!(advance(&rig.ctx(), &run).is_err());
                assert!(rig.ledger.read(&run).unwrap().is_empty());
                assert_eq!(rig.sim.labels(), ["factory:triage"]);
                assert!(calls(&rig).is_empty());
                assert_eq!(approval_reads(&rig), 1);
            }

            /// The claim removed `factory:ready`: the run is approved on the newest label an
            /// instructor applied before it, and carried on.
            #[test]
            fn records_the_approval_of_the_label_the_claim_removed() {
                let rig = Rig::new();
                rig.worker.on_create(Ok(ws_id()));
                let run = taken_over(&rig, elsewhere());
                rig.sim.set_approval(removed_by_the_claim("tom", &[]));
                assert_eq!(pass(&rig, &run), StartStep::Waiting(Wait::Workspace));
                assert_eq!(rig.steps(&run)[0], format!("approved {LABELED_AT}"));
                assert!(!rig.finished(&run));
            }

            #[test]
            fn a_label_the_claim_removed_that_a_stranger_applied_goes_stuck() {
                let rig = Rig::new();
                let run = taken_over(&rig, rig.machine);
                rig.sim.set_approval(removed_by_the_claim("mallory", &[]));
                assert_eq!(
                    pass(&rig, &run),
                    StartStep::Stuck {
                        reason: CANNOT_CONFIRM.into()
                    }
                );
                assert!(calls(&rig).is_empty());
            }

            #[test]
            fn a_text_a_stranger_edited_after_the_label_the_claim_removed_goes_stuck() {
                let rig = Rig::new();
                let run = taken_over(&rig, rig.machine);
                rig.sim.set_approval(removed_by_the_claim(
                    "tom",
                    &[("2026-10-04T10:01:00Z", "mallory")],
                ));
                assert_eq!(
                    pass(&rig, &run),
                    StartStep::Stuck {
                        reason: CANNOT_CONFIRM.into()
                    }
                );
                assert!(calls(&rig).is_empty());
            }

            /// The claim's removal must be the label's newest event: a label applied and removed
            /// again after the claim hides it, and without the claim comment's time the factory
            /// cannot tell that from an approval withdrawn, so the run goes stuck.
            #[test]
            fn a_label_applied_and_removed_after_the_claim_goes_stuck() {
                let rig = Rig::new();
                let run = taken_over(&rig, rig.machine);
                let mut nodes = vec![label_event(
                    "LabeledEvent",
                    "factory:ready",
                    LABELED_AT,
                    "User",
                    "tom",
                )];
                nodes.extend(claim_swap("2026-10-04T10:05:00Z"));
                nodes.push(label_event(
                    "LabeledEvent",
                    "factory:ready",
                    "2026-10-04T10:06:00Z",
                    "User",
                    "mallory",
                ));
                nodes.push(label_event(
                    "UnlabeledEvent",
                    "factory:ready",
                    "2026-10-04T10:07:00Z",
                    "User",
                    "mallory",
                ));
                rig.sim.set_approval(timeline_of(nodes, &[]));
                goes_stuck_unapproved(&rig, &run);
            }

            /// The claim's removal with no label before it on the timeline: nothing says what it
            /// was approved on, so the run goes stuck.
            #[test]
            fn a_claims_removal_with_no_label_before_it_goes_stuck() {
                let rig = Rig::new();
                let run = taken_over(&rig, rig.machine);
                rig.sim
                    .set_approval(timeline_of(claim_swap("2026-10-04T10:05:00Z").into(), &[]));
                goes_stuck_unapproved(&rig, &run);
            }

            /// A label whose actor GitHub gives as none (a deleted account) is no instructor's.
            #[test]
            fn a_label_the_claim_removed_with_no_actor_goes_stuck() {
                let rig = Rig::new();
                let run = taken_over(&rig, rig.machine);
                let mut label =
                    label_event("LabeledEvent", "factory:ready", LABELED_AT, "User", "tom");
                label["actor"] = serde_json::Value::Null;
                let mut nodes = vec![label];
                nodes.extend(claim_swap("2026-10-04T10:05:00Z"));
                rig.sim.set_approval(timeline_of(nodes, &[]));
                goes_stuck_unapproved(&rig, &run);
            }

            /// GitHub logins match whatever their case: a removal by the factory's login written
            /// in another case is still the claim's.
            #[test]
            fn the_claims_removal_is_known_by_its_login_in_any_case() {
                let rig = Rig::new();
                rig.worker.on_create(Ok(ws_id()));
                let run = taken_over(&rig, elsewhere());
                let mut nodes = vec![label_event(
                    "LabeledEvent",
                    "factory:ready",
                    LABELED_AT,
                    "User",
                    "tom",
                )];
                nodes.push(label_event(
                    "UnlabeledEvent",
                    "factory:ready",
                    "2026-10-04T10:05:00Z",
                    "User",
                    &LOGIN.to_uppercase(),
                ));
                rig.sim.set_approval(timeline_of(nodes, &[]));
                assert_eq!(pass(&rig, &run), StartStep::Waiting(Wait::Workspace));
                assert_eq!(rig.steps(&run)[0], format!("approved {LABELED_AT}"));
            }

            /// A stranger's label in force at the claim is no approval, though an instructor's
            /// came before it.
            #[test]
            fn a_strangers_label_in_force_at_the_claim_goes_stuck() {
                let rig = Rig::new();
                let run = taken_over(&rig, rig.machine);
                let mut nodes = vec![
                    label_event("LabeledEvent", "factory:ready", LABELED_AT, "User", "tom"),
                    label_event(
                        "UnlabeledEvent",
                        "factory:ready",
                        "2026-10-04T10:01:00Z",
                        "User",
                        "tom",
                    ),
                    label_event(
                        "LabeledEvent",
                        "factory:ready",
                        "2026-10-04T10:02:00Z",
                        "User",
                        "mallory",
                    ),
                ];
                nodes.extend(claim_swap("2026-10-04T10:05:00Z"));
                rig.sim.set_approval(timeline_of(nodes, &[]));
                goes_stuck_unapproved(&rig, &run);
            }

            /// A label taken off by anyone but the claim was withdrawn, so it approves nothing.
            #[test]
            fn a_label_taken_off_by_someone_other_than_the_claim_goes_stuck() {
                let rig = Rig::new();
                let run = taken_over(&rig, rig.machine);
                let nodes = vec![
                    label_event("LabeledEvent", "factory:ready", LABELED_AT, "User", "tom"),
                    label_event(
                        "UnlabeledEvent",
                        "factory:ready",
                        "2026-10-04T10:01:00Z",
                        "User",
                        "tom",
                    ),
                    label_event(
                        "LabeledEvent",
                        "factory:triage",
                        "2026-10-04T10:02:00Z",
                        "User",
                        "mallory",
                    ),
                ];
                rig.sim.set_approval(timeline_of(nodes, &[]));
                goes_stuck_unapproved(&rig, &run);
            }

            /// An instructor is a person: a GitHub App under an instructor's login approves
            /// nothing.
            #[test]
            fn a_label_an_app_applied_under_an_instructors_login_goes_stuck() {
                let rig = Rig::new();
                let run = taken_over(&rig, rig.machine);
                let mut nodes = vec![label_event(
                    "LabeledEvent",
                    "factory:ready",
                    LABELED_AT,
                    "Bot",
                    "tom",
                )];
                nodes.extend(claim_swap("2026-10-04T10:05:00Z"));
                rig.sim.set_approval(timeline_of(nodes, &[]));
                goes_stuck_unapproved(&rig, &run);
            }

            /// An edit history cut off after the label cannot be judged whole, so it confirms
            /// nothing.
            #[test]
            fn a_label_the_claim_removed_with_an_edit_history_cut_off_goes_stuck() {
                let rig = Rig::new();
                let run = taken_over(&rig, rig.machine);
                let mut answer: serde_json::Value = serde_json::from_str(&removed_by_the_claim(
                    "tom",
                    &[("2026-10-04T10:01:00Z", "tom")],
                ))
                .unwrap();
                answer["data"]["repository"]["issue"]["userContentEdits"]["totalCount"] =
                    150.into();
                rig.sim.set_approval(answer.to_string());
                goes_stuck_unapproved(&rig, &run);
            }

            /// `pass` on `run` is carried on, approved on the label applied at `labeled_at`.
            fn carried_on_approved_at(rig: &Rig, run: &RunId, labeled_at: &str) {
                assert_eq!(pass(rig, run), StartStep::Waiting(Wait::Workspace));
                assert_eq!(rig.steps(run)[0], format!("approved {labeled_at}"));
            }

            /// An earlier attempt's claim took the label off once; the label an instructor put
            /// back and took off again before this claim could is no approval.
            #[test]
            fn a_label_withdrawn_after_an_earlier_attempts_claim_goes_stuck() {
                let rig = Rig::new();
                let run = taken_over(&rig, rig.machine);
                let mut nodes = vec![label_event(
                    "LabeledEvent",
                    "factory:ready",
                    LABELED_AT,
                    "User",
                    "tom",
                )];
                nodes.extend(claim_swap("2026-10-04T10:05:00Z"));
                nodes.push(label_event(
                    "LabeledEvent",
                    "factory:ready",
                    "2026-10-04T11:00:00Z",
                    "User",
                    "tom",
                ));
                nodes.push(label_event(
                    "UnlabeledEvent",
                    "factory:ready",
                    "2026-10-04T11:05:00Z",
                    "User",
                    "tom",
                ));
                rig.sim.set_approval(timeline_of(nodes, &[]));
                goes_stuck_unapproved(&rig, &run);
            }

            /// A second attempt is approved on the label put back after the first went stuck,
            /// not on the first attempt's.
            #[test]
            fn a_second_attempt_is_approved_on_the_label_its_own_claim_took_off() {
                let rig = Rig::new();
                rig.worker.on_create(Ok(ws_id()));
                let run = taken_over(&rig, elsewhere());
                let again = "2026-10-04T11:00:00Z";
                let mut nodes = vec![label_event(
                    "LabeledEvent",
                    "factory:ready",
                    LABELED_AT,
                    "User",
                    "tom",
                )];
                nodes.extend(claim_swap("2026-10-04T10:05:00Z"));
                nodes.push(label_event(
                    "UnlabeledEvent",
                    "factory:triage",
                    "2026-10-04T10:30:00Z",
                    "User",
                    LOGIN,
                ));
                nodes.push(label_event(
                    "LabeledEvent",
                    "factory:stuck",
                    "2026-10-04T10:30:00Z",
                    "User",
                    LOGIN,
                ));
                nodes.push(label_event(
                    "LabeledEvent",
                    "factory:ready",
                    again,
                    "User",
                    "tom",
                ));
                nodes.extend(claim_swap("2026-10-04T11:05:00Z"));
                rig.sim.set_approval(timeline_of(nodes, &[]));
                carried_on_approved_at(&rig, &run, again);
            }

            /// Other labels applied or removed around the claim, by anyone, change nothing.
            #[test]
            fn other_labels_around_the_claim_leave_its_approval_as_it_was() {
                let rig = Rig::new();
                rig.worker.on_create(Ok(ws_id()));
                let run = taken_over(&rig, elsewhere());
                let mut nodes = vec![
                    label_event("LabeledEvent", "factory:ready", LABELED_AT, "User", "tom"),
                    label_event(
                        "UnlabeledEvent",
                        "factory:proposed",
                        LABELED_AT,
                        "User",
                        "tom",
                    ),
                    label_event(
                        "LabeledEvent",
                        "bug",
                        "2026-10-04T10:01:00Z",
                        "Bot",
                        "triage-bot",
                    ),
                ];
                nodes.extend(claim_swap("2026-10-04T10:05:00Z"));
                nodes.push(label_event(
                    "UnlabeledEvent",
                    "factory:triage",
                    "2026-10-04T10:06:00Z",
                    "User",
                    LOGIN,
                ));
                nodes.push(label_event(
                    "LabeledEvent",
                    "factory:building",
                    "2026-10-04T10:06:00Z",
                    "User",
                    LOGIN,
                ));
                rig.sim.set_approval(timeline_of(nodes, &[]));
                carried_on_approved_at(&rig, &run, LABELED_AT);
            }
        }

        mod the_setup {
            use super::*;

            #[test]
            fn still_running_is_waited_for_with_no_session() {
                let (mut rig, run) = claimed_rig();
                assert_eq!(pass(&rig, &run), StartStep::Waiting(Wait::Workspace));
                show_setup(&mut rig, TerminalState::Running, None);
                assert_eq!(pass(&rig, &run), StartStep::Waiting(Wait::Setup));
                assert!(start_sessions(&rig).is_empty());
                assert!(
                    !calls(&rig)
                        .iter()
                        .any(|call| matches!(call, Call::Tail { .. }))
                );
            }

            #[test]
            fn on_an_unreachable_machine_is_waited_for() {
                let (mut rig, run) = claimed_rig();
                pass(&rig, &run);
                show_setup(&mut rig, TerminalState::Unreachable, None);
                assert_eq!(pass(&rig, &run), StartStep::Waiting(Wait::Setup));
            }

            #[test]
            fn that_exited_cleanly_goes_on_to_the_session() {
                let (mut rig, run) = claimed_rig();
                pass(&rig, &run);
                show_setup(&mut rig, TerminalState::Exited, Some(0));
                assert_eq!(pass(&rig, &run), StartStep::Started { session: session() });
            }

            /// The spec: a failed setup makes the run stuck, with the tail of the setup
            /// terminal in the comment. The session is ended and the workspace kept.
            #[test]
            fn that_failed_makes_the_run_stuck_with_the_terminals_tail() {
                let (mut rig, run) = claimed_rig();
                pass(&rig, &run);
                show_setup(&mut rig, TerminalState::Exited, Some(2));
                rig.worker.on_tail(Ok(
                    "npm ERR! the install failed\nsetup failed: cargo fetch exited 2".into(),
                ));
                assert_eq!(
                    pass(&rig, &run),
                    StartStep::Stuck {
                        reason: SETUP_FAILED.into()
                    }
                );
                let comment = stuck_comment(&rig);
                assert!(comment.contains(&format!("Reason: {SETUP_FAILED}")));
                assert!(comment.contains("setup failed: cargo fetch exited 2"));
                assert_eq!(rig.sim.labels(), vec!["factory:stuck"]);
                assert!(rig.sim.assignees().is_empty());
                assert!(rig.finished(&run));
                assert!(start_sessions(&rig).is_empty());
                // The workspace is kept: nothing archives it.
                assert!(
                    !calls(&rig)
                        .iter()
                        .any(|call| matches!(call, Call::Archive { .. }))
                );
                let stuck = rig
                    .events(&run)
                    .into_iter()
                    .find(|e| e.event == "stuck")
                    .unwrap();
                assert!(
                    stuck.data["reason"]
                        .as_str()
                        .unwrap()
                        .contains("cargo fetch exited 2")
                );
            }

            /// The spec: output in any comment is cut to its last 20,000 characters, and the
            /// event keeps up to 64 KB.
            #[test]
            fn a_long_tail_is_cut_in_the_comment_and_kept_longer_in_the_event() {
                let (mut rig, run) = claimed_rig();
                pass(&rig, &run);
                show_setup(&mut rig, TerminalState::Exited, Some(1));
                let tail = format!("{}THE-END", "x".repeat(70_000));
                rig.worker.on_tail(Ok(tail));
                pass(&rig, &run);
                let comment = &stuck_comment(&rig);
                assert!(comment.contains("THE-END"));
                assert!(comment.contains("[cut]"));
                assert!(comment.chars().count() < 21_000);
                let stuck = rig
                    .events(&run)
                    .into_iter()
                    .find(|e| e.event == "stuck")
                    .unwrap();
                let recorded = stuck.data["reason"].as_str().unwrap();
                assert!(recorded.ends_with("THE-END"));
                assert!(recorded.len() > 30_000);
                assert!(recorded.len() <= 64 * 1024);
            }

            #[test]
            fn whose_tail_cannot_be_read_is_asked_again_next_pass() {
                let (mut rig, run) = claimed_rig();
                pass(&rig, &run);
                show_setup(&mut rig, TerminalState::Exited, Some(1));
                rig.worker
                    .on_tail(Err(WorkerError::Unreachable("down".into())));
                assert!(advance(&rig.ctx(), &run).is_err());
                assert!(!rig.finished(&run));
                assert_eq!(rig.sim.labels(), vec!["factory:triage"]);
            }
        }

        mod the_approval_checked_before_the_session {
            use super::*;

            /// Acceptance: the approval is checked before the session starts, not after.
            #[test]
            fn is_checked_before_the_session_starts_not_after() {
                let (rig, run) = workspace_shown();
                assert_eq!(pass(&rig, &run), StartStep::Started { session: session() });
                let names = rig.names(&run);
                let steps = rig.steps(&run);
                let rechecked = steps
                    .iter()
                    .position(|s| s == "approval-rechecked")
                    .unwrap();
                let sending = steps.iter().position(|s| s == "launch-sending").unwrap();
                assert!(rechecked < sending, "{steps:?}");
                let launched = names.iter().position(|n| n == "session_launched").unwrap();
                assert!(
                    names
                        .iter()
                        .enumerate()
                        .filter(|(_, n)| *n == "step_changed")
                        .any(|(at, _)| at < launched)
                );
            }

            /// The claim swaps `factory:ready` for `factory:triage`, and GitHub records that
            /// removal: the approval then reads as no label in force. The claim already holds
            /// the approval, so the check before the session is of the text since it, and the
            /// session starts.
            #[test]
            fn a_label_the_claim_removed_still_starts_the_session() {
                let (rig, run) = workspace_shown();
                let mut answer: serde_json::Value =
                    serde_json::from_str(&approval_answer(LABELED_AT, "tom", &[])).unwrap();
                let timeline = &mut answer["data"]["repository"]["issue"]["timelineItems"];
                timeline["totalCount"] = 2.into();
                timeline["nodes"]
                    .as_array_mut()
                    .unwrap()
                    .push(serde_json::json!({
                        "__typename": "UnlabeledEvent",
                        "createdAt": "2026-10-04T10:05:00Z",
                        "actor": {"__typename": "User", "login": LOGIN},
                        "label": {"name": "factory:ready"},
                    }));
                rig.sim.set_approval(answer.to_string());
                assert_eq!(
                    rig.sim
                        .approval(REPO, ISSUE, &rig.ctx().config.instructors)
                        .unwrap(),
                    Approval::Refused(Refusal::LabelNotFound)
                );
                assert_eq!(pass(&rig, &run), StartStep::Started { session: session() });
                assert_eq!(start_sessions(&rig).len(), 1);
                assert_eq!(rig.sim.labels(), vec!["factory:building"]);
                assert!(!rig.finished(&run));
            }

            /// Who applied the label in force now is not read again before the session: a label
            /// that reads as a stranger's, with the text unchanged since the approval the run
            /// recorded, still starts it.
            #[test]
            fn a_label_that_now_reads_as_a_strangers_still_starts_the_session() {
                let (rig, run) = workspace_shown();
                rig.sim
                    .set_approval(approval_answer(LABELED_AT, "mallory", &[]));
                assert_eq!(pass(&rig, &run), StartStep::Started { session: session() });
                assert!(
                    !rig.sim
                        .comments()
                        .iter()
                        .any(|c| c.contains("did not start on this issue"))
                );
            }

            #[test]
            fn a_text_edited_before_the_approval_the_run_recorded_goes_stuck() {
                let (mut rig, run) = claimed_rig();
                pass(&rig, &run);
                show_workspace(&mut rig);
                // The label now in force is later than the one the run was approved on, and
                // someone who is not an instructor edited the text in between.
                rig.sim.set_approval(approval_answer(
                    "2026-10-04T12:00:00Z",
                    "tom",
                    &[("2026-10-04T10:30:00Z", "mallory")],
                ));
                assert_eq!(
                    pass(&rig, &run),
                    StartStep::Stuck {
                        reason: TEXT_CHANGED.into()
                    }
                );
                assert!(start_sessions(&rig).is_empty());
                assert_eq!(rig.sim.labels(), vec!["factory:stuck"]);
            }

            /// A text history that cannot be judged whole since the approval counts as changed:
            /// the edit history is cut off and its oldest entry shown is after the approval.
            #[test]
            fn a_text_history_that_cannot_be_judged_goes_stuck() {
                let (rig, run) = workspace_shown();
                let mut answer: serde_json::Value = serde_json::from_str(&approval_answer(
                    LABELED_AT,
                    "tom",
                    &[("2026-10-04T10:30:00Z", "tom")],
                ))
                .unwrap();
                answer["data"]["repository"]["issue"]["userContentEdits"]["totalCount"] = 5.into();
                rig.sim.set_approval(answer.to_string());
                assert_eq!(
                    rig.sim
                        .text_check(REPO, ISSUE, LABELED_AT, &rig.ctx().config.instructors)
                        .unwrap(),
                    TextCheck::Unjudged
                );
                assert_eq!(
                    pass(&rig, &run),
                    StartStep::Stuck {
                        reason: TEXT_CHANGED.into()
                    }
                );
                assert!(start_sessions(&rig).is_empty());
                assert_eq!(rig.sim.labels(), vec!["factory:stuck"]);
            }

            #[test]
            fn passed_is_recorded_once() {
                let (rig, run) = started();
                assert_eq!(
                    rig.steps(&run)
                        .iter()
                        .filter(|s| *s == "approval-rechecked")
                        .count(),
                    1
                );
            }

            #[test]
            fn a_failed_read_is_an_error_and_the_next_pass_checks_again() {
                let (rig, run) = workspace_shown();
                rig.sim.fail("graphql", 1);
                assert!(advance(&rig.ctx(), &run).is_err());
                assert!(start_sessions(&rig).is_empty());
                assert!(!rig.steps(&run).contains(&"approval-rechecked".to_string()));
                assert_eq!(pass(&rig, &run), StartStep::Started { session: session() });
            }
        }

        mod the_session {
            use super::*;

            /// The runner's case "claims, launches the session and moves the issue to
            /// building".
            #[test]
            fn is_started_with_the_launch_message_and_the_issue_moves_to_building() {
                let (rig, run) = started();
                assert_eq!(
                    start_sessions(&rig),
                    vec![Call::StartSession {
                        workspace: ws_id(),
                        project: project_id(),
                        login: LOGIN.into(),
                        model: "opus".into(),
                        effort: "high".into(),
                        first: launch_message(),
                    }]
                );
                assert_eq!(rig.sim.labels(), vec!["factory:building"]);
                let comments = rig.sim.comments();
                assert!(
                    comments
                        .last()
                        .unwrap()
                        .contains("**Factory status:** building")
                );
                let names = rig.names(&run);
                assert_eq!(
                    names[names.len() - 2..],
                    ["session_launched", "state_changed"]
                );
                let launched = rig
                    .events(&run)
                    .into_iter()
                    .find(|e| e.event == "session_launched")
                    .unwrap();
                assert_eq!(launched.data["thread_id"], session().to_string());
                assert_eq!(launched.data["worktree"], ws_id().to_string());
                assert_eq!(launched.data["branch"], "fac/8-1");
                assert_eq!(launched.data["base"], "main");
                assert_eq!(launched.data["title"], "Make it faster");
                assert_eq!(launched.data["model"], "opus");
                assert_eq!(launched.data["resumed_from"], serde_json::Value::Null);
                assert!(rig.steps(&run).contains(&"launch-sending".to_string()));
                let seen = rig.ctx().seen(&run).unwrap();
                assert_eq!(seen.state, Some(State::Building));
                assert_eq!(
                    seen.thread_id.as_deref(),
                    Some(session().to_string().as_str())
                );
            }

            /// The runner's case "launches on the project's model and records it": the model and
            /// the effort are the fleet's settings.
            #[test]
            fn launches_on_the_project_s_model_and_records_it() {
                let (mut rig, run) = claimed_rig();
                rig.settings.fleet.model = "sonnet".into();
                rig.settings.fleet.effort = "low".into();
                pass(&rig, &run);
                show_workspace(&mut rig);
                pass(&rig, &run);
                match &start_sessions(&rig)[..] {
                    [Call::StartSession { model, effort, .. }] => {
                        assert_eq!(model, "sonnet");
                        assert_eq!(effort, "low");
                    }
                    other => panic!("{other:?}"),
                }
                assert!(rig.steps(&run).contains(&"launch-model sonnet".to_string()));
                let launched = rig
                    .events(&run)
                    .into_iter()
                    .find(|e| e.event == "session_launched")
                    .unwrap();
                assert_eq!(launched.data["model"], "sonnet");
                assert_eq!(launched.data["model_options"][0]["id"], "effort");
                assert_eq!(launched.data["model_options"][0]["value"], "low");
            }

            /// The runner's case "a launch sent again keeps the model the first send decided
            /// and asks no cooldown": a setting changed after the decision does not change it.
            #[test]
            fn a_launch_sent_again_keeps_the_model_the_first_send_decided_and_asks_no_cooldown() {
                let (mut rig, run) = claimed_rig();
                rig.settings.fleet.model = "sonnet".into();
                pass(&rig, &run);
                show_workspace(&mut rig);
                rig.settings.fleet.model = "haiku".into();
                pass(&rig, &run);
                match &start_sessions(&rig)[..] {
                    [Call::StartSession { model, .. }] => assert_eq!(model, "sonnet"),
                    other => panic!("{other:?}"),
                }
            }

            /// The runner's case "a launch T3 refuses goes stuck with the detail in the ledger
            /// only".
            #[test]
            fn a_launch_t3_refuses_goes_stuck_with_the_detail_in_the_ledger_only() {
                let rig = Rig::new();
                rig.worker.on_create(Ok(ws_id()));
                rig.worker.on_start_session(Err(WorkerError::Failed(
                    "no such skill factory-build".into(),
                )));
                let run = match claim(&rig.ctx(), ISSUE, rig.machine).unwrap() {
                    ClaimOutcome::Claimed { run } => run,
                    other => panic!("{other:?}"),
                };
                let mut rig = rig;
                pass(&rig, &run);
                show_workspace(&mut rig);
                let step = pass(&rig, &run);
                assert_eq!(
                    step,
                    StartStep::Stuck {
                        reason: LAUNCH_FAILED.into()
                    }
                );
                let comment = stuck_comment(&rig);
                assert!(comment.contains(LAUNCH_FAILED));
                assert!(!comment.contains("factory-build"));
                let stuck = rig
                    .events(&run)
                    .into_iter()
                    .find(|e| e.event == "stuck")
                    .unwrap();
                let reason = stuck.data["reason"].as_str().unwrap();
                assert!(reason.contains("no such skill factory-build"));
                assert_eq!(rig.sim.labels(), vec!["factory:stuck"]);
            }

            /// A launch-failed stuck path cut short after its decision is finished by the next
            /// pass with the same detail in the `stuck` line.
            #[test]
            fn a_resumed_launch_failed_stuck_path_keeps_the_detail_in_the_ledger() {
                let rig = Rig::new();
                rig.worker.on_create(Ok(ws_id()));
                rig.worker.on_start_session(Err(WorkerError::Failed(
                    "no such skill factory-build".into(),
                )));
                let run = match claim(&rig.ctx(), ISSUE, rig.machine).unwrap() {
                    ClaimOutcome::Claimed { run } => run,
                    other => panic!("{other:?}"),
                };
                let mut rig = rig;
                pass(&rig, &run);
                show_workspace(&mut rig);
                rig.sim.fail("issue comment", 1);
                assert!(advance(&rig.ctx(), &run).is_err());
                assert!(!rig.names(&run).contains(&"stuck".to_string()));
                assert!(matches!(pass(&rig, &run), StartStep::Stuck { .. }));
                let stuck = rig
                    .events(&run)
                    .into_iter()
                    .find(|e| e.event == "stuck")
                    .unwrap();
                let reason = stuck.data["reason"].as_str().unwrap();
                assert!(reason.contains("no such skill factory-build"), "{reason}");
                assert!(!stuck_comment(&rig).contains("factory-build"));
            }

            /// The runner's case "a first launch that never went out has no such note": a
            /// refusal on the first start says no session may exist.
            #[test]
            fn a_first_launch_that_never_went_out_has_no_such_note() {
                let rig = Rig::new();
                rig.worker.on_create(Ok(ws_id()));
                rig.worker
                    .on_start_session(Err(WorkerError::Failed("refused".into())));
                let run = match claim(&rig.ctx(), ISSUE, rig.machine).unwrap() {
                    ClaimOutcome::Claimed { run } => run,
                    other => panic!("{other:?}"),
                };
                let mut rig = rig;
                pass(&rig, &run);
                show_workspace(&mut rig);
                pass(&rig, &run);
                let stuck = rig
                    .events(&run)
                    .into_iter()
                    .find(|e| e.event == "stuck")
                    .unwrap();
                let reason = stuck.data["reason"].as_str().unwrap();
                assert!(reason.contains("refused"));
                assert!(!reason.contains(crate::factory::messages::SESSION_MAY_EXIST));
            }

            /// The runner's case "a launch that cannot be sent after an earlier one may have been
            /// says a session may exist": the first start's answer was lost, the next is refused.
            #[test]
            fn a_launch_that_cannot_be_sent_after_an_earlier_one_may_have_been_says_a_session_may_exist()
             {
                let rig = Rig::new();
                rig.worker.on_create(Ok(ws_id()));
                rig.worker
                    .on_start_session(Err(WorkerError::Unreachable("did not answer".into())));
                rig.worker
                    .on_start_session(Err(WorkerError::Failed("refused".into())));
                let run = match claim(&rig.ctx(), ISSUE, rig.machine).unwrap() {
                    ClaimOutcome::Claimed { run } => run,
                    other => panic!("{other:?}"),
                };
                let mut rig = rig;
                pass(&rig, &run);
                show_workspace(&mut rig);
                assert!(advance(&rig.ctx(), &run).is_err());
                pass(&rig, &run);
                let stuck = rig
                    .events(&run)
                    .into_iter()
                    .find(|e| e.event == "stuck")
                    .unwrap();
                let reason = stuck.data["reason"].as_str().unwrap();
                assert!(reason.contains(crate::factory::messages::SESSION_MAY_EXIST));
                assert!(reason.contains("foundry:"));
            }

            /// The runner's case "a launch with no room waits in triage, and the next pass sends
            /// the same launch again": a machine that cannot be reached is no refusal.
            #[test]
            fn a_launch_with_no_room_waits_in_triage_and_the_next_pass_sends_the_same_launch_again()
            {
                let rig = Rig::new();
                rig.worker.on_create(Ok(ws_id()));
                rig.worker
                    .on_start_session(Err(WorkerError::Unreachable("down".into())));
                rig.worker.on_start_session(Ok(session()));
                let run = match claim(&rig.ctx(), ISSUE, rig.machine).unwrap() {
                    ClaimOutcome::Claimed { run } => run,
                    other => panic!("{other:?}"),
                };
                let mut rig = rig;
                pass(&rig, &run);
                show_workspace(&mut rig);
                assert!(advance(&rig.ctx(), &run).is_err());
                assert_eq!(rig.sim.labels(), vec!["factory:triage"]);
                assert!(!rig.finished(&run));
                assert_eq!(pass(&rig, &run), StartStep::Started { session: session() });
                match &start_sessions(&rig)[..] {
                    [one, two] => assert_eq!(one, two),
                    other => panic!("{other:?}"),
                }
                // The first may have started: the sessions of the workspace were ended once.
                assert_eq!(
                    calls(&rig)
                        .iter()
                        .filter(|call| matches!(call, Call::EndSessions { .. }))
                        .count(),
                    1
                );
            }

            /// The runner's case "writes the launch-sending step once and sends under the same
            /// command id".
            #[test]
            fn writes_the_launch_sending_step_once_and_sends_under_the_same_command_id() {
                let rig = Rig::new();
                rig.worker.on_create(Ok(ws_id()));
                rig.worker
                    .on_start_session(Err(WorkerError::Unreachable("down".into())));
                rig.worker.on_start_session(Ok(session()));
                let run = match claim(&rig.ctx(), ISSUE, rig.machine).unwrap() {
                    ClaimOutcome::Claimed { run } => run,
                    other => panic!("{other:?}"),
                };
                let mut rig = rig;
                pass(&rig, &run);
                show_workspace(&mut rig);
                let _ = advance(&rig.ctx(), &run);
                pass(&rig, &run);
                assert_eq!(
                    rig.steps(&run)
                        .iter()
                        .filter(|s| *s == "launch-sending")
                        .count(),
                    1
                );
            }

            /// A launch that was sent and not recorded, whose session the fleet shows, is
            /// adopted and not started twice.
            #[test]
            fn a_session_the_fleet_shows_for_a_launch_that_may_have_gone_out_is_adopted() {
                let (mut rig, run) = workspace_shown();
                // The step was written, the call went out, the ledger line was lost.
                rig.step(&run, "approval-rechecked");
                rig.step(&run, "launch-sending");
                rig.view = FleetView::builder()
                    .online(rig.machine)
                    .workspace(workspace_record(rig.machine, "fac/8-1", None))
                    .session(session_record(rig.machine, SessionState::Working))
                    .turn(foundry_proto::factory::SessionTurn {
                        session: session(),
                        workspace: ws_id(),
                        project: project_id(),
                        turn: 1,
                        prompts: 1,
                        question: false,
                    })
                    .build();
                assert_eq!(pass(&rig, &run), StartStep::Started { session: session() });
                assert!(start_sessions(&rig).is_empty());
                assert_eq!(rig.sim.labels(), vec!["factory:building"]);
                assert!(rig.names(&run).contains(&"session_launched".to_string()));
            }

            #[test]
            fn a_lost_lease_starts_nothing_and_writes_nothing() {
                let (mut rig, run) = workspace_shown();
                rig.guard = Box::new(Lost);
                let writes = rig.sim.writes().len();
                assert!(matches!(
                    advance(&rig.ctx(), &run),
                    Err(RunError::LeaseLost(_))
                ));
                assert!(start_sessions(&rig).is_empty());
                assert_eq!(rig.sim.writes().len(), writes);
                // No launch is recorded as sent that never went out.
                assert!(!rig.steps(&run).contains(&"launch-sending".to_string()));
            }

            #[test]
            fn a_session_already_launched_is_not_started_again() {
                let (rig, run) = started();
                pass(&rig, &run);
                pass(&rig, &run);
                assert_eq!(start_sessions(&rig).len(), 1);
            }
        }

        /// The spec's step 3, the launch asked for again by count.
        mod the_launch_asked_for_again {
            use super::*;

            pub(super) fn show_session(
                rig: &mut Rig,
                state: SessionState,
                turn: Option<(u64, u64)>,
            ) {
                let mut view = FleetView::builder()
                    .online(rig.machine)
                    .workspace(workspace_record(rig.machine, "fac/8-1", None))
                    .session(session_record(rig.machine, state));
                if let Some((turn, prompts)) = turn {
                    view = view.turn(foundry_proto::factory::SessionTurn {
                        session: session(),
                        workspace: ws_id(),
                        project: project_id(),
                        turn,
                        prompts,
                        question: false,
                    });
                }
                rig.view = view.build();
            }

            pub(super) fn sends(rig: &Rig) -> Vec<Call> {
                calls(rig)
                    .into_iter()
                    .filter(|call| matches!(call, Call::Send { .. }))
                    .collect()
            }

            #[test]
            fn a_session_idle_a_pass_after_the_launch_is_asked_for_the_launch_again_by_count() {
                let (mut rig, run) = started();
                show_session(&mut rig, SessionState::Idle, None);
                rig.worker.on_send(Ok(0));
                assert_eq!(pass(&rig, &run), StartStep::Resent { session: session() });
                assert_eq!(
                    sends(&rig),
                    vec![Call::Send {
                        session: session(),
                        message: launch_message(),
                        only_if_prompts: Some(0),
                    }]
                );
                assert!(rig.steps(&run).contains(&"launch-resending 0".to_string()));
            }

            #[test]
            fn the_pass_that_started_the_session_sends_nothing() {
                let (rig, _run) = started();
                assert!(sends(&rig).is_empty());
            }

            #[test]
            fn a_session_idle_with_a_turn_zero_record_and_no_prompts_is_asked_again() {
                let (mut rig, run) = started();
                show_session(&mut rig, SessionState::Idle, Some((0, 0)));
                rig.worker.on_send(Ok(0));
                assert_eq!(pass(&rig, &run), StartStep::Resent { session: session() });
            }

            /// The spec: a session that is idle because its turn ended, as a build session is
            /// for the whole gate, has taken its message and gets nothing again.
            #[test]
            fn a_session_idle_because_its_turn_ended_gets_nothing() {
                let (mut rig, run) = started();
                show_session(&mut rig, SessionState::Idle, Some((1, 1)));
                assert_eq!(pass(&rig, &run), StartStep::Started { session: session() });
                assert!(sends(&rig).is_empty());
            }

            #[test]
            fn a_session_whose_prompt_count_rose_is_not_asked_again() {
                let (mut rig, run) = started();
                show_session(&mut rig, SessionState::Idle, Some((0, 1)));
                pass(&rig, &run);
                assert!(sends(&rig).is_empty());
            }

            #[test]
            fn a_session_that_is_working_is_left_alone() {
                let (mut rig, run) = started();
                show_session(&mut rig, SessionState::Working, None);
                pass(&rig, &run);
                assert!(sends(&rig).is_empty());
            }

            /// The worker decides: its count rose, so the message was typed. Nothing more.
            #[test]
            fn a_refusal_because_the_count_rose_means_it_was_typed() {
                let (mut rig, run) = started();
                show_session(&mut rig, SessionState::Idle, None);
                rig.worker.on_send(Err(SendRefused::CountRose { now: 1 }));
                assert_eq!(pass(&rig, &run), StartStep::Started { session: session() });
            }

            #[test]
            fn a_refusal_because_the_session_is_not_idle_is_looked_at_again_next_pass() {
                let (mut rig, run) = started();
                show_session(&mut rig, SessionState::Idle, None);
                rig.worker.on_send(Err(SendRefused::NotIdle));
                rig.worker.on_send(Ok(0));
                assert_eq!(pass(&rig, &run), StartStep::Started { session: session() });
                assert_eq!(pass(&rig, &run), StartStep::Resent { session: session() });
                assert_eq!(sends(&rig).len(), 2);
            }

            #[test]
            fn is_asked_once_per_pass() {
                let (mut rig, run) = started();
                show_session(&mut rig, SessionState::Idle, None);
                rig.worker.on_send(Ok(0));
                pass(&rig, &run);
                assert_eq!(sends(&rig).len(), 1);
            }

            #[test]
            fn a_send_that_could_not_reach_the_machine_is_an_error() {
                let (mut rig, run) = started();
                show_session(&mut rig, SessionState::Idle, None);
                rig.worker.on_send(Err(SendRefused::Failed {
                    reason: "the machine is down".into(),
                    prompts: None,
                }));
                assert!(matches!(
                    advance(&rig.ctx(), &run),
                    Err(RunError::Send(SendRefused::Failed { .. }))
                ));
            }
        }

        mod an_issue_set_ready_again {
            use super::*;

            pub(super) fn url(pr: u64) -> String {
                format!("https://github.com/{REPO}/pull/{pr}")
            }

            /// A second attempt: the first ended and the issue was set `ready` again.
            pub(super) fn second_attempt(rig: &Rig) -> RunId {
                let first = run_id(1);
                rig.step(&first, &format!("approved {LABELED_AT}"));
                rig.moved(&first, State::Ready, State::Triage);
                rig.moved(&first, State::Triage, State::NeedsInfo);
                let _ = rig.ledger.finish(
                    &first,
                    Outcome::NeedsInfo,
                    None,
                    None,
                    &Cause::new(CauseStep::Start, "test"),
                    None,
                );
                match claim(&rig.ctx(), ISSUE, rig.machine).unwrap() {
                    ClaimOutcome::Claimed { run } => run,
                    other => panic!("{other:?}"),
                }
            }

            pub(super) fn rig_for_second_attempt() -> Rig {
                let rig = Rig::new();
                rig.worker.on_create(Ok(ws_id()));
                rig.worker.on_start_session(Ok(session()));
                rig
            }

            pub(super) fn first_create(rig: &Rig) -> crate::factory::worker::CreateWorkspace {
                match &creates(rig)[..] {
                    [Call::Create { request, .. }] => request.clone(),
                    other => panic!("{other:?}"),
                }
            }

            pub(super) fn show_second(rig: &mut Rig, name: &str) {
                rig.view = FleetView::builder()
                    .online(rig.machine)
                    .workspace(workspace_record(rig.machine, name, None))
                    .build();
            }

            /// Acceptance: an issue set `ready` again that has an open pull request of the
            /// project's login carries on from that pull request's branch, as the runner's
            /// `pr_adopted` does.
            #[test]
            fn with_an_open_pull_request_of_the_logins_carries_on_from_its_branch() {
                let mut rig = rig_for_second_attempt();
                rig.sim.closing_pull(12, "fac/8-1", "main", LOGIN, false);
                let run = second_attempt(&rig);
                assert_eq!(run, run_id(2));
                assert_eq!(pass(&rig, &run), StartStep::Waiting(Wait::Workspace));
                let request = first_create(&rig);
                assert_eq!(request.name, "fac/8-2");
                assert_eq!(
                    request.checkout,
                    Checkout::Branch {
                        name: "fac/8-1".into()
                    }
                );
                let steps = rig.steps(&run);
                let pr = steps
                    .iter()
                    .position(|s| s == &format!("launch-pull-request fac/8-1 {}", url(12)))
                    .unwrap();
                let resuming = steps
                    .iter()
                    .position(|s| s == "launch-resuming fac/8-1")
                    .unwrap();
                let requested = steps
                    .iter()
                    .position(|s| s == "workspace-requested fac/8-2")
                    .unwrap();
                assert!(pr < resuming && resuming < requested);
                // The workspace is shown, and the session starts from the pull request.
                rig.view = FleetView::builder()
                    .online(rig.machine)
                    .workspace(workspace_record(rig.machine, "fac/8-2", None))
                    .build();
                assert_eq!(pass(&rig, &run), StartStep::Started { session: session() });
                match &start_sessions(&rig)[..] {
                    [Call::StartSession { first, .. }] => assert_eq!(
                        first,
                        &messages::resumed(
                            REPO,
                            ISSUE,
                            LOGIN,
                            &ResumeFrom::ExistingPr {
                                branch: "fac/8-1".into(),
                                pr_url: url(12)
                            },
                            None,
                            BODY_PATH
                        )
                    ),
                    other => panic!("{other:?}"),
                }
                let launched = rig
                    .events(&run)
                    .into_iter()
                    .find(|e| e.event == "session_launched")
                    .unwrap();
                assert_eq!(launched.data["resumed_from"]["branch"], "fac/8-1");
                assert_eq!(launched.data["resumed_from"]["run"], run.to_string());
                assert!(
                    rig.steps(&run)
                        .contains(&"launch-sending resumed-from fac/8-1".to_string())
                );
            }

            /// The runner's case "matches the factory's login on GitHub whatever its case".
            #[test]
            fn matches_the_factory_s_login_on_github_whatever_its_case() {
                let rig = rig_for_second_attempt();
                rig.sim
                    .closing_pull(12, "fac/8-1", "main", "SoftFac-Bot", false);
                let run = second_attempt(&rig);
                pass(&rig, &run);
                assert_eq!(
                    first_create(&rig).checkout,
                    Checkout::Branch {
                        name: "fac/8-1".into()
                    }
                );
            }

            /// The runner's case "does not take another login's pull request when no host has
            /// that login".
            #[test]
            fn does_not_take_another_login_s_pull_request_when_no_host_has_that_login() {
                let rig = rig_for_second_attempt();
                rig.sim
                    .closing_pull(12, "fac/8-1", "main", "someone-else", false);
                let run = second_attempt(&rig);
                pass(&rig, &run);
                assert!(matches!(
                    first_create(&rig).checkout,
                    Checkout::NewBranch { .. }
                ));
            }

            /// The runner's case "finds an open pull request by the Paradev GitHub App only on a
            /// cloud launch branch of the issue, for a repo with a cloud environment": Foundry
            /// has no cloud launch, so an app's pull request is never the run's own.
            #[test]
            fn finds_an_open_pull_request_by_the_paradev_github_app_only_on_a_cloud_launch_branch_of_the_issue_for_a_repo_with_a_cloud_environment()
             {
                let rig = rig_for_second_attempt();
                rig.sim
                    .closing_pull(12, "paradev/8-abc", "main", "superset-app", false);
                let run = second_attempt(&rig);
                pass(&rig, &run);
                assert!(matches!(
                    first_create(&rig).checkout,
                    Checkout::NewBranch { .. }
                ));
            }

            /// The runner's case "takes the newest open pull request of the factory's own on
            /// GitHub".
            #[test]
            fn takes_the_newest_open_pull_request_of_the_factory_s_own_on_github() {
                let rig = rig_for_second_attempt();
                rig.sim.closing_pull(12, "fac/8-old", "main", LOGIN, false);
                rig.sim.closing_pull(14, "fac/8-new", "main", LOGIN, false);
                rig.sim.closing_pull(13, "fac/8-mid", "main", LOGIN, false);
                let run = second_attempt(&rig);
                pass(&rig, &run);
                assert_eq!(
                    first_create(&rig).checkout,
                    Checkout::Branch {
                        name: "fac/8-new".into()
                    }
                );
            }

            /// The runner's case "passes over pull requests that are not the factory's own on
            /// this base".
            #[test]
            fn passes_over_pull_requests_that_are_not_the_factory_s_own_on_this_base() {
                let rig = rig_for_second_attempt();
                rig.sim.closing_pull(12, "fac/8-fork", "main", LOGIN, true);
                rig.sim
                    .closing_pull(13, "fac/8-other-base", "release", LOGIN, false);
                rig.sim.closing_pull(14, "-x", "main", LOGIN, false);
                rig.sim.closing_pull(15, "fac/8-ok", "main", LOGIN, false);
                let run = second_attempt(&rig);
                pass(&rig, &run);
                assert_eq!(
                    first_create(&rig).checkout,
                    Checkout::Branch {
                        name: "fac/8-ok".into()
                    }
                );
            }

            /// The runner's case "after needs-info, continues the pull request the earlier
            /// run's ledger names".
            #[test]
            fn after_needs_info_continues_the_pull_request_the_earlier_run_s_ledger_names() {
                let rig = rig_for_second_attempt();
                rig.write(
                    &run_id(1),
                    Event::PrOpened {
                        pr: 12,
                        url: url(12),
                    },
                );
                rig.sim.set_pull(
                    12,
                    SimPull {
                        state: "OPEN",
                        draft: true,
                        head: "fac/8-1".into(),
                        base: "main".into(),
                        cross: false,
                    },
                );
                let run = second_attempt(&rig);
                pass(&rig, &run);
                assert_eq!(
                    first_create(&rig).checkout,
                    Checkout::Branch {
                        name: "fac/8-1".into()
                    }
                );
                assert!(
                    rig.steps(&run)
                        .contains(&format!("launch-pull-request fac/8-1 {}", url(12)))
                );
            }

            /// The runner's case "continues the pull request a stopping session left open".
            #[test]
            fn continues_the_pull_request_a_stopping_session_left_open() {
                let rig = rig_for_second_attempt();
                rig.write(
                    &run_id(1),
                    Event::PrLeft {
                        pr: 12,
                        url: url(12),
                    },
                );
                rig.sim.set_pull(
                    12,
                    SimPull {
                        state: "OPEN",
                        draft: true,
                        head: "fac/8-left".into(),
                        base: "main".into(),
                        cross: false,
                    },
                );
                let run = second_attempt(&rig);
                pass(&rig, &run);
                assert_eq!(
                    first_create(&rig).checkout,
                    Checkout::Branch {
                        name: "fac/8-left".into()
                    }
                );
            }

            /// The runner's case "falls back to a plain start when the ledger's pull request is
            /// no longer open".
            #[test]
            fn falls_back_to_a_plain_start_when_the_ledger_s_pull_request_is_no_longer_open() {
                let rig = rig_for_second_attempt();
                rig.write(
                    &run_id(1),
                    Event::PrOpened {
                        pr: 12,
                        url: url(12),
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
                let run = second_attempt(&rig);
                pass(&rig, &run);
                assert!(matches!(
                    first_create(&rig).checkout,
                    Checkout::NewBranch { .. }
                ));
            }

            /// The runner's case "sent again, reuses the recorded pull request and asks GitHub
            /// nothing": the decision is in the ledger before the workspace is asked for.
            #[test]
            fn sent_again_reuses_the_recorded_pull_request_and_asks_github_nothing() {
                let mut rig = rig_for_second_attempt();
                rig.sim.closing_pull(12, "fac/8-1", "main", LOGIN, false);
                let run = second_attempt(&rig);
                pass(&rig, &run);
                let asked = rig.sim.calls().len();
                // The pull request is gone from GitHub; the run goes on from what it recorded.
                show_second(&mut rig, "fac/8-2");
                assert_eq!(pass(&rig, &run), StartStep::Started { session: session() });
                let later: Vec<String> = rig.sim.calls()[asked..]
                    .iter()
                    .map(|call| call.join(" "))
                    .collect();
                assert!(
                    later
                        .iter()
                        .all(|call| !call.contains("closedByPullRequestsReferences")
                            && !call.starts_with("pr view")
                            && !call.starts_with("pr list")),
                    "{later:#?}"
                );
                match &start_sessions(&rig)[..] {
                    [Call::StartSession { first, .. }] => assert_eq!(
                        first,
                        &messages::resumed(
                            REPO,
                            ISSUE,
                            LOGIN,
                            &ResumeFrom::ExistingPr {
                                branch: "fac/8-1".into(),
                                pr_url: url(12)
                            },
                            None,
                            BODY_PATH
                        )
                    ),
                    other => panic!("{other:?}"),
                }
            }

            pub(super) fn time_limited_first_attempt(rig: &Rig, branch: &str) {
                let first = run_id(1);
                rig.step(&first, &format!("approved {LABELED_AT}"));
                rig.moved(&first, State::Ready, State::Triage);
                rig.write(
                    &first,
                    Event::SessionLaunched {
                        thread_id: session().to_string(),
                        worktree: Some(ws_id().to_string()),
                        branch: Some(branch.into()),
                        model_instance: None,
                        model: None,
                        model_options: None,
                        resumed_from: None,
                        base: None,
                        title: None,
                    },
                );
                rig.moved(&first, State::Triage, State::Building);
                rig.ledger
                    .write(
                        &first,
                        &Event::Stuck {
                            reason: "the run passed its time limit".into(),
                            branch: None,
                            commits_ahead: None,
                        },
                        &Cause::new(CauseStep::Watch, "time-limit"),
                    )
                    .unwrap();
                rig.moved(&first, State::Building, State::Stuck);
                rig.ledger
                    .finish(
                        &first,
                        Outcome::Stuck,
                        None,
                        None,
                        &Cause::new(CauseStep::Watch, "time-limit"),
                        None,
                    )
                    .unwrap();
                rig.sim.set_labels(&["factory:ready"]);
                rig.sim.set_assignees(&[]);
            }

            /// The runner's case "continues from the branch an earlier time-limited run left on
            /// origin".
            #[test]
            fn continues_from_the_branch_an_earlier_time_limited_run_left_on_origin() {
                let rig = rig_for_second_attempt();
                time_limited_first_attempt(&rig, "fac/8-1");
                rig.sim.add_branch("fac/8-1");
                let run = match claim(&rig.ctx(), ISSUE, rig.machine).unwrap() {
                    ClaimOutcome::Claimed { run } => run,
                    other => panic!("{other:?}"),
                };
                pass(&rig, &run);
                assert_eq!(
                    first_create(&rig).checkout,
                    Checkout::Branch {
                        name: "fac/8-1".into()
                    }
                );
                let mut rig = rig;
                show_second(&mut rig, "fac/8-2");
                pass(&rig, &run);
                match &start_sessions(&rig)[..] {
                    [Call::StartSession { first, .. }] => assert_eq!(
                        first,
                        &messages::resumed(
                            REPO,
                            ISSUE,
                            LOGIN,
                            &ResumeFrom::TimeLimit {
                                run: run_id(1),
                                branch: "fac/8-1".into()
                            },
                            None,
                            BODY_PATH
                        )
                    ),
                    other => panic!("{other:?}"),
                }
            }

            /// The runner's case "starts from base when the branch is not on origin".
            #[test]
            fn starts_from_base_when_the_branch_is_not_on_origin() {
                let rig = rig_for_second_attempt();
                time_limited_first_attempt(&rig, "fac/8-1");
                let run = match claim(&rig.ctx(), ISSUE, rig.machine).unwrap() {
                    ClaimOutcome::Claimed { run } => run,
                    other => panic!("{other:?}"),
                };
                pass(&rig, &run);
                assert!(matches!(
                    first_create(&rig).checkout,
                    Checkout::NewBranch { .. }
                ));
            }

            /// The runner's case "is not made from a branch name that could be taken for a git
            /// option".
            #[test]
            fn is_not_made_from_a_branch_name_that_could_be_taken_for_a_git_option() {
                let rig = rig_for_second_attempt();
                time_limited_first_attempt(&rig, "-x");
                rig.sim.add_branch("-x");
                let run = match claim(&rig.ctx(), ISSUE, rig.machine).unwrap() {
                    ClaimOutcome::Claimed { run } => run,
                    other => panic!("{other:?}"),
                };
                pass(&rig, &run);
                assert!(matches!(
                    first_create(&rig).checkout,
                    Checkout::NewBranch { .. }
                ));
            }

            /// The runner's case "is not made when the issue text changed since the earlier
            /// approval".
            #[test]
            fn is_not_made_when_the_issue_text_changed_since_the_earlier_approval() {
                let rig = rig_for_second_attempt();
                time_limited_first_attempt(&rig, "fac/8-1");
                rig.sim.add_branch("fac/8-1");
                rig.sim.set_approval(approval_answer(
                    "2026-10-04T12:00:00Z",
                    "tom",
                    &[("2026-10-04T11:00:00Z", "tom")],
                ));
                let run = match claim(&rig.ctx(), ISSUE, rig.machine).unwrap() {
                    ClaimOutcome::Claimed { run } => run,
                    other => panic!("{other:?}"),
                };
                pass(&rig, &run);
                assert!(matches!(
                    first_create(&rig).checkout,
                    Checkout::NewBranch { .. }
                ));
            }

            /// The runner's case "is an error that leaves the issue in triage when the branch
            /// cannot be looked up".
            #[test]
            fn is_an_error_that_leaves_the_issue_in_triage_when_the_branch_cannot_be_looked_up() {
                let rig = rig_for_second_attempt();
                time_limited_first_attempt(&rig, "fac/8-1");
                let run = match claim(&rig.ctx(), ISSUE, rig.machine).unwrap() {
                    ClaimOutcome::Claimed { run } => run,
                    other => panic!("{other:?}"),
                };
                rig.sim.fail("git/ref/heads", 1);
                assert!(advance(&rig.ctx(), &run).is_err());
                assert_eq!(rig.sim.labels(), vec!["factory:triage"]);
                // Nothing was decided or asked: the next pass decides again.
                assert!(creates(&rig).is_empty());
                assert!(
                    !rig.steps(&run)
                        .iter()
                        .any(|s| s.starts_with("workspace-requested"))
                );
            }

            /// The runner's case "a first attempt is a plain start".
            #[test]
            fn a_first_attempt_is_a_plain_start() {
                let (rig, run) = claimed_rig();
                pass(&rig, &run);
                assert!(matches!(
                    first_create(&rig).checkout,
                    Checkout::NewBranch { .. }
                ));
                assert!(
                    !rig.steps(&run)
                        .iter()
                        .any(|s| s.starts_with("launch-pull-request")
                            || s.starts_with("launch-resuming"))
                );
            }
        }

        mod an_issue_that_names_its_base {
            use super::*;

            pub(super) fn claimed_with_body(body: &str) -> (Rig, RunId) {
                let rig = Rig::new();
                rig.worker.on_create(Ok(ws_id()));
                rig.worker.on_start_session(Ok(session()));
                rig.sim.set_body(body);
                let run = match claim(&rig.ctx(), ISSUE, rig.machine).unwrap() {
                    ClaimOutcome::Claimed { run } => run,
                    other => panic!("{other:?}"),
                };
                (rig, run)
            }

            /// The runner's case "launches from the named branch on origin and records it".
            #[test]
            fn launches_from_the_named_branch_on_origin_and_records_it() {
                let (mut rig, run) = claimed_with_body("Do it.\n\n## Base\nrelease/2\n");
                rig.sim.add_branch("release/2");
                pass(&rig, &run);
                match &creates(&rig)[..] {
                    [Call::Create { request, .. }] => assert_eq!(
                        request.checkout,
                        Checkout::NewBranch {
                            base: "release/2".into(),
                            branch: Some("fac/8-1".into())
                        }
                    ),
                    other => panic!("{other:?}"),
                }
                assert!(
                    rig.steps(&run)
                        .contains(&"launch-base release/2".to_string())
                );
                show_workspace(&mut rig);
                pass(&rig, &run);
                match &start_sessions(&rig)[..] {
                    [Call::StartSession { first, .. }] => assert_eq!(
                        first,
                        &messages::launch(REPO, ISSUE, LOGIN, Some("release/2"), BODY_PATH)
                    ),
                    other => panic!("{other:?}"),
                }
                let launched = rig
                    .events(&run)
                    .into_iter()
                    .find(|e| e.event == "session_launched")
                    .unwrap();
                assert_eq!(launched.data["base"], "release/2");
            }

            /// A decision cut short before the workspace was asked for leaves no base to the
            /// decision taken again: the issue no longer names one, so the launch is on the
            /// project's base.
            #[test]
            fn a_decision_cut_short_leaves_no_base_to_the_next_decision() {
                let (mut rig, run) = claimed_with_body("Do it.");
                rig.step(&run, "launch-model sonnet");
                rig.step(&run, "launch-base release/2");
                pass(&rig, &run);
                match &creates(&rig)[..] {
                    [Call::Create { request, .. }] => assert_eq!(
                        request.checkout,
                        Checkout::NewBranch {
                            base: "main".into(),
                            branch: Some("fac/8-1".into())
                        }
                    ),
                    other => panic!("{other:?}"),
                }
                show_workspace(&mut rig);
                pass(&rig, &run);
                match &start_sessions(&rig)[..] {
                    [Call::StartSession { first, .. }] => assert_eq!(
                        first,
                        &messages::launch(REPO, ISSUE, LOGIN, None, BODY_PATH)
                    ),
                    other => panic!("{other:?}"),
                }
                let launched = rig
                    .events(&run)
                    .into_iter()
                    .find(|e| e.event == "session_launched")
                    .unwrap();
                assert_eq!(launched.data["base"], "main");
            }

            /// The runner's case "an issue with no base records the project's base and says
            /// nothing more".
            #[test]
            fn an_issue_with_no_base_records_the_project_s_base_and_says_nothing_more() {
                let (mut rig, run) = claimed_with_body("Do it.");
                pass(&rig, &run);
                show_workspace(&mut rig);
                pass(&rig, &run);
                assert!(!rig.steps(&run).iter().any(|s| s.starts_with("launch-base")));
                let launched = rig
                    .events(&run)
                    .into_iter()
                    .find(|e| e.event == "session_launched")
                    .unwrap();
                assert_eq!(launched.data["base"], "main");
            }

            /// The runner's case "a base that is not a branch name goes stuck before any launch
            /// is sent".
            #[test]
            fn a_base_that_is_not_a_branch_name_goes_stuck_before_any_launch_is_sent() {
                let (rig, run) = claimed_with_body("## Base\nrelease..2\n");
                let step = pass(&rig, &run);
                assert_eq!(
                    step,
                    StartStep::Stuck {
                        reason: messages::base_not_a_branch("release..2")
                    }
                );
                assert!(calls(&rig).is_empty());
                assert_eq!(rig.sim.labels(), vec!["factory:stuck"]);
            }

            /// The runner's case "a base that origin does not have goes stuck before any launch
            /// is sent".
            #[test]
            fn a_base_that_origin_does_not_have_goes_stuck_before_any_launch_is_sent() {
                let (rig, run) = claimed_with_body("## Base\nrelease/2\n");
                let step = pass(&rig, &run);
                assert_eq!(
                    step,
                    StartStep::Stuck {
                        reason: messages::base_not_on_origin("release/2")
                    }
                );
                assert!(calls(&rig).is_empty());
            }

            /// The runner's case "a base with an empty or dot segment is not a branch name".
            #[test]
            fn a_base_with_an_empty_or_dot_segment_is_not_a_branch_name() {
                for base in ["release//2", "release/./2", "release/../2"] {
                    let (rig, run) = claimed_with_body(&format!("## Base\n{base}\n"));
                    assert_eq!(
                        pass(&rig, &run),
                        StartStep::Stuck {
                            reason: messages::base_not_a_branch(base)
                        },
                        "{base}"
                    );
                }
            }

            /// The runner's case "a base whose lookup fails claims and waits for the next pass".
            #[test]
            fn a_base_whose_lookup_fails_claims_and_waits_for_the_next_pass() {
                let (rig, run) = claimed_with_body("## Base\nrelease/2\n");
                rig.sim.add_branch("release/2");
                rig.sim.fail("git/ref/heads", 1);
                assert!(advance(&rig.ctx(), &run).is_err());
                assert!(creates(&rig).is_empty());
                assert_eq!(rig.sim.labels(), vec!["factory:triage"]);
                assert_eq!(pass(&rig, &run), StartStep::Waiting(Wait::Workspace));
            }

            /// The runner's case "the first send records the base it decided before the launch".
            #[test]
            fn the_first_send_records_the_base_it_decided_before_the_launch() {
                let (rig, run) = claimed_with_body("## Base\nrelease/2\n");
                rig.sim.add_branch("release/2");
                pass(&rig, &run);
                let steps = rig.steps(&run);
                let base = steps
                    .iter()
                    .position(|s| s == "launch-base release/2")
                    .unwrap();
                let requested = steps
                    .iter()
                    .position(|s| s.starts_with("workspace-requested"))
                    .unwrap();
                assert!(base < requested);
            }
        }

        /// The runner's `start.test.ts` cases for the claim and the launch that the groups above
        /// do not carry under their own names. Where the runner's case has no counterpart in
        /// Foundry (T3, a host or cloud launch, a model cooling down, claims of several
        /// machines), the test keeps the runner's name and asserts what replaces it.
        mod a_fresh_ready_issue {
            use super::*;

            /// The runner's case, where the host message names the machine's resolved login: in
            /// Foundry there is one login for a project, and the one launch message names it,
            /// whichever machine builds.
            #[test]
            fn sends_the_host_message_naming_the_machine_s_resolved_login_the_runner_s_login_for_a_machine_with_none()
             {
                let (rig, _run) = started();
                match &start_sessions(&rig)[..] {
                    [Call::StartSession { login, first, .. }] => {
                        assert_eq!(login, LOGIN);
                        assert_eq!(first, &launch_message());
                    }
                    other => panic!("{other:?}"),
                }
            }

            /// The runner's case, where the cloud message names no login beside the local and
            /// host ones: in Foundry there is no cloud launch, so a build on another machine
            /// sends the very same message.
            #[test]
            fn sends_the_cloud_message_which_names_no_login_beside_the_local_and_host_ones() {
                let rig = Rig::new();
                rig.worker.on_create(Ok(ws_id()));
                rig.worker.on_start_session(Ok(session()));
                let run = match claim(&rig.ctx(), ISSUE, elsewhere()).unwrap() {
                    ClaimOutcome::Claimed { run } => run,
                    other => panic!("{other:?}"),
                };
                let mut rig = rig;
                assert_eq!(pass(&rig, &run), StartStep::Waiting(Wait::Workspace));
                rig.view = FleetView::builder()
                    .online(elsewhere())
                    .workspace(workspace_record(elsewhere(), "fac/8-1", None))
                    .build();
                assert_eq!(pass(&rig, &run), StartStep::Started { session: session() });
                match &start_sessions(&rig)[..] {
                    [Call::StartSession { first, .. }] => assert_eq!(first, &launch_message()),
                    other => panic!("{other:?}"),
                }
            }

            /// The runner's case, where `launchModel` picks a model while the project's cools
            /// down: in Foundry there is no cooldown model, the session starts on the fleet's
            /// model and effort, and the launch records them before the workspace is asked for.
            #[test]
            fn launches_on_the_model_launchmodel_picks_while_the_project_s_model_cools_down() {
                let mut rig = Rig::new();
                rig.settings.fleet.model = "claude-opus-4-1".into();
                rig.settings.fleet.effort = "high".into();
                rig.worker.on_create(Ok(ws_id()));
                rig.worker.on_start_session(Ok(session()));
                let run = claim_run(&rig);
                pass(&rig, &run);
                assert!(
                    rig.steps(&run)
                        .contains(&"launch-model claude-opus-4-1".to_string())
                );
                show_workspace(&mut rig);
                pass(&rig, &run);
                match &start_sessions(&rig)[..] {
                    [Call::StartSession { model, effort, .. }] => {
                        assert_eq!(model, "claude-opus-4-1");
                        assert_eq!(effort, "high");
                    }
                    other => panic!("{other:?}"),
                }
            }

            /// The runner's case: a launch decided again writes its model over one a crash left
            /// unsent, and the newest `launch-model` step is the one read.
            #[test]
            fn a_launch_decided_again_writes_its_model_over_one_a_crash_left_unsent() {
                let (rig, run) = claimed_rig();
                rig.step(&run, "launch-model stale-model");
                pass(&rig, &run);
                let fleet = rig.settings.fleet.model.clone();
                assert_eq!(
                    rig.steps(&run)
                        .into_iter()
                        .rfind(|s| s.starts_with("launch-model "))
                        .unwrap(),
                    format!("launch-model {fleet}")
                );
                assert_eq!(read_progress(&rig.events(&run)).launch_model, Some(fleet));
            }

            /// The runner's case, where the project's model carries provider options: in
            /// Foundry the launch line records the fleet's effort as the model's one option.
            #[test]
            fn records_the_provider_options_the_project_s_model_carries() {
                let (rig, run) = started();
                let launched = rig
                    .events(&run)
                    .into_iter()
                    .find(|e| e.event == "session_launched")
                    .unwrap();
                let options = &launched.data["model_options"];
                assert_eq!(options[0]["id"], "effort");
                assert_eq!(options[0]["value"], rig.settings.fleet.effort.as_str());
                assert_eq!(launched.data["model"], rig.settings.fleet.model.as_str());
            }

            /// The runner's case, where the launch line has no worktree yet: a line that names
            /// none is read with no worktree, and the progress takes the workspace the step
            /// recorded.
            #[test]
            fn records_a_launch_that_has_no_worktree_yet() {
                let (rig, run) = claimed_rig();
                rig.step(&run, &format!("workspace-created {}", ws_id()));
                rig.write(
                    &run,
                    Event::SessionLaunched {
                        thread_id: session().to_string(),
                        worktree: None,
                        branch: None,
                        model_instance: None,
                        model: None,
                        model_options: None,
                        resumed_from: None,
                        base: None,
                        title: None,
                    },
                );
                let seen = rig.ctx().seen(&run).unwrap();
                assert_eq!(seen.thread_id, Some(session().to_string()));
                assert_eq!(seen.worktree, None);
                let at = crate::factory::runs::progress_from(
                    &run,
                    CauseStep::Start,
                    &seen,
                    State::Building,
                );
                assert_eq!(at.worktree, Some(ws_id().to_string()));
            }
        }

        mod an_issue_in_triage_that_is_ours {
            use super::*;

            /// The runner's case "the new attempt comes after a finished older run, and the
            /// older run is left alone".
            #[test]
            fn the_new_attempt_comes_after_a_finished_older_run_and_the_older_run_is_left_alone() {
                let rig = Rig::new();
                let first = run_id(1);
                rig.step(&first, &format!("approved {LABELED_AT}"));
                rig.moved(&first, State::Ready, State::Triage);
                rig.ledger
                    .finish(
                        &first,
                        Outcome::Stuck,
                        None,
                        None,
                        &Cause::new(CauseStep::Start, "test"),
                        None,
                    )
                    .unwrap();
                let before = rig.events(&first);
                assert_eq!(claim_run(&rig), run_id(2));
                assert_eq!(rig.events(&first), before);
            }

            /// The runner's case "with an unfinished run that records no approval, goes stuck in
            /// that run".
            #[test]
            fn with_an_unfinished_run_that_records_no_approval_goes_stuck_in_that_run() {
                let rig = Rig::new();
                let run = run_id(1);
                rig.sim.set_labels(&["factory:triage"]);
                rig.sim.set_assignees(&[LOGIN]);
                rig.write(
                    &run,
                    Event::Claimed {
                        machine: rig.machine.to_string(),
                        github_user: LOGIN.into(),
                    },
                );
                assert_eq!(
                    pass(&rig, &run),
                    StartStep::Stuck {
                        reason: CANNOT_CONFIRM.into()
                    }
                );
                assert!(rig.finished(&run));
                assert_eq!(rig.sim.labels(), vec!["factory:stuck"]);
                assert!(calls(&rig).is_empty());
            }

            /// The runner's case "with an unfinished run in stuck, finishes it and goes stuck in
            /// a new attempt".
            #[test]
            fn with_an_unfinished_run_in_stuck_finishes_it_and_goes_stuck_in_a_new_attempt() {
                let rig = Rig::new();
                let first = run_id(1);
                rig.step(&first, &format!("approved {LABELED_AT}"));
                rig.moved(&first, State::Ready, State::Triage);
                rig.moved(&first, State::Triage, State::Stuck);
                rig.sim.set_body("## Base\nrelease..2\n");
                let run = claim_run(&rig);
                assert_eq!(run, run_id(2));
                assert!(rig.finished(&first));
                assert_eq!(rig.events(&first).pop().unwrap().data["outcome"], "stuck");
                assert_eq!(
                    pass(&rig, &run),
                    StartStep::Stuck {
                        reason: messages::base_not_a_branch("release..2")
                    }
                );
                assert!(rig.finished(&run));
            }

            /// The runner's case "goes on with a triage run of ours as start does".
            #[test]
            fn goes_on_with_a_triage_run_of_ours_as_start_does() {
                let (rig, run) = claimed_rig();
                assert_eq!(pass(&rig, &run), StartStep::Waiting(Wait::Workspace));
                assert_eq!(creates(&rig).len(), 1);
            }

            /// The runner's case "says nothing moved for a ready issue someone is assigned to".
            #[test]
            fn says_nothing_moved_for_a_ready_issue_someone_is_assigned_to() {
                let (rig, run) = claimed_rig();
                rig.sim.set_labels(&["factory:ready"]);
                rig.sim.set_assignees(&["someone"]);
                let writes = rig.sim.writes();
                assert!(matches!(pass(&rig, &run), StartStep::Skipped { .. }));
                assert_eq!(rig.sim.writes(), writes);
                assert!(calls(&rig).is_empty());
            }

            /// The runner's case "leaves alone a stuck issue whose run never went stuck: a
            /// person stopped it": the issue is not in triage or building, so nothing is done.
            #[test]
            fn leaves_alone_a_stuck_issue_whose_run_never_went_stuck_a_person_stopped_it() {
                let (rig, run) = claimed_rig();
                rig.sim.set_labels(&["factory:stuck"]);
                let before = (rig.events(&run), rig.sim.writes());
                assert!(matches!(pass(&rig, &run), StartStep::Skipped { .. }));
                assert_eq!((rig.events(&run), rig.sim.writes()), before);
                assert!(calls(&rig).is_empty());
            }

            /// The runner's case "leaves a stuck issue alone when its run is finished, or it has
            /// none".
            #[test]
            fn leaves_a_stuck_issue_alone_when_its_run_is_finished_or_it_has_none() {
                let (rig, run) = claimed_rig();
                rig.sim.set_labels(&["factory:stuck"]);
                rig.ledger
                    .finish(
                        &run,
                        Outcome::Stuck,
                        None,
                        None,
                        &Cause::new(CauseStep::Start, "test"),
                        None,
                    )
                    .unwrap();
                assert!(matches!(pass(&rig, &run), StartStep::Skipped { .. }));
                // A run with no events is looked for in the claim comments, and none names it.
                assert!(matches!(pass(&rig, &run_id(5)), StartStep::Skipped { .. }));
                assert!(rig.ledger.read(&run_id(5)).unwrap().is_empty());
                assert!(calls(&rig).is_empty());
            }
        }

        mod the_launch_decided {
            use super::*;

            /// The runner's case, where a repo with no T3 project goes stuck: in Foundry the
            /// project is the one the run belongs to, and the workspace is asked for on it.
            #[test]
            fn a_repo_with_no_t3_project_goes_stuck_before_any_launch_is_sent() {
                let (rig, run) = claimed_rig();
                assert_eq!(pass(&rig, &run), StartStep::Waiting(Wait::Workspace));
                match &creates(&rig)[..] {
                    [Call::Create { request, .. }] => {
                        assert_eq!(request.project, project_id());
                        assert_eq!(request.tag.project, project_id());
                    }
                    other => panic!("{other:?}"),
                }
            }

            /// The runner's case, where a project with no default model goes stuck: in Foundry
            /// the model is the fleet's setting, which is never empty, and the session starts
            /// on it.
            #[test]
            fn a_project_with_no_default_model_goes_stuck_before_any_launch_is_sent() {
                assert!(!fleet_default_model().is_empty());
                let (rig, _run) = started();
                match &start_sessions(&rig)[..] {
                    [Call::StartSession { model, .. }] => assert!(!model.is_empty()),
                    other => panic!("{other:?}"),
                }
            }

            fn fleet_default_model() -> String {
                foundry_proto::factory::FactorySettings::default().model
            }

            /// The runner's case, where a project that is another repo's clone goes stuck: in
            /// Foundry the workspace is tagged with the run and made on the run's own project,
            /// by the run's name.
            #[test]
            fn a_project_that_is_another_repo_s_clone_or_of_none_goes_stuck_before_any_launch() {
                let (rig, run) = claimed_rig();
                pass(&rig, &run);
                match &creates(&rig)[..] {
                    [Call::Create { request, .. }] => {
                        assert_eq!(request.name, "fac/8-1");
                        assert_eq!(request.tag.run, run.to_string());
                    }
                    other => panic!("{other:?}"),
                }
            }

            /// The runner's case, where a T3 that cannot be reached when the project is looked
            /// up claims nothing: in Foundry there is no lookup, and a claim calls no worker.
            #[test]
            fn a_t3_that_cannot_be_reached_when_the_project_is_looked_up_claims_nothing() {
                let rig = Rig::new();
                rig.worker
                    .on_create(Err(WorkerError::Unreachable("down".into())));
                assert!(matches!(
                    claim_it_for_the_launch(&rig),
                    ClaimOutcome::Claimed { .. }
                ));
                assert!(calls(&rig).is_empty());
            }

            fn claim_it_for_the_launch(rig: &Rig) -> ClaimOutcome {
                claim(&rig.ctx(), ISSUE, rig.machine).unwrap()
            }

            /// The runner's case "a second attempt gets the next number and its own command id":
            /// the workspace is named for the attempt and tagged with the run.
            #[test]
            fn a_second_attempt_gets_the_next_number_and_its_own_command_id() {
                let rig = super::an_issue_set_ready_again::rig_for_second_attempt();
                let run = super::an_issue_set_ready_again::second_attempt(&rig);
                assert_eq!(run, run_id(2));
                pass(&rig, &run);
                let request = super::an_issue_set_ready_again::first_create(&rig);
                assert_eq!(request.name, "fac/8-2");
                assert_eq!(request.tag.run, run.to_string());
            }

            /// The runner's case "a text that changed before the second look archives the
            /// session and goes stuck": a start that may have been sent has its sessions ended.
            #[test]
            fn a_text_that_changed_before_the_second_look_archives_the_session_and_goes_stuck() {
                let (rig, run) = workspace_shown();
                rig.step(&run, "launch-sending");
                rig.sim.set_approval(approval_answer(
                    "2026-10-04T12:00:00Z",
                    "tom",
                    &[("2026-10-04T10:30:00Z", "mallory")],
                ));
                assert_eq!(
                    pass(&rig, &run),
                    StartStep::Stuck {
                        reason: TEXT_CHANGED.into()
                    }
                );
                assert!(calls(&rig).contains(&Call::EndSessions { workspace: ws_id() }));
                assert!(start_sessions(&rig).is_empty());
            }

            /// The runner's case "an archive that fails is an error before anything is written,
            /// and the next start retries": the sessions that cannot be ended fail the stuck
            /// path with nothing written, and the next pass ends them and finishes it.
            #[test]
            fn an_archive_that_fails_is_an_error_before_anything_is_written_and_the_next_start_retries()
             {
                let (rig, run) = workspace_shown();
                rig.step(&run, "launch-sending");
                rig.sim.set_approval(approval_answer(
                    "2026-10-04T12:00:00Z",
                    "tom",
                    &[("2026-10-04T10:30:00Z", "mallory")],
                ));
                rig.worker
                    .on_end_sessions(Err(WorkerError::Unreachable("down".into())));
                let writes = rig.sim.writes();
                assert!(advance(&rig.ctx(), &run).is_err());
                assert_eq!(rig.sim.writes(), writes);
                assert_eq!(rig.sim.labels(), vec!["factory:triage"]);
                assert!(!rig.finished(&run));
                assert!(matches!(pass(&rig, &run), StartStep::Stuck { .. }));
                assert!(rig.finished(&run));
                assert_eq!(
                    calls(&rig)
                        .iter()
                        .filter(|call| matches!(call, Call::EndSessions { .. }))
                        .count(),
                    2
                );
            }

            /// The runner's case "the second look comes after the launch is recorded and before
            /// the label": in Foundry the look comes before the launch is sent, and the label
            /// moves after the launch is recorded.
            #[test]
            fn the_second_look_comes_after_the_launch_is_recorded_and_before_the_label() {
                let (rig, run) = started();
                let trail: Vec<String> = rig
                    .events(&run)
                    .into_iter()
                    .map(|event| match event.data["step"].as_str() {
                        Some(step) => step.to_owned(),
                        None => event.event,
                    })
                    .collect();
                let at = |what: &str| trail.iter().position(|t| t == what).unwrap();
                let building = trail.iter().rposition(|t| t == "state_changed").unwrap();
                assert!(at("approval-rechecked") < at("launch-sending"));
                assert!(at("launch-sending") < at("session_launched"));
                assert!(at("session_launched") < building);
            }

            /// The runner's case "a label change that fails after the launch is a runner error
            /// with no rollback": the session stays, and the next pass moves the label.
            #[test]
            fn a_label_change_that_fails_after_the_launch_is_a_runner_error_with_no_rollback() {
                let (rig, run) = workspace_shown();
                rig.sim.fail("--add-label factory:building", 1);
                assert!(advance(&rig.ctx(), &run).is_err());
                assert!(rig.names(&run).contains(&"session_launched".to_string()));
                assert_eq!(rig.sim.labels(), vec!["factory:triage"]);
                assert!(!rig.finished(&run));
                assert!(
                    !calls(&rig)
                        .iter()
                        .any(|call| matches!(call, Call::EndSessions { .. }))
                );
                assert_eq!(pass(&rig, &run), StartStep::Started { session: session() });
                assert_eq!(rig.sim.labels(), vec!["factory:building"]);
                assert_eq!(start_sessions(&rig).len(), 1);
                assert!(
                    rig.events(&run)
                        .iter()
                        .any(|e| e.event == "state_changed" && e.data["to"] == "building")
                );
            }

            /// The runner's case "an unassign that fails in the stuck path is a runner error".
            #[test]
            fn an_unassign_that_fails_in_the_stuck_path_is_a_runner_error() {
                let (mut rig, run) = claimed_rig();
                pass(&rig, &run);
                show_setup(&mut rig, TerminalState::Exited, Some(1));
                rig.worker.on_tail(Ok("boom".into()));
                rig.sim.fail("--remove-assignee", 1);
                assert!(matches!(
                    advance(&rig.ctx(), &run),
                    Err(RunError::Tracker(_))
                ));
                // Nothing is rolled back: the label stays moved, the run is not finished.
                assert_eq!(rig.sim.labels(), vec!["factory:stuck"]);
                assert!(!rig.finished(&run));
            }
        }

        mod a_launch_that_an_earlier_pass_may_have_sent {
            use super::*;

            /// The runner's case "writes the launch-sending step once and sends under the same
            /// command id", for a start asked for again after it was sent: one step, and the
            /// same launch message both times.
            #[test]
            fn a_start_asked_for_again_sends_the_same_launch_message() {
                let rig = Rig::new();
                rig.worker.on_create(Ok(ws_id()));
                rig.worker
                    .on_start_session(Err(WorkerError::Unreachable("down".into())));
                rig.worker.on_start_session(Ok(session()));
                let run = claim_run(&rig);
                let mut rig = rig;
                pass(&rig, &run);
                show_workspace(&mut rig);
                let _ = advance(&rig.ctx(), &run);
                pass(&rig, &run);
                match &start_sessions(&rig)[..] {
                    [
                        Call::StartSession { first: one, .. },
                        Call::StartSession { first: two, .. },
                    ] => {
                        assert_eq!(one, two)
                    }
                    other => panic!("{other:?}"),
                }
            }
        }

        mod a_resume {
            use super::*;

            /// The runner's case "with no session launches and then looks at the text once".
            #[test]
            fn with_no_session_launches_and_then_looks_at_the_text_once() {
                let (rig, run) = workspace_shown();
                let graphql = |rig: &Rig| {
                    rig.sim
                        .calls()
                        .iter()
                        .filter(|call| call.iter().any(|a| a == "graphql"))
                        .count()
                };
                let before = graphql(&rig);
                assert_eq!(pass(&rig, &run), StartStep::Started { session: session() });
                let looked = graphql(&rig) - before;
                assert!(looked >= 1);
                pass(&rig, &run);
                assert_eq!(graphql(&rig) - before, looked);
                assert_eq!(
                    rig.steps(&run)
                        .iter()
                        .filter(|s| *s == "approval-rechecked")
                        .count(),
                    1
                );
            }

            /// The runner's case "with a session that was not looked at again does not launch
            /// again".
            #[test]
            fn with_a_session_that_was_not_looked_at_again_does_not_launch_again() {
                let (rig, run) = started();
                let asked = rig.sim.calls().len();
                rig.sim
                    .set_approval(approval_answer(LABELED_AT, "mallory", &[]));
                assert_eq!(pass(&rig, &run), StartStep::Started { session: session() });
                assert_eq!(start_sessions(&rig).len(), 1);
                assert!(
                    rig.sim.calls()[asked..]
                        .iter()
                        .all(|call| !call.iter().any(|a| a == "graphql"))
                );
            }

            /// The runner's case "with a second look that passed does not repeat it".
            #[test]
            fn with_a_second_look_that_passed_does_not_repeat_it() {
                let (rig, run) = started();
                let rechecked = |rig: &Rig, run: &RunId| {
                    rig.steps(run)
                        .iter()
                        .filter(|s| *s == "approval-rechecked")
                        .count()
                };
                assert_eq!(rechecked(&rig, &run), 1);
                pass(&rig, &run);
                pass(&rig, &run);
                assert_eq!(rechecked(&rig, &run), 1);
            }
        }

        mod stopping_a_session_that_goes_stuck {
            use super::*;

            /// The runner's case "a removal that cannot archive the threads on the worktree is
            /// an error with nothing written, and the next start removes it": the sessions of
            /// the workspace cannot be ended, nothing is written, and the next pass ends them.
            #[test]
            fn a_removal_that_cannot_archive_the_threads_on_the_worktree_is_an_error_with_nothing_written_and_the_next_start_removes_it()
             {
                let (rig, run) = started();
                rig.write(
                    &run,
                    Event::Stuck {
                        reason: "the earlier reason".into(),
                        branch: None,
                        commits_ahead: None,
                    },
                );
                rig.worker
                    .on_end_sessions(Err(WorkerError::Unreachable("down".into())));
                let writes = rig.sim.writes();
                assert!(advance(&rig.ctx(), &run).is_err());
                assert_eq!(rig.sim.writes(), writes);
                assert!(!rig.finished(&run));
                assert!(matches!(pass(&rig, &run), StartStep::Stuck { .. }));
                assert!(rig.finished(&run));
                assert_eq!(
                    calls(&rig)
                        .iter()
                        .filter(|call| matches!(call, Call::EndSessions { .. }))
                        .count(),
                    2
                );
            }

            /// The runner's case "a thread T3 no longer knows is stopped at the worktree the
            /// ledger records": a session the fleet does not show is ended at the workspace the
            /// ledger names.
            #[test]
            fn a_thread_t3_no_longer_knows_is_stopped_at_the_worktree_the_ledger_records() {
                let (mut rig, run) = started();
                rig.view = FleetView::builder().online(rig.machine).build();
                rig.write(
                    &run,
                    Event::Stuck {
                        reason: "the earlier reason".into(),
                        branch: None,
                        commits_ahead: None,
                    },
                );
                assert!(matches!(pass(&rig, &run), StartStep::Stuck { .. }));
                assert!(calls(&rig).contains(&Call::EndSessions { workspace: ws_id() }));
            }
        }

        mod a_closed_issue_of_a_run {
            use super::*;

            /// The runner's case "is the same for resume": a closed issue ends a run in triage
            /// and a run that is building, with its session ended, writing nothing on GitHub.
            #[test]
            fn is_the_same_for_resume() {
                let (rig, run) = claimed_rig();
                rig.sim.set_closed(true);
                let writes = rig.sim.writes();
                assert_eq!(pass(&rig, &run), StartStep::Abandoned);
                assert!(rig.finished(&run));
                assert_eq!(rig.sim.writes(), writes);
                let (rig, run) = started();
                rig.sim.set_closed(true);
                let writes = rig.sim.writes();
                assert_eq!(pass(&rig, &run), StartStep::Abandoned);
                assert!(calls(&rig).contains(&Call::EndSessions { workspace: ws_id() }));
                assert_eq!(rig.sim.writes(), writes);
            }

            /// The runner's case "leaves the actor out when GitHub names none or cannot be
            /// asked": the tracker has no `closedBy`, so a run ended for a closed issue names no
            /// actor.
            #[test]
            fn leaves_the_actor_out_when_github_names_none_or_cannot_be_asked() {
                let (rig, run) = claimed_rig();
                rig.sim.set_closed(true);
                pass(&rig, &run);
                let finished = rig.events(&run).pop().unwrap();
                assert_eq!(finished.event, "finished");
                assert!(finished.cause.unwrap().get("actor").is_none());
            }
        }

        /// What an earlier attempt left, written as the ledger of the attempt that ended: the
        /// runner's `seedStuckRun`, in Foundry's lines.
        mod the_resume_cases {
            use super::an_issue_set_ready_again::{
                first_create, rig_for_second_attempt, show_second, url,
            };
            use super::*;

            #[derive(Default)]
            struct Earlier<'a> {
                launched: Option<&'a str>,
                recorded: Option<&'a str>,
                resumed_from: Option<(RunId, &'a str)>,
                pr: Option<u64>,
                reason: &'a str,
                interrupted: Option<(bool, Option<&'a str>)>,
                questions: bool,
            }

            fn earlier(rig: &Rig, attempt: u32, e: &Earlier<'_>) {
                let run = run_id(attempt);
                let end = Cause::new(CauseStep::Watch, e.reason);
                rig.step(&run, &format!("approved {LABELED_AT}"));
                rig.moved(&run, State::Ready, State::Triage);
                rig.write(
                    &run,
                    Event::SessionLaunched {
                        thread_id: session().to_string(),
                        worktree: Some(ws_id().to_string()),
                        branch: e.launched.map(Into::into),
                        model_instance: None,
                        model: None,
                        model_options: None,
                        resumed_from: e.resumed_from.as_ref().map(|(run, branch)| ResumedFrom {
                            run: run.to_string(),
                            branch: (*branch).into(),
                        }),
                        base: None,
                        title: None,
                    },
                );
                rig.moved(&run, State::Triage, State::Building);
                if let Some(pr) = e.pr {
                    rig.write(&run, Event::PrOpened { pr, url: url(pr) });
                }
                if let Some((relaunch, branch)) = e.interrupted {
                    rig.write(
                        &run,
                        Event::SessionInterrupted {
                            resumes: 1,
                            relaunch,
                            run_ordinal: if relaunch { None } else { Some(1) },
                            branch: branch.map(Into::into),
                            t3_error: None,
                            ended_in_pass: 2,
                        },
                    );
                }
                if e.questions {
                    rig.write(
                        &run,
                        Event::QuestionsPosted {
                            questions: "1. Which?".into(),
                        },
                    );
                    rig.moved(&run, State::Building, State::NeedsInfo);
                    rig.ledger
                        .finish(&run, Outcome::NeedsInfo, None, None, &end, None)
                        .unwrap();
                } else {
                    rig.ledger
                        .write(
                            &run,
                            &Event::Stuck {
                                reason: "the run ended".into(),
                                branch: e.recorded.map(Into::into),
                                commits_ahead: Some(2),
                            },
                            &end,
                        )
                        .unwrap();
                    rig.moved(&run, State::Building, State::Stuck);
                    rig.ledger
                        .finish(&run, Outcome::Stuck, None, None, &end, None)
                        .unwrap();
                }
            }

            fn open_pull(rig: &Rig, pr: u64, head: &str) {
                rig.sim.set_pull(
                    pr,
                    SimPull {
                        state: "OPEN",
                        draft: true,
                        head: head.into(),
                        base: "main".into(),
                        cross: false,
                    },
                );
            }

            /// The issue is set `ready` again and claimed: the run that follows the earlier ones.
            fn next(rig: &Rig) -> RunId {
                rig.sim.set_labels(&["factory:ready"]);
                rig.sim.set_assignees(&[]);
                claim_run(rig)
            }

            fn on_branch(name: &str) -> Checkout {
                Checkout::Branch { name: name.into() }
            }

            fn plain(rig: &Rig) {
                assert!(
                    matches!(first_create(rig).checkout, Checkout::NewBranch { .. }),
                    "{:?}",
                    first_create(rig).checkout
                );
            }

            fn lookups(rig: &Rig) -> Vec<String> {
                rig.sim
                    .calls()
                    .iter()
                    .map(|call| call.join(" "))
                    .filter(|call| call.contains("git/ref/heads/"))
                    .map(|call| call.rsplit("heads/").next().unwrap().to_owned())
                    .collect()
            }

            fn time_limited<'a>(
                launched: Option<&'a str>,
                recorded: Option<&'a str>,
            ) -> Earlier<'a> {
                Earlier {
                    launched,
                    recorded,
                    reason: TIME_LIMIT,
                    ..Earlier::default()
                }
            }

            /// The runner's case "starts from base after a run whose interruption was resumed in
            /// its own thread": only a relaunch or a time limit leaves work to continue from.
            #[test]
            fn starts_from_base_after_a_run_whose_interruption_was_resumed_in_its_own_thread() {
                let rig = rig_for_second_attempt();
                earlier(
                    &rig,
                    1,
                    &Earlier {
                        launched: Some("fac/8-1"),
                        resumed_from: Some((run_id(1), "fac/old")),
                        interrupted: Some((false, Some("fac/8-1"))),
                        reason: "session-ended-badly",
                        ..Earlier::default()
                    },
                );
                rig.sim.add_branch("fac/8-1");
                rig.sim.add_branch("fac/old");
                let run = next(&rig);
                pass(&rig, &run);
                plain(&rig);
                assert!(lookups(&rig).is_empty());
            }

            /// The runner's case "of a resumed run falls back to the branch its session was told
            /// to push to": the attempt's own recorded branch is tried first, then the one it
            /// resumed from.
            #[test]
            fn of_a_resumed_run_falls_back_to_the_branch_its_session_was_told_to_push_to() {
                let rig = rig_for_second_attempt();
                earlier(&rig, 1, &time_limited(Some("fac/8-1"), Some("fac/8-1")));
                earlier(
                    &rig,
                    2,
                    &Earlier {
                        resumed_from: Some((run_id(1), "fac/8-1")),
                        ..time_limited(None, Some("fac/own"))
                    },
                );
                rig.sim.add_branch("fac/8-1");
                let run = next(&rig);
                assert_eq!(run, run_id(3));
                pass(&rig, &run);
                assert_eq!(first_create(&rig).checkout, on_branch("fac/8-1"));
                assert_eq!(lookups(&rig), vec!["fac/own", "fac/8-1"]);
            }

            /// The runner's case "of a resumed run continues from T3's own branch when origin has
            /// it": in Foundry, the branch the attempt recorded on its stuck line.
            #[test]
            fn of_a_resumed_run_continues_from_t3_s_own_branch_when_origin_has_it() {
                let rig = rig_for_second_attempt();
                earlier(&rig, 1, &time_limited(Some("fac/8-1"), Some("fac/8-1")));
                earlier(
                    &rig,
                    2,
                    &Earlier {
                        resumed_from: Some((run_id(1), "fac/8-1")),
                        ..time_limited(None, Some("fac/own"))
                    },
                );
                rig.sim.add_branch("fac/own");
                let run = next(&rig);
                pass(&rig, &run);
                assert_eq!(first_create(&rig).checkout, on_branch("fac/own"));
                assert_eq!(lookups(&rig), vec!["fac/own"]);
            }

            /// The runner's case "continues from the branch a relaunch after an interruption
            /// left".
            #[test]
            fn continues_from_the_branch_a_relaunch_after_an_interruption_left() {
                let rig = rig_for_second_attempt();
                earlier(
                    &rig,
                    1,
                    &Earlier {
                        interrupted: Some((true, Some("fac/8-1"))),
                        reason: "session-interrupted",
                        ..Earlier::default()
                    },
                );
                rig.sim.add_branch("fac/8-1");
                let run = next(&rig);
                pass(&rig, &run);
                assert_eq!(first_create(&rig).checkout, on_branch("fac/8-1"));
                let mut rig = rig;
                show_second(&mut rig, "fac/8-2");
                pass(&rig, &run);
                match &start_sessions(&rig)[..] {
                    [Call::StartSession { first, .. }] => assert_eq!(
                        first,
                        &messages::resumed(
                            REPO,
                            ISSUE,
                            LOGIN,
                            &ResumeFrom::Interrupted {
                                run: run_id(1),
                                branch: "fac/8-1".into()
                            },
                            None,
                            BODY_PATH
                        )
                    ),
                    other => panic!("{other:?}"),
                }
                let launched = rig
                    .events(&run)
                    .into_iter()
                    .find(|e| e.event == "session_launched")
                    .unwrap();
                assert_eq!(launched.cause.unwrap()["reason"], "resumed-from-branch");
            }

            /// The runner's case "is not made after an interruption's relaunch that then asked
            /// questions".
            #[test]
            fn is_not_made_after_an_interruption_s_relaunch_that_then_asked_questions() {
                let rig = rig_for_second_attempt();
                earlier(
                    &rig,
                    1,
                    &Earlier {
                        interrupted: Some((true, Some("fac/8-1"))),
                        questions: true,
                        reason: "session-interrupted",
                        ..Earlier::default()
                    },
                );
                rig.sim.add_branch("fac/8-1");
                let run = next(&rig);
                pass(&rig, &run);
                plain(&rig);
                assert!(lookups(&rig).is_empty());
            }

            /// The runner's case "reads the branch from the stuck line when the launch line has
            /// none".
            #[test]
            fn reads_the_branch_from_the_stuck_line_when_the_launch_line_has_none() {
                let rig = rig_for_second_attempt();
                earlier(&rig, 1, &time_limited(None, Some("fac/8-1")));
                rig.sim.add_branch("fac/8-1");
                let run = next(&rig);
                pass(&rig, &run);
                assert_eq!(first_create(&rig).checkout, on_branch("fac/8-1"));
            }

            /// The runner's case "prefers the branch on the launch line".
            #[test]
            fn prefers_the_branch_on_the_launch_line() {
                let rig = rig_for_second_attempt();
                earlier(
                    &rig,
                    1,
                    &time_limited(Some("fac/launched"), Some("fac/8-1")),
                );
                rig.sim.add_branch("fac/launched");
                rig.sim.add_branch("fac/8-1");
                let run = next(&rig);
                pass(&rig, &run);
                assert_eq!(first_create(&rig).checkout, on_branch("fac/launched"));
            }

            /// The runner's case "writes no launch-sending step when GitHub fails while
            /// deciding".
            #[test]
            fn writes_no_launch_sending_step_when_github_fails_while_deciding() {
                let rig = rig_for_second_attempt();
                earlier(&rig, 1, &time_limited(Some("fac/8-1"), Some("fac/8-1")));
                rig.sim.add_branch("fac/8-1");
                let run = next(&rig);
                rig.sim.fail("git/ref/heads", 1);
                assert!(advance(&rig.ctx(), &run).is_err());
                assert!(
                    !rig.steps(&run).iter().any(
                        |s| s.starts_with("launch-sending") || s.starts_with("launch-resuming")
                    )
                );
            }

            /// The runner's case "is an error that leaves the issue in triage when the issue text
            /// cannot be read".
            #[test]
            fn is_an_error_that_leaves_the_issue_in_triage_when_the_issue_text_cannot_be_read() {
                let rig = rig_for_second_attempt();
                earlier(&rig, 1, &time_limited(Some("fac/8-1"), Some("fac/8-1")));
                rig.sim.add_branch("fac/8-1");
                let run = next(&rig);
                rig.sim.fail("graphql", 1);
                assert!(advance(&rig.ctx(), &run).is_err());
                assert_eq!(rig.sim.labels(), vec!["factory:triage"]);
                assert!(creates(&rig).is_empty());
                assert_eq!(pass(&rig, &run), StartStep::Waiting(Wait::Workspace));
                assert_eq!(first_create(&rig).checkout, on_branch("fac/8-1"));
            }

            /// The runner's case "is not made when the earlier run stopped for another reason".
            #[test]
            fn is_not_made_when_the_earlier_run_stopped_for_another_reason() {
                let rig = rig_for_second_attempt();
                earlier(
                    &rig,
                    1,
                    &Earlier {
                        launched: Some("fac/8-1"),
                        recorded: Some("fac/8-1"),
                        reason: "session-said-stuck",
                        ..Earlier::default()
                    },
                );
                rig.sim.add_branch("fac/8-1");
                let run = next(&rig);
                pass(&rig, &run);
                plain(&rig);
                assert!(lookups(&rig).is_empty());
            }

            /// The runner's case "is not made when the earlier run recorded no branch".
            #[test]
            fn is_not_made_when_the_earlier_run_recorded_no_branch() {
                let rig = rig_for_second_attempt();
                earlier(&rig, 1, &time_limited(None, None));
                let run = next(&rig);
                pass(&rig, &run);
                plain(&rig);
                assert!(lookups(&rig).is_empty());
            }

            /// The runner's case "records the decision in the launch-sending step, before the
            /// launch is sent".
            #[test]
            fn records_the_decision_in_the_launch_sending_step_before_the_launch_is_sent() {
                let mut rig = rig_for_second_attempt();
                earlier(&rig, 1, &time_limited(Some("fac/8-1"), Some("fac/8-1")));
                rig.sim.add_branch("fac/8-1");
                let run = next(&rig);
                pass(&rig, &run);
                show_second(&mut rig, "fac/8-2");
                pass(&rig, &run);
                let steps = rig.steps(&run);
                let at = |step: &str| steps.iter().position(|s| s == step).unwrap();
                assert!(at("launch-resuming fac/8-1") < at("launch-sending resumed-from fac/8-1"));
            }

            /// A rig whose first start of a session is not answered.
            fn rig_whose_first_start_is_lost() -> Rig {
                let rig = Rig::new();
                rig.worker.on_create(Ok(ws_id()));
                rig.worker
                    .on_start_session(Err(WorkerError::Unreachable("down".into())));
                rig.worker.on_start_session(Ok(session()));
                rig
            }

            /// The runner's case "sent again, reuses the recorded decision and asks GitHub
            /// nothing".
            #[test]
            fn sent_again_reuses_the_recorded_decision_and_asks_github_nothing() {
                let mut rig = rig_whose_first_start_is_lost();
                earlier(&rig, 1, &time_limited(Some("fac/8-1"), Some("fac/8-1")));
                rig.sim.add_branch("fac/8-1");
                let run = next(&rig);
                pass(&rig, &run);
                show_second(&mut rig, "fac/8-2");
                assert!(advance(&rig.ctx(), &run).is_err());
                let asked = rig.sim.calls().len();
                assert_eq!(pass(&rig, &run), StartStep::Started { session: session() });
                assert!(
                    rig.sim.calls()[asked..]
                        .iter()
                        .map(|call| call.join(" "))
                        .all(|call| !call.contains("git/ref/heads")
                            && !call.contains("closedByPullRequestsReferences")
                            && !call.starts_with("pr view"))
                );
                let resumed = messages::resumed(
                    REPO,
                    ISSUE,
                    LOGIN,
                    &ResumeFrom::TimeLimit {
                        run: run_id(1),
                        branch: "fac/8-1".into(),
                    },
                    None,
                    BODY_PATH,
                );
                match &start_sessions(&rig)[..] {
                    [
                        Call::StartSession { first: one, .. },
                        Call::StartSession { first: two, .. },
                    ] => {
                        assert_eq!(one, &resumed);
                        assert_eq!(two, &resumed);
                    }
                    other => panic!("{other:?}"),
                }
            }

            /// The runner's case "sent again after a plain start, stays plain though a resume is
            /// now possible".
            #[test]
            fn sent_again_after_a_plain_start_stays_plain_though_a_resume_is_now_possible() {
                let mut rig = rig_whose_first_start_is_lost();
                earlier(&rig, 1, &time_limited(Some("fac/8-1"), Some("fac/8-1")));
                let run = next(&rig);
                pass(&rig, &run);
                show_second(&mut rig, "fac/8-2");
                assert!(advance(&rig.ctx(), &run).is_err());
                // The branch reached origin since the decision.
                rig.sim.add_branch("fac/8-1");
                assert_eq!(pass(&rig, &run), StartStep::Started { session: session() });
                plain(&rig);
                assert!(
                    !rig.steps(&run)
                        .iter()
                        .any(|s| s.starts_with("launch-resuming"))
                );
                match &start_sessions(&rig)[..] {
                    [
                        Call::StartSession { first: one, .. },
                        Call::StartSession { first: two, .. },
                    ] => {
                        assert_eq!(one, &launch_message());
                        assert_eq!(two, &launch_message());
                    }
                    other => panic!("{other:?}"),
                }
            }

            /// The runner's case "after a session that said stuck, continues its pull request".
            #[test]
            fn after_a_session_that_said_stuck_continues_its_pull_request() {
                let rig = rig_for_second_attempt();
                earlier(
                    &rig,
                    1,
                    &Earlier {
                        launched: Some("fac/8-1"),
                        pr: Some(12),
                        reason: "session-said-stuck",
                        ..Earlier::default()
                    },
                );
                open_pull(&rig, 12, "fac/8-1");
                let run = next(&rig);
                pass(&rig, &run);
                assert_eq!(first_create(&rig).checkout, on_branch("fac/8-1"));
                assert!(
                    rig.steps(&run)
                        .contains(&format!("launch-pull-request fac/8-1 {}", url(12)))
                );
            }

            /// The runner's case "is an error that leaves the issue in triage when GitHub cannot
            /// read the ledger's pull request".
            #[test]
            fn is_an_error_that_leaves_the_issue_in_triage_when_github_cannot_read_the_ledger_s_pull_request()
             {
                let rig = rig_for_second_attempt();
                earlier(
                    &rig,
                    1,
                    &Earlier {
                        launched: Some("fac/8-1"),
                        pr: Some(12),
                        reason: "session-said-stuck",
                        ..Earlier::default()
                    },
                );
                open_pull(&rig, 12, "fac/8-1");
                let run = next(&rig);
                rig.sim.fail("pr view", 1);
                assert!(advance(&rig.ctx(), &run).is_err());
                assert_eq!(rig.sim.labels(), vec!["factory:triage"]);
                assert!(creates(&rig).is_empty());
                assert!(
                    !rig.steps(&run)
                        .iter()
                        .any(|s| s.starts_with("workspace-requested"))
                );
                assert_eq!(pass(&rig, &run), StartStep::Waiting(Wait::Workspace));
            }

            /// The runner's case "after a time limit, the pull request wins over the branch".
            #[test]
            fn after_a_time_limit_the_pull_request_wins_over_the_branch() {
                let rig = rig_for_second_attempt();
                earlier(
                    &rig,
                    1,
                    &Earlier {
                        pr: Some(12),
                        ..time_limited(Some("fac/8-1"), Some("fac/8-1"))
                    },
                );
                open_pull(&rig, 12, "fac/8-pr");
                rig.sim.add_branch("fac/8-1");
                let run = next(&rig);
                pass(&rig, &run);
                assert_eq!(first_create(&rig).checkout, on_branch("fac/8-pr"));
                assert!(
                    rig.steps(&run)
                        .contains(&format!("launch-pull-request fac/8-pr {}", url(12)))
                );
            }

            /// The runner's case "is made even when the issue text changed since the earlier
            /// approval": a pull request is continued whatever the text.
            #[test]
            fn is_made_even_when_the_issue_text_changed_since_the_earlier_approval() {
                let rig = rig_for_second_attempt();
                earlier(
                    &rig,
                    1,
                    &Earlier {
                        launched: Some("fac/8-1"),
                        pr: Some(12),
                        reason: "session-said-stuck",
                        ..Earlier::default()
                    },
                );
                open_pull(&rig, 12, "fac/8-1");
                rig.sim.set_approval(approval_answer(
                    "2026-10-04T12:00:00Z",
                    "tom",
                    &[("2026-10-04T11:00:00Z", "tom")],
                ));
                let run = next(&rig);
                pass(&rig, &run);
                assert_eq!(first_create(&rig).checkout, on_branch("fac/8-1"));
            }

            /// The runner's case "with an empty ledger, continues the open pull request GitHub
            /// says closes the issue".
            #[test]
            fn with_an_empty_ledger_continues_the_open_pull_request_github_says_closes_the_issue() {
                let rig = rig_for_second_attempt();
                rig.sim.closing_pull(12, "fac/8-1", "main", LOGIN, false);
                let run = claim_run(&rig);
                assert_eq!(run, run_id(1));
                pass(&rig, &run);
                assert_eq!(first_create(&rig).checkout, on_branch("fac/8-1"));
            }

            fn plain_attempt(rig: &Rig, attempt: u32) {
                earlier(
                    rig,
                    attempt,
                    &Earlier {
                        reason: "session-said-stuck",
                        ..Earlier::default()
                    },
                );
            }

            fn with_pull(rig: &Rig, attempt: u32, pr: u64) {
                earlier(
                    rig,
                    attempt,
                    &Earlier {
                        pr: Some(pr),
                        reason: "session-said-stuck",
                        ..Earlier::default()
                    },
                );
            }

            /// The runner's case "continues the ledger's pull request of an attempt before the
            /// last one".
            #[test]
            fn continues_the_ledger_s_pull_request_of_an_attempt_before_the_last_one() {
                let rig = rig_for_second_attempt();
                with_pull(&rig, 1, 12);
                plain_attempt(&rig, 2);
                open_pull(&rig, 12, "fac/8-1");
                let run = next(&rig);
                assert_eq!(run, run_id(3));
                pass(&rig, &run);
                assert_eq!(first_create(&rig).checkout, on_branch("fac/8-1"));
            }

            /// The runner's case "continues the newest attempt's pull request when two attempts
            /// recorded one each".
            #[test]
            fn continues_the_newest_attempt_s_pull_request_when_two_attempts_recorded_one_each() {
                let rig = rig_for_second_attempt();
                with_pull(&rig, 1, 12);
                with_pull(&rig, 2, 13);
                open_pull(&rig, 12, "fac/8-1");
                open_pull(&rig, 13, "fac/8-2");
                let run = next(&rig);
                pass(&rig, &run);
                assert_eq!(first_create(&rig).checkout, on_branch("fac/8-2"));
            }

            /// The runner's case "passes over the ledger's pull request when it is not on this
            /// base, from a fork, or on a branch a command cannot be handed".
            #[test]
            fn passes_over_the_ledger_s_pull_request_when_it_is_not_on_this_base_from_a_fork_or_on_a_branch_a_command_cannot_be_handed()
             {
                let rig = rig_for_second_attempt();
                with_pull(&rig, 1, 12);
                with_pull(&rig, 2, 13);
                with_pull(&rig, 3, 14);
                rig.sim.set_pull(
                    12,
                    SimPull {
                        state: "OPEN",
                        draft: true,
                        head: "fac/8-fork".into(),
                        base: "main".into(),
                        cross: true,
                    },
                );
                rig.sim.set_pull(
                    13,
                    SimPull {
                        state: "OPEN",
                        draft: true,
                        head: "fac/8-other".into(),
                        base: "release".into(),
                        cross: false,
                    },
                );
                open_pull(&rig, 14, "-x");
                let run = next(&rig);
                assert_eq!(run, run_id(4));
                pass(&rig, &run);
                plain(&rig);
            }

            /// The runner's case "finds an open pull request by a host's login for a resume":
            /// in Foundry there are no hosts, and the project's login is the only author whose
            /// pull request is the run's own.
            #[test]
            fn finds_an_open_pull_request_by_a_host_s_login_for_a_resume() {
                let rig = rig_for_second_attempt();
                rig.sim.closing_pull(12, "fac/8-host", "main", LOGIN, false);
                rig.sim
                    .closing_pull(13, "fac/8-other", "main", "a-host-login", false);
                let run = claim_run(&rig);
                pass(&rig, &run);
                assert_eq!(first_create(&rig).checkout, on_branch("fac/8-host"));
            }

            /// The runner's case "falls back to the branch after a time limit when the pull
            /// request is closed".
            #[test]
            fn falls_back_to_the_branch_after_a_time_limit_when_the_pull_request_is_closed() {
                let rig = rig_for_second_attempt();
                earlier(
                    &rig,
                    1,
                    &Earlier {
                        pr: Some(12),
                        ..time_limited(Some("fac/8-1"), Some("fac/8-1"))
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
                rig.sim.add_branch("fac/8-1");
                let run = next(&rig);
                pass(&rig, &run);
                assert_eq!(first_create(&rig).checkout, on_branch("fac/8-1"));
                assert!(
                    !rig.steps(&run)
                        .iter()
                        .any(|s| s.starts_with("launch-pull-request"))
                );
            }

            /// The runner's case "is an error that leaves the issue in triage when GitHub cannot
            /// list the pull requests".
            #[test]
            fn is_an_error_that_leaves_the_issue_in_triage_when_github_cannot_list_the_pull_requests()
             {
                let rig = rig_for_second_attempt();
                let run = claim_run(&rig);
                rig.sim.fail("closedByPullRequestsReferences", 1);
                assert!(advance(&rig.ctx(), &run).is_err());
                assert_eq!(rig.sim.labels(), vec!["factory:triage"]);
                assert!(creates(&rig).is_empty());
                assert_eq!(pass(&rig, &run), StartStep::Waiting(Wait::Workspace));
            }
        }

        mod the_base_cases {
            use super::an_issue_that_names_its_base::claimed_with_body;
            use super::*;

            /// The runner's case "a base the tracker refuses as a name is not a branch name, not
            /// an error".
            #[test]
            fn a_base_the_tracker_refuses_as_a_name_is_not_a_branch_name_not_an_error() {
                let (rig, run) = claimed_with_body("## Base\nrelease/2\n");
                rig.sim.refuse("git/ref/heads");
                assert_eq!(
                    pass(&rig, &run),
                    StartStep::Stuck {
                        reason: messages::base_not_a_branch("release/2")
                    }
                );
                assert!(calls(&rig).is_empty());
            }

            /// The runner's case "a launch sent again keeps the base the first send decided and
            /// asks GitHub nothing".
            #[test]
            fn a_launch_sent_again_keeps_the_base_the_first_send_decided_and_asks_github_nothing() {
                let rig = Rig::new();
                rig.worker.on_create(Ok(ws_id()));
                rig.worker
                    .on_start_session(Err(WorkerError::Unreachable("down".into())));
                rig.worker.on_start_session(Ok(session()));
                rig.sim.set_body("Do it.\n\n## Base\nrelease/2\n");
                rig.sim.add_branch("release/2");
                let run = claim_run(&rig);
                let mut rig = rig;
                pass(&rig, &run);
                show_workspace(&mut rig);
                assert!(advance(&rig.ctx(), &run).is_err());
                rig.sim.set_body("Do it.");
                let asked = rig.sim.calls().len();
                assert_eq!(pass(&rig, &run), StartStep::Started { session: session() });
                assert!(
                    rig.sim.calls()[asked..]
                        .iter()
                        .all(|call| !call.join(" ").contains("git/ref/heads"))
                );
                let wanted = messages::launch(REPO, ISSUE, LOGIN, Some("release/2"), BODY_PATH);
                match &start_sessions(&rig)[..] {
                    [
                        Call::StartSession { first: one, .. },
                        Call::StartSession { first: two, .. },
                    ] => {
                        assert_eq!(one, &wanted);
                        assert_eq!(two, &wanted);
                    }
                    other => panic!("{other:?}"),
                }
            }

            /// A launch asked for again by count keeps the base the first send decided.
            #[test]
            fn a_launch_asked_for_again_keeps_the_named_base() {
                let (mut rig, run) = claimed_with_body("Do it.\n\n## Base\nrelease/2\n");
                rig.sim.add_branch("release/2");
                pass(&rig, &run);
                show_workspace(&mut rig);
                pass(&rig, &run);
                super::the_launch_asked_for_again::show_session(&mut rig, SessionState::Idle, None);
                rig.worker.on_send(Ok(0));
                assert_eq!(pass(&rig, &run), StartStep::Resent { session: session() });
                match &super::the_launch_asked_for_again::sends(&rig)[..] {
                    [Call::Send { message, .. }] => assert_eq!(
                        message,
                        &messages::launch(REPO, ISSUE, LOGIN, Some("release/2"), BODY_PATH)
                    ),
                    other => panic!("{other:?}"),
                }
            }

            /// A launch asked for again by count keeps the resume the first send decided.
            #[test]
            fn a_launch_asked_for_again_keeps_the_resume() {
                let (mut rig, run) = {
                    let rig = super::an_issue_set_ready_again::rig_for_second_attempt();
                    super::an_issue_set_ready_again::time_limited_first_attempt(&rig, "fac/8-1");
                    rig.sim.add_branch("fac/8-1");
                    let run = claim_run(&rig);
                    (rig, run)
                };
                pass(&rig, &run);
                super::an_issue_set_ready_again::show_second(&mut rig, "fac/8-2");
                pass(&rig, &run);
                super::the_launch_asked_for_again::show_session(&mut rig, SessionState::Idle, None);
                // The fleet shows the workspace under its name for the idle session.
                rig.worker.on_send(Ok(0));
                assert_eq!(pass(&rig, &run), StartStep::Resent { session: session() });
                match &super::the_launch_asked_for_again::sends(&rig)[..] {
                    [Call::Send { message, .. }] => assert_eq!(
                        message,
                        &messages::resumed(
                            REPO,
                            ISSUE,
                            LOGIN,
                            &ResumeFrom::TimeLimit {
                                run: run_id(1),
                                branch: "fac/8-1".into()
                            },
                            None,
                            BODY_PATH
                        )
                    ),
                    other => panic!("{other:?}"),
                }
            }
        }

        /// The runner's "claim comments of several machines". Foundry has one coordinator that
        /// holds the project's lease and claims, so no two machines race for an issue, there is
        /// no claim winner and no give-way. Each case keeps the runner's name and asserts what
        /// stands in its place.
        mod claim_comments_of_several_machines {
            use super::*;

            /// The runner's case, where another machine's win makes no run: in Foundry a
            /// coordinator that lost the lease makes no run, no launch and no status comment.
            #[test]
            fn a_claim_another_machine_won_makes_no_run_no_launch_and_no_status_comment() {
                let mut rig = Rig::new();
                rig.guard = Box::new(Lost);
                assert!(matches!(
                    claim(&rig.ctx(), ISSUE, rig.machine),
                    Err(RunError::LeaseLost(_))
                ));
                assert!(rig.sim.writes().is_empty());
                assert!(rig.ledger.issue_runs(REPO, ISSUE).unwrap().is_empty());
                assert!(calls(&rig).is_empty());
            }

            /// The runner's case, where a triage issue is not resumed when another machine won
            /// it: in Foundry the issue's newest claim names the run, and a run that is not the
            /// newest claim's is left alone.
            #[test]
            fn a_triage_issue_of_the_shared_login_is_not_resumed_when_another_machine_won_it() {
                let rig = Rig::new();
                rig.sim.set_labels(&["factory:triage"]);
                rig.sim.set_assignees(&[LOGIN]);
                rig.foundry_claim(&run_id(2), elsewhere());
                let writes = rig.sim.writes();
                assert!(matches!(pass(&rig, &run_id(1)), StartStep::Skipped { .. }));
                assert!(rig.ledger.read(&run_id(1)).unwrap().is_empty());
                assert_eq!(rig.sim.writes(), writes);
            }

            /// The runner's case, where a triage issue whose winning claim names this machine is
            /// resumed: the claim comment that names the run is taken over, and the build is
            /// asked of the machine it names.
            #[test]
            fn a_triage_issue_whose_winning_claim_names_this_machine_is_resumed() {
                let rig = Rig::new();
                rig.worker.on_create(Ok(ws_id()));
                rig.sim.set_labels(&["factory:triage"]);
                rig.sim.set_assignees(&[LOGIN]);
                rig.foundry_claim(&run_id(1), rig.machine);
                assert_eq!(pass(&rig, &run_id(1)), StartStep::Waiting(Wait::Workspace));
                match &creates(&rig)[..] {
                    [Call::Create { machine, .. }] => assert_eq!(*machine, rig.machine),
                    other => panic!("{other:?}"),
                }
            }

            /// The runner's case, where a run of this machine's that another machine's later
            /// claim beat is finished: in Foundry a later claim of the issue finishes the earlier
            /// run.
            #[test]
            fn a_triage_run_of_this_machine_s_that_another_machine_s_later_claim_beat_is_finished()
            {
                let (rig, first) = claimed_rig();
                rig.sim.set_labels(&["factory:ready"]);
                rig.sim.set_assignees(&[]);
                let second = claim_run(&rig);
                assert_eq!(second, run_id(2));
                assert!(rig.finished(&first));
                assert!(!rig.finished(&second));
            }

            /// The runner's case, where a failed claim that finds another machine's claim landed
            /// takes nothing: a claim that fails with someone else assigned changes nothing.
            #[test]
            fn a_failed_claim_that_finds_another_machine_s_claim_landed_takes_nothing() {
                let rig = Rig::new();
                rig.sim.set_labels(&["factory:triage"]);
                rig.sim.set_assignees(&["another-factory"]);
                let error = RunError::Invalid("the claim call failed".into());
                assert!(repair_claim(&rig.ctx(), ISSUE, &run_id(1), error).is_err());
                assert!(rig.sim.writes().is_empty());
                assert_eq!(rig.sim.assignees(), vec!["another-factory"]);
                assert_eq!(rig.sim.labels(), vec!["factory:triage"]);
            }

            fn unfinished(rig: &Rig, run: &RunId, state: State) {
                rig.step(run, &format!("approved {LABELED_AT}"));
                rig.moved(run, State::Ready, State::Triage);
                if state != State::Triage {
                    rig.moved(run, State::Triage, state);
                }
            }

            /// The runner's case "yieldToClaim finishes the runs claimed before another machine's
            /// winning claim, writing nothing to GitHub": a new claim finishes the earlier runs
            /// in the ledger, and the only writes on GitHub are its own.
            #[test]
            fn yieldtoclaim_finishes_the_runs_claimed_before_another_machine_s_winning_claim_writing_nothing_to_github()
             {
                let rig = Rig::new();
                unfinished(&rig, &run_id(1), State::Building);
                unfinished(&rig, &run_id(2), State::Triage);
                let run = claim_run(&rig);
                assert_eq!(run, run_id(3));
                assert!(rig.finished(&run_id(1)) && rig.finished(&run_id(2)));
                assert_eq!(rig.sim.writes().len(), 4, "the claim's own writes only");
                assert!(calls(&rig).is_empty());
            }

            /// The runner's case "yieldToClaim leaves a run claimed after the winning comment,
            /// and its own win": the claim leaves the run it makes unfinished, and an earlier run
            /// that is finished already is not touched.
            #[test]
            fn yieldtoclaim_leaves_a_run_claimed_after_the_winning_comment_and_its_own_win() {
                let rig = Rig::new();
                unfinished(&rig, &run_id(1), State::Triage);
                rig.ledger
                    .finish(
                        &run_id(1),
                        Outcome::Stuck,
                        None,
                        None,
                        &Cause::new(CauseStep::Start, "test"),
                        None,
                    )
                    .unwrap();
                let before = rig.events(&run_id(1));
                let run = claim_run(&rig);
                assert!(!rig.finished(&run));
                assert_eq!(rig.events(&run_id(1)), before);
            }

            /// The runner's case "yieldToClaim finishes a run with no claim time as claimed
            /// before the winner": a run with no `claimed` line is finished by the next claim.
            #[test]
            fn yieldtoclaim_finishes_a_run_with_no_claim_time_as_claimed_before_the_winner() {
                let rig = Rig::new();
                rig.step(&run_id(1), &format!("approved {LABELED_AT}"));
                assert!(!read_progress(&rig.events(&run_id(1))).claimed);
                claim_run(&rig);
                assert!(rig.finished(&run_id(1)));
            }

            /// The runner's case "a triage issue whose claim comments cannot be read is not
            /// resumed".
            #[test]
            fn a_triage_issue_whose_claim_comments_cannot_be_read_is_not_resumed() {
                let rig = Rig::new();
                rig.sim.set_labels(&["factory:triage"]);
                rig.sim.set_assignees(&[LOGIN]);
                rig.foundry_claim(&run_id(1), rig.machine);
                rig.sim.fail("/comments", 1);
                let writes = rig.sim.writes();
                assert!(advance(&rig.ctx(), &run_id(1)).is_err());
                assert!(rig.ledger.read(&run_id(1)).unwrap().is_empty());
                assert_eq!(rig.sim.writes(), writes);
                assert!(calls(&rig).is_empty());
            }

            /// The runner's case "a failed claim whose claim comments cannot be read fails with
            /// the claim's error": a claim that fails fails with the failing call's own error,
            /// whatever else cannot be read.
            #[test]
            fn a_failed_claim_whose_claim_comments_cannot_be_read_fails_with_the_claim_s_error() {
                let rig = Rig::new();
                rig.sim.fail("--add-label", usize::MAX);
                match claim(&rig.ctx(), ISSUE, rig.machine) {
                    Err(RunError::Tracker(error)) => {
                        assert!(error.to_string().contains("--add-label"), "{error}")
                    }
                    other => panic!("{other:?}"),
                }
            }

            /// The runner's case "a repo pinned with factory.owner resumes its triage issue
            /// without reading claims": a run that has events is resumed without reading the
            /// issue's claim comments.
            #[test]
            fn a_repo_pinned_with_factory_owner_resumes_its_triage_issue_without_reading_claims() {
                let (rig, run) = claimed_rig();
                let asked = rig.sim.calls().len();
                assert_eq!(pass(&rig, &run), StartStep::Waiting(Wait::Workspace));
                assert!(
                    rig.sim.calls()[asked..]
                        .iter()
                        .all(|call| !call.join(" ").contains("/comments"))
                );
            }
        }

        /// A start cut short after its first write: the next pass finishes what the ledger
        /// shows begun, whatever the issue's label and assignee now are.
        mod a_start_cut_short {
            use super::*;

            fn end_sessions(rig: &Rig) -> Vec<Call> {
                calls(rig)
                    .into_iter()
                    .filter(|call| matches!(call, Call::EndSessions { .. }))
                    .collect()
            }

            fn archives(rig: &Rig) -> Vec<Call> {
                calls(rig)
                    .into_iter()
                    .filter(|call| matches!(call, Call::Archive { .. }))
                    .collect()
            }

            fn said(rig: &Rig, words: &str) -> usize {
                rig.sim
                    .comments()
                    .iter()
                    .filter(|comment| comment.contains(words))
                    .count()
            }

            /// The stuck path of a failed setup, cut short by an unassign that failed after the
            /// label moved to `stuck`: the next pass finishes it and does not comment again.
            #[test]
            fn a_stuck_path_cut_short_after_the_label_moved_is_finished_by_the_next_pass() {
                let (mut rig, run) = claimed_rig();
                pass(&rig, &run);
                show_setup(&mut rig, TerminalState::Exited, Some(1));
                rig.worker.on_tail(Ok("boom".into()));
                rig.sim.fail("--remove-assignee", 1);
                assert!(advance(&rig.ctx(), &run).is_err());
                assert_eq!(rig.sim.labels(), vec!["factory:stuck"]);
                assert_eq!(rig.sim.assignees(), vec![LOGIN]);
                assert!(!rig.finished(&run));
                assert!(matches!(pass(&rig, &run), StartStep::Stuck { .. }));
                assert!(rig.finished(&run));
                assert!(rig.sim.assignees().is_empty());
                assert_eq!(said(&rig, "could not finish this issue"), 1);
                assert_eq!(rig.sim.labels(), vec!["factory:stuck"]);
            }

            /// The resumed stuck path of a failed setup posts the comment with the tail again.
            #[test]
            fn a_resumed_stuck_path_posts_the_setup_tail() {
                let (mut rig, run) = claimed_rig();
                pass(&rig, &run);
                show_setup(&mut rig, TerminalState::Exited, Some(2));
                rig.worker.on_tail(Ok("setup tail words".into()));
                rig.sim.fail("issue comment", 1);
                assert!(advance(&rig.ctx(), &run).is_err());
                assert_eq!(said(&rig, "could not finish this issue"), 0);
                rig.worker.on_tail(Ok("setup tail words".into()));
                assert!(matches!(pass(&rig, &run), StartStep::Stuck { .. }));
                assert!(stuck_comment(&rig).contains("setup tail words"));
            }

            /// The same, cut short by the status comment after the unassign.
            #[test]
            fn a_stuck_path_cut_short_after_the_unassign_is_finished_by_the_next_pass() {
                let (mut rig, run) = claimed_rig();
                pass(&rig, &run);
                show_setup(&mut rig, TerminalState::Exited, Some(1));
                rig.worker.on_tail(Ok("boom".into()));
                // The status comment is the last write of the path.
                rig.sim.fail("Factory status", 1);
                assert!(advance(&rig.ctx(), &run).is_err());
                assert!(rig.sim.assignees().is_empty());
                assert!(!rig.finished(&run));
                assert!(matches!(pass(&rig, &run), StartStep::Stuck { .. }));
                assert!(rig.finished(&run));
            }

            /// A withdraw the ledger shows begun, cut short by an archive that failed after the label moved to
            /// `proposed`: the issue is no longer in triage, and the next pass archives and
            /// finishes.
            #[test]
            fn a_withdraw_cut_short_after_the_label_moved_is_finished_by_the_next_pass() {
                let (rig, run) = workspace_shown();
                rig.step(&run, "approval-withdrawn");
                rig.sim
                    .set_approval(approval_answer(LABELED_AT, "mallory", &[]));
                rig.worker
                    .on_archive(Err(WorkerError::Unreachable("down".into())));
                assert!(advance(&rig.ctx(), &run).is_err());
                assert_eq!(rig.sim.labels(), vec!["factory:proposed"]);
                assert!(!rig.finished(&run));
                assert!(matches!(pass(&rig, &run), StartStep::Unapproved { .. }));
                assert!(rig.finished(&run));
                assert_eq!(archives(&rig).len(), 2);
                assert_eq!(rig.sim.labels(), vec!["factory:proposed"]);
                assert_eq!(said(&rig, "did not start on this issue"), 1);
            }

            /// A withdraw the ledger shows begun, cut short after the unassign by a label call that failed: the
            /// issue is in triage with nobody assigned, and the next pass finishes it.
            #[test]
            fn a_withdraw_cut_short_after_the_unassign_is_finished_by_the_next_pass() {
                let (rig, run) = workspace_shown();
                rig.step(&run, "approval-withdrawn");
                rig.sim
                    .set_approval(approval_answer(LABELED_AT, "mallory", &[]));
                rig.sim.fail("--add-label factory:proposed", 1);
                assert!(advance(&rig.ctx(), &run).is_err());
                assert!(rig.sim.assignees().is_empty());
                assert_eq!(rig.sim.labels(), vec!["factory:triage"]);
                assert!(matches!(pass(&rig, &run), StartStep::Unapproved { .. }));
                assert_eq!(rig.sim.labels(), vec!["factory:proposed"]);
                assert!(rig.finished(&run));
                assert_eq!(archives(&rig).len(), 1);
                assert_eq!(said(&rig, "did not start on this issue"), 1);
            }

            /// A withdraw the ledger shows begun, cut short in triage, whose approval reads as given again on the next
            /// pass: it is finished with no second explanation.
            #[test]
            fn a_withdraw_cut_short_and_approved_again_is_finished_with_no_second_comment() {
                let (rig, run) = workspace_shown();
                rig.step(&run, "approval-withdrawn");
                rig.sim
                    .set_approval(approval_answer(LABELED_AT, "mallory", &[]));
                rig.sim.fail("--add-label factory:proposed", 1);
                assert!(advance(&rig.ctx(), &run).is_err());
                assert_eq!(rig.sim.labels(), vec!["factory:triage"]);
                assert_eq!(said(&rig, "did not start on this issue"), 1);
                rig.sim
                    .set_approval(approval_answer(LABELED_AT, "tom", &[]));
                assert!(matches!(pass(&rig, &run), StartStep::Unapproved { .. }));
                assert_eq!(said(&rig, "did not start on this issue"), 1);
                assert_eq!(rig.sim.labels(), vec!["factory:proposed"]);
                assert!(rig.sim.assignees().is_empty());
                assert_eq!(archives(&rig).len(), 1);
                assert!(rig.finished(&run));
            }

            /// A claim cut short after the label and before the assignee: the next pass assigns
            /// the login, writes the assignee step first, and asks for the workspace.
            #[test]
            fn a_claim_cut_short_after_the_label_is_carried_on_by_the_next_pass() {
                let mut rig = Rig::new();
                rig.worker.on_create(Ok(ws_id()));
                rig.guard = Box::new(HeldFor::new(3));
                assert!(matches!(
                    claim(&rig.ctx(), ISSUE, rig.machine),
                    Err(RunError::LeaseLost(_))
                ));
                assert_eq!(rig.sim.labels(), vec!["factory:triage"]);
                assert!(rig.sim.assignees().is_empty());
                rig.guard = Box::new(crate::factory::worker::AlwaysHeld(1));
                let run = run_id(1);
                assert_eq!(pass(&rig, &run), StartStep::Waiting(Wait::Workspace));
                assert_eq!(rig.sim.assignees(), vec![LOGIN]);
                assert_eq!(creates(&rig).len(), 1);
                assert!(rig.steps(&run).contains(&"claim-assigning".to_string()));
            }

            /// A whole claim (its triage line is written) whose assignee a person then removed
            /// is skipped, as the runner skips it: the person's unassignment stands.
            #[test]
            fn a_whole_claim_later_unassigned_by_a_person_is_skipped() {
                let (rig, run) = claimed_rig();
                rig.sim.set_assignees(&[]);
                let writes = rig.sim.writes().len();
                assert!(matches!(pass(&rig, &run), StartStep::Skipped { .. }));
                assert_eq!(rig.sim.writes().len(), writes);
                assert!(rig.sim.assignees().is_empty());
                assert!(!rig.steps(&run).contains(&"claim-assigning".to_string()));
            }

            /// A 64 KB setup tail, the stuck comment landing with its answer lost: the next pass
            /// reads the decision back, posts no second comment and starts no session.
            #[test]
            fn a_stuck_decision_with_a_64_kb_tail_is_read_back_and_posts_no_second_comment() {
                let (mut rig, run) = claimed_rig();
                pass(&rig, &run);
                show_setup(&mut rig, TerminalState::Exited, Some(1));
                rig.worker.on_tail(Ok("y".repeat(70_000)));
                rig.sim.fail("--add-label", 1);
                assert!(advance(&rig.ctx(), &run).is_err());
                let stuck_comments = |rig: &Rig| {
                    rig.sim
                        .comments()
                        .iter()
                        .filter(|c| c.contains("could not finish this issue"))
                        .count()
                };
                assert_eq!(stuck_comments(&rig), 1);
                assert!(
                    rig.steps(&run)
                        .iter()
                        .all(|step| step.len() < 4_000 && !step.contains("yyyyyyyy")),
                    "no step holds the tail"
                );
                rig.worker.on_tail(Err(WorkerError::Failed("gone".into())));
                assert!(matches!(pass(&rig, &run), StartStep::Stuck { .. }));
                assert_eq!(stuck_comments(&rig), 1);
                assert!(start_sessions(&rig).is_empty());
                assert_eq!(rig.sim.labels(), vec!["factory:stuck"]);
            }

            /// A stuck path the ledger shows begun on an issue an instructor has put back to
            /// `ready`: nothing is written on GitHub, and the claim step takes the issue.
            #[test]
            fn a_begun_stuck_path_leaves_an_issue_put_back_to_ready_to_the_claim_step() {
                let (rig, run) = claimed_rig();
                rig.write(
                    &run,
                    Event::Stuck {
                        reason: "the earlier reason".into(),
                        branch: None,
                        commits_ahead: None,
                    },
                );
                rig.sim.set_labels(&["factory:ready"]);
                rig.sim.set_assignees(&[]);
                let writes = rig.sim.writes().len();
                assert!(matches!(pass(&rig, &run), StartStep::Skipped { .. }));
                assert_eq!(rig.sim.writes().len(), writes);
                assert_eq!(rig.sim.labels(), vec!["factory:ready"]);
                assert!(calls(&rig).is_empty());
            }

            /// A withdraw the ledger shows begun, with the issue approved again and `ready`: the
            /// label stays `ready` and nothing is written.
            #[test]
            fn a_begun_withdraw_leaves_an_issue_put_back_to_ready_to_the_claim_step() {
                let (rig, run) = workspace_shown();
                rig.step(&run, "approval-withdrawn");
                rig.sim.set_labels(&["factory:ready"]);
                rig.sim.set_assignees(&[]);
                let writes = rig.sim.writes().len();
                let asked = calls(&rig).len();
                assert!(matches!(pass(&rig, &run), StartStep::Skipped { .. }));
                assert_eq!(rig.sim.writes().len(), writes);
                assert_eq!(rig.sim.labels(), vec!["factory:ready"]);
                assert_eq!(calls(&rig).len(), asked);
            }

            /// A begun stuck path on an issue a person closed is the closed issue's: the run is
            /// abandoned and nothing is written on GitHub.
            #[test]
            fn a_begun_stuck_path_on_a_closed_issue_is_abandoned_with_no_writes() {
                let (rig, run) = claimed_rig();
                rig.write(
                    &run,
                    Event::Stuck {
                        reason: "the earlier reason".into(),
                        branch: None,
                        commits_ahead: None,
                    },
                );
                rig.sim.set_closed(true);
                let writes = rig.sim.writes().len();
                assert_eq!(pass(&rig, &run), StartStep::Abandoned);
                assert_eq!(rig.sim.writes().len(), writes);
                assert!(rig.finished(&run));
            }

            /// A setup terminal whose tail the machine answers it cannot give is the run's
            /// end, with the reason alone: asking again would never end.
            #[test]
            fn a_setup_whose_tail_the_machine_cannot_give_goes_stuck_with_the_reason_alone() {
                let (mut rig, run) = claimed_rig();
                pass(&rig, &run);
                show_setup(&mut rig, TerminalState::Exited, None);
                rig.worker
                    .on_tail(Err(WorkerError::Failed("the holder is gone".into())));
                assert_eq!(
                    pass(&rig, &run),
                    StartStep::Stuck {
                        reason: SETUP_FAILED.into()
                    }
                );
                assert!(rig.finished(&run));
                assert_eq!(rig.sim.labels(), vec!["factory:stuck"]);
                assert!(!stuck_comment(&rig).contains("the holder is gone"));
            }

            /// A refused first start whose stuck comment landed and whose answer was lost: the
            /// next pass goes stuck from the ledger, starts nothing and posts nothing again,
            /// though the worker would now accept.
            #[test]
            fn a_stuck_comment_that_landed_without_its_line_goes_stuck_with_no_second_start() {
                let rig = Rig::new();
                rig.worker.on_create(Ok(ws_id()));
                rig.worker
                    .on_start_session(Err(WorkerError::Failed("refused".into())));
                rig.worker.on_start_session(Ok(session()));
                let run = match claim(&rig.ctx(), ISSUE, rig.machine).unwrap() {
                    ClaimOutcome::Claimed { run } => run,
                    other => panic!("{other:?}"),
                };
                let mut rig = rig;
                pass(&rig, &run);
                show_workspace(&mut rig);
                rig.sim.lose("issue comment", 1);
                assert!(advance(&rig.ctx(), &run).is_err());
                assert_eq!(said(&rig, "could not finish this issue"), 1);
                assert!(!rig.finished(&run));
                assert_eq!(start_sessions(&rig).len(), 1);
                assert!(matches!(pass(&rig, &run), StartStep::Stuck { .. }));
                assert_eq!(start_sessions(&rig).len(), 1);
                assert_eq!(said(&rig, "could not finish this issue"), 1);
                assert!(rig.finished(&run));
                assert_eq!(rig.sim.labels(), vec!["factory:stuck"]);
            }

            /// The `approval-withdrawn` step is read.
            #[test]
            fn the_approval_withdrawn_step_is_read() {
                let (rig, run) = claimed_rig();
                assert!(!read_progress(&rig.events(&run)).withdrawn);
                rig.step(&run, "approval-withdrawn");
                assert!(read_progress(&rig.events(&run)).withdrawn);
            }

            /// A start whose answer was lost: the next pass ends the workspace's sessions, once,
            /// and only then starts again.
            #[test]
            fn a_start_whose_answer_was_lost_ends_the_sessions_before_it_starts_again() {
                let rig = Rig::new();
                rig.worker.on_create(Ok(ws_id()));
                rig.worker
                    .on_start_session(Err(WorkerError::Unreachable("did not answer".into())));
                rig.worker.on_start_session(Ok(session()));
                let run = match claim(&rig.ctx(), ISSUE, rig.machine).unwrap() {
                    ClaimOutcome::Claimed { run } => run,
                    other => panic!("{other:?}"),
                };
                let mut rig = rig;
                pass(&rig, &run);
                show_workspace(&mut rig);
                assert!(advance(&rig.ctx(), &run).is_err());
                assert!(end_sessions(&rig).is_empty());
                assert_eq!(pass(&rig, &run), StartStep::Started { session: session() });
                let kinds: Vec<&str> = calls(&rig)
                    .iter()
                    .map(|call| match call {
                        Call::Create { .. } => "create",
                        Call::StartSession { .. } => "start",
                        Call::EndSessions { .. } => "end",
                        _ => "other",
                    })
                    .collect();
                assert_eq!(kinds, vec!["create", "start", "end", "start"]);
                assert!(
                    rig.steps(&run)
                        .contains(&"launch-ending-sessions".to_string())
                );
            }

            /// A start whose answer was lost, and whose factory session the fleet already
            /// shows: adopted, nothing ended and nothing started.
            #[test]
            fn a_start_whose_answer_was_lost_adopts_the_factory_session_the_fleet_shows() {
                let (mut rig, run) = workspace_shown();
                rig.step(&run, "launch-sending");
                rig.view = FleetView::builder()
                    .online(rig.machine)
                    .workspace(workspace_record(rig.machine, "fac/8-1", None))
                    .session(session_record(rig.machine, SessionState::Working))
                    .turn(foundry_proto::factory::SessionTurn {
                        session: session(),
                        workspace: ws_id(),
                        project: project_id(),
                        turn: 1,
                        prompts: 1,
                        question: false,
                    })
                    .build();
                assert_eq!(pass(&rig, &run), StartStep::Started { session: session() });
                assert!(start_sessions(&rig).is_empty());
                assert!(end_sessions(&rig).is_empty());
            }

            /// A person's session in the build workspace is no factory session: it is not
            /// adopted, the launch is started.
            #[test]
            fn a_session_with_no_turn_record_is_not_adopted_as_the_runs_own() {
                let (mut rig, run) = workspace_shown();
                rig.step(&run, "launch-sending");
                rig.view = FleetView::builder()
                    .online(rig.machine)
                    .workspace(workspace_record(rig.machine, "fac/8-1", None))
                    .session(session_record(rig.machine, SessionState::Working))
                    .build();
                assert_eq!(pass(&rig, &run), StartStep::Started { session: session() });
                assert_eq!(start_sessions(&rig).len(), 1);
                assert_eq!(end_sessions(&rig).len(), 1);
            }

            /// A start retried a pass later checks the text again, and a stranger's edit since
            /// the approval ends the run stuck with the session stopped.
            #[test]
            fn a_start_retried_a_pass_later_checks_the_text_again() {
                let rig = Rig::new();
                rig.worker.on_create(Ok(ws_id()));
                rig.worker
                    .on_start_session(Err(WorkerError::Unreachable("down".into())));
                rig.worker.on_start_session(Ok(session()));
                let run = match claim(&rig.ctx(), ISSUE, rig.machine).unwrap() {
                    ClaimOutcome::Claimed { run } => run,
                    other => panic!("{other:?}"),
                };
                let mut rig = rig;
                pass(&rig, &run);
                show_workspace(&mut rig);
                assert!(advance(&rig.ctx(), &run).is_err());
                rig.sim.set_approval(approval_answer(
                    LABELED_AT,
                    "tom",
                    &[("2026-10-04T10:30:00Z", "mallory")],
                ));
                assert_eq!(
                    pass(&rig, &run),
                    StartStep::Stuck {
                        reason: TEXT_CHANGED.into()
                    }
                );
                assert_eq!(start_sessions(&rig).len(), 1);
                assert_eq!(end_sessions(&rig).len(), 1);
                assert!(rig.finished(&run));
            }

            /// A setup terminal that exited with no exit code is a failed setup.
            #[test]
            fn a_setup_that_exited_with_no_exit_code_makes_the_run_stuck_with_the_tail() {
                let (mut rig, run) = claimed_rig();
                pass(&rig, &run);
                show_setup(&mut rig, TerminalState::Exited, None);
                rig.worker.on_tail(Ok("the holder is gone".into()));
                assert_eq!(
                    pass(&rig, &run),
                    StartStep::Stuck {
                        reason: SETUP_FAILED.into()
                    }
                );
                assert!(stuck_comment(&rig).contains("the holder is gone"));
                assert!(start_sessions(&rig).is_empty());
                assert!(rig.finished(&run));
            }

            /// A workspace record found with no setup terminal, for a run that has no answer of
            /// a create: the create is asked again (it answers with the first and starts a
            /// setup that is due), and the start waits for the record it shows.
            #[test]
            fn a_found_workspace_with_no_setup_terminal_is_asked_for_again_before_it_is_taken_as_done()
             {
                let (mut rig, run) = claimed_rig();
                show_workspace(&mut rig);
                assert_eq!(pass(&rig, &run), StartStep::Waiting(Wait::Workspace));
                assert_eq!(creates(&rig).len(), 1);
                assert!(start_sessions(&rig).is_empty());
                assert_eq!(pass(&rig, &run), StartStep::Started { session: session() });
                assert_eq!(creates(&rig).len(), 1);
            }

            /// A run with a session and no recorded approval: the session is ended and the
            /// run goes stuck.
            #[test]
            fn with_a_session_and_no_recorded_approval_archives_it_and_goes_stuck() {
                let rig = Rig::new();
                let run = run_id(1);
                rig.sim.set_labels(&["factory:building"]);
                rig.sim.set_assignees(&[LOGIN]);
                rig.write(
                    &run,
                    Event::Claimed {
                        machine: rig.machine.to_string(),
                        github_user: LOGIN.into(),
                    },
                );
                rig.write(
                    &run,
                    Event::SessionLaunched {
                        thread_id: session().to_string(),
                        worktree: Some(ws_id().to_string()),
                        branch: None,
                        model_instance: None,
                        model: None,
                        model_options: None,
                        resumed_from: None,
                        base: None,
                        title: None,
                    },
                );
                rig.moved(&run, State::Ready, State::Building);
                assert_eq!(
                    pass(&rig, &run),
                    StartStep::Stuck {
                        reason: CANNOT_CONFIRM.into()
                    }
                );
                assert_eq!(
                    end_sessions(&rig),
                    vec![Call::EndSessions { workspace: ws_id() }]
                );
                assert_eq!(rig.sim.labels(), vec!["factory:stuck"]);
                assert!(rig.finished(&run));
            }
        }

        mod a_run_that_is_not_this_steps {
            use super::*;

            /// The runner's case "ends the unfinished run of a closed issue and writes nothing
            /// on GitHub", for `resume`.
            #[test]
            fn ends_the_unfinished_run_of_a_closed_issue_and_writes_nothing_on_github() {
                let (rig, run) = started();
                let before = rig.sim.writes().len();
                rig.sim.set_closed(true);
                assert_eq!(pass(&rig, &run), StartStep::Abandoned);
                assert!(rig.finished(&run));
                assert_eq!(rig.sim.writes().len(), before);
                assert!(calls(&rig).contains(&Call::EndSessions { workspace: ws_id() }));
            }

            #[test]
            fn a_closed_issue_whose_own_pull_request_was_merged_ends_the_run_as_its_own_merge() {
                let (rig, run) = started();
                rig.write(
                    &run,
                    Event::PrOpened {
                        pr: 12,
                        url: format!("https://github.com/{REPO}/pull/12"),
                    },
                );
                rig.sim.set_pull(
                    12,
                    SimPull {
                        state: "MERGED",
                        draft: false,
                        head: "fac/8-1".into(),
                        base: "main".into(),
                        cross: false,
                    },
                );
                rig.sim.set_closed(true);
                assert_eq!(pass(&rig, &run), StartStep::Ended);
                let finished = rig.events(&run).pop().unwrap();
                assert_eq!(finished.data["pr_end"], "merged");
            }

            #[test]
            fn a_finished_run_is_skipped() {
                let (rig, run) = started();
                rig.ledger
                    .finish(
                        &run,
                        Outcome::Stuck,
                        None,
                        None,
                        &Cause::new(CauseStep::Start, "test"),
                        None,
                    )
                    .unwrap();
                assert!(matches!(pass(&rig, &run), StartStep::Skipped { .. }));
            }

            /// The runner's case "never claims an issue put back to ready: that is the claim
            /// step's", for a run that has events.
            #[test]
            fn an_issue_put_back_to_ready_is_left_to_the_claim_step() {
                let (rig, run) = claimed_rig();
                rig.sim.set_labels(&["factory:ready"]);
                rig.sim.set_assignees(&[]);
                assert!(matches!(pass(&rig, &run), StartStep::Skipped { .. }));
                assert!(calls(&rig).is_empty());
            }

            /// The runner's case "with an unfinished run in another state is left to the step
            /// that owns it".
            #[test]
            fn with_an_unfinished_run_in_another_state_is_left_to_the_step_that_owns_it() {
                let (rig, run) = started();
                rig.moved(&run, State::Building, State::PrOpen);
                let before = rig.names(&run);
                assert!(matches!(pass(&rig, &run), StartStep::Skipped { .. }));
                assert_eq!(rig.names(&run), before);
            }

            /// The runner's case "finishes a stuck path cut short after its label moved": a
            /// `stuck` line is in the ledger, the label is already `factory:stuck`, the factory
            /// is still assigned and the run is unfinished.
            #[test]
            fn finishes_a_stuck_path_cut_short_after_its_label_moved() {
                let (rig, run) = claimed_rig();
                rig.sim.set_labels(&["factory:stuck"]);
                assert_eq!(rig.sim.assignees(), vec![LOGIN]);
                rig.write(
                    &run,
                    Event::Stuck {
                        reason: "the earlier reason".into(),
                        branch: None,
                        commits_ahead: None,
                    },
                );
                assert_eq!(
                    pass(&rig, &run),
                    StartStep::Stuck {
                        reason: "the earlier reason".into()
                    }
                );
                assert_eq!(rig.sim.labels(), vec!["factory:stuck"]);
                assert!(rig.sim.assignees().is_empty());
                assert!(rig.finished(&run));
                assert!(creates(&rig).is_empty());
            }

            /// The runner's case "a decision to give up that is already in the ledger stands" is
            /// the one above. This one: the comment is not posted twice.
            #[test]
            fn a_stuck_path_cut_short_does_not_post_the_comment_again() {
                let (rig, run) = claimed_rig();
                rig.write(
                    &run,
                    Event::Stuck {
                        reason: "the earlier reason".into(),
                        branch: None,
                        commits_ahead: None,
                    },
                );
                pass(&rig, &run);
                assert!(
                    rig.sim
                        .comments()
                        .iter()
                        .all(|comment| !comment.contains("could not finish"))
                );
            }

            /// The runner's case "a worktree that cannot be removed is written down and the run
            /// still goes stuck": a workspace whose sessions cannot be ended leaves the stuck
            /// path to be retried, with nothing written.
            #[test]
            fn a_worktree_that_cannot_be_removed_is_written_down_and_the_run_still_goes_stuck() {
                let (rig, run) = started();
                rig.worker
                    .on_end_sessions(Err(WorkerError::Unreachable("down".into())));
                rig.write(
                    &run,
                    Event::Stuck {
                        reason: "the earlier reason".into(),
                        branch: None,
                        commits_ahead: None,
                    },
                );
                rig.moved(&run, State::Building, State::Triage);
                assert!(advance(&rig.ctx(), &run).is_err());
                assert!(!rig.finished(&run));
            }
        }

        mod the_pull_requests_of_the_issue {
            use super::*;

            /// The runner's case, where the author is one of the factory's logins (the runner's
            /// or a host's), matched without case.
            #[test]
            fn an_author_is_the_runs_own_by_the_projects_login_whatever_its_case() {
                assert!(is_own_pull(Some("SoftFac-Bot"), LOGIN));
                assert!(is_own_pull(Some(LOGIN), LOGIN));
                assert!(!is_own_pull(Some("someone"), LOGIN));
                assert!(!is_own_pull(Some("app/superset-app"), LOGIN));
                assert!(!is_own_pull(None, LOGIN));
            }

            /// The runner's `isResumablePull`: in this repo, onto the base, on a branch a command
            /// can be handed.
            #[test]
            fn a_pull_request_is_resumable_in_this_repo_onto_the_base_on_a_safe_branch() {
                assert!(is_resumable_pull(false, "main", "fac/8-1", "main"));
                assert!(!is_resumable_pull(true, "main", "fac/8-1", "main"));
                assert!(!is_resumable_pull(false, "release", "fac/8-1", "main"));
                assert!(!is_resumable_pull(false, "main", "-x", "main"));
                assert!(!is_resumable_pull(false, "main", "fac/8 1", "main"));
                assert!(!is_resumable_pull(false, "main", "a..b", "main"));
            }
        }

        /// The query the issue's closing pull requests are read with, scripted by its arguments
        /// as the fake tracker is.
        mod the_query_for_the_pull_requests_closing_the_issue {
            use super::*;
            use crate::factory::github::{Answer, FakeTracker};

            #[test]
            fn is_one_graphql_call_for_the_issue_and_reads_a_bot_as_app_slash_its_login() {
                let fake = FakeTracker::new();
                let args =
                    crate::factory::github::read::issue_query_args(REPO, CLOSING_PRS_QUERY, ISSUE)
                        .unwrap();
                fake.on(
                    args.clone(),
                    Answer::ok(
                        serde_json::json!({"data": {"repository": {"issue": {
                            "closedByPullRequestsReferences": {"nodes": [
                                {"number": 12, "url": "u12", "state": "OPEN", "headRefName": "a",
                                 "baseRefName": "main", "isCrossRepository": false,
                                 "author": {"__typename": "Bot", "login": "superset-app"}},
                                {"number": 13, "url": "u13", "state": "CLOSED", "headRefName": "b",
                                 "baseRefName": "main", "isCrossRepository": false,
                                 "author": {"__typename": "User", "login": "rawphp"}},
                                {"number": 14, "url": "u14", "state": "OPEN", "headRefName": "c",
                                 "baseRefName": "main", "isCrossRepository": true, "author": null},
                            ]}
                        }}}})
                        .to_string(),
                    ),
                );
                let pulls = open_pull_requests_closing(&fake, REPO, ISSUE).unwrap();
                assert_eq!(fake.calls(), vec![args]);
                assert_eq!(pulls.len(), 2);
                assert_eq!(pulls[0].number, 12);
                assert_eq!(pulls[0].author.as_deref(), Some("app/superset-app"));
                assert_eq!(pulls[1].number, 14);
                assert_eq!(pulls[1].author, None);
                assert!(pulls[1].cross_repository);
            }
        }
    }
}
