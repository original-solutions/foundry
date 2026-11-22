//! Adding a machine by name: machine A gives a code for B through `POST /api/machines/add`,
//! B uses it with `join_at`, as `foundryd join` does, and A lists B under the name typed and
//! links with it.

use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use foundry_proto::settings::Machine;
use foundryd::api::machine_add;
use foundryd::coordinator::Coordinator;
use foundryd::join::{self, KeyedDialer};
use foundryd::secrets::FileSecrets;
use foundryd::sync::{Conn, Dialer};
use futures::future::BoxFuture;
use serde_json::{Value, json};

use crate::relay::{Keyed, Nowhere, ServedBy, keys};
use crate::{lock, until};

/// Dials through B's keys once B runs, and fails until then.
#[derive(Clone, Default)]
struct Later(Arc<Mutex<Option<ServedBy>>>);

impl Dialer for Later {
    fn dial(&self, peer: &Machine) -> BoxFuture<'static, io::Result<Conn>> {
        match lock(&self.0).as_ref() {
            Some(served) => served.dial(peer),
            None => Box::pin(async { Err(io::Error::other("B is not running yet")) }),
        }
    }
}

#[tokio::test]
async fn a_machine_added_by_name_is_listed_under_that_name_once_it_joins_and_links() {
    let (a_dir, b_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let later = Later::default();
    let a_keys = keys(&a_dir);
    let dialer = KeyedDialer::new(Arc::new(later.clone()), a_keys.clone());
    // A lists itself as `machine-0` and acts alone. It is listed at a loopback address other
    // than `localhost`'s, as a fleet run on one machine is, so it may add `localhost`. Nothing
    // dials A at it here.
    let a = Keyed::new(&[], a_keys, a_dir, Arc::new(dialer));
    {
        let mut store = lock(&a.store);
        let mut held = store.settings().unwrap();
        held.seq += 1;
        for listed in &mut held.machines {
            if listed.id == a.keys.me() {
                listed.address = "127.0.0.2".into();
            }
        }
        store.apply_settings(held).unwrap();
    }
    let coordinator = Arc::new(Coordinator::new(
        a.keys.me(),
        a.store.clone(),
        a.peers.clone(),
    ));
    a.peers.coordinate(coordinator.clone());
    tokio::spawn({
        let peers = a.peers.clone();
        async move { peers.run().await }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let lister = machine_add::Lister::new(coordinator, a.store.clone());
    let app = join::router(a.keys.clone(), Some(lister))
        .merge(machine_add::router(a.keys.clone(), a.store.clone()));
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap()
    });

    // In A's UI: add machine `localhost`.
    let added: Value = tokio::task::spawn_blocking(move || {
        let mut answer = ureq::post(format!("http://127.0.0.1:{port}/api/machines/add"))
            .send_json(json!({ "name": "localhost" }))
            .unwrap();
        assert_eq!(answer.status(), 201);
        answer.body_mut().read_json().unwrap()
    })
    .await
    .unwrap();
    let code = added["code"].as_str().unwrap().to_owned();
    assert_eq!(added["command"], format!("foundryd join 127.0.0.2 {code}"));

    // On B: `foundryd join`, then its daemon starts.
    let secrets = FileSecrets::new(b_dir.path().join("secrets"));
    let joined = tokio::task::spawn_blocking(move || {
        join::join_at("127.0.0.1", port, &code, &secrets, None)
    })
    .await
    .unwrap()
    .unwrap();
    let b = Keyed::new(&[], keys(&b_dir), b_dir, Arc::new(Nowhere));
    assert_eq!(b.keys.me(), joined);
    tokio::spawn({
        let peers = b.peers.clone();
        async move { peers.run().await }
    });
    *lock(&later.0) = Some(ServedBy {
        keys: b.keys.clone(),
        peers: b.peers.clone(),
    });

    let a_settings = || lock(&a.store).settings().unwrap();
    until(Duration::from_secs(10), "B listed on A", || {
        a_settings()
            .machines
            .iter()
            .any(|m| m.id == joined && m.name == "localhost")
    })
    .await;
    let listed = a_settings()
        .machines
        .into_iter()
        .find(|m| m.id == joined)
        .unwrap();
    assert!(
        listed
            .address
            .parse::<std::net::IpAddr>()
            .unwrap()
            .is_loopback(),
        "{listed:?}"
    );
    until(Duration::from_secs(10), "A and B linked", || {
        a.peers.online().contains(&joined) && b.peers.online().contains(&a.keys.me())
    })
    .await;
}
