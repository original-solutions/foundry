//! The `foundryd` binary run as a daemon: the data-directory lock, a clean stop that leaves
//! terminals running, `status`, the peer route's key proof and the gateway settings. Each
//! test uses its own data directory, a free port and the file secret store, never the
//! defaults or the login keychain.

use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use foundry_proto::ids::{MachineId, ProjectId, TerminalId};
use foundry_proto::messages::{PROTOCOL, PeerMessage, UiMessage};
use foundry_proto::record::AnyRecord;
use foundry_proto::settings::{Machine, Project, SharedSettings};
use foundry_proto::state::TerminalState;
use foundryd::secrets::{FileSecrets, Secrets};
use foundryd::store::Store;
use futures::{SinkExt as _, StreamExt as _};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use tempfile::TempDir;
use tokio_tungstenite::tungstenite::Message;

const WAIT: Duration = Duration::from_secs(30);

/// A pid file's marker: the shell the daemon starts runs `sleep` for this many seconds, so a
/// stray one is found by it.
const SLEEP: &str = "74331";

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
    daemon: Option<Child>,
}

impl Fixture {
    fn new() -> Self {
        Self {
            home: tempfile::Builder::new()
                .prefix("fd")
                .tempdir_in("/tmp")
                .unwrap(),
            user: tempfile::tempdir().unwrap(),
            port: free_port(),
            shell: PathBuf::from("/bin/sh"),
            daemon: None,
        }
    }

    /// `foundryd args` on this data directory and `port`, with no gateway URL.
    fn command(&self, port: u16, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_foundryd"));
        command
            .args(args)
            .env("FOUNDRY_HOME", self.home.path())
            .env("FOUNDRY_PORT", port.to_string())
            .env("FOUNDRY_SECRETS", "file")
            .env("HOME", self.user.path())
            .env("SHELL", &self.shell)
            .env_remove("FOUNDRY_GATEWAY_URL")
            .env_remove("CLAUDE_CONFIG_DIR");
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
        let mut child = self.daemon.take().expect("a daemon is running");
        kill(
            Pid::from_raw(i32::try_from(child.id()).unwrap()),
            Signal::SIGTERM,
        )
        .unwrap();
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

    let second = run_to_end(first.command(free_port(), &["run"]));

    assert_eq!(second.status.code(), Some(1), "{}", text(&second.stderr));
    assert!(
        text(&second.stderr).contains("daemon.lock"),
        "stderr: {}",
        text(&second.stderr)
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
