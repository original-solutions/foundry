//! Factory records, factory settings and run ids.
//!
//! Factory records are kept and synced apart from P1's records, on a version per owner of their
//! own, and only on links whose two ends speak protocol 5. Each is written only by its owner,
//! and none is ever deleted. The factory settings are shared settings of their own, under the
//! keys [`SettingKey::Factory`](crate::settings::SettingKey::Factory) and
//! [`SettingKey::ProjectFactory`](crate::settings::SettingKey::ProjectFactory).

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::ids::{MachineId, ProjectId, SessionId, WorkspaceId};

/// One run: a repo, an issue in it, and which attempt at that issue this is. Shown and parsed
/// as `<owner>__<repo>__<issue>__<attempt>`, the runner's run id, which names its run file.
///
/// [`RunId::repo`] is lowercase `owner/name`, as [`RunId::new`] makes it. GitHub names are
/// case-insensitive, and so are macOS file systems, so one repo must be one run id. The fields
/// are private, so [`RunId::new`] and parsing, which both check the repo, are the only ways to
/// make one outside this module:
///
/// ```compile_fail,E0451
/// let id = foundry_proto::factory::RunId { repo: "Rawphp/SoftFac".into(), issue: 8, attempt: 1 };
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RunId {
    repo: String,
    issue: u64,
    attempt: u32,
}

const SHAPE: &str = "expected <owner>__<repo>__<issue>__<attempt>";
const REPO_REASON: &str = "the repo must be <owner>/<name>: the owner uses letters, digits and `-` plus single `_` between them, the name uses letters, digits, `.`, `_` and `-` and is not `.` or `..`";

impl RunId {
    /// A run id for `owner/name`, lowercased. Refuses a repo that is not a legal GitHub one.
    pub fn new(repo: &str, issue: u64, attempt: u32) -> Result<Self, String> {
        let repo = normalise_repo(repo).ok_or_else(|| refused(repo, REPO_REASON))?;
        Ok(Self {
            repo,
            issue,
            attempt,
        })
    }

    /// The repo, lowercase `owner/name`.
    pub fn repo(&self) -> &str {
        &self.repo
    }

    /// The issue number in the repo.
    pub fn issue(&self) -> u64 {
        self.issue
    }

    /// Which attempt at the issue this run is.
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// True when the repo is already in the form [`RunId::new`] makes, so the id names one
    /// file and cannot leave the directory it is written in. Only code in this module can build
    /// one that is not.
    fn is_valid(&self) -> bool {
        normalise_repo(&self.repo).as_deref() == Some(self.repo.as_str())
    }
}

fn refused(input: &str, reason: &str) -> String {
    format!("{input:?} is not a run id: {reason}")
}

/// Lowercase `owner/name` after checking both parts, lowercasing ASCII letters only: a Unicode
/// lowercase would fold U+212A (Kelvin) into a `k`. GitHub owners use `[A-Za-z0-9-]`, and
/// Enterprise Managed User logins add a `_`, so an owner may hold single underscores between
/// other characters: none first, none last, never `__`. That is what lets a parse end the owner
/// at the first `__`. Repo names use `[A-Za-z0-9._-]`.
fn normalise_repo(input: &str) -> Option<String> {
    let repo = input.to_ascii_lowercase();
    let (owner, name) = repo.split_once('/')?;
    let owner_ok = !owner.is_empty()
        && !owner.starts_with('_')
        && !owner.ends_with('_')
        && !owner.contains("__")
        && owner
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
    let name_ok = !name.is_empty()
        && name != "."
        && name != ".."
        && name.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        });
    (owner_ok && name_ok).then_some(repo)
}

/// A number the way the runner writes it: digits, no sign, no leading zero.
fn canonical<T: FromStr>(text: &str) -> Option<T> {
    let digits = !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
    let leading_zero = text.len() > 1 && text.starts_with('0');
    if digits && !leading_zero {
        text.parse().ok()
    } else {
        None
    }
}

/// A host that makes a run's workspaces itself, in place of a machine of the fleet.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HostKind {
    /// Vims.
    Vims,
}

impl HostKind {
    /// The kind's name, as a claim marker and the ledger write it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Vims => "vims",
        }
    }
}

impl fmt::Display for HostKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for HostKind {
    type Err = String;

    /// Parses exactly the spelling [`HostKind::as_str`] gives.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "vims" => Ok(Self::Vims),
            _ => Err(format!("{text:?} is not a host kind this build knows")),
        }
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (owner, name) = self.repo.split_once('/').unwrap_or((&self.repo, ""));
        write!(f, "{owner}__{name}__{}__{}", self.issue, self.attempt)
    }
}

impl FromStr for RunId {
    type Err = String;

    /// Parses `<owner>__<repo>__<issue>__<attempt>`, lowercasing the owner and repo. The owner
    /// never holds `__`, so it ends at the first `__`. The last two fields are numbers, so they
    /// are taken from the right. What is left in the middle is the repo name, which may itself
    /// hold `__`.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let bad = || refused(text, SHAPE);
        let (owner, rest) = text.split_once("__").ok_or_else(bad)?;
        let (rest, attempt) = rest.rsplit_once("__").ok_or_else(bad)?;
        let (name, issue) = rest.rsplit_once("__").ok_or_else(bad)?;
        let attempt = canonical(attempt).ok_or_else(bad)?;
        let issue = canonical(issue).ok_or_else(bad)?;
        let repo = format!("{owner}/{name}");
        let repo = normalise_repo(&repo).ok_or_else(|| refused(text, REPO_REASON))?;
        Ok(Self {
            repo,
            issue,
            attempt,
        })
    }
}

impl Serialize for RunId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if !self.is_valid() {
            return Err(serde::ser::Error::custom(refused(&self.repo, REPO_REASON)));
        }
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for RunId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// A factory record: written only by its owner, under the owner's factory version, which only
/// goes up and is apart from its version for P1's records. Written with the body's `kind` beside
/// the owner and version, as P1's records are.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FactoryRecord {
    pub owner: MachineId,
    pub version: u64,
    #[serde(flatten)]
    pub body: FactoryBody,
}

/// What a factory record holds, tagged by `kind`. A kind this build does not know is an error.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "body")]
pub enum FactoryBody {
    /// One step of a run.
    #[serde(rename = "run_event")]
    RunEvent(RunEvent),
    /// What the factory of one project looks like to the machine that wrote it.
    #[serde(rename = "factory_status")]
    Status(FactoryStatus),
    /// The GitHub logins the owner holds a token for, and whether it has the skill.
    #[serde(rename = "machine_held")]
    Held(MachineHeld),
    /// The newest turn of one of the owner's factory sessions.
    #[serde(rename = "session_turn")]
    Turn(SessionTurn),
}

impl FactoryBody {
    /// The `kind` the body is written under.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::RunEvent(_) => "run_event",
            Self::Status(_) => "factory_status",
            Self::Held(_) => "machine_held",
            Self::Turn(_) => "session_turn",
        }
    }
}

/// What makes a factory record one record: its kind and an id within the kind. A newer version
/// of a key from the same owner replaces the one held.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FactoryKey {
    pub kind: &'static str,
    pub id: String,
}

impl FactoryRecord {
    /// The record's key. Every run event is a record of its own, `<run>/<owner>/<version>`, as
    /// no event is ever rewritten. A status is one per project and writer,
    /// `<project>/<owner>`. A machine's logins held are keyed by the machine. A session's turn
    /// is one per session and writer, `<session>/<owner>`, so a machine that does not own a
    /// session never takes the key its owner writes the session's turns under; the reader takes
    /// the turn of the machine that owns the session's P1 record.
    pub fn key(&self) -> FactoryKey {
        let id = match &self.body {
            FactoryBody::RunEvent(event) => {
                format!("{}/{}/{}", event.run, self.owner, self.version)
            }
            FactoryBody::Status(status) => format!("{}/{}", status.project, self.owner),
            FactoryBody::Held(_) => self.owner.to_string(),
            FactoryBody::Turn(turn) => format!("{}/{}", turn.session, self.owner),
        };
        FactoryKey {
            kind: self.body.kind(),
            id,
        }
    }
}

/// One step of a run. `ts`, `ms`, `event`, `data` and `cause` are a line of the runner's ledger,
/// unchanged: written without `run` and `generation`, this has the line's keys and values. Keys
/// inside `data` and `cause` are written in sorted order, which the runner's reader does not
/// depend on. A run's events are
/// ordered by `generation`, then by the writer's factory version; never by `ts` or `ms`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunEvent {
    pub run: RunId,
    /// The lease generation the event was written under.
    pub generation: u64,
    /// The writer's wall-clock time, as the runner formats it.
    pub ts: String,
    /// The same time as Unix epoch milliseconds.
    pub ms: u64,
    pub event: String,
    pub data: Value,
    /// Which part of the factory wrote the line, and why. Left out of the line when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<Value>,
}

/// One project's factory as one machine sees it, written by the acting coordinator each pass.
/// A writer that does not hold the lease says why in `not_holding`, and nothing more.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FactoryStatus {
    pub project: ProjectId,
    /// The writer's pass count, which only rises.
    pub pass: u64,
    /// Whether the writer holds the project's lease.
    pub holds: bool,
    /// The lease generation the writer holds, or the highest it has seen.
    pub generation: u64,
    pub not_holding: Option<NotHolding>,
    /// Why the factory is idle, when it is.
    pub idle: Option<String>,
    /// The repo sets a `factory.owner`, which is ignored.
    pub owner_ignored: bool,
    /// The ready queue in claim order, each with why it waits.
    pub ready: Vec<Waiting>,
    /// Feedback rounds that wait to start.
    pub feedback_waiting: Vec<Waiting>,
    pub runs: Vec<LiveRun>,
    /// Runs that are stuck or wait for answers.
    pub held_up: Vec<HeldUp>,
    /// Runs with a gate that could not run at all.
    pub gate_infra: Vec<RunId>,
    /// The last finished runs, newest first.
    pub finished: Vec<FinishedRun>,
    /// Issues in `triage` or `building` whose newest claim is not Foundry's.
    pub foreign: Vec<ForeignClaim>,
}

/// Why a writer does not hold a project's lease.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NotHolding {
    /// It holds no token for the project's login.
    NoToken { login: String },
    /// The lease ref names this holder at this generation.
    HeldBy { machine: MachineId, generation: u64 },
}

/// An issue, or a run's feedback round, that waits to start, and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Waiting {
    pub issue: u64,
    pub title: String,
    pub reason: String,
    /// The run a feedback round belongs to. `None` for a ready issue, which has no run yet.
    #[serde(default)]
    pub run: Option<RunId>,
}

/// A run in progress.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveRun {
    pub run: RunId,
    pub title: String,
    pub step: String,
    /// The machine its active workspace is on.
    pub machine: MachineId,
    pub gate_retries_used: u32,
    pub minutes_used: u64,
    pub workspaces: Vec<WorkspaceId>,
    pub sessions: Vec<SessionId>,
}

/// A run that waits on a person.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldUp {
    pub run: RunId,
    pub kind: HeldUpKind,
    pub reason: String,
    /// The run's current workspaces, as a live run's are. Empty in a status from a peer built
    /// before held-up runs named them.
    #[serde(default)]
    pub workspaces: Vec<WorkspaceId>,
    /// The pull request the run reported, else the one it finished with. `None` in a status
    /// from a peer built before held-up runs named it.
    #[serde(default)]
    pub pr_url: Option<String>,
    /// The session the run launched last (at most one today). Empty in a status from a peer
    /// built before held-up runs named it.
    #[serde(default)]
    pub sessions: Vec<SessionId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeldUpKind {
    Stuck,
    NeedsInfo,
}

/// A run that ended, and how, as the runner's `finished` event says it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinishedRun {
    pub run: RunId,
    pub title: String,
    pub outcome: RunOutcome,
    /// Set for a run that ended because its pull request did.
    pub pr_end: Option<PrEnd>,
    pub pr_url: Option<String>,
}

/// How a run ended: the runner's `Outcome`, written as the runner writes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunOutcome {
    PrOpen,
    Stuck,
    NeedsInfo,
}

/// How a run's pull request ended: the runner's `PrEnd`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PrEnd {
    Merged,
    Closed,
}

/// An issue claimed by something other than Foundry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForeignClaim {
    pub issue: u64,
    pub title: String,
    /// The factory label it is in, without the `factory:` prefix.
    pub state: String,
}

/// The GitHub logins a machine holds a token for, and whether it has the `factory-build` skill.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineHeld {
    pub logins: Vec<String>,
    pub skill: bool,
}

/// The newest turn of a factory session, written by its machine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTurn {
    pub session: SessionId,
    pub workspace: WorkspaceId,
    pub project: ProjectId,
    /// The newest turn's number, which only rises.
    pub turn: u64,
    /// How many prompts the session has had submitted.
    pub prompts: u64,
    /// Whether a question waits.
    pub question: bool,
}

/// The fleet's factory settings. A field left out reads as its default.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FactorySettings {
    /// Claims are paused for every project.
    pub paused: bool,
    /// How often a pass runs: from [`POLL_SECONDS_MIN`] to [`POLL_SECONDS_MAX`]. Nothing else
    /// parses, and nothing else is written.
    #[serde(
        serialize_with = "write_poll_seconds",
        deserialize_with = "read_poll_seconds"
    )]
    pub poll_seconds: u32,
    /// How often a run in `pr-open` reads new feedback.
    pub feedback_seconds: u64,
    /// Runs that hold a slot at once.
    pub max_runs: u32,
    /// Gates that run at once.
    pub max_gates: u32,
    /// The model a factory session starts with.
    pub model: String,
    /// The effort a factory session starts with.
    pub effort: String,
}

/// The shortest `poll_seconds`.
pub const POLL_SECONDS_MIN: u32 = 10;
/// The longest `poll_seconds`.
pub const POLL_SECONDS_MAX: u32 = 60;

fn poll_seconds_refused(seconds: impl fmt::Display) -> String {
    format!("poll_seconds is {seconds}: it must be from {POLL_SECONDS_MIN} to {POLL_SECONDS_MAX}")
}

/// Refuses to write a `poll_seconds` that would not read back: the receiver would refuse the
/// whole settings copy, as [`crate::serialize_finite`] says of a non-finite number.
fn write_poll_seconds<S: Serializer>(seconds: &u32, s: S) -> Result<S::Ok, S::Error> {
    if (POLL_SECONDS_MIN..=POLL_SECONDS_MAX).contains(seconds) {
        s.serialize_u32(*seconds)
    } else {
        Err(serde::ser::Error::custom(poll_seconds_refused(seconds)))
    }
}

fn read_poll_seconds<'de, D: Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
    let seconds = u64::deserialize(d)?;
    u32::try_from(seconds)
        .ok()
        .filter(|s| (POLL_SECONDS_MIN..=POLL_SECONDS_MAX).contains(s))
        .ok_or_else(|| serde::de::Error::custom(poll_seconds_refused(seconds)))
}

impl Default for FactorySettings {
    fn default() -> Self {
        Self {
            paused: false,
            poll_seconds: 30,
            feedback_seconds: 600,
            max_runs: 3,
            max_gates: 2,
            model: "opus".into(),
            effort: "high".into(),
        }
    }
}

/// One project's factory setting. A field left out reads as its default: off, no login, not
/// paused.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectFactory {
    /// The factory watches the project.
    pub on: bool,
    /// The GitHub login the project acts as, written when the factory is turned on.
    pub login: String,
    /// Claims are paused for this project.
    pub paused: bool,
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::ids::{MachineId, ProjectId, SessionId, WorkspaceId};
    use crate::{assert_round_trip, parse_error};

    fn run(text: &str) -> RunId {
        text.parse().expect(text)
    }

    #[test]
    fn a_host_kind_is_written_in_lower_case_and_read_back() {
        assert_eq!(serde_json::to_string(&HostKind::Vims).unwrap(), "\"vims\"");
        let back: HostKind = serde_json::from_str("\"vims\"").unwrap();
        assert_eq!(back, HostKind::Vims);
        assert_eq!(HostKind::Vims.to_string(), "vims");
        assert_eq!("vims".parse::<HostKind>(), Ok(HostKind::Vims));
        assert!("Vims".parse::<HostKind>().is_err());
        assert!("kubernetes".parse::<HostKind>().is_err());
    }

    fn new_run(repo: &str, issue: u64, attempt: u32) -> RunId {
        RunId::new(repo, issue, attempt).expect(repo)
    }

    // The runner's `src/ledger/runId.test.ts` at 8ed9bef, for the numeric issues a GitHub
    // repo has.

    #[test]
    fn run_id_converts_to_and_from_its_string() {
        let id = RunId {
            repo: "rawphp/softfac".into(),
            issue: 8,
            attempt: 1,
        };
        assert_eq!(id.to_string(), "rawphp__softfac__8__1");
        assert_eq!(run("rawphp__softfac__8__1"), id);
        assert_eq!(new_run("rawphp/softfac", 8, 1), id);
        assert_eq!(
            (id.repo(), id.issue(), id.attempt()),
            ("rawphp/softfac", 8, 1)
        );
    }

    #[test]
    fn run_id_round_trips_dots_dashes_and_large_numbers() {
        let id = new_run("my-org/my.repo-2", u64::MAX, u32::MAX);
        assert_eq!(
            id.to_string(),
            "my-org__my.repo-2__18446744073709551615__4294967295"
        );
        assert_eq!(run(&id.to_string()), id);
    }

    #[test]
    fn run_id_new_rejects_bad_repos() {
        for repo in [
            "",
            "softfac",
            "rawphp/",
            "/softfac",
            "a/b/c",
            "a\\b/c",
            "a/b c",
            "a/b\\c",
            "a/b\0",
            "a/b\u{1b}",
            "a/bé",
            "a/b~",
            "a/b:c",
            "a/b*",
            "../x",
            "x/..",
            "./x",
            "../../etc/passwd",
            "a b/c",
            "_tom/x",
            "tom_/x",
            "to__m/x",
            "tom_tc_/x",
            "my.org/x",
            "a/.",
            "a/b\n",
            "a\0/b",
            "raé/b",
        ] {
            let error = RunId::new(repo, 1, 1).expect_err(repo);
            assert!(error.contains("repo"), "{repo:?}: {error}");
        }
    }

    #[test]
    fn run_id_round_trips_an_owner_with_an_interior_underscore() {
        for (repo, text) in [
            ("tom_tc/softfac", "tom_tc__softfac__8__1"),
            ("tom_tc/a__b", "tom_tc__a__b__8__1"),
            ("a_b_c/_x_", "a_b_c___x___8__1"),
        ] {
            let id = new_run(repo, 8, 1);
            assert_eq!(id.repo(), repo);
            assert_eq!(id.to_string(), text);
            assert_eq!(run(text), id);
        }
    }

    #[test]
    fn run_ids_for_awkward_owners_and_repos_are_distinct_and_parse_back() {
        let combos: [(&str, &str, u64, u32); 16] = [
            ("tom_tc", "softfac", 8, 1),
            ("tom_tc", "a__b", 8, 1),
            ("tom", "_x", 8, 1),
            ("tom", "x_", 8, 1),
            ("tom", "__", 8, 1),
            ("tom", "_", 8, 1),
            ("tom", "8__1", 8, 1),
            ("tom", "8", 1, 8),
            ("tom", "8__1", 1, 8),
            ("tom_8", "1__2", 3, 4),
            ("tom", "1__2", 3, 4),
            ("a_b_c", "d_e__f_", 12, 34),
            ("a", "b__c", 1, 1),
            ("a_b", "c", 1, 1),
            ("a-b", "c.d-e_f", 0, 0),
            ("x", "y", u64::MAX, u32::MAX),
        ];
        let mut seen = std::collections::BTreeSet::new();
        for (owner, name, issue, attempt) in combos {
            let id = new_run(&format!("{owner}/{name}"), issue, attempt);
            let text = id.to_string();
            assert_eq!(run(&text), id, "{text}");
            assert!(seen.insert(text.clone()), "{text} is not distinct");
        }
        assert_eq!(seen.len(), combos.len());
    }

    #[test]
    fn run_id_round_trips_underscores_in_the_repo_name() {
        for (repo, text) in [
            ("rawphp/my_app", "rawphp__my_app__8__1"),
            ("rawphp/_private", "rawphp___private__8__1"),
            ("rawphp/trailing_", "rawphp__trailing___8__1"),
            ("rawphp/a__b", "rawphp__a__b__8__1"),
            ("rawphp/a__8", "rawphp__a__8__8__1"),
            ("rawphp/__", "rawphp______8__1"),
            ("rawphp/softfac__8__1", "rawphp__softfac__8__1__8__1"),
        ] {
            let id = new_run(repo, 8, 1);
            assert_eq!(id.repo(), repo);
            assert_eq!(id.to_string(), text, "{repo}");
            assert_eq!(run(text), id, "{repo}");
        }
    }

    #[test]
    fn run_id_lowercases_so_one_repo_is_one_id_and_one_file() {
        let mixed = new_run("Rawphp/SoftFac", 8, 1);
        let lower = new_run("rawphp/softfac", 8, 1);
        assert_eq!(mixed, lower);
        assert_eq!(mixed.repo(), "rawphp/softfac");
        assert_eq!(mixed.to_string(), "rawphp__softfac__8__1");
        assert_eq!(run("Rawphp__SoftFac__8__1"), lower);
        assert_eq!(
            new_run("Rawphp/My__App", 8, 1).to_string(),
            "rawphp__my__app__8__1"
        );
        assert_eq!(
            run("RAWPHP__My__App__8__1"),
            new_run("rawphp/my__app", 8, 1)
        );
    }

    #[test]
    fn run_id_lowercases_ascii_only_so_a_lookalike_capital_is_not_folded_into_a_legal_name() {
        // U+212A KELVIN SIGN lowercases to an ASCII `k` under Unicode rules.
        assert!(RunId::new("\u{212A}elvin/x", 1, 1).is_err());
        assert!("\u{212A}elvin__x__1__1".parse::<RunId>().is_err());
    }

    #[test]
    fn a_run_id_that_is_not_the_shape_is_refused() {
        for text in [
            "",
            "rawphp",
            "rawphp__softfac",
            "rawphp__softfac__8",
            "rawphp__softfac__08__1",
            "rawphp__softfac__8__01",
            "rawphp__softfac__+8__1",
            "rawphp__softfac__-8__1",
            "rawphp__softfac__8__-1",
            "rawphp__softfac__eng-42__1",
            "rawphp__softfac__ENG-42__1",
            "rawphp__softfac__8__4294967296",
            "rawphp__softfac__18446744073709551616__1",
            "rawphp__softfac__8__1.5",
            "__softfac__8__1",
            "rawphp____8__1",
            "rawphp__..__8__1",
            "rawphp__.__8__1",
            "raw.php__softfac__8__1",
            "rawphp__soft fac__8__1",
            "rawphp__soft/fac__8__1",
        ] {
            assert!(text.parse::<RunId>().is_err(), "{text:?}");
            parse_error::<RunId>(&serde_json::to_string(text).unwrap());
        }
    }

    #[test]
    fn a_run_id_is_written_as_its_string() {
        let id = new_run("original-solutions/foundry", 129, 2);
        assert_eq!(
            serde_json::to_value(&id).unwrap(),
            "original-solutions__foundry__129__2"
        );
        assert_round_trip(&id);
        let read: RunId = serde_json::from_str("\"original-solutions__foundry__129__2\"").unwrap();
        assert_eq!(read, id);
    }

    #[test]
    fn a_hand_built_run_id_that_is_not_in_its_lowercase_form_cannot_be_written() {
        for repo in ["Rawphp/softfac", "../x", "a/b/c", "softfac"] {
            let id = RunId {
                repo: repo.into(),
                issue: 1,
                attempt: 1,
            };
            assert!(serde_json::to_string(&id).is_err(), "{repo}");
        }
    }

    // Factory records.

    /// Two lines of the runner's `src/ledger/ledger.test.ts` at 8ed9bef, one with a cause and one
    /// without, and a third made for this test from its `gate_command` data, with a null and a
    /// negative number added.
    const LEDGER_LINES: [&str; 3] = [
        r#"{"ts":"2026-10-04T10:13:16.123Z","ms":1791108796123,"event":"step_changed","data":{"step":"red"},"cause":{"step":"pass","reason":"claimed"}}"#,
        r#"{"ts":"2026-10-04T10:13:16.123Z","ms":1791108796123,"event":"step_changed","data":{"step":"red"}}"#,
        r#"{"ts":"2026-10-04T10:13:16.123Z","ms":1791108796123,"event":"gate_command","data":{"attempt":1,"command":"cargo test","exit_code":-1,"duration_ms":4200,"extra":null},"cause":{"step":"gate","reason":null}}"#,
    ];

    fn event_from_line(line: &str, run: RunId, generation: u64) -> RunEvent {
        let line: serde_json::Value = serde_json::from_str(line).unwrap();
        RunEvent {
            run,
            generation,
            ts: line["ts"].as_str().unwrap().to_owned(),
            ms: line["ms"].as_u64().unwrap(),
            event: line["event"].as_str().unwrap().to_owned(),
            data: line["data"].clone(),
            cause: line.get("cause").cloned(),
        }
    }

    pub(crate) fn run_event() -> RunEvent {
        event_from_line(
            LEDGER_LINES[0],
            new_run("original-solutions/foundry", 129, 1),
            3,
        )
    }

    #[test]
    fn a_run_event_serialises_to_the_keys_of_a_ledger_line_beside_its_run_and_generation() {
        for line in LEDGER_LINES {
            let id = new_run("rawphp/softfac", 8, 1);
            let event = event_from_line(line, id.clone(), 7);
            let mut json = serde_json::to_value(&event).unwrap();
            let object = json.as_object_mut().unwrap();
            assert_eq!(object.remove("run").unwrap(), "rawphp__softfac__8__1");
            assert_eq!(object.remove("generation").unwrap(), 7);
            let expected: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(json, expected, "{line}");
            assert_round_trip(&event);
        }
    }

    #[test]
    fn a_run_event_reads_a_ledger_line_with_its_run_and_generation() {
        let line = LEDGER_LINES[1];
        let json = format!(
            r#"{{"run":"rawphp__softfac__8__1","generation":2,{}"#,
            &line[1..]
        );
        let read: RunEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(
            read,
            event_from_line(line, new_run("rawphp/softfac", 8, 1), 2)
        );
        assert_eq!(read.cause, None);
    }

    pub(crate) fn status() -> FactoryStatus {
        let id = new_run("original-solutions/foundry", 129, 1);
        let other = new_run("original-solutions/foundry", 130, 2);
        FactoryStatus {
            project: ProjectId::random(),
            pass: 41,
            holds: true,
            generation: 3,
            not_holding: None,
            idle: Some("`gh` failed three passes running".into()),
            owner_ignored: true,
            ready: vec![Waiting {
                issue: 131,
                title: "feat: the clock".into(),
                reason: "no room".into(),
                run: None,
            }],
            feedback_waiting: vec![Waiting {
                issue: 130,
                title: "feat: the store".into(),
                reason: "every run slot is held".into(),
                run: Some(other.clone()),
            }],
            runs: vec![LiveRun {
                run: id.clone(),
                title: "feat: the shared types".into(),
                step: "building".into(),
                machine: MachineId::random(),
                gate_retries_used: 1,
                minutes_used: 42,
                workspaces: vec![WorkspaceId::random(), WorkspaceId::random()],
                sessions: vec![SessionId::random()],
            }],
            held_up: vec![
                HeldUp {
                    run: other.clone(),
                    kind: HeldUpKind::Stuck,
                    reason: "the gate failed".into(),
                    workspaces: vec![WorkspaceId::random()],
                    pr_url: None,
                    sessions: vec![],
                },
                HeldUp {
                    run: id.clone(),
                    kind: HeldUpKind::NeedsInfo,
                    reason: "1. Which login?".into(),
                    workspaces: vec![],
                    pr_url: None,
                    sessions: vec![],
                },
            ],
            gate_infra: vec![other.clone()],
            finished: vec![FinishedRun {
                run: new_run("original-solutions/foundry", 99, 1),
                title: "fix: a typo".into(),
                outcome: RunOutcome::PrOpen,
                pr_end: Some(PrEnd::Merged),
                pr_url: Some("https://github.com/original-solutions/foundry/pull/7".into()),
            }],
            foreign: vec![ForeignClaim {
                issue: 12,
                title: "chore: claimed by the runner".into(),
                state: "building".into(),
            }],
        }
    }

    pub(crate) fn held() -> MachineHeld {
        MachineHeld {
            logins: vec!["tomkaczocha".into(), "rawphp".into()],
            skill: true,
            default: Some("rawphp".into()),
            gateway: Some(GatewayKeys {
                key: true,
                management: false,
            }),
        }
    }

    pub(crate) fn turn() -> SessionTurn {
        SessionTurn {
            session: SessionId::random(),
            workspace: WorkspaceId::random(),
            project: ProjectId::random(),
            turn: 4,
            prompts: 5,
            question: true,
        }
    }

    pub(crate) fn records() -> Vec<FactoryRecord> {
        let owner = MachineId::random();
        vec![
            FactoryRecord {
                owner,
                version: 1,
                body: FactoryBody::RunEvent(run_event()),
            },
            FactoryRecord {
                owner,
                version: 2,
                body: FactoryBody::Status(status()),
            },
            FactoryRecord {
                owner,
                version: 3,
                body: FactoryBody::Held(held()),
            },
            FactoryRecord {
                owner,
                version: 4,
                body: FactoryBody::Turn(turn()),
            },
        ]
    }

    /// The runner's `Outcome` and `PrEnd` in `src/ledger/events.ts` at 8ed9bef, and nothing else.
    #[test]
    fn a_finished_run_takes_only_the_runners_outcomes_and_pull_request_ends() {
        let finished = |outcome: &str, pr_end: serde_json::Value| {
            serde_json::json!({
                "run": "rawphp__softfac__8__1",
                "title": "feat: x",
                "outcome": outcome,
                "pr_end": pr_end,
                "pr_url": null,
            })
        };
        for (text, outcome) in [
            ("pr-open", RunOutcome::PrOpen),
            ("stuck", RunOutcome::Stuck),
            ("needs-info", RunOutcome::NeedsInfo),
        ] {
            let read: FinishedRun =
                serde_json::from_value(finished(text, serde_json::Value::Null)).unwrap();
            assert_eq!(read.outcome, outcome);
            assert_eq!(read.pr_end, None);
            assert_eq!(serde_json::to_value(&read).unwrap()["outcome"], text);
        }
        for (text, pr_end) in [("merged", PrEnd::Merged), ("closed", PrEnd::Closed)] {
            let read: FinishedRun =
                serde_json::from_value(finished("pr-open", text.into())).unwrap();
            assert_eq!(read.pr_end, Some(pr_end));
            assert_eq!(serde_json::to_value(&read).unwrap()["pr_end"], text);
        }
        for outcome in ["needs_info", "Stuck", "merged", ""] {
            let json = finished(outcome, serde_json::Value::Null);
            assert!(
                serde_json::from_value::<FinishedRun>(json).is_err(),
                "{outcome}"
            );
        }
        for pr_end in ["open", "Merged", ""] {
            let json = finished("pr-open", pr_end.into());
            assert!(
                serde_json::from_value::<FinishedRun>(json).is_err(),
                "{pr_end}"
            );
        }
    }

    #[test]
    fn every_factory_type_round_trips() {
        let status = status();
        assert_round_trip(&run_event());
        assert_round_trip(&status);
        assert_round_trip(&held());
        assert_round_trip(&turn());
        for waiting in status.ready.iter().chain(&status.feedback_waiting) {
            assert_round_trip(waiting);
        }
        assert_round_trip(&status.runs[0]);
        for held_up in &status.held_up {
            assert_round_trip(held_up);
            assert_round_trip(&held_up.kind);
        }
        assert_round_trip(&status.finished[0]);
        assert_round_trip(&status.foreign[0]);
        for not_holding in [
            NotHolding::NoToken {
                login: "rawphp".into(),
            },
            NotHolding::HeldBy {
                machine: MachineId::random(),
                generation: 9,
            },
        ] {
            assert_round_trip(&not_holding);
            assert_round_trip(&FactoryStatus {
                holds: false,
                not_holding: Some(not_holding),
                ..status.clone()
            });
        }
        assert_round_trip(&FactoryStatus {
            idle: None,
            ready: vec![],
            feedback_waiting: vec![],
            runs: vec![],
            held_up: vec![],
            gate_infra: vec![],
            finished: vec![],
            foreign: vec![],
            ..status
        });
        for record in records() {
            assert_round_trip(&record);
        }
    }

    #[test]
    fn a_factory_record_is_written_with_its_kind_beside_its_owner_and_version() {
        let kinds = [
            "run_event",
            "factory_status",
            "machine_held",
            "session_turn",
        ];
        for (record, kind) in records().into_iter().zip(kinds) {
            let json = serde_json::to_value(&record).unwrap();
            assert_eq!(json["kind"], kind, "{json}");
            assert_eq!(json["owner"], record.owner.to_string());
            assert_eq!(json["version"], record.version);
            assert!(json["body"].is_object(), "{json}");
            assert_eq!(record.body.kind(), kind);
            assert_eq!(record.key().kind, kind);
        }
    }

    #[test]
    fn a_held_record_from_before_the_default_and_the_gateway_parses_with_neither() {
        let held: MachineHeld = serde_json::from_str(r#"{"logins":["a"],"skill":false}"#).unwrap();
        assert_eq!(
            held,
            MachineHeld {
                logins: vec!["a".into()],
                skill: false,
                default: None,
                gateway: None,
            }
        );
    }

    #[test]
    fn a_held_record_with_a_default_and_gateway_keys_round_trips() {
        assert_round_trip(&held());
        assert_round_trip(&MachineHeld::default());
    }

    #[test]
    fn held_up_and_not_holding_are_written_under_their_names() {
        assert_eq!(
            serde_json::to_value(HeldUpKind::NeedsInfo).unwrap(),
            "needs_info"
        );
        assert_eq!(serde_json::to_value(HeldUpKind::Stuck).unwrap(), "stuck");
        let machine = "67e55044-10b1-426f-9247-bb680e5fe0c8";
        let cases = [
            (
                r#"{"kind":"no_token","login":"rawphp"}"#.to_owned(),
                NotHolding::NoToken {
                    login: "rawphp".into(),
                },
            ),
            (
                format!(r#"{{"kind":"held_by","machine":"{machine}","generation":4}}"#),
                NotHolding::HeldBy {
                    machine: MachineId(machine.parse().unwrap()),
                    generation: 4,
                },
            ),
        ];
        for (json, expected) in cases {
            assert_eq!(serde_json::from_str::<NotHolding>(&json).unwrap(), expected);
            assert_eq!(serde_json::to_string(&expected).unwrap(), json);
        }
    }

    #[test]
    fn a_factory_record_reads_from_its_wire_form() {
        let machine = "67e55044-10b1-426f-9247-bb680e5fe0c8";
        let session = "1b4e28ba-2fa1-11d2-883f-0016d3cca427";
        let workspace = "22222222-2222-4222-8222-222222222222";
        let project = "11111111-1111-4111-8111-111111111111";
        let owner = MachineId(machine.parse().unwrap());
        let cases = [
            (
                format!(
                    r#"{{"kind":"machine_held","owner":"{machine}","version":3,"body":{{"logins":["rawphp"],"skill":false}}}}"#
                ),
                FactoryRecord {
                    owner,
                    version: 3,
                    body: FactoryBody::Held(MachineHeld {
                        logins: vec!["rawphp".into()],
                        skill: false,
                        ..Default::default()
                    }),
                },
            ),
            (
                format!(
                    r#"{{"kind":"session_turn","owner":"{machine}","version":4,"body":{{"session":"{session}","workspace":"{workspace}","project":"{project}","turn":2,"prompts":3,"question":false}}}}"#
                ),
                FactoryRecord {
                    owner,
                    version: 4,
                    body: FactoryBody::Turn(SessionTurn {
                        session: SessionId(session.parse().unwrap()),
                        workspace: WorkspaceId(workspace.parse().unwrap()),
                        project: ProjectId(project.parse().unwrap()),
                        turn: 2,
                        prompts: 3,
                        question: false,
                    }),
                },
            ),
            (
                format!(
                    r#"{{"kind":"run_event","owner":"{machine}","version":5,"body":{{"run":"rawphp__softfac__8__1","generation":1,"ts":"t","ms":0,"event":"claimed","data":{{"machine":"m","github_user":"rawphp"}}}}}}"#
                ),
                FactoryRecord {
                    owner,
                    version: 5,
                    body: FactoryBody::RunEvent(RunEvent {
                        run: new_run("rawphp/softfac", 8, 1),
                        generation: 1,
                        ts: "t".into(),
                        ms: 0,
                        event: "claimed".into(),
                        data: serde_json::json!({"machine": "m", "github_user": "rawphp"}),
                        cause: None,
                    }),
                },
            ),
            (
                format!(
                    r#"{{"kind":"factory_status","owner":"{machine}","version":6,"body":{{"project":"{project}","pass":1,"holds":false,"generation":0,"not_holding":{{"kind":"no_token","login":"rawphp"}},"idle":null,"owner_ignored":false,"ready":[],"feedback_waiting":[],"runs":[],"held_up":[],"gate_infra":[],"finished":[],"foreign":[]}}}}"#
                ),
                FactoryRecord {
                    owner,
                    version: 6,
                    body: FactoryBody::Status(FactoryStatus {
                        project: ProjectId(project.parse().unwrap()),
                        pass: 1,
                        holds: false,
                        generation: 0,
                        not_holding: Some(NotHolding::NoToken {
                            login: "rawphp".into(),
                        }),
                        idle: None,
                        owner_ignored: false,
                        ready: vec![],
                        feedback_waiting: vec![],
                        runs: vec![],
                        held_up: vec![],
                        gate_infra: vec![],
                        finished: vec![],
                        foreign: vec![],
                    }),
                },
            ),
        ];
        for (json, expected) in cases {
            assert_eq!(
                serde_json::from_str::<FactoryRecord>(&json).expect(&json),
                expected
            );
        }
    }

    #[test]
    fn an_unknown_kind_of_factory_record_is_an_error_that_names_it() {
        let machine = "67e55044-10b1-426f-9247-bb680e5fe0c8";
        let error = parse_error::<FactoryRecord>(&format!(
            r#"{{"kind":"workspace","owner":"{machine}","version":1,"body":{{}}}}"#
        ));
        assert!(error.contains("workspace"), "{error}");
    }

    #[test]
    fn each_record_is_keyed_by_its_kind_and_the_id_that_makes_it_one_record() {
        let owner = MachineId::random();
        let other = MachineId::random();
        let event = run_event();
        let status = status();
        let turn = turn();
        let record = |owner, version, body| FactoryRecord {
            owner,
            version,
            body,
        };

        // One record per event: two events of a run, by one owner or two, never share a key.
        let first = record(owner, 1, FactoryBody::RunEvent(event.clone()));
        let second = record(owner, 2, FactoryBody::RunEvent(event.clone()));
        let elsewhere = record(other, 1, FactoryBody::RunEvent(event.clone()));
        assert_ne!(first.key(), second.key());
        assert_ne!(first.key(), elsewhere.key());
        assert!(first.key().id.starts_with(&event.run.to_string()));

        // One status per project and writer, rewritten under new versions.
        let mine = record(owner, 1, FactoryBody::Status(status.clone()));
        let again = record(owner, 9, FactoryBody::Status(status.clone()));
        let theirs = record(other, 1, FactoryBody::Status(status.clone()));
        let next_project = record(
            owner,
            1,
            FactoryBody::Status(FactoryStatus {
                project: ProjectId::random(),
                ..status
            }),
        );
        assert_eq!(mine.key(), again.key());
        assert_ne!(mine.key(), theirs.key());
        assert_ne!(mine.key(), next_project.key());

        // One `MachineHeld` per machine.
        let held_mine = record(owner, 1, FactoryBody::Held(held()));
        let held_again = record(owner, 5, FactoryBody::Held(MachineHeld::default()));
        let held_theirs = record(other, 1, FactoryBody::Held(held()));
        assert_eq!(held_mine.key(), held_again.key());
        assert_ne!(held_mine.key(), held_theirs.key());
        assert_eq!(held_mine.key().id, owner.to_string());

        // One `SessionTurn` per session and writer, so a machine that does not own a session
        // never takes the key its owner writes the session's turns under.
        let turn_one = record(owner, 1, FactoryBody::Turn(turn.clone()));
        let turn_two = record(
            owner,
            2,
            FactoryBody::Turn(SessionTurn {
                turn: 5,
                ..turn.clone()
            }),
        );
        let turn_theirs = record(other, 1, FactoryBody::Turn(turn.clone()));
        assert_eq!(turn_one.key(), turn_two.key());
        assert_ne!(turn_one.key(), turn_theirs.key());
        assert_eq!(turn_one.key().id, format!("{}/{owner}", turn.session));

        // Kinds never share a key.
        let keys = [first.key(), mine.key(), held_mine.key(), turn_one.key()];
        for (i, a) in keys.iter().enumerate() {
            for b in &keys[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn an_unknown_field_is_ignored() {
        for record in records() {
            let mut json = serde_json::to_value(&record).unwrap();
            json["added_later"] = serde_json::json!(1);
            json["body"]["added_later"] = serde_json::json!("x");
            let back: FactoryRecord = serde_json::from_value(json).unwrap();
            assert_eq!(back, record);
        }
    }

    /// A status from a peer built before held-up runs named their workspaces still parses,
    /// each held-up run with none.
    #[test]
    fn a_held_up_run_from_an_older_peer_has_no_workspaces() {
        let mut json = serde_json::to_value(status()).unwrap();
        for held in json["held_up"].as_array_mut().unwrap() {
            held.as_object_mut().unwrap().remove("workspaces");
        }
        let read: FactoryStatus = serde_json::from_value(json).unwrap();
        assert_eq!(read.held_up.len(), 2);
        assert!(read.held_up.iter().all(|held| held.workspaces.is_empty()));
    }

    /// A status from a peer built before held-up runs named a pull request and sessions still
    /// parses, each held-up run with no pull request and no sessions.
    #[test]
    fn a_held_up_run_from_an_older_peer_has_no_pull_request_or_sessions() {
        let mut json = serde_json::to_value(status()).unwrap();
        for held in json["held_up"].as_array_mut().unwrap() {
            let held = held.as_object_mut().unwrap();
            held.remove("pr_url");
            held.remove("sessions");
        }
        let read: FactoryStatus = serde_json::from_value(json).unwrap();
        assert_eq!(read.held_up.len(), 2);
        assert!(
            read.held_up
                .iter()
                .all(|held| held.pr_url.is_none() && held.sessions.is_empty())
        );
    }
}
