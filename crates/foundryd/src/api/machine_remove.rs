//! Removing a machine from the fleet.

use std::sync::{Arc, Mutex};

use foundry_proto::ids::MachineId;

use crate::coordinator::Coordinator;
use crate::store::Store;

#[cfg(test)]
mod tests {
    use std::io;
    use std::time::Duration;

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

    /// One machine's coordinator and store, its remove route served on a loopback port.
    struct Served {
        _data: tempfile::TempDir,
        me: MachineId,
        store: Arc<Mutex<Store>>,
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
            let app = router(me, coordinator, store.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            Self {
                _data: data,
                me,
                store,
                addr,
                _server: server,
            }
        }

        fn settings(&self) -> SharedSettings {
            self.store.lock().unwrap().settings().unwrap()
        }

        /// `(status, body)` of a POST to the remove route of `id`.
        async fn remove(&self, id: impl std::fmt::Display) -> (u16, String) {
            let mut stream = tokio::net::TcpStream::connect(self.addr).await.unwrap();
            let request = format!(
                "POST /api/machines/{id}/remove HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\
                 Content-Length: 0\r\n\r\n",
                self.addr
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

    fn other(settings: &mut SharedSettings) -> MachineId {
        let id = MachineId::random();
        settings.machines.push(Machine {
            id,
            name: "trilogy-mac".into(),
            address: "100.64.0.2".into(),
            key: String::new(),
        });
        id
    }

    #[tokio::test]
    async fn removing_the_machine_that_serves_the_request_is_409_and_sends_nothing() {
        let served = Served::new(|settings| {
            other(settings);
        })
        .await;
        let seq = served.settings().seq;

        let (status, body) = served.remove(served.me).await;

        assert_eq!(status, 409, "{body}");
        let json: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["error"], "remove a machine from another machine's UI");
        let after = served.settings();
        assert_eq!(after.seq, seq);
        assert!(after.machines.iter().any(|m| m.id == served.me));
    }

    #[tokio::test]
    async fn a_machine_the_list_does_not_hold_is_404_and_nothing_is_sent() {
        let served = Served::new(|_| {}).await;
        let seq = served.settings().seq;

        for id in [MachineId::random().to_string(), "not-an-id".to_owned()] {
            let (status, body) = served.remove(&id).await;
            assert_eq!(status, 404, "{id}: {body}");
            let json: Value = serde_json::from_str(&body).unwrap();
            assert!(!json["error"].as_str().unwrap().is_empty(), "{body}");
        }

        assert_eq!(served.settings().seq, seq);
    }

    #[tokio::test]
    async fn another_listed_machine_is_removed_and_answers_204() {
        let mut gone = None;
        let served = Served::new(|settings| gone = Some(other(settings))).await;
        let gone = gone.unwrap();
        let seq = served.settings().seq;

        let (status, body) = served.remove(gone).await;

        assert_eq!(status, 204, "{body}");
        let after = served.settings();
        assert!(after.machines.iter().all(|m| m.id != gone));
        assert!(after.machines.iter().any(|m| m.id == served.me));
        assert!(after.seq > seq);
    }
}
