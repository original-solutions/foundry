//! The factory start of a session, messages to it, and ending factory sessions.
//!
//! A factory session is a P1 session started with factory options ([`Inner::start`] does both):
//! Claude Code runs with `--dangerously-skip-permissions` and an `--effort`, its settings file
//! holds `skipDangerousModePermissionPrompt`, and its environment holds the project login's
//! token as `GH_TOKEN` and as the answer of a `GIT_ASKPASS`, read from the secret store at the
//! start. The token passes through the terminal's private, short-lived env file, as the
//! gateway key does; it is in no argument, log line, error or other file.
//!
//! A file `factory` in the session's own folder, `<data dir>/sessions/<session>/`, marks the
//! session as a factory session and holds its project. [`Sessions::adopt`] reads the marks
//! again, so a daemon that starts again still knows its factory sessions.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use foundry_proto::ids::{ProjectId, SessionId, WorkspaceId};

use super::{Inner, SessionError, Sessions, blocking, claude, lock};
use crate::factory::messages::Message;
use crate::git_auth::{self, GitAuth};
use crate::terminals::TerminalError;

/// The name of the mark file in a factory session's folder.
const MARK: &str = "factory";

/// What a factory session is started for. It holds no secret: the token is read from the
/// secret store at the start.
#[derive(Clone, Debug)]
pub struct FactoryStart {
    /// The project the session works for.
    pub project: ProjectId,
    /// The GitHub login whose token the session acts with.
    pub login: String,
    /// Claude Code's model.
    pub model: String,
    /// Claude Code's `--effort`.
    pub effort: String,
}

/// The factory sessions, live or ended, and the project each is for, and how many times each
/// workspace's factory sessions were ended.
#[derive(Default)]
pub(super) struct FactoryState {
    /// A session is in it from the moment it is live in its start, is taken out if the start
    /// fails, or was found by adopt.
    pub marks: HashMap<SessionId, ProjectId>,
    /// Per workspace, the number of `end_factory` calls so far.
    pub ended: HashMap<WorkspaceId, u64>,
}

/// What [`Inner::start`] adds for a factory session. It holds the token, so it has no `Debug`.
pub(super) struct FactoryLaunch {
    pub project: ProjectId,
    pub effort: String,
    /// The child's extra environment: the token and the askpass.
    pub env: Vec<(String, String)>,
    /// The workspace's end generation when the start began: an `end_factory` since then means
    /// this start must not go on.
    pub generation: u64,
}

/// Session `id`'s own folder, in the sessions folder `dir`.
pub(super) fn folder(dir: &Path, id: SessionId) -> PathBuf {
    dir.join(id.to_string())
}

/// The mark file of session `id`.
pub(super) fn mark_file(dir: &Path, id: SessionId) -> PathBuf {
    folder(dir, id).join(MARK)
}

/// Writes the mark at `path`, which makes the session's folder, readable by this user only.
pub(super) fn write_mark(path: &Path, project: ProjectId) -> Result<(), SessionError> {
    claude::write_new_private(path, project.to_string().as_bytes())
}

/// The environment a factory session's child gets for `login`: the token for `gh` and as the
/// askpass's answer, the askpass script, and git's credential helpers emptied. The askpass and
/// the token are read from what [`GitAuth`] gives a git child, so there is one source of them.
///
/// The session's git keeps the machine owner's config: system and global, so the owner's
/// identity, global ignore file and hooks work as in a P1 session. It is hardened only as
/// the spec's check 3 says: credential helpers emptied (`GIT_CONFIG_COUNT` pins one empty
/// `credential.helper`, which resets the list), `GIT_ALLOW_PROTOCOL=https` read from
/// [`GitAuth`] so a repository's config cannot reach ssh, an inherited `GIT_CONFIG_PARAMETERS`
/// (read after `GIT_CONFIG_COUNT`) emptied, and the other GitHub tokens that `gh` and git's
/// helpers read blanked, the ones [`git_auth::helpers_off_env`] blanks.
///
/// What the askpass cannot cover is a setup of the owner's that answers github.com before git
/// asks it: a URL rewrite, an `extraHeader`, a user name in the `origin` remote's URL or push URL, a netrc entry. A
/// factory start refuses those, before it reads the token ([`git_refusal`]).
fn factory_env(
    secrets: &dyn crate::secrets::Secrets,
    login: &str,
    data: &Path,
) -> Result<Vec<(String, String)>, SessionError> {
    let auth = GitAuth::for_login(secrets, login, data)?;
    let mut git = Command::new("git");
    auth.apply(&mut git);
    // `git` holds the token: it is read from, and never formatted.
    let get = |name: &str| {
        git.get_envs()
            .find(|(key, _)| *key == name)
            .and_then(|(_, value)| value)
            .map(|value| value.to_string_lossy().into_owned())
    };
    let (Some(askpass), Some(token), Some(protocol)) = (
        get("GIT_ASKPASS"),
        get(git_auth::TOKEN_VAR),
        get("GIT_ALLOW_PROTOCOL"),
    ) else {
        return Err(SessionError::NoAskpass);
    };
    let mut env = vec![
        ("GH_TOKEN".to_owned(), token.clone()),
        ("GIT_ASKPASS".into(), askpass),
        (git_auth::TOKEN_VAR.into(), token),
        ("GIT_CONFIG_COUNT".into(), "1".into()),
        ("GIT_CONFIG_KEY_0".into(), "credential.helper".into()),
        ("GIT_CONFIG_VALUE_0".into(), String::new()),
        ("GIT_CONFIG_PARAMETERS".into(), String::new()),
        ("GIT_ALLOW_PROTOCOL".into(), protocol),
    ];
    env.extend(
        git_auth::helpers_off_env()
            .into_iter()
            .filter(|(key, value)| {
                key.ends_with("_TOKEN") && key != "GH_TOKEN" && value.is_empty()
            }),
    );
    Ok(env)
}

/// The netrc of the owner whose HOME is `home`: `<home>/.netrc`. None without a HOME.
fn netrc_of(home: Option<std::ffi::OsString>) -> Option<PathBuf> {
    home.filter(|home| !home.is_empty())
        .map(|home| PathBuf::from(home).join(".netrc"))
}

/// The check's git: `base` with what the session overrides out of it, so it lists the config
/// the session's git reads: `GIT_CONFIG_PARAMETERS` and `GIT_CONFIG_COUNT` (the session sets
/// its own) and `GIT_CONFIG` (`git config` reads only that file when it is set, and `git push`
/// ignores it). With `home` (tests), that folder is HOME and XDG_CONFIG_HOME and there is no
/// system config.
fn check_git(mut git: Command, home: Option<&Path>) -> Command {
    git.env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG");
    if let Some(home) = home {
        git.env("HOME", home)
            .env("XDG_CONFIG_HOME", home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_CONFIG_GLOBAL")
            .env_remove("GIT_CONFIG_SYSTEM");
    }
    git
}

/// [`git_refusal`] for the real setup: the process's `git` as [`check_git`] makes it, and
/// `$HOME/.netrc`. With `home` (tests), that folder stands for HOME.
fn owner_setup_refusal(home: Option<&Path>, worktree: &Path) -> Result<(), SessionError> {
    let git = || check_git(Command::new("git"), home);
    let netrc = netrc_of(
        home.map(|home| home.as_os_str().to_owned())
            .or_else(|| std::env::var_os("HOME")),
    );
    git_refusal(git, netrc.as_deref(), worktree)
}

/// One thing in the owner's git setup that would answer github.com before the askpass: what it
/// is, the config key (or netrc entry) it is in, and the file. No value: a header, a URL's
/// password and a netrc password are secrets.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Finding {
    what: &'static str,
    key: String,
    file: String,
}

/// The findings that refuse a factory start. Names only, never a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitRefused(Vec<Finding>);

impl std::fmt::Display for GitRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "sessions: a factory start is refused: the owner's git setup would answer github.com \
             before the askpass is asked: "
        )?;
        for (n, found) in self.0.iter().enumerate() {
            let sep = if n == 0 { "" } else { "; " };
            write!(f, "{sep}{} ({} in {})", found.what, found.key, found.file)?;
        }
        Ok(())
    }
}

impl std::error::Error for GitRefused {}

const GITHUB: &str = "https://github.com/";

/// Runs `git` with `args`, giving its stdout when it succeeds.
fn git_out(mut git: Command, args: &[&OsStr]) -> Option<Vec<u8>> {
    let out = git
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status.success().then_some(out.stdout)
}

/// Refuses the start when the owner's git setup would answer github.com before the askpass is
/// asked: in the git config in effect in `worktree` (system, global, repo and included files),
/// a `url.<base>.insteadOf` or `pushInsteadOf` that rewrites `https://github.com/`, an
/// `http.extraHeader` that covers it, a `origin` URL or push URL that carries a user name; and
/// in `netrc`, a `machine github.com` or `default` entry. `git` gives a git command with the
/// session's overrides of config (`GIT_CONFIG_PARAMETERS`) not in it. Git that cannot read the
/// config refuses too. The message names each finding and where, never a value.
fn git_refusal(
    git: impl Fn() -> Command,
    netrc: Option<&Path>,
    worktree: &Path,
) -> Result<(), SessionError> {
    let mut found = Vec::new();
    let listed = git_out(
        git(),
        &[
            OsStr::new("-C"),
            worktree.as_os_str(),
            OsStr::new("config"),
            OsStr::new("--list"),
            OsStr::new("--show-origin"),
            OsStr::new("-z"),
        ],
    );
    let Some(listed) = listed else {
        found.push(Finding {
            what: "the git config could not be read",
            key: "git config --list".into(),
            file: worktree.display().to_string(),
        });
        return Err(GitRefused(found).into());
    };
    let entries = config_entries(&listed);
    for entry in &entries {
        if let Some(what) = config_finding(&entry.key, &entry.value) {
            found.push(Finding {
                what,
                key: mask_user_info(&entry.key),
                file: entry.file.clone(),
            });
        }
    }
    found.extend(origin_findings(&git, &entries, worktree));
    if let Some(netrc) = netrc {
        found.extend(netrc_findings(netrc));
    }
    if found.is_empty() {
        Ok(())
    } else {
        Err(GitRefused(found).into())
    }
}

/// The findings for a user name in the `origin` remote's URL or push URL, named where it is:
/// the `remote.origin.url` or `pushurl` entry that has one, or the `url.<base>.insteadof` whose
/// base has one and whose value starts a raw origin URL. The URL git then uses (rewrites
/// applied) is a safety net: a user name there that none of those explain is one generic
/// finding.
fn origin_findings(git: &impl Fn() -> Command, entries: &[Entry], worktree: &Path) -> Vec<Finding> {
    let mut found = Vec::new();
    let raw: Vec<&Entry> = entries
        .iter()
        .filter(|e| e.key == "remote.origin.url" || e.key == "remote.origin.pushurl")
        .collect();
    for entry in &raw {
        if has_user_name(&entry.value) {
            found.push(Finding {
                what: "a user name in the origin URL",
                key: entry.key.clone(),
                file: entry.file.clone(),
            });
        }
    }
    for entry in entries {
        let base = entry.key.strip_prefix("url.").and_then(|rest| {
            rest.strip_suffix(".insteadof")
                .or_else(|| rest.strip_suffix(".pushinsteadof"))
        });
        let Some(base) = base else { continue };
        if has_user_name(base)
            && !entry.value.is_empty()
            && raw.iter().any(|url| url.value.starts_with(&entry.value))
        {
            found.push(Finding {
                what: "a user name in the origin URL after a URL rewrite",
                key: mask_user_info(&entry.key),
                file: entry.file.clone(),
            });
        }
    }
    if found.is_empty() {
        for flag in [None, Some("--push")] {
            let mut args: Vec<&OsStr> = vec![
                OsStr::new("-C"),
                worktree.as_os_str(),
                OsStr::new("remote"),
                OsStr::new("get-url"),
            ];
            args.extend(flag.map(OsStr::new));
            args.extend([OsStr::new("--all"), OsStr::new("origin")]);
            let urls = git_out(git(), &args).unwrap_or_default();
            if String::from_utf8_lossy(&urls).lines().any(has_user_name) {
                found.push(Finding {
                    what: "a user name in the origin URL after a URL rewrite",
                    key: "remote.origin.url".into(),
                    file: "the git config".into(),
                });
                break;
            }
        }
    }
    found
}

/// One entry of `git config --list --show-origin -z`.
struct Entry {
    file: String,
    key: String,
    value: String,
}

/// The entries of `git config --list --show-origin -z`: `<origin>\0<key>\n<value>\0` each, and
/// just `<key>` for a key with no value.
fn config_entries(listed: &[u8]) -> Vec<Entry> {
    let text = String::from_utf8_lossy(listed);
    let mut parts = text.split('\0');
    let mut entries = Vec::new();
    while let (Some(origin), Some(entry)) = (parts.next(), parts.next()) {
        let (key, value) = entry.split_once('\n').unwrap_or((entry, ""));
        entries.push(Entry {
            file: origin.strip_prefix("file:").unwrap_or(origin).to_owned(),
            key: key.to_owned(),
            value: value.to_owned(),
        });
    }
    entries
}

/// What `key = value` of the config does that is refused, if it does. `key` has its section
/// and variable in lower case, as git lists it.
fn config_finding(key: &str, value: &str) -> Option<&'static str> {
    let url_key = |suffix: &str| {
        key.strip_prefix("url.")
            .is_some_and(|rest| rest.ends_with(suffix))
    };
    if url_key(".insteadof") || url_key(".pushinsteadof") {
        // An empty value is a prefix of every URL: it rewrites them all.
        return (GITHUB.starts_with(value) || value.starts_with(GITHUB))
            .then_some("a URL rewrite of https://github.com/");
    }
    if value.is_empty() {
        return None;
    }
    if key == "http.extraheader" {
        return Some("an extra HTTP header");
    }
    let url = key.strip_prefix("http.")?.strip_suffix(".extraheader")?;
    covers_github(url).then_some("an extra HTTP header for github.com")
}

/// Whether the `http.<url>.` section's `url` covers `https://github.com/`: https, the host
/// `github.com` or a pattern whose `*` stands for a whole label, no port or 443, any user and
/// path.
fn covers_github(url: &str) -> bool {
    let Some((scheme, rest)) = url.split_once("://") else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("https") {
        return false;
    }
    let authority = rest.split('/').next().unwrap_or("");
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let (host, port) = match host_port.rsplit_once(':') {
        Some((host, port)) if !host.ends_with(']') || host.starts_with('[') => (host, Some(port)),
        _ => (host_port, None),
    };
    if port.is_some_and(|port| port != "443") {
        return false;
    }
    let labels: Vec<&str> = host.split('.').collect();
    let github = ["github", "com"];
    labels.len() == github.len()
        && labels
            .iter()
            .zip(github)
            .all(|(label, want)| *label == "*" || label.eq_ignore_ascii_case(want))
}

/// Whether the remote URL `url` carries a user name: `scheme://user@host/...` or `user@host:path`.
fn has_user_name(url: &str) -> bool {
    if let Some((_, rest)) = url.split_once("://") {
        return rest
            .split('/')
            .next()
            .is_some_and(|auth| auth.contains('@'));
    }
    match (url.find(':'), url.find('/')) {
        (Some(colon), slash) if slash.is_none_or(|slash| colon < slash) => {
            url[..colon].contains('@')
        }
        _ => false,
    }
}

/// `key` with the user info of a URL in it replaced, so a password in a config key is not
/// printed: everything between `://` and the last `@` after it.
fn mask_user_info(key: &str) -> String {
    let Some(start) = key.find("://").map(|at| at + 3) else {
        return key.to_owned();
    };
    match key[start..].rfind('@') {
        Some(at) => format!("{}***{}", &key[..start], &key[start + at..]),
        None => key.to_owned(),
    }
}

/// The tokens of a netrc line, as curl reads them: split at white space, a token in double
/// quotes may hold white space, and a backslash makes the next character itself.
fn netrc_tokens(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut chars = line.chars().peekable();
    while let Some(&first) = chars.peek() {
        if first.is_whitespace() {
            chars.next();
            continue;
        }
        let quoted = first == '"';
        if quoted {
            chars.next();
        }
        let mut token = String::new();
        while let Some(&c) = chars.peek() {
            if (quoted && c == '"') || (!quoted && c.is_whitespace()) {
                chars.next();
                break;
            }
            chars.next();
            if c == '\\' {
                if let Some(next) = chars.next() {
                    token.push(next);
                }
            } else {
                token.push(c);
            }
        }
        tokens.push(token);
    }
    tokens
}

/// The entries of the netrc at `path` that answer github.com: a `machine github.com` and a
/// `default`. A netrc that is not there has none; one that cannot be read is refused.
fn netrc_findings(path: &Path) -> Vec<Finding> {
    let file = path.display().to_string();
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(_) => {
            return vec![Finding {
                what: "a netrc that could not be read",
                key: "netrc".into(),
                file,
            }];
        }
    };
    let text = String::from_utf8_lossy(&bytes);
    let mut found = Vec::new();
    let (mut macdef, mut machine, mut skip) = (false, false, false);
    for line in text.lines() {
        if macdef {
            macdef = !line.trim().is_empty();
            continue;
        }
        for token in netrc_tokens(line) {
            if std::mem::take(&mut skip) {
                continue;
            }
            if std::mem::take(&mut machine) {
                if token.eq_ignore_ascii_case("github.com") {
                    found.push(Finding {
                        what: "a netrc entry for github.com",
                        key: "machine github.com".into(),
                        file: file.clone(),
                    });
                }
                continue;
            }
            let is = |word: &str| token.eq_ignore_ascii_case(word);
            if is("macdef") {
                macdef = true;
                break;
            } else if is("machine") {
                machine = true;
            } else if is("login") || is("password") || is("account") {
                skip = true;
            } else if is("default") {
                found.push(Finding {
                    what: "a netrc default entry",
                    key: "default".into(),
                    file: file.clone(),
                });
            }
        }
    }
    found
}

impl Sessions {
    /// Starts Claude Code for a factory session in a new terminal in `worktree`, the worktree
    /// of `workspace`, as [`Sessions::start`] does, with the flags, settings key, environment
    /// and mark of a factory session. `first`'s line is its first prompt, typed as
    /// [`Sessions::start_with`] types one.
    ///
    /// A start is refused with [`SessionError::GitSetup`] when the owner's git setup would
    /// answer github.com before the askpass is asked ([`git_refusal`]); it is checked here, at
    /// the start only. Then the token is read: a login the machine holds no token for is
    /// [`SessionError::NoToken`], and so is a machine with no secret store. A start that fails
    /// leaves no terminal, no file, no folder and no record.
    ///
    /// A first message with a body is refused with [`SessionError::FirstMessageBody`]: writing
    /// a body to a file in the session's folder, and typing the line with its path, is what
    /// the turns and messages unit adds, and it fills this seam.
    pub async fn start_factory(
        &self,
        workspace: WorkspaceId,
        worktree: &Path,
        start: FactoryStart,
        first: Message,
    ) -> Result<SessionId, SessionError> {
        if first.body.is_some() {
            return Err(SessionError::FirstMessageBody);
        }
        // Before anything is read or written: an `end_factory` from here on beats this start.
        let generation = lock(&self.inner.factory)
            .ended
            .get(&workspace)
            .copied()
            .unwrap_or(0);
        let inner = self.inner.clone();
        let worktree = worktree.to_path_buf();
        let prompt = super::first_prompt(&first.line);
        tokio::spawn(async move {
            let secrets = inner.secrets.get().cloned();
            let (login, data) = (start.login.clone(), inner.data.clone());
            let (home, checked) = (inner.git_home(), worktree.clone());
            let env = blocking(move || {
                // First, before the token is read or anything is written.
                owner_setup_refusal(home.as_deref(), &checked)?;
                match secrets {
                    Some(secrets) => factory_env(secrets.as_ref(), &login, &data),
                    None => Err(crate::git_auth::NoToken::Unset { login }.into()),
                }
            })
            .await??;
            let launch = FactoryLaunch {
                project: start.project,
                effort: start.effort,
                env,
                generation,
            };
            inner
                .start(
                    workspace,
                    &worktree,
                    &start.model,
                    prompt.as_deref(),
                    Some(&launch),
                )
                .await
        })
        .await?
    }

    /// The project session `session` is a factory session of, whether it still runs or has
    /// ended. `None` for a P1 session and for one this daemon knows nothing of.
    pub fn is_factory(&self, session: SessionId) -> Option<ProjectId> {
        lock(&self.inner.factory).marks.get(&session).copied()
    }

    /// The factory sessions of `project` that still run, sorted.
    pub fn factory_sessions(&self, project: ProjectId) -> Vec<SessionId> {
        let mut sessions: Vec<_> = lock(&self.inner.factory)
            .marks
            .iter()
            .filter(|(id, p)| **p == project && self.inner.live(**id).is_some())
            .map(|(id, _)| *id)
            .collect();
        sessions.sort();
        sessions
    }

    /// Closes the terminal of each factory session of `workspace` that still runs, and makes a
    /// start of one that has not yet gone live fail with [`SessionError::Ended`]. A P1 session
    /// and another workspace's factory session are left alone. A terminal already gone counts
    /// as ended. Every close is tried; the first other failure is given.
    pub async fn end_factory(&self, workspace: WorkspaceId) -> Result<(), SessionError> {
        // One lock for both: a start that has not yet marked its session sees the new
        // generation and gives up, and one that has is in the snapshot.
        let marked: Vec<SessionId> = {
            let mut state = lock(&self.inner.factory);
            *state.ended.entry(workspace).or_insert(0) += 1;
            state.marks.keys().copied().collect()
        };
        let mut terminals = Vec::new();
        for id in marked {
            let Some(live) = self.inner.live(id) else {
                continue;
            };
            let body = live.body.lock().await;
            // A start that failed while this waited is no longer live, and one that has not
            // yet got its terminal has none to close.
            if !self.inner.is_live(&live) || body.terminal.0.is_nil() {
                continue;
            }
            if body.workspace == workspace {
                terminals.push(body.terminal);
            }
        }
        let mut failed = None;
        for terminal in terminals {
            match self.inner.terminals.close(terminal).await {
                Ok(()) | Err(TerminalError::Gone(_) | TerminalError::Unknown(_)) => {}
                Err(error) => {
                    failed.get_or_insert(error);
                }
            }
        }
        failed.map_or(Ok(()), |error| Err(error.into()))
    }
}

impl Inner {
    /// Tests: the home the check of the owner's git setup reads. Never set outside tests.
    fn git_home(&self) -> Option<PathBuf> {
        #[cfg(test)]
        {
            self.git_home.get().cloned()
        }
        #[cfg(not(test))]
        {
            None
        }
    }

    /// Reads the marks of the factory sessions an earlier daemon started, so they are still
    /// factory sessions. A mark that cannot be read, or holds no project, is skipped with a
    /// warning that does not show its contents.
    pub(super) async fn adopt_marks(&self) {
        let dir = self.dir.clone();
        let found = blocking(move || {
            let mut found = Vec::new();
            let entries = match std::fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return found,
                Err(error) => {
                    tracing::warn!(%error, "the sessions folder cannot be read, so no factory mark is taken");
                    return found;
                }
            };
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        tracing::warn!(%error, "an entry of the sessions folder cannot be read");
                        continue;
                    }
                };
                let mark = entry.path().join(MARK);
                if !mark.is_file() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                let session = uuid::Uuid::parse_str(&name).map(SessionId);
                let project = std::fs::read_to_string(&mark)
                    .ok()
                    .and_then(|text| uuid::Uuid::parse_str(text.trim()).ok())
                    .map(ProjectId);
                match (session, project) {
                    (Ok(session), Some(project)) => found.push((session, project)),
                    _ => tracing::warn!(
                        folder = %name,
                        "a factory mark cannot be read, so the session is not taken as a factory session"
                    ),
                }
            }
            found
        })
        .await
        .unwrap_or_default();
        lock(&self.factory).marks.extend(found);
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use foundry_holder::protocol::Request;
    use foundry_proto::ids::MachineId;
    use foundry_proto::record::{AnyRecord, Record, Session};
    use foundry_proto::state::SessionState;
    use futures::StreamExt as _;
    use tempfile::TempDir;

    use super::*;
    use crate::secrets::{MemorySecrets, Secrets};
    use crate::sessions::{Config, Gateway};
    use crate::store::Store;
    use crate::terminals::{Output, Terminals};
    use crate::test_log::Logged;

    const WAIT: Duration = Duration::from_secs(20);
    const MODEL: &str = "test-model";
    const EFFORT: &str = "xhigh";
    const LOGIN: &str = "factory-bot";
    const TOKEN: &str = "ghp_madeUpFactoryToken1234567890abcdEFGH";
    const FIRST_LINE: &str = "/factory-build acme/widgets#7 /data/sessions/x/body.md";

    /// Stands between the daemon and the fake agent, whose own command line refuses the two
    /// factory flags: it prints each argument it got, has `GIT_ASKPASS` answer a password
    /// prompt, drops the two flags and runs the agent with the rest.
    const SHIM: &str = r#"agent=$1; shift
printf 'ARG=%s\n' "$@"
if [ -n "$GIT_ASKPASS" ]; then
  printf 'ASKPASS=%s\n' "$("$GIT_ASKPASS" "Password for 'https://x-access-token@github.com': ")"
fi
skip=0
for a do
  shift
  if [ $skip = 1 ]; then skip=0; continue; fi
  case $a in
    --dangerously-skip-permissions) continue ;;
    --effort) skip=1; continue ;;
  esac
  set -- "$@" "$a"
done
exec "$agent" "$@""#;

    fn built(name: &str) -> PathBuf {
        let exe = std::env::current_exe().unwrap();
        let path = exe.parent().and_then(Path::parent).unwrap().join(name);
        assert!(
            path.exists(),
            "{} is not built: run `cargo test --workspace`",
            path.display()
        );
        path
    }

    /// A daemon's worth of sessions: real holder, the fake agent behind [`SHIM`], a store
    /// holding a token for [`LOGIN`], and the hook endpoint on a loopback port.
    struct Fx {
        data: TempDir,
        worktree: TempDir,
        /// Holds Claude Code's state file.
        _home: TempDir,
        /// The home the factory start's check of the owner's git setup reads.
        git_home: TempDir,
        me: MachineId,
        store: Arc<Mutex<Store>>,
        terminals: Arc<Terminals>,
        config: Config,
        secrets: Arc<MemorySecrets>,
        sessions: Sessions,
        server: tokio::task::JoinHandle<()>,
    }

    impl Fx {
        async fn new(script: &str) -> Fx {
            let data = tempfile::Builder::new()
                .prefix("ff")
                .tempdir_in("/tmp")
                .unwrap();
            let script_path = data.path().join("script.yaml");
            std::fs::write(&script_path, script).unwrap();
            let me = MachineId::random();
            let store = Arc::new(Mutex::new(
                Store::open(data.path().join("foundry.db")).unwrap(),
            ));
            let terminals = Arc::new(Terminals::new(
                me,
                store.clone(),
                data.path(),
                built("foundry-holder"),
            ));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let home = tempfile::tempdir().unwrap();
            let config = Config {
                claude: [
                    "sh",
                    "-c",
                    SHIM,
                    "sh",
                    &built("foundry-fake-agent").to_string_lossy(),
                    "--script",
                    &script_path.to_string_lossy(),
                ]
                .map(str::to_owned)
                .to_vec(),
                claude_json: home.path().join(".claude.json"),
                hook_port: port,
                gateway: Gateway {
                    base_url: "http://127.0.0.1:9/gateway".into(),
                    key: format!("gw-key-{}", uuid::Uuid::new_v4()),
                },
            };
            let secrets = Arc::new(MemorySecrets::default());
            secrets.set(&format!("github:{LOGIN}"), TOKEN).unwrap();
            let git_home = tempfile::tempdir().unwrap();
            let sessions = Sessions::new(
                me,
                store.clone(),
                terminals.clone(),
                data.path(),
                config.clone(),
            )
            .with_git_home(git_home.path().to_path_buf())
            .with_secrets(secrets.clone());
            let router = sessions.router();
            let server = tokio::spawn(async move {
                axum::serve(
                    listener,
                    router.into_make_service_with_connect_info::<SocketAddr>(),
                )
                .await
                .unwrap();
            });
            Fx {
                data,
                worktree: tempfile::tempdir().unwrap(),
                _home: home,
                git_home,
                me,
                store,
                terminals,
                config,
                secrets,
                sessions,
                server,
            }
        }

        /// Sessions over the same store and terminals, with or without a secret store.
        fn again(&self, with_secrets: bool) -> Sessions {
            let sessions = Sessions::new(
                self.me,
                self.store.clone(),
                self.terminals.clone(),
                self.data.path(),
                self.config.clone(),
            )
            .with_git_home(self.git_home.path().to_path_buf());
            if with_secrets {
                sessions.with_secrets(self.secrets.clone())
            } else {
                sessions
            }
        }

        fn sessions_dir(&self) -> PathBuf {
            self.data.path().join("sessions")
        }

        fn settings_file(&self, id: SessionId) -> PathBuf {
            self.sessions_dir().join(format!("{id}.json"))
        }

        fn mark_file(&self, id: SessionId) -> PathBuf {
            self.sessions_dir().join(id.to_string()).join("factory")
        }

        fn start_for(&self, project: ProjectId) -> FactoryStart {
            FactoryStart {
                project,
                login: LOGIN.into(),
                model: MODEL.into(),
                effort: EFFORT.into(),
            }
        }

        fn line(&self) -> Message {
            Message {
                line: FIRST_LINE.into(),
                body: None,
            }
        }

        async fn start_factory(&self, workspace: WorkspaceId, project: ProjectId) -> SessionId {
            self.sessions
                .start_factory(
                    workspace,
                    self.worktree.path(),
                    self.start_for(project),
                    self.line(),
                )
                .await
                .unwrap()
        }

        fn state(&self, id: SessionId) -> Option<SessionState> {
            let store = self.store.lock().unwrap();
            store
                .since(self.me, 0)
                .unwrap()
                .into_iter()
                .filter_map(|record| match record {
                    AnyRecord::Session(r) if r.body.id == id => Some(r),
                    _ => None,
                })
                .next_back()
                .map(|r: Record<Session>| r.body.state)
        }

        fn session_records(&self) -> usize {
            let store = self.store.lock().unwrap();
            store
                .since(self.me, 0)
                .unwrap()
                .into_iter()
                .filter(|record| matches!(record, AnyRecord::Session(_)))
                .count()
        }

        fn terminal_of(&self, id: SessionId) -> foundry_proto::ids::TerminalId {
            let store = self.store.lock().unwrap();
            store
                .since(self.me, 0)
                .unwrap()
                .into_iter()
                .filter_map(|record| match record {
                    AnyRecord::Session(r) if r.body.id == id => Some(r.body.terminal),
                    _ => None,
                })
                .next_back()
                .unwrap()
        }

        /// What the session's terminal showed, read until `done` holds for it.
        async fn shown(&self, id: SessionId, done: impl Fn(&str) -> bool) -> String {
            let terminal = self.terminal_of(id);
            let (snapshot, mut output) = self.terminals.attach(terminal).await.unwrap();
            read_until(&snapshot.bytes, &mut output, done).await
        }

        /// That nothing of a refused start is left: no record, no file, nothing live.
        fn assert_nothing_left(&self, sessions: &Sessions, project: ProjectId) {
            assert_eq!(self.session_records(), 0);
            let left: Vec<_> = std::fs::read_dir(self.sessions_dir())
                .map(|entries| entries.flatten().map(|e| e.path()).collect())
                .unwrap_or_default();
            assert!(left.is_empty(), "left behind: {left:?}");
            assert!(crate::sessions::lock(&sessions.inner.live).is_empty());
            assert!(
                crate::sessions::lock(&sessions.inner.factory)
                    .marks
                    .is_empty()
            );
            assert!(sessions.factory_sessions(project).is_empty());
        }
    }

    impl Drop for Fx {
        fn drop(&mut self) {
            self.server.abort();
            let Ok(entries) = std::fs::read_dir(self.data.path().join("terminals")) else {
                return;
            };
            for entry in entries.flatten() {
                if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(entry.path()) {
                    use std::io::Write as _;
                    let _ = stream.write_all(&Request::Close.encode());
                }
            }
        }
    }

    async fn read_until(
        snapshot: &[u8],
        stream: &mut Output,
        done: impl Fn(&str) -> bool,
    ) -> String {
        let mut seen = snapshot.to_vec();
        let deadline = Instant::now() + WAIT;
        while !done(&String::from_utf8_lossy(&seen)) {
            let left = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(left, stream.next()).await {
                Ok(Some(Ok(bytes))) => seen.extend_from_slice(&bytes),
                other => panic!(
                    "no more output ({:?}); seen: {:?}",
                    other.map(|o| o.is_some()),
                    String::from_utf8_lossy(&seen)
                ),
            }
        }
        String::from_utf8_lossy(&seen).into_owned()
    }

    async fn until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + WAIT;
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// The `ARG=` lines of what the shim printed.
    fn args(text: &str) -> Vec<String> {
        text.lines()
            .filter_map(|l| l.trim_end().strip_prefix("ARG=").map(str::to_owned))
            .collect()
    }

    /// The value of the line that starts with `name=`, when it is complete.
    fn value(text: &str, name: &str) -> Option<String> {
        let printed = text.rfind('\n').map_or("", |end| &text[..end]);
        printed
            .lines()
            .find_map(|l| l.trim_end().strip_prefix(name).map(str::to_owned))
    }

    const ENV_SCRIPT: &str = r"
- print_env: GH_TOKEN
- print_env: GIT_CONFIG_COUNT
- print_env: GIT_CONFIG_KEY_0
- print_env: GIT_CONFIG_VALUE_0
- print_env: GITHUB_TOKEN
- print_env: GH_ENTERPRISE_TOKEN
- print_env: GITHUB_ENTERPRISE_TOKEN
- print_env: GIT_CONFIG_PARAMETERS
- print_env: GIT_ALLOW_PROTOCOL
- print_env: GIT_CONFIG_GLOBAL
- print_env: GIT_CONFIG_NOSYSTEM
- print_env: FOUNDRY_GIT_TOKEN
- wait_input
";

    #[tokio::test]
    async fn a_factory_start_gives_claude_the_flags_the_key_the_token_and_an_askpass_and_logs_no_token()
     {
        let _ = Logged::everywhere();
        let f = Fx::new(ENV_SCRIPT).await;
        let project = ProjectId::random();

        let id = f.start_factory(WorkspaceId::random(), project).await;

        let seen = f
            .shown(id, |t| value(t, "FOUNDRY_GIT_TOKEN=").is_some())
            .await;
        let args = args(&seen);
        let at = |flag: &str| args.iter().position(|a| a == flag).unwrap();
        assert!(
            args.contains(&"--dangerously-skip-permissions".to_string()),
            "{args:?}"
        );
        assert_eq!(args[at("--effort") + 1], EFFORT, "{args:?}");
        assert_eq!(args[at("--model") + 1], MODEL, "{args:?}");
        assert_eq!(
            args[at("--settings") + 1],
            f.settings_file(id).to_string_lossy(),
            "{args:?}"
        );
        assert!(!args.iter().any(|a| a.contains(TOKEN)), "{args:?}");

        let settings: serde_json::Value =
            serde_json::from_slice(&std::fs::read(f.settings_file(id)).unwrap()).unwrap();
        assert_eq!(settings["skipDangerousModePermissionPrompt"], true);
        assert!(settings["hooks"]["SessionStart"].is_array(), "{settings}");

        assert_eq!(value(&seen, "GH_TOKEN=").as_deref(), Some(TOKEN));
        assert_eq!(value(&seen, "FOUNDRY_GIT_TOKEN=").as_deref(), Some(TOKEN));
        assert_eq!(value(&seen, "ASKPASS=").as_deref(), Some(TOKEN), "{seen}");
        assert_eq!(value(&seen, "GIT_CONFIG_COUNT=").as_deref(), Some("1"));
        assert_eq!(
            value(&seen, "GIT_CONFIG_KEY_0=").as_deref(),
            Some("credential.helper")
        );
        assert_eq!(value(&seen, "GIT_CONFIG_VALUE_0=").as_deref(), Some(""));
        for blank in [
            "GITHUB_TOKEN=",
            "GH_ENTERPRISE_TOKEN=",
            "GITHUB_ENTERPRISE_TOKEN=",
            "GIT_CONFIG_PARAMETERS=",
        ] {
            assert_eq!(value(&seen, blank).as_deref(), Some(""), "{blank} {seen}");
        }
        assert_eq!(
            value(&seen, "GIT_ALLOW_PROTOCOL=").as_deref(),
            Some("https")
        );
        // The owner's config is kept: we give the session no isolation. (The test process's
        // own environment is inherited, so only the values we would have set are refused.)
        assert_ne!(
            value(&seen, "GIT_CONFIG_GLOBAL=").as_deref(),
            Some("/dev/null")
        );
        assert_ne!(value(&seen, "GIT_CONFIG_NOSYSTEM=").as_deref(), Some("1"));

        let mark = f.mark_file(id);
        assert_eq!(std::fs::read_to_string(&mark).unwrap(), project.to_string());
        assert_eq!(
            std::fs::metadata(&mark).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(mark.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(f.sessions.is_factory(id), Some(project));
        assert_eq!(f.sessions.factory_sessions(project), [id]);
        assert!(f.sessions.factory_sessions(ProjectId::random()).is_empty());

        // A refusal before the token is read: this holds the token only if it leaked earlier.
        // The failure after the read is checked in the folder test below.
        let refused = f
            .sessions
            .start_factory(
                WorkspaceId::random(),
                f.worktree.path(),
                f.start_for(project),
                Message::new(FIRST_LINE.into(), "body".into()),
            )
            .await
            .unwrap_err();
        assert!(!format!("{refused} {refused:?}").contains(TOKEN));
        assert!(!Logged::everywhere().text().contains(TOKEN));
    }

    #[tokio::test]
    async fn the_first_messages_line_is_typed_as_the_first_prompt() {
        let f = Fx::new("- hook: {event: SessionStart}\n- wait_prompt\n- wait_input\n").await;

        let id = f
            .start_factory(WorkspaceId::random(), ProjectId::random())
            .await;

        let seen = f.shown(id, |t| value(t, "PROMPT=").is_some()).await;
        let typed: String = serde_json::from_str(&value(&seen, "PROMPT=").unwrap()).unwrap();
        assert_eq!(typed, FIRST_LINE);

        // The token is in no file: not under the data dir, not in Claude Code's state file.
        let mut files = vec![f.config.claude_json.clone()];
        let mut dirs = vec![f.data.path().to_path_buf()];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                let kind = entry.file_type().unwrap();
                if kind.is_dir() {
                    dirs.push(entry.path());
                } else if kind.is_file() {
                    files.push(entry.path());
                }
            }
        }
        assert!(files.len() > 3, "{files:?}");
        for file in files {
            let bytes = std::fs::read(&file).unwrap_or_default();
            assert!(
                !bytes.windows(TOKEN.len()).any(|w| w == TOKEN.as_bytes()),
                "the token is in {}",
                file.display()
            );
        }
    }

    #[tokio::test]
    async fn each_start_reads_the_token_again() {
        let f = Fx::new("- print_env: GH_TOKEN\n- wait_input\n").await;
        let other = "ghp_anotherMadeUpToken0987654321zyxwvuTSRQ";

        let first = f
            .start_factory(WorkspaceId::random(), ProjectId::random())
            .await;
        let seen = f.shown(first, |t| value(t, "GH_TOKEN=").is_some()).await;
        assert_eq!(value(&seen, "GH_TOKEN=").as_deref(), Some(TOKEN));

        f.secrets.set(&format!("github:{LOGIN}"), other).unwrap();
        let second = f
            .start_factory(WorkspaceId::random(), ProjectId::random())
            .await;
        let seen = f.shown(second, |t| value(t, "GH_TOKEN=").is_some()).await;
        assert_eq!(value(&seen, "GH_TOKEN=").as_deref(), Some(other));
    }

    #[tokio::test]
    async fn end_factory_is_ok_when_one_terminal_is_already_closed_and_ends_the_other() {
        let f = Fx::new("- wait_input\n").await;
        let project = ProjectId::random();
        let w = WorkspaceId::random();
        let first = f.start_factory(w, project).await;
        let second = f.start_factory(w, project).await;

        f.terminals.close(f.terminal_of(first)).await.unwrap();
        f.sessions.end_factory(w).await.unwrap();

        until("the second session to end", || {
            matches!(
                f.state(second),
                Some(SessionState::Finished | SessionState::Failed)
            )
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn end_factory_ends_a_start_that_is_still_in_flight() {
        let f = Arc::new(Fx::new("- wait_input\n").await);
        let project = ProjectId::random();
        let w = WorkspaceId::random();
        // Another thread holds the store, so the start stops in `launch`, after it is live.
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let holder = std::thread::spawn({
            let store = f.store.clone();
            move || {
                let _held = crate::sessions::lock(&store);
                held_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            }
        });
        held_rx.recv().unwrap();
        let start = tokio::spawn({
            let f = f.clone();
            async move { f.start_factory(w, project).await }
        });
        until("the start to be live", || {
            !crate::sessions::lock(&f.sessions.inner.live).is_empty()
        })
        .await;
        let end = tokio::spawn({
            let f = f.clone();
            async move { f.sessions.end_factory(w).await }
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        release_tx.send(()).unwrap();
        holder.join().unwrap();

        let id = start.await.unwrap();
        end.await.unwrap().unwrap();
        until("the session in flight to end", || {
            matches!(
                f.state(id),
                Some(SessionState::Finished | SessionState::Failed)
            )
        })
        .await;
    }

    /// `git` run as a factory session's child would, with HOME and XDG_CONFIG_HOME at `home`
    /// (where the owner's `.gitconfig` is), and over a `GIT_CONFIG_PARAMETERS` that names a
    /// helper, as an inherited one could. The test keeps the box's own `/etc/gitconfig` out.
    fn factory_git(home: &Path, args: &[&str]) -> Command {
        let data = tempfile::tempdir().unwrap();
        let secrets = MemorySecrets::default();
        secrets.set(&format!("github:{LOGIN}"), TOKEN).unwrap();
        let env = factory_env(&secrets, LOGIN, data.path()).unwrap();
        let mut git = Command::new("git");
        git.args(args)
            .current_dir(home)
            .env("HOME", home)
            .env("XDG_CONFIG_HOME", home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_CONFIG_GLOBAL")
            .env("GIT_SSH_COMMAND", "false");
        git.env("GIT_CONFIG_PARAMETERS", "'credential.helper'='store'");
        git.envs(env);
        git
    }

    fn out(mut git: Command) -> String {
        let out = git.output().unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    #[test]
    fn the_session_keeps_the_owners_git_config_and_has_no_credential_helper() {
        let home = tempfile::tempdir().unwrap();
        let hooks = home.path().join("hooks");
        std::fs::write(home.path().join("ignore"), ".env\n").unwrap();
        std::fs::write(
            home.path().join(".gitconfig"),
            format!(
                "[user]\n\tname = Owner Name\n\temail = owner@example.com\n\
                 [core]\n\texcludesFile = {}\n\thooksPath = {}\n\
                 [credential]\n\thelper = store\n",
                home.path().join("ignore").display(),
                hooks.display()
            ),
        )
        .unwrap();
        let repo = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let mut git = factory_git(home.path(), args);
            git.current_dir(repo.path());
            git
        };
        assert!(git(&["init", "-q"]).output().unwrap().status.success());

        assert_eq!(out(git(&["config", "user.name"])).trim(), "Owner Name");
        assert_eq!(
            out(git(&["config", "user.email"])).trim(),
            "owner@example.com"
        );
        assert_eq!(
            out(git(&["check-ignore", ".env"])).trim(),
            ".env",
            "the owner's global ignore file applies"
        );
        assert_eq!(
            out(git(&["config", "core.hooksPath"])).trim(),
            hooks.to_string_lossy()
        );
        // The last value git reads is the empty one: no helper is in effect.
        let helpers = out(git(&["config", "--get-all", "credential.helper"]));
        assert_eq!(helpers.lines().last(), Some(""), "{helpers:?}");
        let protocol = git(&["--version"])
            .get_envs()
            .find(|(k, _)| *k == "GIT_ALLOW_PROTOCOL")
            .and_then(|(_, v)| v.map(|v| v.to_string_lossy().into_owned()));
        assert_eq!(protocol.as_deref(), Some("https"));
        let ssh = git(&["ls-remote", "ssh://localhost/x"]).output().unwrap();
        assert!(
            String::from_utf8_lossy(&ssh.stderr).contains("not allowed"),
            "{}",
            String::from_utf8_lossy(&ssh.stderr)
        );
    }

    #[tokio::test]
    async fn a_start_for_a_login_with_no_token_is_refused_and_leaves_nothing() {
        let f = Fx::new("- wait_input\n").await;
        let project = ProjectId::random();
        let start = FactoryStart {
            login: "nobody".into(),
            ..f.start_for(project)
        };

        let error = f
            .sessions
            .start_factory(WorkspaceId::random(), f.worktree.path(), start, f.line())
            .await
            .unwrap_err();

        assert!(matches!(error, SessionError::NoToken(_)), "{error}");
        f.assert_nothing_left(&f.sessions, project);
    }

    #[tokio::test]
    async fn a_start_with_no_secret_store_is_refused_as_no_token_and_leaves_nothing() {
        let f = Fx::new("- wait_input\n").await;
        let project = ProjectId::random();
        let bare = f.again(false);

        let error = bare
            .start_factory(
                WorkspaceId::random(),
                f.worktree.path(),
                f.start_for(project),
                f.line(),
            )
            .await
            .unwrap_err();

        assert!(matches!(error, SessionError::NoToken(_)), "{error}");
        f.assert_nothing_left(&bare, project);
    }

    #[tokio::test]
    async fn a_first_message_with_a_body_is_refused_and_leaves_nothing() {
        let f = Fx::new("- wait_input\n").await;
        let project = ProjectId::random();

        let error = f
            .sessions
            .start_factory(
                WorkspaceId::random(),
                f.worktree.path(),
                f.start_for(project),
                Message::new(FIRST_LINE.into(), "the rules".into()),
            )
            .await
            .unwrap_err();

        assert!(matches!(error, SessionError::FirstMessageBody), "{error}");
        f.assert_nothing_left(&f.sessions, project);
    }

    #[tokio::test]
    async fn after_adopt_a_factory_session_is_still_one_and_a_p1_session_is_not() {
        let mut f = Fx::new("- wait_input\n").await;
        let project = ProjectId::random();
        let id = f.start_factory(WorkspaceId::random(), project).await;
        let p1 = f
            .sessions
            .start(WorkspaceId::random(), f.worktree.path(), MODEL)
            .await
            .unwrap();
        // A mark nobody can read is skipped, not taken.
        let junk = SessionId::random();
        let junk_dir = f.sessions_dir().join(junk.to_string());
        std::fs::create_dir_all(&junk_dir).unwrap();
        std::fs::write(junk_dir.join("factory"), "not a project").unwrap();

        let again = f.again(true);
        drop(std::mem::replace(&mut f.sessions, again));
        f.sessions.adopt().await;

        assert_eq!(f.sessions.is_factory(id), Some(project));
        assert_eq!(f.sessions.factory_sessions(project), [id]);
        assert_eq!(f.sessions.is_factory(p1), None);
        assert_eq!(f.sessions.is_factory(junk), None);
    }

    #[tokio::test]
    async fn end_factory_ends_only_factory_sessions_of_that_workspace() {
        let f = Fx::new("- wait_input\n").await;
        let project = ProjectId::random();
        let (a, b) = (WorkspaceId::random(), WorkspaceId::random());
        let in_a = f.start_factory(a, project).await;
        let in_b = f.start_factory(b, project).await;
        let p1_in_a = f.sessions.start(a, f.worktree.path(), MODEL).await.unwrap();

        f.sessions.end_factory(a).await.unwrap();

        until("the factory session of A to end", || {
            matches!(
                f.state(in_a),
                Some(SessionState::Finished | SessionState::Failed)
            )
        })
        .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        for other in [in_b, p1_in_a] {
            assert!(
                !matches!(
                    f.state(other),
                    Some(SessionState::Finished | SessionState::Failed)
                ),
                "{other} ended: {:?}",
                f.state(other)
            );
        }
        assert_eq!(f.sessions.factory_sessions(project), [in_b]);
        assert_eq!(f.sessions.is_factory(in_a), Some(project));
        // Ending a workspace with none, or twice, is no error.
        f.sessions.end_factory(a).await.unwrap();
        f.sessions.end_factory(WorkspaceId::random()).await.unwrap();
    }

    #[tokio::test]
    async fn a_p1_start_has_no_factory_flag_key_token_or_folder() {
        let f = Fx::new("- print_env: GH_TOKEN\n- wait_input\n").await;

        let id = f
            .sessions
            .start(WorkspaceId::random(), f.worktree.path(), MODEL)
            .await
            .unwrap();

        let seen = f
            .shown(id, |t| {
                value(t, "GH_TOKEN=").is_some()
                    || t.lines().any(|l| l.trim_end() == "GH_TOKEN unset")
            })
            .await;
        let args = args(&seen);
        assert!(args.contains(&"--model".to_string()), "{args:?}");
        assert!(
            !args
                .iter()
                .any(|a| a == "--dangerously-skip-permissions" || a == "--effort"),
            "{args:?}"
        );
        let settings: serde_json::Value =
            serde_json::from_slice(&std::fs::read(f.settings_file(id)).unwrap()).unwrap();
        assert!(settings.get("skipDangerousModePermissionPrompt").is_none());
        assert!(
            value(&seen, "ASKPASS=").is_none_or(|a| a.is_empty()),
            "{seen}"
        );
        // The test process may hold a GH_TOKEN of its own; the session must not hold ours.
        assert_ne!(value(&seen, "GH_TOKEN=").as_deref(), Some(TOKEN));
        assert!(!f.sessions_dir().join(id.to_string()).exists());
        assert_eq!(f.sessions.is_factory(id), None);
    }

    #[tokio::test]
    async fn a_start_that_fails_after_its_folder_is_written_removes_the_folder_too() {
        let logged = Logged::everywhere();
        let f = Fx::new("- wait_input\n").await;
        let project = ProjectId::random();
        std::fs::write(&f.config.claude_json, "not json").unwrap();

        let error = f
            .sessions
            .start_factory(
                WorkspaceId::random(),
                f.worktree.path(),
                f.start_for(project),
                f.line(),
            )
            .await
            .unwrap_err();

        // The token was read before this failure, so this is a check that it did not leak.
        assert!(matches!(error, SessionError::ClaudeState { .. }), "{error}");
        assert!(!format!("{error} {error:?}").contains(TOKEN));
        assert!(!logged.text().contains(TOKEN));
        f.assert_nothing_left(&f.sessions, project);
        assert_eq!(f.sessions.is_factory(SessionId::random()), None);
    }

    /// Holds the trust lock on another thread, so a start stops after its files are written.
    /// Dropping the returned sender (or sending) releases it.
    fn hold_trust(f: &Fx) -> (std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>) {
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let inner = f.sessions.inner.clone();
        let holder = std::thread::spawn(move || {
            let _held = crate::sessions::lock(&inner.trust);
            held_tx.send(()).unwrap();
            let _ = release_rx.recv();
        });
        held_rx.recv().unwrap();
        (release_tx, holder)
    }

    fn settings_files(f: &Fx) -> usize {
        std::fs::read_dir(f.sessions_dir())
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
                    .count()
            })
            .unwrap_or(0)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_end_that_comes_while_a_start_waits_for_the_trust_lock_wins() {
        let f = Arc::new(Fx::new("- wait_input\n").await);
        let project = ProjectId::random();
        let w = WorkspaceId::random();
        let (release, holder) = hold_trust(&f);
        let start = tokio::spawn({
            let f = f.clone();
            async move {
                f.sessions
                    .start_factory(w, f.worktree.path(), f.start_for(project), f.line())
                    .await
            }
        });
        until("the start's settings file", || settings_files(&f) == 1).await;

        f.sessions.end_factory(w).await.unwrap();
        release.send(()).unwrap();
        holder.join().unwrap();

        let result = start.await.unwrap();
        assert!(matches!(result, Err(SessionError::Ended)), "{result:?}");
        f.assert_nothing_left(&f.sessions, project);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_end_for_one_workspace_leaves_a_start_for_another_alone() {
        let f = Arc::new(Fx::new("- wait_input\n").await);
        let project = ProjectId::random();
        let (w, other) = (WorkspaceId::random(), WorkspaceId::random());
        let (release, holder) = hold_trust(&f);
        let starts = [w, other].map(|workspace| {
            tokio::spawn({
                let f = f.clone();
                async move {
                    f.sessions
                        .start_factory(workspace, f.worktree.path(), f.start_for(project), f.line())
                        .await
                }
            })
        });
        until("both settings files", || settings_files(&f) == 2).await;

        f.sessions.end_factory(w).await.unwrap();
        release.send(()).unwrap();
        holder.join().unwrap();

        let [ended, kept] = starts;
        let ended = ended.await.unwrap();
        assert!(matches!(ended, Err(SessionError::Ended)), "{ended:?}");
        let kept = kept.await.unwrap().unwrap();
        assert_eq!(f.sessions.factory_sessions(project), [kept]);
        assert_eq!(f.session_records(), 1);
        // Started after the end, a start for the same workspace is not refused.
        f.start_factory(w, project).await;
    }

    #[tokio::test]
    async fn a_factory_start_whose_record_cannot_be_written_errs_and_leaves_nothing() {
        let f = Fx::new("- wait_input\n").await;
        let project = ProjectId::random();
        f.sessions
            .inner
            .fail_writes
            .store(1, std::sync::atomic::Ordering::SeqCst);

        let error = f
            .sessions
            .start_factory(
                WorkspaceId::random(),
                f.worktree.path(),
                f.start_for(project),
                f.line(),
            )
            .await
            .unwrap_err();

        assert!(matches!(error, SessionError::Store(_)), "{error}");
        f.assert_nothing_left(&f.sessions, project);
    }

    // The start-time refusal of an owner's setup that would answer github.com before the askpass.

    const SECRET: &str = "SECRETPW-4f9a1c";

    /// The owner's git: HOME and XDG_CONFIG_HOME at `home`, no system config, none of the
    /// session's own overrides.
    fn owner_git(home: &Path) -> impl Fn() -> Command + '_ {
        move || {
            let mut git = Command::new("git");
            git.env("HOME", home)
                .env("XDG_CONFIG_HOME", home)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env_remove("GIT_CONFIG_GLOBAL")
                .env_remove("GIT_CONFIG_SYSTEM")
                .env_remove("GIT_CONFIG_PARAMETERS")
                .env_remove("GIT_CONFIG_COUNT");
            git
        }
    }

    /// A made-up home whose `.gitconfig` holds `global`, and a fresh repository.
    fn setup(global: &str) -> (TempDir, TempDir) {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join(".gitconfig"), global).unwrap();
        let repo = tempfile::tempdir().unwrap();
        run(owner_git(home.path()), repo.path(), &["init", "-q"]);
        (home, repo)
    }

    fn run(git: impl Fn() -> Command, repo: &Path, args: &[&str]) {
        let out = git().args(args).current_dir(repo).output().unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// The production check, over the made-up `home`.
    fn check(home: &Path, repo: &Path) -> Result<(), SessionError> {
        owner_setup_refusal(Some(home), repo)
    }

    /// What the refusal says, Display and Debug, when `result` is one.
    fn said(result: Result<(), SessionError>) -> String {
        let error = result.expect_err("the start should be refused");
        assert!(matches!(error, SessionError::GitSetup(_)), "{error}");
        format!("{error} | {error:?}")
    }

    #[test]
    fn a_global_insteadof_that_rewrites_github_refuses_and_names_the_key_and_file() {
        let (home, repo) = setup(&format!(
            "[url \"https://other:{SECRET}@mirror.example/\"]\n\tinsteadOf = https://github.com/\n"
        ));

        let said = said(check(home.path(), repo.path()));

        assert!(
            said.contains("url.https://***@mirror.example/.insteadof"),
            "{said}"
        );
        assert!(said.contains(".gitconfig"), "{said}");
        assert!(!said.contains(SECRET), "{said}");
    }

    #[test]
    fn a_global_pushinsteadof_that_rewrites_github_refuses() {
        let (home, repo) =
            setup("[url \"https://push.example/\"]\n\tpushInsteadOf = https://github.com/org/\n");

        let said = said(check(home.path(), repo.path()));

        assert!(
            said.contains("url.https://push.example/.pushinsteadof"),
            "{said}"
        );
        assert!(said.contains(".gitconfig"), "{said}");
    }

    #[test]
    fn a_system_insteadof_that_rewrites_github_refuses() {
        let (home, repo) = setup("");
        let system = home.path().join("system-gitconfig");
        std::fs::write(
            &system,
            "[url \"https://sys.example/\"]\n\tinsteadOf = https://\n",
        )
        .unwrap();
        let git = || {
            let mut git = owner_git(home.path())();
            git.env_remove("GIT_CONFIG_NOSYSTEM")
                .env("GIT_CONFIG_SYSTEM", &system);
            git
        };

        let said = said(git_refusal(git, None, repo.path()));

        assert!(
            said.contains("url.https://sys.example/.insteadof"),
            "{said}"
        );
        assert!(said.contains("system-gitconfig"), "{said}");
    }

    #[test]
    fn a_repo_insteadof_that_rewrites_github_refuses() {
        let (home, repo) = setup("");
        run(
            owner_git(home.path()),
            repo.path(),
            &[
                "config",
                "url.https://repo.example/.insteadOf",
                "https://github.com",
            ],
        );

        let said = said(check(home.path(), repo.path()));

        assert!(
            said.contains("url.https://repo.example/.insteadof"),
            "{said}"
        );
        assert!(said.contains(".git/config"), "{said}");
    }

    #[test]
    fn an_insteadof_in_an_included_file_refuses_and_names_that_file() {
        let home = tempfile::tempdir().unwrap();
        let extra = home.path().join("extra-config");
        std::fs::write(
            &extra,
            "[url \"https://inc.example/\"]\n\tinsteadOf = https://github.com/\n",
        )
        .unwrap();
        let (home_with, repo) = {
            std::fs::write(
                home.path().join(".gitconfig"),
                format!("[include]\n\tpath = {}\n", extra.display()),
            )
            .unwrap();
            let repo = tempfile::tempdir().unwrap();
            run(owner_git(home.path()), repo.path(), &["init", "-q"]);
            (home, repo)
        };

        let said = said(check(home_with.path(), repo.path()));

        assert!(
            said.contains("url.https://inc.example/.insteadof"),
            "{said}"
        );
        assert!(said.contains("extra-config"), "{said}");
    }

    #[test]
    fn a_global_extraheader_refuses_and_does_not_print_it() {
        let (home, repo) = setup(&format!(
            "[http]\n\textraHeader = Authorization: Basic {SECRET}\n"
        ));

        let said = said(check(home.path(), repo.path()));

        assert!(said.contains("http.extraheader"), "{said}");
        assert!(said.contains(".gitconfig"), "{said}");
        assert!(!said.contains(SECRET), "{said}");
    }

    #[test]
    fn an_extraheader_for_github_com_refuses_and_does_not_print_it() {
        let (home, repo) = setup(&format!(
            "[http \"https://github.com/\"]\n\textraHeader = Authorization: Basic {SECRET}\n"
        ));

        let said = said(check(home.path(), repo.path()));

        assert!(
            said.contains("http.https://github.com/.extraheader"),
            "{said}"
        );
        assert!(!said.contains(SECRET), "{said}");
    }

    #[test]
    fn an_origin_url_with_a_password_refuses_and_does_not_print_it() {
        let (home, repo) = setup("");
        run(
            owner_git(home.path()),
            repo.path(),
            &[
                "remote",
                "add",
                "origin",
                &format!("https://bob:{SECRET}@example.com/o/r.git"),
            ],
        );

        let said = said(check(home.path(), repo.path()));

        assert!(said.contains("remote.origin.url"), "{said}");
        assert!(!said.contains("remote.origin.pushurl"), "{said}");
        assert!(said.contains(".git/config"), "{said}");
        assert!(!said.contains(SECRET), "{said}");
    }

    #[test]
    fn an_origin_rewritten_to_carry_a_user_name_names_the_rewrite_key() {
        let (home, repo) = setup(&format!(
            "[url \"https://bob:{SECRET}@github.com/\"]\n\tinsteadOf = gh:\n"
        ));
        run(
            owner_git(home.path()),
            repo.path(),
            &["remote", "add", "origin", "gh:o/r.git"],
        );

        let said = said(check(home.path(), repo.path()));

        assert!(
            said.contains("url.https://***@github.com/.insteadof"),
            "{said}"
        );
        assert!(said.contains(".gitconfig"), "{said}");
        assert!(!said.contains(SECRET), "{said}");
    }

    #[test]
    fn an_origin_scp_url_with_a_user_name_refuses() {
        let (home, repo) = setup("");
        run(
            owner_git(home.path()),
            repo.path(),
            &["remote", "add", "origin", "bob@example.com:o/r.git"],
        );

        let said = said(check(home.path(), repo.path()));

        assert!(said.contains("remote.origin.url"), "{said}");
        assert!(!said.contains("bob"), "{said}");
    }

    #[test]
    fn an_extraheader_for_a_url_that_covers_github_com_in_another_spelling_refuses() {
        for section in [
            "https://github.com:443/",
            "https://GitHub.com/",
            "https://bob@github.com/",
        ] {
            let (home, repo) = setup(&format!(
                "[http \"{section}\"]\n\textraHeader = Authorization: Basic {SECRET}\n"
            ));

            let said = said(check(home.path(), repo.path()));

            assert!(said.contains(".extraheader"), "{section} {said}");
            assert!(said.contains(".gitconfig"), "{section} {said}");
            assert!(!said.contains(SECRET), "{section} {said}");
        }
    }

    #[test]
    fn an_empty_insteadof_refuses() {
        let (home, repo) = setup("[url \"https://rewrite.example/\"]\n\tinsteadOf =\n");

        let said = said(check(home.path(), repo.path()));

        assert!(
            said.contains("url.https://rewrite.example/.insteadof"),
            "{said}"
        );
    }

    #[test]
    fn a_netrc_that_cannot_be_read_refuses_naming_the_file() {
        let (home, repo) = setup("");
        std::fs::create_dir(home.path().join(".netrc")).unwrap();

        let said = said(check(home.path(), repo.path()));

        assert!(said.contains(".netrc"), "{said}");
    }

    #[test]
    fn netrc_keywords_and_hosts_are_read_as_curl_reads_them() {
        for netrc in [
            format!("MACHINE github.com LOGIN bob PASSWORD {SECRET}\n"),
            format!("machine \"github.com\" login bob password {SECRET}\n"),
            format!("Machine GITHUB.COM login bob password {SECRET}\n"),
            format!("Default login bob password {SECRET}\n"),
            format!(
                "machine example.com login a\\ b password x\nmachine github.com password {SECRET}\n"
            ),
        ] {
            let (home, repo) = setup("");
            std::fs::write(home.path().join(".netrc"), &netrc).unwrap();

            let said = said(check(home.path(), repo.path()));

            assert!(said.contains(".netrc"), "{netrc:?} {said}");
            assert!(!said.contains(SECRET), "{said}");
        }
    }

    #[test]
    fn a_git_config_variable_in_the_environment_does_not_hide_the_owners_config() {
        let (home, repo) =
            setup("[url \"https://rewrite.example/\"]\n\tinsteadOf = https://github.com/\n");
        let empty = home.path().join("empty-config");
        std::fs::write(&empty, "").unwrap();
        let git = || {
            let mut base = Command::new("git");
            base.env("GIT_CONFIG", &empty);
            let mut git = check_git(base, Some(home.path()));
            git.env("HOME", home.path());
            git
        };

        let said = said(git_refusal(git, None, repo.path()));

        assert!(
            said.contains("url.https://rewrite.example/.insteadof"),
            "{said}"
        );
    }

    #[test]
    fn the_netrc_is_in_the_home_given_and_none_without_one() {
        assert_eq!(
            netrc_of(Some("/home/o".into())),
            Some(PathBuf::from("/home/o/.netrc"))
        );
        assert_eq!(netrc_of(None), None);
    }

    #[tokio::test]
    async fn a_start_with_a_netrc_entry_for_github_com_is_refused_and_leaves_nothing() {
        let logged = Logged::everywhere();
        let f = Fx::new("- wait_input\n").await;
        let project = ProjectId::random();
        std::fs::write(
            f.git_home.path().join(".netrc"),
            format!("machine github.com login x password {SECRET}\n"),
        )
        .unwrap();

        let error = f
            .sessions
            .start_factory(
                WorkspaceId::random(),
                f.worktree.path(),
                f.start_for(project),
                f.line(),
            )
            .await
            .unwrap_err();

        assert!(matches!(error, SessionError::GitSetup(_)), "{error}");
        let said = format!("{error} {error:?}");
        assert!(said.contains(".netrc"), "{said}");
        assert!(!said.contains(TOKEN) && !said.contains(SECRET), "{said}");
        assert!(!logged.text().contains(SECRET) && !logged.text().contains(TOKEN));
        f.assert_nothing_left(&f.sessions, project);
    }

    #[tokio::test]
    async fn a_start_in_a_worktree_whose_origin_carries_a_password_is_refused_and_leaves_nothing() {
        let logged = Logged::everywhere();
        let f = Fx::new("- wait_input\n").await;
        let project = ProjectId::random();
        let git = owner_git(f.git_home.path());
        run(&git, f.worktree.path(), &["init", "-q"]);
        run(
            &git,
            f.worktree.path(),
            &[
                "remote",
                "add",
                "origin",
                &format!("https://bob:{SECRET}@github.com/o/r.git"),
            ],
        );

        let error = f
            .sessions
            .start_factory(
                WorkspaceId::random(),
                f.worktree.path(),
                f.start_for(project),
                f.line(),
            )
            .await
            .unwrap_err();

        assert!(matches!(error, SessionError::GitSetup(_)), "{error}");
        let said = format!("{error} {error:?}");
        assert!(said.contains("remote.origin.url"), "{said}");
        assert!(!said.contains(TOKEN) && !said.contains(SECRET), "{said}");
        assert!(!logged.text().contains(SECRET) && !logged.text().contains(TOKEN));
        f.assert_nothing_left(&f.sessions, project);
    }

    #[test]
    fn an_origin_push_url_with_a_user_name_refuses() {
        let (home, repo) = setup("");
        let git = owner_git(home.path());
        run(
            &git,
            repo.path(),
            &["remote", "add", "origin", "https://example.com/o/r.git"],
        );
        run(
            &git,
            repo.path(),
            &[
                "remote",
                "set-url",
                "--push",
                "origin",
                "https://bob@example.com/o/r.git",
            ],
        );

        let said = said(check(home.path(), repo.path()));

        assert!(said.contains("remote.origin.pushurl"), "{said}");
        assert!(!said.contains("bob"), "{said}");
    }

    #[test]
    fn a_netrc_entry_for_github_com_refuses_and_does_not_print_the_password() {
        let (home, repo) = setup("");
        std::fs::write(
            home.path().join(".netrc"),
            format!("machine example.com login a password b\nmachine GitHub.com login bob password {SECRET}\n"),
        )
        .unwrap();

        let said = said(check(home.path(), repo.path()));

        assert!(said.contains("machine github.com"), "{said}");
        assert!(said.contains(".netrc"), "{said}");
        assert!(!said.contains(SECRET), "{said}");
    }

    #[test]
    fn a_netrc_default_entry_refuses() {
        let (home, repo) = setup("");
        std::fs::write(
            home.path().join(".netrc"),
            format!("default login bob password {SECRET}\n"),
        )
        .unwrap();

        let said = said(check(home.path(), repo.path()));

        assert!(said.contains("default"), "{said}");
        assert!(said.contains(".netrc"), "{said}");
        assert!(!said.contains(SECRET), "{said}");
    }

    #[test]
    fn a_git_that_cannot_read_the_config_refuses() {
        let (home, _repo) = setup("");
        let missing = home.path().join("no-such-worktree");

        let said = said(check(home.path(), &missing));

        assert!(said.contains("could not be read"), "{said}");
    }

    #[test]
    fn setups_that_do_not_answer_github_com_are_not_refused() {
        let hooks = tempfile::tempdir().unwrap();
        let (home, repo) = setup(&format!(
            "[user]\n\tname = Owner\n\temail = o@example.com\n\
             [core]\n\texcludesFile = ~/ignore\n\thooksPath = {}\n\
             [url \"https://github.com/\"]\n\tinsteadOf = git@github.com:\n\
             [url \"http://plain.example/\"]\n\tinsteadOf = http://\n\
             [http \"https://example.com/\"]\n\textraHeader = X-Other: {SECRET}\n\
             [http \"http://github.com/\"]\n\textraHeader = X-Other: {SECRET}\n\
             [http \"https://github.com:8443/\"]\n\textraHeader = X-Other: {SECRET}\n\
             [http \"https://*.github.com/\"]\n\textraHeader = X-Other: {SECRET}\n\
             [credential]\n\thelper = store\n",
            hooks.path().display()
        ));
        std::fs::write(
            home.path().join(".netrc"),
            "machine example.com login a password default\nmacdef init\nmachine github.com\n\n",
        )
        .unwrap();
        run(
            owner_git(home.path()),
            repo.path(),
            &["remote", "add", "origin", "https://github.com/o/r.git"],
        );

        check(home.path(), repo.path()).unwrap();
    }

    #[test]
    fn a_wildcard_extraheader_url_that_covers_github_com_refuses() {
        let (home, repo) = setup("[http \"https://*.com/\"]\n\textraHeader = X: y\n");
        assert!(said(check(home.path(), repo.path())).contains("http.https://*.com/.extraheader"));
    }

    #[tokio::test]
    async fn a_start_whose_owner_setup_would_answer_github_com_is_refused_and_leaves_nothing() {
        let logged = Logged::everywhere();
        let f = Fx::new("- wait_input\n").await;
        let project = ProjectId::random();
        std::fs::write(
            f.git_home.path().join(".gitconfig"),
            format!(
                "[url \"https://other:{SECRET}@github.com/\"]\n\tinsteadOf = https://github.com/\n"
            ),
        )
        .unwrap();

        let error = f
            .sessions
            .start_factory(
                WorkspaceId::random(),
                f.worktree.path(),
                f.start_for(project),
                f.line(),
            )
            .await
            .unwrap_err();

        assert!(matches!(error, SessionError::GitSetup(_)), "{error}");
        let said = format!("{error} {error:?}");
        assert!(!said.contains(TOKEN) && !said.contains(SECRET), "{said}");
        assert!(!logged.text().contains(TOKEN));
        assert!(!logged.text().contains(SECRET));
        f.assert_nothing_left(&f.sessions, project);
    }
}
