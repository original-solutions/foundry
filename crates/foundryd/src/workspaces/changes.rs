//! What a workspace has changed since it left its base: [`Workspaces::changes`].
//!
//! The changes are the worktree as it is on disk, committed or not, against the commit where its
//! branch left `origin/<base>`, plus each new file git does not ignore. A read changes nothing in
//! the worktree, its index or its refs. A new file is read only when it is a plain file, never
//! through a symlink, so a link the agent made cannot show a file outside the worktree.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use foundry_proto::ids::WorkspaceId;
use serde::Serialize;

use super::{Inner, WorkspaceError, Workspaces};

/// How long a whole read may take before it is stopped and fails, so a git that hangs cannot
/// hold the workspace's lifecycle lock.
const READ_WITHIN: Duration = Duration::from_secs(20);

/// The longest diff kept for one file, in bytes. A longer one is listed with its counts only.
pub(crate) const FILE_AT_MOST: usize = 256 * 1024;

/// The most diff text one read sends, in bytes. Files past it are listed with counts only.
const ALL_AT_MOST: usize = 4 * 1024 * 1024;

/// The most new files read for their text. Any more are listed by name only.
const NEW_READ_AT_MOST: usize = 500;

/// The most commits listed.
const COMMITS_AT_MOST: usize = 200;

/// A workspace's changes since its base.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Changes {
    /// The branch the workspace was made from.
    pub base: String,
    /// Commits on the worktree's `HEAD` that its base does not have, newest first, each as its
    /// short hash and subject.
    pub commits: Vec<String>,
    /// Each changed path, sorted by path.
    pub files: Vec<FileChange>,
}

/// One changed path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FileChange {
    pub path: String,
    /// The path it was renamed from.
    pub from: Option<String>,
    pub status: FileStatus,
    /// Lines added and removed, `None` for a binary file.
    pub added: Option<u64>,
    pub removed: Option<u64>,
    /// Whether some of the change is not committed yet.
    pub uncommitted: bool,
    /// The hunks, from the first `@@` line, or `None` when the file is binary, a symlink or too
    /// large to send.
    pub patch: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    /// New, and not added to git yet.
    Untracked,
}

impl Workspaces {
    /// What live workspace `id` has changed since it left its base, read under the workspace's
    /// lifecycle lock so an archive never removes the worktree mid-read.
    pub async fn changes(&self, id: WorkspaceId) -> Result<Changes, WorkspaceError> {
        let inner = self.inner.clone();
        self.locked(id, async move {
            let body = inner.live(id).await?;
            let worktree = inner.worktree(body.project, &body.branch);
            match tokio::time::timeout(READ_WITHIN, inner.changes(&worktree, &body.base)).await {
                Ok(changes) => changes,
                Err(_) => Err(WorkspaceError::Git {
                    command: "diff",
                    stderr: format!(
                        "reading the changes did not finish within {} s and was stopped",
                        READ_WITHIN.as_secs()
                    ),
                }),
            }
        })
        .await
    }
}

impl Inner {
    async fn changes(&self, worktree: &Path, base: &str) -> Result<Changes, WorkspaceError> {
        // Where the branch left its base; with the base gone from the remote, the last commit.
        let from = match self
            .git(
                worktree,
                "merge-base",
                ["HEAD", &format!("refs/remotes/origin/{base}")],
            )
            .await
        {
            Ok(out) => out.trim().to_owned(),
            Err(_) => "HEAD".to_owned(),
        };
        let commits = self
            .git(
                worktree,
                "log",
                [
                    format!("--max-count={COMMITS_AT_MOST}"),
                    "--format=%h %s".to_owned(),
                    format!("{from}..HEAD"),
                    "--".to_owned(),
                ],
            )
            .await?
            .lines()
            .map(str::to_owned)
            .collect();
        let diff = ["--no-color", "--no-ext-diff", "--no-textconv", "-M"];
        let names = self
            .git(
                worktree,
                "diff",
                diff.iter()
                    .copied()
                    .chain(["--name-status", "-z", from.as_str(), "--"]),
            )
            .await?;
        let counts = self
            .git(
                worktree,
                "diff",
                diff.iter()
                    .copied()
                    .chain(["--numstat", "-z", from.as_str(), "--"]),
            )
            .await?;
        let patches = self
            .git(
                worktree,
                "diff",
                diff.iter().copied().chain([from.as_str(), "--"]),
            )
            .await?;
        let uncommitted: BTreeSet<String> = self.uncommitted(worktree).await?.into_iter().collect();

        let mut files = Vec::new();
        let mut left = ALL_AT_MOST;
        let counts = numstat(&counts);
        let patches = split_patches(&patches);
        for (i, (status, path, old)) in name_status(&names).into_iter().enumerate() {
            let (added, removed) = counts.get(i).copied().unwrap_or((None, None));
            let patch = patches
                .get(i)
                .and_then(|p| hunks(p))
                .filter(|p| p.len() <= FILE_AT_MOST && p.len() <= left);
            left -= patch.as_ref().map_or(0, String::len);
            files.push(FileChange {
                uncommitted: uncommitted.contains(&path)
                    || old.as_ref().is_some_and(|f| uncommitted.contains(f)),
                path,
                from: old,
                status,
                added,
                removed,
                patch,
            });
        }

        let new = self
            .git(
                worktree,
                "ls-files",
                ["--others", "--exclude-standard", "-z", "--"],
            )
            .await?;
        for (n, path) in new.split('\0').filter(|p| !p.is_empty()).enumerate() {
            let read = if n < NEW_READ_AT_MOST {
                read_new(&worktree.join(path)).await
            } else {
                New::Unread
            };
            let (added, patch) = match read {
                New::Text(text) => {
                    let (lines, patch) = as_added(&text);
                    let patch =
                        (patch.len() <= FILE_AT_MOST && patch.len() <= left).then_some(patch);
                    left -= patch.as_ref().map_or(0, String::len);
                    (Some(lines), patch)
                }
                New::Binary => (None, None),
                New::Unread => (None, None),
            };
            files.push(FileChange {
                path: path.to_owned(),
                from: None,
                status: FileStatus::Untracked,
                added,
                removed: added.map(|_| 0),
                uncommitted: true,
                patch,
            });
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(Changes {
            base: base.to_owned(),
            commits,
            files,
        })
    }
}

enum New {
    Text(String),
    Binary,
    /// A symlink, a folder, one past the count read, too large, or gone.
    Unread,
}

/// A new file's text, read only when it is a plain file no larger than [`FILE_AT_MOST`].
async fn read_new(path: &Path) -> New {
    match tokio::fs::symlink_metadata(path).await {
        Ok(meta) if meta.is_file() && meta.len() <= FILE_AT_MOST as u64 => {}
        _ => return New::Unread,
    }
    match tokio::fs::read(path).await {
        Ok(bytes) if bytes.contains(&0) => New::Binary,
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(text) => New::Text(text),
            Err(_) => New::Binary,
        },
        Err(_) => New::Unread,
    }
}

/// A new file's text as one hunk of added lines, and its line count.
fn as_added(text: &str) -> (u64, String) {
    let lines: Vec<&str> = text.lines().collect();
    let mut patch = format!("@@ -0,0 +1,{} @@\n", lines.len());
    for line in &lines {
        patch.push('+');
        patch.push_str(line);
        patch.push('\n');
    }
    if !text.is_empty() && !text.ends_with('\n') {
        patch.push_str("\\ No newline at end of file\n");
    }
    (lines.len() as u64, patch)
}

/// `git diff --name-status -z`: each path's status, path and the path it came from.
fn name_status(out: &str) -> Vec<(FileStatus, String, Option<String>)> {
    let mut parts = out.split('\0').filter(|p| !p.is_empty());
    let mut files = Vec::new();
    while let Some(code) = parts.next() {
        let status = match code.as_bytes().first() {
            Some(b'A') => FileStatus::Added,
            Some(b'D') => FileStatus::Deleted,
            Some(b'R') | Some(b'C') => FileStatus::Renamed,
            _ => FileStatus::Modified,
        };
        if code.starts_with(['R', 'C']) {
            let (Some(old), Some(new)) = (parts.next(), parts.next()) else {
                break;
            };
            files.push((status, new.to_owned(), Some(old.to_owned())));
        } else {
            let Some(path) = parts.next() else { break };
            files.push((status, path.to_owned(), None));
        }
    }
    files
}

/// `git diff --numstat -z`: each path's lines added and removed, `None` for a binary file.
fn numstat(out: &str) -> Vec<(Option<u64>, Option<u64>)> {
    let mut parts = out.split('\0');
    let mut counts = Vec::new();
    while let Some(entry) = parts.next() {
        if entry.is_empty() {
            continue;
        }
        let mut fields = entry.splitn(3, '\t');
        let added = fields.next().and_then(|n| n.parse().ok());
        let removed = fields.next().and_then(|n| n.parse().ok());
        // A rename leaves the path field empty and gives both paths after it.
        if fields.next() == Some("") {
            parts.next();
            parts.next();
        }
        counts.push((added, removed));
    }
    counts
}

/// A whole `git diff` cut into one piece per file. Inside a hunk every line starts with ` `,
/// `+`, `-` or `\`, so a line starting `diff --git ` always begins a file.
fn split_patches(out: &str) -> Vec<&str> {
    let mut starts: Vec<usize> = Vec::new();
    let mut at = 0;
    for line in out.split_inclusive('\n') {
        if line.starts_with("diff --git ") {
            starts.push(at);
        }
        at += line.len();
    }
    starts
        .iter()
        .enumerate()
        .map(|(i, &start)| &out[start..starts.get(i + 1).copied().unwrap_or(out.len())])
        .collect()
}

/// A file's piece of a diff from its first `@@` line, or `None` when it has no hunks, as a
/// binary file or a change of mode alone has.
fn hunks(piece: &str) -> Option<String> {
    let at = if piece.starts_with("@@") {
        0
    } else {
        piece.find("\n@@")? + 1
    };
    Some(piece[at..].to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_status_reads_renames_with_both_paths() {
        let out = "M\0a.txt\0R087\0old name.txt\0new name.txt\0D\0gone\0A\0new\0";
        assert_eq!(
            name_status(out),
            [
                (FileStatus::Modified, "a.txt".into(), None),
                (
                    FileStatus::Renamed,
                    "new name.txt".into(),
                    Some("old name.txt".into())
                ),
                (FileStatus::Deleted, "gone".into(), None),
                (FileStatus::Added, "new".into(), None),
            ]
        );
    }

    #[test]
    fn numstat_reads_binary_files_and_renames() {
        let out = "3\t1\ta.txt\0-\t-\tlogo.png\0\x30\t2\t\0old\0new\0";
        assert_eq!(
            numstat(out),
            [(Some(3), Some(1)), (None, None), (Some(0), Some(2))]
        );
    }

    #[test]
    fn a_diff_is_cut_per_file_and_only_its_hunks_are_kept() {
        let out = "diff --git a/a b/a\nindex 1..2 100644\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-x\n+diff --git not a header\ndiff --git a/b b/b\nBinary files a/b and b/b differ\n";
        let pieces = split_patches(out);
        assert_eq!(pieces.len(), 2);
        assert_eq!(
            hunks(pieces[0]).as_deref(),
            Some("@@ -1 +1 @@\n-x\n+diff --git not a header\n")
        );
        assert_eq!(hunks(pieces[1]), None);
    }

    #[test]
    fn a_new_file_is_one_hunk_of_added_lines() {
        assert_eq!(as_added("a\nb\n"), (2, "@@ -0,0 +1,2 @@\n+a\n+b\n".into()));
        assert_eq!(
            as_added("a"),
            (
                1,
                "@@ -0,0 +1,1 @@\n+a\n\\ No newline at end of file\n".into()
            )
        );
    }
}
