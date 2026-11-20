use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use foundry_holder::protocol::Request;
use foundry_proto::ids::{MachineId, ProjectId, TerminalId, WorkspaceId};
use foundry_proto::record::{AnyRecord, Record, Terminal, Workspace};
use foundry_proto::settings::Project;
use foundry_proto::state::TerminalState;
use tempfile::TempDir;

use super::*;
use crate::store::Store;
use crate::terminals::Terminals;

const WAIT: Duration = Duration::from_secs(20);

/// Names a person might type that git or a shell would take badly.
const AWKWARD: &[&str] = &[
    "fix login",
    "café ☕ täst",
    "../../escape",
    "-rf",
    "--force",
    "a..b",
    "x.lock",
    "feature/login",
    "@{u}",
    "HEAD",
    "...",
    "",
    "  ",
    "~^:?*[\\",
    "tab\there\nnewline",
    ".hidden",
    "trailing.",
    "-",
];

/// `cargo test --workspace` builds the holder beside the test binary's `deps` folder.
fn holder() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let path = exe
        .parent()
        .and_then(Path::parent)
        .unwrap()
        .join("foundry-holder");
    assert!(
        path.exists(),
        "{} is not built: run `cargo build -p foundry-holder` first",
        path.display()
    );
    path
}

/// Runs git in `dir` with an identity of its own, and returns its standard output.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=Foundry Test",
            "-c",
            "user.email=test@foundry.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
            "-c",
            "protocol.file.allow=always",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// A bare repo standing in for a project's remote, and a clone of it to push from.
struct Remote {
    project: ProjectId,
    bare: PathBuf,
    src: PathBuf,
}

impl Remote {
    /// Commits `files` on `branch` of the source clone and pushes it.
    fn push(&self, branch: &str, files: &[(&str, &str)]) {
        let current = git(&self.src, &["branch", "--show-current"]);
        if current.trim() != branch {
            git(&self.src, &["checkout", "-q", "-B", branch]);
        }
        for (name, text) in files {
            let path = self.src.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        git(&self.src, &["add", "-A"]);
        git(
            &self.src,
            &["commit", "-q", "--allow-empty", "-m", "change"],
        );
        git(&self.src, &["push", "-q", "origin", branch]);
    }
}

/// A data directory, the daemon parts workspaces need, and a folder for remotes. Dropping it
/// closes every holder left under the data directory, so no test leaves a process behind.
struct Setup {
    data: TempDir,
    remotes: TempDir,
    me: MachineId,
    store: Arc<Mutex<Store>>,
    terminals: Arc<Terminals>,
    workspaces: Workspaces,
}

impl Setup {
    fn new() -> Self {
        Self::with(holder(), GIT_WITHIN)
    }

    /// Terminals started with the holder binary at `holder`, and git calls given `git_within`.
    fn with(holder: PathBuf, git_within: Duration) -> Self {
        // A Unix socket path is at most about 100 bytes, which the system's own temporary
        // folder on macOS would take most of.
        let data = tempfile::Builder::new()
            .prefix("fw")
            .tempdir_in("/tmp")
            .unwrap();
        let me = MachineId::random();
        let store = Arc::new(Mutex::new(
            Store::open(data.path().join("foundry.db")).unwrap(),
        ));
        let terminals = Arc::new(Terminals::new(me, store.clone(), data.path(), holder));
        let workspaces = Workspaces::with_git_within(
            me,
            store.clone(),
            data.path(),
            terminals.clone(),
            git_within,
        );
        Setup {
            data,
            remotes: tempfile::tempdir().unwrap(),
            me,
            store,
            terminals,
            workspaces,
        }
    }

    /// A project in the shared settings whose remote has `files` on `main`.
    fn project(&self, files: &[(&str, &str)]) -> Remote {
        let project = ProjectId::random();
        let root = self.remotes.path().join(project.to_string());
        let bare = root.join("remote.git");
        let src = root.join("src");
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q", "--bare", "remote.git"]);
        git(&root, &["clone", "-q", "remote.git", "src"]);
        let remote = Remote { project, bare, src };
        remote.push("main", files);

        let mut store = self.store.lock().unwrap();
        let mut settings = store.settings().unwrap();
        settings.seq += 1;
        settings.projects.push(Project {
            id: project,
            name: "demo".into(),
            repo: remote.bare.to_string_lossy().into_owned(),
        });
        assert!(store.apply_settings(settings).unwrap());
        remote
    }

    fn clones(&self) -> PathBuf {
        self.data.path().join("clones")
    }

    fn clone_of(&self, project: ProjectId) -> PathBuf {
        self.clones().join(format!("{project}.git"))
    }

    fn worktree(&self, project: ProjectId, branch: &str) -> PathBuf {
        self.data
            .path()
            .join("worktrees")
            .join(project.to_string())
            .join(branch)
    }

    async fn create(&self, remote: &Remote, base: &str, name: &str) -> WorkspaceId {
        self.workspaces
            .create(remote.project, base, name)
            .await
            .unwrap_or_else(|e| panic!("create {name:?}: {e}"))
    }

    /// The newest record held for workspace `id`.
    fn record(&self, id: WorkspaceId) -> Option<Record<Workspace>> {
        let store = self.store.lock().unwrap();
        store
            .since(self.me, 0)
            .unwrap()
            .into_iter()
            .filter_map(|record| match record {
                AnyRecord::Workspace(r) if r.body.id == id => Some(r),
                _ => None,
            })
            .next_back()
    }

    fn body(&self, id: WorkspaceId) -> Workspace {
        self.record(id).expect("a workspace record").body
    }

    /// The records of the terminals in workspace `id`.
    fn terminals_of(&self, id: WorkspaceId) -> Vec<Record<Terminal>> {
        let store = self.store.lock().unwrap();
        store
            .since(self.me, 0)
            .unwrap()
            .into_iter()
            .filter_map(|record| match record {
                AnyRecord::Terminal(r) if r.body.workspace == id => Some(r),
                _ => None,
            })
            .collect()
    }

    /// Waits for the workspace's only terminal to exit and returns its record.
    async fn setup_ended(&self, id: WorkspaceId) -> Terminal {
        let deadline = Instant::now() + WAIT;
        loop {
            let terminals = self.terminals_of(id);
            assert_eq!(terminals.len(), 1, "one setup terminal: {terminals:?}");
            if terminals[0].body.state == TerminalState::Exited {
                return terminals[0].body.clone();
            }
            assert!(Instant::now() < deadline, "timed out waiting for setup");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// A terminal in workspace `id` whose child sleeps until it is hung up.
    async fn sleeper(&self, id: WorkspaceId, cwd: &Path) -> TerminalId {
        let argv = ["/bin/sh", "-c", "sleep 3127"].map(String::from);
        self.terminals.start(id, cwd, &argv, &[]).await.unwrap()
    }

    /// Everything the terminal's child wrote, as kept after it exited.
    async fn screen(&self, id: TerminalId) -> String {
        let (snapshot, _) = self.terminals.attach(id).await.unwrap();
        String::from_utf8_lossy(&snapshot.bytes).into_owned()
    }
}

impl Drop for Setup {
    fn drop(&mut self) {
        use std::io::Write as _;
        let Ok(entries) = std::fs::read_dir(self.data.path().join("terminals")) else {
            return;
        };
        for entry in entries.flatten() {
            if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(entry.path()) {
                let _ = stream.write_all(&Request::Close.encode());
            }
        }
    }
}

fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn a_name_is_made_safe_for_git() {
    for (name, branch) in [
        ("fix-login", "fix-login"),
        ("fix login", "fix-login"),
        ("Fix Login", "Fix-Login"),
        ("  fix   login  ", "fix-login"),
        ("café ☕ täst", "café-täst"),
        ("CAFÉ", "CAFÉ"),
        ("../../escape", "escape"),
        ("-rf", "rf"),
        ("--force", "force"),
        ("a..b", "a.b"),
        ("x.lock", "x-lock"),
        ("feature/login", "feature-login"),
        ("@{u}", "u"),
        (".hidden", "hidden"),
        ("trailing.", "trailing"),
        ("HEAD", "HEAD-workspace"),
        ("", "workspace"),
        ("...", "workspace"),
        ("-", "workspace"),
    ] {
        assert_eq!(safe_branch(name), branch, "{name:?}");
    }
}

#[test]
fn a_long_name_is_cut_on_a_character() {
    // Three bytes a character, so a cut at 100 bytes falls inside one.
    let branch = safe_branch(&"日".repeat(60));
    assert_eq!(branch, "日".repeat(33));
}

#[test]
fn every_safe_branch_is_a_valid_branch_and_one_path_part() {
    let long = "a.".repeat(80);
    for name in AWKWARD.iter().copied().chain([long.as_str()]) {
        let branch = safe_branch(name);
        assert!(!branch.is_empty(), "{name:?}");
        assert!(!branch.starts_with(['-', '.']), "{name:?} -> {branch:?}");
        assert!(!branch.contains('/'), "{name:?} -> {branch:?}");
        let ok = Command::new("git")
            .args(["check-ref-format", "--branch", &branch])
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "{name:?} -> {branch:?} is not a valid branch");
    }
}

#[tokio::test]
async fn an_awkward_name_becomes_a_branch_and_its_worktree_is_made() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    for name in ["fix login", "café ☕ täst", "../../escape", "-rf --force"] {
        let id = setup.create(&remote, "main", name).await;
        let body = setup.body(id);
        assert_eq!(body.name, name);
        assert_eq!(body.branch, safe_branch(name));
        assert_eq!(body.base, "main");
        assert_eq!(body.project, remote.project);

        let worktree = setup.worktree(remote.project, &body.branch);
        assert_eq!(
            std::fs::read_to_string(worktree.join("README.md")).unwrap(),
            "hello\n"
        );
        assert_eq!(
            git(&worktree, &["branch", "--show-current"]).trim(),
            body.branch
        );
        assert_eq!(setup.workspaces.worktree(id).await.unwrap(), worktree);
    }
    // Nothing was made outside the project's own folder of worktrees.
    assert_eq!(
        names_in(setup.data.path().join("worktrees").as_path()).len(),
        1
    );
    assert_eq!(
        names_in(
            &setup
                .data
                .path()
                .join("worktrees")
                .join(remote.project.to_string())
        ),
        ["café-täst", "escape", "fix-login", "rf-force"]
    );
}

#[tokio::test]
async fn a_second_workspace_of_a_project_reuses_its_clone_and_fetches() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "one\n")]);
    setup.create(&remote, "main", "first").await;
    let clone = setup.clone_of(remote.project);
    std::fs::write(clone.join("marker"), "kept").unwrap();

    remote.push("next", &[("NEXT.md", "two\n")]);
    let id = setup.create(&remote, "next", "second").await;

    assert_eq!(
        names_in(&setup.clones()),
        [format!("{}.git", remote.project)]
    );
    assert_eq!(
        std::fs::read_to_string(clone.join("marker")).unwrap(),
        "kept"
    );
    let worktree = setup.worktree(remote.project, &setup.body(id).branch);
    assert_eq!(
        std::fs::read_to_string(worktree.join("NEXT.md")).unwrap(),
        "two\n"
    );
}

#[tokio::test]
async fn a_name_whose_branch_exists_is_refused_and_the_first_workspace_is_untouched() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    let first = setup.create(&remote, "main", "fix login").await;
    let worktree = setup.worktree(remote.project, "fix-login");
    std::fs::write(worktree.join("work.txt"), "mine").unwrap();

    let err = setup
        .workspaces
        .create(remote.project, "main", "fix-login")
        .await
        .unwrap_err();
    assert!(
        matches!(&err, WorkspaceError::BranchExists(b) if b == "fix-login"),
        "{err}"
    );
    assert_eq!(
        std::fs::read_to_string(worktree.join("work.txt")).unwrap(),
        "mine"
    );
    assert!(!setup.record(first).unwrap().deleted);
}

#[tokio::test]
async fn a_project_not_in_the_settings_is_refused() {
    let setup = Setup::new();
    let project = ProjectId::random();
    let err = setup
        .workspaces
        .create(project, "main", "x")
        .await
        .unwrap_err();
    assert!(matches!(err, WorkspaceError::UnknownProject(p) if p == project));
}

#[tokio::test]
async fn a_base_the_remote_does_not_have_is_refused_and_makes_no_workspace() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    let err = setup
        .workspaces
        .create(remote.project, "no-such-branch", "x")
        .await
        .unwrap_err();
    assert!(
        matches!(&err, WorkspaceError::UnknownBase(b) if b == "no-such-branch"),
        "{err}"
    );
    assert!(err.to_string().contains("no-such-branch"), "{err}");
    assert!(!setup.worktree(remote.project, "x").exists());
    let records = setup.store.lock().unwrap().since(setup.me, 0).unwrap();
    assert!(records.is_empty(), "{records:?}");
}

#[tokio::test]
async fn setup_commands_run_in_a_terminal_in_the_worktree() {
    let setup = Setup::new();
    let config =
        "factory:\n  provision:\n    - echo first > first.txt\n    - echo second > second.txt\n";
    let remote = setup.project(&[(".dw/config.yml", config)]);
    let id = setup.create(&remote, "main", "with setup").await;

    let terminal = setup.setup_ended(id).await;
    assert_eq!(terminal.exit_code, Some(0));
    let worktree = setup.worktree(remote.project, "with-setup");
    assert_eq!(
        std::fs::read_to_string(worktree.join("first.txt")).unwrap(),
        "first\n"
    );
    assert_eq!(
        std::fs::read_to_string(worktree.join("second.txt")).unwrap(),
        "second\n"
    );
}

#[tokio::test]
async fn the_workspace_record_names_the_terminal_its_setup_runs_in() {
    let setup = Setup::new();
    let config = "factory:\n  provision:\n    - exit 3\n";
    let remote = setup.project(&[(".dw/config.yml", config)]);
    let id = setup.create(&remote, "main", "named setup").await;

    let terminal = setup.setup_ended(id).await;
    assert_eq!(setup.body(id).setup, Some(terminal.id));
}

#[tokio::test]
async fn a_workspace_with_no_setup_names_no_setup_terminal() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    let id = setup.create(&remote, "main", "no setup").await;

    assert_eq!(setup.body(id).setup, None);
    assert!(setup.terminals_of(id).is_empty());
}

#[tokio::test]
async fn a_failing_setup_command_is_reported_and_the_workspace_stays_usable() {
    let setup = Setup::new();
    let config = "factory:\n  provision:\n    - echo one > one.txt\n    - exit 3\n    - echo two > two.txt\n";
    let remote = setup.project(&[(".dw/config.yml", config)]);
    let id = setup.create(&remote, "main", "bad setup").await;

    let terminal = setup.setup_ended(id).await;
    assert_eq!(terminal.exit_code, Some(3));
    let screen = setup.screen(terminal.id).await;
    assert!(screen.contains("setup failed: exit 3"), "{screen:?}");

    let worktree = setup.worktree(remote.project, "bad-setup");
    assert!(worktree.join("one.txt").exists());
    assert!(
        !worktree.join("two.txt").exists(),
        "setup stops at the failure"
    );
    assert!(!setup.record(id).unwrap().deleted);
    // Usable: work can be committed on the workspace's branch.
    git(&worktree, &["add", "one.txt"]);
    git(&worktree, &["commit", "-q", "-m", "fixed by hand"]);
}

#[tokio::test]
async fn a_config_that_cannot_be_read_is_a_failed_setup() {
    let setup = Setup::new();
    let config = "factory:\n  provision: [unclosed\n";
    let remote = setup.project(&[(".dw/config.yml", config)]);
    let id = setup.create(&remote, "main", "broken config").await;

    let terminal = setup.setup_ended(id).await;
    assert_eq!(terminal.exit_code, Some(1));
    let screen = setup.screen(terminal.id).await;
    assert!(screen.contains(".dw/config.yml"), "{screen:?}");
    assert!(setup.worktree(remote.project, "broken-config").exists());
}

#[tokio::test]
async fn a_repo_with_no_config_or_no_provision_runs_no_setup() {
    let setup = Setup::new();
    let bare = setup.project(&[("README.md", "hello\n")]);
    let without = setup.create(&bare, "main", "no config").await;
    assert!(setup.terminals_of(without).is_empty());

    let config = "factory:\n  base: main\n";
    let other = setup.project(&[(".dw/config.yml", config)]);
    let empty = setup.create(&other, "main", "no provision").await;
    assert!(setup.terminals_of(empty).is_empty());
}

#[tokio::test]
async fn archive_with_uncommitted_files_or_unpushed_commits_is_refused_listing_each() {
    let setup = Setup::new();
    let remote = setup.project(&[
        ("README.md", "hello\n"),
        ("keep.txt", "keep\n"),
        ("old.txt", "old\n"),
    ]);
    let id = setup.create(&remote, "main", "dirty").await;
    let worktree = setup.worktree(remote.project, "dirty");
    let terminal = setup.sleeper(id, &worktree).await;

    std::fs::write(worktree.join("committed.txt"), "c").unwrap();
    git(&worktree, &["add", "committed.txt"]);
    git(&worktree, &["commit", "-q", "-m", "not pushed yet"]);
    std::fs::write(worktree.join("README.md"), "changed\n").unwrap();
    std::fs::write(worktree.join("new file.txt"), "new").unwrap();
    std::fs::remove_file(worktree.join("keep.txt")).unwrap();
    git(&worktree, &["mv", "old.txt", "moved.txt"]);

    let err = setup.workspaces.archive(id, false).await.unwrap_err();
    let WorkspaceError::Dirty(dirty) = err else {
        panic!("{err}");
    };
    assert_eq!(
        dirty.uncommitted,
        ["README.md", "keep.txt", "moved.txt", "new file.txt"]
    );
    assert_eq!(dirty.unpushed.len(), 1, "{:?}", dirty.unpushed);
    assert!(dirty.unpushed[0].ends_with(" not pushed yet"), "{dirty:?}");
    let text = dirty.to_string();
    for part in ["README.md", "keep.txt", "new file.txt", "not pushed yet"] {
        assert!(text.contains(part), "{text}");
    }

    // A refusal changes nothing: the worktree and its running terminal are left as they were.
    assert!(worktree.join("new file.txt").exists());
    assert!(!setup.record(id).unwrap().deleted);
    let terminals = setup.terminals_of(id);
    assert_eq!(terminals.len(), 1);
    assert_eq!(terminals[0].body.id, terminal);
    assert!(!terminals[0].deleted);
    assert_eq!(terminals[0].body.state, TerminalState::Running);
}

#[tokio::test]
async fn an_unpushed_commit_alone_is_refused() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    let id = setup.create(&remote, "main", "ahead").await;
    let worktree = setup.worktree(remote.project, "ahead");
    git(
        &worktree,
        &["commit", "-q", "--allow-empty", "-m", "only here"],
    );

    let err = setup.workspaces.archive(id, false).await.unwrap_err();
    let WorkspaceError::Dirty(dirty) = err else {
        panic!("{err}");
    };
    assert!(dirty.uncommitted.is_empty(), "{dirty:?}");
    assert_eq!(dirty.unpushed.len(), 1, "{dirty:?}");
    assert!(dirty.unpushed[0].ends_with(" only here"), "{dirty:?}");
    assert!(worktree.exists());
}

#[tokio::test]
async fn an_uncommitted_file_alone_is_refused() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    let id = setup.create(&remote, "main", "loose").await;
    let worktree = setup.worktree(remote.project, "loose");
    std::fs::write(worktree.join("draft.txt"), "d").unwrap();

    let err = setup.workspaces.archive(id, false).await.unwrap_err();
    let WorkspaceError::Dirty(dirty) = err else {
        panic!("{err}");
    };
    assert_eq!(dirty.uncommitted, ["draft.txt"]);
    assert!(dirty.unpushed.is_empty(), "{dirty:?}");
    assert!(worktree.join("draft.txt").exists());
}

#[tokio::test]
async fn a_commit_on_a_detached_head_is_unpushed_work() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    let id = setup.create(&remote, "main", "detached").await;
    let worktree = setup.worktree(remote.project, "detached");
    git(&worktree, &["checkout", "-q", "--detach"]);
    git(
        &worktree,
        &["commit", "-q", "--allow-empty", "-m", "off the branch"],
    );

    let err = setup.workspaces.archive(id, false).await.unwrap_err();
    let WorkspaceError::Dirty(dirty) = err else {
        panic!("{err}");
    };
    assert_eq!(dirty.unpushed.len(), 1, "{dirty:?}");
    assert!(dirty.unpushed[0].ends_with(" off the branch"), "{dirty:?}");
    assert!(worktree.exists());
}

#[tokio::test]
async fn ignored_files_do_not_hold_up_archive() {
    let setup = Setup::new();
    let remote = setup.project(&[(".gitignore", "build/\n")]);
    let id = setup.create(&remote, "main", "built").await;
    let worktree = setup.worktree(remote.project, "built");
    std::fs::create_dir_all(worktree.join("build")).unwrap();
    std::fs::write(worktree.join("build").join("out.bin"), "o").unwrap();

    setup.workspaces.archive(id, false).await.unwrap();
    assert!(!worktree.exists());
}

#[tokio::test]
async fn a_pushed_commit_does_not_hold_up_archive() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    let id = setup.create(&remote, "main", "pushed").await;
    let worktree = setup.worktree(remote.project, "pushed");
    std::fs::write(worktree.join("done.txt"), "d").unwrap();
    git(&worktree, &["add", "done.txt"]);
    git(&worktree, &["commit", "-q", "-m", "done"]);
    git(&worktree, &["push", "-q", "origin", "HEAD"]);

    setup.workspaces.archive(id, false).await.unwrap();
    assert!(!worktree.exists());
}

#[tokio::test]
async fn archive_with_force_removes_the_worktree_and_keeps_the_branch() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    let id = setup.create(&remote, "main", "forced").await;
    let worktree = setup.worktree(remote.project, "forced");
    std::fs::write(worktree.join("unpushed.txt"), "u").unwrap();
    git(&worktree, &["add", "unpushed.txt"]);
    git(&worktree, &["commit", "-q", "-m", "unpushed work"]);
    std::fs::write(worktree.join("loose.txt"), "l").unwrap();

    setup.workspaces.archive(id, true).await.unwrap();

    assert!(!worktree.exists());
    let clone = setup.clone_of(remote.project);
    let log = git(&clone, &["log", "--format=%s", "refs/heads/forced", "--"]);
    assert!(log.lines().any(|l| l == "unpushed work"), "{log}");
    assert!(setup.record(id).unwrap().deleted);
    let worktrees = git(&clone, &["worktree", "list", "--porcelain"]);
    assert!(!worktrees.contains("forced"), "{worktrees}");
}

#[tokio::test]
async fn archive_stops_the_workspaces_terminals() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    let id = setup.create(&remote, "main", "busy").await;
    let worktree = setup.worktree(remote.project, "busy");
    let terminal = setup.sleeper(id, &worktree).await;

    setup.workspaces.archive(id, false).await.unwrap();

    let records = setup.terminals_of(id);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].body.id, terminal);
    assert!(records[0].deleted, "{records:?}");
    assert!(!worktree.exists());
}

#[tokio::test]
async fn a_worktree_removed_by_hand_still_archives() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    let id = setup.create(&remote, "main", "gone").await;
    std::fs::remove_dir_all(setup.worktree(remote.project, "gone")).unwrap();

    setup.workspaces.archive(id, false).await.unwrap();
    assert!(setup.record(id).unwrap().deleted);
    // Git no longer lists the removed worktree, so its branch can be checked out again.
    let clone = setup.clone_of(remote.project);
    let worktrees = git(&clone, &["worktree", "list", "--porcelain"]);
    assert!(!worktrees.contains("/gone\n"), "{worktrees}");
    let again = setup.data.path().join("again");
    git(
        &clone,
        &[
            "worktree",
            "add",
            "-q",
            "--",
            again.to_str().unwrap(),
            "gone",
        ],
    );
}

#[tokio::test]
async fn an_unknown_or_archived_workspace_cannot_be_archived() {
    let setup = Setup::new();
    let unknown = WorkspaceId::random();
    let err = setup.workspaces.archive(unknown, true).await.unwrap_err();
    assert!(
        matches!(err, WorkspaceError::Unknown(w) if w == unknown),
        "{err}"
    );

    let remote = setup.project(&[("README.md", "hello\n")]);
    let id = setup.create(&remote, "main", "once").await;
    setup.workspaces.archive(id, false).await.unwrap();
    let err = setup.workspaces.archive(id, true).await.unwrap_err();
    assert!(
        matches!(err, WorkspaceError::Unknown(w) if w == id),
        "{err}"
    );
}

#[tokio::test]
async fn a_name_that_differs_only_in_case_from_a_branch_is_refused() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    let first = setup.create(&remote, "main", "Fix Login").await;
    assert_eq!(setup.body(first).branch, "Fix-Login");

    let err = setup
        .workspaces
        .create(remote.project, "main", "FIX login")
        .await
        .unwrap_err();
    assert!(
        matches!(&err, WorkspaceError::BranchExists(b) if b == "FIX-login"),
        "{err}"
    );

    // A disk that ignores case folds a final sigma to sigma, and a long s to s.
    setup.create(&remote, "main", "λόγος").await;
    let err = setup
        .workspaces
        .create(remote.project, "main", "ΛΌΓΟΣ")
        .await
        .unwrap_err();
    assert!(matches!(err, WorkspaceError::BranchExists(_)), "{err}");
    setup.create(&remote, "main", "six").await;
    let err = setup
        .workspaces
        .create(remote.project, "main", "ſix")
        .await
        .unwrap_err();
    assert!(matches!(err, WorkspaceError::BranchExists(_)), "{err}");

    // A branch made by hand in the clone counts too, whatever its case.
    let clone = setup.clone_of(remote.project);
    git(&clone, &["branch", "Other", "refs/remotes/origin/main"]);
    let err = setup
        .workspaces
        .create(remote.project, "main", "other")
        .await
        .unwrap_err();
    assert!(
        matches!(&err, WorkspaceError::BranchExists(b) if b == "other"),
        "{err}"
    );
    let log = git(
        &clone,
        &["log", "-1", "--format=%H", "refs/heads/Other", "--"],
    );
    let main = git(&clone, &["rev-parse", "refs/remotes/origin/main"]);
    assert_eq!(log, main);
}

#[tokio::test]
async fn the_worktree_of_an_unknown_workspace_is_refused() {
    let setup = Setup::new();
    let unknown = WorkspaceId::random();
    let err = setup.workspaces.worktree(unknown).await.unwrap_err();
    assert!(
        matches!(err, WorkspaceError::Unknown(w) if w == unknown),
        "{err}"
    );
}

#[tokio::test]
async fn two_creates_at_once_share_one_clone() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    let (a, b) = tokio::join!(
        setup.workspaces.create(remote.project, "main", "one"),
        setup.workspaces.create(remote.project, "main", "two"),
    );
    a.unwrap();
    b.unwrap();
    assert_eq!(
        names_in(&setup.clones()),
        [format!("{}.git", remote.project)]
    );

    let (a, b) = tokio::join!(
        setup.workspaces.create(remote.project, "main", "same"),
        setup.workspaces.create(remote.project, "main", "same"),
    );
    let refused = [&a, &b]
        .iter()
        .filter(|r| matches!(r, Err(WorkspaceError::BranchExists(b)) if b == "same"))
        .count();
    assert_eq!(refused, 1, "{a:?} {b:?}");
    assert!(a.is_ok() || b.is_ok(), "{a:?} {b:?}");
}

#[tokio::test]
async fn a_setup_that_cannot_start_leaves_the_workspace_and_names_it() {
    let setup = Setup::with(PathBuf::from("/no/such/foundry-holder"), GIT_WITHIN);
    let config = "factory:\n  provision:\n    - echo hi\n";
    let remote = setup.project(&[(".dw/config.yml", config)]);
    let err = setup
        .workspaces
        .create(remote.project, "main", "no holder")
        .await
        .unwrap_err();
    let WorkspaceError::SetupNotStarted { workspace, .. } = err else {
        panic!("{err}");
    };
    assert!(!setup.record(workspace).unwrap().deleted);
    assert!(setup.worktree(remote.project, "no-holder").exists());
}

#[tokio::test]
async fn a_fetch_that_hangs_fails_in_time_and_frees_the_project() {
    let setup = Setup::with(holder(), Duration::from_secs(1));
    let remote = setup.project(&[("README.md", "hello\n")]);
    let id = setup.create(&remote, "main", "before").await;
    // The remote side now takes longer to answer than git is given.
    let clone = setup.clone_of(remote.project);
    git(
        &clone,
        &[
            "config",
            "remote.origin.uploadpack",
            "sleep 4.73; git-upload-pack",
        ],
    );

    let started = Instant::now();
    let err = setup
        .workspaces
        .create(remote.project, "main", "after")
        .await
        .unwrap_err();
    assert!(
        matches!(&err, WorkspaceError::Git { command: "fetch", stderr } if stderr.contains("did not finish")),
        "{err}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
    // The project's lock was let go: an archive of it goes through.
    setup.workspaces.archive(id, false).await.unwrap();
}

/// Stands in front of terminal `id`'s holder: the next request to it writes `late.txt` into
/// `worktree`, as a child would while it is hung up, then the connection ends as the holder's
/// does on close. The real holder keeps running under another name in the same folder, so
/// `Setup` still closes it.
fn writes_on_close(setup: &Setup, id: TerminalId, worktree: &Path) -> std::thread::JoinHandle<()> {
    use std::io::Read as _;
    let dir = setup.data.path().join("terminals");
    let socket = dir.join(format!("{id}.sock"));
    std::fs::rename(&socket, dir.join(format!("{id}-real.sock"))).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let late = worktree.join("late.txt");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut request = [0u8; 64];
            if stream.read(&mut request).unwrap_or(0) > 0 {
                std::fs::write(&late, "written while hung up").unwrap();
                return;
            }
        }
    })
}

#[tokio::test]
async fn work_written_while_the_terminals_close_is_refused_and_kept() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    let id = setup.create(&remote, "main", "racing").await;
    let worktree = setup.worktree(remote.project, "racing");
    let terminal = setup.sleeper(id, &worktree).await;
    let stand_in = writes_on_close(&setup, terminal, &worktree);

    let err = setup.workspaces.archive(id, false).await.unwrap_err();
    stand_in.join().unwrap();
    let WorkspaceError::Dirty(dirty) = err else {
        panic!("{err}");
    };
    assert_eq!(dirty.uncommitted, ["late.txt"]);
    assert!(worktree.join("late.txt").exists());
    assert!(!setup.record(id).unwrap().deleted);
}

#[tokio::test]
async fn a_worktree_with_a_submodule_is_refused_unless_forced() {
    let setup = Setup::new();
    let library = setup.project(&[("lib.txt", "lib\n")]);
    let remote = setup.project(&[("README.md", "hello\n")]);
    git(
        &remote.src,
        &[
            "submodule",
            "add",
            "-q",
            library.bare.to_str().unwrap(),
            "lib",
        ],
    );
    git(&remote.src, &["commit", "-q", "-m", "add lib"]);
    git(&remote.src, &["push", "-q", "origin", "main"]);
    let id = setup.create(&remote, "main", "with lib").await;
    let worktree = setup.worktree(remote.project, "with-lib");
    git(&worktree, &["submodule", "update", "-q", "--init"]);
    assert!(worktree.join("lib").join("lib.txt").exists());

    // A commit made in the submodule and never pushed lives only in this worktree's git
    // folder, though the workspace's own branch, with the new pointer, is pushed. The archive
    // cannot see into the submodule, so it names it and leaves it be.
    git(
        &worktree.join("lib"),
        &["commit", "-q", "--allow-empty", "-m", "only in lib"],
    );
    git(&worktree, &["commit", "-q", "-am", "bump lib"]);
    git(&worktree, &["push", "-q", "origin", "HEAD"]);
    let terminal = setup.sleeper(id, &worktree).await;
    let err = setup.workspaces.archive(id, false).await.unwrap_err();
    let WorkspaceError::Dirty(dirty) = err else {
        panic!("{err}");
    };
    assert!(dirty.uncommitted.is_empty(), "{dirty:?}");
    assert!(dirty.unpushed.is_empty(), "{dirty:?}");
    assert_eq!(dirty.submodules, ["lib"]);
    assert!(dirty.to_string().contains("submodules lib"), "{dirty}");
    assert!(worktree.join("lib").join("lib.txt").exists());
    let terminals = setup.terminals_of(id);
    assert_eq!(terminals[0].body.id, terminal);
    assert!(
        !terminals[0].deleted,
        "a refusal leaves the terminals running"
    );

    setup.workspaces.archive(id, true).await.unwrap();
    assert!(!worktree.exists());

    // A submodule never checked out holds nothing, so it does not hold up an archive.
    let other = setup.create(&remote, "main", "without lib").await;
    setup.workspaces.archive(other, false).await.unwrap();
    assert!(!setup.worktree(remote.project, "without-lib").exists());
}

#[tokio::test]
async fn two_archives_at_once_archive_once() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    let id = setup.create(&remote, "main", "twice").await;
    let (a, b) = tokio::join!(
        setup.workspaces.archive(id, false),
        setup.workspaces.archive(id, false),
    );
    let unknown = [&a, &b]
        .iter()
        .filter(|r| matches!(r, Err(WorkspaceError::Unknown(w)) if *w == id))
        .count();
    assert_eq!(unknown, 1, "{a:?} {b:?}");
    assert!(a.is_ok() || b.is_ok(), "{a:?} {b:?}");
    let records = setup.store.lock().unwrap().since(setup.me, 0).unwrap();
    let deleted = records
        .iter()
        .filter(|r| matches!(r, AnyRecord::Workspace(w) if w.body.id == id && w.deleted))
        .count();
    assert_eq!(deleted, 1);
    // One write for the create and one for the archive.
    let versions = setup.store.lock().unwrap().versions().unwrap();
    assert_eq!(versions.get(&setup.me), Some(&2));
}

#[tokio::test]
async fn a_create_that_cannot_record_its_workspace_leaves_no_branch_behind() {
    use std::os::unix::fs::PermissionsExt as _;
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    setup.create(&remote, "main", "first").await;

    // The store cannot make its journal in a folder it may not write.
    let mode = |m| {
        std::fs::set_permissions(setup.data.path(), std::fs::Permissions::from_mode(m)).unwrap()
    };
    mode(0o500);
    let err = setup
        .workspaces
        .create(remote.project, "main", "second")
        .await;
    mode(0o700);
    assert!(matches!(err, Err(WorkspaceError::Store(_))), "{err:?}");

    let clone = setup.clone_of(remote.project);
    let branches = git(
        &clone,
        &["for-each-ref", "--format=%(refname)", "refs/heads/"],
    );
    assert!(!branches.contains("second"), "{branches}");
    assert!(!setup.worktree(remote.project, "second").exists());
    setup.create(&remote, "main", "second").await;
}

#[tokio::test]
async fn a_name_whose_folder_is_taken_is_refused_and_the_folder_is_untouched() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    let first = setup.create(&remote, "main", "fix").await;
    let worktree = setup.worktree(remote.project, "fix");
    // Inside the workspace, its branch is renamed, as before opening a pull request.
    git(&worktree, &["branch", "-m", "fix", "fix-renamed"]);
    std::fs::write(worktree.join("precious.txt"), "mine").unwrap();

    let err = setup
        .workspaces
        .create(remote.project, "main", "fix")
        .await
        .unwrap_err();
    assert!(
        matches!(&err, WorkspaceError::BranchExists(b) if b == "fix"),
        "{err}"
    );
    assert_eq!(
        std::fs::read_to_string(worktree.join("precious.txt")).unwrap(),
        "mine"
    );
    let clone = setup.clone_of(remote.project);
    let worktrees = git(&clone, &["worktree", "list", "--porcelain"]);
    assert!(worktrees.contains("/fix\n"), "{worktrees}");
    let branches = git(
        &clone,
        &["for-each-ref", "--format=%(refname)", "refs/heads/"],
    );
    assert!(
        !branches.lines().any(|b| b == "refs/heads/fix"),
        "{branches}"
    );
    assert!(!setup.record(first).unwrap().deleted);

    // With the folder gone too, git still has the worktree registered: still refused.
    std::fs::remove_dir_all(&worktree).unwrap();
    let err = setup
        .workspaces
        .create(remote.project, "main", "fix")
        .await
        .unwrap_err();
    assert!(
        matches!(&err, WorkspaceError::BranchExists(b) if b == "fix"),
        "{err}"
    );
    let worktrees = git(&clone, &["worktree", "list", "--porcelain"]);
    assert!(worktrees.contains("/fix\n"), "{worktrees}");
}

#[tokio::test]
async fn a_name_the_disk_reads_as_another_never_touches_its_work() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    // The same word, composed and decomposed: every character a letter, so safe_branch keeps
    // both as they are. A disk that ignores the difference, as macOS does, reads both as one
    // folder.
    let composed = "\u{d55c}\u{ae00}";
    let decomposed = "\u{1112}\u{1161}\u{11ab}\u{1100}\u{1173}\u{11af}";
    assert_eq!(safe_branch(composed), composed);
    assert_eq!(safe_branch(decomposed), decomposed);
    let first = setup.create(&remote, "main", composed).await;
    let worktree = setup.worktree(remote.project, composed);
    std::fs::write(worktree.join("precious.txt"), "mine").unwrap();
    git(
        &worktree,
        &["commit", "-q", "--allow-empty", "-m", "first work"],
    );

    let second = setup
        .workspaces
        .create(remote.project, "main", decomposed)
        .await;
    if cfg!(target_os = "macos") {
        assert!(
            matches!(&second, Err(WorkspaceError::BranchExists(_))),
            "{second:?}"
        );
        let records = setup.store.lock().unwrap().since(setup.me, 0).unwrap();
        assert_eq!(records.len(), 1, "{records:?}");
    } else {
        second.unwrap();
    }

    assert_eq!(
        std::fs::read_to_string(worktree.join("precious.txt")).unwrap(),
        "mine"
    );
    let clone = setup.clone_of(remote.project);
    let log = git(
        &clone,
        &[
            "log",
            "-1",
            "--format=%s",
            &format!("refs/heads/{composed}"),
            "--",
        ],
    );
    assert_eq!(log.trim(), "first work");
    assert!(!setup.record(first).unwrap().deleted);
}

#[tokio::test]
async fn a_worktree_removed_by_hand_keeps_its_detached_commits_through_a_failed_create() {
    use std::os::unix::fs::PermissionsExt as _;
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    let id = setup.create(&remote, "main", "moved").await;
    let worktree = setup.worktree(remote.project, "moved");
    git(&worktree, &["checkout", "-q", "--detach"]);
    git(
        &worktree,
        &["commit", "-q", "--allow-empty", "-m", "only on its head"],
    );
    std::fs::remove_dir_all(&worktree).unwrap();

    // A create that fails takes back its own worktree and branch, and nothing of this one's.
    let mode = |m| {
        std::fs::set_permissions(setup.data.path(), std::fs::Permissions::from_mode(m)).unwrap()
    };
    mode(0o500);
    let err = setup
        .workspaces
        .create(remote.project, "main", "other")
        .await;
    mode(0o700);
    assert!(matches!(err, Err(WorkspaceError::Store(_))), "{err:?}");

    // The commit only the removed worktree's HEAD held is still unpushed work.
    let err = setup.workspaces.archive(id, false).await.unwrap_err();
    let WorkspaceError::Dirty(dirty) = err else {
        panic!("{err}");
    };
    assert_eq!(dirty.unpushed.len(), 1, "{dirty:?}");
    assert!(
        dirty.unpushed[0].ends_with(" only on its head"),
        "{dirty:?}"
    );

    setup.workspaces.archive(id, true).await.unwrap();
    let clone = setup.clone_of(remote.project);
    let worktrees = git(&clone, &["worktree", "list", "--porcelain"]);
    assert!(!worktrees.contains("/moved\n"), "{worktrees}");
}

#[tokio::test]
async fn archiving_one_removed_worktree_leaves_the_others_entries() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    let a = setup.create(&remote, "main", "a").await;
    let b = setup.create(&remote, "main", "b").await;
    let b_tree = setup.worktree(remote.project, "b");
    git(&b_tree, &["checkout", "-q", "--detach"]);
    git(
        &b_tree,
        &["commit", "-q", "--allow-empty", "-m", "only on b"],
    );
    std::fs::remove_dir_all(setup.worktree(remote.project, "a")).unwrap();
    std::fs::remove_dir_all(&b_tree).unwrap();

    setup.workspaces.archive(a, false).await.unwrap();
    let clone = setup.clone_of(remote.project);
    let worktrees = git(&clone, &["worktree", "list", "--porcelain"]);
    assert!(worktrees.contains("/b\n"), "{worktrees}");
    let err = setup.workspaces.archive(b, false).await.unwrap_err();
    let WorkspaceError::Dirty(dirty) = err else {
        panic!("{err}");
    };
    assert_eq!(dirty.unpushed.len(), 1, "{dirty:?}");
    assert!(dirty.unpushed[0].ends_with(" only on b"), "{dirty:?}");
}

#[tokio::test]
async fn a_worktree_whose_folders_and_entry_are_gone_still_archives() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    let first = setup.create(&remote, "main", "first").await;
    let second = setup.create(&remote, "main", "second").await;
    // The whole project folder goes, so git cannot find either worktree by our spelling of its
    // path, which on macOS runs through the /tmp link.
    std::fs::remove_dir_all(
        setup
            .data
            .path()
            .join("worktrees")
            .join(remote.project.to_string()),
    )
    .unwrap();
    setup.workspaces.archive(first, false).await.unwrap();
    assert!(setup.record(first).unwrap().deleted);

    // And when git has forgotten it too, there is nothing left to remove.
    let clone = setup.clone_of(remote.project);
    git(&clone, &["worktree", "prune"]);
    setup.workspaces.archive(second, true).await.unwrap();
    assert!(setup.record(second).unwrap().deleted);
}

#[tokio::test]
async fn a_decomposed_name_keeps_its_detached_commits() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    // Every character a letter, so safe_branch keeps the decomposed form; git on macOS keeps
    // the composed one for the same folder.
    let name = "\u{1112}\u{1161}\u{11ab}";
    let id = setup.create(&remote, "main", name).await;
    let worktree = setup.workspaces.worktree(id).await.unwrap();
    git(&worktree, &["checkout", "-q", "--detach"]);
    git(
        &worktree,
        &["commit", "-q", "--allow-empty", "-m", "detached"],
    );

    let err = setup.workspaces.archive(id, false).await.unwrap_err();
    let WorkspaceError::Dirty(dirty) = err else {
        panic!("{err}");
    };
    assert_eq!(dirty.unpushed.len(), 1, "{dirty:?}");
    assert!(dirty.unpushed[0].ends_with(" detached"), "{dirty:?}");
    assert!(worktree.exists());
}

#[tokio::test]
async fn a_workspace_whose_branch_was_renamed_or_deleted_still_checks_its_work() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    let id = setup.create(&remote, "main", "fix").await;
    let worktree = setup.worktree(remote.project, "fix");
    // Inside the workspace, its branch is renamed, as before opening a pull request.
    git(&worktree, &["branch", "-m", "fix", "fix-renamed"]);
    std::fs::write(worktree.join("loose.txt"), "mine").unwrap();
    let err = setup.workspaces.archive(id, false).await.unwrap_err();
    let WorkspaceError::Dirty(dirty) = err else {
        panic!("{err}");
    };
    assert_eq!(dirty.uncommitted, ["loose.txt"]);

    // Or another branch is checked out and the workspace's own is deleted.
    std::fs::remove_file(worktree.join("loose.txt")).unwrap();
    git(&worktree, &["checkout", "-q", "-b", "elsewhere"]);
    git(&worktree, &["branch", "-q", "-D", "fix-renamed"]);
    git(
        &worktree,
        &["commit", "-q", "--allow-empty", "-m", "on elsewhere"],
    );
    let err = setup.workspaces.archive(id, false).await.unwrap_err();
    let WorkspaceError::Dirty(dirty) = err else {
        panic!("{err}");
    };
    assert_eq!(dirty.unpushed.len(), 1, "{dirty:?}");
    assert!(dirty.unpushed[0].ends_with(" on elsewhere"), "{dirty:?}");

    // With no branch, no folder and no entry left, there is nothing to check.
    git(&worktree, &["push", "-q", "origin", "elsewhere"]);
    std::fs::remove_dir_all(&worktree).unwrap();
    git(&setup.clone_of(remote.project), &["worktree", "prune"]);
    setup.workspaces.archive(id, false).await.unwrap();
    assert!(setup.record(id).unwrap().deleted);
}

#[tokio::test]
async fn a_decomposed_name_removed_by_hand_keeps_its_detached_commits() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    let name = "\u{1112}\u{1161}\u{11ab}";
    let id = setup.create(&remote, "main", name).await;
    // The record names the branch as git stores it, which on macOS is the composed form.
    let clone = setup.clone_of(remote.project);
    let branches = git(
        &clone,
        &[
            "for-each-ref",
            "--format=%(refname:lstrip=2)",
            "refs/heads/",
        ],
    );
    let branch = setup.body(id).branch;
    assert!(branches.lines().any(|b| b == branch), "{branches}");

    let worktree = setup.workspaces.worktree(id).await.unwrap();
    git(&worktree, &["checkout", "-q", "--detach"]);
    git(
        &worktree,
        &["commit", "-q", "--allow-empty", "-m", "detached"],
    );
    std::fs::remove_dir_all(&worktree).unwrap();

    let err = setup.workspaces.archive(id, false).await.unwrap_err();
    let WorkspaceError::Dirty(dirty) = err else {
        panic!("{err}");
    };
    assert_eq!(dirty.unpushed.len(), 1, "{dirty:?}");
    assert!(dirty.unpushed[0].ends_with(" detached"), "{dirty:?}");

    // Forced, git forgets the worktree, so its branch can be checked out again.
    setup.workspaces.archive(id, true).await.unwrap();
    let again = setup.data.path().join("again");
    git(
        &clone,
        &[
            "worktree",
            "add",
            "-q",
            "--",
            again.to_str().unwrap(),
            &branch,
        ],
    );
}

#[tokio::test]
async fn a_project_whose_repo_changed_is_fetched_from_the_new_repo() {
    let setup = Setup::new();
    let old = setup.project(&[("README.md", "old\n")]);
    old.push("only-old", &[("OLD.md", "old\n")]);
    let before = setup.create(&old, "main", "before").await;
    // A commit that only the old repo has.
    let before_tree = setup.worktree(old.project, "before");
    git(
        &before_tree,
        &["commit", "-q", "--allow-empty", "-m", "only in old"],
    );
    git(&before_tree, &["push", "-q", "origin", "before"]);
    let new = setup.project(&[("NEW.md", "new\n")]);
    {
        let mut store = setup.store.lock().unwrap();
        let mut settings = store.settings().unwrap();
        settings.seq += 1;
        let project = settings
            .projects
            .iter_mut()
            .find(|p| p.id == old.project)
            .unwrap();
        project.repo = new.bare.to_string_lossy().into_owned();
        assert!(store.apply_settings(settings).unwrap());
    }

    // The first fetch after the move fails; the old repo's branches are gone all the same.
    let away = new.bare.with_extension("away");
    std::fs::rename(&new.bare, &away).unwrap();
    let err = setup
        .workspaces
        .create(old.project, "main", "after")
        .await
        .unwrap_err();
    assert!(
        matches!(
            &err,
            WorkspaceError::Git {
                command: "fetch",
                ..
            }
        ),
        "{err}"
    );
    std::fs::rename(&away, &new.bare).unwrap();

    setup.create(&old, "main", "after").await;
    assert!(setup.worktree(old.project, "after").join("NEW.md").exists());
    let clone = setup.clone_of(old.project);
    let url = git(&clone, &["config", "--get", "remote.origin.url"]);
    assert_eq!(url.trim(), new.bare.to_str().unwrap());

    // The old repo's branches no longer back a base or count as pushed.
    let err = setup
        .workspaces
        .create(old.project, "only-old", "stale")
        .await
        .unwrap_err();
    assert!(
        matches!(&err, WorkspaceError::UnknownBase(b) if b == "only-old"),
        "{err}"
    );
    let err = setup.workspaces.archive(before, false).await.unwrap_err();
    let WorkspaceError::Dirty(dirty) = err else {
        panic!("{err}");
    };
    assert!(
        dirty.unpushed.iter().any(|c| c.ends_with(" only in old")),
        "{dirty:?}"
    );
}

#[tokio::test]
async fn an_archive_whose_last_write_fails_is_still_live_and_can_be_archived_again() {
    use std::os::unix::fs::PermissionsExt as _;
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    let id = setup.create(&remote, "main", "kept").await;
    let worktree = setup.worktree(remote.project, "kept");

    // The store cannot make its journal in a folder it may not write.
    let mode = |m| {
        std::fs::set_permissions(setup.data.path(), std::fs::Permissions::from_mode(m)).unwrap()
    };
    mode(0o500);
    let err = setup.workspaces.archive(id, false).await;
    mode(0o700);
    assert!(matches!(err, Err(WorkspaceError::Store(_))), "{err:?}");
    assert!(!setup.record(id).unwrap().deleted);
    assert!(!worktree.exists());

    setup.workspaces.archive(id, false).await.unwrap();
    assert!(setup.record(id).unwrap().deleted);
    let clone = setup.clone_of(remote.project);
    let branches = git(
        &clone,
        &["for-each-ref", "--format=%(refname)", "refs/heads/"],
    );
    assert!(
        branches.lines().any(|b| b == "refs/heads/kept"),
        "{branches}"
    );
}

#[tokio::test]
async fn a_workspace_another_machine_owns_is_not_this_machines() {
    let setup = Setup::new();
    let peer = MachineId::random();
    let id = WorkspaceId::random();
    let record = AnyRecord::Workspace(Record {
        owner: peer,
        version: 1,
        deleted: false,
        body: Workspace {
            id,
            project: ProjectId::random(),
            name: "theirs".into(),
            branch: "theirs".into(),
            base: "main".into(),
            setup: None,
        },
    });
    setup.store.lock().unwrap().apply(&record, peer).unwrap();

    let err = setup.workspaces.worktree(id).await.unwrap_err();
    assert!(
        matches!(err, WorkspaceError::Unknown(w) if w == id),
        "{err}"
    );
    let err = setup.workspaces.archive(id, true).await.unwrap_err();
    assert!(
        matches!(err, WorkspaceError::Unknown(w) if w == id),
        "{err}"
    );
}

#[tokio::test]
async fn an_archive_stopped_by_a_terminal_that_will_not_close_keeps_the_workspace() {
    use std::io::{Read as _, Write as _};
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    let id = setup.create(&remote, "main", "stuck").await;
    let worktree = setup.worktree(remote.project, "stuck");
    let first = setup.sleeper(id, &worktree).await;
    let second = setup.sleeper(id, &worktree).await;

    // The second terminal's holder refuses to close, once.
    let dir = setup.data.path().join("terminals");
    let socket = dir.join(format!("{second}.sock"));
    let real = dir.join(format!("{second}-real.sock"));
    std::fs::rename(&socket, &real).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let stand_in = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0u8; 64];
        let _ = stream.read(&mut request).unwrap();
        stream
            .write_all(&foundry_holder::protocol::Reply::Refused("busy".into()).encode())
            .unwrap();
    });

    let err = setup.workspaces.archive(id, false).await.unwrap_err();
    stand_in.join().unwrap();
    assert!(
        matches!(&err, WorkspaceError::Terminal(TerminalError::Refused { id, .. }) if *id == second),
        "{err}"
    );
    assert!(!setup.record(id).unwrap().deleted);
    assert!(worktree.exists());
    let closed: HashMap<_, _> = setup
        .terminals_of(id)
        .into_iter()
        .map(|t| (t.body.id, t.deleted))
        .collect();
    assert_eq!(closed, HashMap::from([(first, true), (second, false)]));

    // Once it answers again, the archive goes through.
    std::fs::remove_file(&socket).unwrap();
    std::fs::rename(&real, &socket).unwrap();
    setup.workspaces.archive(id, false).await.unwrap();
    assert!(setup.record(id).unwrap().deleted);
    assert!(!worktree.exists());
    assert!(setup.terminals_of(id).iter().all(|t| t.deleted));
}

// --- the lifecycle lock ---

/// Starts a terminal whose child sleeps in workspace `id` through [`Workspaces::start_in`].
async fn start_sleeper(
    workspaces: &Workspaces,
    id: WorkspaceId,
) -> Result<TerminalId, WorkspaceError> {
    let terminals = workspaces.inner.terminals.clone();
    workspaces
        .start_in(id, move |worktree| async move {
            let argv = ["/bin/sh", "-c", "sleep 3127"].map(String::from);
            Ok(terminals.start(id, &worktree, &argv, &[]).await?)
        })
        .await
}

#[tokio::test]
async fn a_start_racing_an_archive_leaves_no_live_terminal_in_the_archived_workspace() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    let (mut ran_before, mut found_gone) = (0, 0);
    for round in 0..10 {
        let id = setup
            .create(&remote, "main", &format!("race {round}"))
            .await;
        // Each is polled, and so queues for the lock, in the order given: odd rounds put the
        // archive first, so starts wait behind it as well as ahead of it.
        let archive = setup.workspaces.archive(id, true);
        let (a, b, archived) = if round % 2 == 0 {
            tokio::join!(
                start_sleeper(&setup.workspaces, id),
                start_sleeper(&setup.workspaces, id),
                archive,
            )
        } else {
            let (archived, a, b) = tokio::join!(
                archive,
                start_sleeper(&setup.workspaces, id),
                start_sleeper(&setup.workspaces, id),
            );
            (a, b, archived)
        };
        archived.unwrap_or_else(|e| panic!("round {round}: {e}"));
        for started in [a, b] {
            match started {
                Ok(_) => ran_before += 1,
                Err(WorkspaceError::Unknown(w)) if w == id => found_gone += 1,
                other => panic!("round {round}: {other:?}"),
            }
        }
        let live: Vec<_> = setup
            .terminals_of(id)
            .into_iter()
            .filter(|t| !t.deleted && t.body.state != TerminalState::Exited)
            .map(|t| t.body.id)
            .collect();
        assert!(
            live.is_empty(),
            "round {round}: terminals {live:?} are live in an archived workspace"
        );
    }
    assert!(
        ran_before > 0 && found_gone > 0,
        "starts ran before the archive {ran_before} times and found it gone {found_gone} times"
    );
}

#[tokio::test]
async fn a_start_in_an_archived_workspace_is_refused_and_starts_nothing() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    let id = setup.create(&remote, "main", "gone").await;
    setup.workspaces.archive(id, false).await.unwrap();
    let started = start_sleeper(&setup.workspaces, id).await;
    assert!(
        matches!(started, Err(WorkspaceError::Unknown(w)) if w == id),
        "{started:?}"
    );
    assert!(setup.terminals_of(id).is_empty());
}

#[tokio::test]
async fn an_archive_waits_for_a_start_whose_caller_hung_up() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    let id = setup.create(&remote, "main", "held").await;
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let (holding, held) = tokio::sync::oneshot::channel::<PathBuf>();
    let caller = {
        let workspaces = Workspaces {
            inner: setup.workspaces.inner.clone(),
        };
        tokio::spawn(async move {
            workspaces
                .start_in(id, move |worktree| async move {
                    let _ = holding.send(worktree);
                    let _ = released.await;
                    Ok::<_, WorkspaceError>(())
                })
                .await
        })
    };
    let worktree = tokio::time::timeout(WAIT, held)
        .await
        .expect("the start takes the lock in time")
        .expect("the start runs");
    assert_eq!(worktree, setup.worktree(remote.project, "held"));
    caller.abort();
    let _ = caller.await;

    let archive = setup.workspaces.archive(id, false);
    tokio::pin!(archive);
    assert!(
        tokio::time::timeout(Duration::from_millis(500), &mut archive)
            .await
            .is_err(),
        "the archive ran while a start still held the workspace"
    );
    assert!(worktree.exists());
    release.send(()).unwrap();
    tokio::time::timeout(WAIT, archive)
        .await
        .expect("the archive runs once the start ends")
        .unwrap();
    assert!(!worktree.exists());
}

/// Runs `work` on `workspace` under its lifecycle lock, from a caller that hangs up (is
/// aborted) once the work has the lock. The work runs until `release` is sent.
async fn hung_up_holding(
    workspaces: &Workspaces,
    workspace: WorkspaceId,
) -> tokio::sync::oneshot::Sender<()> {
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let (holding, held) = tokio::sync::oneshot::channel::<()>();
    let caller = tokio::spawn({
        let workspaces = Workspaces {
            inner: workspaces.inner.clone(),
        };
        async move {
            let work = async move {
                let _ = holding.send(());
                let _ = released.await;
                Ok::<_, WorkspaceError>(())
            };
            let _ = workspaces.locked(workspace, work).await;
        }
    });
    tokio::time::timeout(WAIT, held)
        .await
        .expect("the work takes the lock in time")
        .expect("the work runs");
    caller.abort();
    let _ = caller.await;
    release
}

/// Work that does nothing, for [`Workspaces::locked`].
async fn nothing() -> Result<(), WorkspaceError> {
    Ok(())
}

#[tokio::test]
async fn a_caller_hanging_up_leaves_its_workspace_held_until_the_work_ends() {
    let setup = Setup::new();
    let workspace = WorkspaceId::random();
    let release = hung_up_holding(&setup.workspaces, workspace).await;
    let next = setup.workspaces.locked(workspace, nothing());
    tokio::pin!(next);
    assert!(
        tokio::time::timeout(Duration::from_millis(500), &mut next)
            .await
            .is_err(),
        "a second piece of work on the workspace ran while the first still held it"
    );
    release.send(()).unwrap();
    let ran = tokio::time::timeout(WAIT, next)
        .await
        .expect("the second piece of work runs once the first ends");
    assert!(ran.is_ok());
}

#[tokio::test]
async fn work_on_one_workspace_does_not_wait_for_another() {
    let setup = Setup::new();
    let release = hung_up_holding(&setup.workspaces, WorkspaceId::random()).await;
    let ran = tokio::time::timeout(
        WAIT,
        setup.workspaces.locked(WorkspaceId::random(), nothing()),
    )
    .await
    .expect("work on another workspace does not wait");
    assert!(ran.is_ok());
    release.send(()).unwrap();
    // Once nothing holds or waits for a workspace, its lock is forgotten.
    tokio::time::timeout(WAIT, async {
        while !lock(&setup.workspaces.inner.lifecycle).is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("every lock no one holds is forgotten");
}

#[tokio::test]
async fn work_queued_behind_finished_work_still_holds_the_workspace() {
    let setup = Setup::new();
    let workspace = WorkspaceId::random();
    let release_first = hung_up_holding(&setup.workspaces, workspace).await;
    let (release_second, second_released) = tokio::sync::oneshot::channel::<()>();
    let (second_holding, second_held) = tokio::sync::oneshot::channel::<()>();
    let second = tokio::spawn({
        let workspaces = Workspaces {
            inner: setup.workspaces.inner.clone(),
        };
        async move {
            let work = async move {
                let _ = second_holding.send(());
                let _ = second_released.await;
                Ok::<_, WorkspaceError>(())
            };
            workspaces.locked(workspace, work).await.is_ok()
        }
    });
    // The second is queued (the map's clone, then two for each of the first and the second)
    // before the first ends and hands it the lock.
    tokio::time::timeout(WAIT, async {
        while lock(&setup.workspaces.inner.lifecycle)
            .get(&workspace)
            .map(Arc::strong_count)
            != Some(5)
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the second piece of work queues in time");
    release_first.send(()).unwrap();
    tokio::time::timeout(WAIT, second_held)
        .await
        .expect("the second piece of work takes the lock in time")
        .unwrap();
    let third = setup.workspaces.locked(workspace, nothing());
    tokio::pin!(third);
    assert!(
        tokio::time::timeout(Duration::from_millis(500), &mut third)
            .await
            .is_err(),
        "a third piece of work ran while the second held the workspace"
    );
    release_second.send(()).unwrap();
    assert!(second.await.unwrap());
    let ran = tokio::time::timeout(WAIT, third)
        .await
        .expect("the third piece of work runs once the second ends");
    assert!(ran.is_ok());
}

#[tokio::test]
async fn work_that_panics_leaves_its_workspace_free_and_forgotten() {
    let setup = Setup::new();
    let workspace = WorkspaceId::random();
    let work = async { panic!("the work fails") };
    let failed: Result<(), WorkspaceError> = setup.workspaces.locked(workspace, work).await;
    assert!(
        matches!(&failed, Err(WorkspaceError::Interrupted(e)) if e.is_panic()),
        "{failed:?}"
    );
    assert!(
        lock(&setup.workspaces.inner.lifecycle).is_empty(),
        "the lock of work that panicked is still in the map"
    );
    let ran = tokio::time::timeout(WAIT, setup.workspaces.locked(workspace, nothing()))
        .await
        .expect("the workspace is free after a panic");
    assert!(ran.is_ok());
}

impl Setup {
    /// What a create that died after `git worktree add` and before its record leaves: a branch
    /// `branch` at `main` and its worktree, with no workspace record.
    async fn leftover(&self, remote: &Remote, branch: &str) -> PathBuf {
        let repo = remote.bare.to_string_lossy().into_owned();
        let clone = self
            .workspaces
            .inner
            .clone_for(remote.project, &repo)
            .await
            .unwrap();
        git(&clone, &["fetch", "-q", "origin"]);
        git(
            &clone,
            &[
                "branch",
                "-q",
                "--no-track",
                branch,
                "refs/remotes/origin/main",
            ],
        );
        let worktree = self.worktree(remote.project, branch);
        git(
            &clone,
            &[
                "worktree",
                "add",
                "-q",
                "--",
                worktree.to_str().unwrap(),
                branch,
            ],
        );
        worktree
    }

    /// The worktrees git lists for the project's clone.
    fn listed(&self, project: ProjectId) -> String {
        git(
            &self.clone_of(project),
            &["worktree", "list", "--porcelain"],
        )
    }

    /// The commit `refs/heads/<branch>` names in the project's clone.
    fn branch_at(&self, project: ProjectId, branch: &str) -> String {
        let reference = format!("refs/heads/{branch}");
        git(
            &self.clone_of(project),
            &["rev-parse", "--verify", &reference],
        )
    }
}

/// Every tracing event this thread writes until the guard drops, as text.
fn logs() -> (tracing::subscriber::DefaultGuard, Arc<Mutex<Vec<u8>>>) {
    struct Writer(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Writer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let writer = buffer.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(move || Writer(writer.clone()))
        .finish();
    (tracing::subscriber::set_default(subscriber), buffer)
}

fn text_of(buffer: &Arc<Mutex<Vec<u8>>>) -> String {
    String::from_utf8_lossy(&buffer.lock().unwrap()).into_owned()
}

#[tokio::test]
async fn sweep_removes_a_leftover_worktree_with_no_work_and_keeps_its_branch() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    let worktree = setup.leftover(&remote, "crashed").await;
    let start = setup.branch_at(remote.project, "crashed");

    setup.workspaces.sweep().await.unwrap();

    assert!(!worktree.exists());
    assert!(
        !setup.listed(remote.project).contains("/crashed\n"),
        "{}",
        setup.listed(remote.project)
    );
    assert_eq!(setup.branch_at(remote.project, "crashed"), start);
}

#[tokio::test]
async fn sweep_keeps_a_leftover_with_an_uncommitted_file_and_logs_its_path_and_the_file() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    let worktree = setup.leftover(&remote, "half-done").await;
    std::fs::write(worktree.join("draft.txt"), "mine").unwrap();
    let (_guard, logs) = logs();

    setup.workspaces.sweep().await.unwrap();

    assert_eq!(
        std::fs::read_to_string(worktree.join("draft.txt")).unwrap(),
        "mine"
    );
    let text = text_of(&logs);
    let line = text
        .lines()
        .find(|l| l.contains("draft.txt"))
        .unwrap_or_else(|| panic!("no line names draft.txt: {text}"));
    assert!(line.contains("WARN"), "{line}");
    assert!(
        line.contains(&format!("{}/half-done", remote.project)),
        "{line}"
    );
}

#[tokio::test]
async fn sweep_keeps_a_leftover_with_an_unpushed_commit_on_its_branch_or_a_detached_head() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    let ahead = setup.leftover(&remote, "ahead").await;
    git(
        &ahead,
        &["commit", "-q", "--allow-empty", "-m", "only here"],
    );
    let detached = setup.leftover(&remote, "detached").await;
    git(&detached, &["checkout", "-q", "--detach"]);
    git(
        &detached,
        &["commit", "-q", "--allow-empty", "-m", "off the branch"],
    );
    let (_guard, logs) = logs();

    setup.workspaces.sweep().await.unwrap();

    assert!(ahead.exists());
    assert!(detached.exists());
    let text = text_of(&logs);
    for unpushed in ["only here", "off the branch"] {
        assert!(text.contains(unpushed), "{text}");
    }
}

#[tokio::test]
async fn sweep_leaves_a_live_workspace_and_counts_an_archived_record_as_none() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    let live = setup.create(&remote, "main", "live").await;
    let live_worktree = setup.worktree(remote.project, "live");
    let archived = setup.create(&remote, "main", "archived").await;
    let archived_worktree = setup.worktree(remote.project, "archived");
    // The record says archived, but the worktree is still there.
    let body = setup.body(archived);
    setup.workspaces.inner.write(body, true).await.unwrap();

    setup.workspaces.sweep().await.unwrap();

    assert!(live_worktree.exists());
    assert!(!setup.record(live).unwrap().deleted);
    assert_eq!(
        setup.workspaces.worktree(live).await.unwrap(),
        live_worktree
    );
    assert!(!archived_worktree.exists());
    setup.branch_at(remote.project, "archived");
}

#[tokio::test]
async fn sweep_leaves_a_live_workspace_whose_branch_was_renamed() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    // Every character a letter, so safe_branch keeps the decomposed form; git on macOS keeps
    // the composed one for the same folder. Renamed, the branch no longer matches the record.
    let name = "\u{1112}\u{1161}\u{11ab}";
    let id = setup.create(&remote, "main", name).await;
    let worktree = setup.workspaces.worktree(id).await.unwrap();
    git(&worktree, &["branch", "-m", "renamed"]);
    let other = setup.create(&remote, "main", "Fix").await;
    let other_worktree = setup.workspaces.worktree(other).await.unwrap();
    git(&other_worktree, &["branch", "-m", "renamed-too"]);

    setup.workspaces.sweep().await.unwrap();

    assert!(worktree.exists());
    assert!(other_worktree.exists());
}

#[tokio::test]
async fn sweep_never_touches_a_branch_with_no_worktree() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    let id = setup.create(&remote, "main", "kept").await;
    let worktree = setup.worktree(remote.project, "kept");
    git(
        &worktree,
        &["commit", "-q", "--allow-empty", "-m", "unpushed"],
    );
    setup.workspaces.archive(id, true).await.unwrap();
    let kept = setup.branch_at(remote.project, "kept");

    setup.workspaces.sweep().await.unwrap();

    assert_eq!(setup.branch_at(remote.project, "kept"), kept);
}

#[tokio::test]
async fn sweep_with_no_clones_or_a_clone_with_no_worktrees_does_nothing() {
    let setup = Setup::new();
    setup.workspaces.sweep().await.unwrap();
    assert!(!setup.clones().exists());

    let remote = setup.project(&[("README.md", "hello\n")]);
    let repo = remote.bare.to_string_lossy().into_owned();
    let clone = setup
        .workspaces
        .inner
        .clone_for(remote.project, &repo)
        .await
        .unwrap();
    // A clone stopped half way, and a folder that is not a clone, are passed over.
    std::fs::create_dir_all(
        setup
            .clones()
            .join(format!("{}.partial", ProjectId::random())),
    )
    .unwrap();
    std::fs::create_dir_all(setup.clones().join("not-a-project.git")).unwrap();
    let before = names_in(&setup.clones());

    setup.workspaces.sweep().await.unwrap();

    assert!(clone.exists());
    assert_eq!(names_in(&setup.clones()), before);
}

#[tokio::test]
async fn sweep_forgets_a_leftover_whose_folder_is_gone_only_when_its_head_is_pushed() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    let pushed = setup.leftover(&remote, "pushed-gone").await;
    std::fs::remove_dir_all(&pushed).unwrap();
    let unpushed = setup.leftover(&remote, "unpushed-gone").await;
    git(&unpushed, &["checkout", "-q", "--detach"]);
    git(
        &unpushed,
        &["commit", "-q", "--allow-empty", "-m", "only on the head"],
    );
    std::fs::remove_dir_all(&unpushed).unwrap();
    let (_guard, logs) = logs();

    setup.workspaces.sweep().await.unwrap();

    let listed = setup.listed(remote.project);
    assert!(!listed.contains("/pushed-gone\n"), "{listed}");
    assert!(listed.contains("/unpushed-gone\n"), "{listed}");
    setup.branch_at(remote.project, "pushed-gone");
    let text = text_of(&logs);
    let line = text
        .lines()
        .find(|l| l.contains("unpushed-gone"))
        .unwrap_or_else(|| panic!("no line names unpushed-gone: {text}"));
    assert!(line.contains("only on the head"), "{line}");
}

impl Setup {
    /// This machine's workspaces as another daemon would see them: on the same data directory
    /// and terminals, as machine `me`, with `store`.
    fn workspaces_as(&self, me: MachineId, store: Arc<Mutex<Store>>) -> Workspaces {
        Workspaces::with_git_within(
            me,
            store,
            self.data.path(),
            self.terminals.clone(),
            GIT_WITHIN,
        )
    }

    /// A clean worktree of the project's clone at `path`, on a new branch `branch` at `main`.
    fn worktree_at(&self, project: ProjectId, path: &Path, branch: &str) {
        let clone = self.clone_of(project);
        git(
            &clone,
            &[
                "branch",
                "-q",
                "--no-track",
                branch,
                "refs/remotes/origin/main",
            ],
        );
        git(
            &clone,
            &[
                "worktree",
                "add",
                "-q",
                "--",
                path.to_str().unwrap(),
                branch,
            ],
        );
    }
}

/// The machine's identity can be made again while its data directory stays: its live
/// workspaces' records are then another owner's, and still name their worktrees.
#[tokio::test]
async fn sweep_keeps_the_worktrees_live_records_of_another_identity_name() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    setup.create(&remote, "main", "mine").await;
    // Clean, with nothing ignored: only the record keeps it.
    let worktree = setup.worktree(remote.project, "mine");

    let after_new_identity = setup.workspaces_as(MachineId::random(), setup.store.clone());
    let (_guard, logs) = logs();
    after_new_identity.sweep().await.unwrap();

    assert!(worktree.exists());
    let listed = setup.listed(remote.project);
    assert!(
        listed.contains("/mine\0") || listed.contains("/mine\n"),
        "{listed}"
    );
    let text = text_of(&logs);
    let line = text
        .lines()
        .find(|l| l.contains(&format!("{}/mine", remote.project)))
        .unwrap_or_else(|| panic!("no line names the worktree: {text}"));
    assert!(line.contains("WARN"), "{line}");
    assert!(line.contains("another owner"), "{line}");
}

/// A store made again over the same data directory has no settings, so it cannot say which
/// worktrees its records name: nothing is removed.
#[tokio::test]
async fn sweep_passes_over_a_project_the_store_does_not_hold() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    let leftover = setup.leftover(&remote, "crashed").await;
    let fresh = Arc::new(Mutex::new(
        Store::open(setup.data.path().join("fresh.db")).unwrap(),
    ));

    setup.workspaces_as(setup.me, fresh).sweep().await.unwrap();

    assert!(leftover.exists());
}

#[tokio::test]
async fn sweep_logs_a_clone_git_cannot_list_and_goes_on_to_the_next() {
    let setup = Setup::new();
    let broken = setup.project(&[("README.md", "hello\n")]);
    std::fs::create_dir_all(setup.clone_of(broken.project)).unwrap();
    let remote = setup.project(&[("README.md", "hello\n")]);
    let leftover = setup.leftover(&remote, "crashed").await;
    let (_guard, logs) = logs();

    setup.workspaces.sweep().await.unwrap();

    assert!(!leftover.exists());
    let text = text_of(&logs);
    let line = text
        .lines()
        .find(|l| l.contains(&format!("{}.git", broken.project)))
        .unwrap_or_else(|| panic!("no line names the broken clone: {text}"));
    assert!(line.contains("WARN"), "{line}");
}

#[tokio::test]
async fn sweep_leaves_and_logs_a_clean_leftover_git_will_not_remove() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    let leftover = setup.leftover(&remote, "locked").await;
    git(
        &setup.clone_of(remote.project),
        &["worktree", "lock", "--", leftover.to_str().unwrap()],
    );
    let (_guard, logs) = logs();

    setup.workspaces.sweep().await.unwrap();

    assert!(leftover.exists());
    let text = text_of(&logs);
    let line = text
        .lines()
        .find(|l| l.contains(&format!("{}/locked", remote.project)))
        .unwrap_or_else(|| panic!("no line names the locked leftover: {text}"));
    assert!(line.contains("WARN"), "{line}");
}

/// The record's branch and the folder's name share nothing; only the disk says they are one.
#[tokio::test]
async fn sweep_leaves_a_live_workspace_that_is_the_same_folder_by_another_path() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    let id = setup.create(&remote, "main", "real").await;
    let worktree = setup.worktree(remote.project, "real");
    git(&worktree, &["branch", "-m", "renamed"]);
    std::os::unix::fs::symlink(&worktree, setup.worktree(remote.project, "alias")).unwrap();
    let mut body = setup.body(id);
    body.branch = "alias".into();
    setup.workspaces.inner.write(body, false).await.unwrap();

    setup.workspaces.sweep().await.unwrap();

    assert!(worktree.exists());
}

#[tokio::test]
async fn sweep_leaves_the_entry_of_a_live_workspace_whose_folder_is_gone() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    // A decomposed name, which git on macOS keeps composed, a plain one, and one whose branch
    // was renamed, so only its folder's name is left to match.
    for name in ["\u{1112}\u{1161}\u{11ab}", "plain", "renamed"] {
        let id = setup.create(&remote, "main", name).await;
        let worktree = setup.workspaces.worktree(id).await.unwrap();
        if name == "renamed" {
            git(&worktree, &["branch", "-m", "elsewhere"]);
        }
        std::fs::remove_dir_all(worktree).unwrap();
    }
    let entries = |listed: &str| {
        listed
            .lines()
            .filter(|l| l.starts_with("worktree "))
            .count()
    };
    // The clone and the three workspaces.
    assert_eq!(entries(&setup.listed(remote.project)), 4);

    setup.workspaces.sweep().await.unwrap();

    let listed = setup.listed(remote.project);
    assert_eq!(entries(&listed), 4, "{listed}");
}

#[tokio::test]
async fn sweep_never_touches_a_worktree_outside_the_projects_folder_of_worktrees() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    setup.leftover(&remote, "crashed").await;
    let elsewhere = setup.data.path().join("elsewhere").join("by-hand");
    setup.worktree_at(remote.project, &elsewhere, "by-hand");
    // In the folder of worktrees, but not under this project's.
    let other_project = setup
        .data
        .path()
        .join("worktrees")
        .join("elsewhere")
        .join("by-hand-too");
    setup.worktree_at(remote.project, &other_project, "by-hand-too");
    let lookalike = setup
        .data
        .path()
        .join("other")
        .join(remote.project.to_string())
        .join("lookalike");
    setup.worktree_at(remote.project, &lookalike, "lookalike");

    setup.workspaces.sweep().await.unwrap();

    assert!(elsewhere.exists());
    assert!(other_project.exists());
    assert!(lookalike.exists());
    assert!(!setup.worktree(remote.project, "crashed").exists());
}

/// A worktree moved by hand is found only by the branch it has checked out.
#[tokio::test]
async fn sweep_leaves_a_live_workspace_whose_worktree_was_moved() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    setup.create(&remote, "main", "a").await;
    let moved = setup.worktree(remote.project, "b");
    git(
        &setup.clone_of(remote.project),
        &[
            "worktree",
            "move",
            setup.worktree(remote.project, "a").to_str().unwrap(),
            moved.to_str().unwrap(),
        ],
    );

    setup.workspaces.sweep().await.unwrap();

    assert!(moved.exists());
}

#[tokio::test]
async fn sweep_leaves_and_logs_a_leftover_it_cannot_check() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    let leftover = setup.leftover(&remote, "unreadable").await;
    std::fs::write(leftover.join(".git"), "gitdir: /nonexistent\n").unwrap();
    let (_guard, logs) = logs();

    setup.workspaces.sweep().await.unwrap();

    assert!(leftover.exists());
    let text = text_of(&logs);
    let line = text
        .lines()
        .find(|l| l.contains(&format!("{}/unreadable", remote.project)))
        .unwrap_or_else(|| panic!("no line names the leftover: {text}"));
    assert!(line.contains("WARN"), "{line}");
    assert!(line.contains("could not be checked"), "{line}");
}

/// A create that dies leaves a bare checkout: setup and sessions start only after its record.
/// A folder holding files git ignores was worked in, and git could not give them back.
#[tokio::test]
async fn sweep_leaves_and_logs_a_leftover_holding_files_git_ignores() {
    let setup = Setup::new();
    let remote = setup.project(&[(".gitignore", ".env\nbuild/\n")]);
    let leftover = setup.leftover(&remote, "worked-in").await;
    std::fs::write(leftover.join(".env"), "mine").unwrap();
    std::fs::create_dir_all(leftover.join("build")).unwrap();
    std::fs::write(leftover.join("build").join("out.bin"), "o").unwrap();
    let (_guard, logs) = logs();

    setup.workspaces.sweep().await.unwrap();

    assert_eq!(
        std::fs::read_to_string(leftover.join(".env")).unwrap(),
        "mine"
    );
    let text = text_of(&logs);
    let line = text
        .lines()
        .find(|l| l.contains(&format!("{}/worked-in", remote.project)))
        .unwrap_or_else(|| panic!("no line names the leftover: {text}"));
    assert!(line.contains("WARN"), "{line}");
    assert!(line.contains(".env"), "{line}");
    assert!(line.contains("build/"), "{line}");
}

/// A store made again over the same data directory gets its settings back from the fleet, but
/// never its own workspace records: nothing a live workspace holds that git cannot give back
/// is removed.
#[tokio::test]
async fn sweep_with_settings_and_no_records_keeps_what_git_cannot_give_back() {
    let setup = Setup::new();
    let remote = setup.project(&[(".gitignore", ".env\n")]);
    setup.create(&remote, "main", "worked-in").await;
    let worked_in = setup.worktree(remote.project, "worked-in");
    std::fs::write(worked_in.join(".env"), "mine").unwrap();
    let fresh = Arc::new(Mutex::new(
        Store::open(setup.data.path().join("fresh.db")).unwrap(),
    ));
    {
        let settings = setup.store.lock().unwrap().settings().unwrap();
        assert!(fresh.lock().unwrap().apply_settings(settings).unwrap());
    }

    setup.workspaces_as(setup.me, fresh).sweep().await.unwrap();

    assert_eq!(
        std::fs::read_to_string(worked_in.join(".env")).unwrap(),
        "mine"
    );
}

/// A row a newer build wrote, from another owner, which this build cannot read.
fn unreadable_row(db: &Path) {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.execute(
        "INSERT INTO records (kind, id, owner, version, record) VALUES (?1, ?2, ?3, 1, ?4)",
        rusqlite::params![
            "workspace",
            WorkspaceId::random().to_string(),
            MachineId::random().to_string(),
            r#"{"kind":"from-a-newer-build"}"#,
        ],
    )
    .unwrap();
}

/// What the store holds cannot all be read, so it cannot say which worktrees are named: the
/// sweep is skipped, says so, and the daemon still starts.
#[tokio::test]
async fn sweep_skips_and_logs_when_a_record_cannot_be_read() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    let leftover = setup.leftover(&remote, "crashed").await;
    unreadable_row(&setup.data.path().join("foundry.db"));
    let (_guard, logs) = logs();

    setup.workspaces.sweep().await.unwrap();

    assert!(leftover.exists());
    let text = text_of(&logs);
    assert!(
        text.lines()
            .any(|l| l.contains("WARN") && l.contains("sweep")),
        "{text}"
    );
}

/// A row of another kind this build cannot read says nothing about which worktrees are
/// named, so the sweep still runs.
#[tokio::test]
async fn sweep_runs_when_only_a_row_of_another_kind_cannot_be_read() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    let leftover = setup.leftover(&remote, "crashed").await;
    let conn = rusqlite::Connection::open(setup.data.path().join("foundry.db")).unwrap();
    conn.execute(
        "INSERT INTO records (kind, id, owner, version, record) VALUES (?1, ?2, ?3, 1, ?4)",
        rusqlite::params![
            "terminal",
            TerminalId::random().to_string(),
            MachineId::random().to_string(),
            r#"{"kind":"from-a-newer-build"}"#,
        ],
    )
    .unwrap();

    setup.workspaces.sweep().await.unwrap();

    assert!(!leftover.exists());
}

/// `git worktree list --porcelain` before git 2.36 has no `-z`: one field per line, and a blank
/// line after each worktree.
#[test]
fn worktrees_in_reads_the_porcelain_list_line_by_line() {
    let list = "worktree /data/clones/p.git\nbare\n\n\
                worktree /data/worktrees/p/fix-login\nHEAD abc123\nbranch refs/heads/fix-login\n\n\
                worktree /data/worktrees/p/detached\nHEAD def456\ndetached\n\n";
    let found: Vec<_> = worktrees_in(list)
        .into_iter()
        .map(|l| (l.path, l.head, l.branch))
        .collect();
    assert_eq!(
        found,
        vec![
            (PathBuf::from("/data/clones/p.git"), None, None),
            (
                PathBuf::from("/data/worktrees/p/fix-login"),
                Some("abc123".to_owned()),
                Some("fix-login".to_owned())
            ),
            (
                PathBuf::from("/data/worktrees/p/detached"),
                Some("def456".to_owned()),
                None
            ),
        ]
    );
}

/// The sweep has one deadline for all of it. A project it cannot get through in time, here
/// one whose lock is held, stops it with a WARN line naming the clone, and start goes on.
#[tokio::test]
async fn sweep_stops_at_its_deadline_and_logs_the_clone_it_stopped_in() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    let leftover = setup.leftover(&remote, "crashed").await;
    let lock = setup.workspaces.inner.lock(remote.project);
    let _held = lock.lock().await;
    let (_guard, logs) = logs();

    let started = Instant::now();
    setup
        .workspaces
        .inner
        .sweep(Duration::from_millis(200))
        .await
        .unwrap();

    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(leftover.exists());
    let text = text_of(&logs);
    let line = text
        .lines()
        .find(|l| l.contains(&format!("{}.git", remote.project)))
        .unwrap_or_else(|| panic!("no line names the clone: {text}"));
    assert!(line.contains("WARN"), "{line}");
    assert!(line.contains("next start"), "{line}");
}

#[tokio::test]
async fn sweep_logs_a_clone_it_cannot_read_and_goes_on_to_the_next() {
    let setup = Setup::new();
    let broken = setup.project(&[("README.md", "hello\n")]);
    // A link to itself cannot be read: the check for it fails rather than saying no.
    let clone = setup.clone_of(broken.project);
    std::fs::create_dir_all(setup.clones()).unwrap();
    std::os::unix::fs::symlink(&clone, &clone).unwrap();
    let remote = setup.project(&[("README.md", "hello\n")]);
    let leftover = setup.leftover(&remote, "crashed").await;
    let (_guard, logs) = logs();

    setup.workspaces.sweep().await.unwrap();

    assert!(!leftover.exists());
    let text = text_of(&logs);
    let line = text
        .lines()
        .find(|l| l.contains(&format!("{}.git", broken.project)))
        .unwrap_or_else(|| panic!("no line names the unreadable clone: {text}"));
    assert!(line.contains("WARN"), "{line}");
    assert!(line.contains("could not be read"), "{line}");
}

#[tokio::test]
async fn changes_show_committed_uncommitted_and_new_work_since_the_base() {
    let setup = Setup::new();
    let remote = setup.project(&[
        ("README.md", "hello\nworld\n"),
        ("old.txt", "old\n"),
        ("gone.txt", "bye\n"),
    ]);
    let id = setup.create(&remote, "main", "edits").await;
    let worktree = setup.worktree(remote.project, "edits");
    // The base moves on after the workspace was made: none of it is the workspace's change.
    remote.push("main", &[("later.txt", "later\n")]);
    git(&setup.clone_of(remote.project), &["fetch", "-q", "origin"]);

    std::fs::write(worktree.join("committed.txt"), "c\n").unwrap();
    git(&worktree, &["add", "committed.txt"]);
    git(&worktree, &["commit", "-q", "-m", "add committed"]);
    std::fs::write(worktree.join("README.md"), "hello\nthere\n").unwrap();
    std::fs::remove_file(worktree.join("gone.txt")).unwrap();
    git(&worktree, &["mv", "old.txt", "moved.txt"]);
    std::fs::write(worktree.join("new file.txt"), "one\ntwo").unwrap();
    std::fs::write(worktree.join("blob.bin"), [0u8, 1, 2]).unwrap();
    std::fs::write(worktree.join(".gitignore"), "ignored.txt\n").unwrap();
    std::fs::write(worktree.join("ignored.txt"), "never shown").unwrap();
    // A link the agent made to a file outside the worktree is listed, never read.
    let outside = setup.remotes.path().join("secret.txt");
    std::fs::write(&outside, "token-xyz").unwrap();
    std::os::unix::fs::symlink(&outside, worktree.join("link")).unwrap();

    let changes = setup.workspaces.changes(id).await.unwrap();
    assert_eq!(changes.base, "main");
    assert_eq!(changes.commits.len(), 1, "{:?}", changes.commits);
    let commit = &changes.commits[0];
    assert_eq!(commit.subject, "add committed");
    assert_eq!(
        commit.hash,
        git(&worktree, &["rev-parse", "--short", "HEAD"]).trim()
    );
    let paths: Vec<&str> = changes.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            ".gitignore",
            "README.md",
            "blob.bin",
            "committed.txt",
            "gone.txt",
            "link",
            "moved.txt",
            "new file.txt"
        ]
    );
    let file = |path: &str| changes.files.iter().find(|f| f.path == path).unwrap();

    let readme = file("README.md");
    assert_eq!(readme.status, FileStatus::Modified);
    assert_eq!((readme.added, readme.removed), (Some(1), Some(1)));
    assert!(readme.uncommitted);
    let patch = readme.patch.as_deref().unwrap();
    assert!(patch.starts_with("@@ "), "{patch}");
    assert!(patch.contains("-world\n+there\n"), "{patch}");

    let committed = file("committed.txt");
    assert_eq!(committed.status, FileStatus::Added);
    assert!(!committed.uncommitted);
    assert_eq!(committed.patch.as_deref(), Some("@@ -0,0 +1 @@\n+c\n"));

    let moved = file("moved.txt");
    assert_eq!(moved.status, FileStatus::Renamed);
    assert_eq!(moved.from.as_deref(), Some("old.txt"));
    assert_eq!(file("gone.txt").status, FileStatus::Deleted);

    let new = file("new file.txt");
    assert_eq!(new.status, FileStatus::Untracked);
    assert_eq!((new.added, new.removed), (Some(2), Some(0)));
    assert!(new.patch.as_deref().unwrap().contains("+one\n+two\n"));

    let blob = file("blob.bin");
    assert_eq!((blob.added, blob.patch.as_deref()), (None, None));
    let link = file("link");
    assert_eq!((link.added, link.patch.as_deref()), (None, None));
    let shown = format!("{changes:?}");
    assert!(!shown.contains("token-xyz") && !shown.contains("never shown"));

    // A read changes nothing: the index still holds only what was added by hand.
    let staged = git(
        &worktree,
        &["diff", "--cached", "--no-renames", "--name-only"],
    );
    assert_eq!(staged.lines().collect::<Vec<_>>(), ["moved.txt", "old.txt"]);
}

#[tokio::test]
async fn changes_pair_each_file_with_its_own_diff_past_a_link_made_a_file() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    let id = setup.create(&remote, "main", "typechange").await;
    let worktree = setup.worktree(remote.project, "typechange");
    // The base holds `a` as a link: git prints a link made a file as two diffs, a delete and
    // an add, though it lists the path once.
    std::os::unix::fs::symlink("README.md", worktree.join("a")).unwrap();
    std::fs::write(worktree.join("b.txt"), "b\n").unwrap();
    git(&worktree, &["add", "a", "b.txt"]);
    git(&worktree, &["commit", "-q", "-m", "link"]);
    git(
        &worktree,
        &["update-ref", "refs/remotes/origin/main", "HEAD"],
    );
    std::fs::remove_file(worktree.join("a")).unwrap();
    std::fs::write(worktree.join("a"), "real\n").unwrap();
    std::fs::write(worktree.join("b.txt"), "b2\n").unwrap();

    let changes = setup.workspaces.changes(id).await.unwrap();
    let file = |path: &str| changes.files.iter().find(|f| f.path == path).unwrap();
    assert_eq!(file("a").status, FileStatus::Modified);
    let b = file("b.txt").patch.as_deref().unwrap();
    assert!(b.contains("-b\n+b2\n"), "{b}");
}

#[tokio::test]
async fn a_changes_read_never_writes_the_index() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    let id = setup.create(&remote, "main", "touched").await;
    let worktree = setup.worktree(remote.project, "touched");
    // The same text with a newer time: a status free to refresh the index writes it, under the
    // index lock an agent's `git add` in the same worktree then fails on.
    std::thread::sleep(Duration::from_millis(1100));
    std::fs::write(worktree.join("README.md"), "hello\n").unwrap();
    let index = worktree.join(git(&worktree, &["rev-parse", "--git-path", "index"]).trim());
    let before = std::fs::read(&index).unwrap();

    setup.workspaces.changes(id).await.unwrap();

    assert!(
        std::fs::read(&index).unwrap() == before,
        "the read wrote the index"
    );
    let left: Vec<_> = std::fs::read_dir(index.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .filter(|n| n.to_string_lossy().starts_with("index."))
        .collect();
    assert!(left.is_empty(), "the read left {left:?}");
}

#[tokio::test]
async fn a_changes_read_does_not_wait_for_the_lifecycle_lock() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    let id = setup.create(&remote, "main", "busy").await;
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let (holding, held) = tokio::sync::oneshot::channel::<()>();
    let start = {
        let workspaces = Workspaces {
            inner: setup.workspaces.inner.clone(),
        };
        tokio::spawn(async move {
            workspaces
                .start_in(id, move |_| async move {
                    let _ = holding.send(());
                    let _ = released.await;
                    Ok::<_, WorkspaceError>(())
                })
                .await
        })
    };
    tokio::time::timeout(WAIT, held)
        .await
        .expect("the start takes the lock in time")
        .expect("the start runs");

    let read = tokio::time::timeout(WAIT, setup.workspaces.changes(id)).await;
    let _ = release.send(());
    start.await.unwrap().unwrap();
    read.expect("the read does not queue behind the start")
        .unwrap();
}

#[tokio::test]
async fn changes_send_no_diff_past_the_caps_and_still_list_the_file() {
    use super::changes::{ALL_AT_MOST, FILE_AT_MOST, NEW_READ_AT_MOST};
    let setup = Setup::new();
    let remote = setup.project(&[("big.txt", "x\n")]);
    let id = setup.create(&remote, "main", "caps").await;
    let worktree = setup.worktree(remote.project, "caps");
    let line = format!("{}\n", "z".repeat(99));

    // A tracked file whose diff is over the cap for one file: counted, not sent.
    std::fs::write(
        worktree.join("big.txt"),
        line.repeat(FILE_AT_MOST / 100 + 1),
    )
    .unwrap();
    // Added files each under the cap for one file that together pass the cap for a whole read.
    let each = FILE_AT_MOST * 9 / 10 / 100;
    let fill = ALL_AT_MOST / (each * 101) + 1;
    for n in 0..fill {
        std::fs::write(worktree.join(format!("fill-{n:02}.txt")), line.repeat(each)).unwrap();
    }
    git(&worktree, &["add", "."]);
    // A new file as large, read after them, finds the budget spent too.
    std::fs::write(worktree.join("late.txt"), line.repeat(each)).unwrap();
    // A new file over the cap for one file: listed, never read.
    std::fs::write(
        worktree.join("huge.txt"),
        line.repeat(FILE_AT_MOST / 100 + 1),
    )
    .unwrap();
    // More new files than are read: every one is listed, the ones past the count by name only.
    let read_tiny = NEW_READ_AT_MOST - 2;
    for n in 0..=read_tiny {
        std::fs::write(worktree.join(format!("tiny-{n:03}.txt")), "t\n").unwrap();
    }

    let changes = setup.workspaces.changes(id).await.unwrap();
    let file = |path: &str| {
        changes
            .files
            .iter()
            .find(|f| f.path == path)
            .unwrap_or_else(|| panic!("{path} is listed"))
    };
    let big = file("big.txt");
    assert_eq!(big.patch, None);
    assert_eq!(
        (big.added, big.removed),
        (Some(FILE_AT_MOST as u64 / 100 + 1), Some(1))
    );
    let huge = file("huge.txt");
    assert_eq!((huge.added, huge.patch.as_deref()), (None, None));

    let sent: usize = changes
        .files
        .iter()
        .filter_map(|f| f.patch.as_ref().map(String::len))
        .sum();
    assert!(sent <= ALL_AT_MOST, "{sent} bytes sent");
    let first = file("fill-00.txt");
    assert!(first.patch.is_some());
    let last = file(&format!("fill-{:02}.txt", fill - 1));
    assert_eq!(last.status, FileStatus::Added);
    assert_eq!(last.patch, None);
    assert_eq!(
        last.added,
        Some(each as u64),
        "a file past the budget keeps its count"
    );

    let late = file("late.txt");
    assert_eq!(late.status, FileStatus::Untracked);
    assert_eq!(
        (late.added, late.patch.as_deref()),
        (Some(each as u64), None)
    );

    // New files are read in name order: `huge.txt` and `late.txt` come before `tiny-*`.
    let in_count = file(&format!("tiny-{:03}.txt", read_tiny - 1));
    assert_eq!(in_count.added, Some(1));
    let past_count = file(&format!("tiny-{read_tiny:03}.txt"));
    assert_eq!(past_count.status, FileStatus::Untracked);
    assert_eq!(
        (past_count.added, past_count.patch.as_deref()),
        (None, None)
    );
}

#[tokio::test]
async fn a_commit_on_the_branch_shows_only_its_own_change() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\nworld\n"), ("old.txt", "old\n")]);
    let id = setup.create(&remote, "main", "steps").await;
    let worktree = setup.worktree(remote.project, "steps");
    std::fs::write(worktree.join("first.txt"), "one\n").unwrap();
    git(&worktree, &["add", "first.txt"]);
    git(&worktree, &["commit", "-q", "-m", "add first"]);
    let first = git(&worktree, &["rev-parse", "--short", "HEAD"])
        .trim()
        .to_owned();
    std::fs::write(worktree.join("README.md"), "hello\nthere\n").unwrap();
    git(&worktree, &["mv", "old.txt", "moved.txt"]);
    git(&worktree, &["commit", "-qam", "edit readme and move old"]);
    let second = git(&worktree, &["rev-parse", "--short", "HEAD"])
        .trim()
        .to_owned();
    // Work not committed yet is never part of a commit.
    std::fs::write(worktree.join("first.txt"), "one\ntwo\n").unwrap();
    std::fs::write(worktree.join("new.txt"), "new\n").unwrap();

    let one = setup.workspaces.commit(id, &first).await.unwrap();
    assert_eq!(one.base, "main");
    assert_eq!(one.commits.len(), 1);
    assert_eq!(
        (
            one.commits[0].hash.as_str(),
            one.commits[0].subject.as_str()
        ),
        (first.as_str(), "add first")
    );
    assert_eq!(one.files.len(), 1, "{:?}", one.files);
    let file = &one.files[0];
    assert_eq!(
        (file.path.as_str(), file.status),
        ("first.txt", FileStatus::Added)
    );
    assert_eq!(
        (file.added, file.removed, file.uncommitted),
        (Some(1), Some(0), false)
    );
    assert_eq!(file.patch.as_deref(), Some("@@ -0,0 +1 @@\n+one\n"));

    let two = setup.workspaces.commit(id, &second).await.unwrap();
    let paths: Vec<&str> = two.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["README.md", "moved.txt"]);
    assert!(
        two.files[0]
            .patch
            .as_deref()
            .unwrap()
            .contains("-world\n+there\n")
    );
    assert_eq!(two.files[1].status, FileStatus::Renamed);
    assert_eq!(two.files[1].from.as_deref(), Some("old.txt"));
    assert!(two.files.iter().all(|f| !f.uncommitted));
}

#[tokio::test]
async fn a_commit_not_on_the_branch_is_refused_as_unknown() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    let id = setup.create(&remote, "main", "nothing").await;
    let worktree = setup.worktree(remote.project, "nothing");
    // The base's own commit is not the workspace's change, and no option reaches git.
    let base = git(&worktree, &["rev-parse", "--short", "HEAD"])
        .trim()
        .to_owned();
    for asked in [base.as_str(), "--output=x", "HEAD", "abc1234", ""] {
        let err = setup.workspaces.commit(id, asked).await.unwrap_err();
        assert!(
            matches!(err, WorkspaceError::UnknownCommit(_)),
            "{asked}: {err}"
        );
    }
    assert!(!worktree.join("x").exists());
}

#[tokio::test]
async fn changes_of_a_workspace_whose_worktree_is_gone_are_refused_as_unknown() {
    // As an archive leaves it for a moment: the worktree removed, the record not yet written.
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    let id = setup.create(&remote, "main", "removed").await;
    std::fs::remove_dir_all(setup.worktree(remote.project, "removed")).unwrap();
    let err = setup.workspaces.changes(id).await.unwrap_err();
    assert!(matches!(err, WorkspaceError::Unknown(_)), "{err}");
}

#[tokio::test]
async fn changes_read_while_an_archive_removes_the_worktree_are_refused_as_unknown() {
    // Git removes a worktree file by file, so a read beside it sees the files go one by one, as
    // deletes, and then no repo at all, while the folder is still there.
    let setup = Setup::new();
    let names: Vec<String> = (0..1500).map(|i| format!("f{i:04}.txt")).collect();
    let files: Vec<(&str, &str)> = names.iter().map(|n| (n.as_str(), "x\n")).collect();
    let remote = setup.project(&files);
    let id = setup.create(&remote, "main", "leaving").await;
    let done = std::sync::atomic::AtomicBool::new(false);

    let archive = async {
        let archived = setup.workspaces.archive(id, true).await;
        done.store(true, std::sync::atomic::Ordering::SeqCst);
        archived
    };
    let reads = async {
        let mut seen = Vec::new();
        while !done.load(std::sync::atomic::Ordering::SeqCst) {
            seen.push(setup.workspaces.changes(id).await);
        }
        seen
    };
    let (archived, seen) = tokio::join!(archive, reads);
    archived.unwrap();

    for read in &seen {
        match read {
            Ok(changes) => assert!(
                changes.files.is_empty(),
                "a read of a half-removed worktree: {} files",
                changes.files.len()
            ),
            Err(WorkspaceError::Unknown(_)) => {}
            Err(other) => panic!("a read during the archive failed as {other}, not as gone"),
        }
    }
}

/// A read that starts and ends while the worktree is being removed, with nothing removed yet.
#[tokio::test]
async fn changes_are_refused_as_unknown_while_the_worktree_is_being_removed() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    let id = setup.create(&remote, "main", "removing").await;
    let removing = Removing::mark(&setup.workspaces.inner, id);
    let err = setup.workspaces.changes(id).await.unwrap_err();
    assert!(matches!(err, WorkspaceError::Unknown(_)), "{err}");
    drop(removing);
    setup.workspaces.changes(id).await.unwrap();
}

#[tokio::test]
async fn changes_of_a_workspace_that_is_not_here_are_refused() {
    let setup = Setup::new();
    let err = setup
        .workspaces
        .changes(WorkspaceId::random())
        .await
        .unwrap_err();
    assert!(matches!(err, WorkspaceError::Unknown(_)), "{err}");
}

/// Workspaces over `setup`'s data that ask GitHub with a script printing `answer`, which also
/// writes each call's arguments to `asked` in the script's folder.
fn with_gh(setup: &Setup, answer: &str) -> (Workspaces, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let dir = setup.remotes.path().join("gh");
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("gh");
    let asked = dir.join("asked");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\ncat <<'JSON'\n{answer}\nJSON\n",
            asked.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let workspaces = Workspaces::with_git_within(
        setup.me,
        setup.store.clone(),
        setup.data.path(),
        setup.terminals.clone(),
        GIT_WITHIN,
    )
    .with_gh(&script);
    (workspaces, asked)
}

#[tokio::test]
async fn a_branch_never_pushed_counts_what_is_only_here_and_never_asks_github() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    let id = setup.create(&remote, "main", "local only").await;
    let worktree = setup.worktree(remote.project, "local-only");
    let (workspaces, asked) = with_gh(&setup, "[]");

    let fresh = workspaces.shipping(id).await.unwrap();
    assert_eq!(fresh.branch, "local-only");
    assert!(!fresh.pushed);
    assert_eq!(fresh.unpushed, 0);

    std::fs::write(worktree.join("a.txt"), "a").unwrap();
    git(&worktree, &["add", "a.txt"]);
    git(&worktree, &["commit", "-q", "-m", "a"]);
    git(&worktree, &["commit", "-q", "--allow-empty", "-m", "b"]);
    let read = workspaces.shipping(id).await.unwrap();
    assert!(!read.pushed);
    assert_eq!(read.unpushed, 2);
    assert_eq!(read.pr, None);
    assert!(!read.unreachable);
    assert!(
        !asked.exists(),
        "GitHub was asked about a branch it cannot have"
    );
}

#[tokio::test]
async fn a_pushed_branch_shows_its_pull_request_and_what_is_not_pushed_yet() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    let id = setup.create(&remote, "main", "shipped").await;
    let worktree = setup.worktree(remote.project, "shipped");
    git(&worktree, &["commit", "-q", "--allow-empty", "-m", "done"]);
    git(&worktree, &["push", "-q", "origin", "HEAD"]);
    let done = git(&worktree, &["rev-parse", "HEAD"]);
    let (workspaces, asked) = with_gh(
        &setup,
        &format!(
            r#"[{{"number":7,"title":"Ship it","url":"https://github.com/o/r/pull/7","state":"MERGED","isDraft":false,"reviewDecision":"APPROVED","statusCheckRollup":[],"headRefOid":"{}"}}]"#,
            done.trim()
        ),
    );

    let read = workspaces.shipping(id).await.unwrap();
    assert!(read.pushed);
    assert_eq!(read.unpushed, 0);
    let pr = read.pr.expect("its pull request");
    assert_eq!((pr.number, pr.state), (7, PrState::Merged));
    assert!(!read.unreachable);
    let args = std::fs::read_to_string(&asked).unwrap();
    assert!(args.contains("pr list"), "{args}");
    assert!(args.trim_end().ends_with("--head shipped"), "{args}");

    git(&worktree, &["commit", "-q", "--allow-empty", "-m", "after"]);
    let moved_past = workspaces.shipping(id).await.unwrap();
    assert_eq!(moved_past.unpushed, 1);
    assert_eq!(
        moved_past.pr.map(|pr| pr.number),
        Some(7),
        "a pull request whose head the branch has moved past is still its own"
    );
}

#[tokio::test]
async fn a_pull_request_from_another_branch_of_the_same_name_is_never_shown() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    let id = setup.create(&remote, "main", "namesake").await;
    let worktree = setup.worktree(remote.project, "namesake");
    git(&worktree, &["commit", "-q", "--allow-empty", "-m", "mine"]);
    git(&worktree, &["push", "-q", "origin", "HEAD"]);
    let mine = git(&worktree, &["rev-parse", "HEAD"]).trim().to_owned();
    let in_base = git(&worktree, &["rev-parse", "refs/remotes/origin/main"])
        .trim()
        .to_owned();
    let unknown = "0123456789abcdef0123456789abcdef01234567";
    let listed = |number: u64, state: &str, head: &str| {
        format!(
            r#"{{"number":{number},"title":"PR {number}","url":"https://github.com/o/r/pull/{number}","state":"{state}","isDraft":false,"reviewDecision":"","statusCheckRollup":[],"headRefOid":"{head}"}}"#
        )
    };

    let (others, asked) = with_gh(
        &setup,
        &format!(
            "[{},{}]",
            listed(32, "MERGED", unknown),
            listed(31, "CLOSED", &in_base)
        ),
    );
    let read = others.shipping(id).await.unwrap();
    assert_eq!(
        read.pr, None,
        "an old branch's pull request is not this one's"
    );
    assert!(!read.unreachable);
    let args = std::fs::read_to_string(&asked).unwrap();
    let limit: u64 = args
        .split_whitespace()
        .skip_while(|a| *a != "--limit")
        .nth(1)
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    assert!(
        limit > 1,
        "only the newest pull request was asked for: {args}"
    );

    let (behind, _) = with_gh(
        &setup,
        &format!(
            "[{},{}]",
            listed(32, "MERGED", unknown),
            listed(40, "OPEN", &mine)
        ),
    );
    let read = behind.shipping(id).await.unwrap();
    assert_eq!(
        read.pr.map(|pr| (pr.number, pr.state)),
        Some((40, PrState::Open)),
        "its own pull request is found behind a newer one of the same name"
    );
}

#[tokio::test]
async fn a_github_that_cannot_answer_leaves_the_pull_request_unknown_not_an_error() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    let id = setup.create(&remote, "main", "offline").await;
    let worktree = setup.worktree(remote.project, "offline");
    git(&worktree, &["push", "-q", "origin", "HEAD"]);

    let missing = Workspaces::with_git_within(
        setup.me,
        setup.store.clone(),
        setup.data.path(),
        setup.terminals.clone(),
        GIT_WITHIN,
    )
    .with_gh(&setup.remotes.path().join("no-such-gh"));
    let read = missing.shipping(id).await.unwrap();
    assert!(read.pushed);
    assert_eq!(read.pr, None);
    assert!(read.unreachable);

    let (garbled, _) = with_gh(
        &setup,
        "To get started with GitHub CLI, please run: gh auth login",
    );
    assert!(garbled.shipping(id).await.unwrap().unreachable);
}

#[tokio::test]
async fn the_shipping_of_an_unknown_workspace_is_refused() {
    let setup = Setup::new();
    let missing = WorkspaceId::random();
    assert!(matches!(
        setup.workspaces.shipping(missing).await,
        Err(WorkspaceError::Unknown(id)) if id == missing
    ));
}

#[tokio::test]
async fn a_remote_branch_of_the_same_name_without_this_workspaces_work_is_not_pushed() {
    let setup = Setup::new();
    let remote = setup.project(&[("README.md", "hello\n")]);
    let id = setup.create(&remote, "main", "reused").await;
    let worktree = setup.worktree(remote.project, "reused");
    // An older branch of the same name, left on origin with work this one does not have.
    git(
        &worktree,
        &["commit", "-q", "--allow-empty", "-m", "old work"],
    );
    git(&worktree, &["push", "-q", "origin", "HEAD"]);
    git(
        &worktree,
        &["reset", "-q", "--hard", "refs/remotes/origin/main"],
    );
    git(&worktree, &["commit", "-q", "--allow-empty", "-m", "mine"]);
    let (workspaces, asked) = with_gh(&setup, "[]");

    let read = workspaces.shipping(id).await.unwrap();
    assert!(!read.pushed, "the old branch's push is not this one's");
    assert_eq!(read.unpushed, 1);
    assert_eq!(read.pr, None);
    assert!(!read.unreachable);
    assert!(
        !asked.exists(),
        "GitHub was asked about a branch this workspace never pushed"
    );

    git(&worktree, &["push", "-q", "--force", "origin", "HEAD"]);
    let read = workspaces.shipping(id).await.unwrap();
    assert!(read.pushed);
    assert_eq!(read.unpushed, 0);
}
