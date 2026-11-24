//! What a workspace has changed since it left its base: [`Workspaces::changes`].
//!
//! The changes are the worktree as it is on disk, committed or not, against the commit where its
//! branch left `origin/<base>`, or the commit a commit checkout was made at, plus each new file
//! git does not ignore. A read changes nothing in
//! the worktree, its index or its refs, and takes no lock. It works on its own copy of the index,
//! so it never takes git's index lock, which an agent's own git command in the worktree needs.
//! It does not take the workspace's lifecycle lock, so it never holds up a start or an archive.
//! Git removes a worktree file by file, so a read beside an archive would see its files as
//! deleted, then no repo at all. A read that overlaps the removal, or ends after the archive, is
//! refused as the workspace being gone, never sent.
//! A new file is read only when it is a plain file, never through a symlink, so a link the agent
//! made cannot show a file outside the worktree.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use foundry_proto::ids::WorkspaceId;
use serde::Serialize;

use super::{Inner, WorkspaceError, Workspaces};

/// The longest diff kept for one file, in bytes. A longer one is listed with its counts only.
pub(crate) const FILE_AT_MOST: usize = 256 * 1024;

/// The most diff text one read sends, in bytes. Files past it are listed with counts only.
pub(crate) const ALL_AT_MOST: usize = 4 * 1024 * 1024;

/// The most new files read for their text. Any more are listed by name only.
pub(crate) const NEW_READ_AT_MOST: usize = 500;

/// The most commits listed.
const COMMITS_AT_MOST: usize = 200;

/// A workspace's changes since its base.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Changes {
    /// The branch the workspace was made from.
    pub base: String,
    /// Commits on the worktree's `HEAD` that its base does not have, newest first.
    pub commits: Vec<Commit>,
    /// Each changed path, sorted by path.
    pub files: Vec<FileChange>,
}

/// One commit, by its short hash and its subject.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Commit {
    pub hash: String,
    pub subject: String,
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
    /// The hunks, from the first `@@` line, or `None` when the file is binary, a symlink, changed
    /// type or is too large to send.
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
    /// What live workspace `id` has changed since it left its base. Each git call is bounded
    /// as every git call here is. A read an archive's removal of the worktree overlaps, and one
    /// that fails with the worktree gone, is [`WorkspaceError::Unknown`]: there is nothing here
    /// to read.
    pub async fn changes(&self, id: WorkspaceId) -> Result<Changes, WorkspaceError> {
        let (worktree, base) = self.reading(id).await?;
        let read = self.inner.changes(&worktree, &base).await;
        self.read(id, &worktree, read).await
    }

    /// What one commit on live workspace `id`'s branch changed, against its first parent: the
    /// commit itself and its files, none of them uncommitted. `hash` must be the short hash of
    /// one of [`Changes::commits`]; anything else, the base's own commits among them, is
    /// [`WorkspaceError::UnknownCommit`], and it never reaches git as an argument.
    pub async fn commit(&self, id: WorkspaceId, hash: &str) -> Result<Changes, WorkspaceError> {
        let (worktree, base) = self.reading(id).await?;
        let read = self.inner.commit(&worktree, &base, hash).await;
        self.read(id, &worktree, read).await
    }

    /// Live workspace `id`'s worktree and base, to read.
    async fn reading(&self, id: WorkspaceId) -> Result<(PathBuf, String), WorkspaceError> {
        let body = self.inner.live(id).await?;
        Ok((self.inner.folder(&body).await, body.base))
    }

    /// `read` of workspace `id`, or [`WorkspaceError::Unknown`] when an archive's removal of the
    /// worktree overlapped it. Checked after the read: an archive marks the worktree before it
    /// removes any of it and writes its record before it unmarks, so a removal that overlapped
    /// the read is seen as one or the other.
    pub(super) async fn read<T>(
        &self,
        id: WorkspaceId,
        worktree: &Path,
        read: Result<T, WorkspaceError>,
    ) -> Result<T, WorkspaceError> {
        if self.inner.removing(id)
            || self.inner.live(id).await.is_err()
            || (read.is_err() && !tokio::fs::try_exists(worktree).await.unwrap_or(true))
        {
            return Err(WorkspaceError::Unknown(id));
        }
        read
    }
}

impl Inner {
    /// Where the branch left its base; with the base gone from the remote, the last commit. A
    /// commit checkout's base is the commit it was made at, which no remote branch is named.
    async fn left_base(&self, worktree: &Path, base: &str) -> String {
        let mut from = vec![format!("refs/remotes/origin/{base}")];
        if super::is_commit_id(base) {
            from.push(format!("{base}^{{commit}}"));
        }
        for from in from {
            if let Ok(out) = self.git(worktree, "merge-base", ["HEAD", &from]).await {
                return out.trim().to_owned();
            }
        }
        "HEAD".to_owned()
    }

    /// The commits on `HEAD` since `from`, newest first.
    async fn commits(&self, worktree: &Path, from: &str) -> Result<Vec<Commit>, WorkspaceError> {
        Ok(self
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
            .map(|line| {
                let (hash, subject) = line.split_once(' ').unwrap_or((line, ""));
                Commit {
                    hash: hash.to_owned(),
                    subject: subject.to_owned(),
                }
            })
            .collect())
    }

    async fn changes(&self, worktree: &Path, base: &str) -> Result<Changes, WorkspaceError> {
        let from = self.left_base(worktree, base).await;
        let commits = self.commits(worktree, &from).await?;
        // Git refreshes the index as it compares the worktree with it, and a `diff` writes what
        // it refreshed under the index's lock whatever `GIT_OPTIONAL_LOCKS` says. So the read
        // works on its own copy, and an agent's git command in the worktree never finds the
        // lock taken.
        let copy = IndexCopy::of(self, worktree).await?;
        let index = Some(copy.0.as_path());
        let uncommitted: BTreeSet<String> = self
            .uncommitted(worktree, index)
            .await?
            .into_iter()
            .collect();
        let mut left = ALL_AT_MOST;
        let mut files = self
            .diffed(worktree, index, &[&from], &uncommitted, &mut left)
            .await?;

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

    /// What `git diff <range> --` lists, each file with its counts and hunks while they fit in
    /// `left` bytes; a path in `uncommitted`, or renamed from one, is marked so.
    async fn diffed(
        &self,
        worktree: &Path,
        index: Option<&Path>,
        range: &[&str],
        uncommitted: &BTreeSet<String>,
        left: &mut usize,
    ) -> Result<Vec<FileChange>, WorkspaceError> {
        let diff = ["--no-color", "--no-ext-diff", "--no-textconv", "-M"];
        let run = |how: &'static [&'static str]| {
            self.git_on(
                worktree,
                index,
                "diff",
                diff.iter().chain(how).chain(range).copied().chain(["--"]),
            )
        };
        let names = run(&["--name-status", "-z"]).await?;
        let counts = numstat(&run(&["--numstat", "-z"]).await?);
        let patches = run(&[]).await?;
        let mut files = Vec::new();
        let patches = split_patches(&patches);
        // `--name-status` and `--numstat` list each path once; the patch has two pieces for a
        // path whose type changed, so its pieces are walked on their own.
        let mut piece = 0;
        for (i, (status, path, old, typechange)) in name_status(&names).into_iter().enumerate() {
            let (added, removed) = counts.get(i).copied().unwrap_or((None, None));
            let patch = if typechange {
                piece += 2;
                None
            } else {
                piece += 1;
                patches.get(piece - 1).and_then(|p| hunks(p))
            }
            .filter(|p| p.len() <= FILE_AT_MOST && p.len() <= *left);
            *left -= patch.as_ref().map_or(0, String::len);
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

        Ok(files)
    }

    async fn commit(
        &self,
        worktree: &Path,
        base: &str,
        hash: &str,
    ) -> Result<Changes, WorkspaceError> {
        let from = self.left_base(worktree, base).await;
        let Some(commit) = self
            .commits(worktree, &from)
            .await?
            .into_iter()
            .find(|c| !hash.is_empty() && c.hash == hash)
        else {
            return Err(WorkspaceError::UnknownCommit(hash.to_owned()));
        };
        // Two commits' trees: neither the worktree nor its index is read.
        let parent = format!("{}^", commit.hash);
        let mut left = ALL_AT_MOST;
        let files = self
            .diffed(
                worktree,
                None,
                &[&parent, &commit.hash],
                &BTreeSet::new(),
                &mut left,
            )
            .await?;
        Ok(Changes {
            base: base.to_owned(),
            commits: vec![commit],
            files,
        })
    }
}

/// A copy of a worktree's index, beside it, for one read; removed when dropped.
struct IndexCopy(PathBuf);

impl IndexCopy {
    async fn of(inner: &Inner, worktree: &Path) -> Result<IndexCopy, WorkspaceError> {
        let index = inner
            .git(worktree, "rev-parse", ["--git-path", "index"])
            .await?;
        let index = worktree.join(index.trim_end_matches('\n'));
        let copy = index.with_file_name(format!("index.foundry-read-{}", uuid::Uuid::new_v4()));
        tokio::fs::copy(&index, &copy).await?;
        Ok(IndexCopy(copy))
    }
}

impl Drop for IndexCopy {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
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

/// `git diff --name-status -z`: each path's status, path, the path it came from, and whether its
/// type changed (a link made a file, or the other way), which git shows as `M`'s status here
/// and as a delete and an add in a patch.
fn name_status(out: &str) -> Vec<(FileStatus, String, Option<String>, bool)> {
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
            files.push((status, new.to_owned(), Some(old.to_owned()), false));
        } else {
            let Some(path) = parts.next() else { break };
            files.push((status, path.to_owned(), None, code.starts_with('T')));
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
        let out = "M\0a.txt\0R087\0old name.txt\0new name.txt\0D\0gone\0A\0new\0T\0link\0";
        assert_eq!(
            name_status(out),
            [
                (FileStatus::Modified, "a.txt".into(), None, false),
                (
                    FileStatus::Renamed,
                    "new name.txt".into(),
                    Some("old name.txt".into()),
                    false
                ),
                (FileStatus::Deleted, "gone".into(), None, false),
                (FileStatus::Added, "new".into(), None, false),
                (FileStatus::Modified, "link".into(), None, true),
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
