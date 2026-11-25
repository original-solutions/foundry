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
use foundryd::sync::call::CallError;
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
        let terminals = Arc::new(Terminals::new(
            me,
            store.clone(),
            data.path(),
            built("foundry-holder"),
        ));
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
        let sessions = Arc::new(match factory {
            Some(factory) => sessions.with_secrets(factory.secrets),
            None => sessions,
        });
        let hook_router = sessions.router();
        let api = Api::new(store, peers, terminals, workspaces, sessions, Vec::new());
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
        let url = format!("http://{}{path}", self.addr);
        tokio::task::spawn_blocking(move || {
            let agent: ureq::Agent = ureq::Agent::config_builder()
                .http_status_as_error(false)
                .timeout_global(Some(WAIT))
                .build()
                .into();
            let request = agent.post(&url);
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

/// The factory session's script on B: two turns, the first of which quotes the token.
const SESSION_SCRIPT: &str = "- wait_prompt
- stop: {last_assistant_message: \"pushed with ghp_FactoryRouteToken0123456789abcdef\"}
- wait_prompt
- stop: {last_assistant_message: \"second turn\"}
- wait_input
";

/// Asks `on` for `path` with `body` until it answers other than `404`, and gives that answer.
async fn once_found(on: &Served, path: &str, body: Value) -> (u16, String) {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let (status, text) = on.post_text(path, Some(body.clone())).await;
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

#[tokio::test]
async fn every_factory_route_called_on_a_for_what_b_owns_reaches_b_and_no_answer_holds_a_token() {
    let fleet = Fleet::start(2);
    let (a, b) = (&fleet.daemons[0], &fleet.daemons[1]);
    let remotes = tempfile::tempdir().unwrap();
    let project = project(&fleet, remotes.path());
    let on_a = Served::of(a).await;
    let factory = Factory {
        script: SESSION_SCRIPT,
        secrets: Held::with_token(),
    };
    let on_b = Served::start_with(b.id, b.store.clone(), b.peers.clone(), Some(factory)).await;
    b.peers.serve_calls(on_b.addr);
    hears(&a.peers, b.id).await;
    let mut answers: Vec<String> = Vec::new();

    // A factory create naming B is made on B, with the setup and branch it names.
    let setup = "yes 0123456789abcdef | head -n 20000; echo last-line";
    let create = json!({
        "machine": b.id,
        "project": project,
        "name": "fac/12-1",
        "checkout": {"kind": "new_branch", "base": "main", "branch": "fac/12-1"},
        "setup": {"kind": "commands", "commands": [setup], "limit_secs": 60},
        "tag": {"project": project, "run": "run-12"},
    });
    let (status, text) = on_a
        .post_text("/api/factory/workspaces", Some(create.clone()))
        .await;
    assert_eq!(status, 201, "{text}");
    let answer: Value = serde_json::from_str(&text).unwrap();
    let workspace = WorkspaceId(id_of(&answer).parse().unwrap());
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
    let (status, text) = on_a
        .post_text("/api/factory/workspaces", Some(create))
        .await;
    assert_eq!(status, 201, "{text}");
    let again: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        id_of(&again),
        workspace.to_string(),
        "the same tag is one workspace"
    );
    answers.push(text);

    // The setup terminal's tail, read from A: at most 64 KB, ending with its last line.
    let setup_terminal = terminals_of(&on_a, workspace).await;
    let [setup_terminal] = setup_terminal[..] else {
        panic!("one setup terminal: {setup_terminal:?}");
    };
    let tail_path = format!("/api/terminals/{setup_terminal}/tail");
    let deadline = tokio::time::Instant::now() + WAIT;
    let tail = loop {
        let (status, text) = on_a.post_text(&tail_path, None).await;
        assert_eq!(status, 200, "{text}");
        if text.trim_end().ends_with("last-line") {
            break text;
        }
        assert!(tokio::time::Instant::now() < deadline, "{text}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(tail.len() <= 64 * 1024, "{} bytes", tail.len());
    assert!(tail.contains("0123456789abcdef\n"), "the setup's output");
    assert!(!tail.contains('\u{1b}'), "no escape is left");
    answers.push(tail);

    // A factory session started from A runs on B, on the first message.
    let start = json!({
        "project": project,
        "login": LOGIN,
        "model": "test-model",
        "effort": "high",
        "message": {"line": "/factory-build demo#12", "body": null},
    });
    let path = format!("/api/workspaces/{workspace}/factory/session");
    let (status, text) = on_a.post_text(&path, Some(start)).await;
    assert_eq!(status, 201, "{text}");
    let session = SessionId(
        id_of(&serde_json::from_str(&text).unwrap())
            .parse()
            .unwrap(),
    );
    answers.push(text);
    assert!(
        b.held(|r| r.owner() == b.id && matches!(r, AnyRecord::Session(s) if s.body.id == session))
            .is_some(),
        "the session runs on B"
    );

    // Its first turn, read from A, with the token it quoted hidden.
    let turn = format!("/api/sessions/{session}/factory/turn");
    let (status, text) = once_found(&on_a, &turn, json!({"turn": 1})).await;
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

    // A message sent from A is typed on B, and answers the count of prompts before it.
    let message = format!("/api/sessions/{session}/factory/message");
    let send = json!({"message": {"line": "carry on", "body": null}, "only_if_prompts": 1});
    let (status, text) = on_a.post_text(&message, Some(send.clone())).await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(
        serde_json::from_str::<Value>(&text).unwrap(),
        json!({"prompts": 1})
    );
    answers.push(text);
    let (status, text) = once_found(&on_a, &turn, json!({"turn": 2})).await;
    assert_eq!(status, 200, "{text}");
    let second: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(second["message"], "second turn", "{second}");
    answers.push(text);

    // The same send again finds the count risen, and is refused with the reason.
    let (status, text) = on_a.post_text(&message, Some(send)).await;
    assert_eq!(status, 409, "{text}");
    let refused: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(refused["reason"], "count_rose", "{refused}");
    assert_eq!(refused["now"], 2, "{refused}");
    answers.push(text);

    // The session's own terminal reads from A too.
    let session_terminal = terminals_of(&on_a, workspace)
        .await
        .into_iter()
        .find(|t| *t != setup_terminal)
        .expect("the session's terminal");
    let (status, text) = on_a
        .post_text(&format!("/api/terminals/{session_terminal}/tail"), None)
        .await;
    assert_eq!(status, 200, "{text}");
    answers.push(text);

    // Called on B with no carried mark, each route is 404, and changes nothing.
    let direct = [
        (
            "/api/factory/workspaces".to_owned(),
            Some(json!({"machine": b.id, "project": project, "name": "x",
                "checkout": {"kind": "new_branch", "base": "main", "branch": null},
                "setup": {"kind": "none"}, "tag": null})),
        ),
        (
            format!("/api/workspaces/{workspace}/factory/session"),
            Some(
                json!({"project": project, "login": LOGIN, "model": "m", "effort": "high",
                "message": {"line": "hi", "body": null}}),
            ),
        ),
        (format!("/api/workspaces/{workspace}/factory/end"), None),
        (
            message.clone(),
            Some(json!({"message": {"line": "hi", "body": null}, "only_if_prompts": null})),
        ),
        (turn.clone(), Some(json!({"turn": 1}))),
        (tail_path.clone(), None),
    ];
    for (path, body) in direct {
        let (status, text) = on_b.post_text(&path, body).await;
        assert_eq!(status, 404, "{path}: {text}");
        answers.push(text);
    }
    let (status, _) = on_b.post_text(&turn, Some(json!("not a turn"))).await;
    assert_eq!(status, 404, "a body the route cannot take is still 404");
    assert!(
        b.held(|r| matches!(r, AnyRecord::Workspace(w) if w.body.name == "x"))
            .is_none(),
        "the uncarried create made nothing"
    );
    let live = |records: &[AnyRecord]| {
        records.iter().any(|r| {
            matches!(r, AnyRecord::Session(s) if s.body.id == session && !s.deleted
                && !matches!(s.body.state, SessionState::Finished | SessionState::Failed))
        })
    };
    assert!(live(&on_a.state().await), "the uncarried end ended nothing");

    // Ending the workspace's factory sessions from A ends the one on B.
    let (status, text) = on_a
        .post_text(&format!("/api/workspaces/{workspace}/factory/end"), None)
        .await;
    assert_eq!(status, 204, "{text}");
    answers.push(text);
    let deadline = tokio::time::Instant::now() + WAIT;
    while live(&on_a.state().await) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the session still runs"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    for text in &answers {
        assert!(!text.contains(TOKEN), "an answer holds the token: {text}");
    }
}
