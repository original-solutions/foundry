//! The order in which machines take the coordinator role, set from the machines board.
//!
//! - `POST /api/machines/priority` `{order: [machine id, ...]}`: sets the order through the
//!   acting coordinator, `204`. The order is stored as given; a machine left out acts after the
//!   listed ones, by id, as [`acting`](crate::coordinator::acting) treats it. An empty list is
//!   allowed. An id that repeats, one the machine list does not hold or text that is no id is
//!   `422`.
//!
//! Nothing is sent when a request is refused, and an edit the coordinator could not make answers
//! through [`edit_status`].

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use foundry_proto::ids::MachineId;
use foundry_proto::settings::Edit;
use serde::Deserialize;

use super::{ApiError, Body, edit_status, on_store, unprocessable};
use crate::coordinator::Coordinator;
use crate::store::Store;

/// What the route acts on: the coordinator that sends the edit and the store whose machine list
/// says whether to send it.
struct Held {
    coordinator: Arc<Coordinator>,
    store: Arc<Mutex<Store>>,
}

/// The priority route. The caller guards it as it guards the rest of the API.
pub fn router(coordinator: Arc<Coordinator>, store: Arc<Mutex<Store>>) -> Router {
    Router::new()
        .route("/api/machines/priority", post(set))
        .with_state(Arc::new(Held { coordinator, store }))
}

#[derive(Deserialize)]
struct Wanted {
    order: Vec<String>,
}

async fn set(
    State(held): State<Arc<Held>>,
    Body(wanted): Body<Wanted>,
) -> Result<Response, ApiError> {
    let listed = on_store(&held.store, |store| store.settings())
        .await?
        .machines;
    let mut order: Vec<MachineId> = Vec::with_capacity(wanted.order.len());
    for text in &wanted.order {
        let id = text
            .parse::<uuid::Uuid>()
            .map(MachineId)
            .ok()
            .filter(|id| listed.iter().any(|m| m.id == *id))
            .ok_or_else(|| unprocessable(format!("no machine {text:?} in the machine list")))?;
        if order.contains(&id) {
            return Err(unprocessable(format!("{id} is in the order twice")));
        }
        order.push(id);
    }
    match held.coordinator.edit(Edit::Priority { order }).await {
        Ok(_) => Ok(StatusCode::NO_CONTENT.into_response()),
        Err(e) => Err(ApiError::Rejected(edit_status(&e), e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io;
    use std::time::Duration;

    use foundry_proto::ids::MachineId;
    use foundry_proto::messages::{PROTOCOL, PeerMessage};
    use foundry_proto::settings::{Machine, SharedSettings};
    use futures::future::BoxFuture;
    use serde_json::Value;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::*;
    use crate::sync::{Conn, Dialer, Peers};

    struct NoDial;

    impl Dialer for NoDial {
        fn dial(&self, _: &Machine) -> BoxFuture<'static, io::Result<Conn>> {
            Box::pin(async { Err(io::Error::other("no peers in these tests")) })
        }
    }

    /// One machine's coordinator and store, its priority route served on a loopback port.
    struct Served {
        _data: tempfile::TempDir,
        store: Arc<Mutex<Store>>,
        peers: Peers,
        addr: std::net::SocketAddr,
        _server: tokio::task::JoinHandle<()>,
    }

    impl Served {
        /// A machine that acts alone, listed as `studio`, with `settings` applied on top.
        async fn new(settings: impl FnOnce(&mut SharedSettings)) -> Self {
            let data = tempfile::tempdir().unwrap();
            let me = MachineId::random();
            let mut store = Store::open(data.path().join("foundry.db")).unwrap();
            let mut held = store.settings().unwrap();
            held.seq += 1;
            held.machines.push(Machine {
                id: me,
                name: "studio".into(),
                address: "100.64.0.1".into(),
                key: String::new(),
            });
            settings(&mut held);
            store.apply_settings(held).unwrap();
            let store = Arc::new(Mutex::new(store));
            let peers = Peers::new(me, store.clone(), Arc::new(NoDial));
            let coordinator = Arc::new(Coordinator::new(me, store.clone(), peers.clone()));
            peers.coordinate(coordinator.clone());
            let app = router(coordinator, store.clone());
            let served_peers = peers.clone();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            Self {
                _data: data,
                store,
                peers: served_peers,
                addr,
                _server: server,
            }
        }

        fn settings(&self) -> SharedSettings {
            self.store.lock().unwrap().settings().unwrap()
        }

        /// Hears `peer` once, so it counts as online, then drops the line: an edit sent to it
        /// finds no one to answer.
        async fn hear(&self, peer: MachineId) {
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
            for message in [hello, nothing] {
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

        /// `(status, body)` of a POST of the JSON `body` to the priority route.
        async fn post(&self, body: &str) -> (u16, String) {
            let mut stream = tokio::net::TcpStream::connect(self.addr).await.unwrap();
            let request = format!(
                "POST /api/machines/priority HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\
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
    }

    /// Lists a second machine and a third, and gives their ids.
    fn others(settings: &mut SharedSettings) -> (MachineId, MachineId) {
        let ids = (MachineId::random(), MachineId::random());
        for (id, name, address) in [
            (ids.0, "trilogy-mac", "100.64.0.2"),
            (ids.1, "laptop", "100.64.0.3"),
        ] {
            settings.machines.push(Machine {
                id,
                name: name.into(),
                address: address.into(),
                key: String::new(),
            });
        }
        ids
    }

    fn order(ids: &[String]) -> String {
        serde_json::json!({ "order": ids }).to_string()
    }

    #[tokio::test]
    async fn an_id_that_repeats_is_422_and_nothing_is_sent() {
        let mut ids = None;
        let served = Served::new(|settings| ids = Some(others(settings))).await;
        let (a, _) = ids.unwrap();
        let before = served.settings();

        let (status, body) = served.post(&order(&[a.to_string(), a.to_string()])).await;

        assert_eq!(status, 422, "{body}");
        let json: Value = serde_json::from_str(&body).unwrap();
        assert!(json["error"].as_str().unwrap().contains(&a.to_string()));
        let after = served.settings();
        assert_eq!(after.seq, before.seq);
        assert_eq!(after.priority, before.priority);
    }

    #[tokio::test]
    async fn an_id_the_machine_list_does_not_hold_is_422_and_nothing_is_sent() {
        let served = Served::new(|_| {}).await;
        let before = served.settings();
        let stranger = MachineId::random().to_string();

        let (status, body) = served.post(&order(std::slice::from_ref(&stranger))).await;

        assert_eq!(status, 422, "{body}");
        let json: Value = serde_json::from_str(&body).unwrap();
        assert!(json["error"].as_str().unwrap().contains(&stranger));
        let after = served.settings();
        assert_eq!(after.seq, before.seq);
        assert_eq!(after.priority, before.priority);
    }

    #[tokio::test]
    async fn text_that_is_not_an_id_is_422_and_nothing_is_sent() {
        let served = Served::new(|_| {}).await;
        let before = served.settings();

        let (status, body) = served.post(&order(&["not-an-id".to_owned()])).await;

        assert_eq!(status, 422, "{body}");
        let json: Value = serde_json::from_str(&body).unwrap();
        assert!(json["error"].as_str().unwrap().contains("not-an-id"));
        let after = served.settings();
        assert_eq!(after.seq, before.seq);
        assert_eq!(after.priority, before.priority);
    }

    #[tokio::test]
    async fn a_body_without_an_order_is_422_and_nothing_is_sent() {
        let served = Served::new(|_| {}).await;
        let before = served.settings();

        for body in ["{}", "[]", "{\"order\": \"x\"}"] {
            let (status, answer) = served.post(body).await;
            assert_eq!(status, 422, "{body}: {answer}");
        }

        assert_eq!(served.settings().seq, before.seq);
    }

    #[tokio::test]
    async fn an_order_of_listed_machines_is_stored_as_given_and_answers_204() {
        let mut ids = None;
        let served = Served::new(|settings| ids = Some(others(settings))).await;
        let (a, b) = ids.unwrap();
        let seq = served.settings().seq;

        let (status, body) = served.post(&order(&[b.to_string(), a.to_string()])).await;

        assert_eq!(status, 204, "{body}");
        let after = served.settings();
        assert_eq!(after.priority, vec![b, a]);
        assert!(after.seq > seq);
    }

    #[tokio::test]
    async fn an_empty_order_answers_204() {
        let served = Served::new(|settings| {
            let (a, _) = others(settings);
            settings.priority = vec![a];
        })
        .await;
        let seq = served.settings().seq;

        let (status, body) = served.post(&order(&[])).await;

        assert_eq!(status, 204, "{body}");
        let after = served.settings();
        assert!(after.priority.is_empty());
        assert!(after.seq > seq);
    }

    #[tokio::test]
    async fn an_order_the_coordinator_cannot_send_answers_503_and_stores_nothing() {
        let acting = MachineId::random();
        let mut ids = None;
        let served = Served::new(|settings| {
            ids = Some(others(settings));
            settings.machines.push(Machine {
                id: acting,
                name: "x".into(),
                address: "100.64.0.9".into(),
                key: String::new(),
            });
            // An unreachable machine ahead in priority acts as coordinator.
            settings.priority = vec![acting, settings.machines[0].id];
        })
        .await;
        let (a, b) = ids.unwrap();
        served.hear(acting).await;
        let before = served.settings();

        let (status, body) = served.post(&order(&[b.to_string(), a.to_string()])).await;

        assert_eq!(status, 503, "{body}");
        let json: Value = serde_json::from_str(&body).unwrap();
        assert!(
            json["error"]
                .as_str()
                .unwrap()
                .contains(&acting.to_string())
        );
        let after = served.settings();
        assert_eq!(after.seq, before.seq);
        assert_eq!(after.priority, before.priority);
    }
}
