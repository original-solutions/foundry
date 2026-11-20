//! The `foundryd` binary run as a daemon: the data-directory lock, a clean stop that leaves
//! terminals running, `status`, the peer route's key proof and the gateway settings. Each
//! test uses its own data directory, a free port and the file secret store, never the
//! defaults or the login keychain.

use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use foundry_proto::ids::{MachineId, ProjectId, TerminalId};
use foundry_proto::messages::{PROTOCOL, PeerMessage, UiMessage};
use foundry_proto::record::{AnyRecord, MachineSample, Record};
use foundry_proto::settings::{Machine, Project, SharedSettings};
use foundry_proto::state::TerminalState;
use foundryd::join::{KeyedDialer, Keys};
use foundryd::secrets::{FileSecrets, Secrets};
use foundryd::store::Store;
use foundryd::sync::Dialer as _;
use foundryd::sync::ws::WsDialer;
use futures::{SinkExt as _, StreamExt as _};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use tempfile::TempDir;
use tokio_tungstenite::tungstenite::Message;

const WAIT: Duration = Duration::from_secs(30);

/// A pid file's marker: the shell the daemon starts runs `sleep` for this many seconds, so a
/// stray one is found by it.
const SLEEP: &str = "74331";

/// The same, for the fake `claude` a session runs.
const SLEEP_CLAUDE: &str = "74332";

/// Polls `check` until it gives a value, or fails the test once [`WAIT`] passes.
fn wait_for<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    let end = Instant::now() + WAIT;
    loop {
        if let Some(found) = check() {
            return found;
        }
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// How `child` exited, or `None` once `wait` passes, when it is killed and reaped.
fn exit_within(child: &mut Child, wait: Duration) -> Option<ExitStatus> {
    let end = Instant::now() + wait;
    loop {
        if let Some(exit) = child.try_wait().unwrap() {
            return Some(exit);
        }
        if Instant::now() > end {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A port nothing listens on now.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Runs git in `dir` with an identity of its own.
fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=Foundry Test",
            "-c",
            "user.email=test@foundry.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// One data directory and the daemons run on it, one at a time. Dropping it stops the daemon
/// and closes every holder left under the data directory.
struct Fixture {
    /// The data directory. A Unix socket path under it has to stay short.
    home: TempDir,
    /// The `HOME` the daemon sees, so nothing touches the real one.
    user: TempDir,
    port: u16,
    shell: PathBuf,
    /// `FOUNDRY_GATEWAY_URL` for the daemon, when the test sets one.
    gateway_url: Option<String>,
    /// A directory put first on the daemon's `PATH`.
    path_first: Option<PathBuf>,
    daemon: Option<Child>,
}

impl Fixture {
    fn new() -> Self {
        // `run` refuses a data directory other users can reach, and the temporary one is
        // made with the default mode.
        let home = tempfile::Builder::new()
            .prefix("fd")
            .tempdir_in("/tmp")
            .unwrap();
        fs::set_permissions(home.path(), fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            home,
            user: tempfile::tempdir().unwrap(),
            port: free_port(),
            shell: PathBuf::from("/bin/sh"),
            gateway_url: None,
            path_first: None,
            daemon: None,
        }
    }

    /// `foundryd args` on this data directory and `port`, with the gateway URL the test set,
    /// else none.
    fn command(&self, port: u16, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_foundryd"));
        command
            .args(args)
            .env("FOUNDRY_HOME", self.home.path())
            .env("FOUNDRY_PORT", port.to_string())
            .env("FOUNDRY_SECRETS", "file")
            .env("HOME", self.user.path())
            .env("SHELL", &self.shell)
            .env_remove("CLAUDE_CONFIG_DIR");
        match &self.gateway_url {
            Some(url) => command.env("FOUNDRY_GATEWAY_URL", url),
            None => command.env_remove("FOUNDRY_GATEWAY_URL"),
        };
        if let Some(first) = &self.path_first {
            let mut paths = vec![first.clone()];
            paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
            command.env("PATH", std::env::join_paths(paths).unwrap());
        }
        command
    }

    /// Starts the daemon and waits until it answers.
    fn start(&mut self) {
        assert!(self.daemon.is_none(), "a daemon is already running");
        let log = fs::File::create(self.user.path().join("daemon.log")).unwrap();
        let child = self
            .command(self.port, &["run"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .unwrap();
        self.daemon = Some(child);
        wait_for("the daemon to answer", || {
            if let Some(exit) = self.daemon.as_mut().unwrap().try_wait().unwrap() {
                panic!("the daemon exited at start: {exit}; {}", self.log());
            }
            self.state()
        });
    }

    fn log(&self) -> String {
        fs::read_to_string(self.user.path().join("daemon.log")).unwrap_or_default()
    }

    /// Sends SIGTERM to the daemon, waits for it to exit and returns its exit code.
    fn stop(&mut self) -> Option<i32> {
        self.stop_with(Signal::SIGTERM)
    }

    /// Sends `signal` to the daemon, waits for it to exit and returns its exit code.
    fn stop_with(&mut self, signal: Signal) -> Option<i32> {
        let mut child = self.daemon.take().expect("a daemon is running");
        kill(Pid::from_raw(i32::try_from(child.id()).unwrap()), signal).unwrap();
        wait_for("the daemon to exit", || child.try_wait().unwrap()).code()
    }

    fn agent() -> ureq::Agent {
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(20)))
            .build()
            .into()
    }

    /// `(status, body)` of `method path` with `body` as JSON, or `None` while the daemon does
    /// not answer.
    fn try_call(
        &self,
        method: &str,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Option<(u16, String)> {
        let url = format!("http://127.0.0.1:{}{path}", self.port);
        let agent = Self::agent();
        let sent = match (method, body) {
            ("POST", Some(body)) => agent.post(&url).send_json(body),
            ("POST", None) => agent.post(&url).send_empty(),
            _ => agent.get(&url).call(),
        };
        let mut response = sent.ok()?;
        let status = response.status().as_u16();
        Some((status, response.body_mut().read_to_string().ok()?))
    }

    fn call(&self, method: &str, path: &str, body: Option<serde_json::Value>) -> (u16, String) {
        self.try_call(method, path, body)
            .unwrap_or_else(|| panic!("{method} {path} got no answer; {}", self.log()))
    }

    /// The records `GET /api/state` returns, once it answers 200.
    fn state(&self) -> Option<Vec<AnyRecord>> {
        match self.try_call("GET", "/api/state", None)? {
            (200, body) => match serde_json::from_str(&body).ok()? {
                UiMessage::State { records, .. } => Some(records),
                _ => None,
            },
            _ => None,
        }
    }

    /// Puts the machine list and projects `edit` makes in the store, which no daemon has open.
    fn seed(&self, edit: impl FnOnce(&mut SharedSettings)) {
        assert!(self.daemon.is_none(), "seed before the daemon starts");
        let mut store = Store::open(self.home.path().join("store.sqlite")).unwrap();
        let mut settings = store.settings().unwrap();
        settings.seq += 1;
        edit(&mut settings);
        assert!(store.apply_settings(settings).unwrap());
    }

    /// A project whose remote, a local bare repo, has one commit on `main`.
    fn project(&self) -> Project {
        let root = self.user.path().join("remote");
        fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q", "--bare", "remote.git"]);
        git(&root, &["clone", "-q", "remote.git", "src"]);
        let src = root.join("src");
        fs::write(src.join("README"), "demo\n").unwrap();
        git(&src, &["add", "."]);
        git(&src, &["commit", "-q", "-m", "first"]);
        git(&src, &["push", "-q", "origin", "main"]);
        Project {
            id: ProjectId::random(),
            name: "demo".into(),
            repo: root.join("remote.git").to_string_lossy().into_owned(),
        }
    }

    /// A shell for the daemon's terminals: it writes its pid to `shell.pid` and becomes a
    /// long `sleep`.
    fn sleeping_shell(&mut self) -> PathBuf {
        let script = self.user.path().join("shell.sh");
        let pid = self.user.path().join("shell.pid");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\necho $$ > {}\nexec sleep {SLEEP}\n",
                pid.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        self.shell = script.clone();
        pid
    }

    /// A workspace of `project`, made over the API.
    fn workspace(&self, project: &Project) -> String {
        let (status, body) = self.call(
            "POST",
            "/api/workspaces",
            Some(serde_json::json!({"project": project.id, "base": "main", "name": "w"})),
        );
        assert_eq!(status, 201, "{body}");
        id_of(&body)
    }
}

fn id_of(body: &str) -> String {
    let json: serde_json::Value = serde_json::from_str(body).unwrap();
    json["id"].as_str().unwrap().to_owned()
}

impl Drop for Fixture {
    fn drop(&mut self) {
        use std::io::Write as _;
        if let Some(mut child) = self.daemon.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let Ok(entries) = fs::read_dir(self.home.path().join("terminals")) else {
            return;
        };
        for entry in entries.flatten() {
            if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(entry.path()) {
                let _ = stream.write_all(&foundry_holder::protocol::Request::Close.encode());
            }
        }
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn run_to_end(mut command: Command) -> Output {
    command.output().expect("foundryd runs")
}

#[test]
fn run_twice_on_one_data_directory_exits_1_and_names_the_lock() {
    let mut first = Fixture::new();
    first.start();

    let mut second = first
        .command(free_port(), &["run"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let exit = exit_within(&mut second, WAIT);
    let exit = exit.expect("the second daemon kept running instead of refusing the lock");
    let mut stderr = String::new();
    std::io::Read::read_to_string(&mut second.stderr.take().unwrap(), &mut stderr).unwrap();

    assert_eq!(exit.code(), Some(1), "{stderr}");
    assert!(stderr.contains("daemon.lock"), "stderr: {stderr}");
}

#[test]
fn after_sigint_the_daemon_exits_0() {
    let mut fixture = Fixture::new();
    fixture.start();

    assert_eq!(
        fixture.stop_with(Signal::SIGINT),
        Some(0),
        "{}",
        fixture.log()
    );
}

#[test]
fn after_sigterm_the_daemon_exits_0_and_a_terminal_child_still_runs() {
    let mut fixture = Fixture::new();
    let pid_file = fixture.sleeping_shell();
    let project = fixture.project();
    fixture.seed(|settings| settings.projects.push(project.clone()));
    fixture.start();
    let workspace = fixture.workspace(&project);
    let (status, body) = fixture.call("POST", &format!("/api/workspaces/{workspace}/shells"), None);
    assert_eq!(status, 201, "{body}");
    let terminal = id_of(&body);
    let pid = wait_for("the shell to write its pid", || {
        fs::read_to_string(&pid_file)
            .ok()
            .and_then(|text| text.trim().parse::<i32>().ok())
    });

    let code = fixture.stop();

    assert_eq!(code, Some(0), "{}", fixture.log());
    assert!(
        kill(Pid::from_raw(pid), None).is_ok(),
        "the terminal's child {pid} is gone"
    );

    // A daemon started again on the same data directory shows the terminal as running.
    fixture.start();
    let id: TerminalId = serde_json::from_value(serde_json::json!(terminal)).unwrap();
    let running = |fixture: &Fixture| {
        fixture
            .state()?
            .into_iter()
            .find_map(|record| match record {
                AnyRecord::Terminal(record) if record.body.id == id => Some(record.body.state),
                _ => None,
            })
    };
    let state = wait_for("the terminal to show", || running(&fixture));
    assert_eq!(state, TerminalState::Running);
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(running(&fixture), Some(TerminalState::Running));

    let (status, body) = fixture.call("POST", &format!("/api/terminals/{terminal}/close"), None);
    assert_eq!(status, 204, "{body}");
    assert_eq!(fixture.stop(), Some(0));
}

#[test]
fn status_against_a_running_daemon_prints_this_machine_and_exits_0() {
    let mut fixture = Fixture::new();
    fixture.start();
    let owner = wait_for("a machine sample", || {
        fixture
            .state()?
            .into_iter()
            .find_map(|record| match record {
                AnyRecord::MachineSample(sample) => Some(sample.owner),
                _ => None,
            })
    });

    let out = run_to_end(fixture.command(fixture.port, &["status"]));

    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(
        text(&out.stdout).contains(&owner.to_string()),
        "stdout: {}",
        text(&out.stdout)
    );
}

#[test]
fn status_with_no_daemon_exits_1_and_says_none_is_running() {
    let fixture = Fixture::new();

    let out = run_to_end(fixture.command(free_port(), &["status"]));

    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("no daemon is running"),
        "stderr: {}",
        text(&out.stderr)
    );
}

#[tokio::test]
async fn the_daemons_peer_route_refuses_a_link_with_no_key_proof() {
    let mut fixture = Fixture::new();
    let peer = MachineId::random();
    fixture.seed(|settings| {
        settings.machines.push(Machine {
            id: peer,
            name: "peer".into(),
            address: "127.0.0.2".into(),
        });
    });
    fixture.start();

    let url = format!("ws://127.0.0.1:{}/api/peer", fixture.port);
    let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    let hello = PeerMessage::Hello {
        protocol: PROTOCOL,
        machine: peer,
        versions: Default::default(),
    };
    socket
        .send(Message::text(serde_json::to_string(&hello).unwrap()))
        .await
        .unwrap();

    let end = tokio::time::timeout(Duration::from_secs(20), async {
        while let Some(Ok(message)) = socket.next().await {
            if let Message::Text(text) = message {
                let said: Result<PeerMessage, _> = serde_json::from_str(text.as_str());
                assert!(
                    !matches!(
                        said,
                        Ok(PeerMessage::Hello { .. } | PeerMessage::Records { .. })
                    ),
                    "the daemon took the link: {text}"
                );
            }
        }
    })
    .await;
    assert!(end.is_ok(), "the link was left open");
}

#[test]
fn a_session_start_without_a_gateway_is_refused_naming_what_is_missing() {
    let mut fixture = Fixture::new();
    let key = "sk-test-distinct-0123456789";
    FileSecrets::new(fixture.home.path().join("secrets.json"))
        .set("gateway", key)
        .unwrap();
    let project = fixture.project();
    fixture.seed(|settings| settings.projects.push(project.clone()));
    fixture.start();
    let workspace = fixture.workspace(&project);

    let (status, body) = fixture.call(
        "POST",
        &format!("/api/workspaces/{workspace}/sessions"),
        Some(serde_json::json!({"model": "opus"})),
    );
    assert!(!(200..300).contains(&status), "{status}: {body}");
    assert!(body.contains("FOUNDRY_GATEWAY_URL"), "{body}");
    assert!(!body.contains(key), "{body}");
    assert!(!fixture.log().contains(key), "the key reached the log");

    let (status, body) = fixture.call("POST", &format!("/api/workspaces/{workspace}/shells"), None);
    assert_eq!(status, 201, "{body}");
}

#[test]
fn a_session_start_with_a_gateway_gives_the_child_its_url_and_key() {
    let mut fixture = Fixture::new();
    let key = "sk-test-happy-0123456789";
    FileSecrets::new(fixture.home.path().join("secrets.json"))
        .set("gateway", key)
        .unwrap();
    fixture.gateway_url = Some("http://gw.invalid:8317".into());
    let bin = fixture.user.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let seen = fixture.user.path().join("claude.env");
    let script = bin.join("claude");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s\\n%s\\n' \"$ANTHROPIC_BASE_URL\" \"$ANTHROPIC_AUTH_TOKEN\" > {}\nexec sleep {SLEEP_CLAUDE}\n",
            seen.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    fixture.path_first = Some(bin);
    let project = fixture.project();
    fixture.seed(|settings| settings.projects.push(project.clone()));
    fixture.start();
    let workspace = fixture.workspace(&project);

    let (status, body) = fixture.call(
        "POST",
        &format!("/api/workspaces/{workspace}/sessions"),
        Some(serde_json::json!({"model": "opus"})),
    );
    assert!(
        (200..300).contains(&status),
        "{status}: {body}; {}",
        fixture.log()
    );

    let lines = wait_for("the fake claude to write what it saw", || {
        let text = fs::read_to_string(&seen).ok()?;
        let lines: Vec<String> = text.lines().map(str::to_owned).collect();
        (lines.len() == 2).then_some(lines)
    });
    assert_eq!(lines, ["http://gw.invalid:8317", key]);
    assert!(!fixture.log().contains(key), "the key reached the log");
}

#[tokio::test]
async fn the_daemon_dials_a_peer_through_the_key_handshake() {
    let mut fixture = Fixture::new();
    let other = tempfile::tempdir().unwrap();
    let peer = Keys::open(Arc::new(FileSecrets::new(
        other.path().join("secrets.json"),
    )))
    .unwrap();
    let peer_id = peer.me();
    {
        let mine = Keys::open(Arc::new(FileSecrets::new(
            fixture.home.path().join("secrets.json"),
        )))
        .unwrap();
        let code = mine.new_code().unwrap();
        mine.redeem(&code.to_string(), peer_id, &peer.public_key())
            .unwrap();
    }
    fixture.seed(|settings| {
        settings.machines.push(Machine {
            id: peer_id,
            name: "peer".into(),
            address: "::1".into(),
        });
    });
    // The daemon listens on 127.0.0.1 and Tailscale addresses only, so this port is free here.
    let listener = tokio::net::TcpListener::bind(("::1", fixture.port))
        .await
        .expect("::1 must be bindable for this test: it plays the peer at address ::1");
    fixture.start();

    let first = tokio::time::timeout(Duration::from_secs(20), async {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        loop {
            match socket.next().await {
                Some(Ok(Message::Text(text))) => return text.as_str().to_owned(),
                Some(Ok(_)) => {}
                other => panic!("the link ended before a first frame: {other:?}"),
            }
        }
    })
    .await
    .expect("the daemon did not dial the peer");

    let said: serde_json::Value = serde_json::from_str(&first).unwrap();
    assert_eq!(said["type"], "challenge", "first frame: {first}");
}

/// A secret read that never returns, as a keychain prompt nobody answers does: the secret
/// store's file is a FIFO the test holds open and never writes to.
#[test]
fn sigterm_stops_a_daemon_stuck_reading_its_secrets_at_start() {
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut fixture = Fixture::new();
    let secrets = fixture.home.path().join("secrets.json");
    nix::unistd::mkfifo(&secrets, nix::sys::stat::Mode::from_bits_truncate(0o600)).unwrap();
    let daemon = fixture
        .command(fixture.port, &["run"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    fixture.daemon = Some(daemon);

    // A writer opens only once the daemon has the FIFO open to read, and holding it open
    // keeps that read waiting.
    let _writer = wait_for("the daemon to read its secrets", || {
        fs::OpenOptions::new()
            .write(true)
            .custom_flags(nix::fcntl::OFlag::O_NONBLOCK.bits())
            .open(&secrets)
            .ok()
    });
    std::thread::sleep(Duration::from_millis(200));

    let mut child = fixture.daemon.take().unwrap();
    kill(
        Pid::from_raw(i32::try_from(child.id()).unwrap()),
        Signal::SIGTERM,
    )
    .unwrap();
    assert!(
        exit_within(&mut child, Duration::from_secs(10)).is_some(),
        "SIGTERM did not stop a daemon waiting on its secrets"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_daemons_peer_route_takes_a_keyed_peer_and_syncs_its_records() {
    let mut fixture = Fixture::new();
    let other = tempfile::tempdir().unwrap();
    let peer = Keys::open(Arc::new(FileSecrets::new(
        other.path().join("secrets.json"),
    )))
    .unwrap();
    let peer_id = peer.me();
    let daemon = {
        let mine = Keys::open(Arc::new(FileSecrets::new(
            fixture.home.path().join("secrets.json"),
        )))
        .unwrap();
        let code = mine.new_code().unwrap();
        mine.redeem(&code.to_string(), peer_id, &peer.public_key())
            .unwrap();
        let code = peer.new_code().unwrap();
        peer.redeem(&code.to_string(), mine.me(), &mine.public_key())
            .unwrap();
        Machine {
            id: mine.me(),
            name: "daemon".into(),
            address: "127.0.0.1".into(),
        }
    };
    fixture.seed(|settings| {
        settings.machines.push(Machine {
            id: peer_id,
            name: "peer".into(),
            address: "127.0.0.2".into(),
        });
    });
    fixture.start();

    let dialer = KeyedDialer::new(Arc::new(WsDialer { port: fixture.port }), peer);
    let mut conn = tokio::time::timeout(Duration::from_secs(20), dialer.dial(&daemon))
        .await
        .expect("the handshake did not finish")
        .expect("the daemon refused a peer that proved its key");
    let hello = PeerMessage::Hello {
        protocol: PROTOCOL,
        machine: peer_id,
        versions: Default::default(),
    };
    conn.send
        .send(serde_json::to_string(&hello).unwrap())
        .await
        .unwrap();
    let answered = tokio::time::timeout(Duration::from_secs(20), async {
        while let Some(text) = conn.recv.recv().await {
            if let Ok(PeerMessage::Hello { machine, .. }) = serde_json::from_str(&text) {
                return Some(machine);
            }
        }
        None
    })
    .await
    .expect("the daemon did not answer the hello");
    assert_eq!(answered, Some(daemon.id));

    let sample = AnyRecord::MachineSample(Record {
        owner: peer_id,
        version: 1,
        deleted: false,
        body: MachineSample {
            load_per_core: 0.25,
            free_memory: 1 << 30,
            free_disk: 1 << 35,
            sessions: 3,
        },
    });
    let records = PeerMessage::Records {
        protocol: PROTOCOL,
        records: vec![sample],
    };
    conn.send
        .send(serde_json::to_string(&records).unwrap())
        .await
        .unwrap();

    tokio::task::block_in_place(|| {
        wait_for("the peer's record in /api/state", || {
            fixture
                .state()?
                .into_iter()
                .any(|record| {
                    matches!(&record, AnyRecord::MachineSample(r)
                        if r.owner == peer_id && r.body.sessions == 3)
                })
                .then_some(())
        });
    });
}

#[test]
fn a_port_already_in_use_exits_1_and_names_the_address() {
    let fixture = Fixture::new();
    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port();

    let mut daemon = fixture
        .command(port, &["run"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let exit = exit_within(&mut daemon, WAIT);
    let exit = exit.expect("the daemon kept running on a port that is taken");
    let mut stderr = String::new();
    std::io::Read::read_to_string(&mut daemon.stderr.take().unwrap(), &mut stderr).unwrap();

    assert_eq!(exit.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains(&format!("127.0.0.1:{port}")),
        "stderr: {stderr}"
    );
}
