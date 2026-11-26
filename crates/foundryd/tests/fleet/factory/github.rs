//! Fleet tests of the factory against a fake GitHub.
//!
//! [`FakeTracker`] is GitHub as the daemons of one fleet test see it: one repo, its issues,
//! labels, comments and pull requests kept in memory, shared by every daemon. It answers the
//! `gh` calls the factory makes by their arguments. What GitHub reads from git, a branch's tip,
//! a file at a commit or a pull request's head, it reads from the project's `origin`, the local
//! bare repo the daemons push to, so a pull request opens once its branch is pushed there.
//! A call it does not know is recorded as unknown and fails, so a test can say none was made.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use foundryd::factory::github::{OtherKind, Tracker, TrackerError};
use serde_json::{Value, json};

/// The factory's labels, as GitHub has them on the repo.
const LABELS: [&str; 7] = [
    "factory:proposed",
    "factory:ready",
    "factory:needs-info",
    "factory:triage",
    "factory:building",
    "factory:pr-open",
    "factory:stuck",
];

/// GitHub's UTC form of `secs` since the epoch: `YYYY-MM-DDTHH:MM:SSZ`.
pub fn utc(secs: u64) -> String {
    let days = secs / 86_400;
    let rest = secs % 86_400;
    // Howard Hinnant's days-to-civil, for the proleptic Gregorian calendar.
    let z = i64::try_from(days).unwrap() + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        (rest / 60) % 60,
        rest % 60
    )
}

/// Now, in GitHub's UTC form.
pub fn now() -> String {
    utc(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs())
}

/// One issue.
#[derive(Clone, Debug)]
pub struct Issue {
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
    pub assignees: Vec<String>,
    pub closed: bool,
    /// Each label applied: the label, when, and by whom.
    pub labeled: Vec<(String, String, String)>,
}

/// One comment, on an issue or a pull request: GitHub keeps both as issue comments.
#[derive(Clone, Debug)]
pub struct Comment {
    pub id: u64,
    /// The issue or pull request it is on.
    pub on: u64,
    pub author: String,
    pub body: String,
    pub created_at: String,
    pub updated_at: String,
}

/// One pull request, open once its head branch is on `origin`.
#[derive(Clone, Debug)]
pub struct Pull {
    pub head: String,
    /// The issue it closes.
    pub closes: u64,
    pub draft: bool,
    /// When it was first seen open.
    pub created_at: Option<String>,
}

#[derive(Default)]
struct Hub {
    issues: BTreeMap<u64, Issue>,
    comments: Vec<Comment>,
    pulls: BTreeMap<u64, Pull>,
    next_comment: u64,
    unknown: Vec<String>,
}

/// GitHub for one repo, shared by the daemons of a fleet test.
pub struct FakeTracker {
    /// `owner/name`.
    repo: String,
    /// The login the factory acts as: every comment and edit made through `gh` is by it.
    login: String,
    /// The bare repo that stands for the repo's git.
    origin: PathBuf,
    hub: Mutex<Hub>,
}

impl FakeTracker {
    /// GitHub for `repo`, whose git is the bare repo at `origin`, as `login` acts on it.
    pub fn new(repo: &str, login: &str, origin: PathBuf) -> Self {
        Self {
            repo: repo.into(),
            login: login.into(),
            origin,
            hub: Mutex::new(Hub {
                next_comment: 1000,
                ..Hub::default()
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Hub> {
        self.hub.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Opens issue `number`, with `title` and `body` and no label.
    pub fn issue(&self, number: u64, title: &str, body: &str) {
        self.lock().issues.insert(
            number,
            Issue {
                title: title.into(),
                body: body.into(),
                labels: Vec::new(),
                assignees: Vec::new(),
                closed: false,
                labeled: Vec::new(),
            },
        );
    }

    /// `by` applies `label` to issue `number`, now.
    pub fn label(&self, number: u64, label: &str, by: &str) {
        let mut hub = self.lock();
        let issue = hub.issues.get_mut(&number).expect("an issue");
        if !issue.labels.iter().any(|l| l == label) {
            issue.labels.push(label.into());
        }
        issue.labeled.push((label.into(), now(), by.into()));
    }

    /// A pull request numbered `number` from `head`, closing issue `closes`: a draft, open
    /// from when `head` is on `origin`.
    pub fn pull(&self, number: u64, head: &str, closes: u64) {
        self.lock().pulls.insert(
            number,
            Pull {
                head: head.into(),
                closes,
                draft: true,
                created_at: None,
            },
        );
    }

    /// `author` comments `body` on issue or pull request `on`, now. Answers its id.
    pub fn comment_as(&self, on: u64, author: &str, body: &str) -> u64 {
        let mut hub = self.lock();
        Self::add_comment(&mut hub, on, author, body)
    }

    fn add_comment(hub: &mut Hub, on: u64, author: &str, body: &str) -> u64 {
        let id = hub.next_comment;
        hub.next_comment += 1;
        let at = now();
        hub.comments.push(Comment {
            id,
            on,
            author: author.into(),
            body: body.into(),
            created_at: at.clone(),
            updated_at: at,
        });
        id
    }

    /// The labels issue `number` has now.
    pub fn labels(&self, number: u64) -> Vec<String> {
        self.lock().issues[&number].labels.clone()
    }

    /// Every comment, on every issue and pull request, in the order made.
    pub fn comments(&self) -> Vec<Comment> {
        self.lock().comments.clone()
    }

    /// The bodies of the comments on `on`, in the order made.
    pub fn comments_on(&self, on: u64) -> Vec<String> {
        self.comments()
            .into_iter()
            .filter(|c| c.on == on)
            .map(|c| c.body)
            .collect()
    }

    /// Whether pull request `number` is a draft.
    pub fn is_draft(&self, number: u64) -> bool {
        self.lock().pulls[&number].draft
    }

    /// The calls this fake does not know, each as one line.
    pub fn unknown(&self) -> Vec<String> {
        self.lock().unknown.clone()
    }

    /// The full commit id of `branch` on `origin`, when it is there.
    pub fn tip(&self, branch: &str) -> Option<String> {
        let out = Command::new("git")
            .args(["rev-parse", "--verify", "--quiet"])
            .arg(format!("refs/heads/{branch}^{{commit}}"))
            .env("GIT_DIR", &self.origin)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }

    /// The text of `path` at commit `sha` on `origin`, when it is there.
    fn file_at(&self, sha: &str, path: &str) -> Option<Vec<u8>> {
        let out = Command::new("git")
            .args(["cat-file", "blob", &format!("{sha}:{path}")])
            .env("GIT_DIR", &self.origin)
            .output()
            .ok()?;
        out.status.success().then_some(out.stdout)
    }

    /// The open pull requests: those whose head is on `origin`, each stamped open the first
    /// time it is seen so.
    fn open_pulls(&self, hub: &mut Hub) -> Vec<(u64, Pull, String)> {
        let mut open = Vec::new();
        for (number, pull) in hub.pulls.iter_mut() {
            if let Some(sha) = self.tip(&pull.head) {
                pull.created_at.get_or_insert_with(now);
                open.push((*number, pull.clone(), sha));
            }
        }
        open
    }

    fn url(&self, number: u64) -> String {
        format!("https://github.com/{}/pull/{number}", self.repo)
    }

    fn failed(args: &[String], message: &str) -> TrackerError {
        TrackerError::Other {
            kind: OtherKind::Failed,
            message: format!("`gh {}` failed: {message}", args.join(" ")),
        }
    }

    fn not_found(args: &[String]) -> TrackerError {
        TrackerError::NotFound {
            command: format!("gh {}", args.join(" ")),
            status: 1,
            stderr: "HTTP 404: Not Found (https://api.github.com/)".into(),
        }
    }

    fn values(args: &[String], flag: &str) -> Vec<String> {
        args.windows(2)
            .filter(|pair| pair[0] == flag)
            .map(|pair| pair[1].clone())
            .collect()
    }

    fn field(args: &[String], flag: &str, name: &str) -> Option<String> {
        Self::values(args, flag)
            .into_iter()
            .find_map(|value| value.strip_prefix(&format!("{name}=")).map(str::to_owned))
    }

    fn actor(login: &str) -> Value {
        json!({"__typename": "User", "login": login})
    }

    fn no_edits() -> Value {
        json!({"totalCount": 0, "nodes": []})
    }

    fn issue_json(number: u64, issue: &Issue) -> Value {
        json!({
            "number": number,
            "title": issue.title,
            "body": issue.body,
            "labels": issue.labels.iter().map(|n| json!({"name": n})).collect::<Vec<_>>(),
            "assignees": issue.assignees.iter().map(|n| json!({"login": n})).collect::<Vec<_>>(),
            "state": if issue.closed { "CLOSED" } else { "OPEN" },
            "author": {"login": "tom"},
        })
    }

    fn comment_row(c: &Comment) -> Value {
        json!({
            "id": c.id,
            "node_id": format!("IC_{}", c.id),
            "user": {"login": c.author},
            "body": c.body,
            "created_at": c.created_at,
            "updated_at": c.updated_at,
        })
    }

    fn pull_json(&self, number: u64, pull: &Pull, sha: &str) -> Value {
        json!({
            "number": number,
            "url": self.url(number),
            "isDraft": pull.draft,
            "state": "OPEN",
            "headRefName": pull.head,
            "baseRefName": "main",
            "isCrossRepository": false,
            "headRefOid": sha,
            "mergeable": "MERGEABLE",
            "createdAt": pull.created_at,
            "author": Self::actor(&self.login),
        })
    }

    fn answer(&self, args: &[String]) -> Result<String, TrackerError> {
        let mut hub = self.lock();
        let joined = args.join(" ");
        let word = |at: usize| args.get(at).map(String::as_str);
        let number = |at: usize| word(at).and_then(|n| n.parse::<u64>().ok());
        let answered = match (word(0), word(1)) {
            (Some("issue"), Some("list")) => {
                let label = Self::values(args, "--label").pop().unwrap_or_default();
                let rows: Vec<Value> = hub
                    .issues
                    .iter()
                    .filter(|(_, issue)| !issue.closed && issue.labels.contains(&label))
                    .map(|(n, issue)| Self::issue_json(*n, issue))
                    .collect();
                Some(Ok(Value::Array(rows).to_string()))
            }
            (Some("issue"), Some("view")) => {
                let n = number(2).unwrap_or(0);
                Some(match hub.issues.get(&n) {
                    Some(issue) => Ok(Self::issue_json(n, issue).to_string()),
                    None => Err(Self::failed(args, "Could not resolve to an issue")),
                })
            }
            (Some("issue"), Some("edit")) => {
                let n = number(2).unwrap_or(0);
                let login = self.login.clone();
                Some(match hub.issues.get_mut(&n) {
                    None => Err(Self::failed(args, "Could not resolve to an issue")),
                    Some(issue) => {
                        for label in Self::values(args, "--add-label") {
                            if !issue.labels.contains(&label) {
                                issue.labels.push(label.clone());
                            }
                            issue.labeled.push((label, now(), login.clone()));
                        }
                        for label in Self::values(args, "--remove-label") {
                            issue.labels.retain(|l| *l != label);
                        }
                        for who in Self::values(args, "--add-assignee") {
                            if !issue.assignees.contains(&who) {
                                issue.assignees.push(who);
                            }
                        }
                        for who in Self::values(args, "--remove-assignee") {
                            issue.assignees.retain(|l| *l != who);
                        }
                        Ok(String::new())
                    }
                })
            }
            (Some("issue" | "pr"), Some("comment")) => {
                let n = number(2).unwrap_or(0);
                let body = Self::values(args, "--body").pop().unwrap_or_default();
                let login = self.login.clone();
                Self::add_comment(&mut hub, n, &login, &body);
                Some(Ok(String::new()))
            }
            (Some("label"), Some("create")) => Some(Ok(String::new())),
            (Some("pr"), Some("ready")) => {
                let n = number(2).unwrap_or(0);
                let undo = args.iter().any(|a| a == "--undo");
                Some(match hub.pulls.get_mut(&n) {
                    Some(pull) => {
                        pull.draft = undo;
                        Ok(String::new())
                    }
                    None => Err(Self::failed(args, "Could not resolve to a PullRequest")),
                })
            }
            (Some("pr"), Some("view")) => {
                let n = number(2).unwrap_or(0);
                let open = self.open_pulls(&mut hub);
                Some(match open.iter().find(|(m, _, _)| *m == n) {
                    Some((_, pull, sha)) => Ok(self.pull_json(n, pull, sha).to_string()),
                    None => Err(Self::failed(
                        args,
                        "GraphQL: Could not resolve to a PullRequest with the number of 0.",
                    )),
                })
            }
            (Some("pr"), Some("list")) => {
                let head = Self::values(args, "--head").pop();
                let rows: Vec<Value> = self
                    .open_pulls(&mut hub)
                    .iter()
                    .filter(|(_, pull, _)| head.as_ref().is_none_or(|h| *h == pull.head))
                    .map(|(n, pull, sha)| self.pull_json(*n, pull, sha))
                    .collect();
                Some(Ok(Value::Array(rows).to_string()))
            }
            (Some("api"), Some("graphql")) => self.graphql(&mut hub, args),
            (Some("api"), Some("-X")) => {
                let path = word(3).unwrap_or_default();
                let id: u64 = path
                    .rsplit('/')
                    .next()
                    .and_then(|id| id.parse().ok())
                    .unwrap_or(0);
                let body = Self::field(args, "-f", "body").unwrap_or_default();
                Some(match hub.comments.iter_mut().find(|c| c.id == id) {
                    Some(c) => {
                        c.body = body;
                        c.updated_at = now();
                        Ok(String::new())
                    }
                    None => Err(Self::not_found(args)),
                })
            }
            (Some("api"), Some(path)) => self.rest(&mut hub, args, path),
            _ => None,
        };
        answered.unwrap_or_else(|| {
            hub.unknown.push(joined);
            Err(Self::failed(
                args,
                "the fake GitHub does not know this call",
            ))
        })
    }

    /// A REST read under `repos/`.
    fn rest(
        &self,
        hub: &mut Hub,
        args: &[String],
        path: &str,
    ) -> Option<Result<String, TrackerError>> {
        let rest = path.strip_prefix(&format!("repos/{}", self.repo))?;
        let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
        if rest.is_empty() {
            return Some(Ok(json!({"default_branch": "main"}).to_string()));
        }
        let parts: Vec<&str> = rest.trim_start_matches('/').split('/').collect();
        let answer = match parts.as_slice() {
            ["labels"] => {
                let rows: Vec<Value> = LABELS.iter().map(|n| json!({"name": n})).collect();
                Ok(Value::Array(rows).to_string())
            }
            ["git", "ref", "heads", branch @ ..] => match self.tip(&branch.join("/")) {
                Some(sha) => Ok(json!({"object": {"sha": sha, "type": "commit"}}).to_string()),
                None => Err(Self::not_found(args)),
            },
            ["contents", file @ ..] => {
                let sha = query.strip_prefix("ref=").unwrap_or_default();
                match self.file_at(sha, &file.join("/")) {
                    Some(bytes) => Ok(json!({
                        "type": "file",
                        "size": bytes.len(),
                        "encoding": "base64",
                        "content": b64(&bytes),
                    })
                    .to_string()),
                    None => Err(Self::not_found(args)),
                }
            }
            ["commits", _, "check-runs"] => Ok("[]".into()),
            ["issues", n, "comments"] => {
                let n: u64 = n.parse().unwrap_or(0);
                let since = query
                    .split('&')
                    .find_map(|pair| pair.strip_prefix("since="))
                    .unwrap_or("");
                let rows: Vec<Value> = hub
                    .comments
                    .iter()
                    .filter(|c| c.on == n && c.updated_at.as_str() >= since)
                    .map(Self::comment_row)
                    .collect();
                Ok(Value::Array(rows).to_string())
            }
            ["issues", _, "dependencies", "blocked_by"] => Ok("[]".into()),
            ["issues", n] => {
                let n: u64 = n.parse().unwrap_or(0);
                match hub.issues.get(&n) {
                    Some(issue) => {
                        let state = if issue.closed { "closed" } else { "open" };
                        Ok(json!({"number": n, "state": state}).to_string())
                    }
                    None => Err(Self::not_found(args)),
                }
            }
            _ => return None,
        };
        Some(answer)
    }

    /// A GraphQL query, told apart by what it asks for.
    fn graphql(&self, hub: &mut Hub, args: &[String]) -> Option<Result<String, TrackerError>> {
        let query = Self::field(args, "-f", "query").unwrap_or_default();
        let number: u64 = Self::field(args, "-F", "number")
            .and_then(|n| n.parse().ok())
            .unwrap_or(0);
        let answer = if query.contains("closedByPullRequestsReferences") {
            let nodes: Vec<Value> = self
                .open_pulls(hub)
                .iter()
                .filter(|(_, pull, _)| pull.closes == number)
                .map(|(n, pull, sha)| self.pull_json(*n, pull, sha))
                .collect();
            json!({"data": {"repository": {"issue": {"closedByPullRequestsReferences": {"nodes": nodes}}}}})
        } else if query.contains("timelineItems") {
            let issue = hub.issues.get(&number)?;
            let nodes: Vec<Value> = issue
                .labeled
                .iter()
                .map(|(label, at, by)| {
                    json!({
                        "__typename": "LabeledEvent",
                        "createdAt": at,
                        "actor": Self::actor(by),
                        "label": {"name": label},
                    })
                })
                .collect();
            json!({"data": {"repository": {"issue": {
                "timelineItems": {"totalCount": nodes.len(), "nodes": nodes},
                "lastEditedAt": null,
                "editor": null,
                "userContentEdits": Self::no_edits(),
            }}}})
        } else if query.contains("nodes(ids:") {
            json!({"data": {"nodes": []}})
        } else if query.contains("pullRequest(number:") {
            let nodes: Vec<Value> = hub
                .comments
                .iter()
                .filter(|c| c.on == number)
                .map(|c| {
                    json!({
                        "fullDatabaseId": c.id.to_string(),
                        "body": c.body,
                        "createdAt": c.created_at,
                        "author": Self::actor(&c.author),
                        "lastEditedAt": null,
                        "editor": null,
                        "userContentEdits": Self::no_edits(),
                    })
                })
                .collect();
            let reviews = json!({"totalCount": 0, "pageInfo": {"startCursor": null}, "nodes": []});
            let comments = json!({"totalCount": nodes.len(), "pageInfo": {"startCursor": null}, "nodes": nodes});
            if !hub.pulls.contains_key(&number) {
                json!({"data": {"repository": {"pullRequest": null}}})
            } else {
                json!({"data": {"repository": {"pullRequest": {"reviews": reviews, "comments": comments}}}})
            }
        } else if query.contains("issue(number:") && query.contains("body lastEditedAt") {
            let issue = hub.issues.get(&number)?;
            json!({"data": {"repository": {"issue": {
                "body": issue.body,
                "lastEditedAt": null,
                "editor": null,
                "userContentEdits": Self::no_edits(),
            }}}})
        } else {
            return None;
        };
        Some(Ok(answer.to_string()))
    }
}

impl Tracker for FakeTracker {
    fn gh(&self, args: &[String]) -> Result<String, TrackerError> {
        self.answer(args)
    }
}

/// `bytes` in standard base64, padded.
fn b64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk.len();
        let word = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for at in 0..4 {
            if at <= n {
                out.push(TABLE[((word >> (18 - 6 * at)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The labels of the factory's states that `labels` holds.
pub fn factory_labels(labels: &[String]) -> BTreeSet<String> {
    labels
        .iter()
        .filter(|l| l.starts_with("factory:"))
        .cloned()
        .collect()
}

#[test]
fn utc_is_githubs_form() {
    assert_eq!(utc(0), "1970-01-01T00:00:00Z");
    assert_eq!(utc(1_795_688_592), "2026-11-26T10:23:12Z");
    assert_eq!(utc(951_782_400), "2000-02-29T00:00:00Z");
}

#[test]
fn base64_pads() {
    assert_eq!(b64(b"f"), "Zg==");
    assert_eq!(b64(b"fo"), "Zm8=");
    assert_eq!(b64(b"foo"), "Zm9v");
}

/// A made-up key of 30 mixed letters and digits, held by no machine.
const KEY: &str = "Zq7Xw2Lk9Pm4Rt6Vb8Nc3Hj5Gf1Ds0";

/// A `github.com` commit link, whose id is as long as a key.
const LINK: &str =
    "https://github.com/acme/widgets/commit/0123456789abcdef0123456789abcdef01234567";

#[test]
fn a_token_and_a_made_up_key_in_gate_output_and_in_questions_reach_github_as_they_should() {
    use super::{Harness, Options, TOKEN, config, isolated};

    isolated(
        module_path!(),
        "a_token_and_a_made_up_key_in_gate_output_and_in_questions_reach_github_as_they_should",
        || async {
            // The command that fails prints both: they are in its output, not in its text.
            let h = Harness::start(Options {
                config: config(&[], &["cat {marks}/leak; exit 1"], 1),
                ..Options::default()
            })
            .await;
            std::fs::write(
                h.marks.join("leak"),
                format!("leaked {TOKEN} and {KEY}, see {LINK}\n"),
            )
            .unwrap();
            // Issue 12's gate prints both, is sent back, fails again and leaves the run stuck.
            h.github.pull(101, "fac/12-1", 12);
            h.script(
                12,
                r#"commit feature.txt 'the feature' 'add the feature'
git push -q origin HEAD
agent '- hook: {event: SessionStart}
- wait_prompt
- stop: {last_assistant_message: "Opened the pull request.\nFACTORY-DONE: https://github.com/acme/widgets/pull/101"}
- wait_prompt
- stop: {last_assistant_message: "Tried again.\nFACTORY-DONE: https://github.com/acme/widgets/pull/101"}
- wait_input'
"#,
            );
            h.ready(12, "Make the widgets faster");
            h.reaches(12, "factory:stuck").await;
            // Issue 14's session asks questions holding both.
            h.script(
                14,
                &format!(
                    r#"agent '- hook: {{event: SessionStart}}
- wait_prompt
- stop: {{last_assistant_message: "1. Is {TOKEN} the token to use, and {KEY} the key? See {LINK}\nFACTORY-STOP: needs-info"}}
- wait_input'
"#
                ),
            );
            h.ready(14, "Make the widgets cheaper");
            h.reaches(14, "factory:needs-info").await;

            let comments = h.github.comments();
            for comment in &comments {
                assert!(
                    !comment.body.contains(TOKEN),
                    "a comment holds the token: {comment:?}"
                );
            }
            let sent = h.sent();
            assert!(sent.len() >= 3, "{sent:?}");
            for message in &sent {
                assert!(
                    !message.text().contains(TOKEN),
                    "a message holds the token: {message:?}"
                );
            }
            let mut events = Vec::new();
            for issue in [12, 14] {
                events.extend(
                    h.events(issue, 1)
                        .iter()
                        .map(|e| serde_json::to_string(e).unwrap()),
                );
            }
            for event in &events {
                assert!(!event.contains(TOKEN), "an event holds the token: {event}");
            }
            // The gate output reaches the session in the failure sent back, and no comment.
            assert!(
                sent.iter()
                    .any(|m| m.text().contains("leaked") && m.text().contains(LINK)),
                "the gate output is sent back: {sent:?}"
            );
            for comment in &comments {
                assert!(
                    !comment.body.contains(KEY),
                    "a comment shows the key: {comment:?}"
                );
            }
            let asked: Vec<&Comment> = comments
                .iter()
                .filter(|c| c.on == 14 && c.body.contains("the token to use"))
                .collect();
            assert_eq!(asked.len(), 1, "the questions are posted: {comments:?}");
            assert!(
                asked[0].body.contains(LINK),
                "the link in the questions comes through whole: {}",
                asked[0].body
            );
            for issue in [12, 14] {
                assert!(
                    h.events(issue, 1)
                        .iter()
                        .any(|e| serde_json::to_string(e).unwrap().contains(KEY)),
                    "an event of #{issue} keeps the key whole"
                );
            }
            h.no_unknown_calls();
        },
    );
}
