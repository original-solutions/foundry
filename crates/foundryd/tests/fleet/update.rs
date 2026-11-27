//! Fleet tests of a machine's install carried to it: machine A's install route sends a call
//! naming machine C to C over a call link, and C's answer comes back as C gave it.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use foundryd::api::{self, Api};
use foundryd::sessions::{self, Gateway, Sessions};
use foundryd::sync::call::Stop;
use foundryd::terminals::Terminals;
use foundryd::workspaces::Workspaces;
use serde_json::Value;
use tempfile::TempDir;
use tokio::task::JoinHandle;

use crate::*;

const WAIT: Duration = Duration::from_secs(20);

/// The version every daemon in the fleet runs as, as `foundryd --version` would say it.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// A machine's install route on a loopback port, over its store and peers, with a data
/// directory of its own whose `current` names the running version, and a stop that counts.
struct Served {
    addr: SocketAddr,
    home: TempDir,
    stops: Arc<AtomicUsize>,
    server: JoinHandle<()>,
}

impl Drop for Served {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl Served {
    /// Serves `daemon`'s install route, and carries the calls its peers send it there.
    async fn of(daemon: &Daemon) -> Self {
        let home = tempfile::tempdir().unwrap();
        let ready = home.path().join("versions/999.0.0");
        std::fs::create_dir_all(&ready).unwrap();
        for name in ["foundryd", "foundry-holder"] {
            std::fs::write(ready.join(name), "new").unwrap();
        }
        std::fs::create_dir_all(home.path().join("versions").join(VERSION)).unwrap();
        std::os::unix::fs::symlink(
            Path::new("versions").join(VERSION),
            home.path().join("current"),
        )
        .unwrap();
        std::fs::write(home.path().join("update.json"), r#"{"ready":"v999.0.0"}"#).unwrap();

        let me = daemon.id;
        let store = daemon.store.clone();
        let data = home.path().join("data");
        let terminals = Arc::new(Terminals::new(
            me,
            store.clone(),
            &data,
            data.join("foundry-holder"),
        ));
        let workspaces = Arc::new(Workspaces::new(me, store.clone(), &data, terminals.clone()));
        let sessions = Arc::new(Sessions::new(
            me,
            store.clone(),
            terminals.clone(),
            &data,
            sessions::Config {
                claude: vec!["false".into()],
                claude_json: data.join(".claude.json"),
                hook_port: 9,
                gateway: Gateway {
                    base_url: "http://127.0.0.1:9/gateway".into(),
                    key: "unused".into(),
                },
            },
        ));
        let api = Api::new(
            store,
            daemon.peers.clone(),
            terminals,
            workspaces,
            sessions,
            Vec::new(),
        );
        let stops = Arc::new(AtomicUsize::new(0));
        let counting = stops.clone();
        let stop = Stop::new(move || {
            counting.fetch_add(1, Ordering::SeqCst);
        });
        let app = api::machine_update::router(api, home.path().to_owned(), stop);
        let listener = api::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        daemon.peers.serve_calls(addr);
        Self {
            addr,
            home,
            stops,
            server,
        }
    }

    /// `(status, answer)` of an install of `tag` on machine `id`, asked of this machine.
    async fn install(&self, id: MachineId, tag: &str) -> (u16, Value) {
        let url = format!("http://{}/api/machines/{id}/update", self.addr);
        let body = serde_json::json!({ "tag": tag }).to_string();
        tokio::task::spawn_blocking(move || {
            let agent: ureq::Agent = ureq::Agent::config_builder()
                .http_status_as_error(false)
                .timeout_global(Some(WAIT))
                .build()
                .into();
            let mut answer = agent
                .post(url)
                .header("content-type", "application/json")
                .send(body.as_bytes())
                .unwrap();
            let text = answer.body_mut().read_to_string().unwrap();
            (
                answer.status().as_u16(),
                serde_json::from_str(&text).unwrap_or(Value::Null),
            )
        })
        .await
        .unwrap()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_install_is_carried_to_the_machine_it_names_and_answered_as_that_machine_answers() {
    let fleet = Fleet::start(3);
    let mut served = Vec::new();
    for daemon in &fleet.daemons {
        served.push(Served::of(daemon).await);
    }
    let (first, third) = (fleet.daemons[0].id, fleet.daemons[2].id);
    until(WAIT, "the first machine links to the third", || {
        fleet.daemons[0].peers.linked().contains(&third)
            && fleet.daemons[0].peers.online().contains(&third)
    })
    .await;

    // The fleet's daemons do not run from `current`, so the third refuses the install, naming
    // its own data directory: the answer is the third's.
    let (status, answer) = served[0].install(third, "v999.0.0").await;
    assert_eq!(status, 409, "{answer}");
    assert_eq!(answer["ready"], Value::Null, "{answer}");
    let error = answer["error"].as_str().unwrap();
    let theirs = served[2].home.path().join("current");
    assert!(error.contains(&theirs.display().to_string()), "{error}");

    // One naming the machine that serves it runs there.
    let (status, answer) = served[0].install(first, "v999.0.0").await;
    assert_eq!(status, 409, "{answer}");
    let ours = served[0].home.path().join("current");
    let error = answer["error"].as_str().unwrap();
    assert!(error.contains(&ours.display().to_string()), "{error}");

    let (status, answer) = served[0].install(MachineId::random(), "v999.0.0").await;
    assert_eq!(status, 404, "{answer}");
    assert!(answer["error"].is_string(), "{answer}");

    for (i, served) in served.iter().enumerate() {
        assert_eq!(served.stops.load(Ordering::SeqCst), 0, "machine {i} stopped");
        assert_eq!(
            std::fs::read_link(served.home.path().join("current")).unwrap(),
            Path::new("versions").join(VERSION),
            "machine {i} moved current"
        );
    }
}
