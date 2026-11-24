//! Bare clones, worktrees, setup commands and archiving.
//!
//! A machine keeps one bare clone per project at `<data dir>/clones/<project>.git`, made the
//! first time it hosts a workspace of that project, with `origin` set to the project's repo.
//! A workspace is a worktree of that clone at `<data dir>/worktrees/<project>/<workspace id>`,
//! made after a fetch in one of three ways ([`Checkout`]): on a new branch from `origin/<base>`,
//! on an existing branch of `origin`, where a local branch of that name is moved to `origin`'s,
//! or detached at one commit, with no branch. A branch the caller gives is used exactly, `/`
//! kept, once [`branch_refused`] lets it through; one made from the workspace's name is
//! [`safe_branch`]. Folders made before workspaces were named by id, at
//! `worktrees/<project>/<branch>`, stay where they are: the folder of a workspace is its id's,
//! or the old one when only that is there, and sweep and archive find either.
//!
//! Its setup ([`Setup`]) runs in a terminal of the workspace: the `factory.provision` list in
//! the worktree's own `.dw/config.yml`, a list of commands the caller gives, each stopped when
//! it runs past its limit, or none. A setup that fails leaves the workspace as it is: the
//! terminal shows what failed and exits with the failing command's code, or 124 for a command
//! that was stopped for running too long.
//!
//! A workspace made for the factory carries a [`FactoryTag`]. It is marked in a file beside its
//! folder, `<workspace id>.factory`, so a create with the same tag and name answers with the
//! workspace already made and makes nothing. Its setup terminal gets no token and has git's
//! credential helpers turned off ([`crate::git_auth::helpers_off_env`]). For a project whose
//! factory is on, the fetch runs with the login's token ([`crate::git_auth::GitAuth`]) in a
//! second bare repo, `<data dir>/fetches/<project>.git`, which only the daemon writes, since the
//! clone's config is shared with every worktree and a session in one could write what reads the
//! token. The clone then fetches from that repo without the token.
//!
//! A create never touches what it did not make: it refuses a name whose branch or folder is
//! already there, makes the branch itself, and when it fails takes back only that branch, while
//! it still points where it was made, or puts a moved branch back, and the worktree in a folder
//! that was not there before.
//! It reads the project's repo from the shared settings at the start and checks they still
//! hold the project as it writes the record, so a project removed while its clone, fetch and
//! worktree were made fails the create, which takes back what it made.
//!
//! Archiving refuses while there is uncommitted or unpushed work, unless forced. Uncommitted
//! work is a changed, deleted or new file that git does not ignore; ignored files, such as build
//! output, go with the worktree. Unpushed work is a commit on the branch, or on whatever the
//! worktree has checked out, that no remote branch has. A checked-out submodule is refused too,
//! as its own work cannot be checked from here. An archive checks, closes the workspace's
//! terminals, checks again for what they wrote as they were hung up, removes the worktree and
//! keeps the branch. A refusal at the first check changes nothing; a refusal at the second
//! leaves the terminals closed and the worktree as it is. Only a forced archive passes `--force`
//! to git, so without it git refuses whatever the checks missed.
//!
//! A create that dies after making its worktree and before writing its record leaves a worktree
//! no record names. At start, [`Workspaces::sweep`] removes each worktree no live record names,
//! whatever left it, that an unforced archive would remove and that holds nothing git ignores,
//! so git can give back all of it, keeping its branch, and leaves and logs any other. It stops at
//! one deadline, and what it did not reach waits for the next start.
//!
//! A start of a shell or session in a workspace goes through [`Workspaces::start_in`], which
//! holds the workspace's lifecycle lock for its whole work, as an archive does. So a start never
//! lands between an archive's listing of the workspace's terminals and its delete: it runs
//! before the archive, whose close then takes its terminal, or after it, and finds the workspace
//! gone. The lock is per workspace, apart from the per-project lock, which a create holds across
//! its setup's start: a start must not wait for a fetch of another workspace of the project.
//!
//! A branch the workspace's name makes is [`safe_branch`], a branch the caller gives is checked
//! by [`branch_refused`] before git runs, and every path and name reaches git after `--` or as
//! the value of an option, so a name as typed never reads as an option and never leaves the
//! project's folder of worktrees.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fmt;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use foundry_proto::ids::{MachineId, ProjectId, TerminalId, WorkspaceId};
use foundry_proto::record::{AnyRecord, Record, Workspace};
use serde::{Deserialize, Serialize};

use crate::git_auth::{GitAuth, NoToken};
use crate::secrets::Secrets;
use crate::store::{Store, StoreError};
use crate::terminals::{TerminalError, Terminals};

mod changes;
mod pr;
#[cfg(test)]
mod tests;

pub use changes::{Changes, Commit, FileChange, FileStatus};
pub use pr::{Checks, PrState, PullRequest, Review, Shipping};

/// The longest branch [`safe_branch`] makes, in bytes.
const BRANCH_AT_MOST: usize = 100;

/// How long any one git call may run before it is killed and fails. A first fetch downloads the
/// whole repo, so this is generous; it is there so a remote that stops answering cannot hold a
/// project's lock for good.
const GIT_WITHIN: Duration = Duration::from_secs(10 * 60);

/// How long the sweep at start may run in all, so a git call that hangs cannot hold the daemon's
/// start for [`GIT_WITHIN`] per worktree.
const SWEEP_WITHIN: Duration = Duration::from_secs(60);

/// Runs each command given after it in turn, says which one failed and exits with its code.
const SETUP: &str = r#"for command do
  printf '$ %s\n' "$command"
  /bin/sh -c "$command" || { code=$?; printf 'setup failed: %s exited %s\n' "$command" "$code"; exit "$code"; }
done
printf 'setup passed\n'"#;

/// Runs each command given after the limit in seconds in turn, each stopped when it runs past
/// the limit, says which one failed and exits with its code, or 124 for one that was stopped.
/// Plain POSIX `sh`, `ps` and `awk`, since the fleet runs on macOS, which has no `timeout`, and on
/// Linux. A command runs in the background with a watcher beside it; the watcher waits out the
/// limit and then stops the command's whole tree: it freezes every process of the tree, lists it
/// again until no new one appears so nothing forks away, and kills them all.
const SETUP_LIMITED: &str = r#"limit=$1
shift
tree() {
  ps -A -o pid= -o ppid= | awk -v root="$1" '
    { kids[$2] = kids[$2] " " $1 }
    END {
      n = 0; queue[n++] = root; seen[root] = 1
      for (i = 0; i < n; i++) {
        m = split(kids[queue[i]], k, " ")
        for (j = 1; j <= m; j++) if (!(k[j] in seen)) { seen[k[j]] = 1; queue[n++] = k[j] }
      }
      for (i = 0; i < n; i++) print queue[i]
    }' | sort -n
}
stop_tree() {
  known=$(tree "$1")
  while :; do
    for p in $known; do kill -STOP "$p" 2>/dev/null; done
    now=$(tree "$1")
    [ "$now" = "$known" ] && break
    known=$now
  done
  for p in $known; do kill -KILL "$p" 2>/dev/null; done
}
dir=
trap 'rm -rf "$dir"' EXIT
trap 'exit 1' HUP INT TERM
for command do
  printf '$ %s\n' "$command"
  dir=$(mktemp -d) || exit 1
  /bin/sh -c "$command" &
  pid=$!
  ( sleep "$limit"; : > "$dir/killed"; stop_tree "$pid" ) &
  watcher=$!
  wait "$pid"
  code=$?
  stop_tree "$watcher"
  if [ -e "$dir/killed" ]; then
    printf 'setup failed: %s ran past %s s and was stopped\n' "$command" "$limit"
    exit 124
  fi
  rm -rf "$dir"
  if [ "$code" -ne 0 ]; then
    printf 'setup failed: %s exited %s\n' "$command" "$code"
    exit "$code"
  fi
done
printf 'setup passed\n'"#;

/// Prints the reason given after it and fails.
const SETUP_UNREADABLE: &str = r#"printf 'setup failed: %s\n' "$1"; exit 1"#;

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    #[error("project {0} is not in the shared settings")]
    UnknownProject(ProjectId),
    #[error("{0:?} is not a plain branch name")]
    BranchRefused(String),
    #[error("{0:?} is not a full commit id: 40 or 64 lower case hex digits")]
    NotACommitId(String),
    #[error("the project's remote has no commit {0}")]
    UnknownCommitId(String),
    #[error("branch {0} is checked out in another workspace of this project")]
    BranchCheckedOut(String),
    #[error(transparent)]
    NoToken(#[from] NoToken),
    #[error("workspace {0} is not a live workspace of this machine")]
    Unknown(WorkspaceId),
    #[error(
        "a branch or worktree named {0} already exists in this project; give the workspace \
         another name"
    )]
    BranchExists(String),
    #[error("the project's remote has no branch {0}")]
    UnknownBase(String),
    #[error("commit {0} is not on this workspace's branch")]
    UnknownCommit(String),
    #[error(transparent)]
    Dirty(Dirty),
    #[error("git {command} failed: {stderr}")]
    Git {
        command: &'static str,
        stderr: String,
    },
    #[error("workspaces: {0}")]
    Io(#[from] io::Error),
    #[error("workspace {workspace} was made, but its setup could not be started: {source}")]
    SetupNotStarted {
        workspace: WorkspaceId,
        source: TerminalError,
    },
    #[error(
        "workspace {workspace} was made, but its setup terminal could not be recorded, so the \
         setup was stopped: {source}"
    )]
    SetupNotRecorded {
        workspace: WorkspaceId,
        source: Box<WorkspaceError>,
    },
    #[error(transparent)]
    Terminal(#[from] TerminalError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("workspaces: a step run apart from its caller did not finish: {0}")]
    Interrupted(#[from] tokio::task::JoinError),
}

/// Work an archive would take out of the worktree or leave only on this machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dirty {
    /// Paths, relative to the worktree, that are changed, deleted or new, not committed and not
    /// ignored by git.
    pub uncommitted: Vec<String>,
    /// Commits on the branch or the worktree's checked-out `HEAD` that no remote branch has,
    /// each as its short hash and subject.
    pub unpushed: Vec<String>,
    /// Submodules checked out in the worktree, as paths relative to it. Their own uncommitted
    /// and unpushed work cannot be checked from here and would go with the worktree.
    pub submodules: Vec<String>,
}

impl Dirty {
    fn is_empty(&self) -> bool {
        self.uncommitted.is_empty() && self.unpushed.is_empty() && self.submodules.is_empty()
    }
}

impl fmt::Display for Dirty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("archive refused")?;
        let mut sep = ":";
        if !self.uncommitted.is_empty() {
            write!(f, "{sep} uncommitted files {}", self.uncommitted.join(", "))?;
            sep = ";";
        }
        if !self.unpushed.is_empty() {
            write!(f, "{sep} unpushed commits {}", self.unpushed.join(", "))?;
            sep = ";";
        }
        if !self.submodules.is_empty() {
            write!(f, "{sep} submodules {}", self.submodules.join(", "))?;
        }
        Ok(())
    }
}

impl std::error::Error for Dirty {}

/// How a new workspace's worktree is checked out.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Checkout {
    /// On a new branch made from `origin/<base>`. The branch is `branch`, exactly, when given,
    /// else [`safe_branch`] of the workspace's name. A branch that already exists is refused.
    NewBranch {
        base: String,
        branch: Option<String>,
    },
    /// On the branch `name` of `origin`. A local branch of that name is moved to `origin`'s, and
    /// a branch another workspace of the project has checked out is refused.
    Branch { name: String },
    /// Detached at the commit `sha`, a full object id, with no branch.
    Commit { sha: String },
}

/// The setup a new workspace runs in its terminal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Setup {
    /// The `factory.provision` list of the worktree's own `.dw/config.yml`, if it has one.
    Own,
    /// These commands, one by one, each stopped when it runs `limit_secs` seconds, and no more
    /// after the first that fails. The terminal exits with the failing command's code, or 124
    /// for one that was stopped.
    Commands {
        commands: Vec<String>,
        limit_secs: u64,
    },
    /// No setup, and no terminal.
    None,
}

/// What marks a workspace as the factory's. A second create with the same tag and name answers
/// with the first workspace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FactoryTag {
    pub project: ProjectId,
    /// The run the workspace is made for.
    pub run: String,
}

/// The file beside a tagged workspace's folder that says what it was made for.
#[derive(Serialize, Deserialize)]
struct Marker {
    project: ProjectId,
    run: String,
    name: String,
}

/// Whether `branch` is no ordinary branch name and must not reach a command: it is empty, starts
/// with `-`, holds `..`, or holds anything but letters, digits, `/`, `.`, `_` and `-`, as the
/// runner's `branchRefused` has it. Git itself refuses more names than this.
pub fn branch_refused(branch: &str) -> bool {
    branch.is_empty()
        || branch.starts_with('-')
        || branch.contains("..")
        || !branch
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-'))
}

/// Whether `sha` is a full object id: 40 or 64 lower case hex digits.
fn is_commit_id(sha: &str) -> bool {
    matches!(sha.len(), 40 | 64)
        && sha
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Whether a folder `worktrees/<project>/<branch>` is one a P1 create could have made for a
/// workspace on `branch`: P1's folders were always [`safe_branch`] names.
fn legacy_able(branch: &str) -> bool {
    !branch.is_empty() && safe_branch(branch) == branch
}

/// `name` made into a git branch that is also one path part: letters and digits of any script,
/// `_`, `-` and single `.` are kept, anything else becomes `-`, a run of `-` becomes one, `-` and
/// `.` are cut from both ends, and the result is at most [`BRANCH_AT_MOST`] bytes. A name with
/// nothing left is `workspace`, and `HEAD`, which git keeps for itself, is `HEAD-workspace`.
pub fn safe_branch(name: &str) -> String {
    let mut branch = String::with_capacity(name.len());
    for c in name.chars() {
        let c = if c.is_alphanumeric() || matches!(c, '_' | '-' | '.') {
            c
        } else {
            '-'
        };
        if matches!(c, '-' | '.') && branch.ends_with(c) {
            continue;
        }
        branch.push(c);
    }
    let mut end = branch.len().min(BRANCH_AT_MOST);
    while !branch.is_char_boundary(end) {
        end -= 1;
    }
    branch.truncate(end);
    let mut branch = branch.trim_matches(['-', '.']).to_owned();
    if let Some(stem) = branch.strip_suffix(".lock") {
        branch = format!("{stem}-lock");
    }
    match branch.as_str() {
        "" => "workspace".into(),
        "HEAD" => "HEAD-workspace".into(),
        _ => branch,
    }
}

/// `s` as a disk that ignores case compares it: each character upper-cased, then lower-cased,
/// so `Fix` and `fix`, a final sigma and a sigma, or a long s and an s, come out the same.
fn fold(s: &str) -> String {
    s.chars()
        .flat_map(char::to_uppercase)
        .flat_map(char::to_lowercase)
        .collect()
}

/// This machine's workspaces.
pub struct Workspaces {
    inner: Arc<Inner>,
}

struct Inner {
    me: MachineId,
    store: Arc<Mutex<Store>>,
    terminals: Arc<Terminals>,
    data: PathBuf,
    git_within: Duration,
    /// The program run to ask GitHub about a branch's pull request.
    gh: std::ffi::OsString,
    /// Where a factory project's GitHub token is read from, when there is a store to read.
    secrets: Option<Arc<dyn Secrets>>,
    /// One lock per project, held across every step that changes its clone or worktrees.
    projects: Mutex<HashMap<ProjectId, Arc<tokio::sync::Mutex<()>>>>,
    /// One lock per workspace, held by an archive and by a start for their whole work
    /// ([`Workspaces::locked`]). A workspace no one holds or waits for has none.
    lifecycle: Mutex<HashMap<WorkspaceId, Arc<tokio::sync::Mutex<()>>>>,
    /// The workspaces whose worktree an archive is removing, from just before it starts to
    /// after the record says archived ([`Removing`]).
    removing: Mutex<HashSet<WorkspaceId>>,
}

impl Workspaces {
    /// The workspaces of machine `me`, with clones and worktrees under `data_dir` and setup
    /// run in `terminals`.
    pub fn new(
        me: MachineId,
        store: Arc<Mutex<Store>>,
        data_dir: &Path,
        terminals: Arc<Terminals>,
    ) -> Self {
        Self::with_git_within(me, store, data_dir, terminals, GIT_WITHIN)
    }

    fn with_git_within(
        me: MachineId,
        store: Arc<Mutex<Store>>,
        data_dir: &Path,
        terminals: Arc<Terminals>,
        git_within: Duration,
    ) -> Self {
        // Git reads a relative worktree path from the clone, not from here.
        let data = std::path::absolute(data_dir).unwrap_or_else(|_| data_dir.to_path_buf());
        Self {
            inner: Arc::new(Inner {
                me,
                store,
                terminals,
                data,
                git_within,
                gh: pr::gh(),
                secrets: None,
                projects: Mutex::default(),
                lifecycle: Mutex::default(),
                removing: Mutex::default(),
            }),
        }
    }

    /// These workspaces asking GitHub with the program at `gh` in place of `gh`.
    #[cfg(test)]
    pub(crate) fn with_gh(mut self, gh: &Path) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("set before the workspaces are shared")
            .gh = gh.into();
        self
    }

    /// These workspaces reading the GitHub tokens of factory projects from `secrets`. Without
    /// them, a create for a project whose factory is on fails with [`WorkspaceError::NoToken`].
    /// Set before the workspaces are shared.
    pub fn with_secrets(mut self, secrets: Arc<dyn Secrets>) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("set before the workspaces are shared")
            .secrets = Some(secrets);
        self
    }

    /// Makes a workspace of `project` named `name`, on a new branch from `base` on the
    /// project's remote, and starts its setup. Returns once the setup has started, not
    /// finished. [`WorkspaceError::SetupNotStarted`] and [`WorkspaceError::SetupNotRecorded`]
    /// mean the workspace was made all the same, with no setup running.
    /// The create finishes even when the caller stops waiting.
    pub async fn create(
        &self,
        project: ProjectId,
        base: &str,
        name: &str,
    ) -> Result<WorkspaceId, WorkspaceError> {
        let checkout = Checkout::NewBranch {
            base: base.to_owned(),
            branch: None,
        };
        self.create_with(project, name, checkout, Setup::Own, None)
            .await
    }

    /// Makes a workspace of `project` named `name` with the worktree `checkout` says and the
    /// setup `setup` says, and starts the setup. A branch the caller gives, or a commit, that
    /// is no plain name or full object id is refused before git runs. With a `factory` tag, a
    /// workspace this machine already made for the same tag and name, and not archived, is the
    /// answer, and nothing is made. Returns once the setup has started, not finished, with the
    /// same errors as [`Workspaces::create`]. The create finishes even when the caller stops
    /// waiting.
    pub async fn create_with(
        &self,
        project: ProjectId,
        name: &str,
        checkout: Checkout,
        setup: Setup,
        factory: Option<FactoryTag>,
    ) -> Result<WorkspaceId, WorkspaceError> {
        let inner = self.inner.clone();
        let name = name.to_owned();
        tokio::spawn(async move {
            inner
                .create(project, &name, checkout, setup, factory.as_ref())
                .await
        })
        .await?
    }

    /// Closes the workspace's terminals, removes its worktree and keeps its branch. Unless
    /// `force`, refuses with [`WorkspaceError::Dirty`] while there is uncommitted or unpushed
    /// work or a checked-out submodule, and changes nothing. Work that appears while the terminals close is refused with
    /// [`WorkspaceError::Dirty`] too, and then the terminals are already closed. The archive
    /// finishes even when the caller stops waiting, and holds the workspace's lifecycle lock
    /// throughout, so no [`Workspaces::start_in`] runs beside it.
    pub async fn archive(&self, id: WorkspaceId, force: bool) -> Result<(), WorkspaceError> {
        let inner = self.inner.clone();
        self.locked(id, async move { inner.archive(id, force).await })
            .await
    }

    /// Runs `start` with the worktree of live workspace `id`, to start a shell or session in it,
    /// holding the workspace's lifecycle lock for the whole start, so it never lands while the
    /// workspace is archived. A start queued behind an archive finds the workspace gone:
    /// [`WorkspaceError::Unknown`]. The start finishes even when the caller stops waiting.
    pub async fn start_in<T, E, F, Fut>(&self, id: WorkspaceId, start: F) -> Result<T, E>
    where
        F: FnOnce(PathBuf) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, E>> + Send + 'static,
        T: Send + 'static,
        E: From<WorkspaceError> + Send + 'static,
    {
        let inner = self.inner.clone();
        self.locked(id, async move {
            let body = inner.live(id).await?;
            start(inner.folder(&body).await).await
        })
        .await
    }

    /// The folder of a live workspace's worktree. It takes no lock, so it is for tests only: a
    /// start goes through [`Workspaces::start_in`].
    #[cfg(test)]
    pub(crate) async fn worktree(&self, id: WorkspaceId) -> Result<PathBuf, WorkspaceError> {
        let body = self.inner.live(id).await?;
        Ok(self.inner.folder(&body).await)
    }

    /// Runs `work` holding workspace `id`'s lifecycle lock, on a task of its own, so the lock is
    /// held until the work ends even when the caller stops waiting. Work that panics or is
    /// stopped with the runtime is [`WorkspaceError::Interrupted`].
    async fn locked<T, E>(
        &self,
        id: WorkspaceId,
        work: impl Future<Output = Result<T, E>> + Send + 'static,
    ) -> Result<T, E>
    where
        T: Send + 'static,
        E: From<WorkspaceError> + Send + 'static,
    {
        let (lock, user) = {
            let mut locks = lock(&self.inner.lifecycle);
            let held = Arc::clone(locks.entry(id).or_default());
            let user = User {
                inner: Arc::clone(&self.inner),
                id,
                lock: Some(Arc::clone(&held)),
            };
            (held, user)
        };
        let task = tokio::spawn(async move {
            // Dropped in the reverse order: the guard first, then the user.
            let _user = user;
            let _held = lock.lock_owned().await;
            work.await
        });
        match task.await {
            Ok(result) => result,
            Err(error) => Err(WorkspaceError::from(error).into()),
        }
    }

    /// Deals with each worktree under `worktrees/<project>/`, for each project in the shared
    /// settings, that no live workspace record in the store names, whatever left it: a create
    /// that died before writing its record leaves one. One that passes the checks of an
    /// unforced archive and holds no file git ignores, which such a create never leaves, is
    /// removed without `--force` and its branch kept; any other, or one git will not remove, is left as it is and logged with what
    /// holds it. One named only by another owner's record is left and logged too. A worktree
    /// whose folder is gone is checked by the `HEAD` git keeps for it. Branches are never
    /// touched. A store that cannot be read skips the sweep, and a clone that cannot be read or
    /// whose worktrees cannot be listed is passed over, each with a log line. After
    /// [`SWEEP_WITHIN`] in all it stops where it is, says which clone it stopped in, and leaves
    /// the rest for the next start.
    pub async fn sweep(&self) -> Result<(), WorkspaceError> {
        self.inner.sweep(SWEEP_WITHIN).await
    }
}

/// What a create is to make, once its checks have passed.
enum Plan {
    /// A new branch `branch` at `start`, from the remote branch `base`.
    New {
        branch: String,
        start: String,
        base: String,
    },
    /// The branch `branch` at `target`, `origin`'s, where `was` is where a local branch of that
    /// name stood, if there was one.
    Existing {
        branch: String,
        target: String,
        was: Option<String>,
    },
    /// No branch, at the commit `sha`.
    Detached { sha: String },
}

/// A branch a create made or moved: what undo needs to take it back.
struct Branched {
    reference: String,
    /// Where this create put it.
    made_at: String,
    /// Where it stood before, when it was there before.
    was: Option<String>,
}

/// One holder or waiter of a workspace's lifecycle lock. When the last one goes, its work
/// panicking included, the lock is dropped from the map.
struct User {
    inner: Arc<Inner>,
    id: WorkspaceId,
    lock: Option<Arc<tokio::sync::Mutex<()>>>,
}

impl Drop for User {
    fn drop(&mut self) {
        let mut locks = lock(&self.inner.lifecycle);
        // Users are made under the map's lock, and a user's guard is dropped before the user,
        // so the last user out sees the map's own clone as the only one left.
        self.lock = None;
        if locks
            .get(&self.id)
            .is_some_and(|held| Arc::strong_count(held) == 1)
        {
            locks.remove(&self.id);
        }
    }
}

impl Inner {
    async fn create(
        &self,
        project: ProjectId,
        name: &str,
        checkout: Checkout,
        provision: Setup,
        factory: Option<&FactoryTag>,
    ) -> Result<WorkspaceId, WorkspaceError> {
        // Before anything is read or run: a name that is no branch, or a commit that is no
        // object id, never reaches git.
        match &checkout {
            Checkout::NewBranch {
                branch: Some(branch),
                ..
            }
            | Checkout::Branch { name: branch }
                if branch_refused(branch) =>
            {
                return Err(WorkspaceError::BranchRefused(branch.clone()));
            }
            Checkout::Commit { sha } if !is_commit_id(sha) => {
                return Err(WorkspaceError::NotACommitId(sha.clone()));
            }
            _ => {}
        }
        let (repo, login) = self
            .with_store(move |store| {
                let settings = store.settings()?;
                let login = settings
                    .project_factory
                    .get(&project)
                    .filter(|f| f.on)
                    .map(|f| f.login.clone());
                Ok(settings
                    .projects
                    .into_iter()
                    .find(|p| p.id == project)
                    .map(|p| (p.repo, login)))
            })
            .await?
            .ok_or(WorkspaceError::UnknownProject(project))?;
        let id = WorkspaceId::random();
        let folder = self.id_folder(project, id);
        let lock = self.lock(project);
        let _held = lock.lock().await;
        if let Some(tag) = factory
            && let Some(made) = self.answered(project, tag, name).await?
        {
            return Ok(made);
        }
        // Before the clone and the fetch, so a project with no token makes nothing.
        let auth = match login {
            Some(login) => Some(self.auth(login).await?),
            None => None,
        };
        let clone = self.clone_for(project, &repo).await?;
        self.fetch(project, &repo, &clone, auth.as_ref()).await?;
        let plan = self.plan(project, &clone, &folder, name, &checkout).await?;
        let mut branched = None;
        let made = async {
            let (branch, base) = match &plan {
                Plan::New {
                    branch,
                    start,
                    base,
                } => {
                    // Git refuses a branch that is already there, by any spelling it reads as
                    // the same.
                    self.git(
                        &clone,
                        "branch",
                        ["--quiet", "--no-track", "--", branch, start],
                    )
                    .await?;
                    branched = Some(Branched {
                        reference: format!("refs/heads/{branch}"),
                        made_at: start.clone(),
                        was: None,
                    });
                    self.git(
                        &clone,
                        "worktree",
                        [
                            OsStr::new("add"),
                            OsStr::new("--quiet"),
                            OsStr::new("--"),
                            folder.as_os_str(),
                            OsStr::new(branch),
                        ],
                    )
                    .await?;
                    (Some(branch.as_str()), base.as_str())
                }
                Plan::Existing {
                    branch,
                    target,
                    was,
                } => {
                    branched = Some(Branched {
                        reference: format!("refs/heads/{branch}"),
                        made_at: target.clone(),
                        was: was.clone(),
                    });
                    // `-B` makes the branch at `origin`'s, or moves a stale one there.
                    self.git(
                        &clone,
                        "worktree",
                        [
                            OsStr::new("add"),
                            OsStr::new("--quiet"),
                            OsStr::new("--no-track"),
                            OsStr::new("-B"),
                            OsStr::new(branch),
                            OsStr::new("--"),
                            folder.as_os_str(),
                            OsStr::new(&format!("refs/remotes/origin/{branch}")),
                        ],
                    )
                    .await?;
                    (Some(branch.as_str()), branch.as_str())
                }
                Plan::Detached { sha } => {
                    self.git(
                        &clone,
                        "worktree",
                        [
                            OsStr::new("add"),
                            OsStr::new("--quiet"),
                            OsStr::new("--detach"),
                            OsStr::new("--"),
                            folder.as_os_str(),
                            OsStr::new(sha),
                        ],
                    )
                    .await?;
                    (None, sha.as_str())
                }
            };
            // Git may store the name in another spelling of the same text: on macOS it keeps
            // Unicode composed. The record takes git's, so every later path and ref this module
            // builds from it is spelled as git lists it.
            let stored = match branch {
                Some(branch) => {
                    let head = self.git(&folder, "symbolic-ref", ["HEAD"]).await?;
                    head.trim()
                        .strip_prefix("refs/heads/")
                        .unwrap_or(branch)
                        .to_owned()
                }
                None => String::new(),
            };
            let body = Workspace {
                id,
                project,
                name: name.to_owned(),
                branch: stored,
                base: base.to_owned(),
                setup: None,
            };
            if let Some(tag) = factory {
                self.mark(project, id, tag, name).await?;
            }
            self.write_new(body.clone()).await?;
            Ok(body)
        }
        .await;
        let body = match made {
            Ok(body) => body,
            Err(e) => {
                self.undo(&clone, &folder, project, id, branched.as_ref())
                    .await;
                return Err(e);
            }
        };
        let argv = match provision {
            Setup::Own => setup(&folder).await,
            Setup::Commands { commands, .. } if commands.is_empty() => None,
            Setup::Commands {
                commands,
                limit_secs,
            } => Some(sh(
                SETUP_LIMITED,
                std::iter::once(limit_secs.to_string()).chain(commands),
            )),
            Setup::None => None,
        };
        // Still under the lock, so an archive cannot run before the setup's terminal is known.
        if let Some(argv) = argv {
            // A factory workspace's terminal gets no token and no helper that answers for one.
            let env = if factory.is_some() {
                crate::git_auth::helpers_off_env()
            } else {
                Vec::new()
            };
            let terminal = self
                .terminals
                .start(id, &folder, &argv, &env)
                .await
                .map_err(|source| WorkspaceError::SetupNotStarted {
                    workspace: id,
                    source,
                })?;
            // The attention list reads a failed setup from this terminal, so a setup the
            // record does not name would fail unseen: it is stopped instead.
            let body = Workspace {
                setup: Some(terminal),
                ..body
            };
            if let Err(e) = self.write(body, false).await {
                if let Err(close) = self.terminals.close(terminal).await {
                    tracing::warn!(
                        "workspace {id}: its unrecorded setup terminal {terminal} could not be \
                         closed: {close}"
                    );
                }
                return Err(WorkspaceError::SetupNotRecorded {
                    workspace: id,
                    source: Box::new(e),
                });
            }
        }
        Ok(id)
    }

    /// What a create of `checkout` is to make, after the checks that make nothing: the refusals
    /// of a branch or folder that is taken, and of a base, branch or commit the remote has not.
    async fn plan(
        &self,
        project: ProjectId,
        clone: &Path,
        folder: &Path,
        name: &str,
        checkout: &Checkout,
    ) -> Result<Plan, WorkspaceError> {
        match checkout {
            Checkout::NewBranch { base, branch } => {
                let branch = branch.clone().unwrap_or_else(|| safe_branch(name));
                // Compared as the disk compares names: where it ignores case, `Fix` and `fix`
                // are one ref file and one folder. A folder already there is someone's,
                // whatever its branch.
                let branches = self
                    .git(
                        clone,
                        "for-each-ref",
                        ["--format=%(refname:lstrip=2)", "refs/heads/"],
                    )
                    .await?;
                let mut taken = branches.lines().any(|b| fold(b) == fold(&branch))
                    || self
                        .live_branches(project)
                        .await?
                        .iter()
                        .any(|b| fold(b) == fold(&branch))
                    || tokio::fs::try_exists(folder).await?;
                if !taken && legacy_able(&branch) {
                    let legacy = self.legacy_folder(project, &branch);
                    taken = tokio::fs::try_exists(&legacy).await?
                        || self.registered(clone, &legacy).await?.is_some();
                }
                if taken {
                    return Err(WorkspaceError::BranchExists(branch));
                }
                let start = self
                    .rev(clone, &format!("refs/remotes/origin/{base}^{{commit}}"))
                    .await?
                    .ok_or_else(|| WorkspaceError::UnknownBase(base.clone()))?;
                Ok(Plan::New {
                    branch,
                    start,
                    base: base.clone(),
                })
            }
            Checkout::Branch { name } => {
                let target = self
                    .rev(clone, &format!("refs/remotes/origin/{name}^{{commit}}"))
                    .await?
                    .ok_or_else(|| WorkspaceError::UnknownBase(name.clone()))?;
                // Git would refuse a branch another worktree has checked out; this says so.
                let list = self.git(clone, "worktree", ["list", "--porcelain"]).await?;
                let checked_out = worktrees_in(&list)
                    .iter()
                    .filter_map(|listed| listed.branch.as_deref())
                    .any(|b| fold(b) == fold(name))
                    || self
                        .live_branches(project)
                        .await?
                        .iter()
                        .any(|b| fold(b) == fold(name));
                if checked_out {
                    return Err(WorkspaceError::BranchCheckedOut(name.clone()));
                }
                let was = self.rev(clone, &format!("refs/heads/{name}")).await?;
                Ok(Plan::Existing {
                    branch: name.clone(),
                    target,
                    was,
                })
            }
            Checkout::Commit { sha } => {
                self.rev(clone, &format!("{sha}^{{commit}}"))
                    .await?
                    .ok_or_else(|| WorkspaceError::UnknownCommitId(sha.clone()))?;
                Ok(Plan::Detached { sha: sha.clone() })
            }
        }
    }

    /// The commit `reference` names in `clone`, or `None` when it names none.
    async fn rev(&self, clone: &Path, reference: &str) -> Result<Option<String>, WorkspaceError> {
        match self
            .git(clone, "rev-parse", ["--verify", "--quiet", reference])
            .await
        {
            Ok(out) => Ok(Some(out.trim().to_owned())),
            // What `--verify --quiet` gives for a ref that is not there.
            Err(WorkspaceError::Git { ref stderr, .. }) if stderr.is_empty() => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The branches the live workspaces of `project` on this machine name.
    async fn live_branches(&self, project: ProjectId) -> Result<Vec<String>, WorkspaceError> {
        let me = self.me;
        self.with_store(move |store| {
            Ok(store
                .live_workspaces()?
                .into_iter()
                .filter(|r| r.owner == me && r.body.project == project)
                .map(|r| r.body.branch)
                .filter(|branch| !branch.is_empty())
                .collect())
        })
        .await
    }

    /// The token for `login`, or why this machine has none.
    async fn auth(&self, login: String) -> Result<GitAuth, WorkspaceError> {
        let Some(secrets) = self.secrets.clone() else {
            return Err(NoToken::Unset { login }.into());
        };
        let data = self.data.clone();
        let auth =
            tokio::task::spawn_blocking(move || GitAuth::for_login(&*secrets, &login, &data))
                .await??;
        Ok(auth)
    }

    /// `git fetch` of the project's remote into `clone`. With a token, the fetch runs in a bare
    /// repo only the daemon writes, since the token must not go to a call that reads a config a
    /// worktree can write, and the clone then fetches from that repo with no token.
    async fn fetch(
        &self,
        project: ProjectId,
        repo: &str,
        clone: &Path,
        auth: Option<&GitAuth>,
    ) -> Result<(), WorkspaceError> {
        // A remote branch deleted since the last fetch is kept, so work merged and then deleted
        // on the remote still counts as pushed.
        let Some(auth) = auth else {
            self.git(clone, "fetch", ["--quiet", "--no-prune", "origin"])
                .await?;
            return Ok(());
        };
        let fetches = self
            .bare_for(&self.data.join("fetches"), project, repo)
            .await?;
        self.git_with(
            &fetches,
            auth,
            repo,
            "fetch",
            ["--quiet", "--no-prune", "origin"],
        )
        .await?;
        self.git(
            clone,
            "fetch",
            [
                OsStr::new("--quiet"),
                OsStr::new("--no-prune"),
                OsStr::new("--"),
                fetches.as_os_str(),
                OsStr::new("+refs/remotes/origin/*:refs/remotes/origin/*"),
            ],
        )
        .await?;
        Ok(())
    }

    /// The marker of a tagged workspace, written beside its folder before its record, so a
    /// create that dies in between leaves a marker no live record answers for.
    async fn mark(
        &self,
        project: ProjectId,
        id: WorkspaceId,
        tag: &FactoryTag,
        name: &str,
    ) -> Result<(), WorkspaceError> {
        let marker = Marker {
            project: tag.project,
            run: tag.run.clone(),
            name: name.to_owned(),
        };
        let json = serde_json::to_vec(&marker).map_err(io::Error::other)?;
        tokio::fs::write(self.marker_path(project, id), json).await?;
        Ok(())
    }

    fn marker_path(&self, project: ProjectId, id: WorkspaceId) -> PathBuf {
        self.data
            .join("worktrees")
            .join(project.to_string())
            .join(format!("{id}.factory"))
    }

    /// The live workspace of this machine that a create with `tag` and `name` was made for
    /// before, if there is one. A marker whose workspace is not live, or that cannot be read,
    /// answers for nothing.
    async fn answered(
        &self,
        project: ProjectId,
        tag: &FactoryTag,
        name: &str,
    ) -> Result<Option<WorkspaceId>, WorkspaceError> {
        let dir = self.data.join("worktrees").join(project.to_string());
        let mut entries = match tokio::fs::read_dir(&dir).await {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        while let Some(entry) = entries.next_entry().await? {
            let file = entry.file_name();
            let Some(id) = file
                .to_str()
                .and_then(|f| f.strip_suffix(".factory"))
                .and_then(|id| uuid::Uuid::parse_str(id).ok())
                .map(WorkspaceId)
            else {
                continue;
            };
            let Ok(text) = tokio::fs::read(entry.path()).await else {
                continue;
            };
            let Ok(marker) = serde_json::from_slice::<Marker>(&text) else {
                continue;
            };
            if marker.project == tag.project
                && marker.run == tag.run
                && marker.name == name
                && self
                    .live(id)
                    .await
                    .is_ok_and(|body| body.project == project)
            {
                return Ok(Some(id));
            }
        }
        Ok(None)
    }

    async fn archive(&self, id: WorkspaceId, force: bool) -> Result<(), WorkspaceError> {
        let project = self.live(id).await?.project;
        let lock = self.lock(project);
        let _held = lock.lock().await;
        // Read again under the lock, so two archives of one workspace do not both run.
        let body = self.live(id).await?;
        let clone = self.clone_path(project);
        let worktree = self.folder(&body).await;
        if !force {
            self.refuse_dirty(&clone, &worktree, &body.branch).await?;
        }
        for terminal in self.terminals_of(id).await? {
            match self.terminals.close(terminal).await {
                Ok(()) | Err(TerminalError::Unknown(_)) => {}
                Err(e) => return Err(e.into()),
            }
        }
        let there = tokio::fs::try_exists(&worktree).await?;
        if there && !force {
            // What the terminals wrote as they were hung up counts too.
            self.refuse_dirty(&clone, &worktree, &body.branch).await?;
        }
        let _removing = Removing::mark(self, id);
        if there {
            self.remove_worktree(&clone, &worktree, force).await?;
        } else if let Some((listed, _)) = self.registered_either(&clone, &body).await? {
            // Removed by hand: git forgets this worktree, and only this one, so its branch can be
            // checked out again. Named as git listed it, since git cannot resolve a path whose
            // folders are gone.
            self.remove_worktree(&clone, &listed, force).await?;
        }
        let (project, id) = (body.project, body.id);
        self.write(body, true).await?;
        // A tagged workspace's marker goes once its record says archived; a marker left is
        // ignored, as its workspace is not live.
        match tokio::fs::remove_file(self.marker_path(project, id)).await {
            Err(e) if e.kind() != io::ErrorKind::NotFound => tracing::warn!(
                workspace = %id,
                error = %e,
                "an archived workspace's factory marker could not be removed"
            ),
            _ => {}
        }
        Ok(())
    }

    /// The worktree git has registered for `body`'s workspace, in the folder named by its id or,
    /// for a branch P1 could have made a folder for, in the one named by its branch, whether or
    /// not the folder is still there.
    async fn registered_either(
        &self,
        clone: &Path,
        body: &Workspace,
    ) -> Result<Option<(PathBuf, String)>, WorkspaceError> {
        if let Some(found) = self
            .registered(clone, &self.id_folder(body.project, body.id))
            .await?
        {
            return Ok(Some(found));
        }
        if legacy_able(&body.branch) {
            return self
                .registered(clone, &self.legacy_folder(body.project, &body.branch))
                .await;
        }
        Ok(None)
    }

    async fn sweep(&self, within: Duration) -> Result<(), WorkspaceError> {
        let deadline = tokio::time::Instant::now() + within;
        // Every owner's records count: the machine's identity can be made again while its data
        // directory stays, and a record only ever keeps a worktree. Projects come from the
        // settings, as a create's do, so a store made again removes nothing until settings reach
        // it. Once they do, it removes the clean worktrees of live workspaces whose records it
        // does not hold yet; each holds nothing git cannot give back, and its branch is kept.
        let read = self
            .with_store(|store| {
                let projects: Vec<ProjectId> = store
                    .settings()?
                    .projects
                    .into_iter()
                    .map(|p| p.id)
                    .collect();
                let live: Vec<_> = store
                    .live_workspaces()?
                    .into_iter()
                    .map(|r| (r.owner, r.body))
                    .collect();
                Ok((projects, live))
            })
            .await;
        // Without every workspace record, a live workspace's worktree could look like a
        // leftover.
        let (projects, live) = match read {
            Ok(read) => read,
            Err(error) => {
                tracing::warn!(
                    %error,
                    "the store could not be read, so the sweep for worktrees no live workspace \
                     record names is skipped this start"
                );
                return Ok(());
            }
        };
        for (at, &project) in projects.iter().enumerate() {
            let clone = self.clone_path(project);
            match tokio::fs::try_exists(&clone).await {
                Ok(true) => {}
                Ok(false) => continue,
                Err(error) => {
                    tracing::warn!(
                        clone = %clone.display(),
                        %error,
                        "the clone could not be read to sweep its worktrees"
                    );
                    continue;
                }
            }
            let named = |mine: bool| -> Vec<(WorkspaceId, &str)> {
                live.iter()
                    .filter(|(owner, w)| w.project == project && (*owner == self.me) == mine)
                    .map(|(_, w)| (w.id, w.branch.as_str()))
                    .collect()
            };
            let swept = tokio::time::timeout_at(
                deadline,
                self.sweep_project(project, &named(true), &named(false)),
            )
            .await;
            match swept {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::warn!(
                    clone = %clone.display(),
                    %error,
                    "the clone's worktrees could not be listed to sweep them"
                ),
                Err(_) => {
                    tracing::warn!(
                        clone = %clone.display(),
                        projects_after = projects.len() - at - 1,
                        "the sweep ran out of time in this clone; it and the projects after it \
                         are swept at the next start"
                    );
                    break;
                }
            }
        }
        Ok(())
    }

    /// [`Workspaces::sweep`] for one project's clone, whose live workspaces of this machine are
    /// on `mine` and of other owners on `others`, each as its id and branch.
    async fn sweep_project(
        &self,
        project: ProjectId,
        mine: &[(WorkspaceId, &str)],
        others: &[(WorkspaceId, &str)],
    ) -> Result<(), WorkspaceError> {
        let lock = self.lock(project);
        let _held = lock.lock().await;
        let clone = self.clone_path(project);
        let list = self
            .git(&clone, "worktree", ["list", "--porcelain"])
            .await?;
        let project_folder = project.to_string();
        for Listed {
            path: listed,
            branch,
            ..
        } in worktrees_in(&list)
        {
            // Only what a create makes, `worktrees/<project>/<id>` or P1's `<branch>`, and never
            // the clone.
            let parent = listed.parent();
            if parent.and_then(Path::file_name) != Some(OsStr::new(&project_folder))
                || parent.and_then(Path::parent).and_then(Path::file_name)
                    != Some(OsStr::new("worktrees"))
            {
                continue;
            }
            let Some(folder) = listed.file_name().and_then(OsStr::to_str) else {
                continue;
            };
            let worktree = listed.display();
            let branch = branch.as_deref();
            if self.named(project, &listed, folder, branch, mine).await {
                continue;
            }
            if self.named(project, &listed, folder, branch, others).await {
                tracing::warn!(
                    %worktree,
                    "a worktree only another owner's workspace record names, another machine's or \
                     this one's under an identity since made again, is left as it is"
                );
                continue;
            }
            // A detached worktree has no branch to check, and `folder` is no branch of its own.
            let checked = match self
                .refuse_dirty(&clone, &listed, branch.unwrap_or(folder))
                .await
            {
                Ok(()) => self.ignored(&listed).await,
                Err(error) => Err(error),
            };
            match checked {
                Ok(ignored) if ignored.is_empty() => {
                    match self.remove_worktree(&clone, &listed, false).await {
                        Ok(_) => tracing::info!(
                            %worktree,
                            "removed a worktree no live workspace record names; its branch is \
                             kept"
                        ),
                        Err(error) => tracing::warn!(
                            %worktree,
                            %error,
                            "a worktree no workspace names could not be removed and is left as it is"
                        ),
                    }
                }
                Ok(ignored) => tracing::warn!(
                    %worktree,
                    ?ignored,
                    "a worktree no workspace names holds files git ignores, which a failed \
                     create never leaves, and is left as it is"
                ),
                Err(WorkspaceError::Dirty(dirty)) => tracing::warn!(
                    %worktree,
                    uncommitted = ?dirty.uncommitted,
                    unpushed = ?dirty.unpushed,
                    submodules = ?dirty.submodules,
                    "a worktree no workspace names holds work and is left as it is"
                ),
                Err(error) => tracing::warn!(
                    %worktree,
                    %error,
                    "a worktree no workspace names could not be checked and is left as it is"
                ),
            }
        }
        Ok(())
    }

    /// Files and folders in `worktree` that git ignores, sorted; none when its folder is gone.
    /// Git cannot give them back once the worktree is removed.
    async fn ignored(&self, worktree: &Path) -> Result<Vec<String>, WorkspaceError> {
        if !tokio::fs::try_exists(worktree).await? {
            return Ok(Vec::new());
        }
        let out = self
            .git(
                worktree,
                "ls-files",
                [
                    "-z",
                    "--others",
                    "--ignored",
                    "--exclude-standard",
                    "--directory",
                ],
            )
            .await?;
        let mut paths: Vec<String> = out
            .split('\0')
            .filter(|path| !path.is_empty())
            .map(str::to_owned)
            .collect();
        paths.sort();
        Ok(paths)
    }

    /// Whether a live workspace of `project`, one of whose ids and branches is on `live`, names
    /// the worktree git lists at `listed`, in `folder`, with `branch` checked out. Any doubt
    /// counts as named: its id is the folder's name, its branch is the one checked out, its
    /// folder's name is its branch's on a disk that ignores case, or its folder is the one P1
    /// named by its branch, however each path is spelled. A workspace with no branch is named by
    /// its id alone.
    async fn named(
        &self,
        project: ProjectId,
        listed: &Path,
        folder: &str,
        branch: Option<&str>,
        live: &[(WorkspaceId, &str)],
    ) -> bool {
        for (id, name) in live {
            if folder == id.to_string()
                || (!name.is_empty()
                    && (branch == Some(*name)
                        || fold(name) == fold(folder)
                        || same_folder(listed, &self.legacy_folder(project, name)).await))
            {
                return true;
            }
        }
        false
    }

    /// `git worktree remove`, passing `--force` only when `force`. Without it git refuses a
    /// worktree with anything uncommitted in it, or with a submodule checked out.
    async fn remove_worktree(
        &self,
        clone: &Path,
        worktree: &Path,
        force: bool,
    ) -> Result<String, WorkspaceError> {
        let mut args = vec![OsStr::new("remove")];
        if force {
            args.push(OsStr::new("--force"));
        }
        args.extend([OsStr::new("--"), worktree.as_os_str()]);
        self.git(clone, "worktree", args).await
    }

    /// Takes back what a create that failed made. The worktree's folder was not there before
    /// this create, under the project's lock, so whatever is in it now is this create's and
    /// holds nothing of anyone's. A branch this create made goes, and one it moved goes back
    /// where it stood, only while it still points where this create put it. A step that finds
    /// nothing to undo fails, and that is fine; one that leaves something behind is logged.
    async fn undo(
        &self,
        clone: &Path,
        worktree: &Path,
        project: ProjectId,
        id: WorkspaceId,
        branched: Option<&Branched>,
    ) {
        if matches!(tokio::fs::try_exists(worktree).await, Ok(true))
            && let Err(error) = self.remove_worktree(clone, worktree, true).await
        {
            tracing::warn!(
                worktree = %worktree.display(),
                %error,
                "a failed create left its worktree behind"
            );
        }
        match tokio::fs::remove_file(self.marker_path(project, id)).await {
            Err(e) if e.kind() != io::ErrorKind::NotFound => tracing::warn!(
                error = %e,
                "a failed create left its factory marker behind"
            ),
            _ => {}
        }
        let Some(Branched {
            reference,
            made_at,
            was,
        }) = branched
        else {
            return;
        };
        let undone = match was {
            None => {
                self.git(clone, "update-ref", ["-d", reference, made_at])
                    .await
            }
            Some(was) => {
                self.git(clone, "update-ref", [reference, was, made_at])
                    .await
            }
        };
        if let Err(error) = undone {
            let left = match self.rev(clone, reference).await {
                Ok(now) => now.as_ref() != was.as_ref(),
                Err(_) => true,
            };
            if left {
                tracing::warn!(%reference, %error, "a failed create left its branch behind");
            }
        }
    }

    /// Whether `reference` names a commit in `clone`.
    async fn has_ref(&self, clone: &Path, reference: &str) -> Result<bool, WorkspaceError> {
        match self
            .git(clone, "rev-parse", ["--verify", "--quiet", reference])
            .await
        {
            Ok(_) => Ok(true),
            // What `--verify --quiet` gives for a ref that is not there.
            Err(WorkspaceError::Git { ref stderr, .. }) if stderr.is_empty() => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// The path git lists, and the commit it keeps as `HEAD`, for a worktree registered at
    /// `worktree`, whether or not its folder is still there. `None` when none is registered
    /// there.
    async fn registered(
        &self,
        clone: &Path,
        worktree: &Path,
    ) -> Result<Option<(PathBuf, String)>, WorkspaceError> {
        let list = self.git(clone, "worktree", ["list", "--porcelain"]).await?;
        // Every worktree of a clone sits at `worktrees/<project>/<branch>`, so the last two
        // parts name it, however git spelled the folders above.
        let tail = |p: &Path| {
            (
                p.file_name().map(OsStr::to_owned),
                p.parent().and_then(Path::file_name).map(OsStr::to_owned),
            )
        };
        Ok(worktrees_in(&list).into_iter().find_map(|listed| {
            let head = listed
                .head
                .filter(|_| tail(&listed.path) == tail(worktree))?;
            Some((listed.path, head))
        }))
    }

    /// [`WorkspaceError::Dirty`] when the worktree has uncommitted files or a checked-out
    /// submodule, or the branch or the worktree's `HEAD` has commits no remote branch has. With
    /// the folder there, the `HEAD` is read inside it, which git resolves however the path is
    /// spelled; with it gone, it is the one git keeps for the worktree.
    async fn refuse_dirty(
        &self,
        clone: &Path,
        worktree: &Path,
        branch: &str,
    ) -> Result<(), WorkspaceError> {
        // The branch can be renamed or deleted inside the workspace; the worktree's HEAD still
        // holds what was on it.
        let reference = format!("refs/heads/{branch}");
        let mut heads = Vec::new();
        if !branch.is_empty() && self.has_ref(clone, &reference).await? {
            heads.push(reference);
        }
        let present = tokio::fs::try_exists(worktree).await?;
        if present {
            let head = self
                .git(worktree, "rev-parse", ["--verify", "HEAD"])
                .await?;
            heads.push(head.trim().to_owned());
        } else if let Some((_, head)) = self.registered(clone, worktree).await? {
            heads.push(head);
        }
        let (uncommitted, submodules) = if present {
            let submodules = self
                .git(
                    worktree,
                    "submodule",
                    [
                        "foreach",
                        "--quiet",
                        "--recursive",
                        r#"printf '%s\n' "$displaypath""#,
                    ],
                )
                .await?
                .lines()
                .map(str::to_owned)
                .collect();
            (self.uncommitted(worktree, None).await?, submodules)
        } else {
            (Vec::new(), Vec::new())
        };
        // With no head left to check, git lists nothing.
        let unpushed = self
            .git(
                clone,
                "log",
                ["--format=%h %s".to_owned()]
                    .into_iter()
                    .chain(heads)
                    .chain(["--not", "--remotes", "--"].map(String::from)),
            )
            .await?
            .lines()
            .map(str::to_owned)
            .collect();
        let dirty = Dirty {
            uncommitted,
            unpushed,
            submodules,
        };
        if dirty.is_empty() {
            Ok(())
        } else {
            Err(WorkspaceError::Dirty(dirty))
        }
    }

    /// Paths in `worktree` that are changed, deleted or new, not committed and not ignored,
    /// sorted. Read with git's index at `index` in place of the worktree's own, when given.
    async fn uncommitted(
        &self,
        worktree: &Path,
        index: Option<&Path>,
    ) -> Result<Vec<String>, WorkspaceError> {
        let out = self
            .git_on(
                worktree,
                index,
                "status",
                ["--porcelain=v1", "-z", "--untracked-files=all"],
            )
            .await?;
        let mut paths = Vec::new();
        let mut entries = out.split('\0');
        while let Some(entry) = entries.next() {
            let Some(path) = entry.get(3..) else {
                continue;
            };
            // A rename or copy is followed by the path it came from.
            if entry.starts_with(['R', 'C']) {
                entries.next();
            }
            paths.push(path.to_owned());
        }
        paths.sort();
        paths.dedup();
        Ok(paths)
    }

    /// Runs `git <command> <args>` in `dir` and returns its standard output. A call that runs
    /// past `git_within` is killed and fails. Git never asks for a password: there is no one to
    /// answer.
    async fn git<I, S>(
        &self,
        dir: &Path,
        command: &'static str,
        args: I,
    ) -> Result<String, WorkspaceError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.git_on(dir, None, command, args).await
    }

    /// [`Inner::git`] with git's index at `index` in place of the worktree's own, when given.
    async fn git_on<I, S>(
        &self,
        dir: &Path,
        index: Option<&Path>,
        command: &'static str,
        args: I,
    ) -> Result<String, WorkspaceError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let failed = |stderr: String| WorkspaceError::Git { command, stderr };
        let mut run = tokio::process::Command::new("git");
        if let Some(index) = index {
            run.env("GIT_INDEX_FILE", index);
        }
        let run = run
            .arg(command)
            .args(args)
            .current_dir(dir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output();
        let out = match tokio::time::timeout(self.git_within, run).await {
            Ok(Ok(out)) => out,
            Ok(Err(e)) => return Err(failed(format!("git could not be run: {e}"))),
            Err(_) => {
                return Err(failed(format!(
                    "did not finish within {} s and was stopped",
                    self.git_within.as_secs_f32()
                )));
            }
        };
        if !out.status.success() {
            return Err(failed(
                String::from_utf8_lossy(&out.stderr).trim().to_owned(),
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// [`Inner::git`] with `auth`'s token and hardening, for a call that only runs in a repo
    /// the daemon alone writes ([`GitAuth::apply`]). It allows `https` only, and `file` too when
    /// `repo` is a folder of this machine or a `file://` URL, as a test's remote is. Neither the
    /// token nor the command, which holds it, is ever in the error.
    async fn git_with<I, S>(
        &self,
        dir: &Path,
        auth: &GitAuth,
        repo: &str,
        command: &'static str,
        args: I,
    ) -> Result<String, WorkspaceError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let failed = |stderr: String| WorkspaceError::Git { command, stderr };
        let mut run = tokio::process::Command::new("git");
        auth.apply(run.as_std_mut());
        if repo.starts_with('/') || repo.starts_with("file://") {
            run.env("GIT_ALLOW_PROTOCOL", "https:file");
        }
        let run = run
            .arg(command)
            .args(args)
            .current_dir(dir)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output();
        let out = match tokio::time::timeout(self.git_within, run).await {
            Ok(Ok(out)) => out,
            Ok(Err(e)) => return Err(failed(format!("git could not be run: {e}"))),
            Err(_) => {
                return Err(failed(format!(
                    "did not finish within {} s and was stopped",
                    self.git_within.as_secs_f32()
                )));
            }
        };
        if !out.status.success() {
            return Err(failed(
                String::from_utf8_lossy(&out.stderr).trim().to_owned(),
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// The project's bare clone, made with `origin` set to `repo` when this machine has none.
    /// See [`Inner::bare_for`].
    async fn clone_for(&self, project: ProjectId, repo: &str) -> Result<PathBuf, WorkspaceError> {
        self.bare_for(&self.data.join("clones"), project, repo)
            .await
    }

    /// The project's bare repo `<parent>/<project>.git`, made with `origin` set to `repo` when
    /// this machine has none. It is made under another name and renamed when ready, so a repo
    /// that stopped half way is never taken for a made one. One already there that the shared
    /// settings have moved to another repo forgets the old repo's branches, so none of them
    /// chooses a base or counts as pushed, and then has `origin` set to `repo`. The URL changes
    /// last, so a move that stops part way is made again by the next create.
    async fn bare_for(
        &self,
        parent: &Path,
        project: ProjectId,
        repo: &str,
    ) -> Result<PathBuf, WorkspaceError> {
        let bare = parent.join(format!("{project}.git"));
        if tokio::fs::try_exists(&bare).await? {
            let origin = self
                .git(&bare, "config", ["--get", "remote.origin.url"])
                .await?;
            if origin.trim_end_matches('\n') != repo {
                let old = self
                    .git(
                        &bare,
                        "for-each-ref",
                        ["--format=%(refname)", "refs/remotes/origin/"],
                    )
                    .await?;
                for reference in old.lines() {
                    self.git(&bare, "update-ref", ["-d", reference]).await?;
                }
                self.git(&bare, "remote", ["set-url", "--", "origin", repo])
                    .await?;
            }
            return Ok(bare);
        }
        tokio::fs::create_dir_all(parent).await?;
        let partial = parent.join(format!("{project}.partial"));
        match tokio::fs::remove_dir_all(&partial).await {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
        self.git(
            parent,
            "init",
            [
                OsStr::new("--quiet"),
                OsStr::new("--bare"),
                OsStr::new("--"),
                partial.as_os_str(),
            ],
        )
        .await?;
        self.git(&partial, "remote", ["add", "--", "origin", repo])
            .await?;
        tokio::fs::rename(&partial, &bare).await?;
        Ok(bare)
    }

    fn clone_path(&self, project: ProjectId) -> PathBuf {
        self.data.join("clones").join(format!("{project}.git"))
    }

    /// Where a P1 create made the worktree of `branch`: `worktrees/<project>/<branch>`.
    fn legacy_folder(&self, project: ProjectId, branch: &str) -> PathBuf {
        self.data
            .join("worktrees")
            .join(project.to_string())
            .join(branch)
    }

    /// Where a create makes the worktree of workspace `id`.
    fn id_folder(&self, project: ProjectId, id: WorkspaceId) -> PathBuf {
        self.legacy_folder(project, &id.to_string())
    }

    /// The folder of a workspace's worktree: the one named by its id, unless that is not there
    /// and the one P1 named by its branch is. P1's folders were always [`safe_branch`] names,
    /// so a branch that is not one never names a folder, and cannot lead out of the project's.
    async fn folder(&self, body: &Workspace) -> PathBuf {
        let by_id = self.id_folder(body.project, body.id);
        if !matches!(tokio::fs::try_exists(&by_id).await, Ok(false)) || !legacy_able(&body.branch) {
            return by_id;
        }
        let legacy = self.legacy_folder(body.project, &body.branch);
        if matches!(tokio::fs::try_exists(&legacy).await, Ok(true)) {
            legacy
        } else {
            by_id
        }
    }

    fn lock(&self, project: ProjectId) -> Arc<tokio::sync::Mutex<()>> {
        lock(&self.projects).entry(project).or_default().clone()
    }

    /// The body of workspace `id` when it is one of this machine's and not archived.
    async fn live(&self, id: WorkspaceId) -> Result<Workspace, WorkspaceError> {
        let me = self.me;
        self.with_store(move |store| {
            Ok(store
                .workspace(id)?
                .filter(|r| r.owner == me && !r.deleted)
                .map(|r| r.body))
        })
        .await?
        .ok_or(WorkspaceError::Unknown(id))
    }

    /// The terminals of workspace `id` that are not closed.
    async fn terminals_of(&self, id: WorkspaceId) -> Result<Vec<TerminalId>, WorkspaceError> {
        let me = self.me;
        self.with_store(move |store| {
            Ok(store
                .terminals_in(me, id)?
                .into_iter()
                .filter(|r| !r.deleted)
                .map(|r| r.body.id)
                .collect())
        })
        .await
    }

    /// Writes the first record of a workspace this machine made, once the shared settings, read
    /// under the same lock of the store, still hold its project; else writes nothing and fails
    /// with [`WorkspaceError::UnknownProject`]. A remove numbered on this machine therefore
    /// lands before the check, which sees it, or after the record, which the acting machine
    /// counts once it has it.
    async fn write_new(&self, body: Workspace) -> Result<(), WorkspaceError> {
        let me = self.me;
        let project = body.project;
        let written = self
            .with_store(move |store| {
                if !store.settings()?.projects.iter().any(|p| p.id == project) {
                    return Ok(false);
                }
                let record = Record {
                    owner: me,
                    version: store.own_next_version()?,
                    deleted: false,
                    body,
                };
                store.apply(&AnyRecord::Workspace(record), me)?;
                Ok(true)
            })
            .await?;
        if written {
            Ok(())
        } else {
            Err(WorkspaceError::UnknownProject(project))
        }
    }

    async fn write(&self, body: Workspace, deleted: bool) -> Result<(), WorkspaceError> {
        let me = self.me;
        self.with_store(move |store| {
            let record = Record {
                owner: me,
                version: store.own_next_version()?,
                deleted,
                body,
            };
            store.apply(&AnyRecord::Workspace(record), me)?;
            Ok(())
        })
        .await
    }

    async fn with_store<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Store) -> Result<T, StoreError> + Send + 'static,
    ) -> Result<T, WorkspaceError> {
        let store = self.store.clone();
        Ok(tokio::task::spawn_blocking(move || f(&mut lock(&store))).await??)
    }
}

/// The setup to run in a new worktree, as the argv of its terminal: the `factory.provision`
/// commands of its `.dw/config.yml`, or a setup that fails saying why the file could not be
/// read. `None` when there is no file or no command in it.
async fn setup(worktree: &Path) -> Option<Vec<String>> {
    #[derive(Deserialize)]
    struct Config {
        factory: Option<Factory>,
    }
    #[derive(Deserialize)]
    struct Factory {
        #[serde(default)]
        provision: Vec<String>,
    }

    let unreadable = |e: &dyn fmt::Display| {
        let reason = format!(".dw/config.yml could not be read: {e}");
        sh(SETUP_UNREADABLE, [reason])
    };
    let text = match tokio::fs::read_to_string(worktree.join(".dw").join("config.yml")).await {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
        Err(e) => return Some(unreadable(&e)),
    };
    match serde_yaml_ng::from_str::<Config>(&text) {
        Ok(Config {
            factory: Some(Factory { provision }),
        }) if !provision.is_empty() => Some(sh(SETUP, provision)),
        Ok(_) => None,
        Err(e) => Some(unreadable(&e)),
    }
}

/// One worktree as `git worktree list --porcelain` gives it.
struct Listed {
    path: PathBuf,
    /// The commit git keeps as its `HEAD`; a bare clone has none.
    head: Option<String>,
    /// The branch it has checked out; none when its `HEAD` is detached.
    branch: Option<String>,
}

/// Each worktree in the output of `git worktree list --porcelain`, one field per line. `-z`
/// would need git 2.36. The paths that matter end in `<project>/<branch>`, and neither part
/// can hold a line break.
fn worktrees_in(list: &str) -> Vec<Listed> {
    let mut found: Vec<Listed> = Vec::new();
    for field in list.lines() {
        if let Some(path) = field.strip_prefix("worktree ") {
            found.push(Listed {
                path: PathBuf::from(path),
                head: None,
                branch: None,
            });
        } else if let Some(listed) = found.last_mut() {
            if let Some(head) = field.strip_prefix("HEAD ") {
                listed.head = Some(head.to_owned());
            } else if let Some(branch) = field.strip_prefix("branch refs/heads/") {
                listed.branch = Some(branch.to_owned());
            }
        }
    }
    found
}

/// Whether `a` and `b` are one folder on disk, however each is spelled. `false` when either
/// cannot be read.
async fn same_folder(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    match (tokio::fs::metadata(a).await, tokio::fs::metadata(b).await) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

/// `/bin/sh` running `script` with `args` as its positional parameters, so no argument is
/// ever parsed as shell.
fn sh(script: &str, args: impl IntoIterator<Item = String>) -> Vec<String> {
    ["/bin/sh", "-c", script, "setup"]
        .into_iter()
        .map(str::to_owned)
        .chain(args)
        .collect()
}

/// Marks a workspace's worktree as being removed until dropped. An archive holds it from just
/// before it removes the worktree until its record says archived, so a read that overlaps any of
/// that, which takes no lock, can tell its result is of a worktree half removed.
struct Removing<'a> {
    inner: &'a Inner,
    id: WorkspaceId,
}

impl<'a> Removing<'a> {
    fn mark(inner: &'a Inner, id: WorkspaceId) -> Self {
        lock(&inner.removing).insert(id);
        Removing { inner, id }
    }
}

impl Drop for Removing<'_> {
    fn drop(&mut self) {
        lock(&self.inner.removing).remove(&self.id);
    }
}

impl Inner {
    fn removing(&self, id: WorkspaceId) -> bool {
        lock(&self.removing).contains(&id)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
