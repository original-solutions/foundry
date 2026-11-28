//! Starts holders, reattaches to them after a restart, and relays their bytes.
//!
//! Each terminal is a `foundry-holder` process serving a socket at
//! `<data dir>/terminals/<id>.sock` and holding a lock on `<id>.lock` beside it for as long as it
//! runs. A socket that refuses connections is a gone holder at once only when that lock is free:
//! on macOS a holder whose queue of new connections is full refuses too. With no lock file, as a
//! holder started before the lock was kept has, it is gone only after a few refusals. The holder outlives this daemon,
//! so a daemon that starts again finds its terminals through their records in the store and
//! their sockets, and attaches to them again with [`Terminals::reattach_all`].
//!
//! Each terminal's record is kept current: running once it is started, exited when the holder
//! reports the child's exit or when its socket is gone, and deleted once it is closed. A record
//! is written before its holder is started, so a daemon that stops in between leaves a record
//! that `reattach_all` finds with no socket and marks exited.

use std::collections::HashMap;
use std::io;
use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use foundry_holder::protocol::{self, ChildStatus, Reply, Request, UNKNOWN_EXIT};
use foundry_holder::pty;
use foundry_proto::ids::{MachineId, TerminalId, WorkspaceId};
use foundry_proto::record::{AnyRecord, Record, Terminal};
use foundry_proto::state::TerminalState;
use futures::StreamExt as _;
use futures::stream::BoxStream;
use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};
use tokio::net::UnixStream;
use tokio::task::AbortHandle;

use crate::store::{Store, StoreError};

/// How long the watcher waits before it attaches again to a holder that dropped it, and the
/// most it waits after repeated failures.
const RECONNECT_AFTER: Duration = Duration::from_millis(200);
const RECONNECT_AT_MOST: Duration = Duration::from_secs(30);

/// How long a holder has to answer a request before the request fails.
const ANSWER_WITHIN: Duration = Duration::from_secs(10);

/// How long a holder has to serve its socket before the process started for it is killed.
/// Unless a holder serves the socket by then, the start fails.
const START_WITHIN: Duration = Duration::from_secs(30);

/// How many times a refused connection is tried while the holder's lock is held or missing, and
/// how long apart. A running holder refuses only while its queue of new connections is full,
/// which empties in moments.
const BUSY_TRIES: u32 = 3;
const BUSY_RETRY: Duration = Duration::from_millis(100);

#[derive(Debug, thiserror::Error)]
pub enum TerminalError {
    #[error("terminal {0} is not a live terminal of this machine")]
    Unknown(TerminalId),
    #[error("terminal {id}: the holder could not be started: {reason}")]
    Start { id: TerminalId, reason: String },
    #[error("terminal {id}: the holder refused the request: {reason}")]
    Refused { id: TerminalId, reason: String },
    #[error(
        "terminal {0}: the holder ended this connection before the child exited, because the \
         client fell behind or the terminal was closed; attach again"
    )]
    Detached(TerminalId),
    #[error("terminal {0}: the holder is gone")]
    Gone(TerminalId),
    #[error("terminal {id}: {source}")]
    Holder {
        id: TerminalId,
        source: protocol::Error,
    },
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("terminals: a step run apart from its caller did not finish: {0}")]
    Interrupted(#[from] tokio::task::JoinError),
}

impl TerminalError {
    fn holder(id: TerminalId, source: impl Into<protocol::Error>) -> Self {
        Self::Holder {
            id,
            source: source.into(),
        }
    }
}

/// What a terminal holds when a client attaches: the kept screen and scrollback.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    /// The offset of the first byte of `bytes` in everything the child has written. The live
    /// bytes that follow start at `start + bytes.len()`, so a client that attaches again skips
    /// what it already has.
    pub start: u64,
    pub bytes: Bytes,
}

/// A terminal's live bytes after its [`Snapshot`]. It ends after the child exits. Any other end
/// is an error item first: [`TerminalError::Detached`] when the holder cut this client off.
pub type Output = BoxStream<'static, Result<Bytes, TerminalError>>;

/// This machine's terminals.
pub struct Terminals {
    inner: Arc<Inner>,
}

struct Inner {
    me: MachineId,
    store: Arc<Mutex<Store>>,
    dir: PathBuf,
    holder: PathBuf,
    live: Mutex<HashMap<TerminalId, Arc<Entry>>>,
}

/// A terminal whose record is not deleted: its holder runs, or it ended and the terminal is not
/// closed yet.
struct Entry {
    id: TerminalId,
    socket: PathBuf,
    /// The body of the terminal's newest record. Held across each write of the record, so one
    /// terminal's writes never cross.
    body: tokio::sync::Mutex<Terminal>,
    /// The connection input and resize are sent on, so they reach the holder in order.
    control: tokio::sync::Mutex<Option<UnixStream>>,
    watcher: Mutex<Option<AbortHandle>>,
}

impl Terminals {
    /// The terminals of machine `me`, whose sockets live under `data_dir`, started with the
    /// `foundry-holder` binary at `holder`. None is live until [`Self::reattach_all`].
    pub fn new(me: MachineId, store: Arc<Mutex<Store>>, data_dir: &Path, holder: PathBuf) -> Self {
        Self {
            inner: Arc::new(Inner {
                me,
                store,
                dir: data_dir.join("terminals"),
                holder,
                live: Mutex::default(),
            }),
        }
    }

    /// Starts `argv` in `cwd` with `env`, plus a colour `TERM` and `COLORTERM` each where `env`
    /// does not name it, set over the daemon's own environment, in a new terminal of
    /// `workspace`. The start finishes even when the caller stops waiting.
    pub async fn start(
        &self,
        workspace: WorkspaceId,
        cwd: &Path,
        argv: &[String],
        env: &[(String, String)],
    ) -> Result<TerminalId, TerminalError> {
        let inner = self.inner.clone();
        let (cwd, argv, env) = (cwd.to_path_buf(), argv.to_vec(), env.to_vec());
        tokio::spawn(async move { inner.start(workspace, &cwd, &argv, &env).await }).await?
    }

    /// The terminal's kept screen and scrollback, then its live bytes. A holder that does not
    /// send the snapshot within [`ANSWER_WITHIN`] fails the attach.
    pub async fn attach(&self, id: TerminalId) -> Result<(Snapshot, Output), TerminalError> {
        let entry = self.inner.entry(id)?;
        let mut stream = entry.connect().await?;
        let answered = tokio::time::timeout(ANSWER_WITHIN, async {
            protocol::write_request(&mut stream, &Request::Attach).await?;
            protocol::read_reply(&mut stream).await
        })
        .await;
        let snapshot = match answered {
            Ok(Ok(Some(Reply::Snapshot { start, bytes }))) => Snapshot { start, bytes },
            Ok(Ok(Some(Reply::Refused(reason)))) => {
                return Err(TerminalError::Refused { id, reason });
            }
            Ok(Ok(_)) => return Err(TerminalError::Detached(id)),
            Ok(Err(e)) => return Err(TerminalError::holder(id, e)),
            Err(_) => return Err(TerminalError::holder(id, no_answer())),
        };
        let output = futures::stream::unfold(Some(stream), move |stream| async move {
            let mut stream = stream?;
            loop {
                let item = match protocol::read_reply(&mut stream).await {
                    Ok(Some(Reply::Output(bytes))) => Ok(bytes),
                    Ok(Some(Reply::Status(ChildStatus::Running) | Reply::Snapshot { .. })) => {
                        continue;
                    }
                    // The holder sends every byte before it reports the exit.
                    Ok(Some(Reply::Status(ChildStatus::Exited(_)))) => return None,
                    Ok(Some(Reply::Refused(reason))) => Err(TerminalError::Refused { id, reason }),
                    Ok(None) => Err(TerminalError::Detached(id)),
                    Err(e) => Err(TerminalError::holder(id, e)),
                };
                let more = item.is_ok().then_some(stream);
                return Some((item, more));
            }
        });
        Ok((snapshot, output.boxed()))
    }

    /// The end of the terminal's kept scrollback and screen as plain text: the output as the
    /// child wrote it, with escape sequences and control characters removed (see [`plain_tail`]),
    /// at most `max_bytes` bytes, cut on a character boundary. It answers for a terminal whose
    /// child has exited as for a running one, since the holder keeps both until the close; a
    /// holder that is gone is [`TerminalError::Gone`].
    pub async fn tail(&self, id: TerminalId, max_bytes: usize) -> Result<String, TerminalError> {
        let (snapshot, _) = self.attach(id).await?;
        Ok(tokio::task::spawn_blocking(move || plain_tail(&snapshot.bytes, max_bytes)).await?)
    }

    /// Bytes typed into the terminal. Input the holder refuses, because the child has left too
    /// much unread, is [`TerminalError::Refused`]; none of it reaches the child. Input to a
    /// child that has exited is dropped with no error, as typing into a finished terminal is.
    pub async fn input(&self, id: TerminalId, bytes: Bytes) -> Result<(), TerminalError> {
        self.inner.entry(id)?.send(&Request::Input(bytes)).await
    }

    pub async fn resize(&self, id: TerminalId, cols: u16, rows: u16) -> Result<(), TerminalError> {
        self.inner
            .entry(id)?
            .send(&Request::Resize { cols, rows })
            .await
    }

    /// The machine whose terminals these are.
    pub fn me(&self) -> MachineId {
        self.inner.me
    }

    /// The folder that keeps the files sent to live terminal `id`, such as a screenshot pasted
    /// into a session, so a path to one can be typed in. It is not made here; [`Self::close`]
    /// removes it with the terminal.
    pub fn files(&self, id: TerminalId) -> Result<PathBuf, TerminalError> {
        let entry = self.inner.entry(id)?;
        Ok(self.inner.files(entry.id))
    }

    /// Runs `write` on live terminal `id`'s files folder (see [`Self::files`]) and gives back
    /// what it returns. A close waits for a write under way, then removes the folder with what
    /// was written; a write that comes after the close is refused, so it never makes the folder
    /// again with nothing left to remove it. The write finishes, and the close goes on waiting
    /// for it, even when the caller stops waiting.
    pub async fn keep_file<F>(
        &self,
        id: TerminalId,
        write: F,
    ) -> Result<io::Result<PathBuf>, TerminalError>
    where
        F: FnOnce(&Path) -> io::Result<PathBuf> + Send + 'static,
    {
        let inner = self.inner.clone();
        tokio::spawn(async move {
            let entry = inner.entry(id)?;
            // A close holds the body from before it lets the entry go until the folder is
            // removed.
            let _body = entry.body.lock().await;
            if !inner.is_live(&entry) {
                return Err(TerminalError::Unknown(id));
            }
            let folder = inner.files(id);
            Ok(tokio::task::spawn_blocking(move || write(&folder)).await?)
        })
        .await?
    }

    /// Ends the terminal: hangs up its child, waits for its holder to end and deletes its
    /// record. A holder that is already gone is no error. The close finishes even when the
    /// caller stops waiting.
    pub async fn close(&self, id: TerminalId) -> Result<(), TerminalError> {
        let inner = self.inner.clone();
        tokio::spawn(async move { inner.close(id).await }).await?
    }

    /// Makes live every terminal this machine's records name and that is not closed, and
    /// watches each holder that still runs. A running terminal whose socket is gone is
    /// recorded as exited with an unknown code. Run once at start, before any [`Self::start`]:
    /// it also removes env files a stopped daemon left behind.
    pub async fn reattach_all(&self) -> Result<(), TerminalError> {
        self.inner.remove_env_files().await?;
        let (store, me) = (self.inner.store.clone(), self.inner.me);
        let held = tokio::task::spawn_blocking(move || -> Result<Vec<Terminal>, StoreError> {
            let records = lock(&store).since(me, 0)?;
            Ok(records
                .into_iter()
                .filter_map(|record| match record {
                    AnyRecord::Terminal(r) if !r.deleted => Some(r.body),
                    _ => None,
                })
                .collect())
        })
        .await??;
        for body in held {
            if self.inner.entry(body.id).is_ok() {
                continue;
            }
            let socket = self.inner.socket(body.id);
            match connect(&socket).await {
                Err(Unreachable::Gone { .. }) => {
                    let body = if body.state == TerminalState::Running {
                        let ended = Terminal {
                            state: TerminalState::Exited,
                            exit_code: None,
                            ..body
                        };
                        self.inner.write(ended.clone(), false).await?;
                        ended
                    } else {
                        body
                    };
                    self.inner.insert(body, socket);
                }
                // A holder that answers, or may yet: the watcher finds out which.
                Ok(_) | Err(Unreachable::Failed(_)) => self.inner.adopt(body, socket),
            }
        }
        Ok(())
    }
}

impl Drop for Terminals {
    /// Stops watching. The holders keep running.
    fn drop(&mut self) {
        for entry in lock(&self.inner.live).values() {
            if let Some(watcher) = lock(&entry.watcher).take() {
                watcher.abort();
            }
        }
    }
}

impl Inner {
    async fn start(
        self: Arc<Self>,
        workspace: WorkspaceId,
        cwd: &Path,
        argv: &[String],
        env: &[(String, String)],
    ) -> Result<TerminalId, TerminalError> {
        let id = TerminalId::random();
        let body = Terminal {
            id,
            workspace,
            state: TerminalState::Running,
            exit_code: None,
        };
        // Written first, so a daemon that stops before the holder is up leaves a record that
        // `reattach_all` finds with no socket.
        self.write(body.clone(), false).await?;
        let socket = self.socket(id);
        let env_file = self.dir.join(format!("{id}.env"));
        let env = colour_env(env);
        let launched = self.launch(&socket, cwd, &env_file, argv, &env);
        let started = match tokio::time::timeout(START_WITHIN, launched).await {
            Ok(started) => started,
            // The process run is killed. A holder it forked that already serves the socket
            // is a started terminal; one that does not is a failed start.
            Err(_) if connect(&socket).await.is_ok() => Ok(()),
            Err(_) => Err(format!(
                "the holder did not start within {} seconds",
                START_WITHIN.as_secs()
            )),
        };
        // The holder deletes the env file once read. One it never read may hold secrets.
        let _ = tokio::fs::remove_file(&env_file).await;
        if let Err(reason) = started {
            let ended = Terminal {
                state: TerminalState::Exited,
                ..body.clone()
            };
            if let Err(error) = self.write(ended, true).await {
                // Its record still says running. It stays live for a close to delete, and its
                // watcher finds no socket and records the exit once the store takes it.
                tracing::warn!(terminal = %id, %error, "failed terminal start not recorded");
                self.adopt(body, socket);
            }
            return Err(TerminalError::Start { id, reason });
        }
        self.adopt(body, socket);
        Ok(id)
    }

    async fn close(self: Arc<Self>, id: TerminalId) -> Result<(), TerminalError> {
        let entry = self.entry(id)?;
        {
            // No input or resize goes to the holder while it is hung up.
            let _control = entry.control.lock().await;
            entry.hang_up().await?;
        }
        // Taken before the entry is let go, so an exit write already under way finishes first
        // and the deleted record is the newest. A write that fails leaves the terminal live, so
        // the close can be tried again.
        let mut body = entry.body.lock().await;
        if !self.is_live(&entry) {
            // Another close got here first.
            return Err(TerminalError::Unknown(id));
        }
        let ended = Terminal {
            state: TerminalState::Exited,
            ..body.clone()
        };
        self.write(ended.clone(), true).await?;
        *body = ended;
        lock(&self.live).remove(&id);
        if let Some(watcher) = lock(&entry.watcher).take() {
            watcher.abort();
        }
        let files = self.files(id);
        tokio::task::spawn_blocking(move || std::fs::remove_dir_all(files))
            .await?
            .ok();
        Ok(())
    }

    fn socket(&self, id: TerminalId) -> PathBuf {
        self.dir.join(format!("{id}.sock"))
    }

    fn files(&self, id: TerminalId) -> PathBuf {
        self.dir.join(format!("{id}.files"))
    }

    fn entry(&self, id: TerminalId) -> Result<Arc<Entry>, TerminalError> {
        lock(&self.live)
            .get(&id)
            .cloned()
            .ok_or(TerminalError::Unknown(id))
    }

    fn is_live(&self, entry: &Arc<Entry>) -> bool {
        lock(&self.live)
            .get(&entry.id)
            .is_some_and(|held| Arc::ptr_eq(held, entry))
    }

    /// Makes `body`'s terminal live with no watcher.
    fn insert(&self, body: Terminal, socket: PathBuf) -> Arc<Entry> {
        let entry = Arc::new(Entry {
            id: body.id,
            socket,
            body: tokio::sync::Mutex::new(body),
            control: tokio::sync::Mutex::new(None),
            watcher: Mutex::new(None),
        });
        lock(&self.live).insert(entry.id, entry.clone());
        entry
    }

    /// Makes `body`'s terminal live and watches its holder for the child's exit.
    fn adopt(self: &Arc<Self>, body: Terminal, socket: PathBuf) {
        let entry = self.insert(body, socket);
        let watcher = tokio::spawn(watch(self.clone(), entry.clone()));
        *lock(&entry.watcher) = Some(watcher.abort_handle());
    }

    /// Writes `body` as this machine's newest record of its terminal, on the blocking pool so a
    /// busy store never stalls an async worker.
    async fn write(&self, body: Terminal, deleted: bool) -> Result<(), TerminalError> {
        let (store, me) = (self.store.clone(), self.me);
        tokio::task::spawn_blocking(move || -> Result<(), StoreError> {
            let mut store = lock(&store);
            let record = Record {
                owner: me,
                version: store.own_next_version()?,
                deleted,
                body,
            };
            store.apply(&AnyRecord::Terminal(record), me)?;
            Ok(())
        })
        .await??;
        Ok(())
    }

    /// Records that a live terminal's child has exited, once. A terminal closed meanwhile keeps
    /// its deleted record.
    async fn exited(&self, entry: &Arc<Entry>, code: Option<i32>) -> Result<(), TerminalError> {
        let mut body = entry.body.lock().await;
        if body.state == TerminalState::Exited || !self.is_live(entry) {
            return Ok(());
        }
        let ended = Terminal {
            state: TerminalState::Exited,
            exit_code: code,
            ..body.clone()
        };
        self.write(ended.clone(), false).await?;
        *body = ended;
        Ok(())
    }

    /// Removes every env file in the terminals directory. Only a `start` in progress has one.
    /// Runs on the blocking pool, as every other file step here does.
    async fn remove_env_files(&self) -> Result<(), TerminalError> {
        let dir = self.dir.clone();
        tokio::task::spawn_blocking(move || {
            let Ok(files) = std::fs::read_dir(dir) else {
                return;
            };
            for file in files.flatten() {
                if file.path().extension().is_some_and(|e| e == "env") {
                    let _ = std::fs::remove_file(file.path());
                }
            }
        })
        .await?;
        Ok(())
    }

    /// Runs the holder, which returns once its socket is served. Dropping this kills the process
    /// it runs. The error is the holder's own reason, which never quotes the env file's contents.
    async fn launch(
        &self,
        socket: &Path,
        cwd: &Path,
        env_file: &Path,
        argv: &[String],
        env: &[(String, String)],
    ) -> Result<(), String> {
        let (dir, file, vars) = (self.dir.clone(), env_file.to_path_buf(), env.to_vec());
        tokio::task::spawn_blocking(move || {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&dir)
                .map_err(|e| format!("{}: {e}", dir.display()))?;
            pty::write_env_file(&file, &vars).map_err(|e| format!("env file: {e}"))
        })
        .await
        .map_err(|e| e.to_string())??;
        let output = tokio::process::Command::new(&self.holder)
            .arg("--socket")
            .arg(socket)
            .arg("--cwd")
            .arg(cwd)
            .arg("--env-file")
            .arg(env_file)
            .arg("--")
            .args(argv)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| format!("cannot run {}: {e}", self.holder.display()))?;
        if output.status.success() {
            return Ok(());
        }
        let said = String::from_utf8_lossy(&output.stderr);
        let said = said.trim();
        Err(match said.strip_prefix("foundry-holder: ") {
            Some(reason) => reason.to_string(),
            None if said.is_empty() => format!("the holder exited with {}", output.status),
            None => said.to_string(),
        })
    }
}

impl Entry {
    async fn connect(&self) -> Result<UnixStream, TerminalError> {
        match connect(&self.socket).await {
            Ok(stream) => Ok(stream),
            Err(Unreachable::Gone { .. }) => Err(TerminalError::Gone(self.id)),
            Err(Unreachable::Failed(e)) => Err(TerminalError::holder(self.id, e)),
        }
    }

    /// Sends `request` on the control connection, then asks for the status, so a request the
    /// holder refuses is an error here rather than lost. A connection that fails is dropped,
    /// and the next request opens a new one.
    async fn send(&self, request: &Request) -> Result<(), TerminalError> {
        let mut control = self.control.lock().await;
        let mut stream = match control.take() {
            Some(stream) => stream,
            None => self.connect().await?,
        };
        self.ask(&mut stream, &[request, &Request::Status], Answer::Status)
            .await?;
        *control = Some(stream);
        Ok(())
    }

    /// Sends `Close` and waits for the holder to end the connection as it exits. A holder that
    /// is gone is done; a stale socket it left is removed, with its lock file, which `connect`
    /// found free.
    async fn hang_up(&self) -> Result<(), TerminalError> {
        let mut stream = match connect(&self.socket).await {
            Ok(stream) => stream,
            Err(Unreachable::Gone { stale }) => {
                if stale {
                    let _ = tokio::fs::remove_file(&self.socket).await;
                    let _ = tokio::fs::remove_file(lock_file(&self.socket)).await;
                }
                return Ok(());
            }
            Err(Unreachable::Failed(e)) => return Err(TerminalError::holder(self.id, e)),
        };
        self.ask(&mut stream, &[&Request::Close], Answer::End).await
    }

    /// Writes `requests` and reads until the holder gives `answer`. A refusal, a broken
    /// connection or no answer within [`ANSWER_WITHIN`] is an error.
    async fn ask(
        &self,
        stream: &mut UnixStream,
        requests: &[&Request],
        answer: Answer,
    ) -> Result<(), TerminalError> {
        let answered = tokio::time::timeout(ANSWER_WITHIN, async {
            for request in requests {
                protocol::write_request(stream, request).await?;
            }
            loop {
                match (protocol::read_reply(stream).await?, answer) {
                    (Some(Reply::Refused(reason)), _) => return Ok(Some(reason)),
                    (Some(Reply::Status(_)), Answer::Status) | (None, Answer::End) => {
                        return Ok(None);
                    }
                    (Some(_), _) => {}
                    (None, Answer::Status) => {
                        return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into());
                    }
                }
            }
        })
        .await;
        match answered {
            Ok(Ok(None)) => Ok(()),
            Ok(Ok(Some(reason))) => Err(TerminalError::Refused {
                id: self.id,
                reason,
            }),
            Ok(Err(source)) => Err(TerminalError::Holder {
                id: self.id,
                source,
            }),
            Err(_) => Err(TerminalError::holder(self.id, no_answer())),
        }
    }
}

/// What ends a request to a holder.
#[derive(Clone, Copy)]
enum Answer {
    /// A status reply.
    Status,
    /// The holder ending the connection.
    End,
}

fn no_answer() -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        format!(
            "the holder did not answer within {} seconds",
            ANSWER_WITHIN.as_secs()
        ),
    )
}

/// `env` with each of a colour terminal's `TERM` and `COLORTERM` that it does not name. The
/// daemon under launchd has no `TERM`, and the browser side, xterm.js, shows truecolor.
fn colour_env(env: &[(String, String)]) -> Vec<(String, String)> {
    [("TERM", "xterm-256color"), ("COLORTERM", "truecolor")]
        .into_iter()
        .filter(|(name, _)| !env.iter().any(|(named, _)| named == name))
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .chain(env.iter().cloned())
        .collect()
}

/// Why a holder's socket could not be connected to.
enum Unreachable {
    /// No holder serves it: the socket is missing, or `stale`, a file that refuses connections
    /// and whose lock no holder holds, or that has no lock file and refused every try.
    Gone { stale: bool },
    /// Something else, which may pass, such as a busy holder that refuses.
    Failed(io::Error),
}

/// Connects to the holder serving `socket`. A refusal while the holder's lock is held is a busy
/// holder, tried [`BUSY_TRIES`] times in all. A free lock makes a refusal a gone holder at once;
/// with no lock file, a refusal is gone only once every try is refused.
async fn connect(socket: &Path) -> Result<UnixStream, Unreachable> {
    let mut tries = 1;
    loop {
        let e = match UnixStream::connect(socket).await {
            Ok(stream) => return Ok(stream),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(Unreachable::Gone { stale: false });
            }
            Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => e,
            Err(e) => return Err(Unreachable::Failed(e)),
        };
        let lock = lock_file(socket);
        match tokio::task::spawn_blocking(move || holder_lock(&lock)).await {
            Ok(Ok(HolderLock::Free)) => return Err(Unreachable::Gone { stale: true }),
            Ok(Ok(HolderLock::Held | HolderLock::Missing)) if tries < BUSY_TRIES => {
                tries += 1;
                tokio::time::sleep(BUSY_RETRY).await;
            }
            Ok(Ok(HolderLock::Missing)) => return Err(Unreachable::Gone { stale: true }),
            Ok(Ok(HolderLock::Held)) => {
                return Err(Unreachable::Failed(io::Error::new(
                    e.kind(),
                    "the holder refused the connection while it runs: it is busy",
                )));
            }
            Ok(Err(e)) => return Err(Unreachable::Failed(e)),
            Err(e) => return Err(Unreachable::Failed(io::Error::other(e))),
        }
    }
}

/// The file the holder serving `socket` keeps locked for as long as it runs.
fn lock_file(socket: &Path) -> PathBuf {
    socket.with_extension("lock")
}

/// What a holder's lock file says about it.
#[derive(Debug, PartialEq)]
enum HolderLock {
    /// A holder runs and holds it.
    Held,
    /// No holder holds it.
    Free,
    /// There is no lock file, as for a holder started before holders kept one.
    Missing,
}

/// Reads the lock at `path` without waiting. The check asks for the lock shared, which only a
/// holder's exclusive lock refuses, so two checks at once never see each other.
fn holder_lock(path: &Path) -> io::Result<HolderLock> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(HolderLock::Missing),
        Err(e) => return Err(e),
    };
    match Flock::lock(file, FlockArg::LockSharedNonblock) {
        Ok(_free) => Ok(HolderLock::Free),
        Err((_, Errno::EWOULDBLOCK)) => Ok(HolderLock::Held),
        Err((_, e)) => Err(e.into()),
    }
}

/// What one attached connection to a holder ended with.
enum Followed {
    Exited(Option<i32>),
    /// The holder ended the connection with the child still running: it cut this client off.
    Ended,
    Trouble(String),
}

/// Stays attached to a terminal's holder until it reports the child's exit or its socket is
/// gone, and records which. A holder that drops the connection is attached to again; one that
/// refuses or cannot be reached is tried again less and less often, and said so once. So is an
/// exit the store could not take.
async fn watch(inner: Arc<Inner>, entry: Arc<Entry>) {
    let mut wait = RECONNECT_AFTER;
    let mut warned = false;
    let mut exit = None;
    loop {
        if let Some(code) = exit {
            let Err(error) = inner.exited(&entry, code).await else {
                return;
            };
            if !warned {
                tracing::warn!(terminal = %entry.id, %error, "terminal exit not recorded yet; trying again");
                warned = true;
            }
            wait = (wait * 2).min(RECONNECT_AT_MOST);
            tokio::time::sleep(wait).await;
            continue;
        }
        let trouble = match connect(&entry.socket).await {
            Err(Unreachable::Gone { .. }) => {
                exit = Some(None);
                continue;
            }
            Err(Unreachable::Failed(e)) => Some(e.to_string()),
            Ok(stream) => match follow(stream).await {
                Followed::Exited(code) => {
                    exit = Some(code);
                    continue;
                }
                Followed::Ended => None,
                Followed::Trouble(trouble) => Some(trouble),
            },
        };
        match trouble {
            None => wait = RECONNECT_AFTER,
            Some(trouble) => {
                if !warned {
                    tracing::warn!(terminal = %entry.id, %trouble, "terminal holder not watched; trying again");
                    warned = true;
                }
                wait = (wait * 2).min(RECONNECT_AT_MOST);
            }
        }
        tokio::time::sleep(wait).await;
    }
}

async fn follow(mut stream: UnixStream) -> Followed {
    if let Err(e) = protocol::write_request(&mut stream, &Request::Attach).await {
        return Followed::Trouble(e.to_string());
    }
    loop {
        match protocol::read_reply(&mut stream).await {
            Ok(Some(Reply::Status(ChildStatus::Exited(code)))) => {
                return Followed::Exited((code != UNKNOWN_EXIT).then_some(code));
            }
            Ok(Some(Reply::Refused(reason))) => return Followed::Trouble(reason),
            Ok(Some(_)) => {}
            Ok(None) => return Followed::Ended,
            Err(e) => return Followed::Trouble(e.to_string()),
        }
    }
}

/// Locks `mutex`, taking the value a panicked holder of the lock left: every value kept under
/// these locks is whole between statements, and SQLite rolls back a transaction a panic cut.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Where [`plain_tail`] is in an escape sequence.
#[derive(Clone, Copy)]
enum Escape {
    /// In text.
    None,
    /// After `ESC`.
    Esc,
    /// After `ESC` and one or more intermediate bytes, until a final byte.
    EscIntermediate,
    /// In a control sequence (`ESC [` or C1 CSI), until a final byte.
    Csi,
    /// In a string (OSC, DCS, SOS, PM or APC), until BEL, ST or a newline. `ESC` in a string
    /// starts a sequence, and `ESC \`, ST, is one.
    Str,
}

/// The terminal output `bytes` as plain text, then its last `max_bytes` bytes cut forward to a
/// character boundary. Bytes that are not UTF-8 become U+FFFD. Escape sequences, NUL and every
/// other control character but tab and newline are removed, carriage returns among them. Nothing
/// is rendered: what a sequence or a carriage return would have moved or erased on a screen is
/// kept, so a line redrawn in place gives each of its versions.
///
/// A string sequence, such as a window title, also ends at a newline, so one left unterminated
/// cannot hide the output after it.
fn plain_tail(bytes: &[u8], max_bytes: usize) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::with_capacity(text.len());
    let mut state = Escape::None;
    for c in text.chars() {
        // A byte that cannot continue the sequence it is in ends that sequence and is read
        // again as text.
        state = match (state, c) {
            (Escape::Esc, '[') => Escape::Csi,
            (Escape::Esc, ']' | 'P' | 'X' | '^' | '_') => Escape::Str,
            (Escape::Esc | Escape::EscIntermediate, ' '..='/') => Escape::EscIntermediate,
            (Escape::Esc | Escape::EscIntermediate, '0'..='~') => Escape::None,
            (Escape::Csi, ' '..='?') => Escape::Csi,
            (Escape::Csi, '@'..='~') => Escape::None,
            (Escape::Str, '\u{7}' | '\u{9c}') => Escape::None,
            (Escape::Str, c) if c != '\n' && c != '\u{1b}' => Escape::Str,
            (_, '\u{1b}') => Escape::Esc,
            (_, '\u{9b}') => Escape::Csi,
            (_, '\u{90}' | '\u{98}' | '\u{9d}' | '\u{9e}' | '\u{9f}') => Escape::Str,
            (_, c) if c.is_control() && c != '\t' && c != '\n' => Escape::None,
            (_, c) => {
                out.push(c);
                Escape::None
            }
        };
    }
    let mut at = out.len().saturating_sub(max_bytes);
    while !out.is_char_boundary(at) {
        at += 1;
    }
    out.split_off(at)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;
    use std::time::{Duration, Instant};

    use foundry_holder::protocol::MAX_PENDING_INPUT;
    use foundry_proto::record::{AnyRecord, Record};
    use foundry_proto::state::TerminalState;
    use tempfile::TempDir;

    use super::*;

    const WAIT: Duration = Duration::from_secs(20);

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

    /// A data directory and a working directory. Dropping it closes every holder left under
    /// the data directory, so no test leaves a process behind.
    struct Setup {
        data: TempDir,
        cwd: TempDir,
        me: MachineId,
    }

    impl Setup {
        fn new() -> Self {
            // A Unix socket path is at most about 100 bytes, which the system's own temporary
            // folder on macOS would take most of.
            let data = tempfile::Builder::new()
                .prefix("ft")
                .tempdir_in("/tmp")
                .unwrap();
            Setup {
                data,
                cwd: tempfile::tempdir().unwrap(),
                me: MachineId::random(),
            }
        }

        /// A daemon's terminals over its own connection to the store, as after a restart.
        fn daemon(&self) -> (Terminals, Arc<Mutex<Store>>) {
            self.daemon_with(holder())
        }

        fn daemon_with(&self, holder: PathBuf) -> (Terminals, Arc<Mutex<Store>>) {
            let store = Arc::new(Mutex::new(
                Store::open(self.data.path().join("foundry.db")).unwrap(),
            ));
            let terminals = Terminals::new(self.me, store.clone(), self.data.path(), holder);
            (terminals, store)
        }

        fn dir(&self) -> PathBuf {
            self.data.path().join("terminals")
        }

        fn socket(&self, id: TerminalId) -> PathBuf {
            self.dir().join(format!("{id}.sock"))
        }

        fn left(&self) -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(self.dir())
                .unwrap()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        }

        async fn start(&self, terminals: &Terminals, script: &str) -> TerminalId {
            terminals
                .start(WorkspaceId::random(), self.cwd.path(), &argv(script), &[])
                .await
                .unwrap()
        }

        /// Ends a holder behind the daemon's back and waits for its socket to go.
        async fn end_holder(&self, id: TerminalId) {
            let mut stream = UnixStream::connect(self.socket(id)).await.unwrap();
            protocol::write_request(&mut stream, &Request::Close)
                .await
                .unwrap();
            until("the holder's socket to go", || !self.socket(id).exists()).await;
        }
    }

    impl Drop for Setup {
        fn drop(&mut self) {
            use std::io::Write as _;
            // A test that made the data directory read-only gives it back, so it can be removed.
            set_mode(self.data.path(), 0o700);
            let Ok(entries) = std::fs::read_dir(self.dir()) else {
                return;
            };
            for entry in entries.flatten() {
                if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(entry.path()) {
                    let _ = stream.write_all(&Request::Close.encode());
                }
            }
        }
    }

    fn set_mode(path: &Path, mode: u32) {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }

    /// Writes a running record for a terminal no `Terminals` started, as a holder from an
    /// earlier daemon would have.
    fn put_running(store: &Mutex<Store>, me: MachineId) -> TerminalId {
        let id = TerminalId::random();
        let mut store = store.lock().unwrap();
        let record = Record {
            owner: me,
            version: store.own_next_version().unwrap(),
            deleted: false,
            body: Terminal {
                id,
                workspace: WorkspaceId::random(),
                state: TerminalState::Running,
                exit_code: None,
            },
        };
        store.apply(&AnyRecord::Terminal(record), me).unwrap();
        id
    }

    /// A stand-in holder serving `socket`. Each connection that sends a request gets the next
    /// of `answers`: its replies, then the connection is closed when the flag says so. Once
    /// the answers run out, connections are held open and never answered. A connection that
    /// sends nothing, such as a probe, takes no answer.
    fn stand_in(socket: PathBuf, answers: Vec<(Vec<Reply>, bool)>) -> tokio::task::JoinHandle<()> {
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            let mut answers = answers.into_iter();
            let mut held = Vec::new();
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                if !matches!(protocol::read_request(&mut stream).await, Ok(Some(_))) {
                    continue;
                }
                let Some((replies, close)) = answers.next() else {
                    held.push(stream);
                    continue;
                };
                for reply in replies {
                    tokio::io::AsyncWriteExt::write_all(&mut stream, &reply.encode())
                        .await
                        .unwrap();
                }
                if !close {
                    held.push(stream);
                }
            }
        })
    }

    fn argv(script: &str) -> Vec<String> {
        vec!["/bin/sh".into(), "-c".into(), script.into()]
    }

    /// The newest record held for terminal `id`.
    fn held(store: &Mutex<Store>, me: MachineId, id: TerminalId) -> Option<Record<Terminal>> {
        let store = store.lock().unwrap();
        store
            .since(me, 0)
            .unwrap()
            .into_iter()
            .filter_map(|record| match record {
                AnyRecord::Terminal(r) if r.body.id == id => Some(r),
                _ => None,
            })
            .next_back()
    }

    async fn until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + WAIT;
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Reads the stream into `seen` until `done` holds for what has been seen.
    async fn read_until(stream: &mut Output, seen: &mut Vec<u8>, done: impl Fn(&[u8]) -> bool) {
        let deadline = Instant::now() + WAIT;
        while !done(seen) {
            let left = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(left, stream.next()).await {
                Ok(Some(Ok(bytes))) => seen.extend_from_slice(&bytes),
                Ok(Some(Err(e))) => panic!("{e}; seen: {:?}", String::from_utf8_lossy(seen)),
                Ok(None) => panic!(
                    "the stream ended; seen: {:?}",
                    String::from_utf8_lossy(seen)
                ),
                Err(_) => panic!(
                    "timed out reading; seen: {:?}",
                    String::from_utf8_lossy(seen)
                ),
            }
        }
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    /// The numbers on every complete line.
    fn counts(bytes: &[u8]) -> Vec<u32> {
        let text = String::from_utf8_lossy(bytes);
        let complete = &text[..text.rfind('\n').map_or(0, |at| at + 1)];
        complete
            .lines()
            .map(|line| line.trim_end_matches('\r'))
            .filter(|line| !line.is_empty())
            .map(|line| {
                line.parse()
                    .unwrap_or_else(|_| panic!("not a count: {line:?}"))
            })
            .collect()
    }

    const COUNT_TO: u32 = 300;

    /// Prints 0 to `COUNT_TO - 1`, one a line, pausing after every tenth, then waits.
    fn counter() -> Vec<String> {
        argv(&format!(
            "i=0; while [ $i -lt {COUNT_TO} ]; do echo $i; i=$((i+1)); [ $((i % 10)) -eq 0 ] && sleep 0.02; done; sleep 60"
        ))
    }

    #[test]
    fn a_new_daemon_task_continues_the_stream_with_no_byte_lost_or_repeated() {
        let setup = Setup::new();

        // The first daemon starts the counter and reads part of it. Dropping its runtime drops
        // every task it had, as a daemon that stops does.
        let first = tokio::runtime::Runtime::new().unwrap();
        let (id, start, mut seen) = first.block_on(async {
            let (terminals, _store) = setup.daemon();
            let id = terminals
                .start(WorkspaceId::random(), setup.cwd.path(), &counter(), &[])
                .await
                .unwrap();
            let (snapshot, mut stream) = terminals.attach(id).await.unwrap();
            let mut seen = snapshot.bytes.to_vec();
            read_until(&mut stream, &mut seen, |seen| counts(seen).len() >= 30).await;
            (id, snapshot.start, seen)
        });
        drop(first);
        assert_eq!(
            start, 0,
            "a new terminal's snapshot starts at its first byte"
        );
        let first_count = counts(&seen).len() as u32;
        assert!(first_count < COUNT_TO, "the counter must still be running");

        // The counter keeps writing with no daemon attached.
        std::thread::sleep(Duration::from_millis(300));

        let second = tokio::runtime::Runtime::new().unwrap();
        second.block_on(async {
            let (terminals, store) = setup.daemon();
            terminals.reattach_all().await.unwrap();
            let record = held(&store, setup.me, id).unwrap();
            assert_eq!(record.body.state, TerminalState::Running);

            let (snapshot, mut stream) = terminals.attach(id).await.unwrap();
            let had = start + seen.len() as u64;
            assert!(
                snapshot.start <= had,
                "the snapshot starts at {} after the {had} bytes already seen",
                snapshot.start
            );
            let skip = usize::try_from(had - snapshot.start).unwrap();
            assert!(
                skip <= snapshot.bytes.len(),
                "the snapshot ends before the bytes seen"
            );
            seen.extend_from_slice(&snapshot.bytes[skip..]);
            let last = format!("{}\r\n", COUNT_TO - 1);
            read_until(&mut stream, &mut seen, |seen| {
                contains(seen, last.as_bytes())
            })
            .await;
            assert_eq!(counts(&seen), (0..COUNT_TO).collect::<Vec<_>>());
            terminals.close(id).await.unwrap();
        });
    }

    /// Once the scrollback has dropped its oldest lines, the snapshot's start is the offset of
    /// its first kept byte, so a client can line it up with what it already has.
    #[tokio::test]
    async fn a_snapshot_after_the_scrollback_dropped_lines_starts_at_its_first_kept_byte() {
        let setup = Setup::new();
        let (terminals, _) = setup.daemon();
        let last = 10_499;
        // The child waits for a line, so the first attach sees its output from the start.
        let id = setup
            .start(
                &terminals,
                &format!("stty -echo; read go; seq 0 {last}; sleep 60"),
            )
            .await;
        let (snapshot, mut stream) = terminals.attach(id).await.unwrap();
        assert_eq!(snapshot.start, 0);
        let mut seen = snapshot.bytes.to_vec();
        terminals
            .input(id, Bytes::from_static(b"\n"))
            .await
            .unwrap();
        let tail = format!("\n{last}\r\n");
        read_until(&mut stream, &mut seen, |seen| {
            contains(seen, tail.as_bytes())
        })
        .await;

        let (snapshot, _) = terminals.attach(id).await.unwrap();
        let kept = counts(&snapshot.bytes);
        let first = kept[0];
        assert!(first > 0, "the scrollback kept every line");
        assert_eq!(kept, (first..=last).collect::<Vec<_>>());
        let start = usize::try_from(snapshot.start).unwrap();
        assert_eq!(
            seen.get(start..start + snapshot.bytes.len()),
            Some(&snapshot.bytes[..]),
            "the snapshot is the stream's bytes from its start"
        );
        terminals.close(id).await.unwrap();
    }

    #[tokio::test]
    async fn a_terminal_record_follows_running_to_exited_with_its_code() {
        let setup = Setup::new();
        let (terminals, store) = setup.daemon();
        let workspace = WorkspaceId::random();
        let id = terminals
            .start(
                workspace,
                setup.cwd.path(),
                &argv("echo last words; sleep 0.5; exit 3"),
                &[],
            )
            .await
            .unwrap();
        let running = held(&store, setup.me, id).unwrap();
        assert_eq!(running.owner, setup.me);
        assert!(!running.deleted);
        assert_eq!(
            running.body,
            Terminal {
                id,
                workspace,
                state: TerminalState::Running,
                exit_code: None,
            }
        );

        let (_, stream) = terminals.attach(id).await.unwrap();
        until("the record to say exited", || {
            held(&store, setup.me, id).unwrap().body.state == TerminalState::Exited
        })
        .await;
        let exited = held(&store, setup.me, id).unwrap();
        assert_eq!(exited.body.exit_code, Some(3));
        assert!(exited.version > running.version);
        assert!(
            !exited.deleted,
            "an exited terminal is kept until it is closed"
        );
        let rest: Vec<_> = tokio::time::timeout(WAIT, stream.collect())
            .await
            .expect("the stream ends once the child has exited");
        assert!(
            rest.iter().all(Result::is_ok),
            "an exit ends the stream with no error: {rest:?}"
        );

        // A daemon started again still shows the final screen, and writes no new record.
        drop(terminals);
        let (terminals, store) = setup.daemon();
        terminals.reattach_all().await.unwrap();
        let (snapshot, _) = terminals.attach(id).await.unwrap();
        assert!(contains(&snapshot.bytes, b"last words"));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(held(&store, setup.me, id).unwrap(), exited);

        // Its holder gone too, a daemon started again keeps the exit code it had.
        drop(terminals);
        setup.end_holder(id).await;
        let (terminals, store) = setup.daemon();
        terminals.reattach_all().await.unwrap();
        assert_eq!(held(&store, setup.me, id).unwrap(), exited);
        terminals.close(id).await.unwrap();
        assert!(held(&store, setup.me, id).unwrap().deleted);
    }

    #[tokio::test]
    async fn a_holder_whose_socket_is_gone_at_restart_is_recorded_exited_and_can_be_closed() {
        let setup = Setup::new();
        let (terminals, store) = setup.daemon();
        let gone = setup.start(&terminals, "sleep 60").await;
        let stale = setup.start(&terminals, "sleep 60").await;
        drop(terminals);

        // While no daemon runs, one holder ends and takes its socket with it, and the other
        // dies without removing its socket, which no one is then listening on.
        setup.end_holder(gone).await;
        setup.end_holder(stale).await;
        drop(std::os::unix::net::UnixListener::bind(setup.socket(stale)).unwrap());
        assert!(setup.socket(stale).exists());

        let (terminals, _) = setup.daemon();
        terminals.reattach_all().await.unwrap();
        for id in [gone, stale] {
            let record = held(&store, setup.me, id).unwrap();
            assert_eq!(record.body.state, TerminalState::Exited, "{id}");
            assert_eq!(record.body.exit_code, None, "{id}");
            assert!(!record.deleted, "{id}");
        }
        for id in [gone, stale] {
            terminals.close(id).await.unwrap();
            assert!(held(&store, setup.me, id).unwrap().deleted, "{id}");
        }
        assert!(!setup.socket(stale).exists(), "the stale socket is removed");
    }

    /// A running record whose holder left a socket nothing listens on, with a lock file beside
    /// it that no one holds. Gives the terminal, its socket and its lock file.
    fn refusing_terminal(setup: &Setup, store: &Mutex<Store>) -> (TerminalId, PathBuf, PathBuf) {
        let id = put_running(store, setup.me);
        std::fs::create_dir_all(setup.dir()).unwrap();
        let socket = setup.socket(id);
        drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
        let lock = lock_file(&socket);
        std::fs::File::create(&lock).unwrap();
        (id, socket, lock)
    }

    fn take_lock(path: &Path, arg: FlockArg) -> Flock<std::fs::File> {
        Flock::lock(std::fs::File::open(path).unwrap(), arg)
            .map_err(|(_, e)| e)
            .unwrap()
    }

    /// A socket that refuses connections while its lock is held is a busy holder, not a gone
    /// one: nothing lets go of it or records an exit until the lock is free.
    #[tokio::test]
    async fn a_refusing_socket_whose_lock_is_held_is_not_taken_for_gone() {
        let setup = Setup::new();
        let (terminals, store) = setup.daemon();
        let (id, socket, lock_path) = refusing_terminal(&setup, &store);
        let lock = take_lock(&lock_path, FlockArg::LockExclusiveNonblock);

        terminals.reattach_all().await.unwrap();
        let error = terminals.close(id).await.unwrap_err();
        assert!(matches!(error, TerminalError::Holder { .. }), "{error:?}");
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(socket.exists(), "the socket is left");
        assert!(terminals.inner.entry(id).is_ok(), "the terminal stays live");
        let record = held(&store, setup.me, id).unwrap();
        assert!(!record.deleted);
        assert_eq!(record.body.state, TerminalState::Running);

        drop(lock);
        until("the record to say exited", || {
            held(&store, setup.me, id).unwrap().body.state == TerminalState::Exited
        })
        .await;
        assert_eq!(held(&store, setup.me, id).unwrap().body.exit_code, None);
        terminals.close(id).await.unwrap();
        assert!(held(&store, setup.me, id).unwrap().deleted);
        assert!(!socket.exists(), "the stale socket is removed");
        assert!(!lock_path.exists(), "its lock file is removed");
    }

    /// A lock file that cannot be checked says nothing about its holder, so the refusal is a
    /// failure that is tried again, never a gone holder.
    #[tokio::test]
    async fn a_refusing_socket_whose_lock_cannot_be_checked_is_not_taken_for_gone() {
        let setup = Setup::new();
        let (terminals, store) = setup.daemon();
        let (id, socket, lock) = refusing_terminal(&setup, &store);
        set_mode(&lock, 0o000);

        terminals.reattach_all().await.unwrap();
        let error = terminals.close(id).await.unwrap_err();
        assert!(matches!(error, TerminalError::Holder { .. }), "{error:?}");
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(socket.exists(), "the socket is left");
        assert!(lock.exists(), "the lock file is left");
        assert!(terminals.inner.entry(id).is_ok(), "the terminal stays live");
        let record = held(&store, setup.me, id).unwrap();
        assert!(!record.deleted);
        assert_eq!(record.body.state, TerminalState::Running);

        set_mode(&lock, 0o600);
        terminals.close(id).await.unwrap();
        assert!(held(&store, setup.me, id).unwrap().deleted);
        assert!(!socket.exists(), "the stale socket is removed");
        assert!(!lock.exists(), "its lock file is removed");
    }

    /// Only a holder takes its lock exclusively. A shared lock, such as another check of the
    /// same file at the same moment, is no holder.
    #[tokio::test]
    async fn a_shared_lock_beside_a_refusing_socket_is_no_holder() {
        let setup = Setup::new();
        let (terminals, store) = setup.daemon();
        let (id, socket, lock) = refusing_terminal(&setup, &store);
        let _check = take_lock(&lock, FlockArg::LockSharedNonblock);

        terminals.reattach_all().await.unwrap();
        assert_eq!(
            held(&store, setup.me, id).unwrap().body.state,
            TerminalState::Exited
        );
        terminals.close(id).await.unwrap();
        assert!(held(&store, setup.me, id).unwrap().deleted);
        assert!(!socket.exists(), "the stale socket is removed");
        assert!(!lock.exists(), "its lock file is removed");
    }

    /// Connects to `socket`, which refuses now, and has a holder serve it again after the first
    /// refusal and before the first retry. The clock is paused, so that order holds however
    /// loaded the machine is.
    async fn connect_as_it_serves_again(
        dir: &Path,
        socket: &Path,
    ) -> Result<UnixStream, Unreachable> {
        let path = socket.to_path_buf();
        let connecting = tokio::spawn(async move { connect(&path).await });
        tokio::time::sleep(BUSY_RETRY / 2).await;
        // Bound beside the socket and renamed over it, so a connect never finds it missing.
        let serving = dir.join("serving.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&serving).unwrap();
        std::fs::rename(&serving, socket).unwrap();
        connecting.await.unwrap()
    }

    /// A running holder whose queue of new connections is full refuses for a moment. The
    /// connection is tried again while its lock is held, so it gets through once the queue
    /// empties rather than failing the request.
    #[tokio::test(start_paused = true)]
    async fn a_busy_holder_that_refuses_is_connected_to_when_it_serves_again() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("t.sock");
        drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
        let lock_path = lock_file(&socket);
        std::fs::File::create(&lock_path).unwrap();
        let _lock = take_lock(&lock_path, FlockArg::LockExclusiveNonblock);

        let connected = connect_as_it_serves_again(dir.path(), &socket).await;
        assert!(connected.is_ok(), "a busy holder's refusal is tried again");
    }

    /// A holder started before holders kept a lock file has none, so a refusal cannot be told
    /// from a gone holder at once. It is tried again like a busy one before it counts as gone.
    #[tokio::test(start_paused = true)]
    async fn a_holder_with_no_lock_file_that_refuses_is_connected_to_when_it_serves_again() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("t.sock");
        drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());

        let connected = connect_as_it_serves_again(dir.path(), &socket).await;
        assert!(
            connected.is_ok(),
            "a refusal with no lock file is tried again"
        );
    }

    /// The holder and foundryd each name the lock file, so a real holder must hold the file
    /// `lock_file` names. A holder that kept its lock anywhere else would read as gone.
    #[tokio::test]
    async fn a_running_holder_holds_the_lock_file_foundryd_checks() {
        let setup = Setup::new();
        let (terminals, _) = setup.daemon();
        let id = setup.start(&terminals, "sleep 60").await;
        let lock = lock_file(&setup.socket(id));
        assert_eq!(
            holder_lock(&lock).unwrap(),
            HolderLock::Held,
            "{}",
            lock.display()
        );
    }

    #[tokio::test]
    async fn a_holder_that_ends_while_the_daemon_runs_is_recorded_exited_and_can_be_closed() {
        let setup = Setup::new();
        let (terminals, store) = setup.daemon();
        let id = setup.start(&terminals, "sleep 60").await;
        setup.end_holder(id).await;
        until("the record to say exited", || {
            held(&store, setup.me, id).unwrap().body.state == TerminalState::Exited
        })
        .await;
        let record = held(&store, setup.me, id).unwrap();
        assert_eq!(record.body.exit_code, None);
        assert!(!record.deleted);
        assert!(
            matches!(terminals.attach(id).await, Err(TerminalError::Gone(gone)) if gone == id),
            "attaching to a terminal whose holder is gone says so"
        );
        terminals.close(id).await.unwrap();
        assert!(held(&store, setup.me, id).unwrap().deleted);
    }

    /// A holder that refuses an attach, or refuses part way through the stream, gives the
    /// client its refusal, not an end that reads as an exit or a cut-off to attach again after.
    #[tokio::test]
    async fn a_holder_that_refuses_an_attach_gives_the_refusal() {
        let setup = Setup::new();
        let (terminals, _) = setup.daemon();
        let id = TerminalId::random();
        // Live with no watcher, so every connection to the stand-in is one of these attaches.
        terminals.inner.insert(
            Terminal {
                id,
                workspace: WorkspaceId::random(),
                state: TerminalState::Running,
                exit_code: None,
            },
            setup.socket(id),
        );
        let holder = stand_in(
            setup.socket(id),
            vec![
                (
                    vec![Reply::Refused("version 9 is not supported".into())],
                    true,
                ),
                (
                    vec![
                        Reply::Snapshot {
                            start: 0,
                            bytes: Bytes::from_static(b"kept"),
                        },
                        Reply::Output(Bytes::from_static(b"live")),
                        Reply::Refused("input too large".into()),
                    ],
                    true,
                ),
            ],
        );

        let refused = terminals.attach(id).await.err();
        assert!(
            matches!(
                &refused,
                Some(TerminalError::Refused { id: r, reason }) if *r == id && reason == "version 9 is not supported"
            ),
            "{refused:?}"
        );

        let (snapshot, stream) = terminals.attach(id).await.unwrap();
        assert_eq!(&snapshot.bytes[..], b"kept");
        let items: Vec<_> = tokio::time::timeout(WAIT, stream.collect())
            .await
            .expect("the stream ends after the refusal");
        let [Ok(live), Err(TerminalError::Refused { id: r, reason })] = items.as_slice() else {
            panic!("expected the live bytes then the refusal, got {items:?}");
        };
        assert_eq!(&live[..], b"live");
        assert_eq!((*r, reason.as_str()), (id, "input too large"));
        holder.abort();
    }

    /// A holder that reports it could not learn the child's exit code (`UNKNOWN_EXIT`) leaves
    /// the record with no exit code, not a made-up one.
    #[tokio::test]
    async fn an_exit_whose_code_the_holder_does_not_know_is_recorded_with_none() {
        let setup = Setup::new();
        let (terminals, store) = setup.daemon();
        let id = put_running(&store, setup.me);
        let holder = stand_in(
            setup.socket(id),
            vec![(
                vec![
                    Reply::Snapshot {
                        start: 0,
                        bytes: Bytes::new(),
                    },
                    Reply::Status(ChildStatus::Exited(UNKNOWN_EXIT)),
                ],
                false,
            )],
        );
        terminals.reattach_all().await.unwrap();
        until("the record to say exited", || {
            held(&store, setup.me, id).unwrap().body.state == TerminalState::Exited
        })
        .await;
        assert_eq!(held(&store, setup.me, id).unwrap().body.exit_code, None);
        holder.abort();
    }

    /// A store that fails part way through `reattach_all` stops it with that error. Run again
    /// once the store takes writes, it finishes the rest, leaves alone what it already made
    /// live, and a further run writes nothing new.
    #[tokio::test]
    async fn reattach_all_stopped_by_the_store_finishes_when_run_again() {
        let setup = Setup::new();
        let (first, store) = setup.daemon();
        let live = setup.start(&first, "sleep 60").await;
        drop(first);
        // Running records with no holder, written after the live one, so they come after it.
        let gone = [put_running(&store, setup.me), put_running(&store, setup.me)];
        let running = held(&store, setup.me, live).unwrap();

        let (terminals, store) = setup.daemon();
        set_mode(setup.data.path(), 0o500);
        let error = terminals.reattach_all().await.unwrap_err();
        assert!(matches!(error, TerminalError::Store(_)), "{error:?}");
        let watched = terminals
            .inner
            .entry(live)
            .expect("the terminal before the failure is live");
        // A run again skips a terminal already live, so it keeps one entry and one watcher.
        let kept = |id, entry: &Arc<Entry>| {
            let now = terminals.inner.entry(id).unwrap();
            assert!(Arc::ptr_eq(&now, entry), "{id} was made live again");
        };
        for id in gone {
            assert!(terminals.inner.entry(id).is_err(), "{id} was made live");
            assert_eq!(
                held(&store, setup.me, id).unwrap().body.state,
                TerminalState::Running,
                "{id}"
            );
        }

        set_mode(setup.data.path(), 0o700);
        terminals.reattach_all().await.unwrap();
        let exited = gone.map(|id| {
            let record = held(&store, setup.me, id).unwrap();
            assert_eq!(record.body.state, TerminalState::Exited, "{id}");
            assert_eq!(record.body.exit_code, None, "{id}");
            assert!(!record.deleted, "{id}");
            record
        });
        assert_eq!(held(&store, setup.me, live).unwrap(), running);
        kept(live, &watched);
        let ended = gone.map(|id| terminals.inner.entry(id).unwrap());

        terminals.reattach_all().await.unwrap();
        for (id, record) in gone.into_iter().zip(exited) {
            assert_eq!(held(&store, setup.me, id).unwrap(), record, "{id}");
        }
        assert_eq!(held(&store, setup.me, live).unwrap(), running);
        kept(live, &watched);
        for (id, entry) in gone.into_iter().zip(&ended) {
            kept(id, entry);
        }

        for id in [live, gone[0], gone[1]] {
            terminals.close(id).await.unwrap();
            assert!(held(&store, setup.me, id).unwrap().deleted, "{id}");
        }
    }

    #[tokio::test]
    async fn input_and_resize_reach_the_child_in_order() {
        let setup = Setup::new();
        let (terminals, _) = setup.daemon();
        let id = setup
            .start(
                &terminals,
                "stty -echo; read a; read b; echo \"got $a $b\"; stty size; sleep 60",
            )
            .await;
        let (snapshot, mut stream) = terminals.attach(id).await.unwrap();
        let mut seen = snapshot.bytes.to_vec();
        terminals.resize(id, 101, 41).await.unwrap();
        for part in ["on", "e\n", "two", "\n"] {
            terminals
                .input(id, Bytes::copy_from_slice(part.as_bytes()))
                .await
                .unwrap();
        }
        read_until(&mut stream, &mut seen, |seen| contains(seen, b"41 101")).await;
        assert!(
            contains(&seen, b"got one two"),
            "{:?}",
            String::from_utf8_lossy(&seen)
        );
        terminals.close(id).await.unwrap();
    }

    /// The holder refuses input that would take what the child has not read over its limit.
    /// That input is an error here, not lost in silence, and later input still reaches it.
    #[tokio::test]
    async fn input_the_holder_refuses_is_an_error_and_later_input_still_arrives() {
        let setup = Setup::new();
        let (terminals, _) = setup.daemon();
        let id = setup
            .start(
                &terminals,
                "stty -echo; sleep 1; read -r line; echo \"read ${#line}\"; sleep 60",
            )
            .await;
        let (snapshot, mut stream) = terminals.attach(id).await.unwrap();
        let mut seen = snapshot.bytes.to_vec();
        let flood = Bytes::from(vec![b'x'; MAX_PENDING_INPUT + 1]);
        let error = terminals.input(id, flood).await.unwrap_err();
        assert!(
            matches!(error, TerminalError::Refused { id: refused, .. } if refused == id),
            "{error:?}"
        );
        terminals
            .input(id, Bytes::from_static(b"abc\n"))
            .await
            .unwrap();
        read_until(&mut stream, &mut seen, |seen| contains(seen, b"read 3")).await;
        terminals.close(id).await.unwrap();
        until("the holder's socket to go", || !setup.socket(id).exists()).await;
    }

    /// A client too slow for the output is cut off by the holder. Its stream says so with an
    /// error, which an exit never gives, so the caller knows to attach again.
    #[tokio::test]
    async fn a_client_cut_off_for_falling_behind_gets_an_error_not_an_end() {
        let setup = Setup::new();
        let (terminals, store) = setup.daemon();
        let id = setup
            .start(
                &terminals,
                "head -c 30000000 /dev/zero | tr '\\0' x; sleep 60",
            )
            .await;
        let (_, stream) = terminals.attach(id).await.unwrap();
        // Not reading lets this client's backlog in the holder pass its limit.
        tokio::time::sleep(Duration::from_secs(3)).await;
        let items: Vec<_> = tokio::time::timeout(WAIT, stream.collect())
            .await
            .expect("the stream ends once the holder cuts the client off");
        let (last, before) = items.split_last().expect("some output arrived");
        assert!(before.iter().all(Result::is_ok));
        assert!(
            matches!(last, Err(TerminalError::Detached(cut)) if *cut == id),
            "{last:?}"
        );
        assert_eq!(
            held(&store, setup.me, id).unwrap().body.state,
            TerminalState::Running
        );
        terminals.close(id).await.unwrap();
    }

    /// What the child of a terminal started with `env` sees in `TERM` and `COLORTERM`.
    async fn colour_vars(env: &[(String, String)]) -> String {
        let setup = Setup::new();
        let (terminals, _) = setup.daemon();
        let id = terminals
            .start(
                WorkspaceId::random(),
                setup.cwd.path(),
                &argv("echo \"[TERM=$TERM] [COLORTERM=$COLORTERM] [end]\"; sleep 60"),
                env,
            )
            .await
            .unwrap();
        let (snapshot, mut stream) = terminals.attach(id).await.unwrap();
        let mut seen = snapshot.bytes.to_vec();
        read_until(&mut stream, &mut seen, |seen| contains(seen, b"[end]")).await;
        terminals.close(id).await.unwrap();
        String::from_utf8_lossy(&seen).into_owned()
    }

    /// The daemon under launchd has no `TERM`, so the terminal names a colour one itself.
    #[tokio::test]
    async fn a_terminal_starts_as_a_colour_terminal() {
        let seen = colour_vars(&[]).await;
        assert!(seen.contains("[TERM=xterm-256color]"), "{seen:?}");
        assert!(seen.contains("[COLORTERM=truecolor]"), "{seen:?}");
    }

    #[tokio::test]
    async fn the_callers_term_wins_over_the_colour_default() {
        let seen = colour_vars(&[("TERM".to_string(), "vt100".to_string())]).await;
        assert!(seen.contains("[TERM=vt100]"), "{seen:?}");
        assert!(seen.contains("[COLORTERM=truecolor]"), "{seen:?}");
    }

    /// Whatever the test process's own `TERM`, a caller's variable replaces its default rather
    /// than sitting beside it.
    #[test]
    fn colour_env_adds_only_the_defaults_the_caller_does_not_name() {
        let pair = |name: &str, value: &str| (name.to_string(), value.to_string());
        assert_eq!(
            colour_env(&[]),
            vec![
                pair("TERM", "xterm-256color"),
                pair("COLORTERM", "truecolor")
            ]
        );
        assert_eq!(
            colour_env(&[pair("TERM", "vt100"), pair("FOUNDRY_TEST_VALUE", "kept")]),
            vec![
                pair("COLORTERM", "truecolor"),
                pair("TERM", "vt100"),
                pair("FOUNDRY_TEST_VALUE", "kept"),
            ]
        );
    }

    #[tokio::test]
    async fn the_child_gets_the_env_and_the_env_file_is_gone() {
        let setup = Setup::new();
        let (terminals, _) = setup.daemon();
        let env = [("FOUNDRY_TEST_VALUE".to_string(), "from the env".to_string())];
        let id = terminals
            .start(
                WorkspaceId::random(),
                setup.cwd.path(),
                &argv("echo \"[$FOUNDRY_TEST_VALUE] $(pwd)\"; sleep 60"),
                &env,
            )
            .await
            .unwrap();
        let (snapshot, mut stream) = terminals.attach(id).await.unwrap();
        let mut seen = snapshot.bytes.to_vec();
        let cwd = setup.cwd.path().canonicalize().unwrap();
        read_until(&mut stream, &mut seen, |seen| {
            contains(seen, cwd.to_string_lossy().as_bytes())
        })
        .await;
        assert!(
            contains(&seen, b"[from the env]"),
            "{:?}",
            String::from_utf8_lossy(&seen)
        );
        assert_eq!(
            setup.left(),
            vec![format!("{id}.lock"), format!("{id}.sock")],
            "only the socket and its holder's lock are left"
        );
        let mode = std::fs::metadata(setup.dir()).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o700,
            "the terminals directory is this user's only"
        );
        terminals.close(id).await.unwrap();
    }

    #[tokio::test]
    async fn close_ends_the_holder_and_deletes_the_record() {
        let setup = Setup::new();
        let (terminals, store) = setup.daemon();
        let id = setup.start(&terminals, "sleep 60").await;
        terminals.close(id).await.unwrap();
        let record = held(&store, setup.me, id).unwrap();
        assert!(record.deleted);
        assert!(
            !setup.socket(id).exists(),
            "close waits for the holder to end"
        );
        assert!(matches!(
            terminals.attach(id).await,
            Err(TerminalError::Unknown(gone)) if gone == id
        ));
        assert!(matches!(
            terminals.input(id, Bytes::from_static(b"x")).await,
            Err(TerminalError::Unknown(_))
        ));
        assert!(matches!(
            terminals.close(id).await,
            Err(TerminalError::Unknown(_))
        ));

        // A daemon started again does not bring a closed terminal back.
        drop(terminals);
        let (terminals, store) = setup.daemon();
        terminals.reattach_all().await.unwrap();
        assert_eq!(held(&store, setup.me, id).unwrap(), record);
        assert!(matches!(
            terminals.attach(id).await,
            Err(TerminalError::Unknown(_))
        ));
    }

    #[tokio::test]
    async fn a_live_terminal_has_a_files_folder_that_goes_with_its_close() {
        let setup = Setup::new();
        let (terminals, _store) = setup.daemon();
        let id = setup.start(&terminals, "sleep 60").await;
        let files = terminals.files(id).unwrap();
        assert_eq!(files, setup.dir().join(format!("{id}.files")));
        std::fs::create_dir_all(&files).unwrap();
        std::fs::write(files.join("shot.png"), b"png").unwrap();

        terminals.close(id).await.unwrap();
        assert!(!files.exists(), "the files are removed with the terminal");
        assert!(matches!(
            terminals.files(id),
            Err(TerminalError::Unknown(gone)) if gone == id
        ));
    }

    #[tokio::test]
    async fn a_file_kept_after_its_terminal_closed_is_refused_and_leaves_no_folder() {
        let setup = Setup::new();
        let (terminals, _store) = setup.daemon();
        let id = setup.start(&terminals, "sleep 60").await;
        // Looked up while the terminal is live, as an upload does before its body arrives.
        let files = terminals.files(id).unwrap();
        terminals.close(id).await.unwrap();

        let kept = terminals
            .keep_file(id, |folder| {
                std::fs::create_dir_all(folder)?;
                std::fs::write(folder.join("shot.png"), b"png")?;
                Ok(folder.join("shot.png"))
            })
            .await;
        assert!(matches!(kept, Err(TerminalError::Unknown(gone)) if gone == id));
        assert!(
            !files.exists(),
            "a closed terminal's folder is not made again"
        );
    }

    #[tokio::test]
    async fn a_close_waits_for_a_file_being_kept_and_removes_it_with_the_folder() {
        let setup = Setup::new();
        let (terminals, _store) = setup.daemon();
        let id = setup.start(&terminals, "sleep 60").await;
        let terminals = Arc::new(terminals);
        let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        let keeping = tokio::spawn({
            let terminals = terminals.clone();
            async move {
                terminals
                    .keep_file(id, move |folder| {
                        started_tx.send(()).unwrap();
                        go_rx.recv().unwrap();
                        std::fs::create_dir_all(folder)?;
                        std::fs::write(folder.join("shot.png"), b"png")?;
                        Ok(folder.join("shot.png"))
                    })
                    .await
            }
        });
        tokio::task::spawn_blocking(move || started_rx.recv().unwrap())
            .await
            .unwrap();
        let closing = tokio::spawn({
            let terminals = terminals.clone();
            async move { terminals.close(id).await }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!closing.is_finished(), "the close waits for the file");

        go_tx.send(()).unwrap();
        let kept = keeping.await.unwrap().unwrap().unwrap();
        closing.await.unwrap().unwrap();
        assert!(!kept.exists(), "the file goes with the terminal");
        assert!(!terminals.inner.files(id).exists());
    }

    #[tokio::test]
    async fn a_close_waits_for_a_file_whose_caller_went_away_and_removes_it_with_the_folder() {
        let setup = Setup::new();
        let (terminals, _store) = setup.daemon();
        let id = setup.start(&terminals, "sleep 60").await;
        let terminals = Arc::new(terminals);
        let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        let (wrote_tx, wrote_rx) = std::sync::mpsc::channel::<PathBuf>();
        let keeping = tokio::spawn({
            let terminals = terminals.clone();
            async move {
                terminals
                    .keep_file(id, move |folder| {
                        started_tx.send(()).unwrap();
                        go_rx.recv().unwrap();
                        std::fs::create_dir_all(folder)?;
                        std::fs::write(folder.join("shot.png"), b"png")?;
                        wrote_tx.send(folder.join("shot.png")).unwrap();
                        Ok(folder.join("shot.png"))
                    })
                    .await
            }
        });
        tokio::task::spawn_blocking(move || started_rx.recv().unwrap())
            .await
            .unwrap();
        // The upload's client hangs up: its handler is dropped while the write goes on.
        keeping.abort();
        let closing = tokio::spawn({
            let terminals = terminals.clone();
            async move { terminals.close(id).await }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!closing.is_finished(), "the close waits for the write");

        go_tx.send(()).unwrap();
        closing.await.unwrap().unwrap();
        let written = tokio::task::spawn_blocking(move || wrote_rx.recv().unwrap())
            .await
            .unwrap();
        assert!(!written.exists(), "the file goes with the terminal");
        assert!(!terminals.inner.files(id).exists());
    }

    #[tokio::test]
    async fn a_file_that_waited_behind_a_close_is_refused_and_leaves_no_folder() {
        let setup = Setup::new();
        let (terminals, _store) = setup.daemon();
        let id = setup.start(&terminals, "sleep 60").await;
        let terminals = Arc::new(terminals);
        // Held here so the close and then the write queue up behind it, in that order.
        let entry = terminals.inner.entry(id).unwrap();
        let held = entry.body.lock().await;
        let closing = tokio::spawn({
            let terminals = terminals.clone();
            async move { terminals.close(id).await }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        let keeping = tokio::spawn({
            let terminals = terminals.clone();
            async move {
                terminals
                    .keep_file(id, |folder| {
                        std::fs::create_dir_all(folder)?;
                        Ok(folder.join("late.png"))
                    })
                    .await
            }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!closing.is_finished() && !keeping.is_finished());

        drop(held);
        closing.await.unwrap().unwrap();
        let kept = keeping.await.unwrap();
        assert!(matches!(kept, Err(TerminalError::Unknown(gone)) if gone == id));
        assert!(!terminals.inner.files(id).exists());
    }

    /// The caller of `start` going away, as an HTTP handler does when its client hangs up,
    /// leaves the terminal started and live, not running with nothing attached to it.
    #[tokio::test]
    async fn a_start_whose_caller_goes_away_still_finishes() {
        let setup = Setup::new();
        let (terminals, store) = setup.daemon();
        let sleep = argv("sleep 60");
        let started = terminals.start(WorkspaceId::random(), setup.cwd.path(), &sleep, &[]);
        // Polled until the holder is being started, then dropped.
        let _ = tokio::time::timeout(Duration::from_millis(5), started).await;
        let mut records = Vec::new();
        until("the start's record", || {
            records = store.lock().unwrap().since(setup.me, 0).unwrap();
            !records.is_empty()
        })
        .await;
        let [AnyRecord::Terminal(record)] = records.as_slice() else {
            panic!("expected one terminal record, got {records:?}");
        };
        let id = record.body.id;
        until("the holder's socket", || setup.socket(id).exists()).await;
        until("the terminal to be live", || {
            terminals.inner.entry(id).is_ok()
        })
        .await;
        terminals.close(id).await.unwrap();
        assert!(held(&store, setup.me, id).unwrap().deleted);
    }

    #[tokio::test]
    async fn a_command_that_cannot_start_is_an_error_and_leaves_no_live_record() {
        let setup = Setup::new();
        let secret = "sk-not-for-any-message";
        let env = [("ANTHROPIC_AUTH_TOKEN".to_string(), secret.to_string())];
        for (holder, argv, says) in [
            (
                holder(),
                vec!["/no/such/program".to_string()],
                "/no/such/program",
            ),
            (
                PathBuf::from("/no/such/foundry-holder"),
                argv("true"),
                "/no/such/foundry-holder",
            ),
        ] {
            let (terminals, store) = setup.daemon_with(holder);
            let error = terminals
                .start(WorkspaceId::random(), setup.cwd.path(), &argv, &env)
                .await
                .unwrap_err();
            let TerminalError::Start { id, ref reason } = error else {
                panic!("expected a start error, got {error:?}");
            };
            assert!(reason.contains(says), "{reason}");
            assert!(!error.to_string().contains(secret), "{error}");
            assert!(held(&store, setup.me, id).unwrap().deleted);
            assert!(terminals.inner.entry(id).is_err(), "nothing is left live");
            assert!(setup.left().is_empty(), "left behind: {:?}", setup.left());
        }
    }

    /// An env file a daemon that stopped part way through `start` left behind may hold
    /// secrets. The next daemon removes it before it starts anything.
    #[tokio::test]
    async fn reattach_all_removes_env_files_left_behind() {
        let setup = Setup::new();
        std::fs::create_dir_all(setup.dir()).unwrap();
        let left = setup.dir().join(format!("{}.env", TerminalId::random()));
        std::fs::write(&left, br#"{"ANTHROPIC_AUTH_TOKEN":"sk-left"}"#).unwrap();
        let (terminals, _) = setup.daemon();
        terminals.reattach_all().await.unwrap();
        assert!(!left.exists());
    }

    /// The holder cuts the watcher off, then refuses it, then reports the exit. The watcher
    /// attaches again after each and records the exit code, not an exit at the first drop.
    #[tokio::test]
    async fn the_watcher_attaches_again_after_the_holder_drops_or_refuses_it() {
        let setup = Setup::new();
        let (terminals, store) = setup.daemon();
        let id = put_running(&store, setup.me);
        let snapshot = || Reply::Snapshot {
            start: 0,
            bytes: Bytes::new(),
        };
        let holder = stand_in(
            setup.socket(id),
            vec![
                (vec![snapshot()], true),
                (
                    vec![Reply::Refused("version 9 is not supported".into())],
                    true,
                ),
                (
                    vec![snapshot(), Reply::Status(ChildStatus::Exited(5))],
                    false,
                ),
            ],
        );
        terminals.reattach_all().await.unwrap();
        until("the record to say exited", || {
            held(&store, setup.me, id).unwrap().body.state == TerminalState::Exited
        })
        .await;
        assert_eq!(held(&store, setup.me, id).unwrap().body.exit_code, Some(5));
        holder.abort();
    }

    /// A holder that takes a request and never answers fails it in bounded time, rather than
    /// holding the caller or the terminal's control connection for ever.
    #[tokio::test(start_paused = true)]
    async fn a_holder_that_does_not_answer_fails_attach_input_and_close_in_time() {
        let setup = Setup::new();
        let (terminals, store) = setup.daemon();
        let id = put_running(&store, setup.me);
        let holder = stand_in(setup.socket(id), vec![]);
        terminals.reattach_all().await.unwrap();
        let attached = tokio::time::timeout(WAIT, terminals.attach(id))
            .await
            .expect("attach gives up before the test does");
        let Err(TerminalError::Holder {
            source: protocol::Error::Io(e),
            ..
        }) = attached
        else {
            panic!("expected attach to time out, got {:?}", attached.err());
        };
        assert_eq!(e.kind(), io::ErrorKind::TimedOut);
        for result in [
            terminals.input(id, Bytes::from_static(b"x")).await,
            terminals.close(id).await,
        ] {
            let Err(TerminalError::Holder {
                source: protocol::Error::Io(e),
                ..
            }) = result
            else {
                panic!("expected the holder to time out, got {result:?}");
            };
            assert_eq!(e.kind(), io::ErrorKind::TimedOut);
        }
        assert!(terminals.inner.entry(id).is_ok(), "the terminal stays live");
        holder.abort();
    }

    /// The caller of `close` going away leaves the terminal closed, not half closed.
    #[tokio::test]
    async fn a_close_whose_caller_goes_away_still_finishes() {
        let setup = Setup::new();
        let (terminals, store) = setup.daemon();
        let id = setup.start(&terminals, "sleep 60").await;
        let _ = tokio::time::timeout(Duration::from_millis(1), terminals.close(id)).await;
        until("the record to be deleted and the terminal let go", || {
            held(&store, setup.me, id).unwrap().deleted && terminals.inner.entry(id).is_err()
        })
        .await;
    }

    /// The watcher's exit write waits for a busy store while the terminal is closed. Close
    /// waits for that write, so its deleted record is the newest and the terminal stays closed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[expect(
        clippy::await_holding_lock,
        reason = "the test holds the store to keep the exit write waiting"
    )]
    async fn close_waits_for_an_exit_write_in_flight_so_its_deleted_record_is_newest() {
        let setup = Setup::new();
        let (terminals, store) = setup.daemon();
        let id = setup.start(&terminals, "stty -echo; read x; exit 2").await;
        let busy = store.lock().unwrap();
        terminals
            .input(id, Bytes::from_static(b"\n"))
            .await
            .unwrap();
        // The child exits and the watcher's write of it waits for the store.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let closing = tokio::spawn(terminals.inner.clone().close(id));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            terminals.inner.entry(id).is_ok(),
            "close lets the terminal go before the exit write in flight is done"
        );
        drop(busy);
        closing.await.unwrap().unwrap();
        let newest = held(&store, setup.me, id).unwrap();
        assert!(newest.deleted, "{newest:?}");
        assert_eq!(newest.body.exit_code, Some(2));
    }

    /// An exit the store could not take is written once it can, not dropped.
    #[tokio::test]
    async fn an_exit_the_store_could_not_record_is_recorded_once_it_can() {
        let setup = Setup::new();
        let (terminals, store) = setup.daemon();
        let id = setup.start(&terminals, "stty -echo; read x; exit 4").await;
        // SQLite cannot make its journal in a read-only directory, so every write fails.
        set_mode(setup.data.path(), 0o500);
        terminals
            .input(id, Bytes::from_static(b"\n"))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(
            held(&store, setup.me, id).unwrap().body.state,
            TerminalState::Running
        );
        set_mode(setup.data.path(), 0o700);
        until("the record to say exited", || {
            held(&store, setup.me, id).unwrap().body.state == TerminalState::Exited
        })
        .await;
        assert_eq!(held(&store, setup.me, id).unwrap().body.exit_code, Some(4));
        terminals.close(id).await.unwrap();
    }

    /// A close whose record the store could not take leaves the terminal live, so the close
    /// can be tried again.
    #[tokio::test]
    async fn a_close_the_store_could_not_record_can_be_tried_again() {
        let setup = Setup::new();
        let (terminals, store) = setup.daemon();
        let id = setup.start(&terminals, "sleep 60").await;
        set_mode(setup.data.path(), 0o500);
        let error = terminals.close(id).await.unwrap_err();
        assert!(matches!(error, TerminalError::Store(_)), "{error:?}");
        assert!(terminals.inner.entry(id).is_ok(), "the terminal stays live");
        set_mode(setup.data.path(), 0o700);
        terminals.close(id).await.unwrap();
        assert!(held(&store, setup.me, id).unwrap().deleted);
    }

    /// A start that fails while the store cannot take the failure still says why it failed,
    /// and leaves a terminal that can be closed rather than a running record nothing reaches.
    #[tokio::test]
    async fn a_failed_start_the_store_could_not_record_can_still_be_closed() {
        let setup = Setup::new();
        // A holder that makes the data directory read-only, then fails.
        let fake = setup.cwd.path().join("fake-holder");
        std::fs::write(
            &fake,
            "#!/bin/sh\nchmod 500 \"$(dirname \"$(dirname \"$2\")\")\"\necho 'foundry-holder: no pty' >&2\nexit 1\n",
        )
        .unwrap();
        set_mode(&fake, 0o755);
        let (terminals, store) = setup.daemon_with(fake);
        let error = terminals
            .start(WorkspaceId::random(), setup.cwd.path(), &argv("true"), &[])
            .await
            .unwrap_err();
        let TerminalError::Start { id, ref reason } = error else {
            panic!("expected a start error, got {error:?}");
        };
        assert_eq!(reason, "no pty");
        set_mode(setup.data.path(), 0o700);
        // Once the store takes writes again, the record says what happened.
        until("the record to say exited", || {
            held(&store, setup.me, id).unwrap().body.state == TerminalState::Exited
        })
        .await;
        let record = held(&store, setup.me, id).unwrap();
        assert_eq!(record.body.exit_code, None);
        assert!(!record.deleted);
        terminals.close(id).await.unwrap();
        assert!(held(&store, setup.me, id).unwrap().deleted);
    }

    /// Writes a fake holder that runs `script` and then hangs, as a starter that never exits
    /// would. It touches `forked` once `script` is done. `$2` in the script is the socket.
    fn hanging_holder(setup: &Setup, script: &str, forked: &Path) -> PathBuf {
        let fake = setup.cwd.path().join("fake-holder");
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\n{script}\ntouch '{}'\nsleep 600\n",
                forked.display()
            ),
        )
        .unwrap();
        set_mode(&fake, 0o755);
        fake
    }

    /// Starts a terminal whose fake holder is `fake`, running a paused clock forward only once
    /// `forked` exists: paused time jumps ahead whenever the runtime is idle, so this lets the
    /// start's time limit run out only after the fake has done its part. Real time resumes
    /// after, so a real holder answers in time.
    async fn start_past_the_limit(
        terminals: &Terminals,
        setup: &Setup,
        forked: PathBuf,
        env: &[(String, String)],
    ) -> Result<TerminalId, TerminalError> {
        let busy = tokio::spawn(async move {
            while !forked.exists() {
                tokio::task::yield_now().await;
            }
        });
        let started = tokio::time::timeout(
            WAIT * 10,
            terminals.start(WorkspaceId::random(), setup.cwd.path(), &argv("true"), env),
        )
        .await
        .expect("start gives up before the test does");
        busy.abort();
        tokio::time::resume();
        started
    }

    /// A start whose holder does not serve its socket in time fails in bounded time, and the
    /// process it ran is killed. The env file goes, and no record or live terminal is left.
    #[tokio::test(start_paused = true)]
    async fn a_holder_not_serving_when_the_time_limit_runs_out_fails_the_start_and_is_killed() {
        let setup = Setup::new();
        let (forked, late) = (
            setup.cwd.path().join("forked"),
            setup.cwd.path().join("late"),
        );
        let fake = hanging_holder(&setup, "", &forked);
        // The fake makes `late` a second after it is ready to hang, unless it is killed.
        std::fs::write(
            &fake,
            std::fs::read_to_string(&fake).unwrap().replace(
                "sleep 600",
                &format!("sleep 1\ntouch '{}'\nsleep 600", late.display()),
            ),
        )
        .unwrap();
        let (terminals, store) = setup.daemon_with(fake);
        let env = [("ANTHROPIC_AUTH_TOKEN".to_string(), "sk-hung".to_string())];
        let error = start_past_the_limit(&terminals, &setup, forked, &env)
            .await
            .unwrap_err();
        let TerminalError::Start { id, ref reason } = error else {
            panic!("expected a start error, got {error:?}");
        };
        assert!(reason.contains("did not start within"), "{reason}");
        assert!(!error.to_string().contains("sk-hung"), "{error}");
        let record = held(&store, setup.me, id).unwrap();
        assert!(record.deleted, "{record:?}");
        assert!(terminals.inner.entry(id).is_err(), "nothing is left live");
        assert!(setup.left().is_empty(), "left behind: {:?}", setup.left());
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(!late.exists(), "the process run was left running");
    }

    /// A holder already serving its socket when the time limit runs out, with the process that
    /// started it not yet done, is a started terminal: the start says so, and it is watched.
    #[tokio::test(start_paused = true)]
    async fn a_holder_serving_when_the_time_limit_runs_out_is_a_started_terminal() {
        let setup = Setup::new();
        let forked = setup.cwd.path().join("forked");
        let fake = hanging_holder(
            &setup,
            &format!(
                "echo '{{}}' > \"$2.own-env\"\n\
                 '{}' --socket \"$2\" --cwd /tmp --env-file \"$2.own-env\" -- sleep 60",
                holder().display()
            ),
            &forked,
        );
        let (terminals, store) = setup.daemon_with(fake);
        let id = start_past_the_limit(&terminals, &setup, forked, &[])
            .await
            .unwrap();
        let record = held(&store, setup.me, id).unwrap();
        assert_eq!(record.body.state, TerminalState::Running);
        assert!(!record.deleted);
        assert_eq!(
            setup.left(),
            vec![format!("{id}.lock"), format!("{id}.sock")],
            "only the socket and its holder's lock are left"
        );
        let (snapshot, _) = terminals.attach(id).await.unwrap();
        assert_eq!(snapshot.start, 0, "the holder that came up is attached to");
        terminals.close(id).await.unwrap();
        assert!(held(&store, setup.me, id).unwrap().deleted);
        assert!(!setup.socket(id).exists(), "the close ended the holder");
    }

    /// A close that cannot reach a holder whose socket is there, for a reason other than the
    /// holder being gone, fails and keeps the terminal, rather than letting go of a holder
    /// that still runs.
    #[tokio::test]
    async fn a_close_that_cannot_reach_a_running_holder_keeps_the_terminal() {
        let setup = Setup::new();
        let (terminals, store) = setup.daemon();
        let id = setup.start(&terminals, "sleep 60").await;
        // Connecting needs write permission on the socket, so this is refused, not gone.
        set_mode(&setup.socket(id), 0o000);
        let error = terminals.close(id).await.unwrap_err();
        assert!(matches!(error, TerminalError::Holder { .. }), "{error:?}");
        assert!(terminals.inner.entry(id).is_ok(), "the terminal stays live");
        let record = held(&store, setup.me, id).unwrap();
        assert!(!record.deleted);
        assert_eq!(record.body.state, TerminalState::Running);
        set_mode(&setup.socket(id), 0o600);
        terminals.close(id).await.unwrap();
        assert!(held(&store, setup.me, id).unwrap().deleted);
        assert!(!setup.socket(id).exists());
    }

    /// A start whose first record the store cannot take launches nothing, so no holder runs
    /// that no record points to.
    #[tokio::test]
    async fn a_start_the_store_cannot_record_launches_nothing() {
        let setup = Setup::new();
        std::fs::create_dir_all(setup.dir()).unwrap();
        let (terminals, store) = setup.daemon();
        set_mode(setup.data.path(), 0o500);
        let error = terminals
            .start(
                WorkspaceId::random(),
                setup.cwd.path(),
                &argv("sleep 60"),
                &[],
            )
            .await
            .unwrap_err();
        assert!(matches!(error, TerminalError::Store(_)), "{error:?}");
        set_mode(setup.data.path(), 0o700);
        assert!(setup.left().is_empty(), "left behind: {:?}", setup.left());
        assert!(store.lock().unwrap().since(setup.me, 0).unwrap().is_empty());
        assert!(lock(&terminals.inner.live).is_empty());
    }

    /// Dropping a `Terminals` while its runtime runs on, as a daemon replaced in process would,
    /// stops its watchers: an exit after the drop is left for the next `reattach_all`.
    #[tokio::test]
    async fn dropping_terminals_stops_its_watchers() {
        let setup = Setup::new();
        let (terminals, store) = setup.daemon();
        let id = setup.start(&terminals, "sleep 60").await;
        drop(terminals);
        setup.end_holder(id).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(
            held(&store, setup.me, id).unwrap().body.state,
            TerminalState::Running
        );
        let (terminals, store) = setup.daemon();
        terminals.reattach_all().await.unwrap();
        assert_eq!(
            held(&store, setup.me, id).unwrap().body.state,
            TerminalState::Exited
        );
    }

    /// Typing into a terminal whose child has exited is no error: the holder drops the input,
    /// as a finished terminal does.
    #[tokio::test]
    async fn input_to_a_child_that_has_exited_is_dropped_with_no_error() {
        let setup = Setup::new();
        let (terminals, store) = setup.daemon();
        let id = setup.start(&terminals, "exit 0").await;
        until("the record to say exited", || {
            held(&store, setup.me, id).unwrap().body.state == TerminalState::Exited
        })
        .await;
        terminals.input(id, Bytes::from_static(b"x")).await.unwrap();
        terminals.resize(id, 100, 30).await.unwrap();
        terminals.close(id).await.unwrap();
    }

    /// A socket the watcher cannot connect to for a reason other than the holder being gone
    /// is tried again, not taken for an exit.
    #[tokio::test]
    async fn a_watcher_that_cannot_connect_for_now_tries_again() {
        let setup = Setup::new();
        let (terminals, store) = setup.daemon();
        let id = put_running(&store, setup.me);
        let holder = stand_in(
            setup.socket(id),
            vec![(
                vec![
                    Reply::Snapshot {
                        start: 0,
                        bytes: Bytes::new(),
                    },
                    Reply::Status(ChildStatus::Exited(6)),
                ],
                false,
            )],
        );
        // Connecting needs write permission on the socket, so this is refused, not gone.
        set_mode(&setup.socket(id), 0o000);
        terminals.reattach_all().await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(
            held(&store, setup.me, id).unwrap().body.state,
            TerminalState::Running
        );
        set_mode(&setup.socket(id), 0o600);
        until("the record to say exited", || {
            held(&store, setup.me, id).unwrap().body.state == TerminalState::Exited
        })
        .await;
        assert_eq!(held(&store, setup.me, id).unwrap().body.exit_code, Some(6));
        holder.abort();
    }

    /// Two closes at once: one closes the terminal and the other finds it already closed.
    #[tokio::test]
    async fn of_two_closes_at_once_one_closes_and_the_other_is_unknown() {
        let setup = Setup::new();
        let (terminals, store) = setup.daemon();
        let id = setup.start(&terminals, "sleep 60").await;
        let (first, second) = tokio::join!(terminals.close(id), terminals.close(id));
        let results = [first, second];
        assert_eq!(
            results.iter().filter(|r| r.is_ok()).count(),
            1,
            "{results:?}"
        );
        assert!(
            results
                .iter()
                .any(|r| matches!(r, Err(TerminalError::Unknown(gone)) if *gone == id)),
            "{results:?}"
        );
        assert!(held(&store, setup.me, id).unwrap().deleted);
    }

    /// The terminal's tail once `done` holds for it.
    async fn tail_until(
        terminals: &Terminals,
        id: TerminalId,
        max_bytes: usize,
        done: impl Fn(&str) -> bool,
    ) -> String {
        let deadline = Instant::now() + WAIT;
        loop {
            let tail = terminals.tail(id, max_bytes).await.unwrap();
            if done(&tail) {
                return tail;
            }
            assert!(Instant::now() < deadline, "timed out; tail: {tail:?}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn the_tail_of_coloured_output_has_no_escape_byte() {
        let setup = Setup::new();
        let (terminals, _store) = setup.daemon();
        let id = setup
            .start(
                &terminals,
                r"printf '\033[1;31mred\033[0m \033]0;title\007plain\000text\n'; sleep 60",
            )
            .await;
        let tail = tail_until(&terminals, id, 64 * 1024, |tail| tail.contains("plaintext")).await;
        assert!(tail.contains("red plaintext"), "{tail:?}");
        assert!(!tail.contains('\u{1b}'), "an escape byte is left: {tail:?}");
        assert!(!tail.contains('\0'), "a NUL byte is left: {tail:?}");
        assert!(!tail.contains("title"), "a window title is left: {tail:?}");
    }

    #[tokio::test]
    async fn two_megabytes_of_output_give_at_most_64_kb_ending_with_the_last_line() {
        const MAX: usize = 64 * 1024;
        let setup = Setup::new();
        let (terminals, _store) = setup.daemon();
        let line = "0123456789".repeat(10);
        let id = setup
            .start(
                &terminals,
                &format!("yes {line} | head -n 21000; echo the last line; sleep 60"),
            )
            .await;
        let tail = tail_until(&terminals, id, MAX, |tail| {
            tail.trim_end().ends_with("the last line")
        })
        .await;
        assert!(tail.len() <= MAX, "{} bytes", tail.len());
        assert!(
            tail.len() > MAX - line.len() - 2,
            "the tail is cut short: {} bytes",
            tail.len()
        );
        assert!(!tail.contains('\r'), "a carriage return is left");
    }

    #[tokio::test]
    async fn a_multi_byte_character_at_the_cut_is_not_split() {
        let setup = Setup::new();
        let (terminals, _store) = setup.daemon();
        let id = setup.start(&terminals, "printf '€€€€\\n'; sleep 60").await;
        tail_until(&terminals, id, 64, |tail| tail == "€€€€\n").await;
        // "€€€€\n" is 13 bytes, a character starting at each of 0, 3, 6 and 9: the last 8 start
        // inside the second, which is left out whole.
        assert_eq!(terminals.tail(id, 8).await.unwrap(), "€€\n");
        assert_eq!(terminals.tail(id, 9).await.unwrap(), "€€\n");
        assert_eq!(terminals.tail(id, 10).await.unwrap(), "€€€\n");
        assert_eq!(terminals.tail(id, 3).await.unwrap(), "\n");
        assert_eq!(terminals.tail(id, 0).await.unwrap(), "");
    }

    #[tokio::test]
    async fn an_exited_terminal_still_answers_with_its_tail() {
        let setup = Setup::new();
        let (terminals, store) = setup.daemon();
        let id = setup
            .start(&terminals, "echo first; echo final words; exit 4")
            .await;
        until("the record to say exited", || {
            held(&store, setup.me, id).unwrap().body.state == TerminalState::Exited
        })
        .await;
        assert_eq!(
            terminals.tail(id, 1024).await.unwrap(),
            "first\nfinal words\n"
        );

        // And after a restart of the daemon.
        drop(terminals);
        let (terminals, _store) = setup.daemon();
        terminals.reattach_all().await.unwrap();
        assert_eq!(
            terminals.tail(id, 1024).await.unwrap(),
            "first\nfinal words\n"
        );
    }

    /// A gate's setup, `SETUP_LIMITED`, started in a real holder's terminal as `sh()` in
    /// `workspaces/mod.rs` starts it, prints no job-control notice of its own shell, whichever
    /// shell that is: macOS's `/bin/sh` is bash, which stands in for it where there is one.
    #[tokio::test]
    async fn a_setup_that_fails_leaves_the_commands_lines_and_no_shell_notice_in_the_tail() {
        for shell in ["/bin/sh", "bash"] {
            if shell == "bash"
                && std::process::Command::new("bash")
                    .arg("-c")
                    .arg(":")
                    .output()
                    .is_err()
            {
                continue;
            }
            let setup = Setup::new();
            let (terminals, store) = setup.daemon();
            let command =
                "echo one; echo 'test tests::example ... FAILED'; echo to-stderr >&2; exit 100";
            let argv = vec![
                shell.to_string(),
                "-c".into(),
                crate::workspaces::setup_limited().into(),
                "setup".into(),
                "1800".into(),
                command.into(),
            ];
            let id = terminals
                .start(WorkspaceId::random(), setup.cwd.path(), &argv, &[])
                .await
                .unwrap();
            until("the record to say exited", || {
                held(&store, setup.me, id).unwrap().body.state == TerminalState::Exited
            })
            .await;
            let tail = terminals.tail(id, 64 * 1024).await.unwrap();
            assert!(
                tail.contains("test tests::example ... FAILED\n"),
                "{shell}: {tail:?}"
            );
            assert!(tail.contains("to-stderr\n"), "{shell}: {tail:?}");
            assert!(
                tail.contains(&format!("setup failed: {command} exited 100\n")),
                "{shell}: {tail:?}"
            );
            assert!(!tail.contains("Killed"), "{shell}: {tail:?}");
            assert!(!tail.contains("sleep \"$limit\""), "{shell}: {tail:?}");
        }
    }

    /// A failed setup's tail keeps the end of a command's output that is longer than the
    /// holder keeps and has no newline, as `cargo nextest run` writes on a terminal: it draws
    /// every line with cursor moves, megabytes of them, and names the failed tests at the end.
    #[tokio::test]
    async fn a_setup_whose_command_writes_more_than_the_holder_keeps_on_one_line_keeps_its_end() {
        let setup = Setup::new();
        let (terminals, store) = setup.daemon();
        // The name is put together as it is printed, so the `$` and `setup failed:` lines,
        // which repeat the command, do not hold it.
        let command = "head -c 9000000 /dev/zero | tr '\\0' ' '; \
                       printf 'test tests::exa%sple ... FAILED' m; \
                       printf '\\nerror: test run failed\\n'; exit 100";
        let argv = vec![
            "/bin/sh".to_string(),
            "-c".into(),
            crate::workspaces::setup_limited().into(),
            "setup".into(),
            "1800".into(),
            command.into(),
        ];
        let id = terminals
            .start(WorkspaceId::random(), setup.cwd.path(), &argv, &[])
            .await
            .unwrap();
        until("the record to say exited", || {
            held(&store, setup.me, id).unwrap().body.state == TerminalState::Exited
        })
        .await;
        let tail = terminals.tail(id, 64 * 1024).await.unwrap();
        let end = format!(
            "test tests::example ... FAILED\nerror: test run failed\n\
             setup failed: {command} exited 100\n"
        );
        assert!(
            tail.ends_with(&end),
            "{:?}",
            &tail[tail.len().saturating_sub(300)..]
        );
    }

    #[tokio::test]
    async fn the_tail_of_a_terminal_that_is_not_live_is_unknown() {
        let setup = Setup::new();
        let (terminals, _store) = setup.daemon();
        let id = TerminalId::random();
        assert!(matches!(
            terminals.tail(id, 1024).await,
            Err(TerminalError::Unknown(unknown)) if unknown == id
        ));
    }

    #[test]
    fn plain_tail_removes_every_kind_of_escape_sequence() {
        let cases: &[(&[u8], &str)] = &[
            (b"\x1b[1;31mred\x1b[0m", "red"),
            (b"\x1b[?25lhidden cursor\x1b[?25h", "hidden cursor"),
            (b"\x1b[2 qcursor shape", "cursor shape"),
            (b"\x1b]0;title\x07text", "text"),
            (b"\x1b]8;;https://x.test\x1b\\link\x1b]8;;\x1b\\", "link"),
            (b"\x1bPq#0;2;0;0;0\x1b\\after dcs", "after dcs"),
            (b"\x1b_kitty\x1b\\after apc", "after apc"),
            (b"\x1b(Bcharset\x1b=keypad\x1b>", "charsetkeypad"),
            ("\u{9b}31mc1 csi".as_bytes(), "c1 csi"),
            ("\u{9d}0;title\u{9c}c1 osc".as_bytes(), "c1 osc"),
            (b"n\0u\0l", "nul"),
            (b"bell\x07 back\x08space", "bell backspace"),
            (b"a\tb", "a\tb"),
            (b"one\r\ntwo\r\n", "one\ntwo\n"),
            (
                b"progress 10%\rprogress 100%\r\ndone",
                "progress 10%progress 100%\ndone",
            ),
            (
                b"frame 1\n\x1b[2K\x1b[1A\x1b[2K\x1b[Gframe 2\n",
                "frame 1\nframe 2\n",
            ),
            (b"\x1bXsos\x1b\\a", "a"),
            (b"\x1b^pm\x07b", "b"),
            ("\u{90}dcs\u{9c}c".as_bytes(), "c"),
            ("\u{98}sos\u{9c}d".as_bytes(), "d"),
            ("\u{9e}pm\u{9c}e".as_bytes(), "e"),
            ("\u{9f}apc\u{9c}f".as_bytes(), "f"),
            (b"\x1b[31", ""),
            (b"\x1b]0;never ended\nnext line", "\nnext line"),
            (b"\x1b[3\x1b[0mkept", "kept"),
            (b"bad \xff byte", "bad \u{fffd} byte"),
        ];
        for (bytes, plain) in cases {
            assert_eq!(
                plain_tail(bytes, usize::MAX),
                *plain,
                "{:?}",
                String::from_utf8_lossy(bytes)
            );
        }
    }

    #[test]
    fn plain_tail_cuts_on_a_character_boundary_after_removing_escapes() {
        assert_eq!(plain_tail("aé".as_bytes(), 2), "é");
        assert_eq!(plain_tail("aé".as_bytes(), 1), "");
        assert_eq!(plain_tail("a🙂".as_bytes(), 3), "");
        assert_eq!(plain_tail("a🙂".as_bytes(), 4), "🙂");
        assert_eq!(plain_tail(b"ab\x1b[31mcd", 3), "bcd");
        assert_eq!(plain_tail(b"short", 64), "short");
        assert_eq!(plain_tail(b"", 64), "");
    }
}
