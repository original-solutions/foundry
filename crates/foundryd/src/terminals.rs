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
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use foundry_proto::ids::{MachineId, TerminalId, WorkspaceId};
use foundry_proto::record::Terminal;
use futures::stream::BoxStream;
use tokio::net::UnixStream;
use tokio::task::AbortHandle;

use crate::store::{Store, StoreError};

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
        source: foundry_holder::protocol::Error,
    },
    #[error(transparent)]
    Store(#[from] StoreError),
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
    pub fn new(
        me: MachineId,
        store: Arc<Mutex<Store>>,
        data_dir: &Path,
        holder: PathBuf,
    ) -> Self {
        let _ = (me, store, data_dir, holder);
        todo!()
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
        let _ = (workspace, cwd, argv, env);
        todo!()
    }

    /// The terminal's kept screen and scrollback, then its live bytes. The stream ends when
    /// the child exits or the holder goes away.
    pub async fn attach(
        &self,
        id: TerminalId,
    ) -> Result<(Snapshot, BoxStream<'static, Bytes>), TerminalError> {
        let _ = id;
        todo!()
    }

    /// Bytes typed into the terminal.
    pub async fn input(&self, id: TerminalId, bytes: Bytes) -> Result<(), TerminalError> {
        let _ = (id, bytes);
        todo!()
    }

    pub async fn resize(&self, id: TerminalId, cols: u16, rows: u16) -> Result<(), TerminalError> {
        let _ = (id, cols, rows);
        todo!()
    }

    /// Ends the terminal: hangs up its child, stops its holder and deletes its record.
    pub async fn close(&self, id: TerminalId) -> Result<(), TerminalError> {
        let _ = id;
        todo!()
    }

    /// Attaches to every holder this machine's records name. A running terminal whose socket
    /// is gone is recorded as exited with an unknown code. Run once at start.
    pub async fn reattach_all(&self) -> Result<(), TerminalError> {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

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
            let store = Arc::new(Mutex::new(
                Store::open(self.data.path().join("foundry.db")).unwrap(),
            ));
            let terminals = Terminals::new(self.me, store.clone(), self.data.path(), holder());
            (terminals, store)
        }

        fn socket(&self, id: TerminalId) -> PathBuf {
            self.data.path().join("terminals").join(format!("{id}.sock"))
        }

        fn argv(script: &str) -> Vec<String> {
            vec!["/bin/sh".into(), "-c".into(), script.into()]
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
                    let close = foundry_holder::protocol::Request::Close.encode();
                    let _ = stream.write_all(&close);
                }
            }
        }
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
    async fn read_until(
        stream: &mut BoxStream<'static, Bytes>,
        seen: &mut Vec<u8>,
        done: impl Fn(&[u8]) -> bool,
    ) {
        let deadline = Instant::now() + WAIT;
        while !done(seen) {
            let left = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(left, stream.next()).await {
                Ok(Some(bytes)) => seen.extend_from_slice(&bytes),
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
            .map(|line| line.parse().unwrap_or_else(|_| panic!("not a count: {line:?}")))
            .collect()
    }

    const COUNT_TO: u32 = 300;

    /// Prints 0 to `COUNT_TO - 1`, one a line and a few milliseconds apart, then waits.
    fn counter() -> Vec<String> {
        Setup::argv(&format!(
            "i=0; while [ $i -lt {COUNT_TO} ]; do echo $i; i=$((i+1)); sleep 0.01; done; sleep 60"
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
            assert!(skip <= snapshot.bytes.len(), "the snapshot ends before the bytes seen");
            seen.extend_from_slice(&snapshot.bytes[skip..]);
            let last = format!("{}\n", COUNT_TO - 1);
            read_until(&mut stream, &mut seen, |seen| {
                contains(seen, last.replace('\n', "\r\n").as_bytes())
            })
            .await;
            assert_eq!(counts(&seen), (0..COUNT_TO).collect::<Vec<_>>());
            terminals.close(id).await.unwrap();
        });
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
                &Setup::argv("echo last words; sleep 0.5; exit 3"),
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
        assert!(!exited.deleted, "an exited terminal is kept until it is closed");
        let rest: Vec<Bytes> = tokio::time::timeout(WAIT, stream.collect())
            .await
            .expect("the stream ends once the child has exited");
        let _ = rest;

        // A daemon started again still shows the final screen, and writes no new record.
        drop(terminals);
        let (terminals, store) = setup.daemon();
        terminals.reattach_all().await.unwrap();
        let (snapshot, _) = terminals.attach(id).await.unwrap();
        assert!(contains(&snapshot.bytes, b"last words"));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(held(&store, setup.me, id).unwrap(), exited);
        terminals.close(id).await.unwrap();
    }

    #[tokio::test]
    async fn a_holder_whose_socket_is_gone_is_recorded_as_exited_with_an_unknown_code() {
        let setup = Setup::new();
        let (terminals, store) = setup.daemon();
        let gone = terminals
            .start(
                WorkspaceId::random(),
                setup.cwd.path(),
                &Setup::argv("sleep 60"),
                &[],
            )
            .await
            .unwrap();
        let stale = terminals
            .start(
                WorkspaceId::random(),
                setup.cwd.path(),
                &Setup::argv("sleep 60"),
                &[],
            )
            .await
            .unwrap();
        drop(terminals);

        // While no daemon runs, one holder ends and takes its socket with it, and the other
        // dies without removing its socket, which no one is then listening on.
        let mut stream = UnixStream::connect(setup.socket(gone)).await.unwrap();
        foundry_holder::protocol::write_request(
            &mut stream,
            &foundry_holder::protocol::Request::Close,
        )
        .await
        .unwrap();
        until("the closed holder's socket to go", || {
            !setup.socket(gone).exists()
        })
        .await;
        let mut stream = UnixStream::connect(setup.socket(stale)).await.unwrap();
        foundry_holder::protocol::write_request(
            &mut stream,
            &foundry_holder::protocol::Request::Close,
        )
        .await
        .unwrap();
        until("the second holder's socket to go", || {
            !setup.socket(stale).exists()
        })
        .await;
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
    }

    #[tokio::test]
    async fn input_and_resize_reach_the_child_in_order() {
        let setup = Setup::new();
        let (terminals, _) = setup.daemon();
        let id = terminals
            .start(
                WorkspaceId::random(),
                setup.cwd.path(),
                &Setup::argv("stty -echo; read a; read b; echo \"got $a $b\"; stty size; sleep 60"),
                &[],
            )
            .await
            .unwrap();
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
        assert!(contains(&seen, b"got one two"), "{:?}", String::from_utf8_lossy(&seen));
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
                &Setup::argv("echo \"[$FOUNDRY_TEST_VALUE] $(pwd)\"; sleep 60"),
                &env,
            )
            .await
            .unwrap();
        let (snapshot, mut stream) = terminals.attach(id).await.unwrap();
        let mut seen = snapshot.bytes.to_vec();
        read_until(&mut stream, &mut seen, |seen| contains(seen, b"]")).await;
        assert!(contains(&seen, b"[from the env]"), "{:?}", String::from_utf8_lossy(&seen));
        let cwd = setup.cwd.path().canonicalize().unwrap();
        read_until(&mut stream, &mut seen, |seen| {
            contains(seen, cwd.to_string_lossy().as_bytes())
        })
        .await;
        let left: Vec<_> = std::fs::read_dir(setup.data.path().join("terminals"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, vec![format!("{id}.sock")], "only the socket is left");
        terminals.close(id).await.unwrap();
    }

    #[tokio::test]
    async fn close_ends_the_holder_and_deletes_the_record() {
        let setup = Setup::new();
        let (terminals, store) = setup.daemon();
        let id = terminals
            .start(
                WorkspaceId::random(),
                setup.cwd.path(),
                &Setup::argv("sleep 60"),
                &[],
            )
            .await
            .unwrap();
        terminals.close(id).await.unwrap();
        let record = held(&store, setup.me, id).unwrap();
        assert!(record.deleted);
        until("the holder's socket to go", || !setup.socket(id).exists()).await;
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
    async fn a_command_that_cannot_start_is_an_error_and_leaves_no_live_record() {
        let setup = Setup::new();
        let (terminals, store) = setup.daemon();
        let secret = "sk-not-for-any-message";
        let error = terminals
            .start(
                WorkspaceId::random(),
                setup.cwd.path(),
                &["/no/such/program".to_string()],
                &[("ANTHROPIC_AUTH_TOKEN".to_string(), secret.to_string())],
            )
            .await
            .unwrap_err();
        let TerminalError::Start { id, ref reason } = error else {
            panic!("expected a start error, got {error:?}");
        };
        assert!(reason.contains("/no/such/program"), "{reason}");
        assert!(!error.to_string().contains(secret), "{error}");
        assert!(held(&store, setup.me, id).unwrap().deleted);
        let left = std::fs::read_dir(setup.data.path().join("terminals"))
            .unwrap()
            .count();
        assert_eq!(left, 0, "no env file or socket is left");
    }
}
