//! The `foundryd` binary run as a daemon: the data-directory lock, a clean stop that leaves
//! terminals running, `status`, the logins and skill it holds, the peer route's key proof and
//! the gateway settings. Each test uses its own data directory, a free port and no copy from
//! the keychain, never the defaults or the login keychain.

// A release build ignores `FOUNDRY_SECRETS`, so on macOS these runs of the binary would move
// secrets out of the login keychain.
#[cfg(all(target_os = "macos", not(debug_assertions)))]
compile_error!(
    "run these tests in a debug build: a release build on macOS copies secrets out of the login keychain"
);

// A release build ignores `FOUNDRY_TAILSCALE`, so these runs of the binary would ask the real
// Tailscale.
#[cfg(not(debug_assertions))]
compile_error!("run these tests in a debug build: a release build asks the real Tailscale");

use std::fs;
use std::net::{IpAddr, Ipv4Addr, TcpListener};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use foundry_proto::factory::{
    FactoryBody, FactoryRecord, HeldUp, HeldUpKind, ProjectFactory, RunId,
};
use foundry_proto::ids::{MachineId, ProjectId, TerminalId};
use foundry_proto::messages::{PROTOCOL, PeerMessage, UiMessage};
use foundry_proto::record::{AnyRecord, MachineSample, Record, Session};
use foundry_proto::settings::{Machine, Project, SharedSettings};
use foundry_proto::state::{SessionState, TerminalState};
use foundryd::join::{KeyedDialer, Keys};
use foundryd::secrets::{FileSecrets, Secrets};
use foundryd::sessions::claude;
use foundryd::store::Store;
use foundryd::sync::ws::WsDialer;
use foundryd::sync::{Conn, Dialer as _};
use futures::{SinkExt as _, StreamExt as _};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use tempfile::TempDir;
use tokio_tungstenite::tungstenite::Message;

const WAIT: Duration = Duration::from_secs(30);

/// Where a code is redeemed from in these tests: one made with `new_code` works from anywhere.
const HERE: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

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

#[test]
fn a_fixture_whose_port_another_process_took_starts_on_another() {
    let mut fixture = Fixture::new();
    // Another process can take the fixture's port before this test does, as it can before the
    // daemon does: then the test takes another one for the fixture.
    let taken = loop {
        match TcpListener::bind(("127.0.0.1", fixture.port)) {
            Ok(taken) => break taken,
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
                fixture.port = free_port();
            }
            Err(error) => panic!("cannot take port {}: {error}", fixture.port),
        }
    };
    let taken_port = fixture.port;
    // Whatever took the port hangs up on every caller, so asking it for the state fails fast.
    std::thread::spawn(move || taken.incoming().for_each(drop));

    fixture.start();

    assert_ne!(fixture.port, taken_port);
    assert!(fixture.state().is_some());
}

#[test]
fn free_ports_are_below_every_ephemeral_range_and_never_repeat() {
    let ports: Vec<u16> = (0..20).map(|_| free_port()).collect();

    assert!(ports.iter().all(|port| *port < 32768), "{ports:?}");
    let distinct: std::collections::HashSet<&u16> = ports.iter().collect();
    assert_eq!(distinct.len(), ports.len(), "{ports:?}");
}

/// Ports below every system's ephemeral range (Linux starts at 32768, macOS at 49152), so
/// neither another socket's `bind(0)` nor an outgoing connection is ever handed one.
const PORTS: std::ops::Range<u32> = 20000..32000;

/// A port nothing listens on now. The daemon binds it only later, so it comes from [`PORTS`],
/// where the system hands nothing out in between. No port is given twice in one run, and each
/// run starts at a random place, so two runs rarely meet. Another process can still take the
/// port first: [`Fixture::start`] then moves to another.
fn free_port() -> u16 {
    use std::hash::{BuildHasher as _, Hasher as _};
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    static START: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    let span = PORTS.end - PORTS.start;
    let start = *START.get_or_init(|| {
        let random = std::collections::hash_map::RandomState::new()
            .build_hasher()
            .finish();
        u32::try_from(random % u64::from(span)).unwrap()
    });
    loop {
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        assert!(n < span, "every port in {PORTS:?} is taken");
        let port = u16::try_from(PORTS.start + (start + n) % span).unwrap();
        if TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
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
        .with_tailscale_stopped()
    }

    /// Puts a stand-in for the Tailscale CLI in the daemon's `HOME`, so no test asks the real
    /// one. It answers as a stopped Tailscale does, and notes its arguments in `tailscale-asked`.
    fn with_tailscale_stopped(self) -> Self {
        let cli = self.user.path().join("tailscale");
        fs::write(
            &cli,
            "#!/bin/sh\necho \"$@\" > \"$(dirname \"$0\")/tailscale-asked\"\necho 'Tailscale is stopped (fake).' >&2\nexit 1\n",
        )
        .unwrap();
        fs::set_permissions(&cli, fs::Permissions::from_mode(0o755)).unwrap();
        self
    }

    /// Puts a stand-in for the Tailscale CLI in the daemon's `HOME` that answers as a running
    /// Tailscale does: `ip` with this machine's address, which no interface carries so nothing
    /// is bound to it, and `status --self --json` with this machine's names. It notes each
    /// question, a line each, in `tailscale-asked`.
    fn with_tailscale_answering(self) -> Self {
        let cli = self.user.path().join("tailscale");
        fs::write(
            &cli,
            r#"#!/bin/sh
echo "$*" >> "$(dirname "$0")/tailscale-asked"
case "$*" in
ip) echo 100.127.254.253 ;;
"status --self --json") echo '{"Self": {"HostName": "Personal Mac", "DNSName": "personal-mac.tail459612.ts.net.", "TailscaleIPs": ["100.127.254.253", "fd7a:115c:a1e0::e832:3612"]}}' ;;
*) exit 9 ;;
esac
"#,
        )
        .unwrap();
        fs::set_permissions(&cli, fs::Permissions::from_mode(0o755)).unwrap();
        self
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
            .env("FOUNDRY_TAILSCALE", self.user.path().join("tailscale"))
            .env("FOUNDRY_CLOCK_AHEAD", self.user.path().join("clock-ahead"))
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

    /// Starts the daemon and waits until it answers. When another process took the port first,
    /// the daemon exits naming it, and this moves to a fresh port and starts again.
    fn start(&mut self) {
        for _ in 0..5 {
            if self.try_start() {
                return;
            }
            self.port = free_port();
        }
        panic!("the daemon found its port taken five times; {}", self.log());
    }

    /// Starts the daemon and waits until it answers. False when it exited because its port was
    /// taken, so the caller can pick another.
    fn try_start(&mut self) -> bool {
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
                let log = self.log();
                if log.contains(&format!("cannot listen on 127.0.0.1:{}", self.port))
                    && log.contains("Address already in use")
                {
                    self.daemon = None;
                    return Some(false);
                }
                panic!("the daemon exited at start: {exit}; {log}");
            }
            self.state().map(|_| true)
        })
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

    /// Moves the running daemon's clock to `total` ahead of the system's, for the factory's
    /// ages: the daemon reads the file `FOUNDRY_CLOCK_AHEAD` names on each reading.
    fn clock_ahead(&self, total: Duration) {
        fs::write(
            self.user.path().join("clock-ahead"),
            total.as_secs().to_string(),
        )
        .unwrap();
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
fn after_sigterm_the_log_names_each_request_that_was_waiting_for_a_machine() {
    let mut fixture = Fixture::new();
    let project = fixture.project();
    fixture.seed(|settings| settings.projects.push(project.clone()));
    fixture.start();
    let me = wait_for("a machine sample", || {
        fixture
            .state()?
            .into_iter()
            .find_map(|record| match record {
                AnyRecord::MachineSample(sample) => Some(sample.owner),
                _ => None,
            })
    });
    assert_eq!(fixture.stop(), Some(0), "{}", fixture.log());
    // No session fits under a limit of none, so a create that lets Foundry choose waits.
    fixture.seed(|settings| {
        settings.limits.insert(
            me,
            foundry_proto::settings::Limits {
                max_sessions: 0,
                max_load_per_core: f64::MAX,
                min_free_disk: 0,
            },
        );
    });
    fixture.start();
    let create = serde_json::json!({
        "project": project.id, "base": "main", "name": "parked", "machine": "choose",
        "model": "test-model",
    });
    let (status, body) = fixture.call("POST", "/api/workspaces", Some(create));
    assert_eq!(status, 202, "{body}");

    assert_eq!(fixture.stop(), Some(0), "{}", fixture.log());

    let log = fixture.log();
    assert!(
        log.contains("the daemon stops, so the request for parked is dropped"),
        "{log}"
    );
}

/// Tailscale stopped, logged out or missing: the daemon asks it, still serves loopback, and
/// says why it serves nothing else in one warning.
#[test]
fn with_tailscale_stopped_the_daemon_serves_loopback_and_warns_once_why() {
    let mut fixture = Fixture::new();
    fixture.start();

    let asked = fs::read_to_string(fixture.user.path().join("tailscale-asked")).unwrap();
    assert_eq!(asked.trim(), "ip");
    let log = fixture.log();
    let warnings: Vec<&str> = log
        .lines()
        .filter(|line| line.contains("WARN") && line.contains("Tailscale"))
        .collect();
    assert_eq!(warnings.len(), 1, "{log}");
    assert!(
        warnings[0].contains("Tailscale is stopped (fake)."),
        "{log}"
    );
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
fn at_start_the_daemon_writes_the_logins_and_the_skill_this_machine_holds() {
    let mut fixture = Fixture::new();
    FileSecrets::new(fixture.home.path().join("secrets.json"))
        .set("github:octocat", "ghp_daemon_held")
        .unwrap();
    let skill = fixture
        .user
        .path()
        .join(".claude/skills/factory-build/SKILL.md");
    fs::create_dir_all(skill.parent().unwrap()).unwrap();
    fs::write(&skill, "the skill").unwrap();
    fixture.start();
    let me = wait_for("a machine sample", || {
        fixture
            .state()?
            .into_iter()
            .find_map(|record| match record {
                AnyRecord::MachineSample(sample) => Some(sample.owner),
                _ => None,
            })
    });

    // The daemon holds the store open; a read that finds it busy is tried again.
    let held = wait_for("this machine's held record", || {
        Store::open(fixture.home.path().join("store.sqlite"))
            .ok()?
            .held(me)
            .ok()?
    });

    assert_eq!(held.logins, ["octocat"]);
    assert!(held.skill);
    assert!(
        !fixture.log().contains("ghp_daemon_held"),
        "{}",
        fixture.log()
    );
}

#[test]
fn a_workspace_of_a_project_whose_factory_is_on_is_made_with_the_logins_token() {
    let mut fixture = Fixture::new();
    let token = "ghp_daemon_factory_0123456789";
    FileSecrets::new(fixture.home.path().join("secrets.json"))
        .set("github:octocat", token)
        .unwrap();
    let project = fixture.project();
    fixture.seed(|settings| {
        settings.projects.push(project.clone());
        settings.project_factory.insert(
            project.id,
            ProjectFactory {
                on: true,
                login: "octocat".into(),
                paused: false,
            },
        );
    });
    fixture.start();

    let (status, body) = fixture.call(
        "POST",
        "/api/workspaces",
        Some(serde_json::json!({"project": project.id, "base": "main", "name": "factory"})),
    );

    assert_eq!(status, 201, "{body}");
    assert!(!body.contains(token), "{body}");
    assert!(!fixture.log().contains(token), "the token reached the log");
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

#[test]
fn at_start_a_leftover_worktree_with_no_work_is_removed_and_its_branch_kept() {
    let mut fixture = Fixture::new();
    let project = fixture.project();
    fixture.seed(|settings| settings.projects.push(project.clone()));
    // What a create that died before writing its record leaves: the project's clone, a branch
    // and its worktree, and no workspace record.
    let clones = fixture.home.path().join("clones");
    let clone = clones.join(format!("{}.git", project.id));
    fs::create_dir_all(&clones).unwrap();
    git(&clones, &["init", "-q", "--bare", clone.to_str().unwrap()]);
    git(&clone, &["remote", "add", "origin", &project.repo]);
    git(&clone, &["fetch", "-q", "origin"]);
    git(
        &clone,
        &[
            "branch",
            "-q",
            "--no-track",
            "crashed",
            "refs/remotes/origin/main",
        ],
    );
    let worktree = fixture
        .home
        .path()
        .join("worktrees")
        .join(project.id.to_string())
        .join("crashed");
    git(
        &clone,
        &[
            "worktree",
            "add",
            "-q",
            "--",
            worktree.to_str().unwrap(),
            "crashed",
        ],
    );

    fixture.start();

    assert!(!worktree.exists(), "{}", fixture.log());
    git(&clone, &["rev-parse", "--verify", "refs/heads/crashed"]);
    assert_eq!(fixture.stop(), Some(0));
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
            key: String::new(),
        });
    });
    fixture.start();

    let url = format!("ws://127.0.0.1:{}/api/peer", fixture.port);
    let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    let hello = PeerMessage::Hello {
        protocol: PROTOCOL,
        machine: peer,
        versions: Default::default(),
        factory_versions: Default::default(),
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
        mine.redeem(&code.to_string(), peer_id, &peer.public_key(), HERE)
            .unwrap();
    }
    fixture.seed(|settings| {
        settings.machines.push(Machine {
            id: peer_id,
            name: "peer".into(),
            address: "::1".into(),
            key: String::new(),
        });
    });
    // The daemon listens on 127.0.0.1 and Tailscale addresses only, so this port is free here.
    // It dials the peer on its own port, so a start on another port needs a listener there.
    let listener = loop {
        let listener = tokio::net::TcpListener::bind(("::1", fixture.port))
            .await
            .expect("::1 must be bindable for this test: it plays the peer at address ::1");
        if fixture.try_start() {
            break listener;
        }
        fixture.port = free_port();
    };

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

#[tokio::test]
async fn the_daemon_admits_a_key_its_machine_list_carries_and_dials_that_peer() {
    let mut fixture = Fixture::new();
    let other = tempfile::tempdir().unwrap();
    let peer = Keys::open(Arc::new(FileSecrets::new(
        other.path().join("secrets.json"),
    )))
    .unwrap();
    let peer_id = peer.me();
    // The key table does not hold the peer: only its machine-list entry carries its key.
    fixture.seed(|settings| {
        settings.machines.push(Machine {
            id: peer_id,
            name: "peer".into(),
            address: "::1".into(),
            key: peer.public_key().hex().to_owned(),
        });
    });
    let listener = loop {
        let listener = tokio::net::TcpListener::bind(("::1", fixture.port))
            .await
            .expect("::1 must be bindable for this test: it plays the peer at address ::1");
        if fixture.try_start() {
            break listener;
        }
        fixture.port = free_port();
    };

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
    .expect("the daemon did not dial the peer its list carries the key of");

    let said: serde_json::Value = serde_json::from_str(&first).unwrap();
    assert_eq!(said["type"], "challenge", "first frame: {first}");
}

/// A secret read that never returns, as one from a disk that does not answer does: the secret
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

/// Starts the fixture's daemon with one more machine in its list, whose keys both sides have
/// exchanged, and opens a peer link to the daemon as that machine, hellos exchanged. Returns
/// the link, the peer's id, and the directory holding the peer's secrets.
async fn keyed_peer_link(fixture: &mut Fixture) -> (Conn, MachineId, TempDir) {
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
        mine.redeem(&code.to_string(), peer_id, &peer.public_key(), HERE)
            .unwrap();
        let code = peer.new_code().unwrap();
        peer.redeem(&code.to_string(), mine.me(), &mine.public_key(), HERE)
            .unwrap();
        Machine {
            id: mine.me(),
            name: "daemon".into(),
            address: "127.0.0.1".into(),
            key: String::new(),
        }
    };
    fixture.seed(|settings| {
        settings.machines.push(Machine {
            id: peer_id,
            name: "peer".into(),
            address: "127.0.0.2".into(),
            key: String::new(),
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
        factory_versions: Default::default(),
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
    (conn, peer_id, other)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_daemons_peer_link_sends_its_shared_settings_after_the_hellos() {
    let mut fixture = Fixture::new();
    let (mut conn, _, _other) = keyed_peer_link(&mut fixture).await;
    let settings = tokio::time::timeout(Duration::from_secs(20), async {
        while let Some(text) = conn.recv.recv().await {
            if let Ok(PeerMessage::Settings { settings, .. }) = serde_json::from_str(&text) {
                return Some(settings);
            }
        }
        None
    })
    .await
    .expect("the daemon sent no settings on the peer link")
    .expect("the link closed before the daemon sent its settings");
    assert!(settings.machines.iter().any(|m| m.name == "peer"));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_daemons_peer_route_takes_a_keyed_peer_and_syncs_its_records() {
    let mut fixture = Fixture::new();
    let (conn, peer_id, _other) = keyed_peer_link(&mut fixture).await;

    let sample = AnyRecord::MachineSample(Record {
        owner: peer_id,
        version: 1,
        deleted: false,
        body: MachineSample {
            load_per_core: 0.25,
            free_memory: 1 << 30,
            free_disk: 1 << 35,
            sessions: 3,
            update_refused: None,
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

/// The items `GET /api/attention` answers with.
fn attention(fixture: &Fixture) -> Vec<serde_json::Value> {
    let (status, body) = fixture.call("GET", "/api/attention", None);
    assert_eq!(status, 200, "{body}");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    json["items"].as_array().unwrap().clone()
}

#[test]
fn a_waiting_for_you_item_is_listed_within_5_seconds_of_the_hook() {
    let mut fixture = Fixture::new();
    FileSecrets::new(fixture.home.path().join("secrets.json"))
        .set("gateway", "sk-test-waiting-0123456789")
        .unwrap();
    fixture.gateway_url = Some("http://gw.invalid:8317".into());
    // `claude` is the fake agent: it starts a turn and asks for permission at once, then
    // waits.
    let bin = fixture.user.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let script = fixture.user.path().join("agent.yaml");
    fs::write(
        &script,
        "- hook: {event: UserPromptSubmit, prompt: \"tidy up\"}\n- hook: {event: Notification, message: \"Claude needs your permission to use Bash\"}\n- sleep_ms: 600000\n",
    )
    .unwrap();
    let agent = Path::new(env!("CARGO_BIN_EXE_foundryd")).with_file_name("foundry-fake-agent");
    assert!(
        agent.exists(),
        "run `cargo test --workspace` to build the fake agent"
    );
    let claude = bin.join("claude");
    fs::write(
        &claude,
        format!(
            "#!/bin/sh\nexec {} --script {} \"$@\"\n",
            agent.display(),
            script.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&claude, fs::Permissions::from_mode(0o755)).unwrap();
    fixture.path_first = Some(bin);
    let project = fixture.project();
    fixture.seed(|settings| settings.projects.push(project.clone()));
    fixture.start();
    let workspace = fixture.workspace(&project);

    // The hook fires after the start, so the start bounds it from before.
    let started = Instant::now();
    let (status, body) = fixture.call(
        "POST",
        &format!("/api/workspaces/{workspace}/sessions"),
        Some(serde_json::json!({"model": "opus"})),
    );
    assert_eq!(status, 201, "{body}; {}", fixture.log());
    let session = id_of(&body);
    let item = wait_for("a waiting-for-you item", || {
        attention(&fixture)
            .into_iter()
            .find(|item| item["kind"] == "session_waiting")
    });
    let took = started.elapsed();
    assert!(took < Duration::from_secs(5), "listed after {took:?}");
    assert_eq!(item["id"], format!("session_waiting:{session}"));
    assert!(
        item["what"].as_str().unwrap().contains("waiting for you"),
        "{item}"
    );
}

/// Makes `claude` on the daemon's `PATH` the fake agent acting out `script`, and gives the
/// daemon a gateway, so a session can start.
fn fake_claude(fixture: &mut Fixture, script: &str) {
    FileSecrets::new(fixture.home.path().join("secrets.json"))
        .set("gateway", "sk-test-restart-0123456789")
        .unwrap();
    fixture.gateway_url = Some("http://gw.invalid:8317".into());
    let bin = fixture.user.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let steps = fixture.user.path().join("agent.yaml");
    fs::write(&steps, script).unwrap();
    let agent = Path::new(env!("CARGO_BIN_EXE_foundryd")).with_file_name("foundry-fake-agent");
    assert!(
        agent.exists(),
        "run `cargo test --workspace` to build the fake agent"
    );
    let claude = bin.join("claude");
    fs::write(
        &claude,
        format!(
            "#!/bin/sh\nexec {} --script {} \"$@\"\n",
            agent.display(),
            steps.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&claude, fs::Permissions::from_mode(0o755)).unwrap();
    fixture.path_first = Some(bin);
}

/// Starts a session in a new workspace of a new project, and gives its id.
fn started_session(fixture: &mut Fixture) -> String {
    let project = fixture.project();
    fixture.seed(|settings| settings.projects.push(project.clone()));
    fixture.start();
    let workspace = fixture.workspace(&project);
    session_in(fixture, &workspace)
}

/// Starts a session in `workspace`, and gives its id.
fn session_in(fixture: &Fixture, workspace: &str) -> String {
    let (status, body) = fixture.call(
        "POST",
        &format!("/api/workspaces/{workspace}/sessions"),
        Some(serde_json::json!({"model": "opus"})),
    );
    assert_eq!(status, 201, "{body}; {}", fixture.log());
    id_of(&body)
}

/// Waits for session `id`'s record to be in one of `states`, and gives it.
fn wait_for_session(fixture: &Fixture, id: &str, states: &[SessionState]) -> Record<Session> {
    let found = || {
        fixture
            .state()?
            .into_iter()
            .find_map(|record| match record {
                AnyRecord::Session(r) if r.body.id.to_string() == id => Some(r),
                _ => None,
            })
    };
    let end = Instant::now() + WAIT;
    loop {
        let record = found();
        if let Some(record) = record.as_ref().filter(|r| states.contains(&r.body.state)) {
            return record.clone();
        }
        assert!(
            Instant::now() < end,
            "session {id} is not {states:?} but {:?}; {}",
            record.map(|r| r.body.state),
            fixture.log()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Waits for this machine's sample written after `version` that counts no session.
fn wait_for_no_sessions_after(fixture: &Fixture, version: u64) {
    wait_for("a newer machine sample with no session", || {
        fixture
            .state()?
            .into_iter()
            .find_map(|record| match record {
                AnyRecord::MachineSample(s) if s.version > version && s.body.sessions == 0 => {
                    Some(())
                }
                _ => None,
            })
    });
}

/// The status a `UserPromptSubmit` hook for `session` gets with the bearer `token`.
fn hook_status(fixture: &Fixture, session: &str, token: &str) -> u16 {
    let url = format!("http://127.0.0.1:{}/hooks/{session}", fixture.port);
    Fixture::agent()
        .post(&url)
        .header("Authorization", format!("Bearer {token}"))
        .send_json(serde_json::json!({"hook_event_name": "UserPromptSubmit"}))
        .unwrap()
        .status()
        .as_u16()
}

/// Closes `terminal`, then waits for `session` to end and for a newer sample to count no
/// session.
fn close_and_wait_for_the_end(fixture: &Fixture, session: &str, terminal: TerminalId) {
    let (status, body) = fixture.call("POST", &format!("/api/terminals/{terminal}/close"), None);
    assert_eq!(status, 204, "{body}");
    let ended = wait_for_session(
        fixture,
        session,
        &[SessionState::Finished, SessionState::Failed],
    );
    wait_for_no_sessions_after(fixture, ended.version);
}

#[test]
fn a_session_keeps_reporting_after_the_daemon_restarts() {
    let mut fixture = Fixture::new();
    fake_claude(
        &mut fixture,
        "- hook: {event: SessionStart}\n- hook: {event: UserPromptSubmit, prompt: \"tidy up\"}\n- sleep_ms: 8000\n- hook: {event: Stop}\n- sleep_ms: 600000\n",
    );
    let session = started_session(&mut fixture);
    let working = wait_for_session(&fixture, &session, &[SessionState::Working]);
    let curl = fixture
        .home
        .path()
        .join("sessions")
        .join(format!("{session}.curl"));
    let token = claude::curl_token(&curl).unwrap();

    let port = fixture.port;
    assert_eq!(fixture.stop(), Some(0));
    fixture.start();
    assert_eq!(fixture.port, port, "the daemon came back on another port");

    let idle = wait_for_session(&fixture, &session, &[SessionState::Idle]);
    assert_eq!(idle.body.terminal, working.body.terminal);

    let wrong = "0123456789abcdef0123456789abcdef";
    assert_eq!(hook_status(&fixture, &session, wrong), 403, "a wrong token");

    close_and_wait_for_the_end(&fixture, &session, idle.body.terminal);
    assert!(!fixture.log().contains(&token), "the token reached the log");
}

#[test]
fn two_sessions_taken_up_after_a_restart_each_keep_their_own_hook_token() {
    let mut fixture = Fixture::new();
    fake_claude(
        &mut fixture,
        "- hook: {event: SessionStart}\n- hook: {event: UserPromptSubmit, prompt: \"tidy up\"}\n- sleep_ms: 8000\n- hook: {event: Stop}\n- sleep_ms: 600000\n",
    );
    let project = fixture.project();
    fixture.seed(|settings| settings.projects.push(project.clone()));
    fixture.start();
    let workspace = fixture.workspace(&project);
    let sessions = [
        session_in(&fixture, &workspace),
        session_in(&fixture, &workspace),
    ];
    let tokens = sessions.each_ref().map(|session| {
        wait_for_session(&fixture, session, &[SessionState::Working]);
        let curl = fixture
            .home
            .path()
            .join("sessions")
            .join(format!("{session}.curl"));
        claude::curl_token(&curl).unwrap()
    });

    let port = fixture.port;
    assert_eq!(fixture.stop(), Some(0));
    fixture.start();
    assert_eq!(fixture.port, port, "the daemon came back on another port");

    let idle = sessions
        .each_ref()
        .map(|session| wait_for_session(&fixture, session, &[SessionState::Idle]));
    for (mine, other) in [(0, 1), (1, 0)] {
        assert_eq!(
            hook_status(&fixture, &sessions[mine], &tokens[other]),
            403,
            "session {mine} with session {other}'s token"
        );
        assert_eq!(
            hook_status(&fixture, &sessions[mine], &tokens[mine]),
            204,
            "session {mine} with its own token"
        );
    }

    for record in &idle {
        let terminal = record.body.terminal;
        let (status, body) =
            fixture.call("POST", &format!("/api/terminals/{terminal}/close"), None);
        assert_eq!(status, 204, "{body}");
    }
    let ended = sessions.each_ref().map(|session| {
        wait_for_session(
            &fixture,
            session,
            &[SessionState::Finished, SessionState::Failed],
        )
        .version
    });
    wait_for_no_sessions_after(&fixture, ended.into_iter().max().unwrap());
}

#[test]
fn a_hook_posted_while_the_daemon_restarts_reaches_it_once_it_is_back() {
    let mut fixture = Fixture::new();
    fake_claude(
        &mut fixture,
        "- hook: {event: SessionStart}\n- hook: {event: UserPromptSubmit, prompt: \"tidy up\"}\n- sleep_ms: 3000\n- hook: {event: Stop}\n- sleep_ms: 600000\n",
    );
    let session = started_session(&mut fixture);
    wait_for_session(&fixture, &session, &[SessionState::Working]);

    let port = fixture.port;
    assert_eq!(fixture.stop(), Some(0));
    // The agent posts Stop 3 seconds after the prompt, while nothing listens.
    std::thread::sleep(Duration::from_secs(4));
    fixture.start();
    assert_eq!(fixture.port, port, "the daemon came back on another port");

    let idle = wait_for_session(&fixture, &session, &[SessionState::Idle]);
    close_and_wait_for_the_end(&fixture, &session, idle.body.terminal);
}

#[test]
fn a_session_whose_agent_ended_while_the_daemon_was_down_is_ended_at_start() {
    let mut fixture = Fixture::new();
    fake_claude(
        &mut fixture,
        "- hook: {event: SessionStart}\n- hook: {event: UserPromptSubmit, prompt: \"tidy up\"}\n- sleep_ms: 3000\n",
    );
    let session = started_session(&mut fixture);
    wait_for_session(&fixture, &session, &[SessionState::Working]);

    assert_eq!(fixture.stop(), Some(0));
    std::thread::sleep(Duration::from_secs(5));
    fixture.start();

    let ended = wait_for_session(
        &fixture,
        &session,
        &[SessionState::Finished, SessionState::Failed],
    );
    // The state is the one the terminal's exit code gives.
    let code = fixture
        .state()
        .unwrap()
        .into_iter()
        .find_map(|record| match record {
            AnyRecord::Terminal(r) if r.body.id == ended.body.terminal => Some(r.body.exit_code),
            _ => None,
        })
        .unwrap_or(None);
    let wanted = if code == Some(0) {
        SessionState::Finished
    } else {
        SessionState::Failed
    };
    assert_eq!(ended.body.state, wanted, "exit code {code:?}");
    wait_for_no_sessions_after(&fixture, ended.version);
    let dir = fixture.home.path().join("sessions");
    for file in [format!("{session}.json"), format!("{session}.curl")] {
        assert!(!dir.join(&file).exists(), "{file} is still there");
    }
}

#[test]
fn a_session_whose_curl_config_lost_its_token_is_still_followed_to_its_end() {
    let mut fixture = Fixture::new();
    fake_claude(
        &mut fixture,
        "- hook: {event: SessionStart}\n- hook: {event: UserPromptSubmit, prompt: \"tidy up\"}\n- sleep_ms: 600000\n",
    );
    let session = started_session(&mut fixture);
    let working = wait_for_session(&fixture, &session, &[SessionState::Working]);
    let curl = fixture
        .home
        .path()
        .join("sessions")
        .join(format!("{session}.curl"));
    let token = claude::curl_token(&curl).unwrap();

    assert_eq!(fixture.stop(), Some(0));
    fs::write(&curl, "header = \"Content-Type: application/json\"\n").unwrap();
    fixture.start();

    // Read before a hook is refused, which logs a warning naming the session too.
    let log = fixture.log();
    assert!(
        log.lines()
            .any(|line| line.contains("holds no token") && line.contains(&session)),
        "no warning names the session; {log}"
    );
    assert_eq!(
        hook_status(&fixture, &session, &token),
        403,
        "the old token"
    );
    assert!(!log.contains(&token), "the token reached the log");
    close_and_wait_for_the_end(&fixture, &session, working.body.terminal);
}

#[test]
fn a_daemon_with_no_gateway_url_lists_gateway_unhealthy_naming_it() {
    let mut fixture = Fixture::new();
    fixture.start();
    let item = wait_for("a gateway item", || {
        attention(&fixture)
            .into_iter()
            .find(|item| item["kind"] == "gateway_unhealthy")
    });
    let what = item["what"].as_str().unwrap();
    assert!(what.contains("FOUNDRY_GATEWAY_URL"), "{what}");
    assert!(what.contains("gateway-management"), "{what}");
}

#[test]
fn the_attention_routes_refuse_a_request_from_another_site() {
    use std::io::{Read as _, Write as _};
    let mut fixture = Fixture::new();
    fixture.start();
    let ask = |method: &str, path: &str, origin: Option<&str>| {
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", fixture.port)).unwrap();
        let origin = origin.map_or_else(String::new, |o| format!("Origin: {o}\r\n"));
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n{origin}Connection: close\r\nContent-Length: 0\r\n\r\n",
            fixture.port
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response.split(' ').nth(1).unwrap().parse::<u16>().unwrap()
    };
    let foreign = Some("http://evil.example");
    assert_eq!(ask("GET", "/api/attention", None), 200);
    assert_eq!(ask("GET", "/api/attention", foreign), 403);
    assert_eq!(ask("POST", "/api/attention/x/dismiss", foreign), 403);
}

#[test]
fn the_project_routes_refuse_a_request_from_another_site() {
    use std::io::{Read as _, Write as _};
    let mut fixture = Fixture::new();
    fixture.start();
    // As in the rename route's test: only a request the route serves sends a body.
    let ask = |path: &str, host: &str, origin: Option<&str>, body: &str| {
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", fixture.port)).unwrap();
        let origin = origin.map_or_else(String::new, |o| format!("Origin: {o}\r\n"));
        write!(
            stream,
            "POST {path} HTTP/1.1\r\nHost: {host}\r\n{origin}Connection: close\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response.split(' ').nth(1).unwrap().parse::<u16>().unwrap()
    };
    let here = format!("127.0.0.1:{}", fixture.port);
    let foreign = Some("http://evil.example");
    // Served here: a project with no name is refused by the route itself.
    let nameless = r#"{"name": "", "repo": ""}"#;
    assert_eq!(ask("/api/projects", &here, None, nameless), 422);
    assert_eq!(ask("/api/projects", &here, foreign, ""), 403);
    assert_eq!(ask("/api/projects", "evil.example", None, ""), 403);
    let remove = format!("/api/projects/{}/remove", ProjectId::random());
    assert_eq!(ask(&remove, &here, foreign, ""), 403);
    assert_eq!(ask(&remove, "evil.example", None, ""), 403);
}

/// `GET /api/machines` once it answers 200: which machine is this one, and the list.
fn machines(fixture: &Fixture) -> (String, Vec<serde_json::Value>) {
    let (status, body) = fixture.call("GET", "/api/machines", None);
    assert_eq!(status, 200, "{body}");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    (
        json["me"].as_str().unwrap().to_owned(),
        json["machines"].as_array().unwrap().clone(),
    )
}

#[test]
fn a_daemon_lists_itself_under_its_tailscale_name_once_tailscale_answers_and_can_be_renamed() {
    let mut fixture = Fixture::new().with_tailscale_answering();
    fixture.start();

    // Its first look, when Tailscale first answers, waits for peers' copies: nothing is sent.
    let asked = fixture.user.path().join("tailscale-asked");
    wait_for("the daemon to ask what Tailscale calls it", || {
        fs::read_to_string(&asked)
            .ok()?
            .contains("status --self --json")
            .then_some(())
    });
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(machines(&fixture).1, Vec::<serde_json::Value>::new());

    let me = wait_for("this machine in the machine list", || {
        let (me, machines) = machines(&fixture);
        machines
            .iter()
            .any(|m| m["id"] == me.as_str() && m["name"] == "personal-mac")
            .then_some(me)
    });
    assert_eq!(machines(&fixture).1.len(), 1);

    let (status, body) = fixture.call(
        "POST",
        &format!("/api/machines/{me}/rename"),
        Some(serde_json::json!({"name": " Tom's desk "})),
    );
    assert_eq!(status, 204, "{body}");
    let (_, listed) = machines(&fixture);
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["name"], "Tom's desk");
    assert_eq!(fixture.stop(), Some(0));

    // Its entry carries its own public key, so every other machine admits it.
    let keys = Keys::open(Arc::new(FileSecrets::new(
        fixture.home.path().join("secrets.json"),
    )))
    .unwrap();
    let store = Store::open(fixture.home.path().join("store.sqlite")).unwrap();
    let own = store
        .settings()
        .unwrap()
        .machines
        .into_iter()
        .find(|m| m.id.to_string() == me)
        .unwrap();
    assert_eq!(own.key, keys.public_key().hex());
}

#[test]
fn the_rename_route_refuses_a_request_from_another_site() {
    use std::io::{Read as _, Write as _};
    let mut fixture = Fixture::new();
    fixture.start();
    // A refused request is answered before its body is read, and a close with the body unread
    // can reset the link before the answer is read, so only the request served sends one.
    let ask = |path: &str, host: &str, origin: Option<&str>, body: &str| {
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", fixture.port)).unwrap();
        let origin = origin.map_or_else(String::new, |o| format!("Origin: {o}\r\n"));
        write!(
            stream,
            "POST {path} HTTP/1.1\r\nHost: {host}\r\n{origin}Connection: close\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response.split(' ').nth(1).unwrap().parse::<u16>().unwrap()
    };
    let here = format!("127.0.0.1:{}", fixture.port);
    let rename = format!("/api/machines/{}/rename", MachineId::random());
    // Served here: a machine the list does not hold is refused by the route itself.
    assert_eq!(ask(&rename, &here, None, r#"{"name": "desk"}"#), 404);
    assert_eq!(ask(&rename, &here, Some("http://evil.example"), ""), 403);
    assert_eq!(ask(&rename, "evil.example", None, ""), 403);
}

#[test]
fn the_limits_routes_refuse_a_request_from_another_site() {
    use std::io::{Read as _, Write as _};
    let mut fixture = Fixture::new();
    fixture.start();
    // As in the rename route's test: only a request the route serves sends a body.
    let ask = |path: &str, host: &str, origin: Option<&str>, body: &str| {
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", fixture.port)).unwrap();
        let origin = origin.map_or_else(String::new, |o| format!("Origin: {o}\r\n"));
        write!(
            stream,
            "POST {path} HTTP/1.1\r\nHost: {host}\r\n{origin}Connection: close\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        let status = response.split(' ').nth(1).unwrap().parse::<u16>().unwrap();
        (status, response)
    };
    let here = format!("127.0.0.1:{}", fixture.port);
    let machine = MachineId::random();
    let limits = format!("/api/machines/{machine}/limits");
    let clear = format!("/api/machines/{machine}/limits/clear");
    let body = r#"{"max_sessions": 2, "max_load_per_core": 1, "min_free_disk_gb": 20}"#;
    // Served here: a machine the list does not hold is refused by the route itself, not as an
    // unrouted path.
    for (path, body) in [(&limits, body), (&clear, "")] {
        let (status, answer) = ask(path, &here, None, body);
        assert_eq!(status, 404, "{path}");
        assert!(answer.contains("machine list"), "{path}: {answer}");
    }
    for path in [&limits, &clear] {
        assert_eq!(
            ask(path, &here, Some("http://evil.example"), "").0,
            403,
            "{path}"
        );
        assert_eq!(ask(path, "evil.example", None, "").0, 403, "{path}");
    }
}

#[test]
fn the_join_and_add_routes_are_served_and_refuse_a_request_from_another_site() {
    use std::io::{Read as _, Write as _};
    let mut fixture = Fixture::new();
    fixture.start();
    // As in the rename route's test: only a request the route serves sends a body.
    let ask = |path: &str, host: &str, origin: Option<&str>, body: &str| {
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", fixture.port)).unwrap();
        let origin = origin.map_or_else(String::new, |o| format!("Origin: {o}\r\n"));
        write!(
            stream,
            "POST {path} HTTP/1.1\r\nHost: {host}\r\n{origin}Connection: close\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response.split(' ').nth(1).unwrap().parse::<u16>().unwrap()
    };
    let here = format!("127.0.0.1:{}", fixture.port);
    for path in ["/api/join", "/api/machines/add"] {
        // Served here: a body the route cannot take is refused by the route itself.
        assert_eq!(ask(path, &here, None, "{}"), 422, "{path}");
        assert_eq!(
            ask(path, &here, Some("http://evil.example"), ""),
            403,
            "{path}"
        );
        assert_eq!(ask(path, "evil.example", None, ""), 403, "{path}");
    }
}

#[test]
fn the_remove_route_is_served_and_refuses_a_request_from_another_site() {
    use std::io::{Read as _, Write as _};
    let mut fixture = Fixture::new();
    fixture.start();
    let ask = |path: &str, host: &str, origin: Option<&str>| {
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", fixture.port)).unwrap();
        let origin = origin.map_or_else(String::new, |o| format!("Origin: {o}\r\n"));
        write!(
            stream,
            "POST {path} HTTP/1.1\r\nHost: {host}\r\n{origin}Connection: close\r\n\
             Content-Length: 0\r\n\r\n"
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        let status = response.split(' ').nth(1).unwrap().parse::<u16>().unwrap();
        (status, response)
    };
    let here = format!("127.0.0.1:{}", fixture.port);
    let remove = format!("/api/machines/{}/remove", MachineId::random());
    // Served here: a machine the list does not hold is refused by the route itself, not as an
    // unrouted path.
    let (status, answer) = ask(&remove, &here, None);
    assert_eq!(status, 404, "{answer}");
    assert!(answer.contains("machine list"), "{answer}");
    assert_eq!(ask(&remove, &here, Some("http://evil.example")).0, 403);
    assert_eq!(ask(&remove, "evil.example", None).0, 403);
}

/// The settings `GET /api/state` returns, once it answers 200.
fn settings_of(fixture: &Fixture) -> SharedSettings {
    let (status, body) = fixture.call("GET", "/api/state", None);
    assert_eq!(status, 200, "{body}");
    match serde_json::from_str(&body).unwrap() {
        UiMessage::State { settings, .. } => settings,
        other => panic!("not a state: {other:?}"),
    }
}

#[test]
fn the_priority_route_is_served_and_refuses_a_request_from_another_site() {
    use std::io::{Read as _, Write as _};
    let mut fixture = Fixture::new();
    fixture.start();
    // The answer, or `None` when the daemon reset the link before it was read. A refused request
    // is answered before its body is read; a body that arrives after the daemon closed resets
    // the link, and macOS then drops an answer not yet read. Sent in one write, the body comes
    // with the head, so that is rare, and the settings below say whether anything was stored.
    let ask = |host: &str, origin: Option<&str>, body: &str| {
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", fixture.port)).unwrap();
        let origin = origin.map_or_else(String::new, |o| format!("Origin: {o}\r\n"));
        let request = format!(
            "POST /api/machines/priority HTTP/1.1\r\nHost: {host}\r\n{origin}Connection: close\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        match stream.read_to_string(&mut response) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => return None,
            Err(e) => panic!("reading the answer: {e}"),
        }
        let status = response.split(' ').nth(1).unwrap().parse::<u16>().unwrap();
        Some((status, response))
    };
    let here = format!("127.0.0.1:{}", fixture.port);
    // Served here: an id the machine list does not hold is refused by the route itself, not as
    // an unrouted path.
    let stranger = format!(r#"{{"order": ["{}"]}}"#, MachineId::random());
    let (status, answer) = ask(&here, None, &stranger).expect("the route reads what it refuses");
    assert_eq!(status, 422, "{answer}");
    assert!(answer.contains("machine list"), "{answer}");
    // An order the route would take from this site, sent from another: refused, nothing set.
    let order = r#"{"order": []}"#;
    let before = settings_of(&fixture);
    for (host, origin) in [
        (here.as_str(), Some("http://evil.example")),
        ("evil.example", None),
    ] {
        if let Some((status, answer)) = ask(host, origin, order) {
            assert_eq!(status, 403, "{answer}");
        }
    }
    let after = settings_of(&fixture);
    assert_eq!(after.seq, before.seq);
    assert_eq!(after.priority, before.priority);
    // The same order from this site is stored, so the unchanged settings above are the refusal.
    let (status, answer) = ask(&here, None, order).expect("the route reads what it stores");
    assert_eq!(status, 204, "{answer}");
    assert_ne!(settings_of(&fixture).seq, before.seq);
}

const MINUTE: Duration = Duration::from_secs(60);

/// Adds `project` to the settings with its factory on, acting as a login nobody holds.
fn with_factory_on(settings: &mut SharedSettings, project: &Project) {
    settings.projects.push(project.clone());
    settings.project_factory.insert(
        project.id,
        ProjectFactory {
            on: true,
            login: "octocat".into(),
            paused: false,
        },
    );
}

/// A project that is only a name and a URL, for a test that never clones it.
fn named_project() -> Project {
    Project {
        id: ProjectId::random(),
        name: "foundry".into(),
        repo: "https://github.com/o/foundry.git".into(),
    }
}

/// The attention items of `kind`.
fn items_of(fixture: &Fixture, kind: &str) -> Vec<serde_json::Value> {
    attention(fixture)
        .into_iter()
        .filter(|item| item["kind"] == kind)
        .collect()
}

#[test]
fn a_factory_on_with_no_status_raises_factory_idle_once_the_clock_is_10_minutes_on() {
    let mut fixture = Fixture::new();
    let project = named_project();
    fixture.seed(|settings| with_factory_on(settings, &project));
    fixture.start();

    assert_eq!(
        items_of(&fixture, "factory_idle"),
        Vec::<serde_json::Value>::new()
    );
    fixture.clock_ahead(10 * MINUTE);

    let idle = items_of(&fixture, "factory_idle");
    assert_eq!(idle.len(), 1, "{idle:?}");
    assert!(
        idle[0]["what"].as_str().unwrap().contains("foundry"),
        "{idle:?}"
    );
}

/// A message on the peer link, to the daemon.
async fn send(conn: &Conn, message: &PeerMessage) {
    conn.send
        .send(serde_json::to_string(message).unwrap())
        .await
        .unwrap();
}

/// A message with no record, which keeps the link in use and so the peer online.
fn heartbeat() -> PeerMessage {
    PeerMessage::Records {
        protocol: PROTOCOL,
        records: Vec::new(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_holders_status_received_at_9_minutes_keeps_factory_idle_away_at_10() {
    let mut fixture = Fixture::new();
    let project = named_project();
    fixture.seed(|settings| with_factory_on(settings, &project));
    let (conn, peer, _other) = keyed_peer_link(&mut fixture).await;
    fixture.clock_ahead(9 * MINUTE);

    let mut status = foundryd::factory::status::build(foundryd::factory::status::PassResults {
        generation: 1,
        pass: 1,
        ..foundryd::factory::status::PassResults::new(project.id)
    });
    status.held_up.push(HeldUp {
        run: RunId::new("o/foundry", 12, 1).unwrap(),
        kind: HeldUpKind::Stuck,
        reason: "the gate failed".into(),
    });
    send(
        &conn,
        &PeerMessage::FactoryRecords {
            protocol: PROTOCOL,
            records: vec![FactoryRecord {
                owner: peer,
                version: 1,
                body: FactoryBody::Status(status),
            }],
        },
    )
    .await;
    tokio::task::block_in_place(|| {
        wait_for("the peer's stuck run", || {
            (!items_of(&fixture, "run_stuck").is_empty()).then_some(())
        });
    });

    fixture.clock_ahead(10 * MINUTE);
    send(&conn, &heartbeat()).await;
    tokio::task::block_in_place(|| {
        assert_eq!(
            items_of(&fixture, "factory_idle"),
            Vec::<serde_json::Value>::new()
        );
    });

    fixture.clock_ahead(20 * MINUTE);
    send(&conn, &heartbeat()).await;
    tokio::task::block_in_place(|| {
        let idle = items_of(&fixture, "factory_idle");
        assert_eq!(idle.len(), 1, "{idle:?}");
    });
}

#[test]
fn a_session_this_machine_marks_as_a_factory_session_raises_no_waiting_item_and_a_persons_does() {
    let mut fixture = Fixture::new();
    fake_claude(
        &mut fixture,
        "- hook: {event: UserPromptSubmit, prompt: \"tidy up\"}\n- hook: {event: Notification, message: \"Claude needs your permission to use Bash\"}\n- sleep_ms: 600000\n",
    );
    let project = fixture.project();
    // The factory is off while the workspace is made: a project whose factory is on makes one
    // only with a token for its login.
    fixture.seed(|settings| settings.projects.push(project.clone()));
    fixture.start();
    let workspace = fixture.workspace(&project);
    let sessions = [
        session_in(&fixture, &workspace),
        session_in(&fixture, &workspace),
    ];
    let waiting = |fixture: &Fixture| {
        items_of(fixture, "session_waiting")
            .into_iter()
            .map(|item| item["id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    let id_of_item = |session: &String| format!("session_waiting:{session}");
    wait_for("both sessions waiting", || {
        let ids = waiting(&fixture);
        sessions
            .iter()
            .all(|s| ids.contains(&id_of_item(s)))
            .then_some(())
    });
    assert_eq!(fixture.stop(), Some(0));
    fixture.seed(|settings| {
        settings.project_factory.insert(
            project.id,
            ProjectFactory {
                on: true,
                login: "octocat".into(),
                paused: false,
            },
        );
    });

    // The mark a factory start writes, as `sessions/factory.rs` does: private, in the
    // session's own folder.
    let folder = fixture.home.path().join("sessions").join(&sessions[0]);
    {
        use std::os::unix::fs::DirBuilderExt as _;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&folder)
            .unwrap();
    }
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(folder.join("factory"))
            .unwrap();
        file.write_all(project.id.to_string().as_bytes()).unwrap();
    }
    fixture.start();

    let ids = wait_for("the person's session waiting", || {
        let ids = waiting(&fixture);
        ids.contains(&id_of_item(&sessions[1])).then_some(ids)
    });
    assert!(!ids.contains(&id_of_item(&sessions[0])), "{ids:?}");
}

/// A project whose repo has a `.dw/config.yml` that provisions with a command that fails, so
/// each workspace's setup terminal exits non-zero.
fn project_with_failing_setup(fixture: &Fixture) -> Project {
    let project = fixture.project();
    let src = fixture.user.path().join("remote").join("src");
    fs::create_dir_all(src.join(".dw")).unwrap();
    fs::write(
        src.join(".dw").join("config.yml"),
        "factory:\n  provision:\n    - \"exit 3\"\n",
    )
    .unwrap();
    git(&src, &["add", "."]);
    git(&src, &["commit", "-q", "-m", "provision"]);
    git(&src, &["push", "-q", "origin", "main"]);
    project
}

/// A workspace of `project` called `name`, made over the API.
fn workspace_named(fixture: &Fixture, project: &Project, name: &str) -> String {
    let (status, body) = fixture.call(
        "POST",
        "/api/workspaces",
        Some(serde_json::json!({"project": project.id, "base": "main", "name": name})),
    );
    assert_eq!(status, 201, "{body}");
    id_of(&body)
}

#[test]
fn a_workspace_this_machine_marks_as_a_factory_workspace_raises_no_setup_failed_and_a_persons_does()
{
    let mut fixture = Fixture::new();
    let project = project_with_failing_setup(&fixture);
    // The factory is off while the workspaces are made: a project whose factory is on makes
    // one only with a token for its login.
    fixture.seed(|settings| settings.projects.push(project.clone()));
    fixture.start();
    let workspaces = [
        workspace_named(&fixture, &project, "one"),
        workspace_named(&fixture, &project, "two"),
    ];
    let failed = |fixture: &Fixture| {
        items_of(fixture, "setup_failed")
            .into_iter()
            .map(|item| item["id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    let item_of = |workspace: &String| format!("setup_failed:{workspace}");
    wait_for("both setups failed", || {
        let ids = failed(&fixture);
        workspaces
            .iter()
            .all(|w| ids.contains(&item_of(w)))
            .then_some(())
    });
    assert_eq!(fixture.stop(), Some(0));
    fixture.seed(|settings| {
        settings.project_factory.insert(
            project.id,
            ProjectFactory {
                on: true,
                login: "octocat".into(),
                paused: false,
            },
        );
    });

    // The marker `Workspaces` writes for a workspace made with a factory tag.
    fs::write(
        fixture
            .home
            .path()
            .join("worktrees")
            .join(project.id.to_string())
            .join(format!("{}.factory", workspaces[0])),
        format!(
            r#"{{"project":"{}","run":"o/r#1","name":"one"}}"#,
            project.id
        ),
    )
    .unwrap();
    fixture.start();

    let ids = wait_for("the person's setup failed", || {
        let ids = failed(&fixture);
        ids.contains(&item_of(&workspaces[1])).then_some(ids)
    });
    assert!(!ids.contains(&item_of(&workspaces[0])), "{ids:?}");
}
