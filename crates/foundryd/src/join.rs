//! One-time join codes and machine keys.
//!
//! A machine already in the fleet shows a one-time [`Code`] ([`Keys::new_code`]). The new
//! machine runs `foundryd join <address> <code>`, which posts the code to that machine's
//! [`router`]. The code works once and for [`CODE_VALID_FOR`]; redeeming it gives the new
//! machine an id and a key of its own ([`Identity`]), which it keeps in its secret store.
//!
//! Every peer link proves its machine's key. A dialed link sends this machine's key first
//! ([`KeyedDialer`]), and [`Keys::accept`] takes a link only when that key is the one held for
//! the machine its hello names and the machine is not revoked. A revoked machine is never
//! dialed, and an open link to one closes within [`RECHECK`].
//!
//! The key table is held in this machine's secret store. Carrying it, and the new machine's
//! place in the machine list, to every other machine belongs with the shared settings.

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use foundry_proto::ids::MachineId;
use foundry_proto::messages::PeerMessage;
use foundry_proto::settings::Machine;
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::time::{Instant, timeout};
use uuid::Uuid;

use crate::secrets::{self, Secrets, SecretsError};
use crate::sync::{Conn, Dialer, OFFLINE_AFTER, Peers};

/// How long a join code works after it is shown.
pub const CODE_VALID_FOR: Duration = Duration::from_secs(10 * 60);

/// How often an open link checks that its machine is still admitted.
pub const RECHECK: Duration = Duration::from_secs(1);

/// The path a joining machine posts its code to.
pub const JOIN_PATH: &str = "/api/join";

/// The port a machine's daemon listens on when `FOUNDRY_PORT` is not set.
pub const DEFAULT_PORT: u16 = 7433;

/// The secret that holds the key table: every machine's key, or that it is revoked.
const KEYS_SECRET: &str = "machine-keys";

/// The secret that holds this machine's own id and key.
const IDENTITY_SECRET: &str = "machine";

/// Letters and digits a person cannot misread: no 0, O, 1 or I. 32 of them, so one random
/// byte modulo 32 picks each without bias.
const CODE_LETTERS: &[u8; 32] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
const CODE_LENGTH: usize = 10;

/// `foundryd join <address> <code>`.
pub fn join(address: &str, code: &str) -> ExitCode {
    let port = match std::env::var("FOUNDRY_PORT") {
        Ok(port) => match port.parse() {
            Ok(port) => port,
            Err(_) => {
                eprintln!("foundryd join: FOUNDRY_PORT is not a port number: {port}");
                return ExitCode::FAILURE;
            }
        },
        Err(_) => DEFAULT_PORT,
    };
    let Some(home) = home() else {
        eprintln!("foundryd join: set FOUNDRY_HOME or HOME");
        return ExitCode::FAILURE;
    };
    match join_at(address, port, code, &*secrets::for_this_machine(&home)) {
        Ok(identity) => {
            println!("joined the fleet as machine {}", identity.machine);
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("foundryd join: {error}");
            ExitCode::FAILURE
        }
    }
}

/// The data directory: `FOUNDRY_HOME`, else `~/.local/share/foundry`.
fn home() -> Option<PathBuf> {
    match std::env::var_os("FOUNDRY_HOME") {
        Some(home) => Some(home.into()),
        None => std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share/foundry")),
    }
}

/// Posts `code` to the machine at `address` (`port` unless the address names one), and keeps
/// the identity it answers with in `secrets`.
pub fn join_at(
    address: &str,
    port: u16,
    code: &str,
    secrets: &dyn Secrets,
) -> Result<Identity, JoinError> {
    let url = join_url(address, port);
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(10)))
        .build()
        .into();
    let request = JoinRequest {
        code: code.to_owned(),
    };
    let identity: Identity = match agent.post(&url).send_json(&request) {
        Ok(mut response) => response
            .body_mut()
            .read_json()
            .map_err(|_| JoinError::BadAnswer(url.clone()))?,
        Err(ureq::Error::StatusCode(403)) => return Err(JoinError::BadCode),
        Err(error) => {
            return Err(JoinError::Unreachable {
                url,
                reason: error.to_string(),
            });
        }
    };
    let text = serde_json::to_string(&identity).map_err(|_| JoinError::BadAnswer(url))?;
    secrets.set(IDENTITY_SECRET, &text)?;
    Ok(identity)
}

/// The URL to post a join code to: `address` is an IP address or a host name, with or
/// without a port.
fn join_url(address: &str, port: u16) -> String {
    if let Ok(socket) = address.parse::<SocketAddr>() {
        return format!("http://{socket}{JOIN_PATH}");
    }
    match address.parse::<IpAddr>() {
        Ok(ip) => format!("http://{}{JOIN_PATH}", SocketAddr::new(ip, port)),
        Err(_) if address.contains(':') => format!("http://{address}{JOIN_PATH}"),
        Err(_) => format!("http://{address}:{port}{JOIN_PATH}"),
    }
}

/// This machine's id and key, if it has joined a fleet.
pub fn identity(secrets: &dyn Secrets) -> Result<Option<Identity>, JoinError> {
    match secrets.get(IDENTITY_SECRET)? {
        None => Ok(None),
        Some(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|_| JoinError::Unreadable(IDENTITY_SECRET)),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum JoinError {
    #[error("the code is wrong, used or expired; ask for a new one")]
    BadCode,
    #[error("{url} could not be reached: {reason}")]
    Unreachable { url: String, reason: String },
    #[error("{0} answered with something that is not an identity")]
    BadAnswer(String),
    #[error("the secret {0} is not in the expected form")]
    Unreadable(&'static str),
    #[error(transparent)]
    Secrets(#[from] SecretsError),
}

/// Why a machine's link is refused. None of these names the key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refused {
    #[error("it sent no key")]
    NoKey,
    #[error("this machine holds no key for it")]
    Unknown,
    #[error("it is revoked")]
    Revoked,
    #[error("its key is wrong")]
    WrongKey,
}

/// What this machine holds for another machine's key, as the UI shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyState {
    Active,
    Revoked,
    NoKey,
}

/// A machine's key. Its value is never shown in debug output.
#[derive(Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MachineKey(String);

impl MachineKey {
    fn random() -> Self {
        Self(format!(
            "{}{}",
            Uuid::new_v4().simple(),
            Uuid::new_v4().simple()
        ))
    }

    /// Compares in time that does not depend on where the keys differ.
    fn matches(&self, other: &Self) -> bool {
        same(self.0.as_bytes(), other.0.as_bytes())
    }
}

impl fmt::Debug for MachineKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MachineKey(..)")
    }
}

/// A machine's id and key, as joining gives them.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Identity {
    pub machine: MachineId,
    pub key: MachineKey,
}

/// A one-time join code, shown to a person. It is never shown in debug output.
pub struct Code(String);

impl fmt::Display for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Code(..)")
    }
}

/// What this machine holds for another: its key, or that it is revoked.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum Entry {
    Active { key: MachineKey },
    Revoked,
}

/// The join codes this machine has shown and the keys of the machines it admits.
#[derive(Clone)]
pub struct Keys {
    inner: Arc<KeysInner>,
}

struct KeysInner {
    secrets: Arc<dyn Secrets>,
    held: Mutex<Held>,
    /// Held across each change to the key table and its save, so saves land in order and a
    /// slow secret store never holds up a link's check.
    saving: Mutex<()>,
}

struct Held {
    codes: Vec<(String, Instant)>,
    machines: BTreeMap<MachineId, Entry>,
}

impl Keys {
    /// The key table kept in `secrets`, with no codes outstanding.
    pub fn load(secrets: Arc<dyn Secrets>) -> Result<Self, JoinError> {
        let machines = match secrets.get(KEYS_SECRET)? {
            None => BTreeMap::new(),
            Some(text) => {
                serde_json::from_str(&text).map_err(|_| JoinError::Unreadable(KEYS_SECRET))?
            }
        };
        Ok(Self {
            inner: Arc::new(KeysInner {
                secrets,
                held: Mutex::new(Held {
                    codes: Vec::new(),
                    machines,
                }),
                saving: Mutex::new(()),
            }),
        })
    }

    /// A new code that works once within [`CODE_VALID_FOR`].
    pub fn new_code(&self) -> Code {
        let random = Uuid::new_v4();
        let bytes = random.as_bytes();
        // Bytes 6 and 8 carry the UUID's version and variant bits, so they are skipped.
        let code: String = [0, 1, 2, 3, 4, 5, 9, 10, 11, 12]
            .iter()
            .take(CODE_LENGTH)
            .map(|&i| char::from(CODE_LETTERS[usize::from(bytes[i]) % CODE_LETTERS.len()]))
            .collect();
        let mut held = self.held();
        held.forget_expired_codes();
        held.codes.push((code.clone(), Instant::now()));
        Code(code)
    }

    /// Uses up `code` and admits a new machine with a key of its own.
    pub fn redeem(&self, code: &str) -> Result<Identity, JoinError> {
        let typed = code.trim().to_ascii_uppercase();
        {
            let mut held = self.held();
            held.forget_expired_codes();
            let found = held
                .codes
                .iter()
                .position(|(code, _)| same(code.as_bytes(), typed.as_bytes()))
                .ok_or(JoinError::BadCode)?;
            held.codes.remove(found);
        }
        let identity = Identity {
            machine: MachineId::random(),
            key: MachineKey::random(),
        };
        let key = identity.key.clone();
        self.change(|machines| {
            machines.insert(identity.machine, Entry::Active { key });
        })?;
        Ok(identity)
    }

    /// Revokes `machine`: its key is forgotten and every link with it is refused.
    pub fn revoke(&self, machine: MachineId) -> Result<(), SecretsError> {
        self.change(|machines| {
            machines.insert(machine, Entry::Revoked);
        })
    }

    /// What this machine holds for `machine`.
    pub fn state(&self, machine: MachineId) -> KeyState {
        match self.held().machines.get(&machine) {
            Some(Entry::Active { .. }) => KeyState::Active,
            Some(Entry::Revoked) => KeyState::Revoked,
            None => KeyState::NoKey,
        }
    }

    /// Whether `key` proves `machine`.
    pub fn check(&self, machine: MachineId, key: Option<&MachineKey>) -> Result<(), Refused> {
        let key = key.ok_or(Refused::NoKey)?;
        match self.held().machines.get(&machine) {
            None => Err(Refused::Unknown),
            Some(Entry::Revoked) => Err(Refused::Revoked),
            Some(Entry::Active { key: held }) if held.matches(key) => Ok(()),
            Some(Entry::Active { .. }) => Err(Refused::WrongKey),
        }
    }

    /// Serves a link a peer opened, as [`Peers::accept`] does, once its first message proves
    /// the key of the machine its hello names. The link closes when that machine is revoked.
    pub async fn accept(&self, peers: &Peers, conn: Conn) {
        let Conn { send, mut recv } = conn;
        let Some((machine, key, hello)) = self.admit(&mut recv).await else {
            return;
        };
        let (near, far) = Conn::pair();
        if near.send.send(hello).await.is_err() {
            return;
        }
        let keys = self.clone();
        let admitted = move || match keys.check(machine, Some(&key)) {
            Ok(()) => true,
            Err(refused) => {
                tracing::info!("join: link from {machine} closed: {refused}");
                false
            }
        };
        tokio::select! {
            () = peers.accept(far) => {}
            () = relay(Conn { send, recv }, near, admitted) => {}
        }
    }

    /// Reads the key message and the hello that follows it, and returns the machine they
    /// prove, its key and the hello to pass on.
    async fn admit(&self, recv: &mut Receiver<String>) -> Option<(MachineId, MachineKey, String)> {
        let first = timeout(OFFLINE_AFTER, recv.recv()).await.ok()??;
        let Ok(KeyMessage { machine, key }) = serde_json::from_str(&first) else {
            tracing::warn!("join: refused a link: {}", Refused::NoKey);
            return None;
        };
        if let Err(refused) = self.check(machine, Some(&key)) {
            tracing::warn!("join: refused a link from {machine}: {refused}");
            return None;
        }
        let hello = timeout(OFFLINE_AFTER, recv.recv()).await.ok()??;
        match serde_json::from_str(&hello) {
            Ok(PeerMessage::Hello { machine: named, .. }) if named == machine => {
                Some((machine, key, hello))
            }
            _ => {
                tracing::warn!("join: refused a link from {machine}: its hello names another");
                None
            }
        }
    }

    /// Applies `edit` to a copy of the key table, saves the copy, then holds it.
    fn change(
        &self,
        edit: impl FnOnce(&mut BTreeMap<MachineId, Entry>),
    ) -> Result<(), SecretsError> {
        let _saving = lock(&self.inner.saving);
        let mut machines = self.held().machines.clone();
        edit(&mut machines);
        // A key table always serializes: its keys are UUIDs and its values plain strings.
        let text = serde_json::to_string(&machines).unwrap_or_default();
        self.inner.secrets.set(KEYS_SECRET, &text)?;
        self.held().machines = machines;
        Ok(())
    }

    fn held(&self) -> MutexGuard<'_, Held> {
        lock(&self.inner.held)
    }
}

impl Held {
    fn forget_expired_codes(&mut self) {
        self.codes
            .retain(|(_, shown)| shown.elapsed() < CODE_VALID_FOR);
    }
}

/// The first message on a link: who is dialing and its key.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename = "key")]
struct KeyMessage {
    machine: MachineId,
    key: MachineKey,
}

fn key_message(identity: &Identity) -> String {
    // A machine id and a plain string always serialize.
    serde_json::to_string(&KeyMessage {
        machine: identity.machine,
        key: identity.key.clone(),
    })
    .unwrap_or_default()
}

/// Opens links with `dialer`, sending this machine's key first. A revoked machine is not
/// dialed, and a link to one closes within [`RECHECK`] of its revocation.
pub struct KeyedDialer {
    dialer: Arc<dyn Dialer>,
    me: Identity,
    keys: Keys,
}

impl KeyedDialer {
    pub fn new(dialer: Arc<dyn Dialer>, me: Identity, keys: Keys) -> Self {
        Self { dialer, me, keys }
    }
}

impl Dialer for KeyedDialer {
    fn dial(&self, peer: &Machine) -> BoxFuture<'static, io::Result<Conn>> {
        if self.keys.state(peer.id) == KeyState::Revoked {
            let refused = io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{} is revoked", peer.name),
            );
            return Box::pin(async move { Err(refused) });
        }
        let dialing = self.dialer.dial(peer);
        let message = key_message(&self.me);
        let (keys, id) = (self.keys.clone(), peer.id);
        Box::pin(async move {
            let conn = dialing.await?;
            conn.send
                .send(message)
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "the link closed"))?;
            let (near, far) = Conn::pair();
            let admitted = move || {
                let revoked = keys.state(id) == KeyState::Revoked;
                if revoked {
                    tracing::info!("join: link to {id} closed: {}", Refused::Revoked);
                }
                !revoked
            };
            tokio::spawn(relay(conn, near, admitted));
            Ok(far)
        })
    }
}

/// Carries messages both ways between `outer` and `inner` until either closes or
/// `admitted` says no, checked every [`RECHECK`]. The two directions run side by side, so a
/// send that waits on one end never stops the other.
async fn relay(outer: Conn, inner: Conn, admitted: impl Fn() -> bool) {
    let Conn {
        send: out_send,
        recv: mut out_recv,
    } = outer;
    let Conn {
        send: in_send,
        recv: mut in_recv,
    } = inner;
    let watch = async {
        let mut every = tokio::time::interval(RECHECK);
        loop {
            every.tick().await;
            if !admitted() {
                return;
            }
        }
    };
    tokio::select! {
        () = forward(&mut out_recv, &in_send) => {}
        () = forward(&mut in_recv, &out_send) => {}
        () = watch => {}
    }
}

async fn forward(from: &mut Receiver<String>, to: &Sender<String>) {
    while let Some(text) = from.recv().await {
        if to.send(text).await.is_err() {
            return;
        }
    }
}

/// The route a joining machine posts its code to. It answers 403 to a code that is wrong,
/// used or expired.
pub fn router(keys: Keys) -> Router {
    Router::new()
        .route(JOIN_PATH, post(redeem))
        .with_state(keys)
}

#[derive(Serialize, Deserialize)]
struct JoinRequest {
    code: String,
}

async fn redeem(State(keys): State<Keys>, Json(request): Json<JoinRequest>) -> Response {
    // Saving the key table can wait on the keychain or the disk.
    match tokio::task::spawn_blocking(move || keys.redeem(&request.code)).await {
        Ok(Ok(identity)) => Json(identity).into_response(),
        Ok(Err(JoinError::BadCode)) => {
            (StatusCode::FORBIDDEN, JoinError::BadCode.to_string()).into_response()
        }
        Ok(Err(error)) => {
            tracing::warn!("join: a machine could not be admitted: {error}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "the new machine's key could not be stored",
            )
                .into_response()
        }
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// Compares two byte strings in time that depends only on their lengths.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |differ, (x, y)| differ | (x ^ y)) == 0
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use foundry_proto::ids::MachineId;
    use foundry_proto::messages::{PROTOCOL, PeerMessage};
    use foundry_proto::settings::{Machine, SharedSettings};
    use futures::future::BoxFuture;
    use tokio::time::timeout;

    use super::*;
    use crate::secrets::{MemorySecrets, Secrets, SecretsError};
    use crate::store::Store;
    use crate::sync::{Conn, Dialer, Peers};

    /// A machine's keys over a fresh in-memory secret store.
    fn machine_keys() -> Keys {
        Keys::open(Arc::new(MemorySecrets::default())).unwrap()
    }

    /// `host` admits `peer` with a fresh code, as `foundryd join` does.
    fn admit(host: &Keys, peer: &Keys) {
        let code = host.new_code().unwrap().to_string();
        host.redeem(&code, peer.me(), &peer.public_key()).unwrap();
    }

    /// Two machines that hold each other's key, as they will once keys reach every machine.
    fn pair() -> (Keys, Keys) {
        let (a, b) = (machine_keys(), machine_keys());
        admit(&a, &b);
        admit(&b, &a);
        (a, b)
    }

    /// A secret store whose writes fail while `failing` is set.
    #[derive(Default)]
    struct Flaky {
        inner: MemorySecrets,
        failing: AtomicBool,
    }

    impl Secrets for Flaky {
        fn get(&self, name: &str) -> Result<Option<String>, SecretsError> {
            self.inner.get(name)
        }
        fn set(&self, name: &str, value: &str) -> Result<(), SecretsError> {
            if self.failing.load(Ordering::SeqCst) {
                return Err(SecretsError::Keychain {
                    action: "store",
                    name: name.to_owned(),
                    reason: "test".into(),
                });
            }
            self.inner.set(name, value)
        }
    }

    // ---- codes and keys ----

    #[tokio::test(start_paused = true)]
    async fn a_code_works_once() {
        let (host, peer, other) = (machine_keys(), machine_keys(), machine_keys());
        let code = host.new_code().unwrap().to_string();
        host.redeem(&code, peer.me(), &peer.public_key()).unwrap();
        assert_eq!(host.state(peer.me()), KeyState::Active);
        let again = host.redeem(&code, other.me(), &other.public_key());
        assert!(matches!(again, Err(JoinError::BadCode)), "{again:?}");
        assert_eq!(host.state(other.me()), KeyState::NoKey);
    }

    #[tokio::test(start_paused = true)]
    async fn the_machine_that_used_a_code_may_send_it_again_when_the_answer_was_lost() {
        let (host, peer) = (machine_keys(), machine_keys());
        let code = host.new_code().unwrap().to_string();
        host.redeem(&code, peer.me(), &peer.public_key()).unwrap();
        host.redeem(&code, peer.me(), &peer.public_key()).unwrap();
        assert_eq!(host.state(peer.me()), KeyState::Active);
    }

    #[tokio::test(start_paused = true)]
    async fn a_code_works_for_ten_minutes_and_not_after() {
        let (host, early, late) = (machine_keys(), machine_keys(), machine_keys());
        let early_code = host.new_code().unwrap().to_string();
        let late_code = host.new_code().unwrap().to_string();
        tokio::time::advance(CODE_VALID_FOR - Duration::from_secs(1)).await;
        host.redeem(&early_code, early.me(), &early.public_key())
            .unwrap();
        tokio::time::advance(Duration::from_secs(1)).await;
        let expired = host.redeem(&late_code, late.me(), &late.public_key());
        assert!(matches!(expired, Err(JoinError::BadCode)), "{expired:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_code_typed_in_lower_case_with_spaces_around_it_works() {
        let (host, peer) = (machine_keys(), machine_keys());
        let code = host.new_code().unwrap().to_string();
        let typed = format!(" {} ", code.to_lowercase());
        host.redeem(&typed, peer.me(), &peer.public_key()).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_code_never_issued_is_refused() {
        let (host, peer) = (machine_keys(), machine_keys());
        host.new_code().unwrap();
        for wrong in ["ABCDEFGHJK", ""] {
            let refused = host.redeem(wrong, peer.me(), &peer.public_key());
            assert!(matches!(refused, Err(JoinError::BadCode)), "{wrong}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn codes_and_machines_differ_each_time() {
        let host = machine_keys();
        let codes: Vec<String> = (0..20)
            .map(|_| host.new_code().unwrap().to_string())
            .collect();
        for code in &codes {
            assert_eq!(code.len(), 10, "{code}");
            assert!(code.bytes().all(|b| CODE_LETTERS.contains(&b)), "{code}");
        }
        let distinct: std::collections::BTreeSet<_> = codes.iter().collect();
        assert_eq!(distinct.len(), codes.len());
        let (a, b) = (machine_keys(), machine_keys());
        assert_ne!(a.me(), b.me());
        assert_ne!(a.public_key().0, b.public_key().0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_code_cannot_claim_a_machine_this_one_already_holds() {
        let (host, peer, thief) = (machine_keys(), machine_keys(), machine_keys());
        admit(&host, &peer);
        for taken in [peer.me(), host.me()] {
            let code = host.new_code().unwrap().to_string();
            let claimed = host.redeem(&code, taken, &thief.public_key());
            assert!(matches!(claimed, Err(JoinError::Taken(_))), "{claimed:?}");
        }
        host.revoke(peer.me()).unwrap();
        let code = host.new_code().unwrap().to_string();
        let rejoined = host.redeem(&code, peer.me(), &peer.public_key());
        assert!(matches!(rejoined, Err(JoinError::Taken(_))), "{rejoined:?}");
        assert_eq!(host.state(peer.me()), KeyState::Revoked);
    }

    #[tokio::test(start_paused = true)]
    async fn a_revoked_machine_is_shown_as_revoked() {
        let (host, peer) = (machine_keys(), machine_keys());
        admit(&host, &peer);
        assert_eq!(host.state(peer.me()), KeyState::Active);
        host.revoke(peer.me()).unwrap();
        assert_eq!(host.state(peer.me()), KeyState::Revoked);
        assert_eq!(host.state(MachineId::random()), KeyState::NoKey);
    }

    #[tokio::test(start_paused = true)]
    async fn a_machine_keeps_its_identity_keys_and_revocations_across_a_restart() {
        let secrets = Arc::new(MemorySecrets::default());
        let host = Keys::open(secrets.clone()).unwrap();
        let (kept, revoked) = (machine_keys(), machine_keys());
        admit(&host, &kept);
        admit(&host, &revoked);
        host.revoke(revoked.me()).unwrap();
        let again = Keys::open(secrets).unwrap();
        assert_eq!(again.me(), host.me());
        assert_eq!(again.public_key().0, host.public_key().0);
        assert_eq!(again.state(again.me()), KeyState::Active);
        assert_eq!(again.state(kept.me()), KeyState::Active);
        assert_eq!(again.state(revoked.me()), KeyState::Revoked);
    }

    #[tokio::test(start_paused = true)]
    async fn the_first_machine_holds_its_own_key() {
        let founder = machine_keys();
        assert_eq!(founder.state(founder.me()), KeyState::Active);
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_save_admits_nobody_and_leaves_the_code_unspent() {
        let flaky = Arc::new(Flaky::default());
        let host = Keys::open(flaky.clone()).unwrap();
        let peer = machine_keys();
        let code = host.new_code().unwrap().to_string();
        flaky.failing.store(true, Ordering::SeqCst);
        let failed = host.redeem(&code, peer.me(), &peer.public_key());
        assert!(matches!(failed, Err(JoinError::Secrets(_))), "{failed:?}");
        assert_eq!(host.state(peer.me()), KeyState::NoKey);
        flaky.failing.store(false, Ordering::SeqCst);
        host.redeem(&code, peer.me(), &peer.public_key()).unwrap();
        assert_eq!(host.state(peer.me()), KeyState::Active);
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_save_leaves_a_machine_unrevoked() {
        let flaky = Arc::new(Flaky::default());
        let host = Keys::open(flaky.clone()).unwrap();
        let peer = machine_keys();
        admit(&host, &peer);
        flaky.failing.store(true, Ordering::SeqCst);
        assert!(host.revoke(peer.me()).is_err());
        assert_eq!(host.state(peer.me()), KeyState::Active);
        let reopened = Keys::open(flaky.clone()).unwrap();
        assert_eq!(reopened.state(peer.me()), KeyState::Active);
    }

    #[test]
    fn machines_admitted_at_the_same_time_are_all_kept() {
        let secrets = Arc::new(MemorySecrets::default());
        let host = Keys::open(secrets.clone()).unwrap();
        let peers: Vec<Keys> = (0..16).map(|_| machine_keys()).collect();
        let codes: Vec<String> = peers
            .iter()
            .map(|_| host.new_code().unwrap().to_string())
            .collect();
        std::thread::scope(|scope| {
            for (peer, code) in peers.iter().zip(&codes) {
                let host = host.clone();
                scope.spawn(move || host.redeem(code, peer.me(), &peer.public_key()).unwrap());
            }
        });
        let reopened = Keys::open(secrets).unwrap();
        for peer in &peers {
            assert_eq!(reopened.state(peer.me()), KeyState::Active);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_private_key_or_code_never_shows_in_debug_output() {
        let secrets = Arc::new(MemorySecrets::default());
        let keys = Keys::open(secrets.clone()).unwrap();
        let code = keys.new_code().unwrap();
        assert!(!format!("{code:?}").contains(&code.to_string()));
        let stored: serde_json::Value =
            serde_json::from_str(&secrets.get(IDENTITY_SECRET).unwrap().unwrap()).unwrap();
        let private = stored["key"].as_str().unwrap().to_owned();
        assert!(private.len() >= 64, "{private}");
        let identity = Identity::load_or_create(&*secrets).unwrap();
        assert!(!format!("{identity:?}").contains(&private));
        let table = secrets.get(KEYS_SECRET).unwrap().unwrap();
        assert!(
            !table.contains(&private),
            "the key table holds a private key"
        );
    }

    // ---- peer links ----

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

    /// A dialer whose links are served by another machine's [`Keys::accept`].
    struct Direct {
        keys: Keys,
        peers: Peers,
    }

    impl Dialer for Direct {
        fn dial(&self, _peer: &Machine) -> BoxFuture<'static, io::Result<Conn>> {
            let (near, far) = Conn::pair();
            let (keys, peers) = (self.keys.clone(), self.peers.clone());
            tokio::spawn(async move { keys.accept(&peers, far).await });
            Box::pin(async move { Ok(near) })
        }
    }

    /// `host` serving links, with `others` in its machine list.
    struct Host {
        keys: Keys,
        peers: Peers,
        _dir: tempfile::TempDir,
    }

    fn host(keys: &Keys, others: &[MachineId]) -> Host {
        let (dir, peers) = peers(keys.me(), others);
        Host {
            keys: keys.clone(),
            peers,
            _dir: dir,
        }
    }

    impl Host {
        /// `from` dialing this host through its [`KeyedDialer`].
        async fn dialed_by(&self, from: &Keys) -> io::Result<Conn> {
            let direct = Direct {
                keys: self.keys.clone(),
                peers: self.peers.clone(),
            };
            KeyedDialer::new(Arc::new(direct), from.clone())
                .dial(&machine(self.keys.me()))
                .await
        }

        /// A raw link into this host, for a test that speaks the handshake itself.
        fn raw(&self) -> Conn {
            let (near, far) = Conn::pair();
            let (keys, peers) = (self.keys.clone(), self.peers.clone());
            tokio::spawn(async move { keys.accept(&peers, far).await });
            near
        }
    }

    fn hello(machine: MachineId) -> String {
        serde_json::to_string(&PeerMessage::Hello {
            protocol: PROTOCOL,
            machine,
            versions: Default::default(),
        })
        .unwrap()
    }

    fn is_hello(text: &str) -> bool {
        matches!(serde_json::from_str(text), Ok(PeerMessage::Hello { .. }))
    }

    /// The next message on `conn`, or `None` once the link is closed.
    async fn next(conn: &mut Conn) -> Option<String> {
        timeout(Duration::from_secs(5), conn.recv.recv())
            .await
            .expect("the link neither answered nor closed")
    }

    /// Waits up to 5 seconds for the link to close, skipping what it carries.
    async fn closed(conn: &mut Conn) {
        timeout(Duration::from_secs(5), async {
            while conn.recv.recv().await.is_some() {}
        })
        .await
        .expect("the link is still open");
    }

    /// Sends a hello and waits for the host's, so the link is known to be served.
    async fn served(conn: &mut Conn, me: MachineId) {
        conn.send.send(hello(me)).await.unwrap();
        let answer = next(conn).await.expect("the link was refused");
        assert!(is_hello(&answer), "{answer}");
    }

    fn send(conn: &Conn, message: &Handshake) -> impl Future<Output = ()> + '_ {
        let text = serde_json::to_string(message).unwrap();
        async move { conn.send.send(text).await.unwrap() }
    }

    async fn read(conn: &mut Conn) -> Handshake {
        serde_json::from_str(&next(conn).await.expect("the link closed")).unwrap()
    }

    #[tokio::test]
    async fn a_link_between_machines_that_hold_each_others_keys_is_served() {
        let (h, p) = pair();
        let host = host(&h, &[p.me()]);
        let mut conn = host.dialed_by(&p).await.unwrap();
        served(&mut conn, p.me()).await;
        let heartbeat = next(&mut conn).await.expect("the link closed");
        assert!(!is_hello(&heartbeat), "{heartbeat}");
    }

    #[tokio::test]
    async fn a_link_with_no_key_is_refused() {
        let (h, p) = pair();
        let host = host(&h, &[p.me()]);
        let mut conn = host.raw();
        conn.send.send(hello(p.me())).await.unwrap();
        closed(&mut conn).await;
        assert!(!host.peers.online().contains(&p.me()));
    }

    #[tokio::test]
    async fn a_link_from_a_revoked_machine_is_refused_and_it_shows_as_revoked() {
        let (h, p) = pair();
        h.revoke(p.me()).unwrap();
        let host = host(&h, &[p.me()]);
        assert!(host.dialed_by(&p).await.is_err());
        assert_eq!(h.state(p.me()), KeyState::Revoked);
        assert!(!host.peers.online().contains(&p.me()));
    }

    #[tokio::test]
    async fn a_link_from_a_machine_this_one_holds_no_key_for_is_refused() {
        let (h, p) = (machine_keys(), machine_keys());
        admit(&p, &h);
        let host = host(&h, &[p.me()]);
        assert!(host.dialed_by(&p).await.is_err());
    }

    #[tokio::test]
    async fn a_link_whose_proof_is_signed_with_another_key_is_refused() {
        let (h, p) = pair();
        let impostor = machine_keys();
        let host = host(&h, &[p.me()]);
        let mut conn = host.raw();
        send(
            &conn,
            &Handshake::Challenge {
                machine: p.me(),
                nonce: nonce().unwrap(),
            },
        )
        .await;
        let Handshake::Answer { nonce, .. } = read(&mut conn).await else {
            panic!("no answer");
        };
        let proof = impostor.sign(&link_message(DIALING, p.me(), h.me(), &nonce));
        send(&conn, &Handshake::Proof { proof }).await;
        conn.send.send(hello(p.me())).await.unwrap();
        closed(&mut conn).await;
    }

    #[tokio::test]
    async fn a_proof_a_machine_gave_one_peer_cannot_open_a_link_to_another() {
        // H holds A and R; A holds R and dials it. R, revoked on H, tries to pass as A.
        let (h, a, r) = (machine_keys(), machine_keys(), machine_keys());
        admit(&h, &a);
        admit(&h, &r);
        admit(&a, &h);
        admit(&a, &r);
        admit(&r, &a);
        h.revoke(r.me()).unwrap();
        let host = host(&h, &[a.me(), r.me()]);

        // R opens a link to H naming A, and keeps H's challenge.
        let mut to_h = host.raw();
        send(
            &to_h,
            &Handshake::Challenge {
                machine: a.me(),
                nonce: nonce().unwrap(),
            },
        )
        .await;
        let Handshake::Answer { nonce: h_nonce, .. } = read(&mut to_h).await else {
            panic!("H did not answer");
        };

        // A dials R, and R answers with H's challenge so that A signs it.
        let inner = Arc::new(Pairs::default());
        let dialer = KeyedDialer::new(inner.clone(), a.clone());
        let dialing = tokio::spawn(async move { dialer.dial(&machine(r.me())).await.map(drop) });
        let mut at_r = inner_far(&inner).await;
        let Handshake::Challenge { nonce: a_nonce, .. } = read(&mut at_r).await else {
            panic!("A sent no challenge");
        };
        let r_proof = r.sign(&link_message(ACCEPTING, r.me(), a.me(), &a_nonce));
        send(
            &at_r,
            &Handshake::Answer {
                machine: r.me(),
                nonce: h_nonce,
                proof: r_proof,
            },
        )
        .await;
        let Handshake::Proof { proof: a_proof } = read(&mut at_r).await else {
            panic!("A sent no proof");
        };
        dialing.await.unwrap().unwrap();

        // R hands A's proof to H.
        send(&to_h, &Handshake::Proof { proof: a_proof }).await;
        to_h.send.send(hello(a.me())).await.unwrap();
        closed(&mut to_h).await;
        assert!(!host.peers.online().contains(&a.me()));
    }

    /// The far end of the link `pairs` hands out once it has dialed.
    async fn inner_far(pairs: &Pairs) -> Conn {
        timeout(Duration::from_secs(5), async {
            loop {
                if let Some(far) = pairs.0.lock().unwrap().pop() {
                    return far;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("nothing was dialed")
    }

    #[tokio::test]
    async fn a_dialed_machine_that_cannot_prove_its_key_is_dropped() {
        let (a, b) = pair();
        let impostor = machine_keys();
        let inner = Arc::new(Pairs::default());
        let dialer = KeyedDialer::new(inner.clone(), a.clone());
        let b_id = b.me();
        let dialing = tokio::spawn(async move { dialer.dial(&machine(b_id)).await.map(drop) });
        let mut far = inner_far(&inner).await;
        let Handshake::Challenge { nonce, .. } = read(&mut far).await else {
            panic!("no challenge");
        };
        let proof = impostor.sign(&link_message(ACCEPTING, b_id, a.me(), &nonce));
        send(
            &far,
            &Handshake::Answer {
                machine: b_id,
                nonce: nonce.clone(),
                proof,
            },
        )
        .await;
        assert!(dialing.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn a_machine_this_one_holds_no_active_key_for_is_not_dialed() {
        let (a, b) = pair();
        let unknown = machine_keys();
        a.revoke(b.me()).unwrap();
        let inner = Arc::new(Pairs::default());
        let dialer = KeyedDialer::new(inner.clone(), a);
        for peer in [b.me(), unknown.me()] {
            assert!(dialer.dial(&machine(peer)).await.is_err());
        }
        assert!(inner.0.lock().unwrap().is_empty(), "a machine was dialed");
    }

    #[tokio::test]
    async fn an_open_link_closes_when_the_dialing_machine_is_revoked() {
        let (h, p) = pair();
        let host = host(&h, &[p.me()]);
        let mut conn = host.dialed_by(&p).await.unwrap();
        served(&mut conn, p.me()).await;
        h.revoke(p.me()).unwrap();
        closed(&mut conn).await;
    }

    #[tokio::test]
    async fn an_open_link_closes_when_the_dialed_machine_is_revoked() {
        let (h, p) = pair();
        let host = host(&h, &[p.me()]);
        let mut conn = host.dialed_by(&p).await.unwrap();
        served(&mut conn, p.me()).await;
        p.revoke(h.me()).unwrap();
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
        code: &str,
        secrets: Arc<MemorySecrets>,
    ) -> Result<MachineId, JoinError> {
        let code = code.to_owned();
        tokio::task::spawn_blocking(move || {
            join_at(&format!("127.0.0.1:{port}"), DEFAULT_PORT, &code, &*secrets)
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn a_machine_joins_with_a_code_and_keeps_its_own_identity() {
        let host = machine_keys();
        let port = serve(host.clone()).await;
        let code = host.new_code().unwrap().to_string();
        let secrets = Arc::new(MemorySecrets::default());
        let joined = join_over_http(port, &code, secrets.clone()).await.unwrap();
        let me = Keys::open(secrets.clone()).unwrap();
        assert_eq!(me.me(), joined);
        assert_eq!(host.state(joined), KeyState::Active);

        // A retry after a lost answer is the same machine, admitted again.
        assert_eq!(
            join_over_http(port, &code, secrets.clone()).await.unwrap(),
            joined
        );
        assert_eq!(Keys::open(secrets).unwrap().me(), joined);

        let other = Arc::new(MemorySecrets::default());
        let refused = join_over_http(port, &code, other).await;
        assert!(matches!(refused, Err(JoinError::BadCode)), "{refused:?}");
    }

    #[tokio::test]
    async fn a_host_that_cannot_store_the_key_is_not_reported_as_unreachable() {
        let flaky = Arc::new(Flaky::default());
        let host = Keys::open(flaky.clone()).unwrap();
        let port = serve(host.clone()).await;
        let code = host.new_code().unwrap().to_string();
        flaky.failing.store(true, Ordering::SeqCst);
        let failed = join_over_http(port, &code, Arc::new(MemorySecrets::default())).await;
        assert!(
            matches!(failed, Err(JoinError::NotAdmitted { .. })),
            "{failed:?}"
        );
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
            join_url("[fd7a:115c:a1e0::1]", 7433),
            "http://[fd7a:115c:a1e0::1]:7433/api/join"
        );
        assert_eq!(
            join_url("[fd7a:115c:a1e0::1]:7777", 7433),
            "http://[fd7a:115c:a1e0::1]:7777/api/join"
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
