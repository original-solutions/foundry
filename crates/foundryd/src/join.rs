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
        let heartbeat = next(&mut far)
            .await
            .expect("the link closed before the revoke");
        assert!(!is_hello(&heartbeat), "{heartbeat}");
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
