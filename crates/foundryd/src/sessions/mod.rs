//! Launches Claude Code in a terminal, receives its hooks and keeps each session's state.
//!
//! [`Sessions::start`] writes a settings file that registers hooks posting to this daemon's
//! loopback port with a per-session token, marks the worktree trusted for Claude Code, and
//! starts `claude --settings <file>` in a terminal with the gateway variables set. A first
//! prompt is typed into the terminal once Claude Code has started the session and takes input,
//! so it is in no process's arguments, which every user on the machine can read. From then on
//! the session's state comes from three sources, combined by [`state::next_state`]: the hooks
//! ([`Sessions::router`]), the terminal's output, and the child's exit. A session that is
//! working with no hook and no output for [`STUCK_AFTER`] is stuck.
//!
//! A session's record is written whenever its state changes, under the session's own lock, so
//! one session's writes never cross. The gateway key reaches the child's environment only: it
//! is in no file and no log line, and [`Gateway`]'s `Debug` hides it.
//!
//! A daemon that starts again takes up the sessions the one before it left running
//! ([`Sessions::adopt`]): the terminal holders outlive the daemon, so their sessions go on, and
//! their hooks still post to the same port with the same token.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use axum::Router;
use axum::routing::post;
use bytes::Bytes;
use foundry_proto::ids::{MachineId, SessionId, TerminalId, WorkspaceId};
use foundry_proto::record::{AnyRecord, Record, Session};
use foundry_proto::state::{SessionState, TerminalState};
use futures::StreamExt as _;
use tokio::sync::watch;
use tokio::task::AbortHandle;
use tokio::time::Instant;

use crate::store::{Store, StoreError};
use crate::terminals::{TerminalError, Terminals};
use state::{Event, Hook, next_state};

pub mod brief;
pub mod claude;
mod hooks;
pub mod state;
#[cfg(test)]
mod tests;

/// A working session with no output and no hook for this long is stuck.
pub const STUCK_AFTER: Duration = Duration::from_secs(10 * 60);

/// How long to wait before trying again after a failure: the output follower attaching again,
/// the exit being read, the exit, a stall or a hook being recorded. Each wait is twice the one
/// before, up to [`RETRY_AT_MOST`].
const RETRY_AFTER: Duration = Duration::from_millis(100);
/// The longest wait between two tries.
const RETRY_AT_MOST: Duration = Duration::from_secs(10);

/// The waits of one retry loop: [`RETRY_AFTER`], then each twice the one before, up to
/// [`RETRY_AT_MOST`]. It also tells the loop which failure is its first, so each loop warns
/// once.
struct Backoff {
    wait: Duration,
    failed: bool,
}

impl Backoff {
    fn new() -> Self {
        Self {
            wait: RETRY_AFTER,
            failed: false,
        }
    }

    /// Notes a failure, and says whether it is the loop's first.
    fn first_failure(&mut self) -> bool {
        !std::mem::replace(&mut self.failed, true)
    }

    /// The wait before the next try. The one after is twice as long, up to [`RETRY_AT_MOST`].
    fn next_wait(&mut self) -> Duration {
        let wait = self.wait;
        self.wait = (wait * 2).min(RETRY_AT_MOST);
        wait
    }

    /// Starts again as a new loop: the waits from [`RETRY_AFTER`], and the next failure is a
    /// first.
    fn reset(&mut self) {
        *self = Self::new();
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error(transparent)]
    Terminal(#[from] TerminalError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("sessions: {what}: {source}")]
    Io {
        /// A path and the step that failed.
        what: String,
        source: io::Error,
    },
    #[error("sessions: Claude Code's state file {} cannot be used: {reason}", path.display())]
    ClaudeState { path: PathBuf, reason: String },
    #[error("sessions: a step run apart from its caller did not finish: {0}")]
    Interrupted(#[from] tokio::task::JoinError),
}

/// The model gateway a session's Claude Code talks to.
#[derive(Clone)]
pub struct Gateway {
    /// `ANTHROPIC_BASE_URL` in the child.
    pub base_url: String,
    /// `ANTHROPIC_AUTH_TOKEN` in the child. Never logged, never written to a file.
    pub key: String,
}

impl fmt::Debug for Gateway {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Gateway")
            .field("base_url", &self.base_url)
            .field("key", &"<redacted>")
            .finish()
    }
}

/// How sessions start Claude Code.
#[derive(Clone, Debug)]
pub struct Config {
    /// The command that starts Claude Code, e.g. `["claude"]`. The settings and the model are
    /// added to it.
    pub claude: Vec<String>,
    /// Claude Code's state file, where a worktree is marked trusted.
    pub claude_json: PathBuf,
    /// The loopback port [`Sessions::router`] is served on.
    pub hook_port: u16,
    pub gateway: Gateway,
}

/// This machine's sessions.
pub struct Sessions {
    inner: Arc<Inner>,
}

struct Inner {
    me: MachineId,
    store: Arc<Mutex<Store>>,
    terminals: Arc<Terminals>,
    /// Where settings files live, `<data dir>/sessions`.
    dir: PathBuf,
    config: Config,
    /// Held while a worktree is marked trusted, so two starts never overwrite each other.
    trust: Mutex<()>,
    live: Mutex<HashMap<SessionId, Arc<Live>>>,
    /// Tests: how many of the next writes fail, as a busy store would.
    #[cfg(test)]
    fail_writes: std::sync::atomic::AtomicUsize,
    /// Tests: how many of the next reads of a terminal's exit fail.
    #[cfg(test)]
    fail_exit_reads: std::sync::atomic::AtomicUsize,
}

/// A session this daemon started, or took up from the daemon before it.
struct Live {
    id: SessionId,
    /// What the session's hooks present as a bearer token.
    token: String,
    settings: PathBuf,
    /// The curl config that holds the token, beside the settings file.
    curl: PathBuf,
    /// The body of the session's newest record. Held across each write of the record, so one
    /// session's writes never cross.
    body: tokio::sync::Mutex<Session>,
    /// When a hook or output last arrived, for the stuck timer.
    activity: watch::Sender<Instant>,
    /// When `SessionStart` first came, for typing the first prompt.
    started: watch::Sender<Option<Instant>>,
    /// What the session is for and doing, from its hooks' payloads.
    brief: Mutex<brief::Brief>,
    tasks: Mutex<Vec<AbortHandle>>,
}

impl Sessions {
    /// The sessions of machine `me`, whose settings files live under `data_dir`, started in
    /// `terminals` as `config` says.
    pub fn new(
        me: MachineId,
        store: Arc<Mutex<Store>>,
        terminals: Arc<Terminals>,
        data_dir: &Path,
        config: Config,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                me,
                store,
                terminals,
                dir: data_dir.join("sessions"),
                config,
                trust: Mutex::new(()),
                live: Mutex::default(),
                #[cfg(test)]
                fail_writes: std::sync::atomic::AtomicUsize::new(0),
                #[cfg(test)]
                fail_exit_reads: std::sync::atomic::AtomicUsize::new(0),
            }),
        }
    }

    /// Starts Claude Code with `model` in a new terminal in `worktree`, the worktree of
    /// `workspace`: the workspaces module owns where that is, so the caller passes it. The
    /// start finishes even when the caller stops waiting. A start that fails leaves no
    /// terminal, no settings file and no record.
    pub async fn start(
        &self,
        workspace: WorkspaceId,
        worktree: &Path,
        model: &str,
    ) -> Result<SessionId, SessionError> {
        self.start_with(workspace, worktree, model, None).await
    }

    /// [`Sessions::start`], with `prompt` as Claude Code's first prompt. It is typed into the
    /// terminal as a paste and Enter once `SessionStart` has come and the output shows Claude
    /// Code takes input ([`claude::TAKES_INPUT`]), so its hooks report it as any typed prompt
    /// and it is never on a command line. A blank prompt is no prompt.
    pub async fn start_with(
        &self,
        workspace: WorkspaceId,
        worktree: &Path,
        model: &str,
        prompt: Option<&str>,
    ) -> Result<SessionId, SessionError> {
        let inner = self.inner.clone();
        let (worktree, model) = (worktree.to_path_buf(), model.to_string());
        let prompt = prompt
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_owned);
        tokio::spawn(async move {
            inner
                .start(workspace, &worktree, &model, prompt.as_deref())
                .await
        })
        .await?
    }

    /// Takes up this machine's sessions that a daemon before this one started and that have
    /// not ended: call it once, after the terminals are reattached and before the hooks are
    /// served. A session whose terminal is gone or has exited is ended now, its record written
    /// as the exit makes it and its files removed. Any other is followed again as
    /// [`Sessions::start`] follows a new one, and its hooks are taken with the token in its curl
    /// config. A session whose curl config holds no token is still followed to its end, but its
    /// hooks stay refused. A store that cannot be read is logged, and no session is taken up.
    pub async fn adopt(&self) {
        if let Err(error) = self.inner.adopt().await {
            tracing::warn!(%error, "the sessions from before this daemon started were not taken up");
        }
    }

    /// `POST /hooks/{session}`, the endpoint a session's hooks post to.
    ///
    /// It must be served with `into_make_service_with_connect_info::<SocketAddr>()`: it
    /// refuses a caller that is not on loopback, and without the connect info every call is
    /// refused, as the extractor fails.
    pub fn router(&self) -> Router {
        Router::new()
            .route("/hooks/{session}", post(hooks::hook))
            .with_state(self.inner.clone())
    }

    /// What each of this daemon's sessions is for and doing, from its hooks since it started.
    pub fn briefs(&self) -> HashMap<SessionId, brief::Brief> {
        lock(&self.inner.live)
            .values()
            .map(|live| (live.id, lock(&live.brief).summary()))
            .collect()
    }

    /// Every step session `id` took since this daemon started, oldest first, or `None` when it
    /// is not one of this daemon's sessions.
    pub fn trail(&self, id: SessionId) -> Option<Vec<brief::Mark>> {
        let live = lock(&self.inner.live).get(&id).cloned()?;
        let trail = lock(&live.brief).trail.iter().cloned().collect();
        Some(trail)
    }
}

impl Drop for Sessions {
    /// Stops this daemon's follower and timer tasks. The terminals keep running.
    fn drop(&mut self) {
        for live in lock(&self.inner.live).values() {
            for task in lock(&live.tasks).drain(..) {
                task.abort();
            }
        }
    }
}

impl Inner {
    async fn start(
        self: Arc<Self>,
        workspace: WorkspaceId,
        worktree: &Path,
        model: &str,
        prompt: Option<&str>,
    ) -> Result<SessionId, SessionError> {
        let id = SessionId::random();
        let token = new_token();
        let (settings, curl) = self.files(id);
        let url = format!("http://127.0.0.1:{}/hooks/{id}", self.config.hook_port);
        let written = {
            let (settings, curl, token) = (settings.clone(), curl.clone(), token.clone());
            blocking(move || {
                claude::write_curl_config(&curl, &url, &token)?;
                claude::write_settings(&settings, &claude::settings_json(&curl))
            })
            .await
        };
        if let Err(error) = written.and_then(|r| r) {
            self.remove_files(&settings, &curl).await;
            return Err(error);
        }
        let trusted = {
            let (me, json, worktree) = (
                self.clone(),
                self.config.claude_json.clone(),
                worktree.to_path_buf(),
            );
            blocking(move || claude::mark_trusted_locked(&me.trust, &json, &worktree)).await
        };
        if let Err(error) = trusted.and_then(|r| r) {
            self.remove_files(&settings, &curl).await;
            return Err(error);
        }

        let (activity, _) = watch::channel(Instant::now());
        let live = Arc::new(Live {
            id,
            token,
            settings: settings.clone(),
            curl: curl.clone(),
            body: tokio::sync::Mutex::new(Session {
                id,
                workspace,
                terminal: TerminalId(uuid::Uuid::nil()),
                model: model.to_string(),
                state: SessionState::Starting,
            }),
            activity,
            started: watch::Sender::new(None),
            brief: Mutex::default(),
            tasks: Mutex::default(),
        });
        // Locked before the session can be found, and kept until its record is written: a hook
        // the child posts before `Terminals::start` returns waits here, and is applied on top
        // of `Starting`.
        let mut body = live.body.lock().await;
        lock(&self.live).insert(id, live.clone());

        let started = self.launch(&settings, workspace, worktree, model).await;
        let (terminal, floor) = match started {
            Ok(started) => started,
            Err(error) => {
                lock(&self.live).remove(&id);
                drop(body);
                self.remove_files(&settings, &curl).await;
                return Err(error);
            }
        };
        body.terminal = terminal;
        if let Err(error) = self.write(body.clone()).await {
            lock(&self.live).remove(&id);
            drop(body);
            let _ = self.terminals.close(terminal).await;
            self.remove_files(&settings, &curl).await;
            return Err(error);
        }
        drop(body);

        self.run(&live, terminal, floor, FirstPrompt::new(prompt));
        tracing::info!(session = %id, workspace = %workspace, model, "session started");
        Ok(id)
    }

    /// Starts following the live session's `terminal` from above `cursor`, and its stuck
    /// timer.
    fn run(
        self: &Arc<Self>,
        live: &Arc<Live>,
        terminal: TerminalId,
        cursor: u64,
        first: FirstPrompt,
    ) {
        let tasks = [
            tokio::spawn(follow(self.clone(), live.clone(), terminal, cursor, first))
                .abort_handle(),
            tokio::spawn(watch_for_stuck(self.clone(), live.clone())).abort_handle(),
        ];
        lock(&live.tasks).extend(tasks);
    }

    /// [`Sessions::adopt`]. The sessions and their terminals are read in one store call, with
    /// this machine's newest version as the cursor its followers read exits from, so an exit
    /// recorded after that call is above the cursor and is not missed.
    async fn adopt(self: &Arc<Self>) -> Result<(), SessionError> {
        let me = self.me;
        let (open, cursor) = self
            .on_store(move |store| {
                let cursor = store.versions()?.get(&me).copied().unwrap_or(0);
                let mut open = Vec::new();
                for record in store.since(me, 0)? {
                    let AnyRecord::Session(record) = record else {
                        continue;
                    };
                    if record.deleted || is_final(record.body.state) {
                        continue;
                    }
                    // The exit code, when the terminal is gone or has exited.
                    let exit = match store.terminal(record.body.terminal)? {
                        Some(t) if !t.deleted && t.body.state != TerminalState::Exited => None,
                        terminal => Some(terminal.and_then(|t| t.body.exit_code)),
                    };
                    open.push((record.body, exit));
                }
                Ok((open, cursor))
            })
            .await?;
        for (session, exit) in open {
            let (settings, curl) = self.files(session.id);
            match exit {
                Some(code) => {
                    let id = session.id;
                    let ended = Session {
                        state: next_state(session.state, Event::Exited(code)),
                        ..session
                    };
                    match self.write(ended).await {
                        Ok(()) => self.remove_files(&settings, &curl).await,
                        Err(error) => {
                            tracing::warn!(session = %id, %error, "the end of a session whose terminal is gone was not recorded");
                        }
                    }
                }
                None => self.take_up(session, settings, curl, cursor).await,
            }
        }
        Ok(())
    }

    /// Follows `session`, which a daemon before this one started and whose terminal still
    /// runs, from above `cursor`, and takes its hooks with the token in `curl`.
    async fn take_up(
        self: &Arc<Self>,
        session: Session,
        settings: PathBuf,
        curl: PathBuf,
        cursor: u64,
    ) {
        let (id, terminal) = (session.id, session.terminal);
        let token = {
            let curl = curl.clone();
            blocking(move || claude::curl_token(&curl))
                .await
                .ok()
                .flatten()
        };
        // No hook can present a token nobody has, so its hooks stay refused.
        let token = token.unwrap_or_else(|| {
            tracing::warn!(session = %id, "the session's curl config holds no token, so its hooks are refused; it is followed to its end");
            new_token()
        });
        let live = Arc::new(Live {
            id,
            token,
            settings,
            curl,
            body: tokio::sync::Mutex::new(session),
            activity: watch::Sender::new(Instant::now()),
            started: watch::Sender::new(None),
            brief: Mutex::default(),
            tasks: Mutex::default(),
        });
        lock(&self.live).insert(id, live.clone());
        self.run(&live, terminal, cursor, FirstPrompt::new(None));
        tracing::info!(session = %id, "session taken up from the daemon before");
    }

    /// The settings file and curl config of session `id`.
    fn files(&self, id: SessionId) -> (PathBuf, PathBuf) {
        (
            self.dir.join(format!("{id}.json")),
            self.dir.join(format!("{id}.curl")),
        )
    }

    /// Starts the terminal, and gives it with the highest version this machine has written
    /// before it, from which the terminal's later records are read.
    async fn launch(
        &self,
        settings: &Path,
        workspace: WorkspaceId,
        worktree: &Path,
        model: &str,
    ) -> Result<(TerminalId, u64), SessionError> {
        let me = self.me;
        let floor = self
            .on_store(move |store| Ok(store.versions()?.get(&me).copied().unwrap_or(0)))
            .await?;
        let mut argv = self.config.claude.clone();
        argv.extend([
            "--settings".into(),
            settings.to_string_lossy().into_owned(),
            "--model".into(),
            model.to_string(),
        ]);
        let env = [
            (
                "ANTHROPIC_BASE_URL".to_string(),
                self.config.gateway.base_url.clone(),
            ),
            (
                "ANTHROPIC_AUTH_TOKEN".to_string(),
                self.config.gateway.key.clone(),
            ),
        ];
        let terminal = self
            .terminals
            .start(workspace, worktree, &argv, &env)
            .await?;
        Ok((terminal, floor))
    }

    fn live(&self, id: SessionId) -> Option<Arc<Live>> {
        lock(&self.live).get(&id).cloned()
    }

    fn is_live(&self, live: &Arc<Live>) -> bool {
        self.live(live.id).is_some_and(|l| Arc::ptr_eq(&l, live))
    }

    /// Applies `event` to the session and gives its state after. The record is written when
    /// the state changed. An event for a session whose start failed does nothing.
    async fn event(&self, live: &Arc<Live>, event: Event) -> Result<SessionState, SessionError> {
        let mut body = live.body.lock().await;
        if !self.is_live(live) {
            return Ok(body.state);
        }
        if matches!(event, Event::Hook(Hook::SessionStart)) {
            live.started
                .send_if_modified(|at| at.replace(at.unwrap_or_else(Instant::now)).is_none());
        }
        if matches!(event, Event::Hook(_) | Event::Output) {
            live.activity.send_replace(Instant::now());
        }
        let next = next_state(body.state, event);
        if next != body.state {
            let changed = Session {
                state: next,
                ..body.clone()
            };
            self.write(changed.clone()).await?;
            *body = changed;
        }
        if is_final(next) {
            // Wakes the stuck timer, which ends.
            live.activity.send_replace(Instant::now());
            if matches!(event, Event::Exited(_)) {
                // The child is gone: nothing more can come from it, so a late hook is refused
                // like one for an unknown session.
                lock(&self.live).remove(&live.id);
                self.remove_files(&live.settings, &live.curl).await;
            }
        }
        Ok(next)
    }

    /// Writes `body` as this machine's newest record of its session, on the blocking pool so a
    /// busy store never stalls an async worker.
    async fn write(&self, body: Session) -> Result<(), SessionError> {
        #[cfg(test)]
        if take_one(&self.fail_writes) {
            return Err(busy().into());
        }
        let me = self.me;
        self.on_store(move |store| {
            let record = Record {
                owner: me,
                version: store.own_next_version()?,
                deleted: false,
                body,
            };
            store.apply(&AnyRecord::Session(record), me)?;
            Ok(())
        })
        .await
    }

    async fn on_store<T: Send + 'static>(
        &self,
        work: impl FnOnce(&mut Store) -> Result<T, StoreError> + Send + 'static,
    ) -> Result<T, SessionError> {
        let store = self.store.clone();
        Ok(blocking(move || work(&mut lock(&store))).await??)
    }

    /// Removes a session's settings file and curl config. Missing is fine, and nothing else can
    /// be done about a failure.
    async fn remove_files(&self, settings: &Path, curl: &Path) {
        let paths = [settings.to_path_buf(), curl.to_path_buf()];
        let _ = blocking(move || {
            for path in paths {
                let _ = std::fs::remove_file(path);
            }
        })
        .await;
    }

    /// The exit of `terminal`, once its record says it exited or was deleted: the code, when
    /// known. Reads only what is above `cursor`, and moves it.
    async fn exit_of(
        &self,
        terminal: TerminalId,
        cursor: &mut u64,
    ) -> Result<Option<Option<i32>>, SessionError> {
        #[cfg(test)]
        if take_one(&self.fail_exit_reads) {
            return Err(busy().into());
        }
        let (me, from) = (self.me, *cursor);
        let (newest, exit) = self
            .on_store(move |store| {
                let mut newest = from;
                let mut exit = None;
                for record in store.since(me, from)? {
                    newest = newest.max(record.version());
                    if let AnyRecord::Terminal(r) = record
                        && r.body.id == terminal
                        && (r.body.state == TerminalState::Exited || r.deleted)
                    {
                        exit = Some(r.body.exit_code);
                    }
                }
                Ok((newest, exit))
            })
            .await?;
        *cursor = newest;
        Ok(exit)
    }
}

/// Follows the session's terminal: each chunk of output is an [`Event::Output`], and the end of
/// the child is an [`Event::Exited`]. The first prompt is typed on the way, once it is ready.
async fn follow(
    inner: Arc<Inner>,
    live: Arc<Live>,
    terminal: TerminalId,
    mut cursor: u64,
    mut first: FirstPrompt,
) {
    let mut started = live.started.subscribe();
    let mut backoff = Backoff::new();
    loop {
        match inner.terminals.attach(terminal).await {
            // What the terminal already held is not new output, but it may show the sign.
            Ok((snapshot, mut output)) => {
                backoff.reset();
                first.attached(&snapshot.bytes);
                loop {
                    let at = *started.borrow_and_update();
                    if let Some(keys) = first.ready(at, Instant::now()) {
                        type_first(&inner, terminal, keys).await;
                    }
                    let grace = first.grace_ends(at);
                    let next = tokio::select! {
                        next = output.next() => next,
                        // `SessionStart` came, or the grace ran out: the prompt may be ready
                        // with no more output.
                        Ok(()) = started.changed(), if first.waiting() => continue,
                        () = tokio::time::sleep_until(grace.unwrap_or_else(Instant::now)),
                            if grace.is_some() => continue,
                    };
                    match next {
                        Some(Ok(bytes)) => {
                            first.read(&bytes, started.borrow().is_some());
                            if !bytes.is_empty() {
                                report(&inner, &live, Event::Output).await;
                            }
                        }
                        // The child exited.
                        None => {
                            first.give_up(terminal);
                            return exited(&inner, &live, terminal, &mut cursor).await;
                        }
                        Some(Err(error)) => {
                            retry(&mut backoff, terminal, error).await;
                            break;
                        }
                    }
                }
            }
            Err(error) => {
                if matches!(error, TerminalError::Gone(_) | TerminalError::Unknown(_)) {
                    first.give_up(terminal);
                    return exited(&inner, &live, terminal, &mut cursor).await;
                }
                retry(&mut backoff, terminal, error).await;
            }
        }
    }
}

/// A session's first prompt until it is typed. Claude Code takes it once the session has
/// started, which `SessionStart` says, and its input box takes keys, which turning bracketed
/// paste on says ([`claude::TAKES_INPUT`]). A screen before the session starts, such as the
/// trust prompt, turns bracketed paste on too, and the paste's Enter would answer it, so the
/// prompt is typed on a sign that comes after `SessionStart`. A sign seen only before it, which
/// is also what a hook that reaches the daemon after the box's sign looks like, counts
/// [`claude::EARLIER_SIGN_GRACE`] after `SessionStart`, by when the box is drawn. So does a
/// sign in what the terminal already held when the follower attached: nothing says when that
/// was written.
///
/// This decides and does no I/O: [`follow`] hands it the output and the time, and types the
/// keys [`FirstPrompt::ready`] gives. The prompt is held here only, so a daemon that restarts
/// before it is typed loses it.
struct FirstPrompt {
    /// The keys that type it, until they are given to be sent.
    keys: Option<Bytes>,
    /// Where the output has shown the sign, as far as the daemon has read.
    sign: Sign,
    /// The end of the output read since the follower attached, for a sign cut between reads.
    tail: Vec<u8>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Sign {
    None,
    BeforeStart,
    AfterStart,
}

impl FirstPrompt {
    fn new(prompt: Option<&str>) -> Self {
        Self {
            keys: prompt.map(|p| Bytes::from(claude::typed_prompt(p))),
            sign: Sign::None,
            tail: Vec::new(),
        }
    }

    fn waiting(&self) -> bool {
        self.keys.is_some()
    }

    /// The follower attached, and the terminal already held `snapshot`. A sign in it counts as
    /// one from before `SessionStart`, whenever the follower attaches: after the holder cut it
    /// off, the snapshot still holds an earlier screen's sign.
    fn attached(&mut self, snapshot: &[u8]) {
        self.tail.clear();
        self.read(snapshot, false);
    }

    /// Notes `output` as it arrives, when `started` says whether `SessionStart` has come.
    fn read(&mut self, output: &[u8], started: bool) {
        if !self.waiting() || self.sign == Sign::AfterStart {
            return;
        }
        if claude::turns_paste_on(&mut self.tail, output) {
            self.sign = if started {
                Sign::AfterStart
            } else {
                Sign::BeforeStart
            };
        }
    }

    /// The keys to type at `now`, when the session started at `started` and takes input. They
    /// are given once.
    fn ready(&mut self, started: Option<Instant>, now: Instant) -> Option<Bytes> {
        let started = started?;
        let ready = match self.sign {
            Sign::None => false,
            Sign::AfterStart => true,
            Sign::BeforeStart => now >= started + claude::EARLIER_SIGN_GRACE,
        };
        if ready { self.keys.take() } else { None }
    }

    /// When a sign from before `SessionStart` starts to count, while the prompt waits on that
    /// alone.
    fn grace_ends(&self, started: Option<Instant>) -> Option<Instant> {
        (self.waiting() && self.sign == Sign::BeforeStart)
            .then_some(started? + claude::EARLIER_SIGN_GRACE)
    }

    /// The child is gone: says so when its first prompt was never typed.
    fn give_up(&self, terminal: TerminalId) {
        if self.waiting() {
            tracing::warn!(
                terminal = %terminal,
                "the first prompt was not typed: Claude Code ended before it took input"
            );
        }
    }
}

/// Types the first prompt's keys. They are sent once: a prompt the holder does not take is
/// lost, and the session goes on without it.
async fn type_first(inner: &Inner, terminal: TerminalId, keys: Bytes) {
    if let Err(error) = inner.terminals.input(terminal, keys).await {
        tracing::warn!(terminal = %terminal, %error, "the first prompt was not typed");
    }
}

/// Waits before attaching again to a terminal that dropped the follower.
async fn retry(backoff: &mut Backoff, terminal: TerminalId, error: TerminalError) {
    match error {
        // The holder cut this client off. Attaching again is the answer.
        TerminalError::Detached(_) => backoff.reset(),
        other => {
            if backoff.first_failure() {
                tracing::warn!(terminal = %terminal, error = %other, "session output not followed; trying again");
            }
        }
    }
    tokio::time::sleep(backoff.next_wait()).await;
}

/// Waits for the terminal's exit to be recorded, then reports it.
async fn exited(inner: &Inner, live: &Arc<Live>, terminal: TerminalId, cursor: &mut u64) {
    // Until the exit is recorded, the store is asked again every `RETRY_AFTER`. A read that
    // fails waits longer each time.
    let mut backoff = Backoff::new();
    let code = loop {
        let wait = match inner.exit_of(terminal, cursor).await {
            Ok(Some(code)) => break code,
            Ok(None) => RETRY_AFTER,
            Err(error) => {
                if backoff.first_failure() {
                    tracing::warn!(terminal = %terminal, %error, "terminal exit not read; trying again");
                }
                backoff.next_wait()
            }
        };
        tokio::time::sleep(wait).await;
    };
    // The end is not given up on: a session left running after its child is gone is wrong for
    // good. It stops once the session is no longer live.
    let mut backoff = Backoff::new();
    while let Err(error) = inner.event(live, Event::Exited(code)).await {
        if !inner.is_live(live) {
            return;
        }
        if backoff.first_failure() {
            tracing::warn!(session = %live.id, %error, "session exit not recorded; trying again");
        }
        tokio::time::sleep(backoff.next_wait()).await;
    }
}

/// Applies `event`, logging a store error: nothing else can be done with it here.
async fn report(inner: &Inner, live: &Arc<Live>, event: Event) {
    if let Err(error) = inner.event(live, event).await {
        tracing::warn!(session = %live.id, %error, "session state not recorded");
    }
}

/// Turns a working session that is silent for [`STUCK_AFTER`] into a stuck one. Runs on the
/// tokio clock, so a paused clock fakes it, and ends once the session is final.
async fn watch_for_stuck(inner: Arc<Inner>, live: Arc<Live>) {
    let mut activity = live.activity.subscribe();
    loop {
        if is_final(live.body.lock().await.state) {
            return;
        }
        let last = *activity.borrow_and_update();
        tokio::select! {
            () = tokio::time::sleep_until(last + STUCK_AFTER) => {
                // Does nothing unless the session is working. A write that fails is tried
                // again until it holds or something arrives, which moves the count on.
                let (mut moved, mut backoff) = (false, Backoff::new());
                while let Err(error) = inner.event(&live, Event::Stalled).await {
                    if backoff.first_failure() {
                        tracing::warn!(session = %live.id, %error, "session stall not recorded; trying again");
                    }
                    tokio::select! {
                        () = tokio::time::sleep(backoff.next_wait()) => {}
                        _ = activity.changed() => {
                            moved = true;
                            break;
                        }
                    }
                }
                if moved {
                    continue;
                }
                if activity.changed().await.is_err() {
                    return;
                }
            }
            changed = activity.changed() => {
                if changed.is_err() {
                    return;
                }
            }
        }
    }
}

/// [`Sessions::adopt`] against a store that is busy for a while. The sessions are ones whose
/// terminal has exited, so no holder is needed.
#[cfg(test)]
mod adopt_tests {
    use std::sync::atomic::Ordering::SeqCst;

    use foundry_proto::record::Terminal;

    use super::*;

    /// Sessions of a new machine whose store holds one `Working` session, with its settings
    /// file and curl config, whose terminal exited with code 0.
    fn with_a_session_whose_terminal_exited() -> (tempfile::TempDir, Sessions, SessionId) {
        let data = tempfile::tempdir().unwrap();
        let me = MachineId::random();
        let mut store = Store::open(data.path().join("foundry.db")).unwrap();
        let (workspace, terminal, id) = (
            WorkspaceId::random(),
            TerminalId::random(),
            SessionId::random(),
        );
        let records = [
            AnyRecord::Terminal(Record {
                owner: me,
                version: store.own_next_version().unwrap(),
                deleted: false,
                body: Terminal {
                    id: terminal,
                    workspace,
                    state: TerminalState::Exited,
                    exit_code: Some(0),
                },
            }),
            AnyRecord::Session(Record {
                owner: me,
                version: store.own_next_version().unwrap(),
                deleted: false,
                body: Session {
                    id,
                    workspace,
                    terminal,
                    model: "test-model".into(),
                    state: SessionState::Working,
                },
            }),
        ];
        for record in &records {
            store.apply(record, me).unwrap();
        }
        let store = Arc::new(Mutex::new(store));
        let terminals = Arc::new(Terminals::new(
            me,
            store.clone(),
            data.path(),
            data.path().join("no-holder"),
        ));
        let config = Config {
            claude: vec!["claude".into()],
            claude_json: data.path().join(".claude.json"),
            hook_port: 9,
            gateway: Gateway {
                base_url: String::new(),
                key: String::new(),
            },
        };
        let sessions = Sessions::new(me, store, terminals, data.path(), config);
        let (settings, curl) = sessions.inner.files(id);
        claude::write_curl_config(&curl, "http://127.0.0.1:9/hooks/x", "t0ken").unwrap();
        claude::write_settings(&settings, &claude::settings_json(&curl)).unwrap();
        (data, sessions, id)
    }

    fn state(sessions: &Sessions, id: SessionId) -> Option<SessionState> {
        let me = sessions.inner.me;
        let records = lock(&sessions.inner.store).since(me, 0).unwrap();
        records.into_iter().find_map(|record| match record {
            AnyRecord::Session(r) if r.body.id == id => Some(r.body.state),
            _ => None,
        })
    }

    fn files_gone(sessions: &Sessions, id: SessionId) -> bool {
        let (settings, curl) = sessions.inner.files(id);
        !settings.exists() && !curl.exists()
    }

    #[tokio::test]
    async fn a_gone_session_whose_end_cannot_be_written_is_ended_once_the_store_answers() {
        let (_data, sessions, id) = with_a_session_whose_terminal_exited();
        sessions.inner.fail_writes.store(3, SeqCst);

        sessions.adopt().await;

        let end = Instant::now() + Duration::from_secs(20);
        while state(&sessions, id) != Some(SessionState::Finished) || !files_gone(&sessions, id) {
            assert!(Instant::now() < end, "the session was not ended");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(sessions.inner.fail_writes.load(SeqCst), 0);
    }

    #[tokio::test]
    async fn a_store_that_cannot_be_read_at_first_is_read_again_before_adopt_returns() {
        let (_data, sessions, id) = with_a_session_whose_terminal_exited();
        sessions.inner.fail_adopt_reads.store(2, SeqCst);

        sessions.adopt().await;

        assert_eq!(state(&sessions, id), Some(SessionState::Finished));
        assert!(files_gone(&sessions, id));
        assert_eq!(sessions.inner.fail_adopt_reads.load(SeqCst), 0);
    }
}

/// Tests: takes one from `count` when it is above zero, and says whether it did.
#[cfg(test)]
fn take_one(count: &std::sync::atomic::AtomicUsize) -> bool {
    use std::sync::atomic::Ordering::SeqCst;
    count
        .try_update(SeqCst, SeqCst, |n| n.checked_sub(1))
        .is_ok()
}

/// Tests: the error a busy store gives.
#[cfg(test)]
fn busy() -> StoreError {
    StoreError::Sqlite(rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
        None,
    ))
}

/// A new session's token: two random UUIDs, as hex.
fn new_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

fn is_final(state: SessionState) -> bool {
    matches!(state, SessionState::Finished | SessionState::Failed)
}

/// Runs `work` on the blocking pool, so file and store work never stalls an async worker.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, SessionError> {
    Ok(tokio::task::spawn_blocking(work).await?)
}

/// Locks `mutex`, taking the value a panicked holder of the lock left: every value kept under
/// these locks is whole between statements.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
