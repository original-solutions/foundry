//! Fleet tests of a call carried to another machine: machine A's API makes and acts on a
//! workspace machine B owns, over a call link from A to B.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use foundry_proto::ids::{MachineId, ProjectId, SessionId, TerminalId, WorkspaceId};
use foundry_proto::messages::{PROTOCOL, PeerMessage};
use foundry_proto::record::AnyRecord;
use foundry_proto::settings::Project;
use foundry_proto::state::SessionState;
use foundryd::api::{self, Api};
use foundryd::join::KeyedDialer;
use foundryd::secrets::{Edit, Secrets, SecretsError};
use foundryd::sessions::{self, Gateway, Sessions};
use foundryd::store::Store;
use foundryd::sync::call::{CARRIED, CallError};
use foundryd::sync::{Conn, Peers};
use foundryd::terminals::Terminals;
use foundryd::workspaces::Workspaces;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::relay::{Keyed, Nowhere, ServedBy, admit, keys};
use crate::*;

const WAIT: Duration = Duration::from_secs(20);

/// Runs the test `name` of this module in a child process of this test binary whose `HOME` and
/// global git config are its own, which must pass ([`run_child`]). A test that serves a factory
/// start (every [`Pair`]) runs so: the start checks the owner's git setup, and the global git
/// config of the machine running the tests may rewrite `https://github.com/`, which refuses every
/// start. `name` is the test function's name.
fn isolated<F, Fut>(name: &str, test: F)
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let module = module_path!();
    if ran_in_child(
        module,
        name,
        tokio::runtime::Builder::new_current_thread(),
        test,
    ) {
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let gitconfig = home.path().join(".gitconfig");
    std::fs::write(&gitconfig, "").unwrap();
    run_child(module, name, home.path(), &gitconfig, |_| {});
}

/// `cargo test --workspace` builds the holder and the fake agent beside the test binary's
/// `deps` folder.
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

/// A machine's whole API on a loopback port, over its store and peers: terminals, workspaces
/// and sessions that run the fake agent. Dropping it stops the servers and closes every holder
/// left under its data directory.
struct Served {
    addr: SocketAddr,
    api: Api,
    data: TempDir,
    servers: Vec<JoinHandle<()>>,
    _home: TempDir,
}

impl Drop for Served {
    fn drop(&mut self) {
        use std::io::Write as _;
        for server in &self.servers {
            server.abort();
        }
        let Ok(entries) = std::fs::read_dir(self.data.path().join("terminals")) else {
            return;
        };
        for entry in entries.flatten() {
            if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(entry.path()) {
                let _ = stream.write_all(&foundry_holder::protocol::Request::Close.encode());
            }
        }
    }
}

/// What a machine's sessions run and read besides P1's: the fake agent's script, and the secret
/// store a factory start reads its token from.
struct Factory {
    script: &'static str,
    secrets: Arc<dyn Secrets>,
    /// Why sessions cannot start here, as a daemon with no gateway says.
    refused: Option<&'static str>,
    /// The terminal holder to start, when not the one built.
    holder: Option<PathBuf>,
}

/// Puts the fake agent in front of a factory start's flags, which it does not take: it drops
/// `--dangerously-skip-permissions` and `--effort <effort>` and runs the agent with the rest.
const SHIM: &str = r#"agent=$1; shift
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

impl Served {
    async fn start(me: MachineId, store: Arc<Mutex<Store>>, peers: Peers) -> Self {
        Self::start_with(me, store, peers, None).await
    }

    async fn start_with(
        me: MachineId,
        store: Arc<Mutex<Store>>,
        peers: Peers,
        factory: Option<Factory>,
    ) -> Self {
        assert!(
            factory.is_none() || in_child(),
            "a test that serves a factory start runs in its own child process: wrap it in `isolated`"
        );
        // A Unix socket path is at most about 100 bytes, which the system's own temporary
        // folder on macOS would take most of.
        let data = tempfile::Builder::new()
            .prefix("fc")
            .tempdir_in("/tmp")
            .unwrap();
        let script = data.path().join("script.yaml");
        let steps = factory.as_ref().map_or("- wait_input\n", |f| f.script);
        std::fs::write(&script, steps).unwrap();
        let agent = built("foundry-fake-agent").to_string_lossy().into_owned();
        let script = script.to_string_lossy().into_owned();
        let claude = match &factory {
            None => vec![agent, "--script".into(), script],
            Some(_) => ["sh", "-c", SHIM, "sh", &agent, "--script", &script]
                .map(str::to_owned)
                .to_vec(),
        };
        let holder = factory
            .as_ref()
            .and_then(|f| f.holder.clone())
            .unwrap_or_else(|| built("foundry-holder"));
        let terminals = Arc::new(Terminals::new(me, store.clone(), data.path(), holder));
        let workspaces = Arc::new(Workspaces::new(
            me,
            store.clone(),
            data.path(),
            terminals.clone(),
        ));
        let hooks = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let home = tempfile::tempdir().unwrap();
        let sessions = Sessions::new(
            me,
            store.clone(),
            terminals.clone(),
            data.path(),
            sessions::Config {
                claude,
                claude_json: home.path().join(".claude.json"),
                hook_port: hooks.local_addr().unwrap().port(),
                gateway: Gateway {
                    base_url: "http://127.0.0.1:9/gateway".into(),
                    key: "unused".into(),
                },
            },
        );
        let refused = factory.as_ref().and_then(|f| f.refused);
        let sessions = Arc::new(match factory {
            Some(factory) => sessions.with_secrets(factory.secrets),
            None => sessions,
        });
        let hook_router = sessions.router();
        let mut api = Api::new(store, peers, terminals, workspaces, sessions, Vec::new());
        api.sessions_refused = refused.map(str::to_owned);
        let kept = api.clone();
        let placing = api.clone();
        let listener = api::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let servers = vec![
            tokio::spawn(foundryd::placement::run(placing)),
            tokio::spawn(async move {
                axum::serve(
                    hooks,
                    hook_router.into_make_service_with_connect_info::<SocketAddr>(),
                )
                .await
                .unwrap();
            }),
            tokio::spawn(async move {
                axum::serve(listener, api::router(api)).await.unwrap();
            }),
        ];
        Self {
            addr,
            api: kept,
            data,
            servers,
            _home: home,
        }
    }

    async fn of(daemon: &Daemon) -> Self {
        Self::start(daemon.id, daemon.store.clone(), daemon.peers.clone()).await
    }

    /// `POST path` with `body` as JSON, or with no body: the status and the JSON answered.
    async fn post(&self, path: &str, body: Option<Value>) -> (u16, Value) {
        let (status, text) = self.post_text(path, body).await;
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    /// `POST path` with `body` as JSON, or with no body: the status and the text answered.
    async fn post_text(&self, path: &str, body: Option<Value>) -> (u16, String) {
        self.post_with(path, body, &[]).await
    }

    /// [`Served::post_text`] with `headers` added.
    async fn post_with(
        &self,
        path: &str,
        body: Option<Value>,
        headers: &[(&'static str, String)],
    ) -> (u16, String) {
        let url = format!("http://{}{path}", self.addr);
        let headers = headers.to_vec();
        tokio::task::spawn_blocking(move || {
            let agent: ureq::Agent = ureq::Agent::config_builder()
                .http_status_as_error(false)
                .timeout_global(Some(WAIT))
                .build()
                .into();
            let mut request = agent.post(&url);
            for (name, value) in headers {
                request = request.header(name, value);
            }
            let mut response = match body {
                Some(body) => request.send_json(&body),
                None => request.send_empty(),
            }
            .unwrap();
            let status = response.status().as_u16();
            let text = response.body_mut().read_to_string().unwrap();
            (status, text)
        })
        .await
        .unwrap()
    }

    /// The JSON `GET path` answers.
    async fn get(&self, path: &str) -> Value {
        let url = format!("http://{}{path}", self.addr);
        tokio::task::spawn_blocking(move || {
            ureq::get(&url)
                .call()
                .unwrap()
                .body_mut()
                .read_json()
                .unwrap()
        })
        .await
        .unwrap()
    }

    /// The records `GET /api/state` shows.
    async fn state(&self) -> Vec<AnyRecord> {
        let url = format!("http://{}/api/state", self.addr);
        let state: Value = tokio::task::spawn_blocking(move || {
            ureq::get(&url)
                .call()
                .unwrap()
                .body_mut()
                .read_json()
                .unwrap()
        })
        .await
        .unwrap();
        serde_json::from_value(state["records"].clone()).unwrap()
    }

    /// The worktree of the one workspace made on this machine.
    fn worktree(&self) -> PathBuf {
        let projects = self.data.path().join("worktrees");
        let project = std::fs::read_dir(projects)
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        let mut branches = std::fs::read_dir(project.path()).unwrap();
        branches.next().unwrap().unwrap().path()
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

/// A project whose remote, a local bare repo under `remotes`, has one commit on `main`, added
/// to the settings every daemon of `fleet` holds.
fn project(fleet: &Fleet, remotes: &Path) -> ProjectId {
    git(remotes, &["init", "-q", "--bare", "remote.git"]);
    git(remotes, &["clone", "-q", "remote.git", "src"]);
    let src = remotes.join("src");
    std::fs::write(src.join("README"), "demo\n").unwrap();
    git(&src, &["add", "."]);
    git(&src, &["commit", "-q", "-m", "first"]);
    git(&src, &["push", "-q", "origin", "main"]);
    let project = ProjectId::random();
    for daemon in &fleet.daemons {
        let mut settings = daemon.settings();
        settings.seq += 1;
        settings.projects.push(Project {
            id: project,
            name: "demo".into(),
            repo: remotes.join("remote.git").to_string_lossy().into_owned(),
        });
        daemon.set_settings(settings);
    }
    project
}

/// Waits until `peers` hears from `machine`.
async fn hears(peers: &Peers, machine: MachineId) {
    until(WAIT, "the peer is heard from", || {
        peers.online().contains(&machine)
    })
    .await;
}

fn id_of(answer: &Value) -> String {
    answer["id"].as_str().expect("an id").to_owned()
}

#[tokio::test]
async fn a_makes_a_workspace_on_b_starts_a_session_and_a_shell_there_and_archives_it() {
    let fleet = Fleet::start(2);
    let (a, b) = (&fleet.daemons[0], &fleet.daemons[1]);
    let remotes = tempfile::tempdir().unwrap();
    let project = project(&fleet, remotes.path());
    let on_a = Served::of(a).await;
    let on_b = Served::of(b).await;
    b.peers.serve_calls(on_b.addr);
    hears(&a.peers, b.id).await;

    let made = json!({"project": project, "base": "main", "name": "far work", "machine": b.id});
    let (status, answer) = on_a.post("/api/workspaces", Some(made)).await;
    assert_eq!(status, 201, "{answer}");
    let workspace: WorkspaceId = WorkspaceId(id_of(&answer).parse().unwrap());
    let shown = |records: &[AnyRecord], find: &dyn Fn(&AnyRecord) -> bool| {
        records
            .iter()
            .any(|record| record.owner() == b.id && find(record))
    };
    let is_workspace = |r: &AnyRecord| matches!(r, AnyRecord::Workspace(w) if w.body.id == workspace && !w.deleted);
    assert!(
        shown(&on_a.state().await, &is_workspace),
        "A shows B's workspace"
    );
    assert!(
        a.held(|r| r.owner() == a.id && matches!(r, AnyRecord::Workspace(_)))
            .is_none(),
        "nothing was made on A"
    );

    let path = format!("/api/workspaces/{workspace}/sessions");
    let (status, answer) = on_a.post(&path, Some(json!({"model": "test-model"}))).await;
    assert_eq!(status, 201, "{answer}");
    let session = SessionId(id_of(&answer).parse().unwrap());
    let is_session = |r: &AnyRecord| matches!(r, AnyRecord::Session(s) if s.body.id == session);
    assert!(
        shown(&on_a.state().await, &is_session),
        "A shows B's session"
    );

    let path = format!("/api/workspaces/{workspace}/shells");
    let (status, answer) = on_a.post(&path, None).await;
    assert_eq!(status, 201, "{answer}");
    let shell = TerminalId(id_of(&answer).parse().unwrap());
    let is_shell = |r: &AnyRecord| matches!(r, AnyRecord::Terminal(t) if t.body.id == shell);
    assert!(shown(&on_a.state().await, &is_shell), "A shows B's shell");

    let (status, answer) = on_a
        .post(&format!("/api/terminals/{shell}/close"), None)
        .await;
    assert_eq!(status, 204, "{answer}");

    std::fs::write(on_b.worktree().join("notes.txt"), "not yet committed\n").unwrap();
    let path = format!("/api/workspaces/{workspace}/archive");
    let (status, answer) = on_a.post(&path, Some(json!({}))).await;
    assert_eq!(status, 409, "{answer}");
    let uncommitted = answer["uncommitted"].as_array().expect("the files listed");
    assert!(
        uncommitted.iter().any(|f| f.as_str() == Some("notes.txt")),
        "{answer}"
    );
    assert!(
        shown(&on_a.state().await, &is_workspace),
        "the refusal changed nothing"
    );

    let (status, answer) = on_a.post(&path, Some(json!({"force": true}))).await;
    assert_eq!(status, 204, "{answer}");
    assert!(
        !shown(&on_a.state().await, &is_workspace),
        "A shows B's workspace archived"
    );
}

#[tokio::test]
async fn a_call_for_b_once_the_link_to_b_is_cut_answers_503_naming_b_and_makes_nothing() {
    let fleet = Fleet::start(2);
    let (a, b) = (&fleet.daemons[0], &fleet.daemons[1]);
    let remotes = tempfile::tempdir().unwrap();
    let project = project(&fleet, remotes.path());
    let on_a = Served::of(a).await;
    let on_b = Served::of(b).await;
    b.peers.serve_calls(on_b.addr);
    hears(&a.peers, b.id).await;

    fleet.cut(1);
    let made = json!({"project": project, "base": "main", "name": "far work", "machine": b.id});
    let (status, answer) = on_a.post("/api/workspaces", Some(made)).await;
    assert_eq!(status, 503, "{answer}");
    let error = answer["error"].as_str().unwrap();
    assert!(error.contains("machine-1"), "{error}");
    assert!(
        b.held(|r| matches!(r, AnyRecord::Workspace(_))).is_none(),
        "nothing was made on B"
    );
}

#[tokio::test]
async fn a_call_to_a_machine_serving_no_calls_is_answered_503_by_it() {
    let fleet = Fleet::start(2);
    let (a, b) = (&fleet.daemons[0], &fleet.daemons[1]);
    hears(&a.peers, b.id).await;
    let path = format!("/api/terminals/{}/close", TerminalId::random());
    let answer = a.peers.call(b.id, &path, None, b"").await.unwrap();
    assert_eq!(answer.status, 503);
    let body = String::from_utf8_lossy(&answer.body);
    assert!(body.contains("serves no calls"), "{body}");
}

#[tokio::test]
async fn a_call_to_a_machine_not_listed_is_refused_before_any_link() {
    let fleet = Fleet::start(1);
    let unknown = MachineId::random();
    match fleet.daemons[0]
        .peers
        .call(unknown, "/api/workspaces", None, b"{}")
        .await
    {
        Err(CallError::NotListed(machine)) => assert_eq!(machine, unknown),
        other => panic!("not refused as unlisted: {other:?}"),
    }
}

#[tokio::test]
async fn a_call_to_a_listed_machine_never_heard_from_is_offline() {
    let silent = MachineId::random();
    let fleet = Fleet::start_with(1, &[silent]);
    match fleet.daemons[0]
        .peers
        .call(silent, "/api/workspaces", None, b"{}")
        .await
    {
        Err(CallError::Offline(name)) => assert_eq!(name, "machine-1"),
        other => panic!("not refused as offline: {other:?}"),
    }
}

/// Opens a raw link to `peers` with a call from `machine` for `path`, and reads its answer.
async fn raw_call(peers: &Peers, machine: MachineId, path: &str) -> Value {
    let (mut near, far) = Conn::pair();
    let peers = peers.clone();
    tokio::spawn(async move { peers.accept(far).await });
    let open = PeerMessage::Call {
        protocol: PROTOCOL,
        machine,
        call: foundry_proto::messages::Call {
            path: path.to_owned(),
            content_type: None,
            body: String::new(),
        },
    };
    near.send
        .send(serde_json::to_string(&open).unwrap())
        .await
        .unwrap();
    timeout(WAIT, async {
        loop {
            let text = near
                .recv
                .recv()
                .await
                .expect("an answer before the link closes");
            let frame: Value = serde_json::from_str(&text).unwrap();
            if frame["type"] == "answer" {
                return frame;
            }
        }
    })
    .await
    .expect("an answer within the wait")
}

#[tokio::test]
async fn a_call_from_a_machine_not_listed_or_the_owner_itself_is_answered_403() {
    let fleet = Fleet::start(1);
    let b = &fleet.daemons[0];
    let on_b = Served::of(b).await;
    b.peers.serve_calls(on_b.addr);
    let path = format!("/api/terminals/{}/close", TerminalId::random());
    for machine in [MachineId::random(), b.id] {
        let answer = raw_call(&b.peers, machine, &path).await;
        assert_eq!(answer["status"], 403, "{answer}");
    }
}

#[tokio::test]
async fn a_call_for_a_path_outside_the_carried_routes_is_answered_400_and_never_requested() {
    let peer = MachineId::random();
    let fleet = Fleet::start_with(1, &[peer]);
    let b = &fleet.daemons[0];
    let on_b = Served::of(b).await;
    b.peers.serve_calls(on_b.addr);
    for path in [
        "@example.com/api/workspaces",
        "/elsewhere",
        "",
        "/api/state",
        "/api/briefs",
    ] {
        let answer = raw_call(&b.peers, peer, path).await;
        assert_eq!(answer["status"], 400, "{path}: {answer}");
    }
}

#[tokio::test]
async fn a_call_through_keys_accept_reaches_the_owners_api_and_its_answer_returns() {
    let (a_dir, b_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (a_keys, b_keys) = (keys(&a_dir), keys(&b_dir));
    admit(&a_keys, &b_keys);
    admit(&b_keys, &a_keys);
    let b = Keyed::new(&[a_keys.me()], b_keys, b_dir, Arc::new(Nowhere));
    let on_b = Served::start(b.keys.me(), b.store.clone(), b.peers.clone()).await;
    b.peers.serve_calls(on_b.addr);
    let dialer = KeyedDialer::new(
        Arc::new(ServedBy {
            keys: b.keys.clone(),
            peers: b.peers.clone(),
        }),
        a_keys.clone(),
    );
    let a = Keyed::new(&[b.keys.me()], a_keys, a_dir, Arc::new(dialer));
    let running = a.peers.clone();
    let _run = Task(tokio::spawn(async move { running.run().await }));
    hears(&a.peers, b.keys.me()).await;

    let terminal = TerminalId::random();
    let path = format!("/api/terminals/{terminal}/close");
    let answer = a.peers.call(b.keys.me(), &path, None, b"").await.unwrap();
    assert_eq!(answer.status, 404);
    let body = String::from_utf8_lossy(&answer.body);
    assert!(body.contains(&terminal.to_string()), "{body}");
}

/// Counts the links it is asked to open, and opens none.
struct Counted(Arc<std::sync::atomic::AtomicUsize>);

impl foundryd::sync::Dialer for Counted {
    fn dial(
        &self,
        _: &foundry_proto::settings::Machine,
    ) -> futures::future::BoxFuture<'static, std::io::Result<Conn>> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async { Err(std::io::Error::other("no links in this test")) })
    }
}

#[tokio::test(start_paused = true)]
async fn a_call_to_a_peer_on_an_older_link_or_on_no_link_is_refused_before_any_link() {
    let (me, old) = (MachineId::random(), MachineId::random());
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path().join("foundry.db")).unwrap();
    assert!(store.apply_settings(settings_for(&[me, old])).unwrap());
    let dials = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let peers = Peers::new(
        me,
        Arc::new(Mutex::new(store)),
        Arc::new(Counted(dials.clone())),
    );
    let (near, far) = Conn::pair();
    let accepting = peers.clone();
    tokio::spawn(async move { accepting.accept(far).await });
    for message in [
        PeerMessage::Hello {
            protocol: 2,
            machine: old,
            versions: Default::default(),
            factory_versions: Default::default(),
        },
        PeerMessage::Records {
            protocol: 2,
            records: Vec::new(),
        },
    ] {
        near.send
            .send(serde_json::to_string(&message).unwrap())
            .await
            .unwrap();
    }
    until(WAIT, "the old peer is heard from", || {
        peers.online().contains(&old)
    })
    .await;
    match peers.call(old, "/api/workspaces", None, b"{}").await {
        Err(CallError::Older(name)) => assert_eq!(name, "machine-1"),
        other => panic!("not refused as older: {other:?}"),
    }

    // The link speaks protocol 2, so `older()` never lists it: wait on the link itself.
    assert!(peers.linked().contains(&old), "the old peer's link is open");
    drop(near);
    until(WAIT, "the old peer's link closes", || {
        !peers.linked().contains(&old)
    })
    .await;
    assert!(
        peers.online().contains(&old),
        "still heard from within the offline limit"
    );
    // With no record link open, nothing says the peer can take a call: none is sent.
    match peers.call(old, "/api/workspaces", None, b"{}").await {
        Err(CallError::Down(name)) => assert_eq!(name, "machine-1"),
        other => panic!("not refused as down: {other:?}"),
    }
    assert_eq!(
        dials.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "a link was dialed"
    );
}

/// A spawned task that is stopped when this is dropped.
struct Task(JoinHandle<()>);

impl Drop for Task {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[tokio::test]
async fn a_call_carried_to_b_for_a_third_machines_workspace_runs_only_on_b_and_is_never_carried_on()
{
    let (peer, third) = (MachineId::random(), MachineId::random());
    let fleet = Fleet::start_with(1, &[peer, third]);
    let b = &fleet.daemons[0];
    let on_b = Served::of(b).await;
    b.peers.serve_calls(on_b.addr);
    let workspace = WorkspaceId::random();
    let record = crate::workspace(workspace, false)(third, 1);
    lock(&b.store).apply(&record, third).unwrap();

    // Carried on, the call would reach the third machine's link and be answered 503.
    let answer = raw_call(
        &b.peers,
        peer,
        &format!("/api/workspaces/{workspace}/shells"),
    )
    .await;
    assert_eq!(answer["status"], 404, "{answer}");
}

#[tokio::test]
async fn a_call_whose_link_goes_down_after_it_was_sent_is_lost_not_refused() {
    let fleet = Fleet::start(2);
    let (a, b) = (&fleet.daemons[0], &fleet.daemons[1]);
    // An API that takes the connection and never answers, so the call is still running on B.
    let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    b.peers.serve_calls(silent.local_addr().unwrap());
    hears(&a.peers, b.id).await;

    let peers = a.peers.clone();
    let id = b.id;
    let call = tokio::spawn(async move {
        peers
            .call(id, "/api/workspaces", Some("application/json"), b"{}")
            .await
    });
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(!call.is_finished(), "the call answered before the cut");
    fleet.cut(1);
    match timeout(WAIT, call).await.unwrap().unwrap() {
        Err(CallError::Lost(name)) => assert_eq!(name, "machine-1"),
        other => panic!("not lost: {other:?}"),
    }
    drop(silent);
}

// --- placement across machines ---

/// One of a machine's own samples, with `sessions` and `load` per core.
fn loaded(sessions: u32, load: f64) -> impl FnOnce(MachineId, u64) -> AnyRecord {
    move |owner, version| {
        AnyRecord::MachineSample(foundry_proto::record::Record {
            owner,
            version,
            deleted: false,
            body: foundry_proto::record::MachineSample {
                load_per_core: load,
                free_memory: 1 << 30,
                free_disk: 1 << 35,
                sessions,
                update_refused: None,
                version: None,
                update_ready: None,
            },
        })
    }
}

/// Gives every machine of `fleet`, in every daemon's settings, at most 4 sessions and a load of
/// 1.0 per core.
fn limit_all(fleet: &Fleet) {
    for daemon in &fleet.daemons {
        let mut settings = daemon.settings();
        settings.seq += 1;
        for machine in &fleet.daemons {
            settings.limits.insert(
                machine.id,
                foundry_proto::settings::Limits {
                    max_sessions: 4,
                    max_load_per_core: 1.0,
                    min_free_disk: 1 << 30,
                },
            );
        }
        daemon.set_settings(settings);
    }
}

/// Waits until `daemon` holds `owner`'s sample at `version` or later.
async fn holds_sample(daemon: &Daemon, owner: MachineId, version: u64) {
    until(WAIT, "the sample arrives", || {
        daemon
            .held(|r| {
                r.owner() == owner
                    && r.version() >= version
                    && matches!(r, AnyRecord::MachineSample(_))
            })
            .is_some()
    })
    .await;
}

fn choose(project: ProjectId, name: &str) -> Value {
    json!({"project": project, "base": "main", "name": name, "machine": "choose", "model": "test-model"})
}

#[tokio::test]
async fn choose_on_a_makes_the_workspace_on_b_when_b_has_the_most_headroom() {
    let fleet = Fleet::start(2);
    let (a, b) = (&fleet.daemons[0], &fleet.daemons[1]);
    let remotes = tempfile::tempdir().unwrap();
    let project = project(&fleet, remotes.path());
    limit_all(&fleet);
    let on_a = Served::of(a).await;
    let on_b = Served::of(b).await;
    b.peers.serve_calls(on_b.addr);
    hears(&a.peers, b.id).await;
    a.write(loaded(0, 0.8));
    let sampled = b.write(loaded(2, 0.3));
    holds_sample(a, b.id, sampled.version()).await;

    let (status, answer) = on_a
        .post("/api/workspaces", Some(choose(project, "far")))
        .await;
    assert_eq!(status, 201, "{answer}");
    assert_eq!(answer["machine"], json!(b.id), "{answer}");
    let workspace = WorkspaceId(id_of(&answer).parse().unwrap());
    assert!(
        a.held(
            |r| r.owner() == b.id && matches!(r, AnyRecord::Workspace(w) if w.body.id == workspace)
        )
        .is_some(),
        "the workspace is B's"
    );
}

#[tokio::test]
async fn a_request_waiting_on_a_is_placed_on_b_with_its_session_once_b_qualifies() {
    let fleet = Fleet::start(2);
    let (a, b) = (&fleet.daemons[0], &fleet.daemons[1]);
    let remotes = tempfile::tempdir().unwrap();
    let project = project(&fleet, remotes.path());
    limit_all(&fleet);
    let on_a = Served::of(a).await;
    let on_b = Served::of(b).await;
    b.peers.serve_calls(on_b.addr);
    hears(&a.peers, b.id).await;
    a.write(loaded(4, 0.1));
    let sampled = b.write(loaded(0, 2.5));
    holds_sample(a, b.id, sampled.version()).await;

    let (status, answer) = on_a
        .post("/api/workspaces", Some(choose(project, "later")))
        .await;
    assert_eq!(status, 202, "{answer}");
    assert_eq!(
        answer["reasons"],
        json!([
            "machine-0: sessions 4 at limit 4",
            "machine-1: load 2.5 over limit 1.0"
        ])
    );

    b.write(loaded(0, 0.2));
    let took = until(Duration::from_secs(10), "placed on B", || {
        let Some(AnyRecord::Workspace(made)) = a.held(|r| {
            r.owner() == b.id && matches!(r, AnyRecord::Workspace(w) if w.body.name == "later")
        }) else {
            return false;
        };
        a.held(|r| {
            r.owner() == b.id
                && matches!(r, AnyRecord::Session(s)
                    if s.body.workspace == made.body.id && s.body.model == "test-model")
        })
        .is_some()
    })
    .await;
    assert!(took < Duration::from_secs(10), "{took:?}");
    assert!(
        a.held(|r| r.owner() == a.id && matches!(r, AnyRecord::Workspace(_)))
            .is_none(),
        "nothing was made on A"
    );
}

#[tokio::test]
async fn a_create_naming_b_over_its_limits_is_made_on_b_and_warns_of_each_limit() {
    let fleet = Fleet::start(2);
    let (a, b) = (&fleet.daemons[0], &fleet.daemons[1]);
    let remotes = tempfile::tempdir().unwrap();
    let project = project(&fleet, remotes.path());
    limit_all(&fleet);
    let on_a = Served::of(a).await;
    let on_b = Served::of(b).await;
    b.peers.serve_calls(on_b.addr);
    hears(&a.peers, b.id).await;
    let sampled = b.write(loaded(5, 1.2));
    holds_sample(a, b.id, sampled.version()).await;

    let named = json!({"project": project, "base": "main", "name": "anyway", "machine": b.id});
    let (status, answer) = on_a.post("/api/workspaces", Some(named)).await;
    assert_eq!(status, 201, "{answer}");
    assert_eq!(
        answer["warning"],
        "machine-1 is over its limits: sessions 5 over limit 4, load 1.2 over limit 1.0"
    );
    let workspace = WorkspaceId(id_of(&answer).parse().unwrap());
    assert!(
        a.held(
            |r| r.owner() == b.id && matches!(r, AnyRecord::Workspace(w) if w.body.id == workspace)
        )
        .is_some(),
        "the workspace is B's"
    );
}

/// Two machines that both qualify, B the roomier, whose API takes a call and never answers.
/// Returns A's API and B's silent listener, which must be kept for the test.
async fn b_never_answers(
    fleet: &Fleet,
    project_on: &Path,
) -> (Served, ProjectId, tokio::net::TcpListener) {
    let (a, b) = (&fleet.daemons[0], &fleet.daemons[1]);
    let project = project(fleet, project_on);
    limit_all(fleet);
    let on_a = Served::of(a).await;
    let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    b.peers.serve_calls(silent.local_addr().unwrap());
    hears(&a.peers, b.id).await;
    (on_a, project, silent)
}

#[tokio::test]
async fn a_choose_whose_create_is_lost_on_b_is_never_sent_to_a_as_well() {
    let fleet = Fleet::start(2);
    let (a, b) = (&fleet.daemons[0], &fleet.daemons[1]);
    let remotes = tempfile::tempdir().unwrap();
    let (on_a, project, silent) = b_never_answers(&fleet, remotes.path()).await;
    a.write(loaded(0, 0.8));
    let sampled = b.write(loaded(0, 0.1));
    holds_sample(a, b.id, sampled.version()).await;

    let posted = tokio::spawn(async move {
        let answer = on_a
            .post("/api/workspaces", Some(choose(project, "lost")))
            .await;
        (on_a, answer)
    });
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(!posted.is_finished(), "B answered");
    fleet.cut(1);
    let (_on_a, (status, answer)) = timeout(WAIT, posted).await.unwrap().unwrap();
    assert_eq!(status, 504, "{answer}");
    assert_eq!(answer["machine"], json!(b.id), "{answer}");
    assert!(
        a.held(|r| matches!(r, AnyRecord::Workspace(w) if w.body.name == "lost"))
            .is_none(),
        "the create was sent to A as well"
    );
    drop(silent);
}

#[tokio::test]
async fn a_queued_request_whose_create_is_lost_on_b_fails_and_is_never_made_on_a() {
    let fleet = Fleet::start(2);
    let (a, b) = (&fleet.daemons[0], &fleet.daemons[1]);
    let remotes = tempfile::tempdir().unwrap();
    let (on_a, project, silent) = b_never_answers(&fleet, remotes.path()).await;
    a.write(loaded(4, 0.1));
    let sampled = b.write(loaded(4, 0.1));
    holds_sample(a, b.id, sampled.version()).await;
    let (status, answer) = on_a
        .post("/api/workspaces", Some(choose(project, "lost")))
        .await;
    assert_eq!(status, 202, "{answer}");

    let sampled = b.write(loaded(0, 0.1));
    holds_sample(a, b.id, sampled.version()).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    fleet.cut(1);
    let started = tokio::time::Instant::now();
    let error = loop {
        let listed = on_a.get("/api/placements").await;
        if listed["requests"][0]["state"] == "failed" {
            break listed["requests"][0]["error"].as_str().unwrap().to_owned();
        }
        assert!(
            started.elapsed() < WAIT,
            "the request never fails: {listed}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(error.contains("may have been made on machine-1"), "{error}");

    a.write(loaded(0, 0.1));
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        a.held(|r| matches!(r, AnyRecord::Workspace(w) if w.body.name == "lost"))
            .is_none(),
        "the create was sent to A as well"
    );
    drop(silent);
}

#[tokio::test]
async fn a_request_being_placed_cannot_be_cancelled_and_ends_failed_when_its_create_is_lost() {
    let fleet = Fleet::start(2);
    let (a, b) = (&fleet.daemons[0], &fleet.daemons[1]);
    let remotes = tempfile::tempdir().unwrap();
    let (on_a, project, silent) = b_never_answers(&fleet, remotes.path()).await;
    a.write(loaded(4, 0.1));
    let sampled = b.write(loaded(4, 0.1));
    holds_sample(a, b.id, sampled.version()).await;
    let (status, answer) = on_a
        .post("/api/workspaces", Some(choose(project, "held")))
        .await;
    assert_eq!(status, 202, "{answer}");
    let request = answer["request"].as_str().unwrap().to_owned();

    let sampled = b.write(loaded(0, 0.1));
    holds_sample(a, b.id, sampled.version()).await;
    let started = tokio::time::Instant::now();
    while on_a.get("/api/placements").await["requests"][0]["state"] != "placing" {
        assert!(started.elapsed() < WAIT, "never placed");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let (status, answer) = on_a
        .post(&format!("/api/placements/{request}/cancel"), None)
        .await;
    assert_eq!(status, 409, "{answer}");

    fleet.cut(1);
    let started = tokio::time::Instant::now();
    loop {
        let listed = on_a.get("/api/placements").await;
        if listed["requests"][0]["state"] == "failed" {
            break;
        }
        assert!(started.elapsed() < WAIT, "never failed: {listed}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    drop(silent);
}

#[tokio::test]
async fn a_request_whose_create_gets_no_answer_from_b_does_not_hold_back_the_next_request() {
    let fleet = Fleet::start(2);
    let (a, b) = (&fleet.daemons[0], &fleet.daemons[1]);
    let remotes = tempfile::tempdir().unwrap();
    let (on_a, project, silent) = b_never_answers(&fleet, remotes.path()).await;
    a.write(loaded(4, 0.1));
    let sampled = b.write(loaded(4, 0.1));
    holds_sample(a, b.id, sampled.version()).await;
    for name in ["held", "next"] {
        let (status, answer) = on_a
            .post("/api/workspaces", Some(choose(project, name)))
            .await;
        assert_eq!(status, 202, "{answer}");
    }

    // B has room for one session. The first request takes it, and B never answers its create.
    let sampled = b.write(loaded(3, 0.1));
    holds_sample(a, b.id, sampled.version()).await;
    let started = tokio::time::Instant::now();
    while on_a.get("/api/placements").await["requests"][0]["state"] != "placing" {
        assert!(
            started.elapsed() < WAIT,
            "the first request is never placed"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // A qualifies while the first request is still being placed on B.
    a.write(loaded(0, 0.1));
    until(
        Duration::from_secs(10),
        "the next request is placed on A",
        || {
            let Some(AnyRecord::Workspace(made)) = a.held(|r| {
                r.owner() == a.id && matches!(r, AnyRecord::Workspace(w) if w.body.name == "next")
            }) else {
                return false;
            };
            a.held(|r| {
                r.owner() == a.id
                    && matches!(r, AnyRecord::Session(s) if s.body.workspace == made.body.id)
            })
            .is_some()
        },
    )
    .await;
    let listed = on_a.get("/api/placements").await;
    assert_eq!(listed["requests"][0]["name"], "held", "{listed}");
    assert_eq!(listed["requests"][0]["state"], "placing", "{listed}");
    drop(silent);
}

// --- the factory's worker routes ---

/// The login whose token a factory session on B acts with, and that token.
const LOGIN: &str = "octo";
const TOKEN: &str = "ghp_FactoryRouteToken0123456789abcdef";

/// A secret store in memory, holding `github:octo`.
#[derive(Default)]
struct Held(Mutex<std::collections::BTreeMap<String, String>>);

impl Held {
    fn with_token() -> Arc<dyn Secrets> {
        let held = Held::default();
        held.set(&format!("github:{LOGIN}"), TOKEN).unwrap();
        Arc::new(held)
    }
}

impl Secrets for Held {
    fn get(&self, name: &str) -> Result<Option<String>, SecretsError> {
        Ok(self.0.lock().unwrap().get(name).cloned())
    }

    fn set(&self, name: &str, value: &str) -> Result<(), SecretsError> {
        self.0
            .lock()
            .unwrap()
            .insert(name.to_owned(), value.to_owned());
        Ok(())
    }

    fn update(&self, name: &str, edit: Edit<'_>) -> Result<(), SecretsError> {
        let mut held = self.0.lock().unwrap();
        if let Some(value) = edit(held.get(name).cloned()) {
            held.insert(name.to_owned(), value);
        }
        Ok(())
    }

    fn names(&self) -> Result<Vec<String>, SecretsError> {
        Ok(self.0.lock().unwrap().keys().cloned().collect())
    }

    fn entries(&self) -> Result<std::collections::BTreeMap<String, String>, SecretsError> {
        Ok(self.0.lock().unwrap().clone())
    }
}

/// The factory session's script on B: the token on its terminal, then two turns, the first of
/// which quotes the token.
const SESSION_SCRIPT: &str = "- hook: {event: SessionStart}
- print_env: GH_TOKEN
- wait_prompt
- stop: {last_assistant_message: \"pushed with ghp_FactoryRouteToken0123456789abcdef\"}
- wait_prompt
- stop: {last_assistant_message: \"second turn\"}
- wait_input
";

/// A project's remote on two machines that serve their APIs, B with the token and the
/// factory session's script, A hearing B.
struct Pair {
    fleet: Fleet,
    _remotes: TempDir,
    project: ProjectId,
    on_a: Served,
    on_b: Served,
}

impl Pair {
    async fn start() -> Pair {
        Self::start_with(None, None).await
    }

    /// A pair whose B refuses sessions for `refused`, when it is given, and starts `holder` for
    /// its terminals, when it is given.
    async fn start_with(refused: Option<&'static str>, holder: Option<PathBuf>) -> Pair {
        let fleet = Fleet::start(2);
        let remotes = tempfile::tempdir().unwrap();
        let project = project(&fleet, remotes.path());
        let (a, b) = (&fleet.daemons[0], &fleet.daemons[1]);
        let on_a = Served::of(a).await;
        let factory = Factory {
            script: SESSION_SCRIPT,
            secrets: Held::with_token(),
            refused,
            holder,
        };
        let on_b = Served::start_with(b.id, b.store.clone(), b.peers.clone(), Some(factory)).await;
        b.peers.serve_calls(on_b.addr);
        hears(&a.peers, b.id).await;
        Pair {
            fleet,
            _remotes: remotes,
            project,
            on_a,
            on_b,
        }
    }

    fn a(&self) -> &Daemon {
        &self.fleet.daemons[0]
    }

    fn b(&self) -> &Daemon {
        &self.fleet.daemons[1]
    }

    /// The run's call for `path` with `body`, made on A: the status and the text answered, by
    /// B when B owns what the path names.
    async fn call(&self, path: &str, body: Option<Value>) -> (u16, String) {
        call_on(&self.on_a, path, body).await
    }

    /// Asks B for `path` with `body` until it answers other than `404`, and gives that answer.
    async fn once_found(&self, path: &str, body: Value) -> (u16, String) {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let (status, text) = self.call(path, Some(body.clone())).await;
            if status != 404 {
                return (status, text);
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "{path} still answers 404: {text}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn create(&self, name: &str, setup: Value) -> Value {
        json!({
            "machine": self.b().id,
            "project": self.project,
            "name": name,
            "checkout": {"kind": "new_branch", "base": "main", "branch": name},
            "setup": setup,
            "tag": {"project": self.project, "run": "run-12"},
        })
    }

    fn start_body(&self, login: &str, model: &str, effort: &str, line: &str) -> Value {
        json!({
            "project": self.project,
            "login": login,
            "model": model,
            "effort": effort,
            "message": {"line": line, "body": null},
        })
    }

    /// The factory sessions B holds for `workspace`.
    fn sessions_in(&self, workspace: WorkspaceId) -> usize {
        self.b()
            .all()
            .iter()
            .filter(|r| {
                r.owner() == self.b().id
                    && matches!(r, AnyRecord::Session(s) if s.body.workspace == workspace)
            })
            .count()
    }

    /// Whether A shows `session` as running.
    async fn runs(&self, session: SessionId) -> bool {
        self.on_a.state().await.iter().any(|r| {
            matches!(r, AnyRecord::Session(s) if s.body.id == session && !s.deleted
                && !matches!(s.body.state, SessionState::Finished | SessionState::Failed))
        })
    }
}

/// The run's call for `path` with `body`, made on `on`: the status and the text answered. A
/// call refused because the record it names has not reached `on` yet is made again, as a run
/// asks again on its next pass.
async fn call_on(on: &Served, path: &str, body: Option<Value>) -> (u16, String) {
    let body = body.map(|body| body.to_string()).unwrap_or_default();
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        match api::factory::call(&on.api, path, body.as_bytes()).await {
            Ok(answer) => {
                return (
                    answer.status,
                    String::from_utf8(answer.body.to_vec()).unwrap(),
                );
            }
            Err(CallError::Refused(said))
                if said.contains(NOT_HERE_YET) && tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => panic!("{path} not answered: {e}"),
        }
    }
}

/// What a run's call says when no record of what it names has reached its machine.
const NOT_HERE_YET: &str = "has not reached this machine";

/// The terminals of `workspace` that `on` shows.
async fn terminals_of(on: &Served, workspace: WorkspaceId) -> Vec<TerminalId> {
    on.state()
        .await
        .into_iter()
        .filter_map(|record| match record {
            AnyRecord::Terminal(t) if t.body.workspace == workspace && !t.deleted => {
                Some(t.body.id)
            }
            _ => None,
        })
        .collect()
}

fn workspace_of(text: &str) -> WorkspaceId {
    WorkspaceId(id_of(&serde_json::from_str(text).unwrap()).parse().unwrap())
}

fn session_of(text: &str) -> SessionId {
    SessionId(id_of(&serde_json::from_str(text).unwrap()).parse().unwrap())
}

#[test]
fn every_factory_route_called_on_a_for_what_b_owns_reaches_b_and_no_answer_holds_a_token() {
    isolated(
        "every_factory_route_called_on_a_for_what_b_owns_reaches_b_and_no_answer_holds_a_token",
        || async {
            let pair = Pair::start().await;
            let (a, b) = (pair.a(), pair.b());
            let mut answers: Vec<String> = Vec::new();

            // A factory create naming B is made on B, with the setup and branch it names.
            // A URL whose password is longer than a tail: cut to the tail's size before it is hidden, it
            // would show the end of its password with no `://` before it.
            let setup = "yes 0123456789abcdef | head -n 20000; \
                     echo fetching https://u:s3cretpass@example.com/r; \
                     printf 'https://u:'; head -c 80000 /dev/zero | tr '\\0' P; \
                     printf '@example.com/long\\n'; echo last-line";
            let setup = json!({"kind": "commands", "commands": [setup], "limit_secs": 60});
            let create = pair.create("fac/12-1", setup);
            let (status, text) = pair
                .call("/api/factory/workspaces", Some(create.clone()))
                .await;
            assert_eq!(status, 201, "{text}");
            let workspace = workspace_of(&text);
            answers.push(text);
            let made_on_b = b.held(|r| {
            matches!(r, AnyRecord::Workspace(w) if w.body.id == workspace && w.body.branch == "fac/12-1")
        });
            assert!(made_on_b.is_some(), "B made the workspace on its branch");
            assert!(
                a.held(|r| r.owner() == a.id && matches!(r, AnyRecord::Workspace(_)))
                    .is_none(),
                "nothing was made on A"
            );
            let (status, text) = pair.call("/api/factory/workspaces", Some(create)).await;
            assert_eq!(status, 201, "{text}");
            assert_eq!(
                workspace_of(&text),
                workspace,
                "the same tag is one workspace"
            );
            answers.push(text);

            // The setup terminal's tail, read from A: at most 64 KB, ending with the setup's last
            // line, with the URL's password hidden.
            let setup_terminal = terminals_of(&pair.on_a, workspace).await;
            let [setup_terminal] = setup_terminal[..] else {
                panic!("one setup terminal: {setup_terminal:?}");
            };
            let tail_path = format!("/api/terminals/{setup_terminal}/tail");
            let deadline = tokio::time::Instant::now() + WAIT;
            let tail = loop {
                let (status, text) = pair.call(&tail_path, None).await;
                assert_eq!(status, 200, "{text}");
                // The setup terminal says how the setup ended after its last command's output.
                if text.lines().rev().take(3).any(|line| line == "last-line") {
                    break text;
                }
                assert!(tokio::time::Instant::now() < deadline, "{text}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            };
            assert!(tail.len() <= 64 * 1024, "{} bytes", tail.len());
            assert!(tail.contains("0123456789abcdef\n"), "the setup's output");
            assert!(!tail.contains('\u{1b}'), "no escape is left");
            assert!(
                tail.contains("https://***@example.com/r"),
                "the URL is shown"
            );
            assert!(!tail.contains("s3cretpass"), "its password is not");
            assert!(
                tail.contains("https://***@example.com/long"),
                "the long URL is shown"
            );
            assert!(!tail.contains(&"P".repeat(64)), "its password is not");
            answers.push(tail);

            // A start the route refuses makes no session on B.
            let path = format!("/api/workspaces/{workspace}/factory/session");
            for (refused, why) in [
                (
                    pair.start_body(LOGIN, "-x", "high", "/go"),
                    "a model like an option",
                ),
                (
                    pair.start_body(LOGIN, "m", "two words", "/go"),
                    "an effort of two words",
                ),
                (
                    pair.start_body(LOGIN, "m", "high", "one\ntwo"),
                    "a line of two lines",
                ),
                (
                    pair.start_body("nobody", "m", "high", "/go"),
                    "a login B holds no token for",
                ),
            ] {
                let (status, text) = pair.call(&path, Some(refused)).await;
                assert_eq!(status, 422, "{why}: {text}");
                answers.push(text);
            }
            assert_eq!(pair.sessions_in(workspace), 0, "no session was made");

            // A factory session started from A runs on B, on the first message. The same start again
            // answers with it, and starts no second one.
            let start = pair.start_body(LOGIN, "test-model", "high", "/factory-build demo#12");
            let (status, text) = pair.call(&path, Some(start.clone())).await;
            assert_eq!(status, 201, "{text}");
            let session = session_of(&text);
            answers.push(text);
            let (status, text) = pair.call(&path, Some(start)).await;
            assert_eq!(status, 200, "{text}");
            assert_eq!(session_of(&text), session, "the running session");
            answers.push(text);
            assert_eq!(pair.sessions_in(workspace), 1, "one session runs on B");

            // Its first turn, read from A, with the token it quoted hidden.
            let turn = format!("/api/sessions/{session}/factory/turn");
            let (status, text) = pair.once_found(&turn, json!({"turn": 1})).await;
            assert_eq!(status, 200, "{text}");
            let first: Value = serde_json::from_str(&text).unwrap();
            assert!(
                first["message"]
                    .as_str()
                    .unwrap()
                    .starts_with("pushed with "),
                "{first}"
            );
            assert_eq!(first["question"], Value::Null, "{first}");
            answers.push(text);

            // A message sent from A is typed on B, and answers the count of prompts before it. A line
            // of two lines is refused and typed nowhere.
            let message = format!("/api/sessions/{session}/factory/message");
            let two_lines =
                json!({"message": {"line": "a\nb", "body": null}, "only_if_prompts": 1});
            let (status, text) = pair.call(&message, Some(two_lines)).await;
            assert_eq!(status, 422, "{text}");
            answers.push(text);
            let send = json!({"message": {"line": "carry on", "body": null}, "only_if_prompts": 1});
            let (status, text) = pair.call(&message, Some(send.clone())).await;
            assert_eq!(status, 200, "{text}");
            assert_eq!(
                serde_json::from_str::<Value>(&text).unwrap(),
                json!({"prompts": 1})
            );
            answers.push(text);
            let (status, text) = pair.once_found(&turn, json!({"turn": 2})).await;
            assert_eq!(status, 200, "{text}");
            let second: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(second["message"], "second turn", "{second}");
            answers.push(text);

            // The same send again finds the count risen, and is refused with the reason.
            let (status, text) = pair.call(&message, Some(send)).await;
            assert_eq!(status, 409, "{text}");
            let refused: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(refused["reason"], "count_rose", "{refused}");
            assert_eq!(refused["now"], 2, "{refused}");
            answers.push(text);

            // The session's own terminal, whose child holds the token and printed it, has no tail.
            let session_terminal = terminals_of(&pair.on_a, workspace)
                .await
                .into_iter()
                .find(|t| *t != setup_terminal)
                .expect("the session's terminal");
            let (status, text) = pair
                .call(&format!("/api/terminals/{session_terminal}/tail"), None)
                .await;
            assert_eq!(status, 404, "{text}");
            answers.push(text);

            // Ending the workspace's factory sessions from A ends the one on B.
            let (status, text) = pair
                .call(&format!("/api/workspaces/{workspace}/factory/end"), None)
                .await;
            assert_eq!(status, 204, "{text}");
            answers.push(text);
            let deadline = tokio::time::Instant::now() + WAIT;
            while pair.runs(session).await {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the session still runs"
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
            }

            for text in &answers {
                assert!(!text.contains(TOKEN), "an answer holds the token: {text}");
            }
        },
    );
}

#[test]
fn a_worker_route_without_the_carried_mark_or_with_a_browsers_headers_is_404_on_every_machine() {
    isolated(
        "a_worker_route_without_the_carried_mark_or_with_a_browsers_headers_is_404_on_every_machine",
        || async {
            let pair = Pair::start().await;
            let b = pair.b();
            let setup = json!({"kind": "commands", "commands": ["echo ready"], "limit_secs": 60});
            let (status, text) = pair
                .call(
                    "/api/factory/workspaces",
                    Some(pair.create("fac/12-1", setup)),
                )
                .await;
            assert_eq!(status, 201, "{text}");
            let workspace = workspace_of(&text);
            let start = pair.start_body(LOGIN, "test-model", "high", "/factory-build demo#12");
            let path = format!("/api/workspaces/{workspace}/factory/session");
            let (status, text) = pair.call(&path, Some(start.clone())).await;
            assert_eq!(status, 201, "{text}");
            let session = session_of(&text);
            let turn = format!("/api/sessions/{session}/factory/turn");
            let (status, text) = pair.once_found(&turn, json!({"turn": 1})).await;
            assert_eq!(status, 200, "{text}");
            let terminal = terminals_of(&pair.on_a, workspace).await[0];

            let routes = [
                (
                    "/api/factory/workspaces".to_owned(),
                    Some(
                        json!({"machine": b.id, "project": pair.project, "name": "x",
                    "checkout": {"kind": "new_branch", "base": "main", "branch": null},
                    "setup": {"kind": "none"}, "tag": null}),
                    ),
                ),
                (path.clone(), Some(start)),
                (format!("/api/workspaces/{workspace}/factory/end"), None),
                (
                    format!("/api/sessions/{session}/factory/message"),
                    Some(json!({"message": {"line": "hi", "body": null}, "only_if_prompts": null})),
                ),
                (turn.clone(), Some(json!({"turn": 1}))),
                (format!("/api/terminals/{terminal}/tail"), None),
            ];
            let origin = format!("http://{}", pair.on_b.addr);
            let browser_marks: [&[(&'static str, String)]; 2] = [
                &[(CARRIED, "1".into()), ("origin", origin)],
                &[(CARRIED, "1".into()), ("sec-fetch-mode", "cors".into())],
            ];
            for (path, body) in routes {
                // Called with no mark on A, which does not own it, and on B, which does.
                for (on, machine) in [(&pair.on_a, "A"), (&pair.on_b, "B")] {
                    let (status, text) = on.post_text(&path, body.clone()).await;
                    assert_eq!(status, 404, "{path} on {machine}: {text}");
                    assert_eq!(text, r#"{"error":"no such route"}"#, "{path} on {machine}");
                }
                // Marked, from a browser on B.
                for headers in browser_marks {
                    let (status, text) = pair.on_b.post_with(&path, body.clone(), headers).await;
                    assert_eq!(status, 404, "{path} with {headers:?}: {text}");
                }
            }
            for on in [&pair.on_a, &pair.on_b] {
                let (status, _) = on.post_text(&turn, Some(json!("not a turn"))).await;
                assert_eq!(status, 404, "a body the route cannot take is still 404");
            }
            assert!(
                b.held(|r| matches!(r, AnyRecord::Workspace(w) if w.body.name == "x"))
                    .is_none(),
                "no uncarried create made anything"
            );
            assert_eq!(pair.sessions_in(workspace), 1, "no second session");
            assert!(pair.runs(session).await, "no uncarried end ended it");
        },
    );
}

#[test]
fn a_start_answers_with_the_factory_session_running_in_its_own_workspace_only() {
    isolated(
        "a_start_answers_with_the_factory_session_running_in_its_own_workspace_only",
        || async {
            let pair = Pair::start().await;
            let none = json!({"kind": "none"});
            let mut workspaces = Vec::new();
            for name in ["fac/12-1", "fac/13-1", "fac/14-1"] {
                let (status, text) = pair
                    .call(
                        "/api/factory/workspaces",
                        Some(pair.create(name, none.clone())),
                    )
                    .await;
                assert_eq!(status, 201, "{text}");
                workspaces.push(workspace_of(&text));
            }
            let [first, second, with_p1] = workspaces[..] else {
                unreachable!()
            };
            let start = pair.start_body(LOGIN, "test-model", "high", "/factory-build demo#12");
            let start_in =
                |workspace: WorkspaceId| format!("/api/workspaces/{workspace}/factory/session");

            // A session running in one workspace is not another workspace's.
            let (status, text) = pair.call(&start_in(first), Some(start.clone())).await;
            assert_eq!(status, 201, "{text}");
            let in_first = session_of(&text);
            let (status, text) = pair.call(&start_in(second), Some(start.clone())).await;
            assert_eq!(status, 201, "{text}");
            assert_ne!(session_of(&text), in_first, "a session of its own");
            assert_eq!(pair.sessions_in(first), 1);
            assert_eq!(pair.sessions_in(second), 1);

            // A P1 session running in a workspace is no factory session.
            let p1 = format!("/api/workspaces/{with_p1}/sessions");
            let (status, text) = pair.on_b.post_text(&p1, Some(json!({"model": "m"}))).await;
            assert_eq!(status, 201, "{text}");
            let p1_session = session_of(&text);
            let (status, text) = pair.call(&start_in(with_p1), Some(start)).await;
            assert_eq!(status, 201, "{text}");
            assert_ne!(
                session_of(&text),
                p1_session,
                "a factory session of its own"
            );
            assert_eq!(pair.sessions_in(with_p1), 2);
        },
    );
}

#[test]
fn a_run_calls_what_its_own_machine_owns_here_and_names_only_worker_routes() {
    isolated(
        "a_run_calls_what_its_own_machine_owns_here_and_names_only_worker_routes",
        || async {
            let pair = Pair::start().await;
            let setup = json!({"kind": "commands", "commands": ["echo ready"], "limit_secs": 60});
            // Made from B for B: served on B, with no peer call.
            let (status, text) = call_on(
                &pair.on_b,
                "/api/factory/workspaces",
                Some(pair.create("fac/12-1", setup)),
            )
            .await;
            assert_eq!(status, 201, "{text}");
            let workspace = workspace_of(&text);
            let start = pair.start_body(LOGIN, "test-model", "high", "/factory-build demo#12");
            let path = format!("/api/workspaces/{workspace}/factory/session");
            let (status, text) = call_on(&pair.on_b, &path, Some(start)).await;
            assert_eq!(status, 201, "{text}");
            let session = session_of(&text);
            let turn = format!("/api/sessions/{session}/factory/turn");
            let deadline = tokio::time::Instant::now() + WAIT;
            loop {
                let (status, text) = call_on(&pair.on_b, &turn, Some(json!({"turn": 1}))).await;
                if status == 200 {
                    let first: Value = serde_json::from_str(&text).unwrap();
                    assert!(!text.contains(TOKEN), "{text}");
                    assert!(
                        first["message"]
                            .as_str()
                            .unwrap()
                            .starts_with("pushed with ")
                    );
                    break;
                }
                assert_eq!(status, 404, "{text}");
                assert!(tokio::time::Instant::now() < deadline, "{text}");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            // The session's terminal has no tail here either, and a bad body is refused here as there.
            let terminal = terminals_of(&pair.on_b, workspace)
                .await
                .into_iter()
                .find(|t| {
                    pair.b()
                        .held(|r| matches!(r, AnyRecord::Session(s) if s.body.terminal == *t))
                        .is_some()
                })
                .expect("the session's terminal");
            let (status, text) =
                call_on(&pair.on_b, &format!("/api/terminals/{terminal}/tail"), None).await;
            assert_eq!(status, 404, "{text}");
            let (status, text) = call_on(&pair.on_b, &turn, Some(json!("not a turn"))).await;
            assert_eq!(status, 422, "{text}");
            let (status, text) = call_on(&pair.on_b, &turn, None).await;
            assert_eq!(status, 400, "no body is no JSON: {text}");

            for path in ["/api/state", "/api/workspaces", "/api/briefs"] {
                match api::factory::call(&pair.on_a.api, path, b"{}").await {
                    Err(CallError::Refused(said)) => {
                        assert!(said.contains("not a worker route"), "{said}")
                    }
                    other => panic!("{path} not refused: {other:?}"),
                }
            }
        },
    );
}

#[test]
fn a_start_on_a_machine_that_refuses_sessions_is_503_and_starts_nothing() {
    isolated(
        "a_start_on_a_machine_that_refuses_sessions_is_503_and_starts_nothing",
        || async {
            let pair = Pair::start_with(Some("this machine has no gateway"), None).await;
            let (status, text) = pair
                .call(
                    "/api/factory/workspaces",
                    Some(pair.create("fac/12-1", json!({"kind": "none"}))),
                )
                .await;
            assert_eq!(status, 201, "{text}");
            let workspace = workspace_of(&text);
            let start = pair.start_body(LOGIN, "test-model", "high", "/factory-build demo#12");
            let (status, text) = pair
                .call(
                    &format!("/api/workspaces/{workspace}/factory/session"),
                    Some(start),
                )
                .await;
            assert_eq!(status, 503, "{text}");
            assert!(text.contains("no gateway"), "{text}");
            assert_eq!(pair.sessions_in(workspace), 0);
        },
    );
}

#[test]
fn a_run_call_naming_what_no_record_here_names_is_refused_and_answered_by_no_machine() {
    isolated(
        "a_run_call_naming_what_no_record_here_names_is_refused_and_answered_by_no_machine",
        || async {
            let pair = Pair::start().await;
            let (workspace, session, terminal) = (
                WorkspaceId::random(),
                SessionId::random(),
                TerminalId::random(),
            );
            let start = pair.start_body(LOGIN, "test-model", "high", "/factory-build demo#12");
            let send = json!({"message": {"line": "hi", "body": null}, "only_if_prompts": null});
            for (path, body) in [
                (
                    format!("/api/workspaces/{workspace}/factory/session"),
                    Some(start),
                ),
                (format!("/api/workspaces/{workspace}/factory/end"), None),
                (
                    format!("/api/sessions/{session}/factory/message"),
                    Some(send),
                ),
                (
                    format!("/api/sessions/{session}/factory/turn"),
                    Some(json!({"turn": 1})),
                ),
                (format!("/api/terminals/{terminal}/tail"), None),
            ] {
                // Answered here, a message would be `409 no_such_session` and the rest `404`: final
                // answers for something that may only not have synced yet.
                let body = body.map(|body| body.to_string()).unwrap_or_default();
                match api::factory::call(&pair.on_a.api, &path, body.as_bytes()).await {
                    Err(CallError::Refused(said)) => {
                        assert!(said.contains(NOT_HERE_YET), "{path}: {said}");
                    }
                    other => panic!("{path} not refused: {other:?}"),
                }
            }
        },
    );
}

#[test]
fn only_a_workspaces_setup_terminal_has_a_tail() {
    isolated("only_a_workspaces_setup_terminal_has_a_tail", || async {
        let pair = Pair::start().await;
        let setup = json!({"kind": "commands", "commands": ["echo ready"], "limit_secs": 60});
        let (status, text) = pair
            .call(
                "/api/factory/workspaces",
                Some(pair.create("fac/12-1", setup)),
            )
            .await;
        assert_eq!(status, 201, "{text}");
        let workspace = workspace_of(&text);
        let Some(AnyRecord::Workspace(record)) = pair
            .b()
            .held(|r| matches!(r, AnyRecord::Workspace(w) if w.body.id == workspace))
        else {
            panic!("B holds the workspace");
        };
        let setup_terminal = record.body.setup.expect("the setup terminal is named");
        let shells = format!("/api/workspaces/{workspace}/shells");
        let (status, text) = pair.on_b.post_text(&shells, None).await;
        assert_eq!(status, 201, "{text}");
        let shell = TerminalId(
            id_of(&serde_json::from_str(&text).unwrap())
                .parse()
                .unwrap(),
        );

        let (status, text) = pair
            .call(&format!("/api/terminals/{setup_terminal}/tail"), None)
            .await;
        assert_eq!(status, 200, "{text}");
        // A shell's child may hold anything its user typed: it is no setup, and has no tail.
        let (status, text) = pair
            .call(&format!("/api/terminals/{shell}/tail"), None)
            .await;
        assert_eq!(status, 404, "{text}");
    });
}

#[test]
fn a_carried_create_or_end_on_a_machine_that_does_not_own_its_target_changes_nothing() {
    isolated(
        "a_carried_create_or_end_on_a_machine_that_does_not_own_its_target_changes_nothing",
        || async {
            let pair = Pair::start().await;
            let (a, b) = (pair.a(), pair.b());
            let (status, text) = pair
                .call(
                    "/api/factory/workspaces",
                    Some(pair.create("fac/12-1", json!({"kind": "none"}))),
                )
                .await;
            assert_eq!(status, 201, "{text}");
            let workspace = workspace_of(&text);
            let start = pair.start_body(LOGIN, "test-model", "high", "/factory-build demo#12");
            let path = format!("/api/workspaces/{workspace}/factory/session");
            let (status, text) = pair.call(&path, Some(start)).await;
            assert_eq!(status, 201, "{text}");
            let session = session_of(&text);
            let deadline = tokio::time::Instant::now() + WAIT;
            while !pair.runs(session).await {
                assert!(tokio::time::Instant::now() < deadline, "A never shows it");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            let carried = [(CARRIED, "1".to_owned())];

            // A create naming A that reaches B is refused there, and made on neither.
            let mut elsewhere = pair.create("fac/13-1", json!({"kind": "none"}));
            elsewhere["machine"] = json!(a.id);
            let (status, text) = pair
                .on_b
                .post_with("/api/factory/workspaces", Some(elsewhere), &carried)
                .await;
            assert_eq!(status, 421, "{text}");
            for daemon in [a, b] {
                assert!(
                    daemon
                        .held(|r| matches!(r, AnyRecord::Workspace(w) if w.body.name == "fac/13-1"))
                        .is_none(),
                    "nothing was made"
                );
            }

            // An end that reaches A, which does not own the workspace, ends nothing.
            let end = format!("/api/workspaces/{workspace}/factory/end");
            let (status, text) = pair.on_a.post_with(&end, None, &carried).await;
            assert_eq!(status, 404, "{text}");
            assert!(pair.runs(session).await, "B's session still runs");
        },
    );
}

#[test]
fn a_start_after_the_workspaces_sessions_were_ended_starts_a_new_session() {
    isolated(
        "a_start_after_the_workspaces_sessions_were_ended_starts_a_new_session",
        || async {
            let pair = Pair::start().await;
            let (status, text) = pair
                .call(
                    "/api/factory/workspaces",
                    Some(pair.create("fac/12-1", json!({"kind": "none"}))),
                )
                .await;
            assert_eq!(status, 201, "{text}");
            let workspace = workspace_of(&text);
            let start = pair.start_body(LOGIN, "test-model", "high", "/factory-build demo#12");
            let path = format!("/api/workspaces/{workspace}/factory/session");
            let (status, text) = pair.call(&path, Some(start.clone())).await;
            assert_eq!(status, 201, "{text}");
            let first = session_of(&text);
            let (status, text) = pair
                .once_found(
                    &format!("/api/sessions/{first}/factory/turn"),
                    json!({"turn": 1}),
                )
                .await;
            assert_eq!(status, 200, "{text}");

            let (status, text) = pair
                .call(&format!("/api/workspaces/{workspace}/factory/end"), None)
                .await;
            assert_eq!(status, 204, "{text}");
            let deadline = tokio::time::Instant::now() + WAIT;
            while pair
                .b()
                .held(|r| {
                    matches!(r, AnyRecord::Session(s) if s.body.id == first
                    && matches!(s.body.state, SessionState::Finished | SessionState::Failed))
                })
                .is_none()
            {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the session still runs"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            // A turn the ended session never wrote never will be: not the 404 of a turn still to come.
            let (status, text) = pair
                .call(
                    &format!("/api/sessions/{first}/factory/turn"),
                    Some(json!({"turn": 5})),
                )
                .await;
            assert_eq!(status, 410, "{text}");

            // The ended session is not the running one, and the end does not refuse the next start.
            let (status, text) = pair.call(&path, Some(start)).await;
            assert_eq!(status, 201, "{text}");
            let second = session_of(&text);
            assert_ne!(second, first, "a new session");
            assert_eq!(pair.sessions_in(workspace), 2);
            let (status, text) = pair
                .once_found(
                    &format!("/api/sessions/{second}/factory/turn"),
                    json!({"turn": 1}),
                )
                .await;
            assert_eq!(status, 200, "{text}");
            let send = json!({"message": {"line": "carry on", "body": null}, "only_if_prompts": 1});
            let (status, text) = pair
                .call(
                    &format!("/api/sessions/{second}/factory/message"),
                    Some(send),
                )
                .await;
            assert_eq!(status, 200, "{text}");
            assert_eq!(
                serde_json::from_str::<Value>(&text).unwrap(),
                json!({"prompts": 1})
            );
        },
    );
}

#[test]
fn a_create_whose_setup_cannot_start_is_201_with_the_workspace_and_its_error_hidden() {
    isolated(
        "a_create_whose_setup_cannot_start_is_201_with_the_workspace_and_its_error_hidden",
        || async {
            // No holder at a path that reads as a URL with a password: the start's error may name it.
            let holder = PathBuf::from("/no/such/https://u:s3cret@example.com/foundry-holder");
            let pair = Pair::start_with(None, Some(holder)).await;
            let setup = json!({"kind": "commands", "commands": ["echo ready"], "limit_secs": 60});
            let create = pair.create("fac/12-1", setup);
            let (status, text) = pair
                .call("/api/factory/workspaces", Some(create.clone()))
                .await;
            assert_eq!(status, 201, "{text}");
            let workspace = workspace_of(&text);
            let answer: Value = serde_json::from_str(&text).unwrap();
            let error = answer["error"].as_str().expect("the error beside the id");
            assert!(error.contains("could not be started"), "{error}");
            assert!(!text.contains("s3cret"), "{text}");
            assert!(
            pair.b()
                .held(|r| matches!(r, AnyRecord::Workspace(w) if w.body.id == workspace && !w.deleted))
                .is_some(),
            "the workspace is kept"
        );

            let (status, text) = pair.call("/api/factory/workspaces", Some(create)).await;
            assert_eq!(status, 201, "{text}");
            assert_eq!(
                workspace_of(&text),
                workspace,
                "a retry answers the same one"
            );
        },
    );
}

#[test]
fn two_starts_sent_at_once_start_one_session_and_both_answer_with_it() {
    isolated(
        "two_starts_sent_at_once_start_one_session_and_both_answer_with_it",
        || async {
            let pair = Pair::start().await;
            let (status, text) = pair
                .call(
                    "/api/factory/workspaces",
                    Some(pair.create("fac/12-1", json!({"kind": "none"}))),
                )
                .await;
            assert_eq!(status, 201, "{text}");
            let workspace = workspace_of(&text);
            let start = pair.start_body(LOGIN, "test-model", "high", "/factory-build demo#12");
            let path = format!("/api/workspaces/{workspace}/factory/session");

            // As a start sent again after a 504 can reach B while the first is still starting there.
            let ((one, one_text), (two, two_text)) = tokio::join!(
                pair.call(&path, Some(start.clone())),
                pair.call(&path, Some(start)),
            );
            let mut statuses = [one, two];
            statuses.sort_unstable();
            assert_eq!(statuses, [200, 201], "{one_text} {two_text}");
            assert_eq!(session_of(&one_text), session_of(&two_text), "one session");
            assert_eq!(pair.sessions_in(workspace), 1, "one session runs on B");
        },
    );
}

/// A turn or a message that reaches A, carried, for B's running factory session is not answered
/// as if the session ran on A: the session is B's, so A's turn is `410` and its message `409
/// no_such_session`, and B's session still runs.
#[test]
fn a_carried_turn_or_message_for_a_session_another_machine_runs_is_not_answered_as_running_here() {
    isolated(
        "a_carried_turn_or_message_for_a_session_another_machine_runs_is_not_answered_as_running_here",
        || async {
            let pair = Pair::start().await;
            let (status, text) = pair
                .call(
                    "/api/factory/workspaces",
                    Some(pair.create("fac/12-1", json!({"kind": "none"}))),
                )
                .await;
            assert_eq!(status, 201, "{text}");
            let workspace = workspace_of(&text);
            let start = pair.start_body(LOGIN, "test-model", "high", "/factory-build demo#12");
            let path = format!("/api/workspaces/{workspace}/factory/session");
            let (status, text) = pair.call(&path, Some(start)).await;
            assert_eq!(status, 201, "{text}");
            let session = session_of(&text);
            let deadline = tokio::time::Instant::now() + WAIT;
            while !pair.runs(session).await {
                assert!(tokio::time::Instant::now() < deadline, "A never shows it");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            let carried = [(CARRIED, "1".to_owned())];

            let turn = format!("/api/sessions/{session}/factory/turn");
            let (status, text) = pair
                .on_a
                .post_with(&turn, Some(json!({"turn": 7})), &carried)
                .await;
            assert_eq!(status, 410, "{text}");
            let message = format!("/api/sessions/{session}/factory/message");
            let send = json!({"message": {"line": "hi", "body": null}, "only_if_prompts": null});
            let (status, text) = pair.on_a.post_with(&message, Some(send), &carried).await;
            assert_eq!(status, 409, "{text}");
            assert!(text.contains("no_such_session"), "{text}");
            assert!(pair.runs(session).await, "B's session still runs");
        },
    );
}

/// A deleted session record names no owner, as it did when only live sessions were read: a run's
/// message or turn call for it is refused as not here yet and sent nowhere, whether B or A wrote
/// it. A turn carried to A for A's own deleted session is `410`, as for a session that ended.
#[test]
fn a_run_call_for_a_deleted_session_is_refused_as_not_here_yet_and_answered_by_no_machine() {
    isolated(
        "a_run_call_for_a_deleted_session_is_refused_as_not_here_yet_and_answered_by_no_machine",
        || async {
            let pair = Pair::start().await;
            let (a, b) = (pair.a(), pair.b());
            for owner in [b.id, a.id] {
                let session = SessionId::random();
                let deleted = AnyRecord::Session(foundry_proto::record::Record {
                    owner,
                    version: 1 << 40,
                    deleted: true,
                    body: foundry_proto::record::Session {
                        id: session,
                        workspace: WorkspaceId::random(),
                        terminal: TerminalId::random(),
                        model: "test-model".into(),
                        state: SessionState::Working,
                    },
                });
                // As sync applies B's records on A, and as A keeps its own.
                a.store.lock().unwrap().apply(&deleted, owner).unwrap();
                let send =
                    json!({"message": {"line": "hi", "body": null}, "only_if_prompts": null});
                for (path, body) in [
                    (format!("/api/sessions/{session}/factory/message"), send),
                    (
                        format!("/api/sessions/{session}/factory/turn"),
                        json!({"turn": 1}),
                    ),
                ] {
                    let body = body.to_string();
                    match api::factory::call(&pair.on_a.api, &path, body.as_bytes()).await {
                        Err(CallError::Refused(said)) => {
                            assert!(said.contains(NOT_HERE_YET), "{path}: {said}");
                        }
                        other => {
                            panic!("{path} of {owner}'s deleted session not refused: {other:?}")
                        }
                    }
                }
                if owner == a.id {
                    // Carried to A, which owns it: a deleted session runs nowhere, so its turn is gone.
                    let turn = format!("/api/sessions/{session}/factory/turn");
                    let carried = [(CARRIED, "1".to_owned())];
                    let (status, text) = pair
                        .on_a
                        .post_with(&turn, Some(json!({"turn": 1})), &carried)
                        .await;
                    assert_eq!(status, 410, "{text}");
                }
            }
        },
    );
}
