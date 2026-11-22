//! Adding a machine by its Tailscale name, from the UI: `POST /api/machines/add` `{name}`.
//!
//! The name is trimmed. One that is blank, over [`NAME_AT_MOST`](super::machine_names::NAME_AT_MOST) characters or holds a control
//! character is `422`, by the rename route's rule ([`checked_name`]), and so is one that
//! resolves to no Tailscale address, by the rule `foundryd join` sends a code by
//! ([`join_targets`]). Loopback counts only on a machine listed at a loopback address, as a
//! fleet run on one machine is. A name a listed machine has, compared trimmed and in any case,
//! is `409`, and so is a name that resolves to a listed machine's address. A machine the list
//! does not hold yet has no address to put in the command, so it answers `503`. Otherwise it
//! answers `201` `{code, command, expires_in_secs, address}`: a new join code,
//! `foundryd join <this machine's address> <code>` to run on the new machine, how long the
//! code works ([`CODE_VALID_FOR`]) and the address the name resolved to that the machine is
//! listed at when it does not say its own: the first IPv4 one, as a machine lists itself.
//!
//! The code works only from an address the name resolved to. The name and that address are
//! held with the code until it expires. When a machine uses the code on the join route, the
//! route lists it through [`Lister`], through the acting coordinator, as every machine list
//! change is, before it answers: with its key, under the name and address it lists itself
//! under, or under the name typed at that address when it sends none. A list that already holds
//! the machine is left as it is. When another machine has the name or the address, or the edit
//! fails, the join is `503`, and the same machine may send the code again. A code made anywhere
//! else lists nothing.

use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use foundry_proto::settings::{Edit, Machine};
use serde::Deserialize;

use super::machine_names::checked_name;
use super::{ApiError, Body, on_store, unprocessable};
use crate::coordinator::Coordinator;
use crate::join::{CODE_VALID_FOR, DEFAULT_PORT, JoinError, Keys, Named, join_targets};
use crate::store::Store;

/// What the add route acts on: the keys that make codes and the store whose machine list it
/// checks.
struct Adding {
    keys: Keys,
    store: Arc<Mutex<Store>>,
}

/// `POST /api/machines/add`. The caller guards it as it guards the rest of the API, and serves
/// [`crate::join::router`] beside it on the same keys, with a [`Lister`].
pub fn router(keys: Keys, store: Arc<Mutex<Store>>) -> Router {
    Router::new()
        .route("/api/machines/add", post(add))
        .with_state(Arc::new(Adding { keys, store }))
}

#[derive(Deserialize)]
struct Add {
    name: String,
}

async fn add(
    State(adding): State<Arc<Adding>>,
    Body(add): Body<Add>,
) -> Result<Response, ApiError> {
    let name = checked_name(&add.name, "type the new machine's Tailscale name")?.to_owned();
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
    let own_loopback = own
        .address
        .trim()
        .parse::<IpAddr>()
        .is_ok_and(|ip| ip.to_canonical().is_loopback());
    let addresses: Vec<IpAddr> = resolve(&name)
        .await?
        .into_iter()
        .filter(|ip| own_loopback || !ip.to_canonical().is_loopback())
        .collect();
    if addresses.is_empty() {
        return Err(not_on_tailscale(&name));
    }
    if let Some(other) = settings
        .machines
        .iter()
        .find(|m| addresses.iter().any(|ip| has_address(&m.address, *ip)))
    {
        return Err(ApiError::Rejected(
            StatusCode::CONFLICT,
            format!("{name:?} is already in the fleet as {:?}", other.name),
        ));
    }
    let address = listed_address(&addresses);
    let named = Named { name, address };
    let code = adding
        .keys
        .new_named_code(named, addresses)
        .map_err(|e| ApiError::Rejected(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let answer = serde_json::json!({
        "command": format!("foundryd join {} {code}", own.address.trim()),
        "code": code.to_string(),
        "expires_in_secs": CODE_VALID_FOR.as_secs(),
        "address": address.to_string(),
    });
    Ok((StatusCode::CREATED, axum::Json(answer)).into_response())
}

/// The address of `addresses`, which a name resolved to, that the machine is listed at when it
/// does not say its own: the first IPv4 one, as a machine lists itself
/// ([`entry`](super::machine_names::entry)), else the first. `addresses` is never empty.
fn listed_address(addresses: &[IpAddr]) -> IpAddr {
    addresses
        .iter()
        .find(|ip| ip.to_canonical().is_ipv4())
        .unwrap_or(&addresses[0])
        .to_canonical()
}

/// Whether a listed `address` is `ip`: compared as an IP when it parses as one, else as text.
fn has_address(address: &str, ip: IpAddr) -> bool {
    let address = address.trim();
    address
        .parse::<IpAddr>()
        .map_or_else(|_| address == ip.to_string(), |listed| listed == ip)
}

/// Every Tailscale address `name` resolves to, first the one `foundryd join` sends a code to.
/// Resolving it can wait on DNS, so it runs on the blocking pool.
async fn resolve(name: &str) -> Result<Vec<IpAddr>, ApiError> {
    let lookup = name.to_owned();
    let resolved = tokio::task::spawn_blocking(move || join_targets(&lookup, DEFAULT_PORT))
        .await
        .map_err(|e| ApiError::Rejected(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    match resolved {
        Ok(targets) => Ok(targets.iter().map(|target| target.ip()).collect()),
        Err(JoinError::NotTailscale(_)) => Err(not_on_tailscale(name)),
        Err(_) => Err(unprocessable(format!(
            "no machine named {name:?} was found; type the name Tailscale shows for it"
        ))),
    }
}

fn not_on_tailscale(name: &str) -> ApiError {
    unprocessable(format!(
        "{name:?} is not on Tailscale: it resolves to no Tailscale address"
    ))
}

/// Lists a machine that used a code the add route made, for the join route.
#[derive(Clone)]
pub struct Lister {
    coordinator: Arc<Coordinator>,
    store: Arc<Mutex<Store>>,
}

impl Lister {
    pub fn new(coordinator: Arc<Coordinator>, store: Arc<Mutex<Store>>) -> Self {
        Self { coordinator, store }
    }

    /// Lists `machine`, which used a code, and says once the edit is numbered. A list that
    /// already holds it (this machine included) is left as it is: its entry is never
    /// overwritten, and a join sent again after a lost answer finds it there. A list that
    /// cannot be read, another machine with its name or its address, or an edit that fails
    /// gives why, which is warned. The code is never logged.
    pub(crate) async fn list(&self, machine: Machine) -> Result<(), String> {
        let id = machine.id;
        let held = on_store(&self.store, |store| store.settings())
            .await
            .map_err(|error| format!("the machine list could not be read: {error}"))?;
        if held.machines.iter().any(|m| m.id == id) {
            return Ok(());
        }
        let wanted = machine.name.trim().to_lowercase();
        let address = machine.address.trim();
        if let Some(other) = held.machines.iter().find(|m| {
            m.name.trim().to_lowercase() == wanted
                || address
                    .parse::<IpAddr>()
                    .map_or(m.address.trim() == address, |ip| {
                        has_address(&m.address, ip)
                    })
        }) {
            let why = format!(
                "{:?} at {} is already in the machine list, so the machine is not listed as {:?} \
                 at {}; rename or remove that machine first",
                other.name, other.address, machine.name, machine.address
            );
            tracing::warn!("machine {id} used a code to join, but {why}");
            return Err(why);
        }
        match self.coordinator.edit(Edit::SetMachine { machine }).await {
            Ok(_) => Ok(()),
            Err(error) => {
                tracing::warn!(
                    "machine {id} used a code to join, but it could not be listed: {error}"
                );
                Err(error.to_string())
            }
        }
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
        /// A machine listed as `personal-mac` at [`OWN`] that acts alone, holding the
        /// settings `settings` makes from that.
        async fn new(settings: impl FnOnce(MachineId, &mut SharedSettings)) -> Self {
            let data = tempfile::tempdir().unwrap();
            let keys = Keys::open(Arc::new(MemorySecrets::default())).unwrap();
            let me = keys.me();
            let mut store = Store::open(data.path().join("foundry.db")).unwrap();
            let mut held = store.settings().unwrap();
            held.seq += 1;
            held.machines.push(machine(me, "personal-mac", OWN));
            held.priority = vec![me];
            settings(me, &mut held);
            store.apply_settings(held).unwrap();
            let store = Arc::new(Mutex::new(store));
            let peers = Peers::new(me, store.clone(), Arc::new(NoDial));
            let coordinator = Arc::new(Coordinator::new(me, store.clone(), peers.clone()));
            peers.coordinate(coordinator.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let lister = Lister::new(coordinator.clone(), store.clone());
            let app = join::router(keys.clone(), lister).merge(router(keys.clone(), store.clone()));
            let server = tokio::spawn(async move {
                axum::serve(
                    listener,
                    app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
                )
                .await
                .unwrap();
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

        /// `(status, body)` of a join with `code` by `joining`, as a `foundryd join` from
        /// before it sent the machine's own entry posts it.
        async fn join(&self, code: &str, joining: &Keys) -> (u16, String) {
            let request = json!({
                "code": code,
                "machine": joining.me(),
                "key": joining.public_key(),
            });
            self.post(join::JOIN_PATH, &request).await
        }

        /// `(status, body)` of a join with `code` by `joining`, as `foundryd join` posts it
        /// with the entry `joining` lists itself under: `name` and `address`, each when given.
        async fn join_as(
            &self,
            code: &str,
            joining: &Keys,
            name: Option<&str>,
            address: Option<&str>,
        ) -> (u16, String) {
            let mut request = json!({
                "code": code,
                "machine": joining.me(),
                "key": joining.public_key(),
            });
            if let Some(name) = name {
                request["name"] = json!(name);
            }
            if let Some(address) = address {
                request["address"] = json!(address);
            }
            self.post(join::JOIN_PATH, &request).await
        }

        /// The entry the machine list holds for `id` now, without waiting.
        fn held(&self, id: MachineId) -> Option<Machine> {
            self.settings().machines.into_iter().find(|m| m.id == id)
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

    /// This machine's address in these tests: loopback, as a fleet run on one machine is, so a
    /// loopback name can be added. A request to the routes comes from 127.0.0.1.
    const OWN: &str = "127.0.0.2";

    fn machine(id: MachineId, name: &str, address: &str) -> Machine {
        Machine {
            id,
            name: name.into(),
            address: address.into(),
            key: String::new(),
        }
    }

    fn error_of(body: &str) -> String {
        let json: Value = serde_json::from_str(body).unwrap();
        json["error"].as_str().unwrap().to_owned()
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
            format!("foundryd join {OWN} {code}"),
            "{body}"
        );
        assert_eq!(answer["expires_in_secs"], CODE_VALID_FOR.as_secs());
        assert_eq!(answer["address"], "127.0.0.1", "{body}");
        // An add changes nothing in the list until the code is used.
        assert_eq!(served.settings().machines.len(), 1);
    }

    /// The entry `joining` is listed under at `name` and `address`, with its key.
    fn keyed(joining: &Keys, name: &str, address: &str) -> Machine {
        Machine {
            key: joining.public_key().hex().to_owned(),
            ..machine(joining.me(), name, address)
        }
    }

    #[tokio::test]
    async fn a_join_that_sends_its_own_entry_is_listed_as_that_entry_before_it_is_answered() {
        let served = Served::new(|_, _| {}).await;
        let joining = Keys::open(Arc::new(MemorySecrets::default())).unwrap();
        let code = served.code_for(" 127.0.0.1 ").await;

        let (status, body) = served
            .join_as(&code, &joining, Some("build-box"), Some("100.64.0.7"))
            .await;

        assert_eq!(status, 200, "{body}");
        assert_eq!(
            served.held(joining.me()),
            Some(keyed(&joining, "build-box", "100.64.0.7"))
        );
    }

    #[tokio::test]
    async fn a_join_with_no_name_or_no_address_is_listed_under_the_name_typed_at_its_address() {
        for (name, address) in [
            (None, None),
            (Some("build-box"), None),
            (None, Some("100.64.0.7")),
            (Some(" "), Some("100.64.0.7")),
            (Some("build-box"), Some(" ")),
        ] {
            let served = Served::new(|_, _| {}).await;
            let joining = Keys::open(Arc::new(MemorySecrets::default())).unwrap();
            let code = served.code_for(" 127.0.0.1 ").await;

            let (status, body) = served.join_as(&code, &joining, name, address).await;

            assert_eq!(status, 200, "{name:?} {address:?}: {body}");
            assert_eq!(
                served.held(joining.me()),
                Some(keyed(&joining, "127.0.0.1", "127.0.0.1")),
                "{name:?} {address:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_name_is_listed_as_typed_at_the_address_it_resolves_to() {
        let served = Served::new(|_, _| {}).await;
        let joining = Keys::open(Arc::new(MemorySecrets::default())).unwrap();
        let code = served.code_for("localhost").await;

        assert_eq!(served.join(&code, &joining).await.0, 200);

        let listed = served.listed(joining.me()).await;
        assert_eq!(listed.name, "localhost");
        let resolved: Vec<IpAddr> = join::join_targets("localhost", join::DEFAULT_PORT)
            .unwrap()
            .iter()
            .map(|target| target.ip())
            .collect();
        assert_eq!(
            listed.address.parse::<IpAddr>().unwrap(),
            listed_address(&resolved)
        );
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
    async fn a_join_whose_sent_name_or_address_another_machine_has_is_503_naming_it() {
        for (name, address) in [(" BUILD-box ", "100.64.0.7"), ("studio", "100.64.0.9")] {
            let other = MachineId::random();
            let served = Served::new(move |_, settings| {
                settings
                    .machines
                    .push(machine(other, "build-box", "100.64.0.9"));
            })
            .await;
            // Caught up with it, so this machine numbers an edit at once.
            served.hear_sending(other, Some(served.settings())).await;
            let joining = Keys::open(Arc::new(MemorySecrets::default())).unwrap();
            let code = served.code_for("127.0.0.1").await;

            let (status, body) = served
                .join_as(&code, &joining, Some(name), Some(address))
                .await;

            assert_eq!(status, 503, "{name:?} at {address}: {body}");
            assert!(error_of(&body).contains("build-box"), "{body}");
            assert_eq!(served.held(joining.me()), None, "{name:?} at {address}");
        }
    }

    #[tokio::test]
    async fn a_long_sent_name_is_listed_as_sent_as_the_machine_lists_itself() {
        let long = "n".repeat(crate::api::machine_names::NAME_AT_MOST + 3);
        let served = Served::new(|_, _| {}).await;
        let joining = Keys::open(Arc::new(MemorySecrets::default())).unwrap();
        let code = served.code_for("127.0.0.1").await;

        let (status, body) = served
            .join_as(&code, &joining, Some(&long), Some("100.64.0.7"))
            .await;

        assert_eq!(status, 200, "{body}");
        assert_eq!(
            served.held(joining.me()),
            Some(keyed(&joining, &long, "100.64.0.7"))
        );
    }

    #[tokio::test]
    async fn a_sent_name_or_address_outside_the_rules_is_503_and_admits_nothing() {
        for (name, address) in [
            ("build\tbox", "100.64.0.7"),
            ("build-box", "relay.example.com"),
            ("build-box", "203.0.113.9"),
        ] {
            let served = Served::new(|_, _| {}).await;
            let joining = Keys::open(Arc::new(MemorySecrets::default())).unwrap();
            let code = served.code_for("127.0.0.1").await;

            let (status, body) = served
                .join_as(&code, &joining, Some(name), Some(address))
                .await;

            assert_eq!(status, 503, "{name:?} at {address}: {body}");
            assert!(!error_of(&body).chars().any(char::is_control), "{body}");
            assert_eq!(served.held(joining.me()), None, "{name:?} at {address}");
            assert_eq!(served.keys.state(joining.me()), join::KeyState::NoKey);
        }
    }

    #[test]
    fn a_name_is_listed_at_its_first_ipv4_address_as_a_machine_lists_itself() {
        let v6: IpAddr = "fd7a:115c:a1e0::7".parse().unwrap();
        let v4: IpAddr = "100.64.0.7".parse().unwrap();
        assert_eq!(listed_address(&[v6, v4]), v4);
        assert_eq!(listed_address(&[v4, v6]), v4);
        assert_eq!(listed_address(&[v6]), v6);
    }

    #[tokio::test]
    async fn a_listing_the_coordinator_cannot_make_is_503_and_the_same_code_works_again() {
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

        let (status, body) = served
            .join_as(&code, &joining, Some("build-box"), Some("100.64.0.7"))
            .await;

        assert_eq!(status, 503, "{body}");
        let error = error_of(&body);
        assert!(error.contains(&peer.to_string()), "{error}");
        assert!(!body.contains(&code), "the code reached the answer: {body}");
        assert_eq!(served.held(joining.me()), None);

        // This machine comes to act, and the same machine sends the same code again.
        let mut acting = served.settings();
        acting.seq += 1;
        acting.priority.reverse();
        served.store.lock().unwrap().apply_settings(acting).unwrap();
        let (status, body) = served
            .join_as(&code, &joining, Some("build-box"), Some("100.64.0.7"))
            .await;

        assert_eq!(status, 200, "{body}");
        assert_eq!(
            served.held(joining.me()),
            Some(keyed(&joining, "build-box", "100.64.0.7"))
        );
    }

    #[tokio::test]
    async fn a_blank_name_and_a_long_one_are_each_422_with_their_own_message() {
        let served = Served::new(|_, _| {}).await;
        for name in ["", "   "] {
            let (status, body) = served.add(name).await;
            assert_eq!(status, 422, "{name:?}: {body}");
            assert_eq!(error_of(&body), "type the new machine's Tailscale name");
        }
        let (status, body) = served.add(&"a".repeat(61)).await;
        assert_eq!(status, 422, "{body}");
        assert!(error_of(&body).contains("at most 60 characters"), "{body}");
        // A name with a control character is refused by the rename route's rule, before any
        // lookup.
        let (status, body) = served.add("trilogy\tmac").await;
        assert_eq!(status, 422, "{body}");
        assert!(error_of(&body).contains("control characters"), "{body}");
    }

    #[tokio::test]
    async fn a_name_of_exactly_sixty_characters_passes_the_length_check() {
        let served = Served::new(|_, _| {}).await;

        // As the 422 test below does, a name under `.invalid`, which nothing resolves.
        let name = format!("{}.invalid", "a".repeat(52));
        assert_eq!(name.chars().count(), 60);

        let (status, body) = served.add(&name).await;

        // It goes on to resolution, which refuses it for the name, not its length.
        assert_eq!(status, 422, "{body}");
        let error = error_of(&body);
        assert!(!error.contains("at most"), "{error}");
        assert!(error.contains(&name), "{error}");
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
    async fn a_loopback_name_is_422_on_a_machine_listed_at_a_tailscale_address() {
        let served = Served::new(|me, settings| {
            for listed in &mut settings.machines {
                if listed.id == me {
                    listed.address = "100.91.54.18".into();
                }
            }
        })
        .await;
        let before = served.settings();

        for name in ["localhost", "127.0.0.1"] {
            let (status, body) = served.add(name).await;
            assert_eq!(status, 422, "{name:?}: {body}");
            assert!(error_of(&body).contains("not on Tailscale"), "{body}");
        }
        assert_eq!(served.settings(), before);
    }

    #[tokio::test]
    async fn a_code_used_from_an_address_the_name_did_not_resolve_to_is_403_and_lists_nothing() {
        let served = Served::new(|_, _| {}).await;
        let joining = Keys::open(Arc::new(MemorySecrets::default())).unwrap();
        // Made for the machine at 127.0.0.3; the join below comes from 127.0.0.1.
        let code = served.code_for("127.0.0.3").await;
        let before = served.settings();

        let (status, body) = served.join(&code, &joining).await;

        assert_eq!(status, 403, "{body}");
        assert!(body.contains("made for another machine"), "{body}");
        assert_eq!(served.keys.state(joining.me()), join::KeyState::NoKey);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(served.settings(), before);
    }

    #[tokio::test]
    async fn a_second_code_for_the_same_name_is_503_once_the_first_is_used_and_lists_nobody() {
        let served = Served::new(|_, _| {}).await;
        let (first, second) = (
            Keys::open(Arc::new(MemorySecrets::default())).unwrap(),
            Keys::open(Arc::new(MemorySecrets::default())).unwrap(),
        );
        let first_code = served.code_for("127.0.0.1").await;
        let second_code = served.code_for("127.0.0.1").await;

        assert_eq!(served.join(&first_code, &first).await.0, 200);
        served.listed(first.me()).await;
        // Caught up with it, so this machine numbers an edit at once.
        served
            .hear_sending(first.me(), Some(served.settings()))
            .await;
        let (status, body) = served.join(&second_code, &second).await;
        assert_eq!(status, 503, "{body}");
        assert!(error_of(&body).contains("127.0.0.1"), "{body}");

        tokio::time::sleep(Duration::from_millis(300)).await;
        let machines = served.settings().machines;
        let named: Vec<_> = machines
            .iter()
            .filter(|m| m.name == "127.0.0.1" || m.address == "127.0.0.1")
            .collect();
        assert_eq!(named, [&keyed(&first, "127.0.0.1", "127.0.0.1")]);
        assert!(
            !machines.iter().any(|m| m.id == second.me()),
            "{machines:?}"
        );
    }

    #[tokio::test]
    async fn a_name_or_an_address_listed_after_the_code_was_made_is_503_and_lists_nobody() {
        // Listed while the code was open, as a rename or another join does.
        for (name, address) in [(" 127.0.0.1 ", "100.64.0.2"), ("trilogy-mac", "127.0.0.1")] {
            let served = Served::new(|_, _| {}).await;
            let joining = Keys::open(Arc::new(MemorySecrets::default())).unwrap();
            let code = served.code_for("127.0.0.1").await;
            let listed = MachineId::random();
            {
                let mut store = served.store.lock().unwrap();
                let mut held = store.settings().unwrap();
                held.seq += 1;
                held.machines.push(machine(listed, name, address));
                store.apply_settings(held).unwrap();
            }
            // Caught up with it, so this machine numbers an edit at once.
            served.hear_sending(listed, Some(served.settings())).await;

            let (status, body) = served.join(&code, &joining).await;
            assert_eq!(status, 503, "{name:?} at {address}: {body}");

            let machines = served.settings().machines;
            assert!(
                !machines.iter().any(|m| m.id == joining.me()),
                "{name:?} at {address}: {machines:?}"
            );
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
    async fn a_name_that_resolves_to_a_listed_machines_address_is_409() {
        let peer = MachineId::random();
        let served = Served::new(move |me, settings| {
            settings
                .machines
                .push(machine(peer, "trilogy-mac", "127.0.0.1"));
            settings.priority = vec![me];
        })
        .await;

        for name in ["localhost", "127.0.0.1"] {
            let (status, body) = served.add(name).await;

            assert_eq!(status, 409, "{name:?}: {body}");
            assert!(error_of(&body).contains("trilogy-mac"), "{body}");
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
