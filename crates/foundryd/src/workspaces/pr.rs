//! Where a workspace's branch stands past this machine: [`Workspaces::shipping`].
//!
//! Whether `origin` has the branch and how many commits on the worktree's `HEAD` no remote
//! branch has come from git, with no fetch, so they are as of this machine's last fetch or push.
//! The pull request comes from GitHub through the `gh` command, signed in as this machine's user,
//! and only once the branch is on `origin`: a branch never pushed has no pull request, and a
//! name another branch once had is not looked up. A `gh` that is missing, signed out, slow or
//! offline makes the pull request unknown, never an error: the git half is still answered.

use std::ffi::OsString;
use std::process::Stdio;
use std::time::Duration;

use foundry_proto::ids::WorkspaceId;
use serde::{Deserialize, Serialize};

use super::{WorkspaceError, Workspaces};

/// How long one `gh` call may run before it is stopped and the pull request is unknown.
const GH_WITHIN: Duration = Duration::from_secs(20);

/// The fields asked of `gh pr list`, read by [`pull_request_from`].
const FIELDS: &str = "number,title,url,state,isDraft,reviewDecision,statusCheckRollup";

/// Where a workspace's branch stands past this machine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Shipping {
    pub branch: String,
    /// Whether `origin` has the branch, as of this machine's last fetch or push.
    pub pushed: bool,
    /// Commits on the worktree's `HEAD` that no remote branch has.
    pub unpushed: u64,
    /// The newest pull request from the branch, when GitHub was asked and has one.
    pub pr: Option<PullRequest>,
    /// Whether GitHub was asked and could not answer, so `pr` says nothing.
    pub unreachable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub state: PrState,
    pub draft: bool,
    pub checks: Checks,
    pub review: Review,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PrState {
    Open,
    Merged,
    Closed,
}

/// The pull request's checks, rolled up: any failing is failing, then any still running is
/// pending, then passing; none at all is `None`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Checks {
    None,
    Pending,
    Passing,
    Failing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Review {
    None,
    Required,
    Approved,
    ChangesRequested,
}

impl Workspaces {
    /// Where live workspace `id`'s branch stands: on `origin` or not, what is only here, and its
    /// pull request.
    pub async fn shipping(&self, id: WorkspaceId) -> Result<Shipping, WorkspaceError> {
        let body = self.inner.live(id).await?;
        let worktree = self.inner.worktree(body.project, &body.branch);
        let remote = format!("refs/remotes/origin/{}", body.branch);
        let pushed = self
            .inner
            .git(&worktree, "rev-parse", ["--verify", "--quiet", &remote])
            .await
            .is_ok();
        let unpushed = self
            .inner
            .git(
                &worktree,
                "rev-list",
                ["--count", "HEAD", "--not", "--remotes", "--"],
            )
            .await?
            .trim()
            .parse()
            .unwrap_or(0);
        let (pr, unreachable) = if pushed {
            match self.inner.gh_pull_request(&worktree, &body.branch).await {
                Some(pr) => (pr, false),
                None => (None, true),
            }
        } else {
            (None, false)
        };
        Ok(Shipping {
            branch: body.branch,
            pushed,
            unpushed,
            pr,
            unreachable,
        })
    }
}

impl super::Inner {
    /// The newest pull request whose head is `branch`, asked of GitHub with `gh` in `worktree`:
    /// `Some(None)` when it has none, `None` when `gh` could not answer.
    async fn gh_pull_request(
        &self,
        worktree: &std::path::Path,
        branch: &str,
    ) -> Option<Option<PullRequest>> {
        let run = tokio::process::Command::new(&self.gh)
            .args([
                "pr", "list", "--state", "all", "--limit", "1", "--json", FIELDS,
            ])
            .arg("--head")
            .arg(branch)
            .current_dir(worktree)
            .env("GH_PROMPT_DISABLED", "1")
            .env("GH_NO_UPDATE_NOTIFIER", "1")
            .env("NO_COLOR", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output();
        let out = match tokio::time::timeout(GH_WITHIN, run).await {
            Ok(Ok(out)) if out.status.success() => out,
            Ok(Ok(out)) => {
                tracing::debug!(code = ?out.status.code(), "gh pr list failed");
                return None;
            }
            Ok(Err(e)) => {
                tracing::debug!(error = %e, "gh could not be run");
                return None;
            }
            Err(_) => return None,
        };
        pull_request_from(&String::from_utf8_lossy(&out.stdout))
    }
}

/// The program [`super::Inner`] runs to ask GitHub.
pub(super) fn gh() -> OsString {
    OsString::from("gh")
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Listed {
    number: u64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    state: String,
    #[serde(default)]
    is_draft: bool,
    #[serde(default)]
    review_decision: Option<String>,
    #[serde(default)]
    status_check_rollup: Option<Vec<Check>>,
}

/// One check run (`status`, `conclusion`) or commit status (`state`).
#[derive(Deserialize)]
struct Check {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    conclusion: Option<String>,
    #[serde(default)]
    state: Option<String>,
}

/// The first pull request `gh pr list --json` printed: `Some(None)` for an empty list, `None`
/// for output that is not such a list.
fn pull_request_from(out: &str) -> Option<Option<PullRequest>> {
    let listed: Vec<Listed> = serde_json::from_str(out).ok()?;
    let Some(first) = listed.into_iter().next() else {
        return Some(None);
    };
    let state = match first.state.as_str() {
        "OPEN" => PrState::Open,
        "MERGED" => PrState::Merged,
        "CLOSED" => PrState::Closed,
        _ => return None,
    };
    let review = match first.review_decision.as_deref() {
        Some("APPROVED") => Review::Approved,
        Some("CHANGES_REQUESTED") => Review::ChangesRequested,
        Some("REVIEW_REQUIRED") => Review::Required,
        _ => Review::None,
    };
    Some(Some(PullRequest {
        number: first.number,
        title: first.title,
        url: first.url,
        state,
        draft: first.is_draft,
        checks: rolled_up(&first.status_check_rollup.unwrap_or_default()),
        review,
    }))
}

fn rolled_up(checks: &[Check]) -> Checks {
    const FAILED: &[&str] = &[
        "FAILURE",
        "ERROR",
        "TIMED_OUT",
        "CANCELLED",
        "ACTION_REQUIRED",
        "STARTUP_FAILURE",
    ];
    let failed = |c: &Check| {
        [&c.conclusion, &c.state]
            .into_iter()
            .flatten()
            .any(|s| FAILED.contains(&s.as_str()))
    };
    let running = |c: &Check| match (&c.status, &c.state) {
        (Some(status), _) => status != "COMPLETED",
        (None, Some(state)) => state == "PENDING" || state == "EXPECTED",
        (None, None) => false,
    };
    if checks.is_empty() {
        Checks::None
    } else if checks.iter().any(failed) {
        Checks::Failing
    } else if checks.iter().any(running) {
        Checks::Pending
    } else {
        Checks::Passing
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(json: &str) -> PullRequest {
        pull_request_from(json).unwrap().unwrap()
    }

    #[test]
    fn an_empty_list_is_no_pull_request_and_other_output_is_unknown() {
        assert_eq!(pull_request_from("[]\n"), Some(None));
        assert_eq!(pull_request_from(""), None);
        assert_eq!(pull_request_from("no pull requests found"), None);
        assert_eq!(pull_request_from(r#"[{"number":1,"state":"WEIRD"}]"#), None);
    }

    #[test]
    fn a_merged_pull_request_is_read_with_its_title_and_link() {
        let read = pr(
            r#"[{"number":35,"title":"Fix the overnight entry","url":"https://github.com/o/r/pull/35","state":"MERGED","isDraft":false,"reviewDecision":"APPROVED","statusCheckRollup":[]}]"#,
        );
        assert_eq!(
            read,
            PullRequest {
                number: 35,
                title: "Fix the overnight entry".into(),
                url: "https://github.com/o/r/pull/35".into(),
                state: PrState::Merged,
                draft: false,
                checks: Checks::None,
                review: Review::Approved,
            }
        );
    }

    #[test]
    fn checks_roll_up_failing_before_running_before_passing() {
        let with = |rollup: &str| {
            pr(&format!(
                r#"[{{"number":1,"state":"OPEN","isDraft":true,"reviewDecision":"","statusCheckRollup":{rollup}}}]"#
            ))
        };
        let done = r#"{"__typename":"CheckRun","status":"COMPLETED","conclusion":"SUCCESS"}"#;
        let running = r#"{"__typename":"CheckRun","status":"IN_PROGRESS","conclusion":""}"#;
        let failed = r#"{"__typename":"CheckRun","status":"COMPLETED","conclusion":"FAILURE"}"#;
        let status_ok = r#"{"__typename":"StatusContext","state":"SUCCESS"}"#;
        let status_wait = r#"{"__typename":"StatusContext","state":"PENDING"}"#;
        let status_err = r#"{"__typename":"StatusContext","state":"ERROR"}"#;

        assert_eq!(with("[]").checks, Checks::None);
        assert_eq!(with("null").checks, Checks::None);
        assert_eq!(
            with(&format!("[{done},{status_ok}]")).checks,
            Checks::Passing
        );
        assert_eq!(with(&format!("[{done},{running}]")).checks, Checks::Pending);
        assert_eq!(
            with(&format!("[{done},{status_wait}]")).checks,
            Checks::Pending
        );
        assert_eq!(
            with(&format!("[{running},{failed}]")).checks,
            Checks::Failing
        );
        assert_eq!(
            with(&format!("[{status_wait},{status_err}]")).checks,
            Checks::Failing
        );
        let draft = with("[]");
        assert!(draft.draft);
        assert_eq!(draft.review, Review::None);
        assert_eq!(draft.state, PrState::Open);
    }

    #[test]
    fn review_decisions_are_read() {
        let with = |decision: &str| {
            pr(&format!(
                r#"[{{"number":1,"state":"OPEN","reviewDecision":"{decision}"}}]"#
            ))
            .review
        };
        assert_eq!(with("REVIEW_REQUIRED"), Review::Required);
        assert_eq!(with("CHANGES_REQUESTED"), Review::ChangesRequested);
        assert_eq!(with("APPROVED"), Review::Approved);
        assert_eq!(with(""), Review::None);
    }
}
