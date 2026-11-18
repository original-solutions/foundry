//! The `foundry-holder` binary, driven over its socket the way `foundryd` drives it.

use std::fs::File;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use foundry_holder::protocol::{self, ChildStatus, Reply, Request};
use foundry_holder::pty;
use foundry_holder::scrollback::MAX_BYTES;
use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};
use tempfile::TempDir;
use tokio::net::UnixStream;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(20);

/// A started holder. Dropping it closes the terminal so no test leaves a process behind.
struct Holder {
    dir: TempDir,
    socket: PathBuf,
}

impl Holder {
    fn start(argv: &[&str], env: &[(&str, &str)]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("h.sock");
        let output = run_starter(holder_command(&dir, &socket, argv, env));
        assert!(
            output.status.success(),
            "foundry-holder failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Holder { dir, socket }
    }

    fn env_file(&self) -> PathBuf {
        self.dir.path().join("env.json")
    }

    async fn connect(&self) -> UnixStream {
        UnixStream::connect(&self.socket).await.unwrap()
    }

    async fn status(&self) -> ChildStatus {
        let mut stream = self.connect().await;
        protocol::write_request(&mut stream, &Request::Status)
            .await
            .unwrap();
        match next(&mut stream).await {
            Reply::Status(status) => status,
            other => panic!("expected a status, got {other:?}"),
        }
    }

    async fn wait_for_exit(&self, within: Duration) -> i32 {
        let deadline = Instant::now() + within;
        loop {
            if let ChildStatus::Exited(code) = self.status().await {
                return code;
            }
            assert!(Instant::now() < deadline, "the child did not exit");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn attach(&self) -> Attached {
        let mut stream = self.connect().await;
        protocol::write_request(&mut stream, &Request::Attach)
            .await
            .unwrap();
        let (start, snapshot) = match next(&mut stream).await {
            Reply::Snapshot { start, bytes } => (start, bytes.to_vec()),
            other => panic!("expected a snapshot first, got {other:?}"),
        };
        Attached {
            stream,
            start,
            seen: snapshot.clone(),
            snapshot,
            exit: None,
        }
    }

    async fn send(&self, request: Request) {
        let mut stream = self.connect().await;
        protocol::write_request(&mut stream, &request)
            .await
            .unwrap();
    }
}

impl Drop for Holder {
    fn drop(&mut self) {
        if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&self.socket) {
            let _ = stream.write_all(&Request::Close.encode());
        }
    }
}

fn holder_command(dir: &TempDir, socket: &Path, argv: &[&str], env: &[(&str, &str)]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_foundry-holder"));
    command.args(holder_args(dir, socket, argv, env));
    command
}

/// Runs the starter with its output captured, as `foundryd` does. It must return once the holder
/// is ready: a holder that kept the starter's stdout or stderr would hold them open for the
/// terminal's whole life.
fn run_starter(mut command: Command) -> Output {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(command.output());
    });
    rx.recv_timeout(WAIT)
        .expect("the starter did not return: the holder kept its stdout or stderr open")
        .unwrap()
}

/// Writes the env file and returns the holder's arguments after the program name.
fn holder_args(dir: &TempDir, socket: &Path, argv: &[&str], env: &[(&str, &str)]) -> Vec<String> {
    let env_file = dir.path().join("env.json");
    let vars: Vec<(String, String)> = env
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    pty::write_env_file(&env_file, &vars).unwrap();
    let mut args = vec![
        "--socket".to_string(),
        socket.display().to_string(),
        "--cwd".to_string(),
        dir.path().display().to_string(),
        "--env-file".to_string(),
        env_file.display().to_string(),
        "--".to_string(),
    ];
    args.extend(argv.iter().map(|a| a.to_string()));
    args
}

async fn next(stream: &mut UnixStream) -> Reply {
    timeout(WAIT, protocol::read_reply(stream))
        .await
        .expect("no reply in time")
        .unwrap()
        .expect("the holder closed the connection")
}

struct Attached {
    stream: UnixStream,
    /// Stream offset of the snapshot's first byte.
    start: u64,
    snapshot: Vec<u8>,
    seen: Vec<u8>,
    exit: Option<i32>,
}

impl Attached {
    /// Reads output until `seen` contains `needle`.
    async fn read_until(&mut self, needle: &[u8]) {
        while !contains(&self.seen, needle) {
            self.read_one().await;
        }
    }

    /// Reads until the holder says the child exited, and returns the code.
    async fn read_until_exit(&mut self) -> i32 {
        while self.exit.is_none() {
            self.read_one().await;
        }
        self.exit.unwrap()
    }

    async fn read_one(&mut self) {
        let reply = next(&mut self.stream).await;
        self.take(reply);
    }

    /// Reads until the holder closes the connection. A frame cut off by the close is not counted.
    async fn read_to_end(&mut self) {
        while let Some(reply) = read_or_end(&mut self.stream).await {
            self.take(reply);
        }
    }

    fn take(&mut self, reply: Reply) {
        match reply {
            Reply::Output(bytes) => self.seen.extend_from_slice(&bytes),
            Reply::Status(ChildStatus::Exited(code)) => self.exit = Some(code),
            other => panic!("unexpected {other:?} on an attached stream"),
        }
    }

    async fn send(&mut self, request: Request) {
        protocol::write_request(&mut self.stream, &request)
            .await
            .unwrap();
    }
}

/// The next reply, or `None` once the holder has closed the connection.
async fn read_or_end(stream: &mut UnixStream) -> Option<Reply> {
    timeout(WAIT, protocol::read_reply(stream))
        .await
        .expect("the holder neither replied nor closed the connection")
        .unwrap()
}

/// Writes requests until the holder's side of the connection is gone. False if it never goes.
async fn writes_fail(stream: &mut UnixStream) -> bool {
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        let input = Request::Input(Bytes::from_static(b"x"));
        if protocol::write_request(stream, &input).await.is_err() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

/// The lock file a holder serving `socket` keeps while it runs.
fn lock_file(socket: &Path) -> PathBuf {
    socket.with_extension("lock")
}

/// Takes the lock on `path` shared and without waiting, as `foundryd` checks it: only a
/// holder's exclusive lock refuses it.
fn try_lock(path: &Path) -> Result<Flock<File>, Errno> {
    let file = File::open(path).unwrap();
    Flock::lock(file, FlockArg::LockSharedNonblock).map_err(|(_, e)| e)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn position(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
        .unwrap_or_else(|| panic!("{:?} not found", String::from_utf8_lossy(needle)))
}

#[tokio::test]
async fn output_is_delivered_byte_for_byte() {
    let dir = tempfile::tempdir().unwrap();
    let payload: Vec<u8> = (0..64).flat_map(|_| 0u8..=255).collect();
    let file = dir.path().join("payload.bin");
    std::fs::write(&file, &payload).unwrap();
    let script = format!(
        "stty raw -echo; printf R; read go; cat '{}'",
        file.display()
    );
    let holder = Holder::start(&["sh", "-c", &script], &[]);

    let mut client = holder.attach().await;
    // R is printed once the terminal is raw, so the go line is neither echoed nor translated.
    client.read_until(b"R").await;
    client
        .send(Request::Input(Bytes::from_static(b"go\n")))
        .await;
    assert_eq!(client.read_until_exit().await, 0);
    assert_eq!(&client.seen[position(&client.seen, b"R") + 1..], payload);
}

#[tokio::test]
async fn a_client_that_attaches_late_gets_the_earlier_output_first() {
    let holder = Holder::start(
        &["sh", "-c", "echo early; read x; echo late-$x; sleep 60"],
        &[],
    );
    let mut first = holder.attach().await;
    first.read_until(b"early").await;

    let mut late = holder.attach().await;
    assert!(contains(&late.snapshot, b"early"), "{:?}", late.snapshot);
    late.send(Request::Input(Bytes::from_static(b"go\r"))).await;
    late.read_until(b"late-go").await;
    assert!(position(&late.seen, b"early") < position(&late.seen, b"late-go"));
    first.read_until(b"late-go").await;
}

#[tokio::test]
async fn scrollback_holds_10000_lines_and_drops_the_oldest() {
    let holder = Holder::start(&["sh", "-c", &format!("{NO_OPOST}; seq 1 10050")], &[]);
    assert_eq!(holder.wait_for_exit(WAIT).await, 0);
    let client = holder.attach().await;
    let counter = counter_stream(b"", 10_050);
    let dropped = counter_stream(b"", 50).len();
    assert_eq!(client.snapshot, &counter[dropped..]);
    assert_eq!(client.start, dropped as u64);
}

#[tokio::test]
async fn a_flood_with_no_client_attached_finishes_in_bounded_memory() {
    // No newlines, so the byte cap, not the line cap, is what bounds the scrollback.
    let holder = Holder::start(
        &["sh", "-c", "head -c 200000000 /dev/zero | tr '\\0' a"],
        &[],
    );
    let rss = PeakRss::watch(holder_pid(&holder.socket));
    let code = holder.wait_for_exit(Duration::from_secs(180)).await;
    let peak_mb = rss.stop();

    assert_eq!(code, 0);
    assert!(peak_mb < 50, "the holder peaked at {peak_mb} MB");
    let client = holder.attach().await;
    assert!(!client.snapshot.is_empty());
    assert!(client.snapshot.len() <= MAX_BYTES);
    assert!(client.snapshot.iter().all(|&b| b == b'a'));
    assert_eq!(client.start + client.snapshot.len() as u64, 200_000_000);
}

#[tokio::test]
async fn a_client_that_stops_reading_is_dropped_without_blocking_the_child() {
    // A counter the stalled client's stream is checked against, then 100 MB to push the holder.
    let script = &format!(
        "{NO_OPOST} -echo; printf R; read x; seq 1 999999; head -c 100000000 /dev/zero | tr '\\0' a"
    );
    let holder = Holder::start(&["sh", "-c", script], &[]);
    let mut driver = holder.attach().await;
    driver.read_until(b"R").await;
    drop(driver);

    let mut stalled = holder.attach().await;
    let rss = PeakRss::watch(holder_pid(&holder.socket));
    holder.send(Request::Input(Bytes::from_static(b"\n"))).await;
    let code = holder.wait_for_exit(Duration::from_secs(180)).await;
    let peak_mb = rss.stop();
    assert_eq!(code, 0);
    assert!(peak_mb < 50, "the holder peaked at {peak_mb} MB");

    // What the stalled client got is the start of the stream with nothing skipped, then EOF.
    stalled.read_to_end().await;
    assert_eq!(stalled.start, 0);
    assert_eq!(stalled.exit, None, "the stalled client was never dropped");
    let counter = counter_stream(b"R", 999_999);
    let mut expected = counter.clone();
    expected.resize(stalled.seen.len().max(counter.len()), b'a');
    expected.truncate(stalled.seen.len());
    assert!(
        stalled.seen == expected,
        "the stalled client's stream has a gap"
    );

    let fresh = holder.attach().await;
    assert!(fresh.snapshot.iter().all(|&b| b == b'a'));
    assert_eq!(
        fresh.start + fresh.snapshot.len() as u64,
        counter.len() as u64 + 100_000_000
    );
}

#[tokio::test]
async fn a_client_that_keeps_reading_gets_every_byte_of_many_small_writes() {
    // seq writes each line on its own, so the holder sees many small reads. Even framed one line
    // at a time the stream stays under the holder's 4 MiB backlog, so the client is never cut off
    // however far behind it reads. A child that floods faster than a client reads, past that
    // backlog, has the client cut off by design.
    let holder = Holder::start(
        &[
            "sh",
            "-c",
            &format!("{NO_OPOST} -echo; printf R; read x; seq 1 200000"),
        ],
        &[],
    );
    let mut client = holder.attach().await;
    client.read_until(b"R").await;
    client.send(Request::Input(Bytes::from_static(b"\n"))).await;
    assert_eq!(client.read_until_exit().await, 0);
    assert!(
        client.seen == counter_stream(b"R", 200_000),
        "the stream differs from the counter"
    );
}

/// Turns off the terminal's output processing for tests that compare output to an exact stream.
/// The kernel's newline translation is not the holder's, and under load macOS's sometimes writes
/// `\r\r\n` for one newline.
const NO_OPOST: &str = "stty -opost";

/// `prefix` then `seq 1 <last>`, with output processing off.
fn counter_stream(prefix: &[u8], last: u64) -> Vec<u8> {
    let mut stream = prefix.to_vec();
    for n in 1..=last {
        stream.extend_from_slice(format!("{n}\n").as_bytes());
    }
    stream
}

#[tokio::test]
async fn killing_the_starter_leaves_the_child_running() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("h.sock");
    let args = holder_args(
        &dir,
        &socket,
        &["sh", "-c", "read x; echo got-$x; sleep 60"],
        &[],
    );
    let quoted: Vec<String> = std::iter::once(env!("CARGO_BIN_EXE_foundry-holder").to_string())
        .chain(args)
        .map(|arg| format!("'{arg}'"))
        .collect();
    // The marker is made once the holder's starter has exited 0, which it does only after it
    // heard the holder was ready. A holder whose starter is killed before that gives up.
    let heard = dir.path().join("heard");
    let mut starter = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "{} && touch '{}' && sleep 60",
            quoted.join(" "),
            heard.display()
        ))
        .process_group(0)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + WAIT;
    while !heard.exists() {
        assert!(
            Instant::now() < deadline,
            "the starter never heard the holder was ready"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let holder = Holder { dir, socket };
    // The starter is back to its own sleep: kill its whole process group.
    let group = nix::unistd::Pid::from_raw(i32::try_from(starter.id()).unwrap());
    nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGKILL).unwrap();
    starter.wait().unwrap();

    assert_eq!(holder.status().await, ChildStatus::Running);
    let mut client = holder.attach().await;
    client
        .send(Request::Input(Bytes::from_static(b"alive\r")))
        .await;
    client.read_until(b"got-alive").await;
}

#[tokio::test]
async fn after_the_child_exits_status_gives_the_code_and_attach_the_final_screen() {
    let holder = Holder::start(&["sh", "-c", "echo final-screen; exit 7"], &[]);
    assert_eq!(holder.wait_for_exit(WAIT).await, 7);
    let mut client = holder.attach().await;
    assert!(
        contains(&client.snapshot, b"final-screen"),
        "{:?}",
        client.snapshot
    );
    assert_eq!(client.read_until_exit().await, 7);
}

#[tokio::test]
async fn a_child_killed_by_a_signal_exits_128_plus_the_signal() {
    let holder = Holder::start(&["sh", "-c", "kill -9 $$"], &[]);
    assert_eq!(holder.wait_for_exit(WAIT).await, 137);
}

#[tokio::test]
async fn the_exit_is_reported_after_all_the_output_before_it() {
    for _ in 0..5 {
        let holder = Holder::start(
            &[
                "sh",
                "-c",
                "stty -echo; printf R; read x; head -c 60000 /dev/zero | tr '\\0' a; exit 3",
            ],
            &[],
        );
        let mut client = holder.attach().await;
        client.read_until(b"R").await;
        client.send(Request::Input(Bytes::from_static(b"\n"))).await;
        assert_eq!(client.read_until_exit().await, 3);
        let after = &client.seen[position(&client.seen, b"R") + 1..];
        assert_eq!(after.len(), 60_000);
        assert!(after.iter().all(|&b| b == b'a'));
    }
}

#[tokio::test]
async fn the_env_file_is_deleted_once_read_and_reaches_the_child() {
    let holder = Holder::start(
        &["sh", "-c", "echo \"foo=$FOUNDRY_TEST_FOO\"; sleep 60"],
        &[("FOUNDRY_TEST_FOO", "bar baz")],
    );
    assert!(!holder.env_file().exists());
    let mut client = holder.attach().await;
    client.read_until(b"foo=bar baz").await;
}

#[test]
fn an_env_file_that_does_not_parse_is_deleted_and_never_echoed() {
    for (bad, secret) in [
        (r#""sk-secret-value""#, "sk-secret-value"),
        (r#"{"PIN": 482913}"#, "482913"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("h.sock");
        let command = holder_command(&dir, &socket, &["sh", "-c", "sleep 60"], &[]);
        let env_file = dir.path().join("env.json");
        std::fs::write(&env_file, bad).unwrap();
        let output = run_starter(command);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success());
        assert!(stderr.contains("env file"), "{stderr}");
        assert!(!stderr.contains(secret), "{stderr}");
        assert!(!env_file.exists());
        assert!(!socket.exists());
    }
}

#[test]
fn arguments_that_do_not_parse_still_delete_the_env_file() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("h.sock");
    // No command after `--`.
    let output = run_starter(holder_command(&dir, &socket, &[], &[("SECRET", "x")]));
    assert!(!output.status.success());
    assert!(!dir.path().join("env.json").exists());

    // The `--env-file=<path>` form, with an unknown flag so the arguments do not parse.
    let env_file = dir.path().join("env.json");
    pty::write_env_file(&env_file, &[]).unwrap();
    let output = run_starter({
        let mut command = Command::new(env!("CARGO_BIN_EXE_foundry-holder"));
        command
            .arg(format!("--env-file={}", env_file.display()))
            .arg("--no-such-flag");
        command
    });
    assert!(!output.status.success());
    assert!(!env_file.exists());
}

#[test]
fn an_env_file_named_only_in_the_childs_command_is_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let other = dir.path().join("child-file");
    std::fs::write(&other, "keep").unwrap();
    let output = run_starter({
        let mut command = Command::new(env!("CARGO_BIN_EXE_foundry-holder"));
        command
            .args(["--socket", "/tmp/unused", "--"])
            .arg("--env-file")
            .arg(&other);
        command
    });
    assert!(!output.status.success());
    assert!(other.exists());
}

#[test]
fn a_missing_working_directory_is_named_as_such() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("h.sock");
    let mut args = holder_args(&dir, &socket, &["sh", "-c", "sleep 60"], &[]);
    let cwd = args.iter().position(|a| a == "--cwd").unwrap() + 1;
    args[cwd] = "/no/such/dir".to_string();
    let mut command = Command::new(env!("CARGO_BIN_EXE_foundry-holder"));
    command.args(args);
    let output = run_starter(command);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("cwd /no/such/dir"), "{stderr}");
    assert!(!socket.exists());
    assert!(!lock_file(&socket).exists());
}

#[tokio::test]
async fn the_socket_is_closed_to_other_users_whatever_the_umask() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("h.sock");
    let args = holder_args(&dir, &socket, &["sh", "-c", "sleep 60"], &[]);
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg("umask 000; exec \"$0\" \"$@\"")
        .arg(env!("CARGO_BIN_EXE_foundry-holder"))
        .args(args);
    let output = run_starter(command);
    assert!(output.status.success());
    let holder = Holder { dir, socket };
    let mode = std::fs::metadata(&holder.socket)
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600, "socket mode {mode:o}");
}

#[tokio::test]
async fn resize_sets_the_terminal_size() {
    let holder = Holder::start(&["sh", "-c", "read x; stty size"], &[]);
    let mut client = holder.attach().await;
    client
        .send(Request::Resize {
            cols: 101,
            rows: 33,
        })
        .await;
    client.send(Request::Input(Bytes::from_static(b"\r"))).await;
    client.read_until(b"33 101").await;
}

#[tokio::test]
async fn close_ends_the_child_and_removes_the_socket() {
    let holder = Holder::start(&["sh", "-c", "echo pid-$$; exec sleep 60"], &[]);
    let mut client = holder.attach().await;
    client.read_until(b"\n").await;
    let text = String::from_utf8_lossy(&client.seen).to_string();
    let pid: i32 = text
        .split("pid-")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("no pid in {text:?}"));

    holder.send(Request::Close).await;
    let deadline = Instant::now() + WAIT;
    while holder.socket.exists()
        || nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
    {
        assert!(Instant::now() < deadline, "the terminal did not close");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_running_holder_holds_its_lock_file_until_it_closes() {
    let holder = Holder::start(&["sh", "-c", "sleep 60"], &[]);
    let lock = lock_file(&holder.socket);
    let mode = std::fs::metadata(&lock).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600, "lock file mode {mode:o}");
    assert_eq!(try_lock(&lock).err(), Some(Errno::EWOULDBLOCK));

    holder.send(Request::Close).await;
    let deadline = Instant::now() + WAIT;
    while holder.socket.exists() || lock.exists() {
        assert!(Instant::now() < deadline, "the lock file was not removed");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[test]
fn a_holder_whose_lock_is_held_fails_to_start_and_names_the_lock_file() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("h.sock");
    let lock = lock_file(&socket);
    std::fs::write(&lock, b"").unwrap();
    // Shared, so only a request for the exclusive lock fails on it.
    let _held = try_lock(&lock).unwrap();
    let output = run_starter(holder_command(
        &dir,
        &socket,
        &["sh", "-c", "sleep 60"],
        &[],
    ));
    // Closes a holder that started after all.
    let holder = Holder { dir, socket };
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains(&lock.display().to_string()), "{stderr}");
    assert!(!holder.socket.exists());
    assert!(
        lock.exists(),
        "the lock file of the holder that has it is left alone"
    );
}

/// The lock goes with the holder however it dies, and the child, which outlives it here, never
/// has it.
#[tokio::test]
async fn a_killed_holder_lets_go_of_its_lock_even_while_its_child_runs() {
    let holder = Holder::start(
        &["sh", "-c", "trap '' HUP; echo pid-$$; exec sleep 60"],
        &[],
    );
    let mut client = holder.attach().await;
    client.read_until(b"\n").await;
    let text = String::from_utf8_lossy(&client.seen).to_string();
    let child = text
        .split("pid-")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|n| n.parse().ok())
        .map(nix::unistd::Pid::from_raw)
        .unwrap_or_else(|| panic!("no pid in {text:?}"));
    let lock = lock_file(&holder.socket);

    let pid = nix::unistd::Pid::from_raw(holder_pid(&holder.socket));
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL).unwrap();
    let deadline = Instant::now() + WAIT;
    while try_lock(&lock).is_err() {
        assert!(Instant::now() < deadline, "the lock outlived the holder");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let child_ran = nix::sys::signal::kill(child, None).is_ok();
    let _ = nix::sys::signal::kill(child, nix::sys::signal::Signal::SIGKILL);
    assert!(
        child_ran,
        "the child was still running when the lock was free"
    );
}

#[tokio::test]
async fn a_command_that_cannot_start_fails_the_starter() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("h.sock");
    let output = run_starter(holder_command(&dir, &socket, &["/no/such/program"], &[]));
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("/no/such/program"));
    assert!(!socket.exists());
    assert!(!lock_file(&socket).exists());
}

#[tokio::test]
async fn a_request_in_another_version_is_refused_with_a_reason() {
    let holder = Holder::start(&["sh", "-c", "sleep 60"], &[]);
    let mut stream = holder.connect().await;
    let mut frame = Request::Status.encode();
    frame[4] = protocol::VERSION + 1;
    tokio::io::AsyncWriteExt::write_all(&mut stream, &frame)
        .await
        .unwrap();
    match next(&mut stream).await {
        Reply::Refused(reason) => assert!(
            reason.contains(&format!("version {}", protocol::VERSION + 1)),
            "{reason}"
        ),
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(read_or_end(&mut stream).await.is_none());
    assert_eq!(holder.status().await, ChildStatus::Running);
}

#[tokio::test]
async fn input_over_the_pending_limit_is_refused_not_dropped() {
    let holder = Holder::start(&["sh", "-c", "sleep 60"], &[]);
    let mut client = holder.attach().await;
    let paste = Bytes::from(vec![b'a'; protocol::MAX_PENDING_INPUT + 1]);
    client.send(Request::Input(paste)).await;
    match next(&mut client.stream).await {
        Reply::Refused(reason) => assert!(reason.contains("input"), "{reason}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(read_or_end(&mut client.stream).await.is_none());
    assert_eq!(holder.status().await, ChildStatus::Running);
}

#[tokio::test]
async fn input_is_refused_once_what_the_child_has_not_read_passes_the_limit() {
    // The child never reads, so input piles up in the holder past what the terminal takes. Raw
    // mode, because a canonical terminal discards input past its line limit instead.
    let holder = Holder::start(&["sh", "-c", "stty raw -echo; printf R; sleep 60"], &[]);
    holder.attach().await.read_until(b"R").await;
    let mut stream = holder.connect().await;
    let three_mb = Bytes::from(vec![b'a'; 3 * 1024 * 1024]);
    protocol::write_request(&mut stream, &Request::Input(three_mb.clone()))
        .await
        .unwrap();
    protocol::write_request(&mut stream, &Request::Input(three_mb))
        .await
        .unwrap();
    match next(&mut stream).await {
        Reply::Refused(reason) => assert!(reason.contains("input"), "{reason}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(read_or_end(&mut stream).await.is_none());
}

#[tokio::test]
async fn nothing_a_client_sends_after_its_refusal_reaches_the_child() {
    let holder = Holder::start(&["sh", "-c", "stty raw -echo; printf R; cat"], &[]);
    let mut observer = holder.attach().await;
    observer.read_until(b"R").await;

    let mut sender = holder.connect().await;
    let paste = Bytes::from(vec![b'a'; protocol::MAX_PENDING_INPUT + 1]);
    protocol::write_request(&mut sender, &Request::Input(paste))
        .await
        .unwrap();
    let _ =
        protocol::write_request(&mut sender, &Request::Input(Bytes::from_static(b"MARK"))).await;
    match next(&mut sender).await {
        Reply::Refused(_) => {}
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(read_or_end(&mut sender).await.is_none());

    holder
        .send(Request::Input(Bytes::from_static(b"END")))
        .await;
    observer.read_until(b"END").await;
    assert!(
        !contains(&observer.seen, b"MARK"),
        "input after a refusal reached the child"
    );
}

/// Waits until the child's 20 MB flood is all in the holder, so the scrollback is full (a snapshot
/// far more than the socket buffers hold) and no more output will follow it.
async fn wait_for_quiet_flood(holder: &Holder) {
    let deadline = Instant::now() + WAIT;
    loop {
        let mut stream = holder.connect().await;
        protocol::write_request(&mut stream, &Request::Attach)
            .await
            .unwrap();
        // Attached mid-flood, a client can fall too far behind and be cut off before its snapshot
        // arrives. Then it tries again.
        if let Some(Reply::Snapshot { start, bytes }) = read_or_end(&mut stream).await
            && start + bytes.len() as u64 == 20_000_000
        {
            assert_eq!(bytes.len(), MAX_BYTES);
            return;
        }
        assert!(Instant::now() < deadline, "the flood never finished");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn a_client_that_hangs_up_still_gets_what_was_queued_for_it() {
    let holder = Holder::start(
        &[
            "sh",
            "-c",
            "head -c 20000000 /dev/zero | tr '\\0' a; sleep 60",
        ],
        &[],
    );
    wait_for_quiet_flood(&holder).await;

    let mut stream = holder.connect().await;
    protocol::write_request(&mut stream, &Request::Attach)
        .await
        .unwrap();
    tokio::io::AsyncWriteExt::shutdown(&mut stream)
        .await
        .unwrap();
    match next(&mut stream).await {
        Reply::Snapshot { bytes, .. } => assert_eq!(bytes.len(), MAX_BYTES),
        other => panic!("expected the snapshot, got {other:?}"),
    }
    assert_eq!(read_or_end(&mut stream).await, None);
}

#[tokio::test]
async fn a_client_that_hangs_up_without_reading_is_let_go() {
    let holder = Holder::start(
        &[
            "sh",
            "-c",
            "head -c 20000000 /dev/zero | tr '\\0' a; sleep 60",
        ],
        &[],
    );
    wait_for_quiet_flood(&holder).await;

    let mut stream = holder.connect().await;
    protocol::write_request(&mut stream, &Request::Attach)
        .await
        .unwrap();
    tokio::io::AsyncWriteExt::shutdown(&mut stream)
        .await
        .unwrap();
    tokio::time::sleep(protocol::CLOSE_GRACE + Duration::from_secs(1)).await;
    // The holder gave up on the snapshot it could not deliver, so the frame ends early.
    assert_eq!(read_or_end(&mut stream).await, None);
    assert_eq!(holder.status().await, ChildStatus::Running);
}

#[tokio::test]
async fn clients_that_stop_reading_are_cut_off_and_let_go() {
    let holder = Holder::start(
        &[
            "sh",
            "-c",
            "stty -echo; printf R; read x; head -c 100000000 /dev/zero | tr '\\0' a",
        ],
        &[],
    );
    let mut driver = holder.attach().await;
    driver.read_until(b"R").await;
    drop(driver);
    let mut stalled = Vec::new();
    for _ in 0..4 {
        stalled.push(holder.attach().await);
    }
    let rss = PeakRss::watch(holder_pid(&holder.socket));
    holder.send(Request::Input(Bytes::from_static(b"\n"))).await;
    assert_eq!(holder.wait_for_exit(Duration::from_secs(180)).await, 0);
    let peak_mb = rss.stop();
    assert!(
        peak_mb < 50,
        "four stalled clients took the holder to {peak_mb} MB"
    );

    // Each was disconnected both ways, so what it sends now fails rather than vanishing.
    for client in &mut stalled {
        assert!(
            writes_fail(&mut client.stream).await,
            "a cut-off client can still write"
        );
    }
}

/// The pid of the detached holder serving `socket`, found by its command line.
fn holder_pid(socket: &Path) -> i32 {
    let out = Command::new("ps")
        .args(["-axo", "pid=,command="])
        .output()
        .unwrap();
    let socket = socket.to_str().unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find(|line| line.contains("foundry-holder") && line.contains(socket))
        .and_then(|line| line.split_whitespace().next())
        .and_then(|pid| pid.parse().ok())
        .expect("the holder is not running")
}

/// Samples a process's resident memory until stopped.
struct PeakRss {
    pid: i32,
    peak_kb: Arc<AtomicU64>,
    done: Arc<AtomicBool>,
    sampler: std::thread::JoinHandle<()>,
}

impl PeakRss {
    fn watch(pid: i32) -> Self {
        let peak_kb = Arc::new(AtomicU64::new(0));
        let done = Arc::new(AtomicBool::new(false));
        let sampler = {
            let (peak_kb, done) = (peak_kb.clone(), done.clone());
            std::thread::spawn(move || {
                while !done.load(Ordering::Relaxed) {
                    if let Some(kb) = rss_kb(pid) {
                        peak_kb.fetch_max(kb, Ordering::Relaxed);
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
            })
        };
        PeakRss {
            pid,
            peak_kb,
            done,
            sampler,
        }
    }

    /// The peak in MB. Fails the test when the memory was never read.
    fn stop(self) -> u64 {
        if let Some(kb) = rss_kb(self.pid) {
            self.peak_kb.fetch_max(kb, Ordering::Relaxed);
        }
        self.done.store(true, Ordering::Relaxed);
        self.sampler.join().unwrap();
        let kb = self.peak_kb.load(Ordering::Relaxed);
        assert!(kb > 0, "the holder's memory was never read");
        kb / 1024
    }
}

fn rss_kb(pid: i32) -> Option<u64> {
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}
