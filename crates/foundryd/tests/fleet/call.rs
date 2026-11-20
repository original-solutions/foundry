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
use foundryd::api::{self, Api};
use foundryd::join::KeyedDialer;
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

impl Served {
    async fn start(me: MachineId, store: Arc<Mutex<Store>>, peers: Peers) -> Self {
        // A Unix socket path is at most about 100 bytes, which the system's own temporary
        // folder on macOS would take most of.
        let data = tempfile::Builder::new()
            .prefix("fc")
            .tempdir_in("/tmp")
            .unwrap();
        let script = data.path().join("script.yaml");
        std::fs::write(&script, "- wait_input\n").unwrap();
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
        let sessions = Arc::new(Sessions::new(
            me,
            store.clone(),
            terminals.clone(),
            data.path(),
            sessions::Config {
                claude: vec![
                    built("foundry-fake-agent").to_string_lossy().into_owned(),
                    "--script".into(),
                    script.to_string_lossy().into_owned(),
                ],
                claude_json: home.path().join(".claude.json"),
                hook_port: hooks.local_addr().unwrap().port(),
                gateway: Gateway {
                    base_url: "http://127.0.0.1:9/gateway".into(),
                    key: "unused".into(),
                },
            },
        ));
        let hook_router = sessions.router();
        let api = Api::new(store, peers, terminals, workspaces, sessions, Vec::new());
        let listener = api::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let servers = vec![
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
            (status, serde_json::from_str(&text).unwrap_or(Value::Null))
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
        let project = std::fs::read_dir(projects).unwrap().next().unwrap().unwrap();
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
        records.iter().any(|record| record.owner() == b.id && find(record))
    };
    let is_workspace =
        |r: &AnyRecord| matches!(r, AnyRecord::Workspace(w) if w.body.id == workspace && !w.deleted);
    assert!(shown(&on_a.state().await, &is_workspace), "A shows B's workspace");
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
    assert!(shown(&on_a.state().await, &is_session), "A shows B's session");

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
    assert!(shown(&on_a.state().await, &is_workspace), "the refusal changed nothing");

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
    match fleet.daemons[0].peers.call(unknown, "/api/workspaces", None, b"{}").await {
        Err(CallError::NotListed(machine)) => assert_eq!(machine, unknown),
        other => panic!("not refused as unlisted: {other:?}"),
    }
}

#[tokio::test]
async fn a_call_to_a_listed_machine_never_heard_from_is_offline() {
    let silent = MachineId::random();
    let fleet = Fleet::start_with(1, &[silent]);
    match fleet.daemons[0].peers.call(silent, "/api/workspaces", None, b"{}").await {
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
            let text = near.recv.recv().await.expect("an answer before the link closes");
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
async fn a_call_for_a_path_outside_the_api_is_answered_400_and_never_requested() {
    let peer = MachineId::random();
    let fleet = Fleet::start_with(1, &[peer]);
    let b = &fleet.daemons[0];
    let on_b = Served::of(b).await;
    b.peers.serve_calls(on_b.addr);
    for path in ["@example.com/api/workspaces", "/elsewhere", ""] {
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

#[tokio::test(start_paused = true)]
async fn a_call_to_a_peer_whose_link_speaks_an_older_protocol_is_refused_before_any_link() {
    let old = MachineId::random();
    let fleet = Fleet::start_with(1, &[old]);
    let a = &fleet.daemons[0];
    let (mut near, far) = Conn::pair();
    let peers = a.peers.clone();
    tokio::spawn(async move { peers.accept(far).await });
    let hello = PeerMessage::Hello {
        protocol: 2,
        machine: old,
        versions: Default::default(),
    };
    near.send
        .send(serde_json::to_string(&hello).unwrap())
        .await
        .unwrap();
    let empty = PeerMessage::Records {
        protocol: 2,
        records: Vec::new(),
    };
    near.send
        .send(serde_json::to_string(&empty).unwrap())
        .await
        .unwrap();
    until(WAIT, "the old peer is heard from", || {
        a.peers.online().contains(&old)
    })
    .await;
    match a.peers.call(old, "/api/workspaces", None, b"{}").await {
        Err(CallError::Older(name)) => assert_eq!(name, "machine-1"),
        other => panic!("not refused as older: {other:?}"),
    }
    drop(near);
}

/// A spawned task that is stopped when this is dropped.
struct Task(JoinHandle<()>);

impl Drop for Task {
    fn drop(&mut self) {
        self.0.abort();
    }
}
