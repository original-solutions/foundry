//! The `foundry-holder` binary, driven over its socket the way `foundryd` drives it.

use std::io::Write as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use foundry_holder::protocol::{self, ChildStatus, Reply, Request};
use foundry_holder::pty;
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
        let status = holder_command(&dir, &socket, argv, env).status().unwrap();
        assert!(status.success(), "foundry-holder exited {status}");
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
        let snapshot = match next(&mut stream).await {
            Reply::Snapshot(bytes) => bytes.to_vec(),
            other => panic!("expected a snapshot first, got {other:?}"),
        };
        Attached {
            stream,
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
        match next(&mut self.stream).await {
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
    let script = format!("stty raw -echo; read go; cat '{}'", file.display());
    let holder = Holder::start(&["sh", "-c", &script], &[]);

    let mut client = holder.attach().await;
    // The terminal is raw once the child has set it, so the go line is not echoed or translated.
    tokio::time::sleep(Duration::from_millis(300)).await;
    client
        .send(Request::Input(Bytes::from_static(b"go\n")))
        .await;
    assert_eq!(client.read_until_exit().await, 0);
    assert_eq!(client.seen, payload);
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
    let holder = Holder::start(&["sh", "-c", "seq 1 10050"], &[]);
    assert_eq!(holder.wait_for_exit(WAIT).await, 0);
    let client = holder.attach().await;
    let expected: Vec<u8> = (51..=10_050)
        .flat_map(|n| format!("{n}\r\n").into_bytes())
        .collect();
    assert_eq!(client.snapshot, expected);
}

#[tokio::test]
async fn a_flood_with_no_client_attached_finishes_in_bounded_memory() {
    let holder = Holder::start(
        &[
            "sh",
            "-c",
            "yes 0123456789abcdefghijklmnopqrstuvwxyz | head -c 200000000",
        ],
        &[],
    );
    let pid = holder_pid(&holder.socket);
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
    let code = holder.wait_for_exit(Duration::from_secs(180)).await;
    if let Some(kb) = rss_kb(pid) {
        peak_kb.fetch_max(kb, Ordering::Relaxed);
    }
    done.store(true, Ordering::Relaxed);
    sampler.join().unwrap();

    assert_eq!(code, 0);
    let peak_mb = peak_kb.load(Ordering::Relaxed) / 1024;
    assert!(peak_mb > 0, "the holder's memory was never read");
    assert!(peak_mb < 50, "the holder peaked at {peak_mb} MB");
    let client = holder.attach().await;
    assert!(client.snapshot.len() <= 16 * 1024 * 1024);
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
    let mut starter = Command::new("sh")
        .arg("-c")
        .arg(format!("{} && sleep 60", quoted.join(" ")))
        .process_group(0)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + WAIT;
    while !socket.exists() {
        assert!(
            Instant::now() < deadline,
            "the holder never made its socket"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let holder = Holder { dir, socket };
    // Wait until the starter is back to its own sleep, then kill its whole process group.
    tokio::time::sleep(Duration::from_millis(200)).await;
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
async fn the_env_file_is_deleted_once_read_and_reaches_the_child() {
    let holder = Holder::start(
        &["sh", "-c", "echo \"foo=$FOUNDRY_TEST_FOO\"; sleep 60"],
        &[("FOUNDRY_TEST_FOO", "bar baz")],
    );
    assert!(!holder.env_file().exists());
    let mut client = holder.attach().await;
    client.read_until(b"foo=bar baz").await;
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
async fn a_command_that_cannot_start_fails_the_starter() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("h.sock");
    let output = holder_command(&dir, &socket, &["/no/such/program"], &[])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("/no/such/program"));
    assert!(!socket.exists());
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

fn rss_kb(pid: i32) -> Option<u64> {
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}
