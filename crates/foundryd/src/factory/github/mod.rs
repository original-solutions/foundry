//! The tracker behind a trait: GitHub through `gh`, or a fake in tests.
//!
//! [`Tracker`] has one required call, [`Tracker::gh`], which runs `gh` once with the arguments
//! given. Every read is a provided method built on it, in [`read`], and so is every write, in
//! [`write`](mod@write), so the real [`Gh`] and the [`FakeTracker`] read and write GitHub by the same code,
//! and a test scripts the fake by the `gh` arguments as the runner's tests script `gh`. Ported
//! from the runner's `src/tracker/github.ts` and `src/tracker/approvalHistory.ts`.
//!
//! Only a tracker built with a hider can post a comment: the comment writes are on
//! [`Commenting`], which [`Hiding`] implements and a bare [`Gh`] or [`FakeTracker`] does not.
//! Every comment body goes through the hider and then `text::for_comment`, and the factory's
//! own markers are added after both. Nothing in the daemon builds a [`Hiding`] yet: the run
//! code that will (tasks 22 and on) is to pass a hider that calls `secrets::hide_known` on the
//! machine's store (task 7), and neither that function nor that code exists today.

pub mod fake;
pub mod gh;
pub mod read;
pub mod write;

use std::sync::Arc;

use foundry_proto::factory::RunId;
use foundry_proto::ids::MachineId;

use super::lifecycle::State;
use super::lifecycle::approval::{Approval, TextCheck};

pub use fake::FakeTracker;
pub use gh::{Answer, Gh};
pub use read::{
    Activity, Blocker, BodyEdit, CheckState, CheckStatus, Comment, DropReason, Dropped, Feedback,
    FeedbackKind, InlineComment, IssueDetail, IssueSummary, Listing, Mergeable, OpenPullRequest,
    PullRequest,
};
pub use write::{
    Claim, IssueComment, MARKER, STATUS_MARKER, claim_comment, claim_marker, newest_claim,
};

/// Hides the secrets it knows in text that is about to leave the machine. The factory's is to
/// be `secrets::hide_known` over the machine's secret store, read at the time of each call;
/// that function and the code that passes it are not written yet.
pub type Hider = Arc<dyn Fn(&str) -> String + Send + Sync>;

/// A tracker built with a hider: `gh` as the tracker it wraps, and every comment body through
/// the hider.
pub struct Hiding {
    tracker: Arc<dyn Tracker>,
    hider: Hider,
}

impl Hiding {
    /// `tracker` with every comment body it posts also through `hider`.
    pub fn new(tracker: Arc<dyn Tracker>, hider: Hider) -> Self {
        Self { tracker, hider }
    }
}

impl std::fmt::Debug for Hiding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hiding").finish_non_exhaustive()
    }
}

impl Tracker for Hiding {
    fn gh(&self, args: &[String]) -> Result<String, TrackerError> {
        self.tracker.gh(args)
    }
}

impl Commenting for Hiding {
    fn hide(&self, text: &str) -> String {
        (self.hider)(text)
    }
}

/// Why a `gh` call gave no answer. The text names the command with every comment body shown
/// as its size, and never holds a token: a token reaches `gh` only through its environment.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TrackerError {
    /// `gh` was still running at its time limit and was killed with everything it started.
    #[error("`{command}` timed out after {seconds} s")]
    Timeout { command: String, seconds: u64 },
    /// GitHub or `gh` refused the login's credentials.
    #[error("`{command}` was refused GitHub's authentication: {}", stderr.trim())]
    Auth { command: String, stderr: String },
    /// GitHub is rate limiting the login: the primary limit, the secondary limit or an HTTP 429.
    #[error("`{command}` hit GitHub's rate limit: {}", stderr.trim())]
    RateLimited { command: String, stderr: String },
    /// GitHub said the thing asked for is not there: `gh` failed with an HTTP 404.
    #[error("`{command}` exited with status {status}: {}", stderr.trim_end())]
    NotFound {
        command: String,
        status: i32,
        stderr: String,
    },
    /// Anything else, with its kind: `gh` could not start, failed, printed too much or printed
    /// what the call did not expect, or an argument was refused before any call.
    #[error("{message}")]
    Other { kind: OtherKind, message: String },
}

/// Which failure a [`TrackerError::Other`] is, so a caller tells them apart without reading the
/// text. These are the failures the runner tags apart and its callers switch on
/// (`GitHubSpawnError`, `GitHubInvalidArgument`, `GitHubParse`), and one for the rest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OtherKind {
    /// `gh` could not be started.
    NotStarted,
    /// An argument was refused before any call.
    InvalidArgument,
    /// `gh` answered with output the call did not expect.
    Parse,
    /// `gh` got no answer from GitHub: the call was ended by a signal or printed more than its
    /// limit, or `gh` said so in one of the shapes `github::gh::finish` names. Asking again
    /// may get one.
    NoAnswer,
    /// `gh` failed in a way no other variant names: GitHub refused the call.
    Failed,
}

/// GitHub read through `gh`. Each call names the repo (`owner/name`) it reads, so one tracker
/// serves every repo of a login.
pub trait Tracker: Send + Sync {
    /// Runs `gh` once with `args` and answers its stdout, or why it gave none.
    fn gh(&self, args: &[String]) -> Result<String, TrackerError>;

    /// The open issues with the state's label, at most [`read::LIST_LIMIT`].
    fn list_issues(&self, repo: &str, state: State) -> Result<Listing<IssueSummary>, TrackerError> {
        read::list_issues(self, repo, state)
    }

    /// One issue with its labels, assignees and body.
    fn issue(&self, repo: &str, number: u64) -> Result<IssueDetail, TrackerError> {
        read::issue(self, repo, number)
    }

    /// Whether an instructor applied the `factory:ready` label in force and every edit since
    /// was an instructor's.
    fn approval(
        &self,
        repo: &str,
        issue: u64,
        instructors: &[String],
    ) -> Result<Approval, TrackerError> {
        read::approval(self, repo, issue, instructors)
    }

    /// Whether the issue text is as an instructor left it at `since`.
    fn text_check(
        &self,
        repo: &str,
        issue: u64,
        since: &str,
        instructors: &[String],
    ) -> Result<TextCheck, TrackerError> {
        read::text_check(self, repo, issue, since, instructors)
    }

    /// Whether anyone, an instructor included, edited the body or renamed the title at or
    /// after `since`.
    fn text_edited_since(&self, repo: &str, issue: u64, since: &str) -> Result<bool, TrackerError> {
        read::text_edited_since(self, repo, issue, since)
    }

    /// The open issues GitHub's dependency links say block `issue`.
    fn open_blockers(&self, repo: &str, issue: u64) -> Result<Vec<Blocker>, TrackerError> {
        read::open_blockers(self, repo, issue)
    }

    /// The issues the body's `## Depends on` section names that are not closed.
    fn text_blockers(&self, repo: &str, issue: u64) -> Result<Vec<Blocker>, TrackerError> {
        read::text_blockers(self, repo, issue)
    }

    /// What the instructors wrote on the issue after `since`: comments and a body edit.
    fn activity_since(
        &self,
        repo: &str,
        issue: u64,
        since: &str,
        instructors: &[String],
    ) -> Result<Activity, TrackerError> {
        read::activity_since(self, repo, issue, since, instructors)
    }

    /// The instructors' reviews and comments on pull request `pr` after `since`, oldest first,
    /// reading back no further than `floor`.
    fn pr_feedback(
        &self,
        repo: &str,
        pr: u64,
        since: &str,
        floor: &str,
        instructors: &[String],
    ) -> Result<Vec<Feedback>, TrackerError> {
        read::pr_feedback(self, repo, pr, since, floor, instructors)
    }

    /// The pull request with its head and state; `None` for a number the repo does not have.
    fn pull_request(&self, repo: &str, number: u64) -> Result<Option<PullRequest>, TrackerError> {
        read::pull_request(self, repo, number)
    }

    /// The open pull requests from the branch `head`, forks included.
    fn open_pull_requests_on_head(
        &self,
        repo: &str,
        head: &str,
    ) -> Result<Vec<OpenPullRequest>, TrackerError> {
        read::open_pull_requests_on_head(self, repo, head)
    }

    /// How each named check stands on the commit `sha`, in the order asked.
    fn checks(
        &self,
        repo: &str,
        sha: &str,
        names: &[String],
    ) -> Result<Vec<CheckStatus>, TrackerError> {
        read::checks(self, repo, sha, names)
    }

    /// The repo's default branch.
    fn default_branch(&self, repo: &str) -> Result<String, TrackerError> {
        read::default_branch(self, repo)
    }

    /// The full commit id at the tip of `branch`.
    fn branch_tip(&self, repo: &str, branch: &str) -> Result<String, TrackerError> {
        read::branch_tip(self, repo, branch)
    }

    /// The text of the file at `path` in the commit `sha`.
    fn file_at(&self, repo: &str, path: &str, sha: &str) -> Result<String, TrackerError> {
        read::file_at(self, repo, path, sha)
    }

    /// Moves the issue to `state`: adds its label, removes every other `factory:*` label, and
    /// touches nothing else. Makes no edit when the labels are already so.
    fn set_state(&self, repo: &str, issue: u64, state: State) -> Result<(), TrackerError> {
        write::set_state(self, repo, issue, state)
    }

    /// Assigns `login` to the issue.
    fn assign(&self, repo: &str, issue: u64, login: &str) -> Result<(), TrackerError> {
        write::assign(self, repo, issue, login)
    }

    /// Takes `login` off the issue's assignees.
    fn unassign(&self, repo: &str, issue: u64, login: &str) -> Result<(), TrackerError> {
        write::unassign(self, repo, issue, login)
    }

    /// Every comment on the issue, in the order GitHub lists them, for [`newest_claim`].
    fn issue_comments(&self, repo: &str, issue: u64) -> Result<Vec<IssueComment>, TrackerError> {
        write::issue_comments(self, repo, issue)
    }

    /// The names of every label the repo has.
    fn label_names(&self, repo: &str) -> Result<Vec<String>, TrackerError> {
        write::label_names(self, repo)
    }

    /// Creates a label. `color` is six hex digits without the `#`. One that exists is an error.
    fn create_label(
        &self,
        repo: &str,
        name: &str,
        color: &str,
        description: &str,
    ) -> Result<(), TrackerError> {
        write::create_label(self, repo, name, color, description)
    }

    /// Creates the factory labels the repo lacks, in lifecycle order, and answers their states.
    fn ensure_labels(&self, repo: &str) -> Result<Vec<State>, TrackerError> {
        write::ensure_labels(self, repo)
    }

    /// Takes pull request `pr` out of draft. One already ready counts as done.
    fn mark_pr_ready(&self, repo: &str, pr: u64) -> Result<(), TrackerError> {
        write::mark_pr_ready(self, repo, pr)
    }

    /// Puts pull request `pr` back to draft. One already a draft, or no longer open, counts as
    /// done.
    fn mark_pr_draft(&self, repo: &str, pr: u64) -> Result<(), TrackerError> {
        write::mark_pr_draft(self, repo, pr)
    }
}

/// A tracker that may post comments: one built with a hider. Every body goes through
/// [`Commenting::hide`] and then `text::for_comment`, and the marker is added after both.
///
/// A post is not idempotent. An error once `gh` has run, a [`TrackerError::Timeout`] above all,
/// does not say whether GitHub made the comment, so posting again can post it twice. A caller
/// that must not double a comment reads [`Tracker::issue_comments`] before it posts again: for a
/// claim, [`newest_claim`] says whether its marker is already there.
///
/// In the daemon only [`Hiding`] implements it, so a tracker built without a hider cannot post:
///
/// ```compile_fail
/// use foundryd::factory::github::{Commenting, FakeTracker};
///
/// FakeTracker::new().post_comment("o/r", 1, "x").unwrap_err();
/// ```
///
/// Built with one, it can:
///
/// ```
/// use std::sync::Arc;
///
/// use foundryd::factory::github::{Commenting, FakeTracker, Hiding};
///
/// let tracker = Hiding::new(Arc::new(FakeTracker::new()), Arc::new(|text: &str| text.to_string()));
/// tracker.post_comment("o/r", 1, "x").unwrap_err();
/// ```
pub trait Commenting: Tracker {
    /// `text` with the secrets this tracker's hider knows hidden, before it goes into a comment.
    fn hide(&self, text: &str) -> String;

    /// Posts a comment on the issue, hidden, ending with [`MARKER`].
    fn post_comment(&self, repo: &str, issue: u64, body: &str) -> Result<(), TrackerError> {
        write::post_comment(self, repo, issue, body)
    }

    /// Posts a comment on pull request `pr`, hidden, ending with [`MARKER`].
    fn comment_on_pr(&self, repo: &str, pr: u64, body: &str) -> Result<(), TrackerError> {
        write::comment_on_pr(self, repo, pr, body)
    }

    /// Writes the issue's one status comment, hidden, ending with [`STATUS_MARKER`]: edits
    /// `login`'s own in place, or posts it.
    fn upsert_status_comment(
        &self,
        repo: &str,
        issue: u64,
        login: &str,
        body: &str,
    ) -> Result<(), TrackerError> {
        write::upsert_status_comment(self, repo, issue, login, body)
    }

    /// Posts Foundry's claim comment for `run` on `machine`: [`claim_comment`].
    fn post_claim(
        &self,
        repo: &str,
        issue: u64,
        run: &RunId,
        machine: &MachineId,
    ) -> Result<(), TrackerError> {
        write::post_claim(self, repo, issue, run, machine)
    }
}
