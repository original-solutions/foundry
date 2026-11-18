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
//! Every branch is made by [`safe_branch`] and every path reaches git after `--`, so a name as
//! typed never reads as an option and never leaves the project's folder of worktrees.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fmt;
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

#[cfg(test)]
mod tests;

/// The longest branch [`safe_branch`] makes, in bytes.
const BRANCH_AT_MOST: usize = 100;

/// How long any one git call may run before it is killed and fails. A first fetch downloads the
/// whole repo, so this is generous; it is there so a remote that stops answering cannot hold a
/// project's lock for good.
const GIT_WITHIN: Duration = Duration::from_secs(10 * 60);

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
            }),
        }
    }

    /// Makes a workspace of `project` named `name`, on a new branch from `base` on the
    /// project's remote, and starts its setup. Returns once the setup has started, not
    /// finished. [`WorkspaceError::SetupNotStarted`] means the workspace was made all the same.
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
    /// finishes even when the caller stops waiting.
    pub async fn archive(&self, id: WorkspaceId, force: bool) -> Result<(), WorkspaceError> {
        let inner = self.inner.clone();
        tokio::spawn(async move { inner.archive(id, force).await }).await?
    }

    /// The folder of a live workspace's worktree, where its terminals start.
    pub async fn worktree(&self, id: WorkspaceId) -> Result<PathBuf, WorkspaceError> {
        let body = self.inner.live(id).await?;
        Ok(self.inner.worktree(body.project, &body.branch))
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
        self.git(&clone, "fetch", ["--quiet", "origin"]).await?;
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
            || self.registered_head(&clone, &worktree).await?.is_some()
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
            .map_err(|_| WorkspaceError::UnknownBase(base.to_owned()))?
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
            let body = Workspace {
                id,
                project,
                name: name.to_owned(),
                branch: branch.clone(),
                base: base.to_owned(),
            };
            self.write(body, false).await
        }
        .await;
        if let Err(e) = made {
            self.undo(&clone, &worktree, &branch, &start).await;
            return Err(e);
        }
        // Still under the lock, so an archive cannot run before the setup's terminal is known.
        if let Some(argv) = setup(&worktree).await {
            self.terminals
                .start(id, &worktree, &argv, &[])
                .await
                .map_err(|source| WorkspaceError::SetupNotStarted {
                    workspace: id,
                    source,
                })?;
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
        } else if self.registered_head(&clone, &worktree).await?.is_some() {
            // Removed by hand: git forgets this worktree, and only this one, so its branch can be
            // checked out again.
            self.remove_worktree(&clone, &worktree, force).await?;
        }
        self.write(body, true).await
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
    /// while it still points at `start`. Each step that finds nothing to undo fails, and that is
    /// fine.
    async fn undo(&self, clone: &Path, worktree: &Path, branch: &str, start: &str) {
        if matches!(tokio::fs::try_exists(worktree).await, Ok(true)) {
            let _ = self.remove_worktree(clone, worktree, true).await;
        }
        let reference = format!("refs/heads/{branch}");
        let _ = self
            .git(clone, "update-ref", ["-d", &reference, start])
            .await;
    }

    /// The commit git keeps as `HEAD` for a worktree registered at `worktree`, whether or not its
    /// folder is still there. `None` when no worktree is registered there.
    async fn registered_head(
        &self,
        clone: &Path,
        worktree: &Path,
    ) -> Result<Option<String>, WorkspaceError> {
        let list = self
            .git(clone, "worktree", ["list", "--porcelain", "-z"])
            .await?;
        // Every worktree of a clone sits at `worktrees/<project>/<branch>`, so the last two
        // parts name it, however git spelled the folders above.
        let tail = |p: &Path| {
            (
                p.file_name().map(OsStr::to_owned),
                p.parent().and_then(Path::file_name).map(OsStr::to_owned),
            )
        };
        let mut here = false;
        for line in list.split('\0') {
            if let Some(path) = line.strip_prefix("worktree ") {
                here = tail(Path::new(path)) == tail(worktree);
            } else if let Some(head) = line.strip_prefix("HEAD ")
                && here
            {
                return Ok(Some(head.to_owned()));
            }
        }
        Ok(None)
    }

    /// [`WorkspaceError::Dirty`] when the worktree has uncommitted files or a checked-out
    /// submodule, or the branch or the worktree's `HEAD` has commits no remote branch has. The
    /// `HEAD` is the one git keeps for the worktree, so it counts even when its folder is gone.
    async fn refuse_dirty(
        &self,
        clone: &Path,
        worktree: &Path,
        branch: &str,
    ) -> Result<(), WorkspaceError> {
        let mut heads = vec![format!("refs/heads/{branch}")];
        heads.extend(self.registered_head(clone, worktree).await?);
        let present = tokio::fs::try_exists(worktree).await?;
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
            (self.uncommitted(worktree).await?, submodules)
        } else {
            (Vec::new(), Vec::new())
        };
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
    /// sorted.
    async fn uncommitted(&self, worktree: &Path) -> Result<Vec<String>, WorkspaceError> {
        let out = self
            .git(
                worktree,
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
        let failed = |stderr: String| WorkspaceError::Git { command, stderr };
        let run = tokio::process::Command::new("git")
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
    /// is never taken for a made one.
    async fn clone_for(&self, project: ProjectId, repo: &str) -> Result<PathBuf, WorkspaceError> {
        let clone = self.clone_path(project);
        if tokio::fs::try_exists(&clone).await? {
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
                .since(me, 0)?
                .into_iter()
                .find_map(|record| match record {
                    AnyRecord::Workspace(r) if r.body.id == id && !r.deleted => Some(r.body),
                    _ => None,
                }))
        })
        .await?
        .ok_or(WorkspaceError::Unknown(id))
    }

    /// The terminals of workspace `id` that are not closed.
    async fn terminals_of(&self, id: WorkspaceId) -> Result<Vec<TerminalId>, WorkspaceError> {
        let me = self.me;
        self.with_store(move |store| {
            Ok(store
                .since(me, 0)?
                .into_iter()
                .filter_map(|record| match record {
                    AnyRecord::Terminal(r) if r.body.workspace == id && !r.deleted => {
                        Some(r.body.id)
                    }
                    _ => None,
                })
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
