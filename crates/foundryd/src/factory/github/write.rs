//! Writes to GitHub: labels, comments and claims.
//!
//! Each write runs `gh` through [`Tracker::gh`], so the real `gh` and the fake are written to
//! alike. Ported from the writing half of the runner's `src/tracker/github.ts`. Every comment
//! body goes through the tracker's hider ([`Commenting::hide`]) and then [`text::for_comment`],
//! and the factory's own marker is added after both, since `for_comment` would hide the run id
//! and the machine id in a claim marker. Only a tracker built with a hider posts: see
//! [`Commenting`].
//!
//! A claim marker counts only in a comment by the project's login. The issue is Foundry's when
//! the newest such comment carries Foundry's marker, `<!-- foundry:claim <run id> <machine or host kind> -->`,
//! as its last line ([`newest_claim`]). Any other claim, the runner's
//! `<!-- softfac:claim <machine> -->` among them, belongs to another factory.

use std::str::FromStr;

use foundry_proto::factory::RunId;
use serde::Deserialize;

use super::read::{gh_json, gh_pages, pull_request};
use super::{Commenting, OtherKind, Tracker, TrackerError};
use crate::factory::lifecycle::State;
use crate::factory::text;
use crate::factory::worker::Target;

/// Ends every comment the factory posts, so it can tell its own comments from instructors'.
pub const MARKER: &str = "<!-- softfac -->";

/// Ends the issue's one status comment in place of [`MARKER`]. It is the runner's marker, so on
/// a login the runner also used, Foundry takes over the runner's status comment and keeps it.
pub const STATUS_MARKER: &str = "<!-- softfac:status -->";

/// How Foundry's claim marker starts: `<!-- foundry:claim <run id> <machine or host kind> -->`.
const CLAIM_PREFIX: &str = "<!-- foundry:claim ";

/// How the runner's claim marker starts: `<!-- softfac:claim <machine> -->`.
const RUNNER_CLAIM_PREFIX: &str = "<!-- softfac:claim ";

/// How every claim marker ends.
const MARKER_END: &str = " -->";

/// What Foundry's claim comment says above its marker. Fixed words: nothing in them is hidden.
const CLAIM_TEXT: &str = "Claimed by Foundry.";

/// `body` and then `marker`, after a blank line.
fn with_marker(body: &str, marker: &str) -> String {
    format!("{body}\n\n{marker}")
}

/// `body` as a comment may carry it: through the tracker's hider, then [`text::for_comment`],
/// then `marker` after both. The hider runs first, so a secret it knows goes whole before
/// `for_comment` can hide part of it and leave the rest unrecognised.
fn comment_body<G: Commenting + ?Sized>(gh: &G, body: &str, marker: &str) -> String {
    with_marker(&text::for_comment(&gh.hide(body)), marker)
}

/// `parts` and then `--repo <repo>`.
fn repo_args(parts: &[&str], repo: &str) -> Vec<String> {
    let mut args: Vec<String> = parts.iter().map(|part| part.to_string()).collect();
    args.push("--repo".into());
    args.push(repo.into());
    args
}

/// Runs a write and drops its stdout.
fn call<G: Tracker + ?Sized>(gh: &G, args: &[String]) -> Result<(), TrackerError> {
    gh.gh(args).map(drop)
}

/// Whether `error` is `gh` failing with `said` on stderr.
fn failed_saying(error: &TrackerError, said: &str) -> bool {
    matches!(error, TrackerError::Other { kind: OtherKind::Failed, message } if message.contains(said))
}

// ---------------------------------------------------------------------------------------------
// The state label and assignees.
// ---------------------------------------------------------------------------------------------

#[derive(Deserialize)]
struct LabelsView {
    labels: Vec<LabelRow>,
}

#[derive(Deserialize)]
struct LabelRow {
    name: String,
}

/// Moves the issue to `state`: adds its label, removes every other `factory:*` label, and
/// touches nothing else. Makes no edit when the labels are already so.
pub fn set_state<G: Tracker + ?Sized>(
    gh: &G,
    repo: &str,
    issue: u64,
    state: State,
) -> Result<(), TrackerError> {
    let number = issue.to_string();
    let label = state.label();
    let mut view = repo_args(&["issue", "view", &number], repo);
    view.extend(["--json".to_string(), "labels".to_string()]);
    let current: LabelsView = gh_json(gh, &view, "an object with `labels`")?;
    let present = current.labels.iter().any(|row| row.name == label);
    let stale: Vec<&str> = current
        .labels
        .iter()
        .map(|row| row.name.as_str())
        .filter(|name| name.starts_with("factory:") && *name != label)
        .collect();
    if present && stale.is_empty() {
        return Ok(());
    }
    let mut edit = repo_args(&["issue", "edit", &number], repo);
    if !present {
        edit.extend(["--add-label".to_string(), label.to_string()]);
    }
    for name in stale {
        edit.extend(["--remove-label".to_string(), name.to_string()]);
    }
    call(gh, &edit)
}

/// Assigns `login` to the issue.
pub fn assign<G: Tracker + ?Sized>(
    gh: &G,
    repo: &str,
    issue: u64,
    login: &str,
) -> Result<(), TrackerError> {
    let mut args = repo_args(&["issue", "edit", &issue.to_string()], repo);
    args.extend(["--add-assignee".to_string(), login.to_string()]);
    call(gh, &args)
}

/// Takes `login` off the issue's assignees.
pub fn unassign<G: Tracker + ?Sized>(
    gh: &G,
    repo: &str,
    issue: u64,
    login: &str,
) -> Result<(), TrackerError> {
    let mut args = repo_args(&["issue", "edit", &issue.to_string()], repo);
    args.extend(["--remove-assignee".to_string(), login.to_string()]);
    call(gh, &args)
}

// ---------------------------------------------------------------------------------------------
// Comments.
// ---------------------------------------------------------------------------------------------

/// One comment on an issue as GitHub's REST listing gives it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssueComment {
    /// GitHub's id for the comment. Ids only rise, so a higher one is a newer comment.
    pub id: u64,
    /// Its author's login; `None` for a deleted account.
    pub author: Option<String>,
    pub body: String,
}

#[derive(Deserialize)]
struct CommentRow {
    id: u64,
    #[serde(default)]
    user: Option<UserRow>,
    #[serde(default)]
    body: Option<String>,
}

#[derive(Deserialize)]
struct UserRow {
    login: String,
}

/// The call that lists every comment on the issue.
fn comments_call(repo: &str, issue: u64) -> Vec<String> {
    vec![
        "api".into(),
        format!("repos/{repo}/issues/{issue}/comments?per_page=100"),
        "--paginate".into(),
    ]
}

/// Every comment on the issue, in the order GitHub lists them.
pub fn issue_comments<G: Tracker + ?Sized>(
    gh: &G,
    repo: &str,
    issue: u64,
) -> Result<Vec<IssueComment>, TrackerError> {
    let rows: Vec<CommentRow> =
        gh_pages(gh, &comments_call(repo, issue), "JSON arrays of comments")?;
    Ok(rows
        .into_iter()
        .map(|row| IssueComment {
            id: row.id,
            author: row.user.map(|user| user.login),
            body: row.body.unwrap_or_default(),
        })
        .collect())
}

/// Posts a comment on the issue, hidden, ending with [`MARKER`].
pub fn post_comment<G: Commenting + ?Sized>(
    gh: &G,
    repo: &str,
    issue: u64,
    body: &str,
) -> Result<(), TrackerError> {
    let mut args = repo_args(&["issue", "comment", &issue.to_string()], repo);
    args.extend(["--body".to_string(), comment_body(gh, body, MARKER)]);
    call(gh, &args)
}

/// Posts a comment on pull request `pr`, hidden, ending with [`MARKER`].
pub fn comment_on_pr<G: Commenting + ?Sized>(
    gh: &G,
    repo: &str,
    pr: u64,
    body: &str,
) -> Result<(), TrackerError> {
    let mut args = repo_args(&["pr", "comment", &pr.to_string()], repo);
    args.extend(["--body".to_string(), comment_body(gh, body, MARKER)]);
    call(gh, &args)
}

/// Writes the issue's one status comment, hidden, ending with [`STATUS_MARKER`]: edits
/// `login`'s existing one in place, or posts it. Only a comment by `login`, matched ignoring
/// ASCII case, that ends with the status marker counts, so nobody else can take it over. A
/// list and then a write, so two callers at once could still post two.
pub fn upsert_status_comment<G: Commenting + ?Sized>(
    gh: &G,
    repo: &str,
    issue: u64,
    login: &str,
    body: &str,
) -> Result<(), TrackerError> {
    let existing = issue_comments(gh, repo, issue)?;
    let status = existing.iter().find(|comment| {
        comment
            .author
            .as_deref()
            .is_some_and(|author| author.eq_ignore_ascii_case(login))
            && comment.body.trim_end().ends_with(STATUS_MARKER)
    });
    let text = comment_body(gh, body, STATUS_MARKER);
    match status {
        None => {
            let mut args = repo_args(&["issue", "comment", &issue.to_string()], repo);
            args.extend(["--body".to_string(), text]);
            call(gh, &args)
        }
        Some(status) => call(
            gh,
            &[
                "api".into(),
                "-X".into(),
                "PATCH".into(),
                format!("repos/{repo}/issues/comments/{}", status.id),
                "-f".into(),
                format!("body={text}"),
            ],
        ),
    }
}

// ---------------------------------------------------------------------------------------------
// Claims.
// ---------------------------------------------------------------------------------------------

/// Whose an issue's newest claim comment says the issue is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Claim {
    /// Foundry's own: the run and the target its marker names.
    Foundry { run: RunId, target: Target },
    /// Another factory's: the runner's marker in the login's comment, or a Foundry marker that
    /// does not parse. A claim marker in another login's comment is no claim at all.
    Foreign { comment_id: u64 },
}

/// Foundry's claim marker for `run` at `target`: `<!-- foundry:claim <run id> <target> -->`, the
/// machine's id or the host's kind.
pub fn claim_marker(run: &RunId, target: &Target) -> String {
    format!("{CLAIM_PREFIX}{run} {target}{MARKER_END}")
}

/// Foundry's claim comment for `run` at `target`, with [`claim_marker`] as its last line.
pub fn claim_comment(run: &RunId, target: &Target) -> String {
    with_marker(CLAIM_TEXT, &claim_marker(run, target))
}

/// Posts Foundry's claim comment for `run` at `target`. Its words go through the hider and
/// [`text::for_comment`] like any comment's; its marker is added after them, whole.
pub fn post_claim<G: Commenting + ?Sized>(
    gh: &G,
    repo: &str,
    issue: u64,
    run: &RunId,
    target: &Target,
) -> Result<(), TrackerError> {
    let mut args = repo_args(&["issue", "comment", &issue.to_string()], repo);
    args.extend([
        "--body".to_string(),
        comment_body(gh, CLAIM_TEXT, &claim_marker(run, target)),
    ]);
    call(gh, &args)
}

/// What a comment's last line says when it is a claim marker: Foundry's, with what it names
/// when that parses, or another factory's.
enum Marker {
    Foundry(Option<(RunId, Target)>),
    Runner,
}

/// The claim marker that is `body`'s last line, after trailing whitespace, if there is one.
fn claim_marker_of(body: &str) -> Option<Marker> {
    let last = body.trim_end().lines().last()?.trim();
    if let Some(named) = last
        .strip_prefix(CLAIM_PREFIX)
        .and_then(|rest| rest.strip_suffix(MARKER_END))
    {
        return Some(Marker::Foundry(parse_foundry_claim(named)));
    }
    let named = last
        .strip_prefix(RUNNER_CLAIM_PREFIX)?
        .strip_suffix(MARKER_END)?;
    (!named.is_empty() && !named.contains(char::is_whitespace)).then_some(Marker::Runner)
}

/// The run and target of Foundry's marker, `<run id> <target>`, each in the one form Foundry
/// writes it. A host kind this build does not know does not parse.
fn parse_foundry_claim(named: &str) -> Option<(RunId, Target)> {
    let (run, target) = named.split_once(' ')?;
    let parsed_run = RunId::from_str(run).ok()?;
    let parsed_target = Target::from_str(target).ok()?;
    (parsed_run.to_string() == run && parsed_target.to_string() == target)
        .then_some((parsed_run, parsed_target))
}

/// Whose the issue is by its newest claim comment, or `None` when it has none.
///
/// A claim comment is one by `login`, matched ignoring ASCII case, whose last line is a claim
/// marker, Foundry's or the runner's. A marker in anyone else's comment counts for nothing, so
/// nobody else can take a claim from Foundry or hand one to it. The newest is the one with the
/// highest id, never by time. It is Foundry's when it carries Foundry's marker and the marker
/// parses; any other is another factory's.
///
/// The run a Foundry claim names is as the comment says: the caller checks that it is a run of
/// the issue it read the comments of.
pub fn newest_claim(comments: &[IssueComment], login: &str) -> Option<Claim> {
    let (comment, marker) = comments
        .iter()
        .filter(|comment| {
            comment
                .author
                .as_deref()
                .is_some_and(|author| author.eq_ignore_ascii_case(login))
        })
        .filter_map(|comment| claim_marker_of(&comment.body).map(|marker| (comment, marker)))
        .max_by_key(|(comment, _)| comment.id)?;
    Some(match marker {
        Marker::Foundry(Some((run, target))) => Claim::Foundry { run, target },
        Marker::Foundry(None) | Marker::Runner => Claim::Foreign {
            comment_id: comment.id,
        },
    })
}

// ---------------------------------------------------------------------------------------------
// Labels.
// ---------------------------------------------------------------------------------------------

/// The names of every label the repo has.
pub fn label_names<G: Tracker + ?Sized>(gh: &G, repo: &str) -> Result<Vec<String>, TrackerError> {
    let args = vec![
        "api".to_string(),
        format!("repos/{repo}/labels?per_page=100"),
        "--paginate".to_string(),
    ];
    let rows: Vec<LabelRow> = gh_pages(gh, &args, "JSON arrays of labels")?;
    Ok(rows.into_iter().map(|row| row.name).collect())
}

/// Creates a label. `color` is six hex digits without the `#`. One that exists is an error.
pub fn create_label<G: Tracker + ?Sized>(
    gh: &G,
    repo: &str,
    name: &str,
    color: &str,
    description: &str,
) -> Result<(), TrackerError> {
    let mut args = repo_args(&["label", "create", name], repo);
    args.extend([
        "--color".to_string(),
        color.to_string(),
        "--description".to_string(),
        description.to_string(),
    ]);
    call(gh, &args)
}

/// Creates the factory labels the repo lacks, in lifecycle order, and answers their states.
/// Stops at the first that fails.
pub fn ensure_labels<G: Tracker + ?Sized>(gh: &G, repo: &str) -> Result<Vec<State>, TrackerError> {
    let existing = label_names(gh, repo)?;
    let mut created = Vec::new();
    for state in State::ALL {
        if existing.iter().any(|name| name == state.label()) {
            continue;
        }
        create_label(gh, repo, state.label(), state.color(), state.description())?;
        created.push(state);
    }
    Ok(created)
}

// ---------------------------------------------------------------------------------------------
// Pull requests: ready and draft.
// ---------------------------------------------------------------------------------------------

/// Takes pull request `pr` out of draft. One that is already ready counts as done: `gh` warns
/// `Pull request <repo>#<n> is already "ready for review"` and exits 0, and should a version
/// fail on it the pull request is still ready. An answer lost on the way back leaves a caller
/// asking again.
pub fn mark_pr_ready<G: Tracker + ?Sized>(gh: &G, repo: &str, pr: u64) -> Result<(), TrackerError> {
    match call(gh, &repo_args(&["pr", "ready", &pr.to_string()], repo)) {
        Err(error) if failed_saying(&error, "is already \"ready for review\"") => Ok(()),
        done => done,
    }
}

/// Puts pull request `pr` back to draft (`gh pr ready --undo`). One already a draft counts as
/// done (`gh` warns and exits 0), and so does one no longer open, so a resumed decision does
/// not fail on it. On any other failure the pull request is read and its state decides, not
/// the error's words; a failed read, one that is gone or one still open leaves the original
/// error.
pub fn mark_pr_draft<G: Tracker + ?Sized>(gh: &G, repo: &str, pr: u64) -> Result<(), TrackerError> {
    let mut args = repo_args(&["pr", "ready", &pr.to_string()], repo);
    args.push("--undo".into());
    let Err(original) = call(gh, &args) else {
        return Ok(());
    };
    if failed_saying(&original, "is already \"in draft\"") {
        return Ok(());
    }
    match pull_request(gh, repo, pr) {
        Ok(Some(pull)) if !pull.open => Ok(()),
        _ => Err(original),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use foundry_proto::factory::{HostKind, RunId};
    use foundry_proto::ids::MachineId;
    use serde_json::json;

    use super::*;
    use crate::factory::github::{
        Answer, Commenting, FakeTracker, Hider, Hiding, OtherKind, Tracker,
    };
    use crate::factory::lifecycle::State;
    use crate::factory::text::for_comment;

    const REPO: &str = "rawphp/softfac";
    const HEAD_SHA: &str = "0123456789abcdef0123456789abcdef01234567";
    const MACHINE: &str = "6f1c2a8e-3b4d-4e5f-9a0b-1c2d3e4f5a6b";

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| part.to_string()).collect()
    }

    fn labels_reply(names: &[&str]) -> String {
        let labels: Vec<String> = names
            .iter()
            .map(|name| {
                format!("{{\"id\":\"LA_1\",\"name\":\"{name}\",\"description\":\"d\",\"color\":\"0e8a16\"}}")
            })
            .collect();
        format!("{{\"labels\":[{}]}}", labels.join(","))
    }

    fn edit_call(rest: &[&str]) -> Vec<String> {
        let mut call = args(&["issue", "edit", "6", "--repo", REPO]);
        call.extend(args(rest));
        call
    }

    fn view_labels_call() -> Vec<String> {
        args(&["issue", "view", "6", "--repo", REPO, "--json", "labels"])
    }

    fn comment_call(issue: &str, body: &str) -> Vec<String> {
        args(&["issue", "comment", issue, "--repo", REPO, "--body", body])
    }

    fn list_comments_call() -> Vec<String> {
        args(&[
            "api",
            "repos/rawphp/softfac/issues/6/comments?per_page=100",
            "--paginate",
        ])
    }

    fn patch_call(id: &str, body: &str) -> Vec<String> {
        vec![
            "api".into(),
            "-X".into(),
            "PATCH".into(),
            format!("repos/rawphp/softfac/issues/comments/{id}"),
            "-f".into(),
            format!("body={body}"),
        ]
    }

    /// The label table: each state, its colour and its description.
    const LABEL_TABLE: [(&str, &str, &str); 7] = [
        (
            "proposed",
            "c5def5",
            "A bot filed it. The factory ignores it until a person sets `factory:ready`.",
        ),
        ("ready", "0e8a16", "Approved for the factory."),
        (
            "triage",
            "fbca04",
            "Claimed. The session is being launched.",
        ),
        (
            "needs-info",
            "d93f0b",
            "The factory asked questions on the issue. Answer them, then set `factory:ready`.",
        ),
        ("building", "1d76db", "A session is building it."),
        (
            "pr-open",
            "5319e7",
            "The PR is open and has passed the gate.",
        ),
        (
            "stuck",
            "b60205",
            "The factory gave up and said why in a comment.",
        ),
    ];

    fn create_call(state: &str, color: &str, description: &str) -> Vec<String> {
        vec![
            "label".into(),
            "create".into(),
            format!("factory:{state}"),
            "--repo".into(),
            REPO.into(),
            "--color".into(),
            color.into(),
            "--description".into(),
            description.into(),
        ]
    }

    fn is_failed(error: &TrackerError) -> bool {
        matches!(
            error,
            TrackerError::Other {
                kind: OtherKind::Failed,
                ..
            }
        )
    }

    fn is_parse(error: &TrackerError) -> bool {
        matches!(
            error,
            TrackerError::Other {
                kind: OtherKind::Parse,
                ..
            }
        )
    }

    /// A fake that may post, with a hider that knows no secret: the body reaches `gh` as
    /// `for_comment` leaves it.
    struct Plain<'a>(&'a FakeTracker);

    impl Tracker for Plain<'_> {
        fn gh(&self, args: &[String]) -> Result<String, TrackerError> {
            self.0.gh(args)
        }
    }

    impl Commenting for Plain<'_> {
        fn hide(&self, text: &str) -> String {
            text.to_string()
        }
    }

    fn plain(fake: &FakeTracker) -> Plain<'_> {
        Plain(fake)
    }

    // -----------------------------------------------------------------------------------------
    // Labels.
    // -----------------------------------------------------------------------------------------

    #[test]
    fn ensure_labels_on_an_empty_repo_creates_all_seven_labels_exactly() {
        let fake = FakeTracker::answering([Answer::ok("[]")]);
        for _ in LABEL_TABLE {
            fake.then(Answer::ok(""));
        }
        let created = fake.ensure_labels(REPO).unwrap();
        assert_eq!(created, State::ALL.to_vec());
        let expected: Vec<Vec<String>> = LABEL_TABLE
            .iter()
            .map(|(state, color, description)| create_call(state, color, description))
            .collect();
        assert_eq!(fake.calls()[1..].to_vec(), expected);
    }

    #[test]
    fn label_names_lists_every_page_with_the_exact_call() {
        let fake = FakeTracker::answering([
            Answer::ok("[{\"name\":\"bug\",\"color\":\"d73a4a\"}]"),
            Answer::ok("[]"),
            Answer::ok("[{\"name\":\"a\"}][{\"name\":\"b\"}]"),
        ]);
        assert_eq!(fake.label_names(REPO).unwrap(), vec!["bug".to_string()]);
        assert_eq!(
            fake.calls()[0],
            args(&[
                "api",
                "repos/rawphp/softfac/labels?per_page=100",
                "--paginate"
            ])
        );
        // One array per page reads the same as one merged array.
        fake.label_names(REPO).unwrap();
        assert_eq!(
            fake.label_names(REPO).unwrap(),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn label_names_of_a_repo_gh_cannot_read_is_the_gh_failure() {
        let fake = FakeTracker::answering([Answer::fail(1, "gh: Not Found (HTTP 404)")]);
        assert!(matches!(
            fake.label_names(REPO).unwrap_err(),
            TrackerError::NotFound { status: 1, .. }
        ));
    }

    #[test]
    fn create_label_sends_the_name_colour_and_description_as_arguments() {
        let fake = FakeTracker::answering([Answer::ok("")]);
        fake.create_label(REPO, "factory:ready", "0e8a16", "Approved for the factory")
            .unwrap();
        assert_eq!(
            fake.calls(),
            vec![args(&[
                "label",
                "create",
                "factory:ready",
                "--repo",
                REPO,
                "--color",
                "0e8a16",
                "--description",
                "Approved for the factory",
            ])]
        );
    }

    #[test]
    fn a_refused_create_label_is_the_gh_failure() {
        let fake = FakeTracker::answering([Answer::fail(1, "HTTP 403: Resource not accessible")]);
        let error = fake
            .create_label(REPO, "factory:ready", "0e8a16", "d")
            .unwrap_err();
        assert!(is_failed(&error), "{error}");
    }

    #[test]
    fn ensure_labels_creates_only_the_missing_factory_labels_in_table_order() {
        let fake = FakeTracker::answering([Answer::ok(
            "[{\"name\":\"bug\"},{\"name\":\"factory:ready\"}][{\"name\":\"factory:stuck\"}]",
        )]);
        for _ in 0..5 {
            fake.then(Answer::ok(""));
        }
        let created = fake.ensure_labels(REPO).unwrap();
        assert_eq!(
            created,
            vec![
                State::Proposed,
                State::Triage,
                State::NeedsInfo,
                State::Building,
                State::PrOpen
            ]
        );
        let calls = fake.calls();
        assert_eq!(calls.len(), 6);
        assert_eq!(
            calls[1],
            create_call(
                "proposed",
                "c5def5",
                "A bot filed it. The factory ignores it until a person sets `factory:ready`.",
            )
        );
        assert_eq!(
            calls[2..]
                .iter()
                .map(|call| call[2].as_str())
                .collect::<Vec<_>>(),
            vec![
                "factory:triage",
                "factory:needs-info",
                "factory:building",
                "factory:pr-open"
            ]
        );
    }

    #[test]
    fn ensure_labels_creates_nothing_when_every_label_exists() {
        let names: Vec<_> = LABEL_TABLE
            .iter()
            .map(|(state, _, _)| json!({ "name": format!("factory:{state}") }))
            .collect();
        let fake = FakeTracker::answering([Answer::ok(json!(names).to_string())]);
        assert_eq!(fake.ensure_labels(REPO).unwrap(), Vec::<State>::new());
        assert_eq!(fake.calls().len(), 1);
    }

    #[test]
    fn ensure_labels_stops_at_the_first_failure() {
        let fake = FakeTracker::answering([
            Answer::ok("[]"),
            Answer::ok(""),
            Answer::fail(1, "HTTP 403"),
            Answer::ok(""),
        ]);
        let error = fake.ensure_labels(REPO).unwrap_err();
        assert!(is_failed(&error), "{error}");
        assert_eq!(fake.calls().len(), 3);
    }

    // -----------------------------------------------------------------------------------------
    // The state label and assignees.
    // -----------------------------------------------------------------------------------------

    #[test]
    fn set_state_from_one_wrong_label() {
        let fake =
            FakeTracker::answering([Answer::ok(labels_reply(&["factory:ready"])), Answer::ok("")]);
        fake.set_state(REPO, 6, State::Triage).unwrap();
        assert_eq!(
            fake.calls(),
            vec![
                view_labels_call(),
                edit_call(&[
                    "--add-label",
                    "factory:triage",
                    "--remove-label",
                    "factory:ready"
                ]),
            ]
        );
    }

    #[test]
    fn set_state_from_two_factory_labels() {
        let fake = FakeTracker::answering([
            Answer::ok(labels_reply(&["factory:ready", "factory:stuck"])),
            Answer::ok(""),
        ]);
        fake.set_state(REPO, 6, State::Building).unwrap();
        assert_eq!(
            fake.calls(),
            vec![
                view_labels_call(),
                edit_call(&[
                    "--add-label",
                    "factory:building",
                    "--remove-label",
                    "factory:ready",
                    "--remove-label",
                    "factory:stuck",
                ]),
            ]
        );
    }

    #[test]
    fn set_state_removes_extras_when_the_target_is_already_present() {
        let fake = FakeTracker::answering([
            Answer::ok(labels_reply(&["factory:triage", "factory:ready"])),
            Answer::ok(""),
        ]);
        fake.set_state(REPO, 6, State::Triage).unwrap();
        assert_eq!(
            fake.calls(),
            vec![
                view_labels_call(),
                edit_call(&["--remove-label", "factory:ready"])
            ]
        );
    }

    #[test]
    fn set_state_adds_when_there_is_no_factory_label() {
        let fake = FakeTracker::answering([Answer::ok(labels_reply(&["phase:1"])), Answer::ok("")]);
        fake.set_state(REPO, 6, State::Ready).unwrap();
        assert_eq!(
            fake.calls(),
            vec![
                view_labels_call(),
                edit_call(&["--add-label", "factory:ready"])
            ]
        );
    }

    #[test]
    fn set_state_already_correct_makes_no_edit() {
        let fake =
            FakeTracker::answering([Answer::ok(labels_reply(&["phase:1", "factory:building"]))]);
        fake.set_state(REPO, 6, State::Building).unwrap();
        assert_eq!(fake.calls(), vec![view_labels_call()]);
    }

    #[test]
    fn set_state_leaves_non_factory_labels_alone() {
        let fake = FakeTracker::answering([
            Answer::ok(labels_reply(&[
                "phase:1",
                "factory:ready",
                "bug",
                "factory-ish",
            ])),
            Answer::ok(""),
        ]);
        fake.set_state(REPO, 6, State::Triage).unwrap();
        assert_eq!(
            fake.calls()[1],
            edit_call(&[
                "--add-label",
                "factory:triage",
                "--remove-label",
                "factory:ready"
            ])
        );
    }

    #[test]
    fn set_state_reads_the_real_view_shape() {
        let real = "{\"assignees\":[{\"login\":\"rawphp\"}],\"labels\":[{\"id\":\"x\",\"name\":\"factory:ready\",\"description\":\"d\",\"color\":\"0e8a16\"},{\"name\":\"phase:1\"}]}";
        let fake = FakeTracker::answering([Answer::ok(real), Answer::ok("")]);
        fake.set_state(REPO, 6, State::Triage).unwrap();
        assert_eq!(fake.calls().len(), 2);
    }

    #[test]
    fn set_state_surfaces_a_failing_edit() {
        let fake = FakeTracker::answering([
            Answer::ok(labels_reply(&["factory:ready"])),
            Answer::fail(1, "label does not exist"),
        ]);
        let error = fake.set_state(REPO, 6, State::Triage).unwrap_err();
        assert!(is_failed(&error), "{error}");
        let message = error.to_string();
        assert!(message.contains("label does not exist"), "{message}");
        assert!(message.starts_with("`gh issue edit 6 "), "{message}");
        assert_eq!(fake.calls().len(), 2);
    }

    #[test]
    fn unassign_removes_the_user() {
        let fake = FakeTracker::answering([Answer::ok("")]);
        fake.unassign(REPO, 6, "rawphp").unwrap();
        assert_eq!(
            fake.calls(),
            vec![edit_call(&["--remove-assignee", "rawphp"])]
        );
    }

    #[test]
    fn assign_adds_the_user_and_a_failure_is_an_error() {
        let fake = FakeTracker::answering([Answer::ok("")]);
        fake.assign(REPO, 6, "rawphp").unwrap();
        assert_eq!(fake.calls(), vec![edit_call(&["--add-assignee", "rawphp"])]);

        let failing = FakeTracker::answering([Answer::fail(1, "HTTP 403\n")]);
        let error = failing.assign(REPO, 6, "rawphp").unwrap_err();
        assert!(is_failed(&error), "{error}");
    }

    // -----------------------------------------------------------------------------------------
    // Comments.
    // -----------------------------------------------------------------------------------------

    #[test]
    fn post_comment_appends_the_marker() {
        let fake = FakeTracker::answering([Answer::ok(
            "https://github.com/rawphp/softfac/issues/6#issuecomment-1\n",
        )]);
        plain(&fake).post_comment(REPO, 6, "hello").unwrap();
        assert_eq!(
            fake.calls(),
            vec![comment_call("6", "hello\n\n<!-- softfac -->")]
        );
        assert_eq!(MARKER, "<!-- softfac -->");
        assert_eq!(STATUS_MARKER, "<!-- softfac:status -->");
    }

    #[test]
    fn untrusted_bodies_arrive_as_one_unmodified_argument() {
        let nasty = "$(rm -rf ~) `id` \"q\" 'q' ; | && > /tmp/x\nsecond line\n--add-label evil";
        let expected = format!("{nasty}\n\n{MARKER}");
        let status = format!("{nasty}\n\n{STATUS_MARKER}");

        let post = FakeTracker::answering([Answer::ok("")]);
        plain(&post).post_comment(REPO, 6, nasty).unwrap();
        assert_eq!(post.calls(), vec![comment_call("6", &expected)]);

        let create = FakeTracker::answering([Answer::ok("[]"), Answer::ok("")]);
        plain(&create)
            .upsert_status_comment(REPO, 6, "rawphp", nasty)
            .unwrap();
        assert_eq!(create.calls()[1], comment_call("6", &status));

        let patch = FakeTracker::answering([
            Answer::ok(
                "[{\"id\":77,\"user\":{\"login\":\"rawphp\"},\"body\":\"old\\n\\n<!-- softfac:status -->\"}]",
            ),
            Answer::ok(""),
        ]);
        plain(&patch)
            .upsert_status_comment(REPO, 6, "rawphp", nasty)
            .unwrap();
        assert_eq!(patch.calls()[1], patch_call("77", &status));
    }

    #[test]
    fn post_comment_errors_carry_the_status_and_stderr() {
        let fake = FakeTracker::answering([Answer::fail(1, "issue is locked")]);
        let error = plain(&fake).post_comment(REPO, 6, "x").unwrap_err();
        assert!(is_failed(&error), "{error}");
        let message = error.to_string();
        assert!(message.contains("exited with status 1"), "{message}");
        assert!(message.contains("issue is locked"), "{message}");
    }

    #[test]
    fn the_status_comment_is_created_when_absent() {
        let listing = "[{\"id\":1,\"user\":{\"login\":\"rawphp\"},\"created_at\":\"2026-10-04T10:13:16Z\",\"body\":\"a person's comment\"},{\"id\":2,\"user\":{\"login\":\"rawphp\"},\"created_at\":\"2026-10-04T10:14:16Z\",\"body\":\"questions\\n\\n<!-- softfac -->\"}]";
        let fake = FakeTracker::answering([Answer::ok(listing), Answer::ok("")]);
        plain(&fake)
            .upsert_status_comment(REPO, 6, "rawphp", "state: building")
            .unwrap();
        assert_eq!(
            fake.calls(),
            vec![
                list_comments_call(),
                comment_call("6", "state: building\n\n<!-- softfac:status -->"),
            ]
        );
    }

    #[test]
    fn the_status_comment_is_patched_by_id_when_present() {
        let listing = "[{\"id\":1,\"user\":{\"login\":\"rawphp\"},\"created_at\":\"2026-10-04T10:13:16Z\",\"body\":\"hi\"},{\"id\":5978910929,\"user\":{\"login\":\"rawphp\"},\"created_at\":\"2026-10-04T10:14:16Z\",\"body\":\"state: triage\\n\\n<!-- softfac:status -->\"}]";
        let fake = FakeTracker::answering([Answer::ok(listing), Answer::ok("{}")]);
        plain(&fake)
            .upsert_status_comment(REPO, 6, "rawphp", "state: building")
            .unwrap();
        assert_eq!(
            fake.calls(),
            vec![
                list_comments_call(),
                patch_call("5978910929", "state: building\n\n<!-- softfac:status -->"),
            ]
        );
    }

    #[test]
    fn the_status_comment_is_never_created_twice() {
        let created = format!(
            "[{{\"id\":42,\"user\":{{\"login\":\"rawphp\"}},\"created_at\":\"2026-10-04T10:13:16Z\",\"body\":\"one\\n\\n{STATUS_MARKER}\"}}]"
        );
        let fake = FakeTracker::answering([
            Answer::ok("[]"),
            Answer::ok(""),
            Answer::ok(created),
            Answer::ok("{}"),
        ]);
        plain(&fake)
            .upsert_status_comment(REPO, 6, "rawphp", "one")
            .unwrap();
        plain(&fake)
            .upsert_status_comment(REPO, 6, "rawphp", "two")
            .unwrap();
        let calls = fake.calls();
        assert_eq!(calls.len(), 4);
        assert_eq!(
            calls
                .iter()
                .filter(|call| call[0] == "issue" && call[1] == "comment")
                .count(),
            1
        );
        assert_eq!(
            calls[3][..4].to_vec(),
            args(&[
                "api",
                "-X",
                "PATCH",
                "repos/rawphp/softfac/issues/comments/42"
            ])
        );
    }

    #[test]
    fn of_several_status_comments_by_the_login_the_first_listed_is_patched() {
        // The runner takes the first in the listing: not the last, and not the highest or
        // lowest id.
        let listing = "[{\"id\":20,\"user\":{\"login\":\"rawphp\"},\"body\":\"a\\n\\n<!-- softfac:status -->\"},{\"id\":10,\"user\":{\"login\":\"rawphp\"},\"body\":\"b\\n\\n<!-- softfac:status -->\"},{\"id\":30,\"user\":{\"login\":\"rawphp\"},\"body\":\"c\\n\\n<!-- softfac:status -->\"}]";
        let fake = FakeTracker::answering([Answer::ok(listing), Answer::ok("{}")]);
        plain(&fake)
            .upsert_status_comment(REPO, 6, "rawphp", "s")
            .unwrap();
        assert_eq!(
            fake.calls(),
            vec![
                list_comments_call(),
                patch_call("20", "s\n\n<!-- softfac:status -->"),
            ]
        );
    }

    #[test]
    fn a_status_marker_posted_by_someone_else_is_not_adopted() {
        let listing = "[{\"id\":10,\"user\":{\"login\":\"stranger\"},\"created_at\":\"2026-10-04T10:13:16Z\",\"body\":\"gotcha\\n\\n<!-- softfac:status -->\"},{\"id\":11,\"user\":null,\"created_at\":\"2026-10-04T10:14:16Z\",\"body\":\"ghost\\n\\n<!-- softfac:status -->\"}]";
        let fake = FakeTracker::answering([Answer::ok(listing), Answer::ok("")]);
        plain(&fake)
            .upsert_status_comment(REPO, 6, "rawphp", "real status")
            .unwrap();
        assert_eq!(
            fake.calls(),
            vec![
                list_comments_call(),
                comment_call("6", "real status\n\n<!-- softfac:status -->"),
            ]
        );
    }

    #[test]
    fn a_status_marker_in_the_middle_of_the_factorys_comment_is_not_adopted() {
        let listing = "[{\"id\":10,\"user\":{\"login\":\"rawphp\"},\"created_at\":\"2026-10-04T10:13:16Z\",\"body\":\"quoting <!-- softfac:status --> and more text\"}]";
        let fake = FakeTracker::answering([Answer::ok(listing), Answer::ok("")]);
        plain(&fake)
            .upsert_status_comment(REPO, 6, "rawphp", "s")
            .unwrap();
        assert_eq!(
            fake.calls(),
            vec![
                list_comments_call(),
                comment_call("6", "s\n\n<!-- softfac:status -->"),
            ]
        );
    }

    #[test]
    fn the_factorys_own_status_comment_is_found_despite_case_and_trailing_whitespace() {
        let listing = "[{\"id\":10,\"user\":{\"login\":\"stranger\"},\"body\":\"fake\\n\\n<!-- softfac:status -->\"},{\"id\":12,\"user\":{\"login\":\"RawPhp\"},\"body\":\"state: triage\\n\\n<!-- softfac:status -->\\n\\n\"}]";
        let fake = FakeTracker::answering([Answer::ok(listing), Answer::ok("{}")]);
        plain(&fake)
            .upsert_status_comment(REPO, 6, "rawphp", "s")
            .unwrap();
        assert_eq!(
            fake.calls()[1],
            patch_call("12", "s\n\n<!-- softfac:status -->")
        );
    }

    #[test]
    fn the_status_comment_user_match_is_exact_not_a_prefix() {
        let listing = "[{\"id\":10,\"user\":{\"login\":\"rawphp-bot\"},\"body\":\"a\\n\\n<!-- softfac:status -->\"},{\"id\":11,\"user\":{\"login\":\"raw\"},\"body\":\"b\\n\\n<!-- softfac:status -->\"}]";
        let fake = FakeTracker::answering([Answer::ok(listing), Answer::ok("")]);
        plain(&fake)
            .upsert_status_comment(REPO, 6, "rawphp", "s")
            .unwrap();
        assert_eq!(
            fake.calls(),
            vec![
                list_comments_call(),
                comment_call("6", "s\n\n<!-- softfac:status -->"),
            ]
        );
    }

    #[test]
    fn the_status_comment_is_found_on_a_later_page() {
        // `gh api --paginate` may print each page as its own array.
        let pages = "[{\"id\":1,\"body\":\"a\"}][{\"id\":2,\"body\":\"b\"},{\"id\":3,\"user\":{\"login\":\"rawphp\"},\"body\":\"state\\n\\n<!-- softfac:status -->\"}]";
        let fake = FakeTracker::answering([Answer::ok(pages), Answer::ok("{}")]);
        plain(&fake)
            .upsert_status_comment(REPO, 6, "rawphp", "x")
            .unwrap();
        assert_eq!(fake.calls()[1][3], "repos/rawphp/softfac/issues/comments/3");
    }

    #[test]
    fn status_comment_errors() {
        let spawn = FakeTracker::new();
        assert!(matches!(
            plain(&spawn)
                .upsert_status_comment(REPO, 6, "rawphp", "x")
                .unwrap_err(),
            TrackerError::Other {
                kind: OtherKind::NotStarted,
                ..
            }
        ));

        let oops = FakeTracker::answering([Answer::ok("oops")]);
        let error = plain(&oops)
            .upsert_status_comment(REPO, 6, "rawphp", "x")
            .unwrap_err();
        assert!(is_parse(&error), "{error}");
        assert_eq!(oops.calls().len(), 1);

        let empty = FakeTracker::answering([Answer::ok("")]);
        let error = plain(&empty)
            .upsert_status_comment(REPO, 6, "rawphp", "x")
            .unwrap_err();
        assert!(is_parse(&error), "{error}");
        let message = error.to_string();
        assert!(
            message.starts_with(
                "`gh api repos/rawphp/softfac/issues/6/comments?per_page=100 --paginate`"
            ),
            "{message}"
        );
        assert!(message.contains("output was empty"), "{message}");

        let failing = FakeTracker::answering([Answer::ok("[]"), Answer::fail(1, "rate limited")]);
        let error = plain(&failing)
            .upsert_status_comment(REPO, 6, "rawphp", "x")
            .unwrap_err();
        assert!(is_failed(&error), "{error}");
        assert!(error.to_string().contains("rate limited"), "{error}");
    }

    #[test]
    fn the_status_comment_surfaces_a_failing_patch() {
        let listing = "[{\"id\":12,\"user\":{\"login\":\"rawphp\"},\"body\":\"state: triage\\n\\n<!-- softfac:status -->\"}]";
        let fake =
            FakeTracker::answering([Answer::ok(listing), Answer::fail(1, "validation failed")]);
        let error = plain(&fake)
            .upsert_status_comment(REPO, 6, "rawphp", "s")
            .unwrap_err();
        assert!(is_failed(&error), "{error}");
        let message = error.to_string();
        assert!(message.contains("validation failed"), "{message}");
        assert!(message.starts_with("`gh api -X PATCH "), "{message}");
        assert_eq!(fake.calls().len(), 2);
    }

    #[test]
    fn comment_on_pr_appends_the_marker() {
        let fake = FakeTracker::answering([Answer::ok("")]);
        plain(&fake).comment_on_pr(REPO, 9, "gate passed").unwrap();
        assert_eq!(
            fake.calls(),
            vec![args(&[
                "pr",
                "comment",
                "9",
                "--repo",
                REPO,
                "--body",
                "gate passed\n\n<!-- softfac -->",
            ])]
        );

        let failed = FakeTracker::new();
        assert!(matches!(
            plain(&failed).comment_on_pr(REPO, 9, "x").unwrap_err(),
            TrackerError::Other {
                kind: OtherKind::NotStarted,
                ..
            }
        ));
    }

    #[test]
    fn issue_comments_reads_every_page_with_the_exact_call() {
        let pages = "[{\"id\":1,\"user\":{\"login\":\"rawphp\"},\"author_association\":\"MEMBER\",\"created_at\":\"2026-10-04T10:13:16Z\",\"body\":\"a\"}][{\"id\":2,\"user\":null,\"body\":null},{\"id\":3}]";
        let fake = FakeTracker::answering([Answer::ok(pages)]);
        let comments = fake.issue_comments(REPO, 6).unwrap();
        assert_eq!(fake.calls(), vec![list_comments_call()]);
        assert_eq!(
            comments,
            vec![
                IssueComment {
                    id: 1,
                    author: Some("rawphp".into()),
                    body: "a".into(),
                },
                IssueComment {
                    id: 2,
                    author: None,
                    body: String::new(),
                },
                IssueComment {
                    id: 3,
                    author: None,
                    body: String::new(),
                },
            ]
        );
    }

    // -----------------------------------------------------------------------------------------
    // Hiding what a comment carries.
    // -----------------------------------------------------------------------------------------

    /// A hider that knows one secret, in place of the store-backed hider the run code will pass.
    fn hider() -> Hider {
        Arc::new(|text: &str| text.replace("hunter2", "[known]"))
    }

    const KEY: &str = "sk9f8e7d6c5b4a39281706f5e4d3c2b1a0";

    #[test]
    fn every_comment_body_goes_through_the_trackers_hider_and_then_for_comment() {
        let body = format!("pass hunter2 and key {KEY} at https://user:pw@example.com/x");
        let shown = for_comment(&body.replace("hunter2", "[known]"));
        assert!(!shown.contains("hunter2") && !shown.contains(KEY) && !shown.contains("pw@"));

        let fake = Arc::new(FakeTracker::new());
        let tracker = Hiding::new(fake.clone(), hider());
        fake.then(Answer::ok(""))
            .then(Answer::ok(""))
            .then(Answer::ok("[]"))
            .then(Answer::ok(""))
            .then(Answer::ok(
                "[{\"id\":77,\"user\":{\"login\":\"rawphp\"},\"body\":\"old\\n\\n<!-- softfac:status -->\"}]",
            ))
            .then(Answer::ok(""));
        tracker.post_comment(REPO, 6, &body).unwrap();
        tracker.comment_on_pr(REPO, 9, &body).unwrap();
        tracker
            .upsert_status_comment(REPO, 6, "rawphp", &body)
            .unwrap();
        tracker
            .upsert_status_comment(REPO, 6, "rawphp", &body)
            .unwrap();
        let calls = fake.calls();
        assert_eq!(calls[0], comment_call("6", &format!("{shown}\n\n{MARKER}")));
        assert_eq!(
            calls[1],
            args(&[
                "pr",
                "comment",
                "9",
                "--repo",
                REPO,
                "--body",
                &format!("{shown}\n\n{MARKER}"),
            ])
        );
        assert_eq!(
            calls[3],
            comment_call("6", &format!("{shown}\n\n{STATUS_MARKER}"))
        );
        assert_eq!(
            calls[5],
            patch_call("77", &format!("{shown}\n\n{STATUS_MARKER}"))
        );
    }

    #[test]
    fn the_hider_runs_before_for_comment_so_a_known_secret_goes_whole() {
        // `for_comment` alone would hide only the long run, and leave `Xy9.Kq2.` to be posted.
        const SECRET: &str = "Xy9.Kq2.abcdefghijklmnopqrstuvwx12";
        assert!(for_comment(SECRET).contains("Xy9.Kq2."));
        let fake = Arc::new(FakeTracker::new());
        let tracker = Hiding::new(
            fake.clone(),
            Arc::new(|text: &str| text.replace(SECRET, "[known]")),
        );
        fake.then(Answer::ok(""))
            .then(Answer::ok(""))
            .then(Answer::ok("[]"))
            .then(Answer::ok(""))
            .then(Answer::ok(
                "[{\"id\":77,\"user\":{\"login\":\"rawphp\"},\"body\":\"old\\n\\n<!-- softfac:status -->\"}]",
            ))
            .then(Answer::ok(""));
        let body = format!("the key is {SECRET}.");
        tracker.post_comment(REPO, 6, &body).unwrap();
        tracker.comment_on_pr(REPO, 9, &body).unwrap();
        tracker
            .upsert_status_comment(REPO, 6, "rawphp", &body)
            .unwrap();
        tracker
            .upsert_status_comment(REPO, 6, "rawphp", &body)
            .unwrap();
        let posted: Vec<String> = fake
            .calls()
            .into_iter()
            .filter(|call| call[0] != "api" || call[1] == "-X")
            .map(|call| call.last().unwrap().trim_start_matches("body=").to_string())
            .collect();
        assert_eq!(posted.len(), 4);
        for body in posted {
            assert!(body.starts_with("the key is [known]."), "{body}");
            assert!(!body.contains("Xy9") && !body.contains("Kq2"), "{body}");
        }
    }

    #[test]
    fn a_pull_request_url_stays_whole_through_the_hider_and_for_comment() {
        let fake = Arc::new(FakeTracker::answering([Answer::ok("")]));
        let tracker = Hiding::new(fake.clone(), hider());
        tracker
            .post_comment(
                REPO,
                6,
                "hunter2 https://github.com/rawphp/softfac/pull/123",
            )
            .unwrap();
        assert_eq!(
            fake.calls()[0][6],
            format!("[known] https://github.com/rawphp/softfac/pull/123\n\n{MARKER}")
        );
    }

    // -----------------------------------------------------------------------------------------
    // Pull requests: ready and draft.
    // -----------------------------------------------------------------------------------------

    fn ready_call() -> Vec<String> {
        args(&["pr", "ready", "9", "--repo", REPO])
    }

    fn undo_call() -> Vec<String> {
        let mut call = ready_call();
        call.push("--undo".into());
        call
    }

    fn pr_view(state: &str, draft: bool) -> String {
        format!(
            "{{\"createdAt\":\"2026-10-29T05:50:00Z\",\"baseRefName\":\"main\",\"headRefName\":\"x\",\"headRefOid\":\"{HEAD_SHA}\",\"isCrossRepository\":false,\"isDraft\":{draft},\"mergeable\":\"MERGEABLE\",\"number\":9,\"state\":\"{state}\"}}"
        )
    }

    fn pr_view_call(number: &str) -> Vec<String> {
        args(&[
            "pr",
            "view",
            number,
            "--repo",
            REPO,
            "--json",
            "number,isDraft,state,headRefName,isCrossRepository,baseRefName,headRefOid,mergeable,createdAt",
        ])
    }

    #[test]
    fn mark_pr_ready_runs_gh_pr_ready() {
        let fake = FakeTracker::answering([Answer::ok("")]);
        fake.mark_pr_ready(REPO, 9).unwrap();
        assert_eq!(fake.calls(), vec![ready_call()]);

        let failed = FakeTracker::answering([Answer::fail(1, "not a draft")]);
        let error = failed.mark_pr_ready(REPO, 9).unwrap_err();
        assert!(is_failed(&error), "{error}");
    }

    #[test]
    fn mark_pr_ready_on_a_pr_that_is_already_ready_is_success() {
        // What `gh` 2.102 does: it warns on stderr and exits 0.
        let stderr = "! Pull request rawphp/softfac#9 is already \"ready for review\"\n";
        let warned = FakeTracker::answering([Answer::ok_warning("", stderr)]);
        warned.mark_pr_ready(REPO, 9).unwrap();

        // Should a version exit non-zero for it, the PR is still ready: that is success too.
        let failed = FakeTracker::answering([Answer::fail(1, stderr)]);
        failed.mark_pr_ready(REPO, 9).unwrap();
        assert_eq!(failed.calls(), vec![ready_call()]);
    }

    #[test]
    fn mark_pr_ready_still_fails_for_a_closed_pr_or_any_other_error() {
        for stderr in [
            "! Pull request rawphp/softfac#9 is closed. Only draft pull requests can be marked as \"ready for review\"\n",
            "GraphQL: Resource not accessible by integration\n",
            "",
        ] {
            let fake = FakeTracker::answering([Answer::fail(1, stderr)]);
            let error = fake.mark_pr_ready(REPO, 9).unwrap_err();
            assert!(is_failed(&error), "{stderr}: {error}");
        }
    }

    #[test]
    fn mark_pr_draft_runs_gh_pr_ready_undo() {
        let fake = FakeTracker::answering([Answer::ok("")]);
        fake.mark_pr_draft(REPO, 9).unwrap();
        assert_eq!(fake.calls(), vec![undo_call()]);

        let failed = FakeTracker::answering([Answer::fail(
            1,
            "GraphQL: Resource not accessible by integration\n",
        )]);
        let error = failed.mark_pr_draft(REPO, 9).unwrap_err();
        assert!(is_failed(&error), "{error}");
    }

    #[test]
    fn mark_pr_draft_on_a_pr_that_is_already_a_draft_is_success() {
        // What `gh` does for `--undo` on a draft: it warns on stderr and exits 0. Should a
        // version exit non-zero for it, the PR is still a draft: that is success too.
        let stderr = "! Pull request rawphp/softfac#9 is already \"in draft\"\n";
        let warned = FakeTracker::answering([Answer::ok_warning("", stderr)]);
        warned.mark_pr_draft(REPO, 9).unwrap();

        let failed = FakeTracker::answering([Answer::fail(1, stderr)]);
        failed.mark_pr_draft(REPO, 9).unwrap();
        assert_eq!(failed.calls(), vec![undo_call()]);
    }

    #[test]
    fn mark_pr_draft_on_a_pr_that_is_closed_or_merged_is_done_whatever_gh_says() {
        // The words of the refusal are not relied on: the pull request's state decides.
        for state in ["CLOSED", "MERGED"] {
            for stderr in [
                "! Pull request rawphp/softfac#9 is closed. Only open pull requests can be converted to draft\n",
                "GraphQL: Something unforeseen\n",
            ] {
                let fake = FakeTracker::answering([
                    Answer::fail(1, stderr),
                    Answer::ok(pr_view(state, false)),
                ]);
                fake.mark_pr_draft(REPO, 9).unwrap();
                assert_eq!(
                    fake.calls(),
                    vec![undo_call(), pr_view_call("9")],
                    "{state}: {stderr}"
                );
            }
        }
    }

    #[test]
    fn mark_pr_draft_reads_the_state_after_any_failure_not_only_a_non_zero_exit() {
        // A timed out, rate limited or oversized undo on a PR that is closed by now is done too.
        for first in [
            Answer::TimedOut,
            Answer::fail(1, "gh: API rate limit exceeded (HTTP 403)"),
            Answer::TooLarge,
        ] {
            let fake = FakeTracker::answering([first, Answer::ok(pr_view("CLOSED", false))]);
            fake.mark_pr_draft(REPO, 9).unwrap();
            assert_eq!(fake.calls(), vec![undo_call(), pr_view_call("9")]);
        }
        // On one that is still open the original error comes back, whatever kind it was.
        let open = FakeTracker::answering([Answer::TimedOut, Answer::ok(pr_view("OPEN", false))]);
        assert!(matches!(
            open.mark_pr_draft(REPO, 9).unwrap_err(),
            TrackerError::Timeout { .. }
        ));
    }

    #[test]
    fn mark_pr_draft_that_fails_on_an_open_pr_or_cannot_read_it_is_the_original_error() {
        // Open: the failure is real.
        let open = FakeTracker::answering([
            Answer::fail(1, "GraphQL: Resource not accessible by integration\n"),
            Answer::ok(pr_view("OPEN", false)),
        ]);
        let error = open.mark_pr_draft(REPO, 9).unwrap_err();
        assert!(is_failed(&error), "{error}");
        assert!(error.to_string().contains("not accessible"), "{error}");

        // The read fails too: still the original error.
        let unread = FakeTracker::answering([
            Answer::fail(1, "GraphQL: Resource not accessible by integration\n"),
            Answer::fail(1, "HTTP 502: Bad Gateway"),
        ]);
        let error = unread.mark_pr_draft(REPO, 9).unwrap_err();
        assert!(is_failed(&error), "{error}");
        assert!(error.to_string().contains("not accessible"), "{error}");

        // One that is gone: nothing to draft either.
        let gone = FakeTracker::answering([
            Answer::fail(1, "boom"),
            Answer::fail(
                1,
                "GraphQL: Could not resolve to a PullRequest with the number of 9.",
            ),
        ]);
        let error = gone.mark_pr_draft(REPO, 9).unwrap_err();
        assert!(is_failed(&error), "{error}");
        assert!(error.to_string().contains("boom"), "{error}");
    }

    // -----------------------------------------------------------------------------------------
    // Claims.
    // -----------------------------------------------------------------------------------------

    fn run() -> RunId {
        RunId::new(REPO, 6, 1).unwrap()
    }

    fn machine() -> MachineId {
        MachineId(MACHINE.parse().unwrap())
    }

    fn target() -> Target {
        Target::Machine(machine())
    }

    fn vims() -> Target {
        Target::Host(HostKind::Vims)
    }

    fn comment(id: u64, author: Option<&str>, body: &str) -> IssueComment {
        IssueComment {
            id,
            author: author.map(str::to_string),
            body: body.into(),
        }
    }

    fn foundry_claim(id: u64, author: &str) -> IssueComment {
        comment(id, Some(author), &claim_comment(&run(), &target()))
    }

    fn runner_claim(id: u64, author: &str) -> IssueComment {
        comment(
            id,
            Some(author),
            "Claimed by machine mac-mini.\n\n<!-- softfac:claim mac-mini -->",
        )
    }

    fn ours() -> Option<Claim> {
        Some(Claim::Foundry {
            run: run(),
            target: target(),
        })
    }

    #[test]
    fn a_claim_comment_has_foundrys_marker_as_its_last_line() {
        let body = claim_comment(&run(), &target());
        assert_eq!(
            body.lines().last().unwrap(),
            format!("<!-- foundry:claim rawphp__softfac__6__1 {MACHINE} -->")
        );
        assert_eq!(
            body,
            format!(
                "Claimed by Foundry.\n\n<!-- foundry:claim rawphp__softfac__6__1 {MACHINE} -->"
            )
        );
    }

    #[test]
    fn a_host_claim_comment_names_the_kind_and_is_foundrys_claim() {
        let body = claim_comment(&run(), &vims());
        assert_eq!(
            body.lines().last().unwrap(),
            "<!-- foundry:claim rawphp__softfac__6__1 vims -->"
        );
        let comments = [comment(5, Some("rawphp"), &body)];
        assert_eq!(
            newest_claim(&comments, "rawphp"),
            Some(Claim::Foundry {
                run: run(),
                target: vims()
            })
        );
        // The machine form still names a machine.
        assert_eq!(
            newest_claim(&[foundry_claim(6, "rawphp")], "rawphp"),
            Some(Claim::Foundry {
                run: run(),
                target: Target::Machine(machine())
            })
        );
    }

    #[test]
    fn a_foundry_marker_naming_a_kind_this_build_does_not_know_is_foreign() {
        let body = format!(
            "Claimed by Foundry.\n\n<!-- foundry:claim {} kubernetes -->",
            run()
        );
        let comments = [comment(8, Some("rawphp"), &body)];
        assert_eq!(
            newest_claim(&comments, "rawphp"),
            Some(Claim::Foreign { comment_id: 8 })
        );
    }

    #[test]
    fn a_posted_claim_is_the_claim_comment_with_its_marker_whole_after_hiding() {
        let fake = Arc::new(FakeTracker::answering([Answer::ok("")]));
        let tracker = Hiding::new(fake.clone(), hider());
        tracker.post_claim(REPO, 6, &run(), &target()).unwrap();
        assert_eq!(
            fake.calls(),
            vec![comment_call("6", &claim_comment(&run(), &target()))]
        );
        // `for_comment` alone would hide the machine id, a long word of letters and digits.
        assert!(!for_comment(MACHINE).contains(MACHINE));
    }

    #[test]
    fn foundrys_own_claim_comment_is_foundrys_claim() {
        let comments = [foundry_claim(5, "rawphp")];
        assert_eq!(newest_claim(&comments, "rawphp"), ours());
    }

    #[test]
    fn the_login_matches_ignoring_ascii_case() {
        let comments = [foundry_claim(5, "RawPHP")];
        assert_eq!(newest_claim(&comments, "rawphp"), ours());
    }

    #[test]
    fn a_comment_with_foundrys_marker_written_by_another_login_is_not_foundrys_claim() {
        // Another login's marker counts for nothing, whoever wrote it.
        let alone = [foundry_claim(5, "someone-else")];
        assert_eq!(newest_claim(&alone, "rawphp"), None);
        // Written after Foundry's own claim, it takes nothing from it.
        let after = [foundry_claim(5, "rawphp"), foundry_claim(6, "mallory")];
        assert_eq!(newest_claim(&after, "rawphp"), ours());
        // Nor does a deleted account's.
        let ghost = [comment(7, None, &claim_comment(&run(), &target()))];
        assert_eq!(newest_claim(&ghost, "rawphp"), None);
    }

    #[test]
    fn the_runners_softfac_claim_comment_is_foreign() {
        // The runner and Foundry act as one GitHub login during the cutover.
        let comments = [runner_claim(9, "rawphp")];
        assert_eq!(
            newest_claim(&comments, "rawphp"),
            Some(Claim::Foreign { comment_id: 9 })
        );
        // Another login's runner marker counts for nothing, as another login's Foundry one.
        let elsewhere = [foundry_claim(5, "rawphp"), runner_claim(9, "tomkaczocha")];
        assert_eq!(newest_claim(&elsewhere, "rawphp"), ours());
    }

    #[test]
    fn of_two_claim_comments_the_newest_counts() {
        let runner_then_foundry = [runner_claim(9, "rawphp"), foundry_claim(12, "rawphp")];
        assert_eq!(newest_claim(&runner_then_foundry, "rawphp"), ours());

        let foundry_then_runner = [foundry_claim(9, "rawphp"), runner_claim(12, "rawphp")];
        assert_eq!(
            newest_claim(&foundry_then_runner, "rawphp"),
            Some(Claim::Foreign { comment_id: 12 })
        );

        // Newest is the higher comment id, whatever order the list is in.
        let later_run = RunId::new(REPO, 6, 2).unwrap();
        let newer = comment(30, Some("rawphp"), &claim_comment(&later_run, &target()));
        let comments = [newer, foundry_claim(20, "rawphp")];
        assert_eq!(
            newest_claim(&comments, "rawphp"),
            Some(Claim::Foundry {
                run: later_run,
                target: target()
            })
        );
    }

    #[test]
    fn a_marker_that_is_not_the_last_line_is_no_claim() {
        let quoted = format!("{}\n\nquoted above", claim_comment(&run(), &target()));
        let comments = [
            comment(5, Some("rawphp"), &quoted),
            comment(
                6,
                Some("rawphp"),
                "see <!-- softfac:claim mac-mini --> here",
            ),
            comment(7, Some("rawphp"), "a person's comment"),
        ];
        assert_eq!(newest_claim(&comments, "rawphp"), None);
        // Trailing whitespace after the marker still leaves it the last line.
        let trailing = [comment(
            8,
            Some("rawphp"),
            &format!("{}\n\n", claim_comment(&run(), &target())),
        )];
        assert_eq!(newest_claim(&trailing, "rawphp"), ours());
    }

    #[test]
    fn a_runner_marker_that_names_no_one_machine_is_no_claim() {
        for marker in [
            "<!-- softfac:claim  -->",
            "<!-- softfac:claim a b -->",
            "<!-- softfac:claim -->",
        ] {
            let comments = [comment(5, Some("rawphp"), &format!("x\n\n{marker}"))];
            assert_eq!(newest_claim(&comments, "rawphp"), None, "{marker}");
        }
    }

    #[test]
    fn a_foundry_marker_that_does_not_parse_is_foreign() {
        for marker in [
            "<!-- foundry:claim not-a-run 6f1c2a8e-3b4d-4e5f-9a0b-1c2d3e4f5a6b -->",
            "<!-- foundry:claim rawphp__softfac__6__1 not-a-machine -->",
            "<!-- foundry:claim rawphp__softfac__6__1 -->",
            "<!-- foundry:claim rawphp__softfac__6__1 6f1c2a8e-3b4d-4e5f-9a0b-1c2d3e4f5a6b extra -->",
            "<!-- foundry:claim rawphp__softfac__6__1 6F1C2A8E-3B4D-4E5F-9A0B-1C2D3E4F5A6B -->",
        ] {
            let comments = [comment(5, Some("rawphp"), &format!("x\n\n{marker}"))];
            assert_eq!(
                newest_claim(&comments, "rawphp"),
                Some(Claim::Foreign { comment_id: 5 }),
                "{marker}"
            );
        }
    }

    #[test]
    fn no_claim_comment_is_no_claim() {
        assert_eq!(newest_claim(&[], "rawphp"), None);
        let comments = [
            comment(1, Some("rawphp"), "hello\n\n<!-- softfac -->"),
            comment(2, Some("rawphp"), "s\n\n<!-- softfac:status -->"),
        ];
        assert_eq!(newest_claim(&comments, "rawphp"), None);
    }
}
