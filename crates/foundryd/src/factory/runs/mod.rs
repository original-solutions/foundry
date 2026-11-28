//! A run: one issue built in a workspace, from claim to its end.
//!
//! This file ports the runner's `src/runner/run.ts`: what every step that stops or finishes a
//! run shares. [`RunContext`] is what a step calls: the tracker, the worker, what the fleet
//! shows, the ledger and the guard. The stuck path ([`go_stuck`]) with its session stop
//! ([`stop_session`]), the move to `factory:building` ([`finish_building`]), the actions a
//! lifecycle transition asks for ([`apply_actions`]) and the three ways a run ends without a
//! gate ([`end_run`], [`abandon`], [`end_own_merge`]) live here, so the building, gating,
//! resume and feedback steps of tasks 23 to 28 call them and add nothing to this file.
//!
//! Where it differs from the runner:
//!
//! - A session is stopped through the worker: [`stop_session`] ends the factory sessions of the
//!   run's build workspace and keeps the workspace, as the spec says of a stuck run. The
//!   runner's archive of a thread and removal of its worktree have no counterpart: no step of
//!   Foundry writes `worktree_removed`, though `read_progress` still reads those lines of a
//!   ledger the runner wrote.
//! - [`Guard::check`] runs before every change on GitHub and every call to the worker, in the
//!   methods of [`RunContext`] that make them. A lost lease is an error with nothing done.
//! - A step that sends a message to the session sends a [`Message`] (a line and a body file),
//!   which its caller builds, in place of the runner's text under a command id. (The relaunch
//!   command ids of `progress` are the runner's, kept for the ledgers it wrote.)
//! - The tracker has no `closedBy`, so [`abandon`] names no actor in its cause.
//! - The status comment names the machine as its id, since a [`RunContext`] holds no machine
//!   names.

pub mod building;
pub mod feedback;
pub mod gating;
pub mod progress;
pub mod resume;
pub mod start;
pub mod stop;

use foundry_proto::factory::{FactorySettings, ProjectFactory, RunId};
use foundry_proto::ids::{ProjectId, SessionId, WorkspaceId};

use super::config::ProjectConfig;
use super::github::read::gh_pages;
use super::github::{Commenting, MARKER, PullRequest, TrackerError};
use super::ledger::events::{Cause, CauseStep, Event, Outcome, PrEnd};
use super::ledger::{Ledger, LedgerError};
use super::lifecycle::{Action, Event as LifecycleEvent, InvalidTransition, State, transition};
use super::messages::{self, Message, WorkLeft};
use super::text;
use super::worker::{FleetView, Guard, LeaseLost, Worker, WorkerError};
use crate::clock::Clock;
use crate::sessions::factory::SendRefused;
use crate::store::StoreError;
use progress::{
    GATE_RESULTS_COMMENTING, GATE_RESULTS_POSTED, QUESTIONS_COMMENTING, SEND_INTENT, SEND_REFUSED,
    SEND_TYPED, STUCK_COMMENTING, STUCK_DETAIL, STUCK_STOP, Seen, SendTries,
};

/// What a step of a run can fail with.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error(transparent)]
    Tracker(#[from] TrackerError),
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Transition(#[from] InvalidTransition),
    #[error(transparent)]
    Worker(#[from] WorkerError),
    #[error(transparent)]
    Send(#[from] SendRefused),
    /// The lease is lost: nothing was done.
    #[error(transparent)]
    LeaseLost(#[from] LeaseLost),
    /// A send to a session waits for the send `key` to it, which may have typed and is not
    /// known typed. Nothing was asked of the worker; the pending key is to be sent again first.
    #[error("a send to the session waits for its send {key}, which may have been typed")]
    SendPending { key: String },
    /// A send to `session` waits for the fleet to show its turn, whose prompt count guards the
    /// send. Nothing was written or asked of the worker.
    #[error("the fleet shows no turn for session {session}: no prompt count to send under")]
    NoPromptCount { session: SessionId },
    /// An older run of the issue has its gate running, so nothing was called or written.
    #[error("the gate of run {run} is running")]
    GateRunning { run: RunId },
    /// A record that cannot be what it must be: a session id that is not one, say.
    #[error("{0}")]
    Invalid(String),
}

/// The project a run belongs to: its id in P1, its repo and the login the factory acts as there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunProject {
    pub id: ProjectId,
    /// `owner/name`.
    pub repo: String,
    /// The GitHub login the factory acts as on this project: the assignee of a claimed issue,
    /// the author of its claim comment and the account a session starts as.
    pub login: String,
}

/// The factory settings a run reads: the fleet's (the model and effort a session starts with,
/// whether claims are paused) and the project's (its own pause).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunSettings {
    pub fleet: FactorySettings,
    pub project: ProjectFactory,
}

/// What a step of a run calls. Everything is borrowed for one pass.
pub struct RunContext<'a> {
    /// GitHub, with a hider: the only tracker that can post a comment.
    pub tracker: &'a dyn Commenting,
    /// What the fleet is asked to do.
    pub worker: &'a dyn Worker,
    /// What the fleet shows this pass.
    pub view: &'a FleetView,
    pub ledger: &'a Ledger,
    /// Whether the lease is still held: checked before every change and every call to a worker.
    pub guard: &'a dyn Guard,
    pub project: &'a RunProject,
    /// The project's `factory` config: its instructors, base and limits.
    pub config: &'a ProjectConfig,
    pub settings: &'a RunSettings,
    /// The clock that counts through sleep, for every wait the factory measures.
    pub clock: &'a dyn Clock,
}

impl RunContext<'_> {
    /// Passes while the lease is held.
    pub fn held(&self) -> Result<u64, RunError> {
        Ok(self.guard.check()?)
    }

    /// The repo, `owner/name`.
    pub fn repo(&self) -> &str {
        &self.project.repo
    }

    /// The label of `issue` moved to `state`.
    pub fn set_state(&self, issue: u64, state: State) -> Result<(), RunError> {
        self.held()?;
        Ok(self.tracker.set_state(self.repo(), issue, state)?)
    }

    /// The project's login assigned to `issue`.
    pub fn assign(&self, issue: u64) -> Result<(), RunError> {
        self.held()?;
        Ok(self
            .tracker
            .assign(self.repo(), issue, &self.project.login)?)
    }

    /// The project's login taken off `issue`.
    pub fn unassign(&self, issue: u64) -> Result<(), RunError> {
        self.held()?;
        Ok(self
            .tracker
            .unassign(self.repo(), issue, &self.project.login)?)
    }

    /// A comment on `issue`.
    pub fn post_comment(&self, issue: u64, body: &str) -> Result<(), RunError> {
        self.held()?;
        Ok(self.tracker.post_comment(self.repo(), issue, body)?)
    }

    /// The issue's status comment, written or edited.
    pub fn upsert_status(&self, issue: u64, body: &str) -> Result<(), RunError> {
        self.held()?;
        Ok(self
            .tracker
            .upsert_status_comment(self.repo(), issue, &self.project.login, body)?)
    }

    /// A comment on pull request `pr`.
    pub fn comment_on_pr(&self, pr: u64, body: &str) -> Result<(), RunError> {
        self.held()?;
        Ok(self.tracker.comment_on_pr(self.repo(), pr, body)?)
    }

    /// Pull request `pr` taken out of draft.
    pub fn mark_pr_ready(&self, pr: u64) -> Result<(), RunError> {
        self.held()?;
        Ok(self.tracker.mark_pr_ready(self.repo(), pr)?)
    }

    /// Pull request `pr` put back to draft.
    pub fn mark_pr_draft(&self, pr: u64) -> Result<(), RunError> {
        self.held()?;
        Ok(self.tracker.mark_pr_draft(self.repo(), pr)?)
    }

    /// A workspace made at `target`.
    pub fn create(
        &self,
        target: super::worker::Target,
        request: super::worker::CreateWorkspace,
    ) -> Result<WorkspaceId, RunError> {
        self.held()?;
        Ok(self.worker.create(target, request)?)
    }

    /// A factory session started in `workspace` with `first` as its first message.
    pub fn start_session(
        &self,
        workspace: WorkspaceId,
        start: crate::sessions::factory::FactoryStart,
        first: Message,
    ) -> Result<SessionId, RunError> {
        self.held()?;
        Ok(self.worker.start_session(workspace, start, first)?)
    }

    /// The output of `terminal`, from its end.
    pub fn tail(&self, terminal: foundry_proto::ids::TerminalId) -> Result<String, RunError> {
        self.held()?;
        Ok(self.worker.tail(terminal)?)
    }

    /// `message` sent to `session`, when its count of prompts is not above `only_if_prompts`.
    pub fn send(
        &self,
        session: SessionId,
        message: Message,
        only_if_prompts: Option<u64>,
    ) -> Result<u64, RunError> {
        self.held()?;
        Ok(self.worker.send(session, message, only_if_prompts)?)
    }

    /// Every factory session of `workspace` ended.
    pub fn end_sessions(&self, workspace: WorkspaceId) -> Result<(), RunError> {
        self.held()?;
        Ok(self.worker.end_sessions(workspace)?)
    }

    /// `workspace` archived, forced.
    pub fn archive(&self, workspace: WorkspaceId) -> Result<(), RunError> {
        self.held()?;
        Ok(self.worker.archive(workspace)?)
    }

    /// One more step of the setup of `workspace` taken.
    pub fn advance(&self, workspace: WorkspaceId) -> Result<(), RunError> {
        self.held()?;
        Ok(self.worker.advance(workspace)?)
    }

    /// `workspace` put at rest.
    pub fn rest(&self, workspace: WorkspaceId) -> Result<(), RunError> {
        self.held()?;
        Ok(self.worker.rest(workspace)?)
    }

    /// `workspace` woken from rest.
    pub fn wake(&self, workspace: WorkspaceId) -> Result<(), RunError> {
        self.held()?;
        Ok(self.worker.wake(workspace)?)
    }

    /// `workspace` kept, with its files, and what it runs ended.
    pub fn keep(&self, workspace: WorkspaceId) -> Result<(), RunError> {
        self.held()?;
        Ok(self.worker.keep(workspace)?)
    }

    /// `event` written to `run`'s ledger.
    pub fn record(&self, run: &RunId, event: Event, cause: &Cause) -> Result<(), RunError> {
        self.ledger.write(run, &event, cause)?;
        Ok(())
    }

    /// A `step_changed` line of `run`.
    pub fn record_step(&self, run: &RunId, step: &str, cause: &Cause) -> Result<(), RunError> {
        self.record(run, Event::StepChanged { step: step.into() }, cause)
    }

    /// The events of `run`, read as a [`Seen`].
    pub fn seen(&self, run: &RunId) -> Result<Seen, RunError> {
        Ok(progress::read_progress(&self.ledger.read(run)?))
    }
}

/// Where the issue stands while the stuck actions are applied. `thread_id` and `worktree` start
/// from the ledger and move as this call launches and stops the session.
#[derive(Clone, Debug)]
pub struct Progress {
    /// The run, whose ledger every line is written to.
    pub run: RunId,
    /// The step running, for the cause of every line written here.
    pub step: CauseStep,
    pub state: State,
    /// The session's id, once launched.
    pub thread_id: Option<String>,
    /// The id of the build workspace, once the worker answered its create.
    pub worktree: Option<String>,
    /// The machine the run was claimed on, as its `claimed` line names it.
    pub machine: Option<String>,
    /// The run's newest pull request, for the status comment; none when absent.
    pub pr: Option<u64>,
    /// Why the run is stopping, set by [`go_stuck`] for the stuck comment; none otherwise.
    pub stop: Option<Stop>,
    /// What the ledger said when the step began: the comment-once guards read it.
    pub seen: Seen,
    /// The ledger already says the gate results are on the pull request.
    pub gate_results_posted: bool,
}

/// A message to the session that a forwarded comment or body edit carries, and for a body edit
/// the `body-edit-forwarded <edited_at>` step that records it. An action whose part is missing
/// is an [`InvalidTransition`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForwardPayload {
    pub message: Message,
    pub step: Option<String>,
}

/// What a failure sent back carries that the ledger does not: the message, and the attempt and
/// run ordinal recorded as sent back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SendPayload {
    pub message: Message,
    pub attempt: u32,
    pub run_ordinal: u32,
}

/// What the gate's and the forwards' actions carry that the ledger does not. An action whose
/// part is missing is an [`InvalidTransition`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ActionPayload {
    /// What `forward-comment` and `forward-body-edit` send.
    pub forward: Option<ForwardPayload>,
    /// The results comment `post-gate-results` posts on the pull request.
    pub comment: Option<String>,
    /// The failure `send-gate-failure` sends back.
    pub send: Option<SendPayload>,
}

/// Where the issue stands, from what the ledger says. `fallback` is the state of a run that has none yet.
pub fn progress_from(run: &RunId, step: CauseStep, seen: &Seen, fallback: State) -> Progress {
    Progress {
        run: run.clone(),
        step,
        state: seen.state.unwrap_or(fallback),
        thread_id: seen.thread_id.clone(),
        worktree: seen.worktree.clone().or_else(|| seen.workspace.clone()),
        machine: seen.machine.clone(),
        pr: seen.last_pr.as_ref().map(|pull| pull.pr),
        stop: None,
        seen: seen.clone(),
        gate_results_posted: seen.results_posted,
    }
}

/// `config` with the base the run's launch recorded (`Seen.base`, the issue's `## Base`), else
/// its own. Where a run's context is built, so every reader of `config.base` follows the run's
/// base.
pub fn run_config(config: &ProjectConfig, base: Option<&str>) -> ProjectConfig {
    match base {
        Some(base) if base != config.base => ProjectConfig {
            base: base.to_owned(),
            ..config.clone()
        },
        _ => config.clone(),
    }
}

/// The label, the status comment and the ledger line that follow a launch. Safe to repeat.
pub fn finish_building(
    ctx: &RunContext<'_>,
    issue: u64,
    at: &mut Progress,
) -> Result<(), RunError> {
    let (_, actions) = transition(at.state, &LifecycleEvent::SessionStarting)?;
    let cause = Cause::new(at.step, "session-started");
    apply_actions(ctx, issue, at, &actions, &cause, &ActionPayload::default())
}

/// Carries out the actions a transition asks for, in order. The label, release and status
/// comment are repeated safely; the two comments are posted once, as `at.seen` says.
/// `state_changed` is written last, so a ledger showing the old state means the transition may
/// be unfinished and runs again. Gate and forward actions without their payload are an
/// [`InvalidTransition`].
pub fn apply_actions(
    ctx: &RunContext<'_>,
    issue: u64,
    at: &mut Progress,
    actions: &[Action],
    cause: &Cause,
    payload: &ActionPayload,
) -> Result<(), RunError> {
    let invalid = |at: &Progress, action: &Action| {
        RunError::Transition(InvalidTransition {
            state: at.state,
            event: action.name(),
        })
    };
    let mut moved_to: Option<State> = None;
    for action in actions {
        match action {
            // Launched already.
            Action::LaunchSession => {}
            Action::PostQuestions { questions } => {
                if !at.seen.questions_posted {
                    post_once(
                        ctx,
                        &at.run,
                        issue,
                        QUESTIONS_COMMENTING,
                        at.seen.questions_commenting,
                        &messages::questions(questions),
                        cause,
                    )?;
                    ctx.record(
                        &at.run,
                        Event::QuestionsPosted {
                            questions: questions.clone(),
                        },
                        cause,
                    )?;
                }
            }
            Action::PostStuckComment { reason } => {
                if at.seen.stuck.is_none() {
                    if at.seen.stuck_stop.is_none()
                        && let Some(stop) = &at.stop
                    {
                        // The detail goes in a plain step of its own, which the ledger hides
                        // as it hides the `stuck` line. The decision holds short fields only:
                        // no detail and no output of a session or a setup, which the ledger's
                        // hiding cannot see through a JSON escape and cuts from its front past
                        // 64 KB. A resumed path reads the detail step back and the output again
                        // from where it came (`setup-failed` reads the tail; a session's text
                        // is its turn's, task 23's to read).
                        if let Some(detail) = &stop.detail {
                            ctx.record_step(&at.run, &format!("{STUCK_DETAIL}{detail}"), cause)?;
                        }
                        let decision = StuckStop {
                            code: cause.reason.clone(),
                            stop: Stop {
                                reason: stop.reason.clone(),
                                detail: None,
                                session_text: None,
                                recorded_text: None,
                                branch: stop.branch.clone(),
                                commits_ahead: stop.commits_ahead,
                            },
                        };
                        let json = serde_json::to_string(&decision)
                            .map_err(|error| RunError::Invalid(error.to_string()))?;
                        ctx.record_step(&at.run, &format!("{STUCK_STOP}{json}"), cause)?;
                    }
                    // The lifecycle's reason is the recorded one and may hold detail the public
                    // must not read, so without a stop the comment says only this.
                    let work = at.stop.as_ref().and_then(|stop| {
                        stop.branch.as_ref().map(|branch| WorkLeft {
                            branch: branch.clone(),
                            commits_ahead: stop.commits_ahead,
                            base: ctx.config.base.clone(),
                        })
                    });
                    let body = match &at.stop {
                        None => messages::stuck("the factory stopped this run", None, None),
                        Some(stop) => messages::stuck(
                            &stop.reason,
                            stop.session_text.as_deref(),
                            work.as_ref(),
                        ),
                    };
                    // The intent is found: the post may have landed, with text this pass no
                    // longer holds (the session's), so a newest comment of ours that opens as a
                    // stuck comment does is taken as it.
                    match at.seen.stuck_commenting {
                        None => {
                            let above = newest_own_id(ctx, issue)?;
                            ctx.record_step(
                                &at.run,
                                &format!("{STUCK_COMMENTING} {above}"),
                                cause,
                            )?;
                            ctx.post_comment(issue, &body)?;
                        }
                        Some(above) => {
                            if !newest_comment_starts_with(ctx, issue, STUCK_HEADER, above)? {
                                ctx.post_comment(issue, &body)?;
                            }
                        }
                    }
                    ctx.record(
                        &at.run,
                        Event::Stuck {
                            reason: reason.clone(),
                            branch: work.as_ref().map(|work| work.branch.clone()),
                            commits_ahead: work.as_ref().and_then(|work| work.commits_ahead),
                        },
                        cause,
                    )?;
                }
            }
            Action::SetLabel { state } => {
                ctx.set_state(issue, *state)?;
                moved_to = Some(*state);
            }
            Action::AssignToMachine => ctx.assign(issue)?,
            Action::Unassign => ctx.unassign(issue)?,
            Action::MarkPrDraft { pr } => ctx.mark_pr_draft(*pr)?,
            Action::MarkPrReady { pr } => ctx.mark_pr_ready(*pr)?,
            Action::PostGateResults { pr } => {
                if at.gate_results_posted {
                    continue;
                }
                let Some(body) = &payload.comment else {
                    return Err(invalid(at, action));
                };
                post_once_on_pr(
                    ctx,
                    &at.run,
                    *pr,
                    at.seen.gate_commenting,
                    body,
                    &Cause::new(at.step, "gate-results-commenting"),
                )?;
                ctx.record_step(
                    &at.run,
                    GATE_RESULTS_POSTED,
                    &Cause::new(at.step, "gate-results-posted"),
                )?;
                at.gate_results_posted = true;
            }
            Action::SendGateFailure { .. } => {
                let (Some(thread), Some(send)) = (&at.thread_id, &payload.send) else {
                    return Err(invalid(at, action));
                };
                send_once(
                    ctx,
                    at,
                    session_id(thread)?,
                    send.message.clone(),
                    &format!("gate-failure {} {}", send.attempt, send.run_ordinal),
                )?;
                ctx.record(
                    &at.run,
                    Event::GateSentBack {
                        attempt: send.attempt,
                        run_ordinal: send.run_ordinal,
                    },
                    &Cause::new(at.step, cause.reason.clone()).attempt(send.attempt),
                )?;
            }
            Action::ForwardComment { comment_id } => {
                let (Some(thread), Some(send)) = (&at.thread_id, &payload.forward) else {
                    return Err(invalid(at, action));
                };
                send_once(
                    ctx,
                    at,
                    session_id(thread)?,
                    send.message.clone(),
                    &format!("forward-comment {comment_id}"),
                )?;
                ctx.record(
                    &at.run,
                    Event::CommentForwarded {
                        comment_id: comment_id.clone(),
                    },
                    cause,
                )?;
            }
            Action::ForwardBodyEdit => {
                let (Some(thread), Some(send)) = (&at.thread_id, &payload.forward) else {
                    return Err(invalid(at, action));
                };
                let Some(step) = &send.step else {
                    return Err(invalid(at, action));
                };
                send_once(
                    ctx,
                    at,
                    session_id(thread)?,
                    send.message.clone(),
                    &format!("forward-body-edit {step}"),
                )?;
                ctx.record_step(&at.run, step, cause)?;
            }
            Action::UpdateStatusComment => {
                let machine = at.machine.as_deref().unwrap_or("none");
                let body = messages::status(
                    moved_to.unwrap_or(at.state),
                    machine,
                    at.thread_id.as_deref(),
                    at.pr,
                );
                ctx.upsert_status(issue, &body)?;
            }
            Action::PostUnapproved { .. } => return Err(invalid(at, action)),
        }
    }
    if let Some(to) = moved_to
        && to != at.state
    {
        ctx.record(&at.run, Event::StateChanged { from: at.state, to }, cause)?;
        at.state = to;
    }
    Ok(())
}

/// The key of a send other than `key` that may have typed and is not known typed: the one
/// [`send_once`] refuses every other send for ([`RunError::SendPending`]). The first in the
/// order `seen.sends` keeps them, if there are several.
pub(super) fn pending_send<'a>(seen: &'a Seen, key: &str) -> Option<&'a str> {
    seen.sends
        .iter()
        .find(|(other, sent)| other.as_str() != key && sent.pending())
        .map(|(other, _)| other.as_str())
}

/// Records that the message under `key` is known typed: the step `send-typed <key>`, written
/// by `step` of the runner.
pub(super) fn record_send_typed(
    ctx: &RunContext<'_>,
    run: &RunId,
    step: CauseStep,
    key: &str,
) -> Result<(), RunError> {
    ctx.record_step(
        run,
        &format!("{SEND_TYPED}{key}"),
        &Cause::new(step, "send-typed"),
    )
}

/// A message sent once to a session that has taken its launch (the launch and its resend by
/// count are sent by start.rs, before the session shows a turn). Every try goes under the
/// session's prompt count, so that a try whose line was typed is refused instead of typed again.
/// The `key` names the message in the ledger: each try writes `send-intent <key> <count>` before
/// the worker is asked, `send-refused <key>` after a refusal that typed nothing, and
/// `send-typed <key>` once the message is known typed.
///
/// The rule:
/// - A try that may have typed (its answer was lost, or the worker failed) keeps the key
///   pending. Every later try of the key goes under that try's count, and a count risen above
///   it is the message typed. A later try's refusal is of that try only.
/// - One message at a time per session: while a key is pending, a send of any other key waits
///   ([`RunError::SendPending`] names it), so no other send can raise the count the pending one
///   is judged by. A key stops being pending once it is known typed, or when its only tries were
///   refused.
/// - A key with no try that may have typed reads the count anew, and a count risen above it is
///   other prompts: the risen count is recorded and the next pass sends under it.
/// - No count, no send: a session the fleet shows no turn for waits
///   ([`RunError::NoPromptCount`]). A lost lease stops a try before its intent is written.
///
/// A prompt the session gets from anywhere else (a person typing in its terminal) also raises
/// the count, and reads as the pending message typed: the count cannot tell them apart.
/// `apply_actions` makes at most one send per session per call.
fn send_once(
    ctx: &RunContext<'_>,
    at: &Progress,
    session: SessionId,
    message: Message,
    key: &str,
) -> Result<(), RunError> {
    let before = at.seen.sends.get(key);
    if before.is_some_and(|sent| sent.typed) {
        return Ok(());
    }
    if let Some(pending) = pending_send(&at.seen, key) {
        return Err(RunError::SendPending {
            key: pending.to_owned(),
        });
    }
    let may_have_typed = before.is_some_and(SendTries::may_have_typed);
    let count = match before {
        Some(sent) if may_have_typed => sent.count,
        _ => {
            let Some(turn) = ctx.view.session_turn(session) else {
                return Err(RunError::NoPromptCount { session });
            };
            before.map_or(turn.prompts, |sent| turn.prompts.max(sent.count))
        }
    };
    let step =
        |name: &str, text: String| ctx.record_step(&at.run, &text, &Cause::new(at.step, name));
    let intent = |count: u64| step("send-intent", format!("{SEND_INTENT}{key} {count}"));
    let refused = || step("send-refused", format!("{SEND_REFUSED}{key}"));
    let typed = || record_send_typed(ctx, &at.run, at.step, key);
    // A workspace put at rest is woken before anything is sent to its session. The wake checks
    // the lease as the send does, and comes before the intent, so a wake that fails leaves no
    // try that may have typed.
    if let Some(workspace) = &at.worktree {
        ctx.wake(workspace_id(workspace)?)?;
    }
    ctx.held()?;
    intent(count)?;
    // The lease was checked just above, so no lost lease can follow the intent and leave a try
    // that asked nothing of the worker looking like one that may have typed.
    match ctx.worker.send(session, message, Some(count)) {
        Ok(_) => typed(),
        Err(SendRefused::CountRose { .. }) if may_have_typed => typed(),
        Err(SendRefused::CountRose { now }) => {
            intent(now)?;
            refused()?;
            Err(SendRefused::CountRose { now }.into())
        }
        Err(
            error @ (SendRefused::NotIdle
            | SendRefused::NoSuchSession
            | SendRefused::BodyWithoutPath
            | SendRefused::SecretsUnreadable),
        ) => {
            refused()?;
            Err(error.into())
        }
        // It may have typed: the key stays pending under `count`.
        Err(error @ SendRefused::Failed { .. }) => Err(error.into()),
    }
}

/// A comment on pull request `pr`, posted once, as [`post_once`] posts one on the issue: the
/// intent is recorded first, and a pass that finds it posts only when the newest comment of the
/// login on the pull request is not already this text.
fn post_once_on_pr(
    ctx: &RunContext<'_>,
    run: &RunId,
    pr: u64,
    intended: Option<u64>,
    body: &str,
    cause: &Cause,
) -> Result<(), RunError> {
    match intended {
        None => {
            let above = newest_own_id(ctx, pr)?;
            ctx.record_step(run, &format!("{GATE_RESULTS_COMMENTING} {above}"), cause)?;
        }
        Some(above) => {
            if newest_comment_is(ctx, pr, body, above)? {
                return Ok(());
            }
        }
    }
    ctx.comment_on_pr(pr, body)
}

/// A comment on `issue`, posted once. The `intent` step is recorded before the post, with the id
/// of the newest marked comment of the login. A pass that finds the intent (`intended`) and not
/// the line that follows the post does not know whether the post landed (an answer lost to a
/// timeout or a 5xx), so it posts only when no comment of the login newer than that id is
/// already this text.
fn post_once(
    ctx: &RunContext<'_>,
    run: &RunId,
    issue: u64,
    intent: &str,
    intended: Option<u64>,
    body: &str,
    cause: &Cause,
) -> Result<(), RunError> {
    match intended {
        None => {
            let above = newest_own_id(ctx, issue)?;
            ctx.record_step(run, &format!("{intent} {above}"), cause)?;
        }
        Some(above) => {
            if newest_comment_is(ctx, issue, body, above)? {
                return Ok(());
            }
        }
    }
    ctx.post_comment(issue, body)
}

/// The first line of every stuck comment, as [`messages::stuck`] opens it.
const STUCK_HEADER: &str = "The factory could not finish this issue and has released it.";

/// Whether the newest comment of the project's login that carries the factory's marker, newer
/// than comment `above`, opens with `prefix`.
fn newest_comment_starts_with(
    ctx: &RunContext<'_>,
    issue: u64,
    prefix: &str,
    above: u64,
) -> Result<bool, RunError> {
    let newest = newest_own(ctx, issue)?;
    Ok(newest.is_some_and(|own| own.id > above && own.body.starts_with(prefix)))
}

/// A comment of the project's login as GitHub lists it.
#[derive(serde::Deserialize)]
struct CommentRow {
    id: u64,
    #[serde(default)]
    user: Option<CommentUser>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
}

#[derive(serde::Deserialize)]
struct CommentUser {
    login: String,
}

/// The newest comment of the project's login that ends with the factory's marker.
pub(crate) struct OwnComment {
    pub id: u64,
    /// As [`as_posted`] gives a text: lines ended `\n`, the end trimmed.
    pub body: String,
    /// GitHub's creation time; empty when GitHub gave none.
    pub created_at: String,
}

/// The newest comment of the project's login on `issue` (or pull request) that ends with the
/// factory's marker. Anyone can paste the marker, so only the login's own comment counts. The
/// one reading of "has the factory already said this" for every comment it posts once.
pub(crate) fn newest_own(ctx: &RunContext<'_>, issue: u64) -> Result<Option<OwnComment>, RunError> {
    let args = vec![
        "api".to_string(),
        format!("repos/{}/issues/{issue}/comments?per_page=100", ctx.repo()),
        "--paginate".to_string(),
    ];
    let rows: Vec<CommentRow> = gh_pages(ctx.tracker, &args, "JSON arrays of comments")?;
    Ok(rows
        .into_iter()
        .filter(|row| {
            row.user
                .as_ref()
                .is_some_and(|user| user.login.eq_ignore_ascii_case(&ctx.project.login))
        })
        .filter_map(|row| {
            let body = normal(&row.body.unwrap_or_default());
            body.ends_with(MARKER).then(|| OwnComment {
                id: row.id,
                body,
                created_at: row.created_at.unwrap_or_default(),
            })
        })
        .max_by_key(|own| own.id))
}

/// A comment's body with its lines ended `\n` and its end trimmed.
fn normal(body: &str) -> String {
    body.replace("\r\n", "\n").trim_end().to_owned()
}

/// `said` as the factory's comment holds it once posted, in the form [`newest_own`] gives.
pub(crate) fn as_posted(ctx: &RunContext<'_>, said: &str) -> String {
    normal(&format!(
        "{}\n\n{MARKER}",
        text::for_comment(&ctx.tracker.hide(said))
    ))
}

/// The id of the newest comment of the login with the marker, 0 when there is none.
fn newest_own_id(ctx: &RunContext<'_>, issue: u64) -> Result<u64, RunError> {
    Ok(newest_own(ctx, issue)?.map_or(0, |own| own.id))
}

/// Whether the newest comment of the project's login that carries the factory's marker, newer
/// than comment `above`, is `said` as it is posted.
fn newest_comment_is(
    ctx: &RunContext<'_>,
    issue: u64,
    said: &str,
    above: u64,
) -> Result<bool, RunError> {
    let expected = as_posted(ctx, said);
    Ok(newest_own(ctx, issue)?.is_some_and(|own| own.id > above && own.body == expected))
}

/// Stops the run's session before it goes stuck: ends the factory sessions of its build
/// workspace, which is kept. A run with no session, or no workspace, has nothing to stop. A
/// failed call fails the step with nothing written and the next pass stops again.
pub fn stop_session(ctx: &RunContext<'_>, at: &Progress) -> Result<(), RunError> {
    if at.thread_id.is_none() && !at.seen.launch_sent {
        return Ok(());
    }
    let Some(workspace) = &at.worktree else {
        return Ok(());
    };
    ctx.end_sessions(workspace_id(workspace)?)
}

/// Why a run goes stuck. `reason` is the public one: a fixed factory line, the only text that
/// goes in the comment's `Reason:`. `detail` holds what the public must not read and stays in
/// the ledger. `session_text` and `branch` are already through `prepare_session_text` or
/// `prepare_output_tail` and left out when empty. `recorded_text` is what the ledger keeps in
/// place of `session_text` when it holds more of it than the comment does: a setup's output is
/// quoted by its last 20,000 characters and recorded by its last 64 KB.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Stop {
    pub reason: String,
    pub detail: Option<String>,
    pub session_text: Option<String>,
    pub recorded_text: Option<String>,
    pub branch: Option<String>,
    pub commits_ahead: Option<u32>,
}

impl Stop {
    /// A stop for the public `reason` alone.
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            ..Self::default()
        }
    }
}

/// The decision to go stuck as the ledger keeps it before the comment is posted: the code that
/// caused the lines and the stop, so a later pass finishes the path with the same words.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct StuckStop {
    pub code: String,
    pub stop: Stop,
}

/// The reason as the ledger stores it and the caller gets it: the factory's words, then the session's.
pub fn recorded_reason(stop: &Stop) -> String {
    let session = stop.recorded_text.as_ref().or(stop.session_text.as_ref());
    let words = stop.detail.as_deref().unwrap_or(&stop.reason);
    match session {
        None => words.to_owned(),
        Some(text) => format!("{words}\n\n{text}"),
    }
}

/// The stuck path. The session is stopped first (see [`stop_session`]), so a failure leaves the
/// issue as it was and the next pass retries. Then `after_stop`, the actions
/// [`transition`] asks for, `state_changed` and the summary line. A comment the ledger already
/// records is skipped, a ready pull request goes back to draft first, and `code` is the reason
/// of every line written.
pub fn go_stuck(
    ctx: &RunContext<'_>,
    issue: u64,
    at: &mut Progress,
    code: &str,
    stop: Stop,
    after_stop: Option<&dyn Fn() -> Result<(), RunError>>,
) -> Result<(), RunError> {
    let cause = Cause::new(at.step, code);
    if at.state == State::Stuck {
        ctx.ledger
            .finish(&at.run, Outcome::Stuck, None, None, &cause, None)?;
        return Ok(());
    }
    stop_session(ctx, at)?;
    if let Some(workspace) = &at.worktree {
        ctx.keep(workspace_id(workspace)?)?;
    }
    if let Some(after_stop) = after_stop {
        after_stop()?;
    }
    // A pull request that was ready for review is not left looking ready: the factory no
    // longer vouches for it. Safe to repeat.
    if at.state == State::PrOpen
        && let Some(pr) = at.pr
    {
        ctx.mark_pr_draft(pr)?;
    }
    let (_, actions) = transition(
        at.state,
        &LifecycleEvent::Failed {
            reason: recorded_reason(&stop),
        },
    )?;
    at.stop = Some(stop);
    apply_actions(ctx, issue, at, &actions, &cause, &ActionPayload::default())?;
    ctx.ledger
        .finish(&at.run, Outcome::Stuck, None, None, &cause, None)?;
    Ok(())
}

/// Ends a run whose pull request was merged or closed, from `pr-open` or from `building` (a
/// person ended it before the gate marked it ready). The session is stopped first; then the
/// summary line, the only write, caused by `step`. Nothing is posted. A gone or unmerged pull
/// request is closed.
pub fn end_run(
    ctx: &RunContext<'_>,
    at: &Progress,
    pr_url: &str,
    pull: Option<&PullRequest>,
    step: CauseStep,
) -> Result<(), RunError> {
    stop_session(ctx, at)?;
    let end = if pull.is_some_and(|pull| pull.merged) {
        PrEnd::Merged
    } else {
        PrEnd::Closed
    };
    ctx.ledger.finish(
        &at.run,
        Outcome::PrOpen,
        None,
        Some(pr_url),
        &Cause::new(step, "pull-request-ended"),
        Some(end),
    )?;
    Ok(())
}

/// What a closed issue means for a run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Closing {
    /// The issue is open, or the run has nothing left to write and was not asked.
    Open,
    /// A person closed it: a move, and the run is abandoned.
    PersonsMove,
    /// The run's own pull request was merged, which closed the issue.
    OwnMerge(PullRequest),
}

/// Whether a closed issue is a person's move for this run, or the run's own finish: the latter
/// when a pull request the run reported (`pr_opened` lines) is merged, gate passed or not. A
/// merge before the first `advance` after the session's report still reads as a person's move.
/// `closed` is what GitHub says about the issue. A failed read is an error, never an answer.
pub fn moved_on_by_closing(
    ctx: &RunContext<'_>,
    seen: &Seen,
    closed: bool,
) -> Result<Closing, RunError> {
    if !closed {
        return Ok(Closing::Open);
    }
    for pr in &seen.opened {
        if let Some(pull) = ctx.tracker.pull_request(ctx.repo(), *pr)?
            && pull.merged
        {
            return Ok(Closing::OwnMerge(pull));
        }
    }
    Ok(Closing::PersonsMove)
}

/// What the run's issue being closed means for it, with the issue as read for that (`None`
/// when it was not read), so a caller that needs the issue too does not read it again. A run
/// that has nothing left to write (`pr-open`, `stuck`) asks nothing of GitHub, and neither does
/// a step that would skip the run (the caller checks that first).
pub fn closed_issue(
    ctx: &RunContext<'_>,
    run: &RunId,
    seen: &Seen,
) -> Result<(Closing, Option<super::github::IssueDetail>), RunError> {
    if matches!(seen.state, Some(State::PrOpen | State::Stuck)) {
        return Ok((Closing::Open, None));
    }
    let detail = ctx.tracker.issue(ctx.repo(), run.issue())?;
    let closing = moved_on_by_closing(ctx, seen, detail.closed)?;
    Ok((closing, Some(detail)))
}

/// Finishes a run whose issue a person closed: ends its session first (an error is an error
/// with nothing written), then finishes the ledger as `needs-info` for a run left in that state
/// and `stuck` otherwise. Nothing is written on GitHub. Returns true when a session may exist
/// that this could not end: a launch was sent but no session recorded.
pub fn abandon(
    ctx: &RunContext<'_>,
    run: &RunId,
    seen: &Seen,
    step: CauseStep,
) -> Result<bool, RunError> {
    stop_session(ctx, &progress_from(run, step, seen, State::Triage))?;
    let outcome = if seen.state == Some(State::NeedsInfo) {
        Outcome::NeedsInfo
    } else {
        Outcome::Stuck
    };
    ctx.ledger.finish(
        run,
        outcome,
        None,
        None,
        &Cause::new(step, "issue-closed"),
        None,
    )?;
    Ok(seen.launch_sent && seen.thread_id.is_none())
}

/// Ends a run whose own merged pull request closed its issue before a gate pass was recorded:
/// [`end_run`] with the URL the session wrote for that pull request (or the one GitHub would
/// give it, for a pull request the ledger holds under another number). `step` is the step that
/// found it.
pub fn end_own_merge(
    ctx: &RunContext<'_>,
    run: &RunId,
    seen: &Seen,
    step: CauseStep,
    pull: &PullRequest,
) -> Result<(), RunError> {
    let url = match &seen.last_pr {
        Some(last) if last.pr == pull.number => last.url.clone(),
        _ => format!("https://github.com/{}/pull/{}", run.repo(), pull.number),
    };
    end_run(
        ctx,
        &progress_from(run, step, seen, State::Triage),
        &url,
        Some(pull),
        step,
    )
}

/// How long a wait has lasted on the sleep-counting clock: from `since`, the lease generation
/// and clock reading (ms) recorded at its start, to `now` under `generation`. `None` when the
/// count starts again: no record, one of another generation (a takeover), or one above the
/// clock now (a reboot that reset the clock). A takeover can lengthen a wait, never end it
/// early. Every wait the runs count on that clock reads it here.
pub fn time_waited(
    since: Option<(u64, u64)>,
    generation: u64,
    now: u64,
) -> Option<std::time::Duration> {
    match since {
        Some((from, at)) if from == generation && now >= at => {
            Some(std::time::Duration::from_millis(now - at))
        }
        _ => None,
    }
}

/// The id of `text` as a session, or why it is not one.
pub fn session_id(text: &str) -> Result<SessionId, RunError> {
    uuid::Uuid::parse_str(text)
        .map(SessionId)
        .map_err(|_| RunError::Invalid(format!("the ledger's session id {text:?} is not an id")))
}

/// The id of `text` as a workspace, or why it is not one.
pub fn workspace_id(text: &str) -> Result<WorkspaceId, RunError> {
    uuid::Uuid::parse_str(text)
        .map(WorkspaceId)
        .map_err(|_| RunError::Invalid(format!("the ledger's workspace id {text:?} is not an id")))
}

/// What the run tests share: GitHub in memory, the worker's fake, a ledger in a store of its
/// own and a context built from them.
#[cfg(test)]
pub(crate) mod testing {
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::{Arc, Mutex, PoisonError};

    use foundry_proto::factory::{FactorySettings, ProjectFactory, RunEvent, RunId};
    use foundry_proto::ids::{MachineId, ProjectId};
    use serde_json::{Value, json};

    use super::*;
    use crate::clock::FakeClock;
    use crate::factory::config::Limits;
    use crate::factory::github::{Hiding, OtherKind, Tracker};
    use crate::factory::ledger::testing::{T0, open_store, ticking};
    use crate::factory::worker::{AlwaysHeld, FakeWorker, Target};

    pub const REPO: &str = "rawphp/softfac";
    pub const LOGIN: &str = "softfac-bot";
    pub const ISSUE: u64 = 8;
    /// When the instructor applied `factory:ready`.
    pub const LABELED_AT: &str = "2026-10-04T10:00:00Z";
    pub const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    /// A pull request GitHub shows.
    #[derive(Clone, Debug)]
    pub struct SimPull {
        pub state: &'static str,
        pub draft: bool,
        pub head: String,
        pub base: String,
        pub cross: bool,
    }

    struct SimState {
        title: String,
        body: String,
        labels: Vec<String>,
        assignees: Vec<String>,
        closed: bool,
        /// The comments in the order GitHub lists them: id, author, body.
        comments: Vec<(u64, String, String)>,
        next_comment: u64,
        approval: String,
        pulls: BTreeMap<u64, SimPull>,
        listed: Vec<Value>,
        closing: Vec<Value>,
        branches: BTreeSet<String>,
        failures: Vec<(String, usize)>,
        lost: Vec<(String, usize)>,
        /// Calls containing one of these are refused as an invalid argument, as a branch name
        /// the tracker will not take is.
        refused: Vec<String>,
        calls: Vec<Vec<String>>,
    }

    /// GitHub in memory for one issue of [`REPO`], answering the `gh` calls the factory makes by
    /// their arguments, as the fake tracker does, and keeping what the writes change. A call can
    /// be made to fail with [`Sim::fail`].
    pub struct Sim {
        state: Mutex<SimState>,
    }

    fn created_at(id: u64) -> String {
        format!("2026-10-04T11:{:02}:{:02}Z", (id / 60) % 60, id % 60)
    }

    /// The approval answer for a label applied at `labeled_at` by `by`, with every edit since
    /// by the logins in `edits_by`.
    pub fn approval_answer(labeled_at: &str, by: &str, edits_by: &[(&str, &str)]) -> String {
        let actor = |login: &str| json!({"__typename": "User", "login": login});
        let nodes: Vec<Value> = edits_by
            .iter()
            .rev()
            .map(|(at, who)| json!({"editedAt": at, "editor": actor(who)}))
            .collect();
        let last = edits_by.last();
        json!({"data": {"repository": {"issue": {
            "timelineItems": {"totalCount": 1, "nodes": [{
                "__typename": "LabeledEvent",
                "createdAt": labeled_at,
                "actor": actor(by),
                "label": {"name": "factory:ready"},
            }]},
            "lastEditedAt": last.map(|(at, _)| *at),
            "editor": last.map_or(Value::Null, |(_, who)| actor(who)),
            "userContentEdits": {"totalCount": nodes.len(), "nodes": nodes},
        }}}})
        .to_string()
    }

    impl Sim {
        /// An open issue labelled `factory:ready`, nobody assigned, approved by `tom` at
        /// [`LABELED_AT`].
        pub fn new() -> Arc<Self> {
            Arc::new(Self {
                state: Mutex::new(SimState {
                    title: "Make it faster".into(),
                    body: "Do the thing.".into(),
                    labels: vec!["factory:ready".into()],
                    assignees: vec![],
                    closed: false,
                    comments: vec![],
                    next_comment: 100,
                    approval: approval_answer(LABELED_AT, "tom", &[]),
                    pulls: BTreeMap::new(),
                    listed: vec![],
                    closing: vec![],
                    branches: BTreeSet::new(),
                    failures: vec![],
                    lost: vec![],
                    refused: vec![],
                    calls: vec![],
                }),
            })
        }

        fn lock(&self) -> std::sync::MutexGuard<'_, SimState> {
            self.state.lock().unwrap_or_else(PoisonError::into_inner)
        }

        pub fn set_labels(&self, labels: &[&str]) {
            self.lock().labels = labels.iter().map(|l| l.to_string()).collect();
        }

        pub fn set_assignees(&self, assignees: &[&str]) {
            self.lock().assignees = assignees.iter().map(|l| l.to_string()).collect();
        }

        pub fn set_body(&self, body: &str) {
            self.lock().body = body.into();
        }

        pub fn set_closed(&self, closed: bool) {
            self.lock().closed = closed;
        }

        pub fn set_approval(&self, answer: String) {
            self.lock().approval = answer;
        }

        pub fn add_branch(&self, branch: &str) {
            self.lock().branches.insert(branch.into());
        }

        pub fn set_pull(&self, number: u64, pull: SimPull) {
            self.lock().pulls.insert(number, pull);
        }

        /// What the closing-pull-requests query answers: one open pull request closing the
        /// issue, by `author`.
        pub fn closing_pull(&self, number: u64, head: &str, base: &str, author: &str, cross: bool) {
            self.lock().closing.push(json!({
                "number": number,
                "url": format!("https://github.com/{REPO}/pull/{number}"),
                "state": "OPEN",
                "headRefName": head,
                "baseRefName": base,
                "isCrossRepository": cross,
                "author": {"__typename": "User", "login": author},
            }));
        }

        /// A comment by `author`, as if posted on GitHub.
        pub fn comment_as(&self, author: &str, body: &str) -> u64 {
            let mut state = self.lock();
            let id = state.next_comment;
            state.next_comment += 1;
            state
                .comments
                .push((id, author.to_string(), body.to_string()));
            id
        }

        /// The next `times` calls whose arguments hold `needle` take effect, and answer an
        /// error all the same: a timeout or a 5xx after GitHub applied the change.
        pub fn lose(&self, needle: &str, times: usize) {
            self.lock().lost.push((needle.into(), times));
        }

        /// Makes the next `times` calls whose arguments contain `needle` fail.
        pub fn fail(&self, needle: &str, times: usize) {
            self.lock().failures.push((needle.into(), times));
        }

        /// Makes every call whose arguments contain `needle` fail as an invalid argument.
        pub fn refuse(&self, needle: &str) {
            self.lock().refused.push(needle.into());
        }

        pub fn calls(&self) -> Vec<Vec<String>> {
            self.lock().calls.clone()
        }

        /// The calls that changed something, as one line each.
        pub fn writes(&self) -> Vec<String> {
            self.calls()
                .iter()
                .filter(|call| {
                    let first = call.first().map(String::as_str);
                    let second = call.get(1).map(String::as_str);
                    matches!(
                        (first, second),
                        (Some("issue"), Some("edit" | "comment"))
                            | (Some("pr"), Some("ready" | "comment"))
                    ) || call.iter().any(|a| a == "PATCH")
                })
                .map(|call| call.join(" "))
                .collect()
        }

        pub fn labels(&self) -> Vec<String> {
            self.lock().labels.clone()
        }

        pub fn assignees(&self) -> Vec<String> {
            self.lock().assignees.clone()
        }

        /// The bodies of the comments on the issue, in the order listed.
        pub fn comments(&self) -> Vec<String> {
            self.lock()
                .comments
                .iter()
                .map(|(_, _, body)| body.clone())
                .collect()
        }

        fn other(args: &[String], message: &str) -> TrackerError {
            TrackerError::Other {
                kind: OtherKind::Failed,
                message: format!("`gh {}` failed: {message}", args.join(" ")),
            }
        }

        fn not_found(args: &[String]) -> TrackerError {
            TrackerError::NotFound {
                command: format!("gh {}", args.join(" ")),
                status: 1,
                stderr: "HTTP 404: Not Found".into(),
            }
        }

        fn flag_values(args: &[String], flag: &str) -> Vec<String> {
            args.windows(2)
                .filter(|pair| pair[0] == flag)
                .map(|pair| pair[1].clone())
                .collect()
        }

        fn answer(&self, args: &[String]) -> Result<String, TrackerError> {
            let lost = {
                let mut state = self.lock();
                let joined = args.join(" ");
                match state
                    .lost
                    .iter()
                    .position(|(needle, times)| *times > 0 && joined.contains(needle.as_str()))
                {
                    Some(at) => {
                        state.lost[at].1 -= 1;
                        true
                    }
                    None => false,
                }
            };
            let result = self.answer_once(args);
            if lost && result.is_ok() {
                // The change landed and the answer did not.
                return Err(Self::other(args, "answer lost"));
            }
            result
        }

        fn answer_once(&self, args: &[String]) -> Result<String, TrackerError> {
            let mut state = self.lock();
            state.calls.push(args.to_vec());
            let joined = args.join(" ");
            if let Some(at) = state
                .failures
                .iter()
                .position(|(needle, times)| *times > 0 && joined.contains(needle.as_str()))
            {
                state.failures[at].1 -= 1;
                return Err(Self::other(args, "injected failure"));
            }
            if state
                .refused
                .iter()
                .any(|needle| joined.contains(needle.as_str()))
            {
                return Err(TrackerError::Other {
                    kind: OtherKind::InvalidArgument,
                    message: format!("`gh {joined}` was refused: not a name"),
                });
            }
            let word = |at: usize| args.get(at).map(String::as_str);
            match (word(0), word(1)) {
                (Some("issue"), Some("view")) => {
                    if args.iter().any(|a| a == "labels") {
                        let labels: Vec<Value> =
                            state.labels.iter().map(|n| json!({"name": n})).collect();
                        return Ok(json!({"labels": labels}).to_string());
                    }
                    Ok(json!({
                        "number": ISSUE,
                        "title": state.title,
                        "body": state.body,
                        "labels": state.labels.iter().map(|n| json!({"name": n})).collect::<Vec<_>>(),
                        "assignees": state.assignees.iter().map(|n| json!({"login": n})).collect::<Vec<_>>(),
                        "state": if state.closed { "CLOSED" } else { "OPEN" },
                        "author": {"login": "tom"},
                    })
                    .to_string())
                }
                (Some("issue"), Some("edit")) => {
                    for label in Self::flag_values(args, "--add-label") {
                        if !state.labels.contains(&label) {
                            state.labels.push(label);
                        }
                    }
                    for label in Self::flag_values(args, "--remove-label") {
                        state.labels.retain(|l| *l != label);
                    }
                    for login in Self::flag_values(args, "--add-assignee") {
                        if !state.assignees.contains(&login) {
                            state.assignees.push(login);
                        }
                    }
                    for login in Self::flag_values(args, "--remove-assignee") {
                        state.assignees.retain(|l| *l != login);
                    }
                    Ok(String::new())
                }
                (Some("issue"), Some("comment")) => {
                    let body = Self::flag_values(args, "--body").pop().unwrap_or_default();
                    let id = state.next_comment;
                    state.next_comment += 1;
                    state.comments.push((id, LOGIN.to_string(), body));
                    Ok(String::new())
                }
                (Some("api"), Some("graphql")) if joined.contains("closedByPullRequestsReferences") => {
                    Ok(json!({"data": {"repository": {"issue": {"closedByPullRequestsReferences": {
                        "nodes": state.closing.clone(),
                    }}}}})
                    .to_string())
                }
                (Some("api"), Some("graphql")) => Ok(state.approval.clone()),
                (Some("api"), Some("-X")) if word(3).is_some_and(|path| path.contains("/issues/comments/")) => {
                    let path = word(3).unwrap_or_default();
                    let id: u64 = path
                        .rsplit('/')
                        .next()
                        .and_then(|id| id.parse().ok())
                        .unwrap_or(0);
                    let body = Self::flag_values(args, "-f")
                        .pop()
                        .and_then(|f| f.strip_prefix("body=").map(str::to_owned))
                        .unwrap_or_default();
                    if let Some(row) = state.comments.iter_mut().find(|row| row.0 == id) {
                        row.2 = body;
                    }
                    Ok(String::new())
                }
                (Some("api"), Some(path))
                    if path.contains("/issues/") && path.contains("/comments") =>
                {
                    let rows: Vec<Value> = state
                        .comments
                        .iter()
                        .map(|(id, author, body)| {
                            json!({"id": id, "user": {"login": author}, "body": body, "created_at": created_at(*id)})
                        })
                        .collect();
                    Ok(Value::Array(rows).to_string())
                }
                (Some("api"), Some(path)) if path.contains("/git/ref/heads/") => {
                    let branch = path.split("/git/ref/heads/").nth(1).unwrap_or_default();
                    if state.branches.contains(branch) {
                        Ok(json!({"object": {"sha": SHA, "type": "commit"}}).to_string())
                    } else {
                        Err(Self::not_found(args))
                    }
                }
                (Some("pr"), Some("view")) => {
                    let number: u64 = word(2).and_then(|n| n.parse().ok()).unwrap_or(0);
                    match state.pulls.get(&number) {
                        Some(pull) => Ok(json!({
                            "number": number,
                            "isDraft": pull.draft,
                            "state": pull.state,
                            "headRefName": pull.head,
                            "isCrossRepository": pull.cross,
                            "baseRefName": pull.base,
                            "headRefOid": SHA,
                            "mergeable": "MERGEABLE",
                            "createdAt": LABELED_AT,
                        })
                        .to_string()),
                        None => Err(Self::other(args, "Could not resolve to a PullRequest")),
                    }
                }
                (Some("pr"), Some("list")) => Ok(Value::Array(state.listed.clone()).to_string()),
                (Some("pr"), Some("comment")) => {
                    let body = Self::flag_values(args, "--body").pop().unwrap_or_default();
                    let id = state.next_comment;
                    state.next_comment += 1;
                    state.comments.push((id, LOGIN.to_string(), body));
                    Ok(String::new())
                }
                (Some("pr"), Some("ready")) => Ok(String::new()),
                _ => Err(Self::other(args, "the simulator does not know this call")),
            }
        }
    }

    impl Tracker for Sim {
        fn gh(&self, args: &[String]) -> Result<String, TrackerError> {
            self.answer(args)
        }
    }

    /// A guard that holds the lease for `n` checks and then loses it, to cut a step short between
    /// two of its calls.
    pub struct HeldFor(pub std::sync::atomic::AtomicUsize);

    impl HeldFor {
        pub fn new(checks: usize) -> Self {
            Self(std::sync::atomic::AtomicUsize::new(checks))
        }
    }

    impl Guard for HeldFor {
        fn check(&self) -> Result<u64, crate::factory::worker::LeaseLost> {
            use std::sync::atomic::Ordering;
            loop {
                let left = self.0.load(Ordering::SeqCst);
                if left == 0 {
                    return Err(crate::factory::worker::LeaseLost {
                        reason: "the test's guard ran out of checks".into(),
                    });
                }
                if self
                    .0
                    .compare_exchange(left, left - 1, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    return Ok(1);
                }
            }
        }
    }

    /// Everything a step of a run needs, built from fakes.
    pub struct Rig {
        pub _dir: tempfile::TempDir,
        pub ledger: Ledger,
        pub sim: Arc<Sim>,
        pub tracker: Hiding,
        pub worker: FakeWorker,
        pub view: FleetView,
        pub guard: Box<dyn Guard>,
        pub clock: FakeClock,
        pub project: RunProject,
        pub config: ProjectConfig,
        pub settings: RunSettings,
        /// The machine the build is placed on.
        pub machine: MachineId,
    }

    pub fn project_id() -> ProjectId {
        ProjectId(uuid::Uuid::from_u128(0x7001))
    }

    pub fn run_id(attempt: u32) -> RunId {
        RunId::new(REPO, ISSUE, attempt).unwrap()
    }

    pub fn config() -> ProjectConfig {
        ProjectConfig {
            instructors: vec!["tom".into()],
            base: "main".into(),
            provision: vec![],
            gate: vec!["cargo test".into()],
            required_checks: vec![],
            limits: Limits {
                minutes: 90,
                gate_retries: 2,
                checks_minutes: 30,
                interruption_resumes: 2,
            },
            owner_set: false,
        }
    }

    impl Rig {
        pub fn new() -> Self {
            let (dir, store) = open_store();
            let me = MachineId(uuid::Uuid::from_u128(0x1));
            let ledger = crate::factory::ledger::testing::ledger(&store, me, 1, ticking(T0, 1000));
            let sim = Sim::new();
            let tracker = Hiding::new(sim.clone(), Arc::new(|text: &str| text.to_string()));
            Self {
                _dir: dir,
                ledger,
                sim,
                tracker,
                worker: FakeWorker::new(),
                view: FleetView::default(),
                guard: Box::new(AlwaysHeld(1)),
                clock: FakeClock::new(),
                project: RunProject {
                    id: project_id(),
                    repo: REPO.into(),
                    login: LOGIN.into(),
                },
                config: config(),
                settings: RunSettings {
                    fleet: FactorySettings::default(),
                    project: ProjectFactory {
                        on: true,
                        login: LOGIN.into(),
                        paused: false,
                    },
                },
                machine: MachineId(uuid::Uuid::from_u128(0x2)),
            }
        }

        pub fn ctx(&self) -> RunContext<'_> {
            RunContext {
                tracker: &self.tracker,
                worker: &self.worker,
                view: &self.view,
                ledger: &self.ledger,
                guard: self.guard.as_ref(),
                project: &self.project,
                config: &self.config,
                settings: &self.settings,
                clock: &self.clock,
            }
        }

        /// The events `run` holds, in the ledger's order.
        pub fn events(&self, run: &RunId) -> Vec<RunEvent> {
            self.ledger.read(run).unwrap()
        }

        /// The names of the events `run` holds, in order.
        pub fn names(&self, run: &RunId) -> Vec<String> {
            self.events(run).into_iter().map(|e| e.event).collect()
        }

        /// The `step_changed` texts of `run`, in order.
        pub fn steps(&self, run: &RunId) -> Vec<String> {
            self.events(run)
                .into_iter()
                .filter(|e| e.event == "step_changed")
                .filter_map(|e| e.data["step"].as_str().map(str::to_owned))
                .collect()
        }

        /// Writes `event` to `run` as a step of the test.
        pub fn write(&self, run: &RunId, event: Event) {
            self.ledger
                .write(run, &event, &Cause::new(CauseStep::Start, "test"))
                .unwrap();
        }

        pub fn step(&self, run: &RunId, text: &str) {
            self.write(run, Event::StepChanged { step: text.into() });
        }

        pub fn moved(&self, run: &RunId, from: State, to: State) {
            self.write(run, Event::StateChanged { from, to });
        }

        pub fn finished(&self, run: &RunId) -> bool {
            self.events(run).iter().any(|e| e.event == "finished")
        }

        /// The claim comment as Foundry posts it, as the project's login.
        pub fn foundry_claim(&self, run: &RunId, machine: MachineId) -> u64 {
            self.sim.comment_as(
                LOGIN,
                &crate::factory::github::claim_comment(run, &Target::Machine(machine)),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use foundry_proto::ids::MachineId;

    use super::testing::*;
    use super::*;
    use crate::factory::github::STATUS_MARKER;
    use crate::factory::ledger::events::ResumedFrom;
    use crate::factory::worker::{Call, HostCall, Lost, Target};

    const SESSION: &str = "00000000-0000-0000-0000-0000000005e5";
    const WORKSPACE: &str = "00000000-0000-0000-0000-000000000a11";

    fn machine() -> MachineId {
        MachineId(uuid::Uuid::from_u128(0x2))
    }

    /// A run that was claimed and launched, and is building.
    fn building(rig: &Rig, run: &RunId) {
        rig.step(run, &format!("approved {LABELED_AT}"));
        rig.write(
            run,
            Event::Claimed {
                machine: machine().to_string(),
                github_user: LOGIN.into(),
            },
        );
        rig.moved(run, State::Ready, State::Triage);
        rig.step(run, &format!("workspace-created {WORKSPACE}"));
        rig.write(
            run,
            Event::SessionLaunched {
                thread_id: SESSION.into(),
                worktree: Some(WORKSPACE.into()),
                branch: Some("fac/8-1".into()),
                model_instance: None,
                model: Some("opus".into()),
                model_options: None,
                resumed_from: None::<ResumedFrom>,
                base: Some("main".into()),
                title: Some("Make it faster".into()),
            },
        );
        rig.moved(run, State::Triage, State::Building);
        rig.sim.set_labels(&["factory:building"]);
        rig.sim.set_assignees(&[LOGIN]);
    }

    fn progress(rig: &Rig, run: &RunId, fallback: State) -> Progress {
        let seen = rig.ctx().seen(run).unwrap();
        progress_from(run, CauseStep::Start, &seen, fallback)
    }

    fn stuck_stop() -> Stop {
        Stop::new("the session said stuck")
    }

    mod the_stop_a_run_records {
        use super::*;

        #[test]
        fn is_the_factorys_words_then_the_sessions() {
            let stop = Stop {
                reason: "public".into(),
                detail: Some("private detail".into()),
                session_text: Some("what the session said".into()),
                ..Stop::default()
            };
            assert_eq!(
                recorded_reason(&stop),
                "private detail\n\nwhat the session said"
            );
            let plain = Stop {
                session_text: None,
                ..stop.clone()
            };
            assert_eq!(recorded_reason(&plain), "private detail");
            let no_detail = Stop {
                detail: None,
                ..stop.clone()
            };
            assert_eq!(
                recorded_reason(&no_detail),
                "public\n\nwhat the session said"
            );
        }

        #[test]
        fn keeps_the_longer_text_when_the_comment_quotes_less_of_it() {
            let stop = Stop {
                reason: "setup failed".into(),
                session_text: Some("last 20000".into()),
                recorded_text: Some("last 64 KB".into()),
                ..Stop::default()
            };
            assert_eq!(recorded_reason(&stop), "setup failed\n\nlast 64 KB");
        }
    }

    mod where_a_run_stands {
        use super::*;

        #[test]
        fn is_read_from_the_ledger_with_the_fallback_for_a_run_with_no_state() {
            let rig = Rig::new();
            let run = run_id(1);
            rig.step(&run, "approved x");
            let at = progress(&rig, &run, State::Triage);
            assert_eq!(at.state, State::Triage);
            assert_eq!(at.thread_id, None);
            building(&rig, &run);
            let at = progress(&rig, &run, State::Triage);
            assert_eq!(at.state, State::Building);
            assert_eq!(at.thread_id.as_deref(), Some(SESSION));
            assert_eq!(at.worktree.as_deref(), Some(WORKSPACE));
            assert_eq!(at.machine.as_deref(), Some(machine().to_string().as_str()));
            assert_eq!(at.pr, None);
            assert!(at.stop.is_none());
        }

        #[test]
        fn the_config_follows_the_base_the_launch_recorded() {
            let rig = Rig::new();
            assert_eq!(run_config(&rig.config, None).base, "main");
            assert_eq!(run_config(&rig.config, Some("main")).base, "main");
            assert_eq!(run_config(&rig.config, Some("release/2")).base, "release/2");
            assert_eq!(rig.config.base, "main");
        }
    }

    mod finishing_a_launch {
        use super::*;

        #[test]
        fn moves_the_label_posts_the_status_and_writes_the_state_last() {
            let rig = Rig::new();
            let run = run_id(1);
            rig.step(&run, "approved x");
            rig.moved(&run, State::Ready, State::Triage);
            rig.sim.set_labels(&["factory:triage"]);
            let mut at = progress(&rig, &run, State::Triage);
            at.thread_id = Some(SESSION.into());
            finish_building(&rig.ctx(), ISSUE, &mut at).unwrap();
            assert_eq!(rig.sim.labels(), vec!["factory:building"]);
            let comments = rig.sim.comments();
            assert_eq!(comments.len(), 1);
            assert!(comments[0].contains("**Factory status:** building"));
            // A comment hides what looks like a token, so the id itself is not shown.
            assert!(!comments[0].contains("Session: none"));
            assert!(comments[0].ends_with(STATUS_MARKER));
            assert_eq!(at.state, State::Building);
            let last = rig.events(&run).pop().unwrap();
            assert_eq!(last.event, "state_changed");
            assert_eq!(last.data["to"], "building");
            assert_eq!(last.cause.unwrap()["reason"], "session-started");
        }

        #[test]
        fn from_building_is_an_invalid_transition_the_caller_guards() {
            let rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            let mut at = progress(&rig, &run, State::Triage);
            assert!(matches!(
                finish_building(&rig.ctx(), ISSUE, &mut at),
                Err(RunError::Transition(_))
            ));
        }
    }

    mod applying_actions {
        use super::*;

        fn cause() -> Cause {
            Cause::new(CauseStep::Start, "test")
        }

        #[test]
        fn a_forward_without_its_payload_is_an_invalid_transition() {
            let rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            let mut at = progress(&rig, &run, State::Building);
            let actions = [Action::ForwardComment {
                comment_id: "c1".into(),
            }];
            let result = apply_actions(
                &rig.ctx(),
                ISSUE,
                &mut at,
                &actions,
                &cause(),
                &ActionPayload::default(),
            );
            assert!(matches!(result, Err(RunError::Transition(_))));
            assert!(rig.worker.calls().is_empty());
        }

        #[test]
        fn a_forward_goes_to_the_session_and_is_recorded() {
            let mut rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            let mut at = progress(&rig, &run, State::Building);
            let message = Message {
                line: "see this".into(),
                body: None,
            };
            let payload = ActionPayload {
                forward: Some(ForwardPayload {
                    message: message.clone(),
                    step: None,
                }),
                ..ActionPayload::default()
            };
            let actions = [Action::ForwardComment {
                comment_id: "c1".into(),
            }];
            rig.view = FleetView::builder()
                .turn(foundry_proto::factory::SessionTurn {
                    session: session_id(SESSION).unwrap(),
                    workspace: workspace_id(WORKSPACE).unwrap(),
                    project: project_id(),
                    turn: 1,
                    prompts: 1,
                    question: false,
                })
                .build();
            rig.worker.on_send(Ok(1));
            apply_actions(&rig.ctx(), ISSUE, &mut at, &actions, &cause(), &payload).unwrap();
            assert_eq!(
                rig.worker.calls(),
                vec![Call::Send {
                    session: session_id(SESSION).unwrap(),
                    message,
                    only_if_prompts: Some(1),
                }]
            );
            assert!(rig.names(&run).contains(&"comment_forwarded".to_string()));
        }

        #[test]
        fn a_forward_wakes_the_runs_workspace_before_it_is_sent() {
            let mut rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            let mut at = progress(&rig, &run, State::Building);
            let payload = ActionPayload {
                forward: Some(ForwardPayload {
                    message: Message {
                        line: "see this".into(),
                        body: None,
                    },
                    step: None,
                }),
                ..ActionPayload::default()
            };
            let actions = [Action::ForwardComment {
                comment_id: "c1".into(),
            }];
            rig.view = FleetView::builder()
                .turn(foundry_proto::factory::SessionTurn {
                    session: session_id(SESSION).unwrap(),
                    workspace: workspace_id(WORKSPACE).unwrap(),
                    project: project_id(),
                    turn: 1,
                    prompts: 1,
                    question: false,
                })
                .build();
            rig.worker.on_send(Ok(1));
            apply_actions(&rig.ctx(), ISSUE, &mut at, &actions, &cause(), &payload).unwrap();
            assert_eq!(
                rig.worker.host_calls(),
                [(
                    0,
                    HostCall::Wake {
                        workspace: workspace_id(WORKSPACE).unwrap()
                    }
                )]
            );
            assert!(matches!(rig.worker.calls()[0], Call::Send { .. }));
        }

        #[test]
        fn the_gate_results_are_posted_once() {
            let rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            let mut at = progress(&rig, &run, State::Building);
            let payload = ActionPayload {
                comment: Some("gate passed".into()),
                ..ActionPayload::default()
            };
            let actions = [Action::PostGateResults { pr: 12 }];
            apply_actions(&rig.ctx(), ISSUE, &mut at, &actions, &cause(), &payload).unwrap();
            apply_actions(&rig.ctx(), ISSUE, &mut at, &actions, &cause(), &payload).unwrap();
            let posted: Vec<String> = rig
                .sim
                .writes()
                .into_iter()
                .filter(|w| w.starts_with("pr comment 12"))
                .collect();
            assert_eq!(posted.len(), 1);
            assert_eq!(
                rig.steps(&run)
                    .iter()
                    .filter(|s| *s == GATE_RESULTS_POSTED)
                    .count(),
                1
            );
        }

        /// The gate-results comment is recorded as intended before it is posted: a post whose
        /// answer was lost (the comment may be on the pull request) is looked for, not posted
        /// again.
        #[test]
        fn the_gate_results_whose_answer_was_lost_are_not_posted_twice() {
            let rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            let payload = ActionPayload {
                comment: Some("gate passed".into()),
                ..ActionPayload::default()
            };
            let actions = [Action::PostGateResults { pr: 12 }];
            rig.sim.lose("pr comment", 1);
            let mut at = progress(&rig, &run, State::Building);
            assert!(
                apply_actions(&rig.ctx(), ISSUE, &mut at, &actions, &cause(), &payload).is_err()
            );
            let steps = rig.steps(&run);
            assert!(
                steps
                    .iter()
                    .any(|s| s.starts_with("gate-results-commenting"))
            );
            assert!(!steps.contains(&GATE_RESULTS_POSTED.to_string()));
            // The comment is on the pull request, as the lost answer left it.
            rig.tracker
                .post_comment(REPO, 12, &payload.comment.clone().unwrap())
                .unwrap();
            let mut at = progress(&rig, &run, State::Building);
            apply_actions(&rig.ctx(), ISSUE, &mut at, &actions, &cause(), &payload).unwrap();
            let posted = rig
                .sim
                .writes()
                .into_iter()
                .filter(|w| w.starts_with("pr comment 12"))
                .count();
            assert_eq!(posted, 1);
            assert!(rig.steps(&run).contains(&GATE_RESULTS_POSTED.to_string()));
        }

        /// A second round's results, intended and not landed, are posted though the first
        /// round's identical comment is the newest on the pull request.
        #[test]
        fn a_second_rounds_gate_results_intended_and_not_landed_are_posted() {
            let rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            let payload = ActionPayload {
                comment: Some("gate passed".into()),
                ..ActionPayload::default()
            };
            let actions = [Action::PostGateResults { pr: 12 }];
            let mut at = progress(&rig, &run, State::Building);
            apply_actions(&rig.ctx(), ISSUE, &mut at, &actions, &cause(), &payload).unwrap();
            let id = rig
                .ctx()
                .tracker
                .issue_comments(REPO, 12)
                .unwrap()
                .iter()
                .map(|c| c.id)
                .max()
                .unwrap();
            rig.step(&run, &format!("gate-results-commenting {id}"));
            let mut at = progress(&rig, &run, State::Building);
            at.gate_results_posted = false;
            apply_actions(&rig.ctx(), ISSUE, &mut at, &actions, &cause(), &payload).unwrap();
            let posted = rig
                .sim
                .writes()
                .into_iter()
                .filter(|w| w.starts_with("pr comment 12"))
                .count();
            assert_eq!(posted, 2);
        }

        #[test]
        fn the_gate_results_intended_and_not_on_the_pull_request_are_posted() {
            let rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            rig.step(&run, "gate-results-commenting");
            let payload = ActionPayload {
                comment: Some("gate passed".into()),
                ..ActionPayload::default()
            };
            let mut at = progress(&rig, &run, State::Building);
            apply_actions(
                &rig.ctx(),
                ISSUE,
                &mut at,
                &[Action::PostGateResults { pr: 12 }],
                &cause(),
                &payload,
            )
            .unwrap();
            let posted = rig
                .sim
                .writes()
                .into_iter()
                .filter(|w| w.starts_with("pr comment 12"))
                .count();
            assert_eq!(posted, 1);
            assert!(rig.steps(&run).contains(&GATE_RESULTS_POSTED.to_string()));
        }

        /// A lookalike of the gate results by another login on the pull request, posted after
        /// the intent, does not stand in for the factory's own.
        #[test]
        fn gate_results_pasted_by_another_login_are_still_posted() {
            let rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            let payload = ActionPayload {
                comment: Some("gate passed".into()),
                ..ActionPayload::default()
            };
            let actions = [Action::PostGateResults { pr: 12 }];
            rig.sim.fail("pr comment", 1);
            let mut at = progress(&rig, &run, State::Building);
            assert!(
                apply_actions(&rig.ctx(), ISSUE, &mut at, &actions, &cause(), &payload).is_err()
            );
            rig.sim
                .comment_as("mallory", &as_posted(&rig.ctx(), "gate passed"));
            let mut at = progress(&rig, &run, State::Building);
            apply_actions(&rig.ctx(), ISSUE, &mut at, &actions, &cause(), &payload).unwrap();
            let posted = rig
                .sim
                .writes()
                .into_iter()
                .filter(|w| w.starts_with("pr comment 12"))
                .count();
            assert_eq!(posted, 2, "the failed post and the one after it");
            assert!(rig.steps(&run).contains(&GATE_RESULTS_POSTED.to_string()));
        }

        /// The intent names the newest marked comment of the login on the pull request, so a
        /// later pass looks only at comments newer than it.
        #[test]
        fn the_gate_results_intent_records_the_newest_own_comments_id() {
            let rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            rig.tracker
                .post_comment(REPO, 12, "an earlier comment")
                .unwrap();
            let id = newest_own_id(&rig.ctx(), 12).unwrap();
            assert!(id > 0);
            let payload = ActionPayload {
                comment: Some("gate passed".into()),
                ..ActionPayload::default()
            };
            let mut at = progress(&rig, &run, State::Building);
            apply_actions(
                &rig.ctx(),
                ISSUE,
                &mut at,
                &[Action::PostGateResults { pr: 12 }],
                &cause(),
                &payload,
            )
            .unwrap();
            assert!(
                rig.steps(&run)
                    .contains(&format!("{GATE_RESULTS_COMMENTING} {id}"))
            );
        }

        /// A forward sent under the session's prompt count, recorded before the send: a repeat
        /// after a lost answer goes under the same count and is delivered when the count rose.
        #[test]
        fn a_forward_repeated_after_a_lost_answer_is_not_typed_twice() {
            let mut rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            let turn = |prompts| foundry_proto::factory::SessionTurn {
                session: session_id(SESSION).unwrap(),
                workspace: workspace_id(WORKSPACE).unwrap(),
                project: project_id(),
                turn: 1,
                prompts,
                question: false,
            };
            let message = Message {
                line: "see this".into(),
                body: None,
            };
            let payload = ActionPayload {
                forward: Some(ForwardPayload {
                    message: message.clone(),
                    step: None,
                }),
                ..ActionPayload::default()
            };
            let actions = [Action::ForwardComment {
                comment_id: "c1".into(),
            }];
            rig.view = FleetView::builder().turn(turn(3)).build();
            rig.worker.on_send(Err(SendRefused::Failed {
                reason: "timed out".into(),
                prompts: Some(3),
            }));
            let mut at = progress(&rig, &run, State::Building);
            assert!(
                apply_actions(&rig.ctx(), ISSUE, &mut at, &actions, &cause(), &payload).is_err()
            );
            assert!(
                rig.steps(&run)
                    .contains(&"send-intent forward-comment c1 3".to_string())
            );
            // The worker typed it and its answer was lost: the count has risen.
            rig.view = FleetView::builder().turn(turn(4)).build();
            rig.worker.on_send(Err(SendRefused::CountRose { now: 4 }));
            let mut at = progress(&rig, &run, State::Building);
            apply_actions(&rig.ctx(), ISSUE, &mut at, &actions, &cause(), &payload).unwrap();
            let counts: Vec<Option<u64>> = rig
                .worker
                .calls()
                .into_iter()
                .filter_map(|call| match call {
                    Call::Send {
                        only_if_prompts, ..
                    } => Some(only_if_prompts),
                    _ => None,
                })
                .collect();
            assert_eq!(counts, vec![Some(3), Some(3)]);
            assert!(rig.names(&run).contains(&"comment_forwarded".to_string()));
        }

        fn forward_rig(prompts: u64) -> (Rig, RunId, ActionPayload, [Action; 1]) {
            let rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            let payload = ActionPayload {
                forward: Some(ForwardPayload {
                    message: Message {
                        line: "see this".into(),
                        body: None,
                    },
                    step: None,
                }),
                ..ActionPayload::default()
            };
            let actions = [Action::ForwardComment {
                comment_id: "c1".into(),
            }];
            let mut rig = rig;
            set_prompts(&mut rig, prompts);
            (rig, run, payload, actions)
        }

        fn set_prompts(rig: &mut Rig, prompts: u64) {
            rig.view = FleetView::builder()
                .turn(foundry_proto::factory::SessionTurn {
                    session: session_id(SESSION).unwrap(),
                    workspace: workspace_id(WORKSPACE).unwrap(),
                    project: project_id(),
                    turn: 1,
                    prompts,
                    question: false,
                })
                .build();
        }

        fn send_counts(rig: &Rig) -> Vec<Option<u64>> {
            rig.worker
                .calls()
                .into_iter()
                .filter_map(|call| match call {
                    Call::Send {
                        only_if_prompts, ..
                    } => Some(only_if_prompts),
                    _ => None,
                })
                .collect()
        }

        /// A send answered Ok whose delivery event was not written is not typed again.
        #[test]
        fn a_send_answered_ok_with_no_delivery_line_is_not_typed_again() {
            let (mut rig, run, payload, actions) = forward_rig(3);
            let at = progress(&rig, &run, State::Building);
            rig.worker.on_send(Ok(4));
            send_once(
                &rig.ctx(),
                &at,
                session_id(SESSION).unwrap(),
                payload.forward.clone().unwrap().message,
                "forward-comment c1",
            )
            .unwrap();
            set_prompts(&mut rig, 4);
            rig.worker.on_send(Err(SendRefused::CountRose { now: 4 }));
            let mut at = progress(&rig, &run, State::Building);
            apply_actions(&rig.ctx(), ISSUE, &mut at, &actions, &cause(), &payload).unwrap();
            assert!(rig.names(&run).contains(&"comment_forwarded".to_string()));
        }

        /// A first send refused with a count risen has no earlier try with an unknown outcome:
        /// nothing of it was typed by this send, so it is not delivered.
        #[test]
        fn a_first_send_refused_count_rose_is_not_delivered() {
            let (rig, run, payload, actions) = forward_rig(3);
            rig.worker.on_send(Err(SendRefused::CountRose { now: 4 }));
            let mut at = progress(&rig, &run, State::Building);
            assert!(
                apply_actions(&rig.ctx(), ISSUE, &mut at, &actions, &cause(), &payload).is_err()
            );
            assert!(!rig.names(&run).contains(&"comment_forwarded".to_string()));
            assert!(
                rig.steps(&run)
                    .contains(&"send-intent forward-comment c1 4".to_string())
            );
        }

        /// A refusal that typed nothing leaves no unknown: the next try reads the count anew
        /// and is sent under it.
        #[test]
        fn a_send_refused_not_idle_is_sent_under_the_new_count_when_retried() {
            let (mut rig, run, payload, actions) = forward_rig(3);
            rig.worker.on_send(Err(SendRefused::NotIdle));
            let mut at = progress(&rig, &run, State::Building);
            assert!(
                apply_actions(&rig.ctx(), ISSUE, &mut at, &actions, &cause(), &payload).is_err()
            );
            set_prompts(&mut rig, 4);
            rig.worker.on_send(Ok(5));
            let mut at = progress(&rig, &run, State::Building);
            apply_actions(&rig.ctx(), ISSUE, &mut at, &actions, &cause(), &payload).unwrap();
            assert_eq!(send_counts(&rig), vec![Some(3), Some(4)]);
            assert!(rig.names(&run).contains(&"comment_forwarded".to_string()));
        }

        /// A gate failure goes to the session under its prompt count, keyed by its attempt and
        /// run ordinal, and is recorded with its attempt in the cause.
        #[test]
        fn a_gate_failure_goes_to_the_session_under_its_count_and_is_recorded_with_its_attempt() {
            let (rig, run, _, _) = forward_rig(3);
            let message = Message {
                line: "the gate failed".into(),
                body: None,
            };
            let payload = ActionPayload {
                send: Some(SendPayload {
                    message: message.clone(),
                    attempt: 2,
                    run_ordinal: 3,
                }),
                ..ActionPayload::default()
            };
            let actions = [Action::SendGateFailure {
                output: "boom".into(),
            }];
            rig.worker.on_send(Ok(3));
            forward(&rig, &run, &payload, &actions).unwrap();
            assert_eq!(
                rig.worker.calls(),
                vec![Call::Send {
                    session: session_id(SESSION).unwrap(),
                    message,
                    only_if_prompts: Some(3),
                }]
            );
            assert!(
                rig.steps(&run)
                    .contains(&"send-intent gate-failure 2 3 3".to_string())
            );
            let sent = rig
                .events(&run)
                .into_iter()
                .find(|event| event.event == "gate_sent_back")
                .unwrap();
            assert_eq!(sent.data["attempt"], 2);
            assert_eq!(sent.data["run_ordinal"], 3);
            assert_eq!(sent.cause.unwrap()["attempt"], 2);
        }

        /// A body edit goes to the session under its prompt count and its
        /// `body-edit-forwarded <edited_at>` step is recorded after it.
        #[test]
        fn a_body_edit_goes_to_the_session_and_its_step_is_recorded() {
            let (rig, run, _, _) = forward_rig(3);
            let step = "body-edit-forwarded 2026-10-04T11:00:00Z";
            let payload = ActionPayload {
                forward: Some(ForwardPayload {
                    message: Message {
                        line: "the body changed".into(),
                        body: None,
                    },
                    step: Some(step.into()),
                }),
                ..ActionPayload::default()
            };
            rig.worker.on_send(Ok(3));
            forward(&rig, &run, &payload, &[Action::ForwardBodyEdit]).unwrap();
            assert_eq!(send_counts(&rig), vec![Some(3)]);
            let steps = rig.steps(&run);
            let intent = steps
                .iter()
                .position(|s| s == &format!("send-intent forward-body-edit {step} 3"))
                .unwrap();
            let recorded = steps.iter().position(|s| s == step).unwrap();
            assert!(intent < recorded, "{steps:?}");
        }

        /// A gate failure with no payload or no session, and a body edit with no step, are
        /// invalid transitions that ask nothing of the worker.
        #[test]
        fn a_send_missing_its_payload_session_or_step_is_an_invalid_transition() {
            let (rig, run, _, _) = forward_rig(3);
            let gate = [Action::SendGateFailure {
                output: "boom".into(),
            }];
            let send = ActionPayload {
                send: Some(SendPayload {
                    message: Message {
                        line: "the gate failed".into(),
                        body: None,
                    },
                    attempt: 1,
                    run_ordinal: 1,
                }),
                ..ActionPayload::default()
            };
            let edit = ActionPayload {
                forward: Some(ForwardPayload {
                    message: Message {
                        line: "the body changed".into(),
                        body: None,
                    },
                    step: None,
                }),
                ..ActionPayload::default()
            };
            assert!(matches!(
                forward(&rig, &run, &ActionPayload::default(), &gate),
                Err(RunError::Transition(_))
            ));
            assert!(matches!(
                forward(&rig, &run, &edit, &[Action::ForwardBodyEdit]),
                Err(RunError::Transition(_))
            ));
            let mut at = progress(&rig, &run, State::Building);
            at.thread_id = None;
            assert!(matches!(
                apply_actions(&rig.ctx(), ISSUE, &mut at, &gate, &cause(), &send),
                Err(RunError::Transition(_))
            ));
            assert!(rig.worker.calls().is_empty());
        }

        fn forward(
            rig: &Rig,
            run: &RunId,
            payload: &ActionPayload,
            actions: &[Action],
        ) -> Result<(), RunError> {
            let mut at = progress(rig, run, State::Building);
            apply_actions(&rig.ctx(), ISSUE, &mut at, actions, &cause(), payload)
        }

        /// A try that may have typed keeps its count through a refused repeat: the session is
        /// busy with the line it took, so its count rose, and a try under the risen count would
        /// type the line a second time.
        #[test]
        fn a_refused_repeat_of_a_send_that_may_have_typed_keeps_its_count() {
            let (mut rig, run, payload, actions) = forward_rig(3);
            rig.worker.on_send(Err(SendRefused::Failed {
                reason: "timed out".into(),
                prompts: Some(3),
            }));
            assert!(forward(&rig, &run, &payload, &actions).is_err());
            set_prompts(&mut rig, 4);
            rig.worker.on_send(Err(SendRefused::NotIdle));
            assert!(forward(&rig, &run, &payload, &actions).is_err());
            rig.worker.on_send(Err(SendRefused::CountRose { now: 4 }));
            forward(&rig, &run, &payload, &actions).unwrap();
            assert_eq!(send_counts(&rig), vec![Some(3), Some(3), Some(3)]);
            assert!(rig.names(&run).contains(&"comment_forwarded".to_string()));
        }

        /// A send stopped by a lost lease asked nothing of the worker and records no try: the
        /// next try reads the count anew, so a count another prompt raised is not taken as this
        /// message typed.
        #[test]
        fn a_send_stopped_by_a_lost_lease_records_no_try() {
            let (mut rig, run, payload, actions) = forward_rig(3);
            rig.guard = Box::new(Lost);
            assert!(matches!(
                forward(&rig, &run, &payload, &actions),
                Err(RunError::LeaseLost(_))
            ));
            assert!(rig.worker.calls().is_empty());
            assert!(
                !rig.steps(&run)
                    .iter()
                    .any(|step| step.starts_with(SEND_INTENT)),
                "{:?}",
                rig.steps(&run)
            );
            rig.guard = Box::new(crate::factory::worker::AlwaysHeld(1));
            set_prompts(&mut rig, 4);
            rig.worker.on_send(Ok(4));
            forward(&rig, &run, &payload, &actions).unwrap();
            assert_eq!(send_counts(&rig), vec![Some(4)]);
        }

        /// A send with no count to go under (the fleet shows no turn for the session) waits:
        /// nothing is written and nothing is asked of the worker, and the next pass that shows
        /// a turn sends under its count.
        #[test]
        fn a_send_to_a_session_that_shows_no_turn_waits_for_one() {
            let (mut rig, run, payload, actions) = forward_rig(3);
            rig.view = FleetView::builder().build();
            assert!(matches!(
                forward(&rig, &run, &payload, &actions),
                Err(RunError::NoPromptCount { .. })
            ));
            assert!(rig.worker.calls().is_empty());
            assert!(
                !rig.steps(&run)
                    .iter()
                    .any(|step| step.starts_with(SEND_INTENT)),
                "{:?}",
                rig.steps(&run)
            );
            set_prompts(&mut rig, 5);
            rig.worker.on_send(Ok(5));
            forward(&rig, &run, &payload, &actions).unwrap();
            assert_eq!(send_counts(&rig), vec![Some(5)]);
        }

        fn gate_failure() -> (ActionPayload, [Action; 1]) {
            (
                ActionPayload {
                    send: Some(SendPayload {
                        message: Message {
                            line: "the gate failed".into(),
                            body: None,
                        },
                        attempt: 1,
                        run_ordinal: 1,
                    }),
                    ..ActionPayload::default()
                },
                [Action::SendGateFailure {
                    output: "boom".into(),
                }],
            )
        }

        /// One send to a session at a time: while a forward may have typed, a gate failure
        /// waits for it, naming it, with nothing asked of the worker; so the count the gate
        /// failure would raise cannot be read as the forward typed.
        #[test]
        fn a_send_waits_while_another_to_the_session_may_have_typed() {
            let (mut rig, run, payload, actions) = forward_rig(3);
            rig.worker.on_send(Err(SendRefused::Failed {
                reason: "timed out".into(),
                prompts: Some(3),
            }));
            assert!(forward(&rig, &run, &payload, &actions).is_err());
            let (gate, gate_actions) = gate_failure();
            match forward(&rig, &run, &gate, &gate_actions) {
                Err(RunError::SendPending { key }) => assert_eq!(key, "forward-comment c1"),
                other => panic!("{other:?}"),
            }
            assert_eq!(send_counts(&rig), vec![Some(3)]);
            // The forward had not typed: tried again under its count, it is typed now.
            rig.worker.on_send(Ok(3));
            forward(&rig, &run, &payload, &actions).unwrap();
            assert!(rig.names(&run).contains(&"comment_forwarded".to_string()));
            set_prompts(&mut rig, 4);
            rig.worker.on_send(Ok(4));
            forward(&rig, &run, &gate, &gate_actions).unwrap();
            assert_eq!(send_counts(&rig), vec![Some(3), Some(3), Some(4)]);
            assert!(rig.names(&run).contains(&"gate_sent_back".to_string()));
        }

        /// A refusal that typed nothing leaves nothing pending: another send goes ahead.
        #[test]
        fn a_refused_send_holds_back_no_other() {
            let (rig, run, payload, actions) = forward_rig(3);
            rig.worker.on_send(Err(SendRefused::NotIdle));
            assert!(forward(&rig, &run, &payload, &actions).is_err());
            let (gate, gate_actions) = gate_failure();
            rig.worker.on_send(Ok(3));
            forward(&rig, &run, &gate, &gate_actions).unwrap();
            assert_eq!(send_counts(&rig), vec![Some(3), Some(3)]);
        }

        /// A send known typed is not typed again, though the line that records its delivery
        /// was not written.
        #[test]
        fn a_send_known_typed_is_not_sent_again() {
            let (rig, run, payload, _) = forward_rig(3);
            let message = payload.forward.clone().unwrap().message;
            let at = progress(&rig, &run, State::Building);
            rig.worker.on_send(Ok(3));
            send_once(
                &rig.ctx(),
                &at,
                session_id(SESSION).unwrap(),
                message.clone(),
                "forward-comment c1",
            )
            .unwrap();
            let at = progress(&rig, &run, State::Building);
            send_once(
                &rig.ctx(),
                &at,
                session_id(SESSION).unwrap(),
                message,
                "forward-comment c1",
            )
            .unwrap();
            assert_eq!(send_counts(&rig), vec![Some(3)]);
        }

        #[test]
        fn a_label_that_does_not_move_the_state_writes_no_state_changed() {
            let rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            let before = rig.names(&run).len();
            let mut at = progress(&rig, &run, State::Building);
            apply_actions(
                &rig.ctx(),
                ISSUE,
                &mut at,
                &[Action::SetLabel {
                    state: State::Building,
                }],
                &cause(),
                &ActionPayload::default(),
            )
            .unwrap();
            assert_eq!(rig.names(&run).len(), before);
        }

        #[test]
        fn an_action_the_runner_does_not_apply_is_an_invalid_transition() {
            let rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            let mut at = progress(&rig, &run, State::Building);
            let actions = [Action::PostUnapproved {
                refusal: crate::factory::lifecycle::approval::Refusal::LabelNotFound,
            }];
            assert!(matches!(
                apply_actions(
                    &rig.ctx(),
                    ISSUE,
                    &mut at,
                    &actions,
                    &cause(),
                    &ActionPayload::default()
                ),
                Err(RunError::Transition(_))
            ));
        }
    }

    mod the_send_helpers {
        use super::*;

        fn sends(tries: &[(&str, SendTries)]) -> Seen {
            Seen {
                sends: tries
                    .iter()
                    .map(|(key, sent)| ((*key).to_owned(), sent.clone()))
                    .collect(),
                ..Seen::default()
            }
        }

        fn may_have_typed() -> SendTries {
            SendTries {
                count: 3,
                tries: 1,
                refused: 0,
                typed: false,
            }
        }

        #[test]
        fn pending_send_names_another_key_that_may_have_typed() {
            let seen = sends(&[("a", may_have_typed()), ("b", may_have_typed())]);
            assert_eq!(pending_send(&seen, "b"), Some("a"));
            assert_eq!(pending_send(&seen, "c"), Some("a"));
        }

        #[test]
        fn pending_send_does_not_name_the_key_asked_about() {
            let seen = sends(&[("a", may_have_typed())]);
            assert_eq!(pending_send(&seen, "a"), None);
        }

        #[test]
        fn pending_send_does_not_name_a_typed_or_refused_key() {
            let typed = SendTries {
                typed: true,
                ..may_have_typed()
            };
            let refused = SendTries {
                refused: 1,
                ..may_have_typed()
            };
            let seen = sends(&[("a", typed), ("b", refused)]);
            assert_eq!(pending_send(&seen, "c"), None);
            assert_eq!(pending_send(&Seen::default(), "c"), None);
        }

        #[test]
        fn record_send_typed_writes_the_step_with_its_cause() {
            let rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            record_send_typed(&rig.ctx(), &run, CauseStep::Forward, "forward-comment c1").unwrap();
            assert_eq!(
                rig.steps(&run).last().map(String::as_str),
                Some("send-typed forward-comment c1")
            );
            let last = rig.events(&run).pop().unwrap();
            let cause = last.cause.unwrap();
            assert_eq!(cause["step"], "forward");
            assert_eq!(cause["reason"], "send-typed");
            assert!(
                rig.ctx().seen(&run).unwrap().sends.is_empty(),
                "a typed step for a key never tried creates no entry"
            );
        }
    }

    mod stopping_a_session {
        use super::*;

        #[test]
        fn ends_the_sessions_of_the_build_workspace_and_keeps_it() {
            let rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            stop_session(&rig.ctx(), &progress(&rig, &run, State::Building)).unwrap();
            assert_eq!(
                rig.worker.calls(),
                vec![Call::EndSessions {
                    workspace: workspace_id(WORKSPACE).unwrap()
                }]
            );
        }

        #[test]
        fn has_nothing_to_stop_for_a_run_that_launched_nothing() {
            let rig = Rig::new();
            let run = run_id(1);
            rig.step(&run, "approved x");
            stop_session(&rig.ctx(), &progress(&rig, &run, State::Triage)).unwrap();
            assert!(rig.worker.calls().is_empty());
        }

        #[test]
        fn a_launch_that_was_sent_may_have_a_session_in_the_workspace_so_it_is_stopped() {
            let rig = Rig::new();
            let run = run_id(1);
            rig.step(&run, "approved x");
            rig.step(&run, &format!("workspace-created {WORKSPACE}"));
            rig.step(&run, "launch-sending");
            stop_session(&rig.ctx(), &progress(&rig, &run, State::Triage)).unwrap();
            assert_eq!(rig.worker.calls().len(), 1);
        }

        #[test]
        fn a_lost_lease_calls_nothing() {
            let mut rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            rig.guard = Box::new(Lost);
            let result = stop_session(&rig.ctx(), &progress(&rig, &run, State::Building));
            assert!(matches!(result, Err(RunError::LeaseLost(_))));
            assert!(rig.worker.calls().is_empty());
        }
    }

    /// Every method of the context that changes GitHub or calls a worker checks the lease first:
    /// with it lost, each is an error, and nothing reached GitHub or the worker. Taking any one
    /// check out of the context fails this.
    #[test]
    fn a_lost_lease_stops_every_change_on_github_and_every_call_to_a_worker() {
        use crate::workspaces::{Checkout, FactoryTag, Setup};
        let mut rig = Rig::new();
        rig.guard = Box::new(Lost);
        let ctx = rig.ctx();
        let workspace = WorkspaceId(uuid::Uuid::from_u128(0xa11));
        let session = SessionId(uuid::Uuid::from_u128(0x5e5));
        let terminal = foundry_proto::ids::TerminalId(uuid::Uuid::from_u128(0x7e));
        let message = Message::new("line".into(), "body".into());
        let request = crate::factory::worker::CreateWorkspace {
            project: project_id(),
            name: "fac/8-1".into(),
            checkout: Checkout::NewBranch {
                base: "main".into(),
                branch: None,
            },
            setup: Setup::Own,
            tag: FactoryTag {
                project: project_id(),
                run: "run".into(),
            },
        };
        let start = crate::sessions::factory::FactoryStart {
            project: project_id(),
            login: LOGIN.into(),
            model: "m".into(),
            effort: "e".into(),
        };
        let results: Vec<(&str, Result<(), RunError>)> = vec![
            ("set_state", ctx.set_state(ISSUE, State::Triage)),
            ("assign", ctx.assign(ISSUE)),
            ("unassign", ctx.unassign(ISSUE)),
            ("post_comment", ctx.post_comment(ISSUE, "x")),
            ("upsert_status", ctx.upsert_status(ISSUE, "x")),
            ("comment_on_pr", ctx.comment_on_pr(1, "x")),
            ("mark_pr_ready", ctx.mark_pr_ready(1)),
            ("mark_pr_draft", ctx.mark_pr_draft(1)),
            (
                "create",
                ctx.create(Target::Machine(machine()), request).map(|_| ()),
            ),
            (
                "start_session",
                ctx.start_session(workspace, start, message.clone())
                    .map(|_| ()),
            ),
            ("tail", ctx.tail(terminal).map(|_| ())),
            ("send", ctx.send(session, message, None).map(|_| ())),
            ("end_sessions", ctx.end_sessions(workspace)),
            ("archive", ctx.archive(workspace)),
            ("advance", ctx.advance(workspace)),
            ("rest", ctx.rest(workspace)),
            ("wake", ctx.wake(workspace)),
            ("keep", ctx.keep(workspace)),
        ];
        for (name, result) in results {
            assert!(matches!(result, Err(RunError::LeaseLost(_))), "{name}");
        }
        assert!(rig.sim.calls().is_empty());
        assert!(rig.worker.calls().is_empty());
        assert!(rig.worker.host_calls().is_empty());
    }

    mod going_stuck {
        use super::*;

        /// The stuck comment is posted after its intent is recorded: a post whose answer was
        /// lost (the comment may be on the issue) is not posted twice.
        #[test]
        fn the_stuck_comment_is_recorded_as_intended_before_it_is_posted() {
            let rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            let mut at = progress(&rig, &run, State::Building);
            go_stuck(&rig.ctx(), ISSUE, &mut at, "c", stuck_stop(), None).unwrap();
            let steps = rig.steps(&run);
            assert!(
                steps.iter().any(|s| s.starts_with("stuck-commenting")),
                "{steps:?}"
            );
            let names = rig.names(&run);
            let intent = names.iter().rposition(|n| n == "step_changed").unwrap();
            let stuck = names.iter().position(|n| n == "stuck").unwrap();
            assert!(intent < stuck);
        }

        #[test]
        fn a_stuck_comment_intended_and_already_the_newest_is_not_posted_again() {
            let rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            rig.step(&run, "stuck-commenting");
            let said = messages::stuck(&stuck_stop().reason, None, None);
            rig.tracker.post_comment(REPO, ISSUE, &said).unwrap();
            let before = rig.sim.comments().len();
            let mut at = progress(&rig, &run, State::Building);
            go_stuck(&rig.ctx(), ISSUE, &mut at, "c", stuck_stop(), None).unwrap();
            let posted = rig
                .sim
                .comments()
                .iter()
                .filter(|c| c.contains("Reason: the session said stuck"))
                .count();
            assert_eq!(posted, 1);
            assert!(
                rig.sim.comments().len() <= before + 1,
                "only the status comment"
            );
            assert!(rig.names(&run).contains(&"stuck".to_string()));
            assert!(rig.finished(&run));
        }

        /// The recorded decision holds the short fields only: a setup tail, which the ledger's
        /// hiding cannot see through a JSON escape, is in no step.
        #[test]
        fn the_stuck_decision_step_holds_no_session_or_setup_text() {
            let rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            let mut at = progress(&rig, &run, State::Building);
            let stop = Stop {
                session_text: Some("ghp_abcdef\nghijkl".into()),
                recorded_text: Some("ghp_abcdef\nghijkl".into()),
                detail: Some("ghp_abcdef\nghijkl".into()),
                ..stuck_stop()
            };
            go_stuck(&rig.ctx(), ISSUE, &mut at, "c", stop, None).unwrap();
            let steps = rig.steps(&run);
            let decision = steps
                .iter()
                .find(|s| s.starts_with(STUCK_STOP))
                .unwrap_or_else(|| panic!("{steps:?}"));
            assert!(
                !decision.contains("ghp_abc") && !decision.contains("ghijkl"),
                "{decision}"
            );
        }

        /// The detail of a stop is kept for a later pass as a plain step, written before the
        /// decision: the ledger hides it as it hides the `stuck` line, which holds the same
        /// detail, a secret wrapped across lines included.
        #[test]
        fn the_stuck_detail_is_a_plain_step_hidden_as_the_stuck_line_holds_it() {
            let mut rig = Rig::new();
            use crate::factory::ledger::testing::{T0, hiding, open_store, ticking};
            let (dir, store) = open_store();
            rig.ledger = Ledger::new(
                store,
                MachineId(uuid::Uuid::from_u128(0x1)),
                1,
                hiding("ghp_abcdefghijkl"),
            )
            .with_wall(ticking(T0, 1000));
            rig._dir = dir;
            let run = run_id(1);
            building(&rig, &run);
            let mut at = progress(&rig, &run, State::Building);
            let stop = Stop {
                detail: Some("the start was refused: ghp_abcdef\nghijkl".into()),
                ..stuck_stop()
            };
            go_stuck(&rig.ctx(), ISSUE, &mut at, "c", stop, None).unwrap();
            let steps = rig.steps(&run);
            let detail = steps
                .iter()
                .position(|s| s.starts_with(STUCK_DETAIL))
                .unwrap_or_else(|| panic!("{steps:?}"));
            let decision = steps
                .iter()
                .position(|s| s.starts_with(STUCK_STOP))
                .unwrap();
            assert!(detail < decision, "{steps:?}");
            let stuck = rig
                .events(&run)
                .into_iter()
                .find(|e| e.event == "stuck")
                .unwrap();
            let reason = stuck.data["reason"].as_str().unwrap();
            assert_eq!(steps[detail], format!("{STUCK_DETAIL}{reason}"));
            assert!(
                !reason.contains("ghp_abc") && !reason.contains("ghijkl"),
                "{reason}"
            );
            assert!(reason.starts_with("the start was refused: "), "{reason}");
        }

        /// An earlier run's stuck comment is not this run's: the intent records the newest
        /// comment's id and only a newer one counts.
        #[test]
        fn an_earlier_runs_stuck_comment_is_not_taken_as_this_runs() {
            let rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            let said = messages::stuck(&stuck_stop().reason, None, None);
            rig.tracker.post_comment(REPO, ISSUE, &said).unwrap();
            let id = rig
                .ctx()
                .tracker
                .issue_comments(REPO, ISSUE)
                .unwrap()
                .iter()
                .map(|c| c.id)
                .max()
                .unwrap();
            rig.step(&run, &format!("stuck-commenting {id}"));
            let mut at = progress(&rig, &run, State::Building);
            go_stuck(&rig.ctx(), ISSUE, &mut at, "c", stuck_stop(), None).unwrap();
            let posted = rig
                .sim
                .comments()
                .iter()
                .filter(|c| c.contains("Reason: the session said stuck"))
                .count();
            assert_eq!(posted, 2);
        }

        #[test]
        fn a_stuck_comment_intended_and_not_on_the_issue_is_posted() {
            let rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            rig.step(&run, "stuck-commenting");
            let mut at = progress(&rig, &run, State::Building);
            go_stuck(&rig.ctx(), ISSUE, &mut at, "c", stuck_stop(), None).unwrap();
            let posted = rig
                .sim
                .comments()
                .iter()
                .filter(|c| c.contains("Reason: the session said stuck"))
                .count();
            assert_eq!(posted, 1);
        }

        #[test]
        fn the_questions_comment_intended_and_already_the_newest_is_not_posted_again() {
            let rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            rig.step(&run, "questions-commenting");
            rig.tracker
                .post_comment(REPO, ISSUE, &messages::questions("which one?"))
                .unwrap();
            let mut at = progress(&rig, &run, State::Building);
            apply_actions(
                &rig.ctx(),
                ISSUE,
                &mut at,
                &[Action::PostQuestions {
                    questions: "which one?".into(),
                }],
                &Cause::new(CauseStep::Start, "t"),
                &ActionPayload::default(),
            )
            .unwrap();
            let posted = rig
                .sim
                .comments()
                .iter()
                .filter(|c| c.contains("which one?"))
                .count();
            assert_eq!(posted, 1);
            assert!(rig.names(&run).contains(&"questions_posted".to_string()));
        }

        fn ask(rig: &Rig, run: &RunId) -> Result<(), RunError> {
            let mut at = progress(rig, run, State::Building);
            apply_actions(
                &rig.ctx(),
                ISSUE,
                &mut at,
                &[Action::PostQuestions {
                    questions: "which one?".into(),
                }],
                &Cause::new(CauseStep::Start, "t"),
                &ActionPayload::default(),
            )
        }

        /// An earlier stop's identical questions are not this stop's: the intent records the
        /// newest comment's id, and a post that failed is made on the next pass.
        #[test]
        fn an_earlier_identical_questions_comment_is_not_taken_as_this_ones() {
            let rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            rig.tracker
                .post_comment(REPO, ISSUE, &messages::questions("which one?"))
                .unwrap();
            rig.sim.fail("issue comment", 1);
            assert!(ask(&rig, &run).is_err());
            ask(&rig, &run).unwrap();
            let posted = rig
                .sim
                .comments()
                .iter()
                .filter(|c| c.contains("which one?"))
                .count();
            assert_eq!(posted, 2);
            assert!(rig.names(&run).contains(&"questions_posted".to_string()));
        }

        /// Anyone can paste the factory's marker: a lookalike of the stuck or questions comment
        /// by another login, posted after the intent, does not stand in for the factory's own.
        #[test]
        fn a_lookalike_by_another_login_does_not_stand_in_for_the_factorys_comment() {
            let rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            let questions = messages::questions("which one?");
            rig.sim.fail("issue comment", 1);
            assert!(ask(&rig, &run).is_err());
            rig.sim
                .comment_as("mallory", &as_posted(&rig.ctx(), &questions));
            ask(&rig, &run).unwrap();
            let asked = rig
                .sim
                .comments()
                .iter()
                .filter(|c| c.contains("which one?"))
                .count();
            assert_eq!(asked, 2, "mallory's and the factory's");
            let stuck = messages::stuck(&stuck_stop().reason, None, None);
            rig.sim.fail("issue comment", 1);
            let mut at = progress(&rig, &run, State::Building);
            assert!(go_stuck(&rig.ctx(), ISSUE, &mut at, "c", stuck_stop(), None).is_err());
            rig.sim
                .comment_as("mallory", &as_posted(&rig.ctx(), &stuck));
            let mut at = progress(&rig, &run, State::Building);
            go_stuck(&rig.ctx(), ISSUE, &mut at, "c", stuck_stop(), None).unwrap();
            let posted = rig
                .sim
                .comments()
                .iter()
                .filter(|c| c.contains("Reason: the session said stuck"))
                .count();
            assert_eq!(posted, 2, "mallory's and the factory's");
        }

        /// The stuck and questions intents name the newest marked comment of the login on the
        /// issue, so a later pass looks only at comments newer than it.
        #[test]
        fn the_stuck_and_questions_intents_record_the_newest_own_comments_id() {
            let rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            rig.tracker
                .post_comment(REPO, ISSUE, "an earlier comment")
                .unwrap();
            let id = newest_own_id(&rig.ctx(), ISSUE).unwrap();
            assert!(id > 0);
            ask(&rig, &run).unwrap();
            assert!(
                rig.steps(&run)
                    .contains(&format!("{QUESTIONS_COMMENTING} {id}")),
                "{:?}",
                rig.steps(&run)
            );
            let newest = newest_own_id(&rig.ctx(), ISSUE).unwrap();
            assert!(newest > id);
            let mut at = progress(&rig, &run, State::Building);
            go_stuck(&rig.ctx(), ISSUE, &mut at, "c", stuck_stop(), None).unwrap();
            assert!(
                rig.steps(&run)
                    .contains(&format!("{STUCK_COMMENTING} {newest}")),
                "{:?}",
                rig.steps(&run)
            );
        }

        #[test]
        fn the_questions_comment_is_recorded_as_intended_before_it_is_posted() {
            let rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            let mut at = progress(&rig, &run, State::Building);
            apply_actions(
                &rig.ctx(),
                ISSUE,
                &mut at,
                &[Action::PostQuestions {
                    questions: "which one?".into(),
                }],
                &Cause::new(CauseStep::Start, "t"),
                &ActionPayload::default(),
            )
            .unwrap();
            assert!(
                rig.steps(&run)
                    .iter()
                    .any(|step| step.starts_with("questions-commenting ")),
                "{:?}",
                rig.steps(&run)
            );
        }

        #[test]
        fn ends_the_session_posts_the_reason_labels_releases_and_finishes() {
            let rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            let mut at = progress(&rig, &run, State::Building);
            go_stuck(&rig.ctx(), ISSUE, &mut at, "test-stuck", stuck_stop(), None).unwrap();
            assert_eq!(rig.worker.calls().len(), 1);
            assert_eq!(rig.sim.labels(), vec!["factory:stuck"]);
            assert!(rig.sim.assignees().is_empty());
            let comments = rig.sim.comments();
            assert!(comments[0].contains("Reason: the session said stuck"));
            assert!(comments[1].contains("**Factory status:** stuck"));
            let names = rig.names(&run);
            assert_eq!(
                names[names.len() - 3..],
                ["stuck", "state_changed", "finished"]
            );
            let finished = rig.events(&run).pop().unwrap();
            assert_eq!(finished.data["outcome"], "stuck");
            assert_eq!(finished.cause.unwrap()["reason"], "test-stuck");
            assert_eq!(at.state, State::Stuck);
        }

        #[test]
        fn a_session_that_cannot_be_ended_writes_nothing() {
            let rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            rig.worker
                .on_end_sessions(Err(WorkerError::Unreachable("gone".into())));
            let before = rig.names(&run);
            let mut at = progress(&rig, &run, State::Building);
            let result = go_stuck(&rig.ctx(), ISSUE, &mut at, "c", stuck_stop(), None);
            assert!(matches!(result, Err(RunError::Worker(_))));
            assert!(rig.sim.writes().is_empty());
            assert_eq!(rig.names(&run), before);
        }

        #[test]
        fn a_lost_lease_changes_nothing() {
            let mut rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            rig.guard = Box::new(Lost);
            let before = rig.names(&run);
            let mut at = progress(&rig, &run, State::Building);
            let result = go_stuck(&rig.ctx(), ISSUE, &mut at, "c", stuck_stop(), None);
            assert!(matches!(result, Err(RunError::LeaseLost(_))));
            assert!(rig.worker.calls().is_empty());
            assert!(rig.sim.writes().is_empty());
            assert_eq!(rig.names(&run), before);
        }

        #[test]
        fn the_stuck_comment_the_ledger_already_records_is_not_posted_again() {
            let rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            rig.write(
                &run,
                Event::Stuck {
                    reason: "earlier".into(),
                    branch: None,
                    commits_ahead: None,
                },
            );
            let mut at = progress(&rig, &run, State::Building);
            go_stuck(&rig.ctx(), ISSUE, &mut at, "c", stuck_stop(), None).unwrap();
            assert!(
                rig.sim
                    .comments()
                    .iter()
                    .all(|c| !c.contains("could not finish"))
            );
            assert_eq!(rig.sim.labels(), vec!["factory:stuck"]);
        }

        #[test]
        fn a_run_going_stuck_keeps_its_build_workspace_once_its_session_is_stopped() {
            let rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            let mut at = progress(&rig, &run, State::Building);
            go_stuck(&rig.ctx(), ISSUE, &mut at, "c", stuck_stop(), None).unwrap();
            let workspace = workspace_id(WORKSPACE).unwrap();
            let kept = rig.worker.host_calls();
            assert_eq!(kept.len(), 1, "{kept:?}");
            let (before, call) = kept[0];
            assert_eq!(call, HostCall::Keep { workspace });
            assert!(
                rig.worker.calls()[..before]
                    .iter()
                    .any(|call| matches!(call, Call::EndSessions { .. })),
                "the session is stopped before the workspace is kept"
            );
        }

        #[test]
        fn a_run_with_no_workspace_keeps_nothing_when_it_goes_stuck() {
            let rig = Rig::new();
            let run = run_id(1);
            rig.step(&run, "approved x");
            let mut at = progress(&rig, &run, State::Triage);
            assert_eq!(at.worktree, None);
            go_stuck(&rig.ctx(), ISSUE, &mut at, "c", stuck_stop(), None).unwrap();
            assert!(rig.worker.host_calls().is_empty());
        }

        #[test]
        fn a_run_already_stuck_is_not_kept_again() {
            let rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            rig.moved(&run, State::Building, State::Stuck);
            let mut at = progress(&rig, &run, State::Building);
            go_stuck(&rig.ctx(), ISSUE, &mut at, "c", stuck_stop(), None).unwrap();
            assert!(rig.worker.host_calls().is_empty());
        }

        #[test]
        fn a_run_already_stuck_only_finishes() {
            let rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            rig.moved(&run, State::Building, State::Stuck);
            let mut at = progress(&rig, &run, State::Building);
            go_stuck(&rig.ctx(), ISSUE, &mut at, "c", stuck_stop(), None).unwrap();
            assert!(rig.sim.writes().is_empty());
            assert!(rig.worker.calls().is_empty());
            assert!(rig.finished(&run));
        }

        #[test]
        fn a_ready_pull_request_goes_back_to_draft_before_anything_is_posted() {
            let rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            rig.write(
                &run,
                Event::PrOpened {
                    pr: 12,
                    url: format!("https://github.com/{REPO}/pull/12"),
                },
            );
            rig.moved(&run, State::Building, State::PrOpen);
            let mut at = progress(&rig, &run, State::Building);
            go_stuck(&rig.ctx(), ISSUE, &mut at, "c", stuck_stop(), None).unwrap();
            let writes = rig.sim.writes();
            assert!(writes[0].starts_with("pr ready 12"));
            assert!(writes[0].ends_with("--undo"));
            assert!(writes[1].starts_with("issue comment"));
        }

        #[test]
        fn the_work_left_names_the_branch_and_the_base() {
            let rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            let mut at = progress(&rig, &run, State::Building);
            let stop = Stop {
                branch: Some("fac/8-1".into()),
                commits_ahead: Some(2),
                ..stuck_stop()
            };
            go_stuck(&rig.ctx(), ISSUE, &mut at, "c", stop, None).unwrap();
            assert!(rig.sim.comments()[0].contains("2 commits ahead of `main`"));
            let stuck = rig
                .events(&run)
                .into_iter()
                .find(|e| e.event == "stuck")
                .unwrap();
            assert_eq!(stuck.data["branch"], "fac/8-1");
            assert_eq!(stuck.data["commits_ahead"], 2);
        }

        #[test]
        fn what_runs_after_the_stop_runs_before_the_label_moves() {
            let rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            let mut at = progress(&rig, &run, State::Building);
            let failing = || Err(RunError::Invalid("after the stop".into()));
            let result = go_stuck(
                &rig.ctx(),
                ISSUE,
                &mut at,
                "c",
                stuck_stop(),
                Some(&failing),
            );
            assert!(matches!(result, Err(RunError::Invalid(_))));
            assert_eq!(rig.worker.calls().len(), 1);
            assert!(rig.sim.writes().is_empty());
        }

        #[test]
        fn an_unlaunched_run_in_triage_goes_stuck_with_no_session_to_end() {
            let rig = Rig::new();
            let run = run_id(1);
            rig.step(&run, "approved x");
            rig.moved(&run, State::Ready, State::Triage);
            rig.sim.set_labels(&["factory:triage"]);
            rig.sim.set_assignees(&[LOGIN]);
            let mut at = progress(&rig, &run, State::Triage);
            go_stuck(&rig.ctx(), ISSUE, &mut at, "c", stuck_stop(), None).unwrap();
            assert!(rig.worker.calls().is_empty());
            assert_eq!(rig.sim.labels(), vec!["factory:stuck"]);
        }
    }

    mod a_run_that_ends_without_a_gate {
        use super::*;

        fn pull(state: &'static str) -> PullRequest {
            PullRequest {
                number: 12,
                is_draft: false,
                open: state == "OPEN",
                merged: state == "MERGED",
                head_ref: "fac/8-1".into(),
                cross_repository: false,
                base_ref: "main".into(),
                head_sha: SHA.into(),
                mergeable: crate::factory::github::Mergeable::Mergeable,
                created_at: LABELED_AT.into(),
            }
        }

        #[test]
        fn a_merged_pull_request_ends_it_as_merged_after_the_session_is_stopped() {
            let rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            let at = progress(&rig, &run, State::Building);
            end_run(
                &rig.ctx(),
                &at,
                "https://github.com/rawphp/softfac/pull/12",
                Some(&pull("MERGED")),
                CauseStep::Watch,
            )
            .unwrap();
            assert_eq!(rig.worker.calls().len(), 1);
            let finished = rig.events(&run).pop().unwrap();
            assert_eq!(finished.data["outcome"], "pr-open");
            assert_eq!(finished.data["pr_end"], "merged");
            assert_eq!(
                finished.data["pr_url"],
                "https://github.com/rawphp/softfac/pull/12"
            );
            assert_eq!(finished.cause.unwrap()["step"], "watch");
            assert!(rig.sim.writes().is_empty());
        }

        #[test]
        fn a_closed_or_gone_one_ends_it_as_closed() {
            let rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            let at = progress(&rig, &run, State::Building);
            end_run(
                &rig.ctx(),
                &at,
                "url",
                Some(&pull("CLOSED")),
                CauseStep::Watch,
            )
            .unwrap();
            assert_eq!(rig.events(&run).pop().unwrap().data["pr_end"], "closed");
            let other = run_id(2);
            building(&rig, &other);
            let at = progress(&rig, &other, State::Building);
            end_run(&rig.ctx(), &at, "url", None, CauseStep::Watch).unwrap();
            assert_eq!(rig.events(&other).pop().unwrap().data["pr_end"], "closed");
        }

        #[test]
        fn an_own_merge_names_the_url_the_session_wrote_else_the_one_github_gives() {
            let rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            rig.write(
                &run,
                Event::PrOpened {
                    pr: 12,
                    url: "https://example.test/the-sessions-link".into(),
                },
            );
            let seen = rig.ctx().seen(&run).unwrap();
            end_own_merge(&rig.ctx(), &run, &seen, CauseStep::Start, &pull("MERGED")).unwrap();
            assert_eq!(
                rig.events(&run).pop().unwrap().data["pr_url"],
                "https://example.test/the-sessions-link"
            );
            let other = run_id(2);
            building(&rig, &other);
            let seen = rig.ctx().seen(&other).unwrap();
            end_own_merge(&rig.ctx(), &other, &seen, CauseStep::Start, &pull("MERGED")).unwrap();
            assert_eq!(
                rig.events(&other).pop().unwrap().data["pr_url"],
                "https://github.com/rawphp/softfac/pull/12"
            );
        }

        #[test]
        fn a_closed_issue_whose_run_a_person_stopped_is_abandoned_with_its_session_ended() {
            let rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            let seen = rig.ctx().seen(&run).unwrap();
            let may_exist = abandon(&rig.ctx(), &run, &seen, CauseStep::Start).unwrap();
            assert!(!may_exist);
            assert_eq!(rig.worker.calls().len(), 1);
            let finished = rig.events(&run).pop().unwrap();
            assert_eq!(finished.data["outcome"], "stuck");
            assert_eq!(finished.cause.as_ref().unwrap()["reason"], "issue-closed");
            assert!(finished.cause.unwrap().get("actor").is_none());
            assert!(rig.sim.writes().is_empty());
        }

        #[test]
        fn a_run_left_in_needs_info_is_abandoned_as_needs_info() {
            let rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            rig.moved(&run, State::Building, State::NeedsInfo);
            let seen = rig.ctx().seen(&run).unwrap();
            abandon(&rig.ctx(), &run, &seen, CauseStep::Start).unwrap();
            assert_eq!(
                rig.events(&run).pop().unwrap().data["outcome"],
                "needs-info"
            );
        }

        #[test]
        fn a_launch_sent_with_no_session_recorded_says_a_session_may_exist() {
            let rig = Rig::new();
            let run = run_id(1);
            rig.step(&run, "approved x");
            rig.step(&run, &format!("workspace-created {WORKSPACE}"));
            rig.step(&run, "launch-sending");
            let seen = rig.ctx().seen(&run).unwrap();
            assert!(abandon(&rig.ctx(), &run, &seen, CauseStep::Start).unwrap());
            assert_eq!(rig.worker.calls().len(), 1);
        }

        #[test]
        fn a_session_that_cannot_be_ended_leaves_the_run_unfinished() {
            let rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            rig.worker
                .on_end_sessions(Err(WorkerError::Unreachable("gone".into())));
            let seen = rig.ctx().seen(&run).unwrap();
            assert!(abandon(&rig.ctx(), &run, &seen, CauseStep::Start).is_err());
            assert!(!rig.finished(&run));
        }
    }

    mod a_closed_issue {
        use super::*;

        #[test]
        fn an_open_issue_is_open_and_asks_of_no_pull_request() {
            let rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            let seen = rig.ctx().seen(&run).unwrap();
            assert_eq!(
                moved_on_by_closing(&rig.ctx(), &seen, false).unwrap(),
                Closing::Open
            );
            assert!(rig.sim.calls().is_empty());
        }

        #[test]
        fn is_a_persons_move_when_no_reported_pull_request_was_merged() {
            let rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            rig.write(
                &run,
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
            let seen = rig.ctx().seen(&run).unwrap();
            assert_eq!(
                moved_on_by_closing(&rig.ctx(), &seen, true).unwrap(),
                Closing::PersonsMove
            );
        }

        #[test]
        fn is_the_runs_own_merge_when_a_reported_pull_request_was_merged() {
            let rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            rig.write(
                &run,
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
                    head: "fac/8-1".into(),
                    base: "main".into(),
                    cross: false,
                },
            );
            let seen = rig.ctx().seen(&run).unwrap();
            let closing = moved_on_by_closing(&rig.ctx(), &seen, true).unwrap();
            assert!(matches!(closing, Closing::OwnMerge(pull) if pull.number == 12));
        }

        #[test]
        fn a_failed_read_of_a_pull_request_is_an_error_never_an_answer() {
            let rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            rig.write(
                &run,
                Event::PrOpened {
                    pr: 12,
                    url: "u".into(),
                },
            );
            rig.sim.fail("pr view 12", 1);
            let seen = rig.ctx().seen(&run).unwrap();
            assert!(moved_on_by_closing(&rig.ctx(), &seen, true).is_err());
        }

        #[test]
        fn a_run_with_nothing_left_to_write_asks_nothing_of_github() {
            let rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            rig.moved(&run, State::Building, State::PrOpen);
            let seen = rig.ctx().seen(&run).unwrap();
            let (closing, detail) = closed_issue(&rig.ctx(), &run, &seen).unwrap();
            assert_eq!(closing, Closing::Open);
            assert!(detail.is_none());
            assert!(rig.sim.calls().is_empty());
        }

        #[test]
        fn is_read_with_its_detail_for_a_run_that_has_more_to_write() {
            let rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            rig.sim.set_closed(true);
            let seen = rig.ctx().seen(&run).unwrap();
            let (closing, detail) = closed_issue(&rig.ctx(), &run, &seen).unwrap();
            assert_eq!(closing, Closing::PersonsMove);
            assert_eq!(detail.unwrap().number, ISSUE);
        }
    }

    mod the_time_a_wait_has_lasted {
        use std::time::Duration;

        use super::*;

        #[test]
        fn is_the_clock_since_the_record_under_the_same_generation() {
            assert_eq!(
                time_waited(Some((3, 1_000)), 3, 4_500),
                Some(Duration::from_millis(3_500))
            );
            assert_eq!(
                time_waited(Some((3, 1_000)), 3, 1_000),
                Some(Duration::ZERO)
            );
        }

        #[test]
        fn starts_again_under_another_generation() {
            assert_eq!(time_waited(Some((3, 1_000)), 4, 9_000), None);
            assert_eq!(time_waited(Some((4, 1_000)), 3, 9_000), None);
        }

        #[test]
        fn starts_again_when_the_clock_reads_below_the_record() {
            assert_eq!(time_waited(Some((3, 5_000)), 3, 4_999), None);
        }

        #[test]
        fn starts_again_with_no_record() {
            assert_eq!(time_waited(None, 3, 9_000), None);
        }
    }
}
