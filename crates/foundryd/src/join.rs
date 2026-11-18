//! One-time join codes and machine keys. Filled by plan task 13.

use std::process::ExitCode;

/// `foundryd join <address> <code>`.
pub fn join(_address: &str, _code: &str) -> ExitCode {
    crate::not_built("join")
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use foundry_proto::ids::MachineId;
    use foundry_proto::messages::{PROTOCOL, PeerMessage};
    use foundry_proto::settings::{Machine, SharedSettings};
    use futures::future::BoxFuture;
    use tokio::time::timeout;

    use super::*;
    use crate::secrets::{MemorySecrets, Secrets};
    use crate::store::Store;
    use crate::sync::{Conn, Dialer, Peers};

    fn keys() -> (Arc<MemorySecrets>, Keys) {
        let secrets = Arc::new(MemorySecrets::default());
        let keys = Keys::load(secrets.clone()).unwrap();
        (secrets, keys)
    }

    /// A machine that joined through `keys` with a fresh code.
    fn joined(keys: &Keys) -> Identity {
        let code = keys.new_code();
        keys.redeem(&code.to_string()).unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn a_code_works_once() {
        let (_, keys) = keys();
        let code = keys.new_code().to_string();
        let identity = keys.redeem(&code).unwrap();
        assert_eq!(keys.check(identity.machine, Some(&identity.key)), Ok(()));
        assert!(matches!(keys.redeem(&code), Err(JoinError::BadCode)));
    }

    #[tokio::test(start_paused = true)]
    async fn a_code_works_for_ten_minutes_and_not_after() {
        let (_, keys) = keys();
        let early = keys.new_code().to_string();
        let late = keys.new_code().to_string();
        tokio::time::advance(CODE_VALID_FOR - Duration::from_secs(1)).await;
        assert!(keys.redeem(&early).is_ok());
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(matches!(keys.redeem(&late), Err(JoinError::BadCode)));
    }

    #[tokio::test(start_paused = true)]
    async fn a_code_typed_in_lower_case_with_spaces_around_it_works() {
        let (_, keys) = keys();
        let code = keys.new_code().to_string();
        assert!(keys.redeem(&format!(" {} ", code.to_lowercase())).is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn a_code_never_issued_is_refused() {
        let (_, keys) = keys();
        keys.new_code();
        assert!(matches!(keys.redeem("ABCDEFGHJK"), Err(JoinError::BadCode)));
        assert!(matches!(keys.redeem(""), Err(JoinError::BadCode)));
    }

    #[tokio::test(start_paused = true)]
    async fn codes_and_keys_differ_each_time() {
        let (_, keys) = keys();
        assert_ne!(keys.new_code().to_string(), keys.new_code().to_string());
        let (a, b) = (joined(&keys), joined(&keys));
        assert_ne!(a.machine, b.machine);
        assert_eq!(keys.check(a.machine, Some(&b.key)), Err(Refused::WrongKey));
    }

    #[tokio::test(start_paused = true)]
    async fn a_machine_is_refused_without_a_key_with_a_wrong_one_or_unknown() {
        let (_, keys) = keys();
        let identity = joined(&keys);
        let other = joined(&keys);
        assert_eq!(keys.check(identity.machine, None), Err(Refused::NoKey));
        assert_eq!(
            keys.check(identity.machine, Some(&other.key)),
            Err(Refused::WrongKey)
        );
        assert_eq!(
            keys.check(MachineId::random(), Some(&identity.key)),
            Err(Refused::Unknown)
        );
        assert_eq!(keys.state(MachineId::random()), KeyState::NoKey);
    }

    #[tokio::test(start_paused = true)]
    async fn a_revoked_machine_is_refused_and_shown_as_revoked() {
        let (_, keys) = keys();
        let identity = joined(&keys);
        assert_eq!(keys.state(identity.machine), KeyState::Active);
        keys.revoke(identity.machine).unwrap();
        assert_eq!(keys.state(identity.machine), KeyState::Revoked);
        assert_eq!(
            keys.check(identity.machine, Some(&identity.key)),
            Err(Refused::Revoked)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn keys_and_revocations_survive_a_restart() {
        let (secrets, keys) = keys();
        let kept = joined(&keys);
        let revoked = joined(&keys);
        keys.revoke(revoked.machine).unwrap();
        let again = Keys::load(secrets).unwrap();
        assert_eq!(again.check(kept.machine, Some(&kept.key)), Ok(()));
        assert_eq!(again.state(revoked.machine), KeyState::Revoked);
    }

    #[tokio::test(start_paused = true)]
    async fn a_key_or_code_never_shows_in_debug_output() {
        let (secrets, keys) = keys();
        let code = keys.new_code();
        assert!(!format!("{code:?}").contains(&code.to_string()));
        let identity = keys.redeem(&code.to_string()).unwrap();
        let stored = secrets.get(KEYS_SECRET).unwrap().unwrap();
        let key: serde_json::Value = serde_json::from_str(&stored).unwrap();
        let key = key[identity.machine.to_string()]["key"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(key.len() >= 32, "{key}");
        assert!(!format!("{identity:?}").contains(&key));
        assert!(!format!("{:?}", identity.key).contains(&key));
    }

    // ---- peer links ----

    /// A dialer that hands out the near end of a fresh pair and keeps the far end for the test.
    #[derive(Default)]
    struct Pairs(Mutex<Vec<Conn>>);

    impl Dialer for Pairs {
        fn dial(&self, _peer: &Machine) -> BoxFuture<'static, io::Result<Conn>> {
            let (near, far) = Conn::pair();
            self.0.lock().unwrap().push(far);
            Box::pin(async move { Ok(near) })
        }
    }

    fn machine(id: MachineId) -> Machine {
        Machine {
            id,
            name: format!("m-{id}"),
            address: "127.0.0.1".into(),
        }
    }

    /// The links of machine `me`, whose machine list holds `me` and `others`.
    fn peers(me: MachineId, others: &[MachineId]) -> (tempfile::TempDir, Peers) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("foundry.db")).unwrap();
        let settings = SharedSettings {
            seq: 1,
            machines: [me].iter().chain(others).map(|id| machine(*id)).collect(),
            ..SharedSettings::default()
        };
        assert!(store.apply_settings(settings).unwrap());
        let peers = Peers::new(me, Arc::new(Mutex::new(store)), Arc::new(Pairs::default()));
        (dir, peers)
    }

    fn hello(machine: MachineId) -> String {
        serde_json::to_string(&PeerMessage::Hello {
            protocol: PROTOCOL,
            machine,
            versions: Default::default(),
        })
        .unwrap()
    }

    /// Opens a link to `keys`' machine as a peer that sends `first`, then a hello naming
    /// `named`, and returns the peer's end.
    async fn open(keys: &Keys, peers: &Peers, first: Option<String>, named: MachineId) -> Conn {
        let (near, far) = Conn::pair();
        let (keys, peers) = (keys.clone(), peers.clone());
        tokio::spawn(async move { keys.accept(&peers, near).await });
        if let Some(first) = first {
            far.send.send(first).await.unwrap();
        }
        far.send.send(hello(named)).await.unwrap();
        far
    }

    /// The next message on `conn`, or `None` once the link is closed.
    async fn next(conn: &mut Conn) -> Option<String> {
        timeout(Duration::from_secs(5), conn.recv.recv())
            .await
            .expect("the link neither answered nor closed")
    }

    /// Waits for the link to close, skipping heartbeats.
    async fn closed(conn: &mut Conn) {
        while next(conn).await.is_some() {}
    }

    fn is_hello(text: &str) -> bool {
        matches!(serde_json::from_str(text), Ok(PeerMessage::Hello { .. }))
    }

    #[tokio::test]
    async fn a_link_with_a_good_key_is_served() {
        let (_, keys) = keys();
        let peer = joined(&keys);
        let me = MachineId::random();
        let (_dir, peers) = peers(me, &[peer.machine]);
        let mut far = open(&keys, &peers, Some(key_message(&peer)), peer.machine).await;
        let answer = next(&mut far).await.expect("the link was refused");
        assert!(is_hello(&answer), "{answer}");
    }

    #[tokio::test]
    async fn a_link_with_no_key_is_refused() {
        let (_, keys) = keys();
        let peer = joined(&keys);
        let (_dir, peers) = peers(MachineId::random(), &[peer.machine]);
        let mut far = open(&keys, &peers, None, peer.machine).await;
        assert_eq!(next(&mut far).await, None);
    }

    #[tokio::test]
    async fn a_link_with_a_revoked_key_is_refused() {
        let (_, keys) = keys();
        let peer = joined(&keys);
        keys.revoke(peer.machine).unwrap();
        let (_dir, peers) = peers(MachineId::random(), &[peer.machine]);
        let mut far = open(&keys, &peers, Some(key_message(&peer)), peer.machine).await;
        assert_eq!(next(&mut far).await, None);
        assert!(!peers.online().contains(&peer.machine));
    }

    #[tokio::test]
    async fn a_key_that_proves_one_machine_cannot_open_a_link_as_another() {
        let (_, keys) = keys();
        let (a, b) = (joined(&keys), joined(&keys));
        let (_dir, peers) = peers(MachineId::random(), &[a.machine, b.machine]);
        let mut far = open(&keys, &peers, Some(key_message(&a)), b.machine).await;
        assert_eq!(next(&mut far).await, None);
    }

    #[tokio::test]
    async fn an_open_link_closes_when_its_machine_is_revoked() {
        let (_, keys) = keys();
        let peer = joined(&keys);
        let (_dir, peers) = peers(MachineId::random(), &[peer.machine]);
        let mut far = open(&keys, &peers, Some(key_message(&peer)), peer.machine).await;
        assert!(next(&mut far).await.is_some());
        keys.revoke(peer.machine).unwrap();
        closed(&mut far).await;
    }

    #[tokio::test]
    async fn a_dialed_link_opens_with_this_machines_key() {
        let (_, keys) = keys();
        let me = joined(&keys);
        let inner = Arc::new(Pairs::default());
        let dialer = KeyedDialer::new(inner.clone(), me.clone(), keys.clone());
        let peer = MachineId::random();
        let _conn = dialer.dial(&machine(peer)).await.unwrap();
        let mut far = inner.0.lock().unwrap().pop().unwrap();
        let first = next(&mut far).await.unwrap();
        assert_eq!(first, key_message(&me));
    }

    #[tokio::test]
    async fn a_revoked_machine_is_not_dialed() {
        let (_, keys) = keys();
        let me = joined(&keys);
        let peer = joined(&keys);
        keys.revoke(peer.machine).unwrap();
        let inner = Arc::new(Pairs::default());
        let dialer = KeyedDialer::new(inner.clone(), me, keys);
        assert!(dialer.dial(&machine(peer.machine)).await.is_err());
        assert!(
            inner.0.lock().unwrap().is_empty(),
            "the revoked machine was dialed"
        );
    }

    #[tokio::test]
    async fn a_dialed_link_closes_when_its_machine_is_revoked() {
        let (_, keys) = keys();
        let me = joined(&keys);
        let peer = joined(&keys);
        let inner = Arc::new(Pairs::default());
        let dialer = KeyedDialer::new(inner.clone(), me, keys.clone());
        let mut conn = dialer.dial(&machine(peer.machine)).await.unwrap();
        let mut far = inner.0.lock().unwrap().pop().unwrap();
        next(&mut far).await.unwrap();
        far.send.send("from the peer".into()).await.unwrap();
        assert_eq!(next(&mut conn).await.as_deref(), Some("from the peer"));
        keys.revoke(peer.machine).unwrap();
        closed(&mut conn).await;
    }

    // ---- joining over HTTP ----

    async fn serve(keys: Keys) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, router(keys)).await });
        port
    }

    async fn join_over_http(
        port: u16,
        code: String,
    ) -> (Arc<MemorySecrets>, Result<Identity, JoinError>) {
        let secrets = Arc::new(MemorySecrets::default());
        let saved = secrets.clone();
        let result = tokio::task::spawn_blocking(move || {
            join_at(&format!("127.0.0.1:{port}"), DEFAULT_PORT, &code, &*saved)
        })
        .await
        .unwrap();
        (secrets, result)
    }

    #[tokio::test]
    async fn a_machine_joins_with_a_code_and_keeps_its_key() {
        let (_, keys) = keys();
        let port = serve(keys.clone()).await;
        let code = keys.new_code().to_string();
        let (secrets, joined) = join_over_http(port, code.clone()).await;
        let joined = joined.unwrap();
        let held = identity(&*secrets).unwrap().expect("no identity kept");
        assert_eq!(held.machine, joined.machine);
        assert_eq!(keys.check(held.machine, Some(&held.key)), Ok(()));

        let (secrets, again) = join_over_http(port, code).await;
        assert!(matches!(again, Err(JoinError::BadCode)), "{again:?}");
        assert!(identity(&*secrets).unwrap().is_none());
    }

    #[test]
    fn a_join_address_may_name_its_port() {
        assert_eq!(
            join_url("100.64.0.1", 7433),
            "http://100.64.0.1:7433/api/join"
        );
        assert_eq!(
            join_url("100.64.0.1:7777", 7433),
            "http://100.64.0.1:7777/api/join"
        );
        assert_eq!(
            join_url("fd7a:115c:a1e0::1", 7433),
            "http://[fd7a:115c:a1e0::1]:7433/api/join"
        );
        assert_eq!(
            join_url("trilogy-mac", 7433),
            "http://trilogy-mac:7433/api/join"
        );
        assert_eq!(
            join_url("trilogy-mac:80", 7433),
            "http://trilogy-mac:80/api/join"
        );
    }
}
