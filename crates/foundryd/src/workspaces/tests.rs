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
        let terminals = Arc::new(Terminals::new(me, store.clone(), data.path(), holder()));
        let workspaces = Workspaces::new(me, store.clone(), data.path(), terminals.clone());
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
        ("  fix   login  ", "fix-login"),
        ("café ☕ täst", "café-täst"),
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
    let branch = safe_branch(&"é".repeat(300));
    assert!(branch.len() <= 100, "{} bytes", branch.len());
    assert!(branch.chars().all(|c| c == 'é'));
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
    assert!(matches!(err, WorkspaceError::Git { .. }), "{err}");
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
    let remote = setup.project(&[("README.md", "hello\n"), ("keep.txt", "keep\n")]);
    let id = setup.create(&remote, "main", "dirty").await;
    let worktree = setup.worktree(remote.project, "dirty");

    std::fs::write(worktree.join("committed.txt"), "c").unwrap();
    git(&worktree, &["add", "committed.txt"]);
    git(&worktree, &["commit", "-q", "-m", "not pushed yet"]);
    std::fs::write(worktree.join("README.md"), "changed\n").unwrap();
    std::fs::write(worktree.join("new file.txt"), "new").unwrap();
    std::fs::remove_file(worktree.join("keep.txt")).unwrap();

    let err = setup.workspaces.archive(id, false).await.unwrap_err();
    let WorkspaceError::Dirty(dirty) = err else {
        panic!("{err}");
    };
    assert_eq!(dirty.uncommitted, ["README.md", "keep.txt", "new file.txt"]);
    assert_eq!(dirty.unpushed.len(), 1, "{:?}", dirty.unpushed);
    assert!(dirty.unpushed[0].ends_with(" not pushed yet"), "{dirty:?}");
    let text = dirty.to_string();
    for part in ["README.md", "keep.txt", "new file.txt", "not pushed yet"] {
        assert!(text.contains(part), "{text}");
    }

    assert!(worktree.join("new file.txt").exists());
    assert!(!setup.record(id).unwrap().deleted);
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
    let terminal = setup
        .terminals
        .start(
            id,
            &worktree,
            &["/bin/sh".into(), "-c".into(), "sleep 3127".into()],
            &[],
        )
        .await
        .unwrap();

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
    // The branch is free of its worktree, so a new one can be made from it.
    setup.create(&remote, "main", "gone again").await;
    let clone = setup.clone_of(remote.project);
    let worktrees = git(&clone, &["worktree", "list", "--porcelain"]);
    assert!(!worktrees.contains("/gone\n"), "{worktrees}");
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
