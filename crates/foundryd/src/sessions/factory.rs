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
/// The invariant: the session's git pushes as `login` and no one else. So it reads no system
/// and no global config (`GIT_CONFIG_NOSYSTEM`, `GIT_CONFIG_GLOBAL=/dev/null`, as
/// [`GitAuth`] sets them): nothing there, whether a helper, a URL rewrite that carries
/// credentials, an `extraHeader` or an ssh command, can answer for the machine's owner. Only
/// the owner's `identity` (`user.name` and `user.email`, from [`owner_identity`]) is carried in,
/// as `GIT_CONFIG_COUNT` pins after the emptied helper. `GIT_ALLOW_PROTOCOL=https` is read from
/// [`GitAuth`] too: a repository's own config can still rewrite a URL, and it cannot reach ssh.
/// An inherited `GIT_CONFIG_PARAMETERS` is read after `GIT_CONFIG_COUNT`, so it is emptied. The
/// other GitHub tokens that `gh` and git's helpers read are blanked, the ones
/// [`git_auth::helpers_off_env`] blanks.
fn factory_env(
    secrets: &dyn crate::secrets::Secrets,
    login: &str,
    data: &Path,
    identity: &[(String, String)],
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
    let (Some(askpass), Some(token), Some(protocol), Some(global), Some(no_system)) = (
        get("GIT_ASKPASS"),
        get(git_auth::TOKEN_VAR),
        get("GIT_ALLOW_PROTOCOL"),
        get("GIT_CONFIG_GLOBAL"),
        get("GIT_CONFIG_NOSYSTEM"),
    ) else {
        return Err(SessionError::NoAskpass);
    };
    let mut env = vec![
        ("GH_TOKEN".to_owned(), token.clone()),
        ("GIT_ASKPASS".into(), askpass),
        (git_auth::TOKEN_VAR.into(), token),
        ("GIT_CONFIG_NOSYSTEM".into(), no_system),
        ("GIT_CONFIG_GLOBAL".into(), global),
        ("GIT_CONFIG_COUNT".into(), (1 + identity.len()).to_string()),
        ("GIT_CONFIG_KEY_0".into(), "credential.helper".into()),
        ("GIT_CONFIG_VALUE_0".into(), String::new()),
        ("GIT_CONFIG_PARAMETERS".into(), String::new()),
        ("GIT_ALLOW_PROTOCOL".into(), protocol),
    ];
    for (n, (key, value)) in identity.iter().enumerate() {
        env.push((format!("GIT_CONFIG_KEY_{}", n + 1), key.clone()));
        env.push((format!("GIT_CONFIG_VALUE_{}", n + 1), value.clone()));
    }
    env.extend(
        git_auth::helpers_off_env()
            .into_iter()
            .filter(|(key, value)| {
                key.ends_with("_TOKEN") && key != "GH_TOKEN" && value.is_empty()
            }),
    );
    Ok(env)
}

/// The owner's `user.name` and `user.email`, from the global config `git` reads, as config
/// pins in that order. One that is not set is left out, and so is both when git cannot run:
/// no identity is pinned, and nothing is an error.
fn owner_identity(mut git: Command) -> Vec<(String, String)> {
    let Ok(out) = git
        .args([
            "config",
            "--global",
            "--get-regexp",
            r"^user\.(name|email)$",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let last = |key: &str| {
        text.lines()
            .filter_map(|line| line.split_once(' '))
            .filter(|(k, _)| *k == key)
            .map(|(_, value)| value.to_owned())
            .next_back()
    };
    ["user.name", "user.email"]
        .into_iter()
        .filter_map(|key| last(key).map(|value| (key.to_owned(), value)))
        .collect()
}

impl Sessions {
    /// Starts Claude Code for a factory session in a new terminal in `worktree`, the worktree
    /// of `workspace`, as [`Sessions::start`] does, with the flags, settings key, environment
    /// and mark of a factory session. `first`'s line is its first prompt, typed as
    /// [`Sessions::start_with`] types one.
    ///
    /// The token is read first: a login the machine holds no token for is
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
            let env = blocking(move || match secrets {
                Some(secrets) => factory_env(
                    secrets.as_ref(),
                    &login,
                    &data,
                    &owner_identity(Command::new("git")),
                ),
                None => Err(crate::git_auth::NoToken::Unset { login }.into()),
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
            let sessions = Sessions::new(
                me,
                store.clone(),
                terminals.clone(),
                data.path(),
                config.clone(),
            )
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
            );
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
        // One pin for the helper, and one for each of the owner's name and email this machine has.
        let count: usize = value(&seen, "GIT_CONFIG_COUNT=").unwrap().parse().unwrap();
        assert!((1..=3).contains(&count), "{count}");
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
        assert_eq!(
            value(&seen, "GIT_CONFIG_GLOBAL=").as_deref(),
            Some("/dev/null")
        );
        assert_eq!(value(&seen, "GIT_CONFIG_NOSYSTEM=").as_deref(), Some("1"));

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
    /// (where the owner's `.gitconfig` is), in the factory env built with `identity`, and over
    /// a `GIT_CONFIG_PARAMETERS` that names a helper, as an inherited one could.
    fn factory_git(home: &Path, identity: &[(String, String)], args: &[&str]) -> Command {
        let data = tempfile::tempdir().unwrap();
        let secrets = MemorySecrets::default();
        secrets.set(&format!("github:{LOGIN}"), TOKEN).unwrap();
        let env = factory_env(&secrets, LOGIN, data.path(), identity).unwrap();
        let mut git = Command::new("git");
        git.args(args)
            .current_dir(home)
            .env("HOME", home)
            .env("XDG_CONFIG_HOME", home)
            .env("GIT_SSH_COMMAND", "false");
        git.env("GIT_CONFIG_PARAMETERS", "'credential.helper'='store'");
        git.envs(env);
        git
    }

    /// The owner's git: HOME and XDG_CONFIG_HOME at `home`, no system config.
    fn owners_git(home: &Path) -> Command {
        let mut git = Command::new("git");
        git.env("HOME", home)
            .env("XDG_CONFIG_HOME", home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_CONFIG_GLOBAL")
            .env_remove("GIT_CONFIG_PARAMETERS")
            .env_remove("GIT_CONFIG_COUNT");
        git
    }

    #[test]
    fn a_global_rewrite_that_carries_another_accounts_credentials_does_not_reach_the_session() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(".gitconfig"),
            "[url \"https://other:OTHERTOKEN@github.com/\"]\n\tinsteadOf = https://github.com/\n\
             [url \"git@github.com:\"]\n\tinsteadOf = https://github.com/\n",
        )
        .unwrap();
        // The owner's own git does rewrite, so the test would show it.
        let owner = owners_git(home.path())
            .args(["ls-remote", "--get-url", "https://github.com/o/r.git"])
            .output()
            .unwrap();
        assert_ne!(
            String::from_utf8_lossy(&owner.stdout).trim(),
            "https://github.com/o/r.git"
        );

        let out = factory_git(
            home.path(),
            &[],
            &["ls-remote", "--get-url", "https://github.com/o/r.git"],
        )
        .output()
        .unwrap();

        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "https://github.com/o/r.git",
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn a_global_header_and_helper_do_not_reach_the_session() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(".gitconfig"),
            "[http \"https://github.com/\"]\n\textraHeader = Authorization: Basic T1RIRVI6VE9LRU4=\n\
             [credential]\n\thelper = store\n",
        )
        .unwrap();

        let header = factory_git(
            home.path(),
            &[],
            &[
                "config",
                "--get-all",
                "http.https://github.com/.extraheader",
            ],
        )
        .output()
        .unwrap();
        let helpers = factory_git(
            home.path(),
            &[],
            &["config", "--get-all", "credential.helper"],
        )
        .output()
        .unwrap();

        assert_eq!(String::from_utf8_lossy(&header.stdout), "");
        assert_eq!(String::from_utf8_lossy(&helpers.stdout), "\n");
    }

    #[test]
    fn the_owners_name_and_email_are_carried_in_and_nothing_else() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(".gitconfig"),
            "[user]\n\tname = Owner Name\n\temail = owner@example.com\n\
             [url \"https://other:OTHERTOKEN@github.com/\"]\n\tinsteadOf = https://github.com/\n",
        )
        .unwrap();

        let pins = owner_identity(owners_git(home.path()));

        assert_eq!(
            pins,
            [
                ("user.name".to_string(), "Owner Name".to_string()),
                ("user.email".to_string(), "owner@example.com".to_string()),
            ]
        );
        let repo = tempfile::tempdir().unwrap();
        let init = factory_git(home.path(), &pins, &["init", "-q"])
            .current_dir(repo.path())
            .output()
            .unwrap();
        assert!(init.status.success());
        for (key, want) in [
            ("user.name", "Owner Name"),
            ("user.email", "owner@example.com"),
        ] {
            let out = factory_git(home.path(), &pins, &["config", key])
                .current_dir(repo.path())
                .output()
                .unwrap();
            assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), want);
        }
        let url = factory_git(
            home.path(),
            &pins,
            &["ls-remote", "--get-url", "https://github.com/o/r.git"],
        )
        .current_dir(repo.path())
        .output()
        .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&url.stdout).trim(),
            "https://github.com/o/r.git"
        );
    }

    #[test]
    fn an_owner_with_no_name_or_email_pins_none() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(".gitconfig"),
            "[core]\n\tautocrlf = false\n",
        )
        .unwrap();
        assert!(owner_identity(owners_git(home.path())).is_empty());

        let bare = tempfile::tempdir().unwrap();
        assert!(owner_identity(owners_git(bare.path())).is_empty());

        // git that cannot run is no identity, not an error.
        assert!(owner_identity(Command::new("/nonexistent/git")).is_empty());
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
}
