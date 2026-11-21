//! Adding a machine by its Tailscale name, from the UI: `POST /api/machines/add` `{name}`.
//!
//! The name is trimmed. One that is blank or over [`NAME_AT_MOST`] characters is `422`, and so
//! is one that resolves to no Tailscale address, by the rule `foundryd join` sends a code by
//! ([`join_targets`]). A name a listed machine has, compared trimmed and in any case, is `409`.
//! A machine the list does not hold yet has no address to put in the command, so it answers `503`.
//! Otherwise it answers `201` `{code, command, expires_in_secs}`: a new join code,
//! `foundryd join <this machine's address> <code>` to run on the new machine, and how long the
//! code works ([`CODE_VALID_FOR`]).
//!
//! The name and the first Tailscale address it resolved to are held in memory until the code is
//! used or expires. When a machine uses the code on the join route, it is listed under that
//! name at that address through the acting coordinator, as every machine list change is. An
//! edit that fails is warned, and the join stands. A code made anywhere else lists nothing.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use foundry_proto::ids::MachineId;
use foundry_proto::settings::{Edit, Machine};
use serde::Deserialize;
use tokio::sync::oneshot;

use super::machine_names::NAME_AT_MOST;
use super::{ApiError, Body, on_store, unprocessable};
use crate::coordinator::Coordinator;
use crate::join::{CODE_VALID_FOR, DEFAULT_PORT, JoinError, Keys, join_targets};
use crate::store::Store;

/// What the add route acts on: the keys that make codes, the coordinator that lists the
/// machine and the store whose machine list it checks.
struct Adding {
    keys: Keys,
    coordinator: Arc<Coordinator>,
    store: Arc<Mutex<Store>>,
}

/// `POST /api/machines/add`. The caller guards it as it guards the rest of the API, and serves
/// [`crate::join::router`] beside it on the same keys.
pub fn router(keys: Keys, coordinator: Arc<Coordinator>, store: Arc<Mutex<Store>>) -> Router {
    Router::new()
        .route("/api/machines/add", post(add))
        .with_state(Arc::new(Adding {
            keys,
            coordinator,
            store,
        }))
}

#[derive(Deserialize)]
struct Add {
    name: String,
}

async fn add(
    State(adding): State<Arc<Adding>>,
    Body(add): Body<Add>,
) -> Result<Response, ApiError> {
    let name = add.name.trim().to_owned();
    if name.is_empty() {
        return Err(unprocessable(
            "type the new machine's Tailscale name".to_owned(),
        ));
    }
    if name.chars().count() > NAME_AT_MOST {
        return Err(unprocessable(format!(
            "a machine's name is at most {NAME_AT_MOST} characters"
        )));
    }
    let settings = on_store(&adding.store, |store| store.settings()).await?;
    let wanted = name.to_lowercase();
    if let Some(other) = settings
        .machines
        .iter()
        .find(|m| m.name.trim().to_lowercase() == wanted)
    {
        return Err(ApiError::Rejected(
            StatusCode::CONFLICT,
            format!("a machine named {:?} is already in the fleet", other.name),
        ));
    }
    let me = adding.keys.me();
    let Some(own) = settings.machines.iter().find(|m| m.id == me) else {
        return Err(ApiError::Rejected(
            StatusCode::SERVICE_UNAVAILABLE,
            "this machine is not in its machine list yet; try again in a minute".to_owned(),
        ));
    };
    let address = resolve(&name).await?;
    let (code, used) = adding
        .keys
        .new_watched_code()
        .map_err(|e| ApiError::Rejected(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    tokio::spawn(list_when_used(
        adding.coordinator.clone(),
        adding.store.clone(),
        used,
        name,
        address,
    ));
    let answer = serde_json::json!({
        "command": format!("foundryd join {} {code}", own.address.trim()),
        "code": code.to_string(),
        "expires_in_secs": CODE_VALID_FOR.as_secs(),
    });
    Ok((StatusCode::CREATED, axum::Json(answer)).into_response())
}

/// The first Tailscale address `name` resolves to, as `foundryd join` would send a code to it.
/// Resolving it can wait on DNS, so it runs on the blocking pool.
async fn resolve(name: &str) -> Result<String, ApiError> {
    let lookup = name.to_owned();
    let resolved = tokio::task::spawn_blocking(move || join_targets(&lookup, DEFAULT_PORT))
        .await
        .map_err(|e| ApiError::Rejected(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    match resolved {
        Ok(targets) => Ok(targets[0].ip().to_string()),
        Err(JoinError::NotTailscale(_)) => Err(unprocessable(format!(
            "{name:?} is not on Tailscale: it resolves to no Tailscale address"
        ))),
        Err(_) => Err(unprocessable(format!(
            "no machine named {name:?} was found; type the name Tailscale shows for it"
        ))),
    }
}

/// Lists the machine that uses the code, told by `used`, as `name` at `address`, or nothing
/// when the code expires unused or the machine is already in the machine list (this machine
/// included), whose entry is never overwritten. The code is never logged.
async fn list_when_used(
    coordinator: Arc<Coordinator>,
    store: Arc<Mutex<Store>>,
    used: oneshot::Receiver<MachineId>,
    name: String,
    address: String,
) {
    // A redeem that starts just before the code expires can wait for the key lock.
    let wait = CODE_VALID_FOR + crate::secrets::LOCK_WAIT + Duration::from_secs(5);
    let Ok(Ok(id)) = tokio::time::timeout(wait, used).await else {
        return;
    };
    let held = on_store(&store, |store| store.settings()).await;
    if held.is_ok_and(|held| held.machines.iter().any(|m| m.id == id)) {
        tracing::warn!(
            "machine {id} used a code to join but is already in the machine list; its entry is left as it is"
        );
        return;
    }
    let machine = Machine { id, name, address };
    if let Err(error) = coordinator.edit(Edit::SetMachine { machine }).await {
        tracing::warn!("machine {id} joined, but it could not be listed: {error}");
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io;
    use std::net::IpAddr;
    use std::time::Duration;

    use foundry_proto::ids::MachineId;
    use foundry_proto::messages::{PROTOCOL, PeerMessage};
    use foundry_proto::settings::{Machine, SharedSettings};
    use futures::future::BoxFuture;
    use serde_json::{Value, json};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::*;
    use crate::join::{self, CODE_VALID_FOR};
    use crate::secrets::MemorySecrets;
    use crate::sync::{Conn, Dialer, Peers};

    struct NoDial;

    impl Dialer for NoDial {
        fn dial(&self, _: &Machine) -> BoxFuture<'static, io::Result<Conn>> {
            Box::pin(async { Err(io::Error::other("no peers in these tests")) })
        }
    }

    /// One machine's keys, coordinator and store, its join and add routes served on a
    /// loopback port.
    struct Served {
        _data: tempfile::TempDir,
        keys: Keys,
        store: Arc<Mutex<Store>>,
        peers: Peers,
        addr: std::net::SocketAddr,
        _server: tokio::task::JoinHandle<()>,
    }

    impl Served {
        /// A machine listed as `personal-mac` that acts alone, holding the settings
        /// `settings` makes from that.
        async fn new(settings: impl FnOnce(MachineId, &mut SharedSettings)) -> Self {
            let data = tempfile::tempdir().unwrap();
            let keys = Keys::open(Arc::new(MemorySecrets::default())).unwrap();
            let me = keys.me();
            let mut store = Store::open(data.path().join("foundry.db")).unwrap();
            let mut held = store.settings().unwrap();
            held.seq += 1;
            held.machines
                .push(machine(me, "personal-mac", "100.91.54.18"));
            held.priority = vec![me];
            settings(me, &mut held);
            store.apply_settings(held).unwrap();
            let store = Arc::new(Mutex::new(store));
            let peers = Peers::new(me, store.clone(), Arc::new(NoDial));
            let coordinator = Arc::new(Coordinator::new(me, store.clone(), peers.clone()));
            peers.coordinate(coordinator.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let app =
                join::router(keys.clone()).merge(router(keys.clone(), coordinator, store.clone()));
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            Self {
                _data: data,
                keys,
                store,
                peers,
                addr,
                _server: server,
            }
        }

        fn settings(&self) -> SharedSettings {
            self.store.lock().unwrap().settings().unwrap()
        }

        /// `(status, body)` of a post of `body` to `path`.
        async fn post(&self, path: &str, body: &Value) -> (u16, String) {
            let mut stream = tokio::net::TcpStream::connect(self.addr).await.unwrap();
            let body = body.to_string();
            let request = format!(
                "POST {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\
                 Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                self.addr,
                body.len()
            );
            stream.write_all(request.as_bytes()).await.unwrap();
            let mut response = Vec::new();
            tokio::time::timeout(Duration::from_secs(20), stream.read_to_end(&mut response))
                .await
                .expect("the route answers in time")
                .unwrap();
            let response = String::from_utf8(response).unwrap();
            let (head, body) = response.split_once("\r\n\r\n").unwrap();
            (
                head.split(' ').nth(1).unwrap().parse().unwrap(),
                body.to_owned(),
            )
        }

        /// `(status, body)` of an add of `name`.
        async fn add(&self, name: &str) -> (u16, String) {
            self.post("/api/machines/add", &json!({ "name": name }))
                .await
        }

        /// The code of an add of `name` that must succeed.
        async fn code_for(&self, name: &str) -> String {
            let (status, body) = self.add(name).await;
            assert_eq!(status, 201, "{body}");
            let answer: Value = serde_json::from_str(&body).unwrap();
            answer["code"].as_str().unwrap().to_owned()
        }

        /// `(status, body)` of a join with `code` by `joining`, as `foundryd join` posts it.
        async fn join(&self, code: &str, joining: &Keys) -> (u16, String) {
            let request = json!({
                "code": code,
                "machine": joining.me(),
                "key": joining.public_key(),
            });
            self.post(join::JOIN_PATH, &request).await
        }

        /// Waits for the machine list to hold `id`.
        async fn listed(&self, id: MachineId) -> Machine {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            loop {
                if let Some(found) = self.settings().machines.into_iter().find(|m| m.id == id) {
                    return found;
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "{id} was not listed"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }

        /// `peer` says hello and sends nothing, so it is heard from, then its link drops: it
        /// acts, and an edit has nowhere to go.
        async fn hear(&self, peer: MachineId) {
            self.hear_sending(peer, None).await;
        }

        /// As [`Served::hear`], and `peer` also sends `settings` when there are some, so this
        /// machine has caught up with it and numbers an edit at once.
        async fn hear_sending(&self, peer: MachineId, settings: Option<SharedSettings>) {
            let (ours, theirs) = Conn::pair();
            let accepting = tokio::spawn({
                let peers = self.peers.clone();
                async move { peers.accept(ours).await }
            });
            let hello = PeerMessage::Hello {
                protocol: PROTOCOL,
                machine: peer,
                versions: BTreeMap::new(),
            };
            let nothing = PeerMessage::Records {
                protocol: PROTOCOL,
                records: Vec::new(),
            };
            let settings = settings.map(|settings| PeerMessage::Settings {
                protocol: PROTOCOL,
                settings,
            });
            for message in [hello].into_iter().chain(settings).chain([nothing]) {
                theirs
                    .send
                    .send(serde_json::to_string(&message).unwrap())
                    .await
                    .unwrap();
            }
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            while !self.peers.online().contains(&peer) {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the peer was not heard"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            drop(theirs);
            accepting.await.unwrap();
        }
    }

    fn machine(id: MachineId, name: &str, address: &str) -> Machine {
        Machine {
            id,
            name: name.into(),
            address: address.into(),
        }
    }

    fn error_of(body: &str) -> String {
        let json: Value = serde_json::from_str(body).unwrap();
        json["error"].as_str().unwrap().to_owned()
    }

    /// Everything logged while it is set, as plain text.
    #[derive(Clone, Default)]
    struct Logged(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Logged {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Logged {
        /// Captures this thread's log until the guard drops.
        fn capture(&self) -> tracing::subscriber::DefaultGuard {
            let logged = self.clone();
            let subscriber = tracing_subscriber::fmt()
                .with_writer(move || logged.clone())
                .with_ansi(false)
                .without_time()
                .finish();
            tracing::subscriber::set_default(subscriber)
        }

        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }

    #[tokio::test]
    async fn an_add_answers_the_code_the_command_to_run_there_and_ten_minutes() {
        let served = Served::new(|_, _| {}).await;

        let (status, body) = served.add("  127.0.0.1 ").await;

        assert_eq!(status, 201, "{body}");
        let answer: Value = serde_json::from_str(&body).unwrap();
        let code = answer["code"].as_str().unwrap();
        assert_eq!(code.len(), 10, "{body}");
        assert_eq!(
            answer["command"],
            format!("foundryd join 100.91.54.18 {code}"),
            "{body}"
        );
        assert_eq!(answer["expires_in_secs"], CODE_VALID_FOR.as_secs());
        // An add changes nothing in the list until the code is used.
        assert_eq!(served.settings().machines.len(), 1);
    }

    #[tokio::test]
    async fn a_used_code_lists_the_machine_under_the_name_typed_at_its_address() {
        let served = Served::new(|_, _| {}).await;
        let joining = Keys::open(Arc::new(MemorySecrets::default())).unwrap();
        let code = served.code_for(" 127.0.0.1 ").await;

        let (status, body) = served.join(&code, &joining).await;

        assert_eq!(status, 200, "{body}");
        assert_eq!(
            served.listed(joining.me()).await,
            machine(joining.me(), "127.0.0.1", "127.0.0.1")
        );
    }

    #[tokio::test]
    async fn a_name_is_listed_as_typed_at_the_first_tailscale_address_it_resolves_to() {
        let served = Served::new(|_, _| {}).await;
        let joining = Keys::open(Arc::new(MemorySecrets::default())).unwrap();
        let code = served.code_for("localhost").await;

        assert_eq!(served.join(&code, &joining).await.0, 200);

        let listed = served.listed(joining.me()).await;
        assert_eq!(listed.name, "localhost");
        let first = join::join_targets("localhost", join::DEFAULT_PORT).unwrap()[0].ip();
        assert_eq!(listed.address.parse::<IpAddr>().unwrap(), first);
    }

    #[tokio::test]
    async fn a_join_with_a_code_this_route_did_not_issue_changes_nothing_in_the_settings() {
        let served = Served::new(|_, _| {}).await;
        served.code_for("127.0.0.1").await;
        let joining = Keys::open(Arc::new(MemorySecrets::default())).unwrap();
        let other = served.keys.new_code().unwrap().to_string();
        let before = served.settings();

        assert_eq!(served.join(&other, &joining).await.0, 200);

        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(served.settings(), before);
    }

    #[tokio::test]
    async fn a_code_redeemed_by_a_machine_already_listed_leaves_its_entry_alone() {
        let joining = Keys::open(Arc::new(MemorySecrets::default())).unwrap();
        let listed = joining.me();
        let served = Served::new(move |_, settings| {
            settings
                .machines
                .push(machine(listed, "trilogy-mac", "100.64.0.2"));
        })
        .await;
        // Caught up with it, so this machine numbers an edit at once.
        served.hear_sending(listed, Some(served.settings())).await;
        let code = served.code_for("127.0.0.1").await;
        let before = served.settings();

        served.join(&code, &joining).await;

        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(served.settings(), before);
    }

    #[tokio::test]
    async fn a_code_redeemed_by_this_machines_own_keys_leaves_its_entry_alone() {
        let served = Served::new(|_, _| {}).await;
        let code = served.code_for("127.0.0.1").await;
        let before = served.settings();

        // Whatever the join route answers, nothing is listed.
        served.join(&code, &served.keys).await;

        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(served.settings(), before);
    }

    #[tokio::test]
    async fn a_join_sent_again_after_its_answer_was_lost_lists_the_machine_once() {
        let served = Served::new(|_, _| {}).await;
        let joining = Keys::open(Arc::new(MemorySecrets::default())).unwrap();
        let code = served.code_for("127.0.0.1").await;

        assert_eq!(served.join(&code, &joining).await.0, 200);
        served.listed(joining.me()).await;
        let seq = served.settings().seq;
        assert_eq!(served.join(&code, &joining).await.0, 200);

        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(served.settings().seq, seq);
    }

    #[tokio::test]
    async fn a_listing_the_coordinator_cannot_send_is_warned_and_the_join_still_succeeds() {
        let logged = Logged::default();
        let _capture = logged.capture();
        let peer = MachineId::random();
        let served = Served::new(move |me, settings| {
            settings
                .machines
                .push(machine(peer, "trilogy-mac", "100.64.0.2"));
            settings.priority = vec![peer, me];
        })
        .await;
        served.hear(peer).await;
        let joining = Keys::open(Arc::new(MemorySecrets::default())).unwrap();
        let code = served.code_for("127.0.0.1").await;

        let (status, body) = served.join(&code, &joining).await;

        assert_eq!(status, 200, "{body}");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !logged.text().contains("WARN") {
            assert!(tokio::time::Instant::now() < deadline, "nothing was warned");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let text = logged.text();
        assert!(text.contains(&joining.me().to_string()), "{text}");
        assert!(!text.contains(&code), "the code reached the log: {text}");
        assert!(
            !served
                .settings()
                .machines
                .iter()
                .any(|m| m.id == joining.me())
        );
    }

    #[tokio::test]
    async fn a_blank_or_long_name_is_422_and_issues_no_code() {
        let served = Served::new(|_, _| {}).await;
        for name in ["", "   ", &"a".repeat(61)] {
            let (status, body) = served.add(name).await;
            assert_eq!(status, 422, "{name:?}: {body}");
            assert!(!error_of(&body).is_empty(), "{body}");
        }
    }

    #[tokio::test]
    async fn a_name_that_resolves_to_no_tailscale_address_is_422() {
        let served = Served::new(|_, _| {}).await;
        // A name nothing resolves, and an address outside Tailscale's ranges.
        for name in ["localhost.invalid", "192.0.2.1"] {
            let (status, body) = served.add(name).await;
            assert_eq!(status, 422, "{name:?}: {body}");
            assert!(error_of(&body).contains(name), "{body}");
        }
    }

    #[tokio::test]
    async fn a_name_a_listed_machine_has_in_any_case_or_spacing_is_409() {
        let peer = MachineId::random();
        let served = Served::new(move |_, settings| {
            settings
                .machines
                .push(machine(peer, " Trilogy-Mac ", "100.64.0.2"));
        })
        .await;

        for name in ["trilogy-mac", " TRILOGY-MAC ", "Personal-Mac"] {
            let (status, body) = served.add(name).await;
            assert_eq!(status, 409, "{name:?}: {body}");
            assert!(!error_of(&body).is_empty(), "{body}");
        }
    }

    #[tokio::test]
    async fn an_add_on_a_machine_not_in_the_list_yet_is_503() {
        let served = Served::new(|me, settings| settings.machines.retain(|m| m.id != me)).await;

        let (status, body) = served.add("127.0.0.1").await;

        assert_eq!(status, 503, "{body}");
        assert_eq!(
            error_of(&body),
            "this machine is not in its machine list yet; try again in a minute"
        );
    }
}
