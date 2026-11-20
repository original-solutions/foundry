//! Bare clones, worktrees, setup commands and archiving.
//!
//! A machine keeps one bare clone per project at `<data dir>/clones/<project>.git`, made the
//! first time it hosts a workspace of that project, with `origin` set to the project's repo.
//! A workspace is a worktree of that clone at `<data dir>/worktrees/<project>/<branch>`, on a
//! new branch made from `origin/<base>` after a fetch. Its setup commands, the
//! `factory.provision` list in the worktree's own `.dw/config.yml`, run in a terminal of the
//! workspace. A setup that fails leaves the workspace as it is: the terminal shows what failed
//! and exits with the failing command's code.
//!
//! A create never touches what it did not make: it refuses a name whose branch or folder is
//! already there, makes the branch itself, and when it fails takes back only that branch, while
//! it still points where it was made, and the worktree in a folder that was not there before.
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
//! Every branch is made by [`safe_branch`] and every path reaches git after `--`, so a name as
//! typed never reads as an option and never leaves the project's folder of worktrees.

use std::collections::HashMap;
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
use serde::Deserialize;

use crate::store::{Store, StoreError};
use crate::terminals::{TerminalError, Terminals};

mod changes;
#[cfg(test)]
mod tests;

pub use changes::{Changes, Commit, FileChange, FileStatus};

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

/// Prints the reason given after it and fails.
const SETUP_UNREADABLE: &str = r#"printf 'setup failed: %s\n' "$1"; exit 1"#;

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    #[error("project {0} is not in the shared settings")]
    UnknownProject(ProjectId),
    #[error("workspace {0} is not a live workspace of this machine")]
    Unknown(WorkspaceId),
    #[error(
        "a branch or worktree named {0} already exists in this project; give the workspace \
         another name"
    )]
    BranchExists(String),
    #[error("the project's remote has no branch {0}")]
    UnknownBase(String),
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
    /// One lock per project, held across every step that changes its clone or worktrees.
    projects: Mutex<HashMap<ProjectId, Arc<tokio::sync::Mutex<()>>>>,
    /// One lock per workspace, held by an archive and by a start for their whole work
    /// ([`Workspaces::locked`]). A workspace no one holds or waits for has none.
    lifecycle: Mutex<HashMap<WorkspaceId, Arc<tokio::sync::Mutex<()>>>>,
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
                projects: Mutex::default(),
                lifecycle: Mutex::default(),
            }),
        }
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
        let inner = self.inner.clone();
        let (base, name) = (base.to_owned(), name.to_owned());
        tokio::spawn(async move { inner.create(project, &base, &name).await }).await?
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
            start(inner.worktree(body.project, &body.branch)).await
        })
        .await
    }

    /// The folder of a live workspace's worktree. It takes no lock, so it is for tests only: a
    /// start goes through [`Workspaces::start_in`].
    #[cfg(test)]
    pub(crate) async fn worktree(&self, id: WorkspaceId) -> Result<PathBuf, WorkspaceError> {
        let body = self.inner.live(id).await?;
        Ok(self.inner.worktree(body.project, &body.branch))
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
        base: &str,
        name: &str,
    ) -> Result<WorkspaceId, WorkspaceError> {
        let repo = self
            .with_store(move |store| {
                Ok(store
                    .settings()?
                    .projects
                    .into_iter()
                    .find(|p| p.id == project)
                    .map(|p| p.repo))
            })
            .await?
            .ok_or(WorkspaceError::UnknownProject(project))?;
        let branch = safe_branch(name);
        let worktree = self.worktree(project, &branch);
        let id = WorkspaceId::random();
        let lock = self.lock(project);
        let _held = lock.lock().await;
        let clone = self.clone_for(project, &repo).await?;
        // A remote branch deleted since the last fetch is kept, so work merged and then deleted
        // on the remote still counts as pushed.
        self.git(&clone, "fetch", ["--quiet", "--no-prune", "origin"])
            .await?;
        // Compared as the disk compares names: where it ignores case, `Fix` and `fix` are one
        // ref file and one folder. A folder already there is someone's, whatever its branch.
        let branches = self
            .git(
                &clone,
                "for-each-ref",
                ["--format=%(refname:lstrip=2)", "refs/heads/"],
            )
            .await?;
        if branches.lines().any(|b| fold(b) == fold(&branch))
            || tokio::fs::try_exists(&worktree).await?
            || self.registered(&clone, &worktree).await?.is_some()
        {
            return Err(WorkspaceError::BranchExists(branch));
        }
        let start = self
            .git(
                &clone,
                "rev-parse",
                [
                    "--verify",
                    "--quiet",
                    &format!("refs/remotes/origin/{base}^{{commit}}"),
                ],
            )
            .await
            .map_err(|e| match e {
                // What `--verify --quiet` gives for a ref that is not there.
                WorkspaceError::Git { ref stderr, .. } if stderr.is_empty() => {
                    WorkspaceError::UnknownBase(base.to_owned())
                }
                e => e,
            })?
            .trim()
            .to_owned();
        // Git refuses a branch that is already there, by any spelling it reads as the same.
        self.git(
            &clone,
            "branch",
            ["--quiet", "--no-track", "--", &branch, &start],
        )
        .await?;
        let made = async {
            self.git(
                &clone,
                "worktree",
                [
                    OsStr::new("add"),
                    OsStr::new("--quiet"),
                    OsStr::new("--"),
                    worktree.as_os_str(),
                    OsStr::new(&branch),
                ],
            )
            .await?;
            // Git may store the name in another spelling of the same text: on macOS it keeps
            // Unicode composed. The record takes git's, so every later path and ref this module
            // builds from it is spelled as git lists it.
            let head = self.git(&worktree, "symbolic-ref", ["HEAD"]).await?;
            let stored = head.trim().strip_prefix("refs/heads/").unwrap_or(&branch);
            let body = Workspace {
                id,
                project,
                name: name.to_owned(),
                branch: stored.to_owned(),
                base: base.to_owned(),
                setup: None,
            };
            self.write(body.clone(), false).await?;
            Ok(body)
        }
        .await;
        let body = match made {
            Ok(body) => body,
            Err(e) => {
                self.undo(&clone, &worktree, &branch, &start).await;
                return Err(e);
            }
        };
        // Still under the lock, so an archive cannot run before the setup's terminal is known.
        if let Some(argv) = setup(&worktree).await {
            let terminal = self
                .terminals
                .start(id, &worktree, &argv, &[])
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

    async fn archive(&self, id: WorkspaceId, force: bool) -> Result<(), WorkspaceError> {
        let project = self.live(id).await?.project;
        let lock = self.lock(project);
        let _held = lock.lock().await;
        // Read again under the lock, so two archives of one workspace do not both run.
        let body = self.live(id).await?;
        let clone = self.clone_path(project);
        let worktree = self.worktree(project, &body.branch);
        if !force {
            self.refuse_dirty(&clone, &worktree, &body.branch).await?;
        }
        for terminal in self.terminals_of(id).await? {
            match self.terminals.close(terminal).await {
                Ok(()) | Err(TerminalError::Unknown(_)) => {}
                Err(e) => return Err(e.into()),
            }
        }
        if tokio::fs::try_exists(&worktree).await? {
            if !force {
                // What the terminals wrote as they were hung up counts too.
                self.refuse_dirty(&clone, &worktree, &body.branch).await?;
            }
            self.remove_worktree(&clone, &worktree, force).await?;
        } else if let Some((listed, _)) = self.registered(&clone, &worktree).await? {
            // Removed by hand: git forgets this worktree, and only this one, so its branch can be
            // checked out again. Named as git listed it, since git cannot resolve a path whose
            // folders are gone.
            self.remove_worktree(&clone, &listed, force).await?;
        }
        self.write(body, true).await
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
            let branches = |mine: bool| -> Vec<&str> {
                live.iter()
                    .filter(|(owner, w)| w.project == project && (*owner == self.me) == mine)
                    .map(|(_, w)| w.branch.as_str())
                    .collect()
            };
            let swept = tokio::time::timeout_at(
                deadline,
                self.sweep_project(project, &branches(true), &branches(false)),
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
    /// on `mine` and of other owners on `others`, by branch.
    async fn sweep_project(
        &self,
        project: ProjectId,
        mine: &[&str],
        others: &[&str],
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
            // Only what a create makes, `worktrees/<project>/<branch>`, and never the clone.
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
            let checked = match self.refuse_dirty(&clone, &listed, folder).await {
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

    /// Whether a live workspace of `project`, one of whose branches is on `live`, names the
    /// worktree git lists at `listed`, in `folder`, with `branch` checked out. Any doubt counts
    /// as named: its branch is the one checked out, its folder's name is the same on a disk that
    /// ignores case, or its folder is the same folder however each path is spelled.
    async fn named(
        &self,
        project: ProjectId,
        listed: &Path,
        folder: &str,
        branch: Option<&str>,
        live: &[&str],
    ) -> bool {
        for name in live {
            if branch == Some(*name)
                || fold(name) == fold(folder)
                || same_folder(listed, &self.worktree(project, name)).await
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

    /// Takes back what a create that failed made after it made `branch` at `start`. The
    /// worktree's folder was not there before this create, under the project's lock, so
    /// whatever is in it now is this create's and holds nothing of anyone's. The branch goes only
    /// while it still points at `start`. A step that finds nothing to undo fails, and that is
    /// fine; one that leaves something behind is logged.
    async fn undo(&self, clone: &Path, worktree: &Path, branch: &str, start: &str) {
        if matches!(tokio::fs::try_exists(worktree).await, Ok(true))
            && let Err(error) = self.remove_worktree(clone, worktree, true).await
        {
            tracing::warn!(
                worktree = %worktree.display(),
                %error,
                "a failed create left its worktree behind"
            );
        }
        let reference = format!("refs/heads/{branch}");
        if let Err(error) = self
            .git(clone, "update-ref", ["-d", &reference, start])
            .await
            && self.has_ref(clone, &reference).await.unwrap_or(true)
        {
            tracing::warn!(%reference, %error, "a failed create left its branch behind");
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
        if self.has_ref(clone, &reference).await? {
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

    /// The project's bare clone, made with `origin` set to `repo` when this machine has none.
    /// It is made under another name and renamed when ready, so a clone that stopped half way
    /// is never taken for a made one. A clone already there that the shared settings have moved
    /// to another repo forgets the old repo's branches, so none of them chooses a base or counts
    /// as pushed, and then has `origin` set to `repo`. The URL changes last, so a move that
    /// stops part way is made again by the next create.
    async fn clone_for(&self, project: ProjectId, repo: &str) -> Result<PathBuf, WorkspaceError> {
        let clone = self.clone_path(project);
        if tokio::fs::try_exists(&clone).await? {
            let origin = self
                .git(&clone, "config", ["--get", "remote.origin.url"])
                .await?;
            if origin.trim_end_matches('\n') != repo {
                let old = self
                    .git(
                        &clone,
                        "for-each-ref",
                        ["--format=%(refname)", "refs/remotes/origin/"],
                    )
                    .await?;
                for reference in old.lines() {
                    self.git(&clone, "update-ref", ["-d", reference]).await?;
                }
                self.git(&clone, "remote", ["set-url", "--", "origin", repo])
                    .await?;
            }
            return Ok(clone);
        }
        let clones = self.data.join("clones");
        tokio::fs::create_dir_all(&clones).await?;
        let partial = clones.join(format!("{project}.partial"));
        match tokio::fs::remove_dir_all(&partial).await {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
        self.git(
            &clones,
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
        tokio::fs::rename(&partial, &clone).await?;
        Ok(clone)
    }

    fn clone_path(&self, project: ProjectId) -> PathBuf {
        self.data.join("clones").join(format!("{project}.git"))
    }

    fn worktree(&self, project: ProjectId, branch: &str) -> PathBuf {
        self.data
            .join("worktrees")
            .join(project.to_string())
            .join(branch)
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

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
