//! The tracker behind a trait: GitHub through `gh`, or a fake in tests.
//!
//! [`Tracker`] has one required call, [`Tracker::gh`], which runs `gh` once with the arguments
//! given. Every read is a provided method built on it, in [`read`], so the real [`Gh`] and the
//! [`FakeTracker`] read GitHub's answers by the same code, and a test scripts the fake by the
//! `gh` arguments as the runner's tests script `gh`. Ported from the runner's
//! `src/tracker/github.ts` and `src/tracker/approvalHistory.ts`.

pub mod fake;
pub mod gh;
pub mod read;
pub mod write;

use super::lifecycle::State;
use super::lifecycle::approval::{Approval, TextCheck};

pub use fake::FakeTracker;
pub use gh::{Answer, Gh};
pub use read::{
    Activity, Blocker, BodyEdit, CheckState, CheckStatus, Comment, DropReason, Dropped, Feedback,
    FeedbackKind, InlineComment, IssueDetail, IssueSummary, Listing, Mergeable, OpenPullRequest,
    PullRequest,
};

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
    /// `gh` failed in a way no other variant names, or printed more than its limit.
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
}
