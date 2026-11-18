//! Starts holders, reattaches to them after a restart, and relays their bytes.
//!
//! Each terminal is a `foundry-holder` process serving a socket at
//! `<data dir>/terminals/<id>.sock`. The holder outlives this daemon, so a daemon that starts
//! again finds its terminals through their records in the store and their sockets, and attaches
//! to them again with [`Terminals::reattach_all`].
//!
//! Each terminal's record is kept current: running once it is started, exited when the holder
//! reports the child's exit or when its socket is gone, and deleted once it is closed. A record
//! is written before its holder is started, so a daemon that stops in between leaves a record
//! that `reattach_all` finds with no socket and marks exited.

use std::collections::HashMap;
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
use tokio::net::UnixStream;
use tokio::task::AbortHandle;

use crate::store::{Store, StoreError};

/// How long a watcher waits before it connects again to a holder that dropped its connection.
const RECONNECT_AFTER: Duration = Duration::from_millis(200);

#[derive(Debug, thiserror::Error)]
pub enum TerminalError {
    #[error("terminal {0} is not a live terminal of this machine")]
    Unknown(TerminalId),
    #[error("terminal {id}: the holder could not be started: {reason}")]
    Start { id: TerminalId, reason: String },
    #[error("terminal {id}: the holder refused the request: {reason}")]
    Refused { id: TerminalId, reason: String },
    #[error("terminal {id}: {source}")]
    Holder {
        id: TerminalId,
        source: protocol::Error,
    },
    #[error(transparent)]
    Store(#[from] StoreError),
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

/// This machine's terminals.
pub struct Terminals {
    shared: Arc<Shared>,
    dir: PathBuf,
    holder: PathBuf,
}

struct Shared {
    me: MachineId,
    store: Arc<Mutex<Store>>,
    live: Mutex<HashMap<TerminalId, Arc<Entry>>>,
}

/// A terminal whose holder this daemon is attached to.
struct Entry {
    socket: PathBuf,
    /// The body of the terminal's newest record.
    body: Mutex<Terminal>,
    /// The connection input, resize and close are sent on, so they reach the holder in order.
    control: tokio::sync::Mutex<Option<UnixStream>>,
    watcher: Mutex<Option<AbortHandle>>,
}

impl Terminals {
    /// The terminals of machine `me`, whose sockets live under `data_dir`, started with the
    /// `foundry-holder` binary at `holder`. None is attached until [`Self::reattach_all`].
    pub fn new(me: MachineId, store: Arc<Mutex<Store>>, data_dir: &Path, holder: PathBuf) -> Self {
        Self {
            shared: Arc::new(Shared {
                me,
                store,
                live: Mutex::default(),
            }),
            dir: data_dir.join("terminals"),
            holder,
        }
    }

    /// Starts `argv` in `cwd` with `env` set over the daemon's own environment, in a new
    /// terminal of `workspace`.
    pub async fn start(
        &self,
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
        // `reattach_all` closes.
        self.shared.write(&body, false)?;
        let socket = self.socket(id);
        let env_file = self.dir.join(format!("{id}.env"));
        let started = self.launch(&socket, cwd, &env_file, argv, env).await;
        // The holder deletes the env file once read. One it never read may hold secrets.
        let _ = std::fs::remove_file(&env_file);
        if let Err(reason) = started {
            let ended = Terminal {
                state: TerminalState::Exited,
                ..body
            };
            self.shared.write(&ended, true)?;
            return Err(TerminalError::Start { id, reason });
        }
        self.adopt(body, socket);
        Ok(id)
    }

    /// The terminal's kept screen and scrollback, then its live bytes. The stream ends when
    /// the child exits or the holder goes away.
    pub async fn attach(
        &self,
        id: TerminalId,
    ) -> Result<(Snapshot, BoxStream<'static, Bytes>), TerminalError> {
        let entry = self.shared.entry(id)?;
        let mut stream = UnixStream::connect(&entry.socket)
            .await
            .map_err(|e| TerminalError::holder(id, e))?;
        protocol::write_request(&mut stream, &Request::Attach)
            .await
            .map_err(|e| TerminalError::holder(id, e))?;
        let snapshot = match protocol::read_reply(&mut stream).await {
            Ok(Some(Reply::Snapshot { start, bytes })) => Snapshot { start, bytes },
            Ok(Some(Reply::Refused(reason))) => return Err(TerminalError::Refused { id, reason }),
            Ok(_) => {
                let reason = "the holder sent no snapshot".to_string();
                return Err(TerminalError::Refused { id, reason });
            }
            Err(e) => return Err(TerminalError::holder(id, e)),
        };
        let output = futures::stream::unfold(Some(stream), |stream| async move {
            let mut stream = stream?;
            loop {
                match protocol::read_reply(&mut stream).await {
                    Ok(Some(Reply::Output(bytes))) => return Some((bytes, Some(stream))),
                    Ok(Some(Reply::Status(ChildStatus::Running) | Reply::Snapshot { .. })) => {}
                    // The holder sends every byte before it reports the exit.
                    _ => return None,
                }
            }
        });
        Ok((snapshot, output.boxed()))
    }

    /// Bytes typed into the terminal.
    pub async fn input(&self, id: TerminalId, bytes: Bytes) -> Result<(), TerminalError> {
        self.control(id, &Request::Input(bytes)).await
    }

    pub async fn resize(&self, id: TerminalId, cols: u16, rows: u16) -> Result<(), TerminalError> {
        self.control(id, &Request::Resize { cols, rows }).await
    }

    /// Ends the terminal: hangs up its child, stops its holder and deletes its record. A holder
    /// that is already gone is no error.
    pub async fn close(&self, id: TerminalId) -> Result<(), TerminalError> {
        let entry = self
            .shared
            .lock_live()
            .remove(&id)
            .ok_or(TerminalError::Unknown(id))?;
        if let Some(watcher) = lock(&entry.watcher).take() {
            watcher.abort();
        }
        let _ = send(&entry, id, &Request::Close).await;
        let body = Terminal {
            state: TerminalState::Exited,
            ..lock(&entry.body).clone()
        };
        self.shared.write(&body, true)?;
        Ok(())
    }

    /// Attaches to every holder this machine's records name. A running terminal whose socket
    /// is gone is recorded as exited with an unknown code. Run once at start.
    pub async fn reattach_all(&self) -> Result<(), TerminalError> {
        let held: Vec<Terminal> = {
            let store = lock(&self.shared.store);
            store
                .since(self.shared.me, 0)?
                .into_iter()
                .filter_map(|record| match record {
                    AnyRecord::Terminal(r) if !r.deleted => Some(r.body),
                    _ => None,
                })
                .collect()
        };
        for body in held {
            if self.shared.lock_live().contains_key(&body.id) {
                continue;
            }
            let socket = self.socket(body.id);
            if UnixStream::connect(&socket).await.is_ok() {
                // The watcher records an exit the holder reports.
                self.adopt(body, socket);
            } else if body.state == TerminalState::Running {
                let ended = Terminal {
                    state: TerminalState::Exited,
                    exit_code: None,
                    ..body
                };
                self.shared.write(&ended, false)?;
            }
        }
        Ok(())
    }

    fn socket(&self, id: TerminalId) -> PathBuf {
        self.dir.join(format!("{id}.sock"))
    }

    /// Runs the holder, which returns once its socket is served. The error is the holder's own
    /// reason, which never quotes the env file's contents.
    async fn launch(
        &self,
        socket: &Path,
        cwd: &Path,
        env_file: &Path,
        argv: &[String],
        env: &[(String, String)],
    ) -> Result<(), String> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)
            .map_err(|e| format!("{}: {e}", self.dir.display()))?;
        pty::write_env_file(env_file, env).map_err(|e| format!("env file: {e}"))?;
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

    /// Keeps `body`'s terminal live and watches its holder for the child's exit.
    fn adopt(&self, body: Terminal, socket: PathBuf) {
        let id = body.id;
        let entry = Arc::new(Entry {
            socket,
            body: Mutex::new(body),
            control: tokio::sync::Mutex::new(None),
            watcher: Mutex::new(None),
        });
        self.shared.lock_live().insert(id, entry.clone());
        let watcher = tokio::spawn(watch(self.shared.clone(), id, entry.socket.clone()));
        *lock(&entry.watcher) = Some(watcher.abort_handle());
    }

    async fn control(&self, id: TerminalId, request: &Request) -> Result<(), TerminalError> {
        let entry = self.shared.entry(id)?;
        send(&entry, id, request).await
    }
}

impl Drop for Terminals {
    /// Stops watching. The holders keep running.
    fn drop(&mut self) {
        for entry in self.shared.lock_live().values() {
            if let Some(watcher) = lock(&entry.watcher).take() {
                watcher.abort();
            }
        }
    }
}

impl Shared {
    fn lock_live(&self) -> MutexGuard<'_, HashMap<TerminalId, Arc<Entry>>> {
        lock(&self.live)
    }

    fn entry(&self, id: TerminalId) -> Result<Arc<Entry>, TerminalError> {
        self.lock_live()
            .get(&id)
            .cloned()
            .ok_or(TerminalError::Unknown(id))
    }

    /// Writes `body` as this machine's newest record of its terminal.
    fn write(&self, body: &Terminal, deleted: bool) -> Result<(), StoreError> {
        let mut store = lock(&self.store);
        let record = Record {
            owner: self.me,
            version: store.own_next_version()?,
            deleted,
            body: body.clone(),
        };
        store.apply(&AnyRecord::Terminal(record), self.me)?;
        Ok(())
    }

    /// Records that a live terminal's child has exited, once. A terminal closed meanwhile keeps
    /// its deleted record.
    fn exited(&self, id: TerminalId, code: Option<i32>) {
        let live = self.lock_live();
        let Some(entry) = live.get(&id) else {
            return;
        };
        let mut body = lock(&entry.body);
        if body.state == TerminalState::Exited {
            return;
        }
        let ended = Terminal {
            state: TerminalState::Exited,
            exit_code: code,
            ..body.clone()
        };
        match self.write(&ended, false) {
            Ok(()) => *body = ended,
            Err(error) => tracing::warn!(terminal = %id, %error, "terminal exit not recorded"),
        }
    }
}

/// Sends `request` on the entry's control connection. The holder may have dropped a held
/// connection since its last use, so a failed write is tried once more on a new one.
async fn send(entry: &Entry, id: TerminalId, request: &Request) -> Result<(), TerminalError> {
    let mut control = entry.control.lock().await;
    if let Some(stream) = control.as_mut()
        && protocol::write_request(stream, request).await.is_ok()
    {
        return Ok(());
    }
    *control = None;
    let mut stream = UnixStream::connect(&entry.socket)
        .await
        .map_err(|e| TerminalError::holder(id, e))?;
    protocol::write_request(&mut stream, request)
        .await
        .map_err(|e| TerminalError::holder(id, e))?;
    *control = Some(stream);
    Ok(())
}

/// Stays attached to a terminal's holder until it reports the child's exit or its socket is
/// gone, and records which. A holder that drops the connection while running is attached again.
async fn watch(shared: Arc<Shared>, id: TerminalId, socket: PathBuf) {
    loop {
        let Ok(mut stream) = UnixStream::connect(&socket).await else {
            shared.exited(id, None);
            return;
        };
        if protocol::write_request(&mut stream, &Request::Attach)
            .await
            .is_ok()
        {
            loop {
                match protocol::read_reply(&mut stream).await {
                    Ok(Some(Reply::Status(ChildStatus::Exited(code)))) => {
                        shared.exited(id, (code != UNKNOWN_EXIT).then_some(code));
                        return;
                    }
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => break,
                }
            }
        }
        tokio::time::sleep(RECONNECT_AFTER).await;
    }
}

/// Locks `mutex`, taking the value a panicked holder of the lock left: every value kept under
/// these locks is whole between statements.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;
    use std::time::{Duration, Instant};

    use foundry_holder::protocol::{MAX_PENDING_INPUT, Request};
    use foundry_proto::record::{AnyRecord, Record};
    use foundry_proto::state::TerminalState;
    use futures::StreamExt as _;
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
        assert_eq!(start, 0, "a new terminal's snapshot starts at its first byte");
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
        let id = setup
            .start(&terminals, &format!("seq 0 {last}; sleep 60"))
            .await;
        let (_, mut stream) = terminals.attach(id).await.unwrap();
        let mut seen = Vec::new();
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
        let before: u64 = (0..first).map(|n| n.to_string().len() as u64 + 2).sum();
        assert_eq!(snapshot.start, before);
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
        terminals.close(id).await.unwrap();
        assert!(held(&store, setup.me, id).unwrap().deleted);
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
        assert_eq!(setup.left(), vec![format!("{id}.sock")], "only the socket is left");
        let mode = std::fs::metadata(setup.dir()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "the terminals directory is this user's only");
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
        assert!(!setup.socket(id).exists(), "close waits for the holder to end");
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

    /// The caller of `start` going away, as an HTTP handler does when its client hangs up,
    /// leaves the terminal started and live, not running with nothing attached to it.
    #[tokio::test]
    async fn a_start_whose_caller_goes_away_still_finishes() {
        let setup = Setup::new();
        let (terminals, store) = setup.daemon();
        let started = terminals.start(
            WorkspaceId::random(),
            setup.cwd.path(),
            &argv("sleep 60"),
            &[],
        );
        // Polled until the holder is being started, then dropped.
        let _ = tokio::time::timeout(Duration::from_millis(5), started).await;
        let id = {
            let store = store.lock().unwrap();
            let records = store.since(setup.me, 0).unwrap();
            let [AnyRecord::Terminal(record)] = records.as_slice() else {
                panic!("expected one terminal record, got {records:?}");
            };
            record.body.id
        };
        until("the holder's socket", || setup.socket(id).exists()).await;
        until("the terminal to be live", || terminals.inner.entry(id).is_ok()).await;
        terminals.close(id).await.unwrap();
        assert!(held(&store, setup.me, id).unwrap().deleted);
    }

    #[tokio::test]
    async fn a_command_that_cannot_start_is_an_error_and_leaves_no_live_record() {
        let setup = Setup::new();
        let secret = "sk-not-for-any-message";
        let env = [("ANTHROPIC_AUTH_TOKEN".to_string(), secret.to_string())];
        for (holder, argv, says) in [
            (holder(), vec!["/no/such/program".to_string()], "/no/such/program"),
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
}
