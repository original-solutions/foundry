//! One-time join codes and machine keys.
//!
//! Every machine has an identity of its own: an id and an Ed25519 key pair, made the first
//! time it is needed and kept in its secret store. The private key never leaves the machine;
//! other machines hold only its public key, in their key table.
//!
//! A machine already in the fleet shows a one-time [`Code`] ([`Keys::new_code`]). The new
//! machine runs `foundryd join <address> <code>`, which posts the code with its id and public
//! key to that machine's [`router`]. The code works once and for [`CODE_VALID_FOR`]. The code
//! travels as plain HTTP, so `join` sends it only to a Tailscale or loopback address.
//!
//! Every peer link proves both machines' keys before sync takes it. The dialing end names
//! itself with a fresh challenge; the accepting end answers with its own challenge and a
//! signature over the dialer's; the dialer answers with a signature over the acceptor's. Each
//! signature covers its role, both machine ids and the other end's fresh challenge, so a proof
//! given on one link opens no other. A machine whose key this one does not hold, or has
//! revoked, is neither dialed nor served, and an open link to one closes within [`RECHECK`] of
//! its revocation.
//!
//! The proofs show who is at each end when the link opens; they do not protect what the link
//! carries after. That rests on the network the daemon binds to, Tailscale, which encrypts
//! every packet and ties each address to one machine. A machine address that is not on it is
//! not safe for a peer link.
//!
//! The key table is held in this machine's secret store. It holds only public keys, so it can
//! be carried to every other machine; carrying it, and the new machine's place in the machine
//! list, belongs with the shared settings.

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError};
use std::time::{Duration, SystemTime};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use foundry_proto::ids::MachineId;
use foundry_proto::messages::PeerMessage;
use foundry_proto::settings::Machine;
use futures::future::BoxFuture;
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::time::{Instant, timeout};

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

/// The secret that holds the key table: every machine's public key, or that it is revoked.
pub(crate) const KEYS_SECRET: &str = "machine-keys";

/// The secret that holds this machine's own id and private key.
pub(crate) const IDENTITY_SECRET: &str = "machine";

/// Letters and digits a person cannot misread: no 0, O, 1 or I. 32 of them, so one random
/// byte modulo 32 picks each without bias.
const CODE_LETTERS: &[u8; 32] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// Letters in a join code: 50 random bits.
const CODE_LENGTH: usize = 10;

/// Random bytes in a link challenge.
const NONCE_LENGTH: usize = 32;

/// The roles a link proof is signed in, so an acceptor's proof never passes as a dialer's.
const ACCEPTING: &str = "accepting";
const DIALING: &str = "dialing";

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
    let secrets = match secrets::for_this_machine(&home) {
        Ok(secrets) => secrets,
        Err(error) => {
            eprintln!("foundryd join: {error}");
            return ExitCode::FAILURE;
        }
    };
    match join_at(address, port, code, &*secrets) {
        Ok(machine) => {
            println!(
                "joined the fleet as machine {machine}; if foundryd is already running here, \
                 restart it to link with the machine that admitted this one"
            );
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

/// Posts `code` with this machine's id and public key to the machine at `address` (`port`
/// unless the address names one), keeps the admitting machine's key in this machine's key
/// table, and returns this machine's id. The identity in `secrets` is
/// made if there is none and never replaced, so joining again, or retrying a join whose answer
/// was lost, is the same machine.
///
/// The code and the answer travel as plain HTTP, which only Tailscale encrypts and
/// authenticates, so `address` must be or resolve to a Tailscale address (or loopback); any
/// other is refused before anything is kept or sent. A name with several such addresses is
/// tried at each in turn until one is reached. This blocks on the network and the secret store.
pub fn join_at(
    address: &str,
    port: u16,
    code: &str,
    secrets: &dyn Secrets,
) -> Result<MachineId, JoinError> {
    let targets = join_targets(address, port)?;
    let identity = Identity::load_or_create(secrets)?;
    let request = JoinRequest {
        code: code.to_owned(),
        machine: identity.machine,
        key: identity.key_pair()?.public(),
    };
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(10)))
        .build()
        .into();
    let mut answered = None;
    for target in targets {
        let url = format!("http://{target}{JOIN_PATH}");
        let answer = post_join(&agent, &url, &request);
        let reached = !matches!(answer, Err(JoinError::Unreachable { .. }));
        answered = Some((url, answer));
        if reached {
            break;
        }
    }
    let (url, answer) = answered.expect("join_targets never returns an empty list");
    let answer = answer?;
    if answer.machine == identity.machine {
        // Only this machine speaks for its own id.
        return Err(JoinError::AnsweredAsThisMachine(url));
    }
    hold_key(secrets, answer.machine, answer.key)?;
    Ok(identity.machine)
}

/// Posts `request` to the join route at `url`.
fn post_join(
    agent: &ureq::Agent,
    url: &str,
    request: &JoinRequest,
) -> Result<JoinAnswer, JoinError> {
    match agent.post(url).send_json(request) {
        Ok(mut response) => response
            .body_mut()
            .read_json()
            .map_err(|_| JoinError::NotAdmitted {
                url: url.to_owned(),
                status: 200,
            }),
        Err(ureq::Error::StatusCode(403)) => Err(JoinError::BadCode),
        Err(ureq::Error::StatusCode(409)) => Err(JoinError::Taken(request.machine)),
        Err(ureq::Error::StatusCode(status)) => Err(JoinError::NotAdmitted {
            url: url.to_owned(),
            status,
        }),
        Err(error) => Err(JoinError::Unreachable {
            url: url.to_owned(),
            reason: error.to_string(),
        }),
    }
}

/// The Tailscale and loopback addresses `address` names or resolves to, at `port` unless it
/// names one. [`JoinError::NotTailscale`] when it has none.
fn join_targets(address: &str, port: u16) -> Result<Vec<SocketAddr>, JoinError> {
    let authority = join_authority(address, port);
    let resolved = authority
        .to_socket_addrs()
        .map_err(|error| JoinError::Unreachable {
            url: format!("http://{authority}{JOIN_PATH}"),
            reason: error.to_string(),
        })?;
    let targets: Vec<SocketAddr> = resolved.filter(|target| joinable(target.ip())).collect();
    if targets.is_empty() {
        return Err(JoinError::NotTailscale(address.to_owned()));
    }
    Ok(targets)
}

/// Whether a join may be sent to `ip`: one in Tailscale's ranges, 100.64.0.0/10 and
/// fd7a:115c:a1e0::/48, or loopback (spec D11: machines meet over Tailscale only).
fn joinable(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [first, second, ..] = ip.octets();
            ip.is_loopback() || (first == 100 && second & 0xc0 == 64)
        }
        IpAddr::V6(ip) => ip.is_loopback() || ip.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
    }
}

/// Puts the admitting machine's key in the key table kept in `secrets`, by
/// [`admit_into`]'s rule, so this machine's links with it are served from the daemon's next
/// start.
fn hold_key(secrets: &dyn Secrets, machine: MachineId, key: PublicKey) -> Result<(), JoinError> {
    change_table(secrets, |machines| {
        admit_into(machines, machine, &key).map_err(|error| match error {
            JoinError::Taken(machine) => JoinError::HostRefusedHere(machine),
            other => other,
        })
    })?;
    Ok(())
}

/// The one rule for admitting a machine to a key table: a key that is not Ed25519 is refused,
/// a machine not held is added, one held with the same key is left as it is, and one held
/// revoked or with another key is refused, so nothing replaces a key or a revocation.
fn admit_into(
    machines: &mut BTreeMap<MachineId, Entry>,
    machine: MachineId,
    key: &PublicKey,
) -> Result<(), JoinError> {
    if key.verifier().is_none() {
        return Err(JoinError::BadKey);
    }
    match machines.get(&machine) {
        None => {
            machines.insert(machine, Entry::Active { key: key.clone() });
            Ok(())
        }
        Some(Entry::Active { key: held }) if held == key => Ok(()),
        Some(_) => Err(JoinError::Taken(machine)),
    }
}

fn read_table(secrets: &dyn Secrets) -> Result<BTreeMap<MachineId, Entry>, JoinError> {
    parse_table(secrets.get(KEYS_SECRET)?)
}

fn parse_table(stored: Option<String>) -> Result<BTreeMap<MachineId, Entry>, JoinError> {
    match stored {
        None => Ok(BTreeMap::new()),
        Some(text) => serde_json::from_str(&text).map_err(|_| JoinError::Unreadable(KEYS_SECRET)),
    }
}

/// Applies `edit` to the key table stored in `secrets` and saves it, in one locked
/// [`Secrets::update`], so a change from another process (the daemon, `foundryd join`) is
/// never lost; an edit that refuses saves nothing. Returns the table saved.
fn change_table(
    secrets: &dyn Secrets,
    edit: impl FnOnce(&mut BTreeMap<MachineId, Entry>) -> Result<(), JoinError>,
) -> Result<BTreeMap<MachineId, Entry>, JoinError> {
    let mut changed = None;
    secrets.update(
        KEYS_SECRET,
        Box::new(|stored| {
            let result = parse_table(stored).and_then(|mut machines| {
                edit(&mut machines)?;
                Ok(machines)
            });
            let save = result.as_ref().ok().map(write_table);
            changed = Some(result);
            save
        }),
    )?;
    changed.expect("an update always calls its edit")
}

fn write_table(machines: &BTreeMap<MachineId, Entry>) -> String {
    serde_json::to_string(machines).expect("a key table of UUIDs and strings always serializes")
}

/// The host and port to post a join code to: `address` is an IP address, a bracketed IPv6
/// address or a host name, with or without a port.
fn join_authority(address: &str, port: u16) -> String {
    let unbracketed = address
        .strip_prefix('[')
        .and_then(|a| a.strip_suffix(']'))
        .unwrap_or(address);
    if let Ok(socket) = address.parse::<SocketAddr>() {
        socket.to_string()
    } else if let Ok(ip) = unbracketed.parse::<IpAddr>() {
        SocketAddr::new(ip, port).to_string()
    } else if address.contains(':') {
        address.to_owned()
    } else {
        format!("{address}:{port}")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum JoinError {
    #[error("the code is wrong, used or expired; ask for a new one")]
    BadCode,
    #[error(
        "machine {0} is revoked, or held with another key; a revoked machine cannot join again \
         under the same id"
    )]
    Taken(MachineId),
    #[error(
        "machine {0} admitted this one, but this machine holds it revoked or with another key \
         and will not link with it"
    )]
    HostRefusedHere(MachineId),
    #[error(
        "{0} answered with this machine's own id; this machine's identity may be a copy of \
         that machine's"
    )]
    AnsweredAsThisMachine(String),
    #[error("the public key sent is not an Ed25519 public key")]
    BadKey,
    #[error("{url} could not admit this machine (HTTP {status}); ask for a new code")]
    NotAdmitted { url: String, status: u16 },
    #[error("{url} could not be reached: {reason}")]
    Unreachable { url: String, reason: String },
    #[error(
        "{0} is not a Tailscale address and resolves to none; join over Tailscale \
         (100.64.0.0/10 or fd7a:115c:a1e0::/48), since a join code travels unencrypted"
    )]
    NotTailscale(String),
    #[error(
        "no other change to the key table has got through for {} s; one may be stuck on the \
         secret store; try again",
        secrets::LOCK_WAIT.as_secs()
    )]
    Busy,
    #[error("the secret {0} is not in the expected form")]
    Unreadable(&'static str),
    #[error("the system gave no random numbers")]
    Random,
    #[error(transparent)]
    Secrets(#[from] SecretsError),
}

/// Why a machine's link is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refused {
    #[error("it proved no key")]
    NoKey,
    #[error("this machine holds no key for it")]
    Unknown,
    #[error("it is revoked")]
    Revoked,
    #[error("its proof does not match its key")]
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

/// A machine's Ed25519 public key, as hex.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PublicKey(String);

impl PublicKey {
    fn verifier(&self) -> Option<UnparsedPublicKey<Vec<u8>>> {
        let bytes = unhex(&self.0).filter(|b| b.len() == ED25519_PUBLIC_KEY_LENGTH)?;
        Some(UnparsedPublicKey::new(&ED25519, bytes))
    }
}

const ED25519_PUBLIC_KEY_LENGTH: usize = 32;

/// This machine's private key, as hex PKCS#8. It is never shown in debug output.
#[derive(Clone, Serialize, Deserialize)]
#[serde(transparent)]
struct PrivateKey(String);

impl fmt::Debug for PrivateKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PrivateKey(..)")
    }
}

/// This machine's id and private key.
#[derive(Debug, Serialize, Deserialize)]
struct Identity {
    machine: MachineId,
    key: PrivateKey,
}

impl Identity {
    /// The identity kept in `secrets`, made and kept there first if there is none.
    /// Made in one locked [`Secrets::update`], so two processes starting at once keep one.
    fn load_or_create(secrets: &dyn Secrets) -> Result<Self, JoinError> {
        let mut found = None;
        secrets.update(
            IDENTITY_SECRET,
            Box::new(|stored| {
                let (identity, save) = match stored {
                    Some(text) => (
                        serde_json::from_str(&text)
                            .map_err(|_| JoinError::Unreadable(IDENTITY_SECRET)),
                        None,
                    ),
                    None => match Self::create() {
                        Ok((identity, text)) => (Ok(identity), Some(text)),
                        Err(error) => (Err(error), None),
                    },
                };
                found = Some(identity);
                save
            }),
        )?;
        found.expect("an update always calls its edit")
    }

    /// A new identity, and its text as kept.
    fn create() -> Result<(Self, String), JoinError> {
        let pkcs8 =
            Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).map_err(|_| JoinError::Random)?;
        let identity = Self {
            machine: MachineId::random(),
            key: PrivateKey(hex(pkcs8.as_ref())),
        };
        let text =
            serde_json::to_string(&identity).map_err(|_| JoinError::Unreadable(IDENTITY_SECRET))?;
        Ok((identity, text))
    }

    fn key_pair(&self) -> Result<Signer, JoinError> {
        unhex(&self.key.0)
            .and_then(|pkcs8| Ed25519KeyPair::from_pkcs8(&pkcs8).ok())
            .map(Signer)
            .ok_or(JoinError::Unreadable(IDENTITY_SECRET))
    }
}

/// This machine's key pair.
struct Signer(Ed25519KeyPair);

impl Signer {
    fn public(&self) -> PublicKey {
        PublicKey(hex(self.0.public_key().as_ref()))
    }
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

/// What this machine holds for another: its public key, or that it is revoked.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum Entry {
    Active { key: PublicKey },
    Revoked,
}

/// This machine's identity, the join codes it has shown and the keys of the machines it
/// admits.
#[derive(Clone)]
pub struct Keys {
    inner: Arc<KeysInner>,
}

struct KeysInner {
    me: MachineId,
    signer: Signer,
    secrets: Arc<dyn Secrets>,
    held: Mutex<Held>,
    /// Held across each change to the key table and its save, so saves land in order and a
    /// slow secret store never holds up a link's check.
    saving: Mutex<()>,
    /// How many times `saving` has been taken: what a change waiting on it counts as progress.
    taken: AtomicU64,
}

struct Held {
    codes: Vec<Shown>,
    machines: BTreeMap<MachineId, Entry>,
}

/// A code shown, and the machine that used it.
struct Shown {
    code: String,
    at: Instant,
    /// Also kept because `at` stops while the machine sleeps.
    wall: SystemTime,
    used_by: Option<MachineId>,
}

impl Shown {
    /// Whether the code is still valid. Either clock passing [`CODE_VALID_FOR`] ends it, and
    /// so does a wall clock now before the showing.
    fn valid(&self) -> bool {
        self.at.elapsed() < CODE_VALID_FOR
            && SystemTime::now()
                .duration_since(self.wall)
                .is_ok_and(|age| age < CODE_VALID_FOR)
    }
}

impl Keys {
    /// This machine's identity and key table, kept in `secrets`, with no codes outstanding.
    /// A machine with no identity yet is given one, and its own entry is always its own key.
    ///
    /// Blocks on the secret store: a disk that does not answer, or another process's lock until
    /// [`secrets::LOCK_WAIT`] passes with no change getting through. Call it from a blocking
    /// context, not an async task.
    pub fn open(secrets: Arc<dyn Secrets>) -> Result<Self, JoinError> {
        let identity = Identity::load_or_create(&*secrets)?;
        let signer = identity.key_pair()?;
        let machines = read_table(&*secrets)?;
        let keys = Self {
            inner: Arc::new(KeysInner {
                me: identity.machine,
                signer,
                secrets,
                held: Mutex::new(Held {
                    codes: Vec::new(),
                    machines,
                }),
                saving: Mutex::new(()),
                taken: AtomicU64::new(0),
            }),
        };
        let own = keys.public_key();
        let holds_own = matches!(
            keys.held().machines.get(&identity.machine),
            Some(Entry::Active { key }) if *key == own
        );
        if !holds_own {
            let saving = keys.saving()?;
            keys.change(&saving, |machines| {
                machines.insert(identity.machine, Entry::Active { key: own });
                Ok(())
            })?;
        }
        Ok(keys)
    }

    /// This machine's id.
    pub fn me(&self) -> MachineId {
        self.inner.me
    }

    /// This machine's public key.
    pub fn public_key(&self) -> PublicKey {
        self.inner.signer.public()
    }

    /// A new code that works once within [`CODE_VALID_FOR`].
    pub fn new_code(&self) -> Result<Code, JoinError> {
        let mut bytes = [0; CODE_LENGTH];
        SystemRandom::new()
            .fill(&mut bytes)
            .map_err(|_| JoinError::Random)?;
        let code: String = bytes
            .iter()
            .map(|&b| char::from(CODE_LETTERS[usize::from(b) % CODE_LETTERS.len()]))
            .collect();
        let mut held = self.held();
        held.forget_expired_codes();
        held.codes.push(Shown {
            code: code.clone(),
            at: Instant::now(),
            wall: SystemTime::now(),
            used_by: None,
        });
        Ok(Code(code))
    }

    /// Uses up `code` to admit `machine` with public key `key`, by [`admit_into`]'s rule on
    /// the stored table. The machine that used a code may send it again while it is valid, as
    /// a join whose answer was lost does. A code is used up only once the key is saved.
    ///
    /// Blocks on the secret store, as [`Keys::open`] does, and behind any other change to the
    /// key table in this process.
    pub fn redeem(&self, code: &str, machine: MachineId, key: &PublicKey) -> Result<(), JoinError> {
        let typed = code.trim().to_ascii_uppercase();
        let saving = self.saving()?;
        let used_by = {
            let mut held = self.held();
            held.forget_expired_codes();
            held.codes
                .iter()
                .find(|shown| same(shown.code.as_bytes(), typed.as_bytes()))
                .ok_or(JoinError::BadCode)?
                .used_by
        };
        if used_by.is_some_and(|user| user != machine) {
            return Err(JoinError::BadCode);
        }
        self.change(&saving, |machines| admit_into(machines, machine, key))?;
        if let Some(shown) = self
            .held()
            .codes
            .iter_mut()
            .find(|shown| same(shown.code.as_bytes(), typed.as_bytes()))
        {
            shown.used_by = Some(machine);
        }
        Ok(())
    }

    /// Revokes `machine`: its key is forgotten and every link with it is refused.
    ///
    /// Blocks on the secret store, as [`Keys::open`] does, and behind any other change to the
    /// key table in this process.
    pub fn revoke(&self, machine: MachineId) -> Result<(), JoinError> {
        let saving = self.saving()?;
        self.change(&saving, |machines| {
            machines.insert(machine, Entry::Revoked);
            Ok(())
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

    /// Whether a link with `machine` may open or stay open.
    fn admitted(&self, machine: MachineId) -> Result<(), Refused> {
        match self.state(machine) {
            KeyState::Active => Ok(()),
            KeyState::Revoked => Err(Refused::Revoked),
            KeyState::NoKey => Err(Refused::Unknown),
        }
    }

    /// Whether `proof` is `machine`'s signature over `message`.
    fn verify(&self, machine: MachineId, message: &[u8], proof: &str) -> Result<(), Refused> {
        let key = match self.held().machines.get(&machine) {
            None => return Err(Refused::Unknown),
            Some(Entry::Revoked) => return Err(Refused::Revoked),
            Some(Entry::Active { key }) => key.clone(),
        };
        let verifier = key.verifier().ok_or(Refused::WrongKey)?;
        let proof = unhex(proof).ok_or(Refused::WrongKey)?;
        verifier
            .verify(message, &proof)
            .map_err(|_| Refused::WrongKey)
    }

    /// This machine's signature over `message`, as hex.
    fn sign(&self, message: &[u8]) -> String {
        hex(self.inner.signer.0.sign(message).as_ref())
    }

    /// Serves a link a peer opened, as [`Peers::accept`] does, once both ends have proved
    /// their keys and the link's first message, a hello, a relay or a call, names the machine
    /// proved. The link closes when that machine is revoked.
    pub async fn accept(&self, peers: &Peers, mut conn: Conn) {
        let Some((machine, first)) = self.admit(&conn.send, &mut conn.recv).await else {
            return;
        };
        tokio::select! {
            () = peers.accept_opened(conn, first) => {}
            () = revoked(self, machine) => {}
        }
    }

    /// The accepting end of the handshake. Returns the machine proved and its first message,
    /// a hello, a relay or a call, parsed.
    async fn admit(
        &self,
        send: &Sender<String>,
        recv: &mut Receiver<String>,
    ) -> Option<(MachineId, PeerMessage)> {
        let me = self.me();
        let Some(Handshake::Challenge {
            machine,
            nonce: theirs,
        }) = read(recv).await
        else {
            tracing::warn!("join: refused a link: {}", Refused::NoKey);
            return None;
        };
        let refuse = |refused: Refused| {
            tracing::warn!("join: refused a link from {machine}: {refused}");
        };
        if let Err(refused) = self.admitted(machine) {
            refuse(refused);
            return None;
        }
        let ours = nonce().ok()?;
        let answer = Handshake::Answer {
            machine: me,
            nonce: ours.clone(),
            proof: self.sign(&link_message(ACCEPTING, me, machine, &theirs)),
        };
        if !write(send, &answer).await {
            return None;
        }
        let Some(Handshake::Proof { proof }) = read(recv).await else {
            refuse(Refused::NoKey);
            return None;
        };
        if let Err(refused) =
            self.verify(machine, &link_message(DIALING, machine, me, &ours), &proof)
        {
            refuse(refused);
            return None;
        }
        let text = timeout(OFFLINE_AFTER, recv.recv()).await.ok()??;
        match serde_json::from_str(&text) {
            Ok(
                first @ (PeerMessage::Hello { machine: named, .. }
                | PeerMessage::Relay { machine: named, .. }
                | PeerMessage::Call { machine: named, .. }),
            ) if named == machine => Some((machine, first)),
            _ => {
                tracing::warn!(
                    "join: refused a link from {machine}: its first message names another"
                );
                None
            }
        }
    }

    /// The lock every change to the key table holds from its read to its save. It waits until
    /// [`secrets::LOCK_WAIT`] passes with no other change taking it, as [`Keys::saving_within`]
    /// says.
    fn saving(&self) -> Result<MutexGuard<'_, ()>, JoinError> {
        self.saving_within(secrets::LOCK_WAIT)
    }

    /// The `saving` lock. A change waiting behind others that keep taking it waits on; one
    /// that sees nobody take it for `wait`, as when a holder is stuck on the secret store,
    /// gives [`JoinError::Busy`].
    fn saving_within(&self, wait: Duration) -> Result<MutexGuard<'_, ()>, JoinError> {
        let mut seen = self.inner.taken.load(Ordering::SeqCst);
        let mut deadline = std::time::Instant::now() + wait;
        loop {
            let guard = match self.inner.saving.try_lock() {
                Ok(guard) => Some(guard),
                Err(TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
                Err(TryLockError::WouldBlock) => None,
            };
            if let Some(guard) = guard {
                self.inner.taken.fetch_add(1, Ordering::SeqCst);
                return Ok(guard);
            }
            let now = self.inner.taken.load(Ordering::SeqCst);
            if now != seen {
                seen = now;
                deadline = std::time::Instant::now() + wait;
            } else if std::time::Instant::now() >= deadline {
                return Err(JoinError::Busy);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Applies `edit` to the key table as stored now, saves it, then holds it; an edit that
    /// refuses saves nothing. The stored table is the one source of truth for changes: every
    /// change is decided by an edit on it, never on the table held in memory, which can lag
    /// what `foundryd join` saved from another process. Links are checked against the table
    /// held in memory, which this refreshes and a restart reloads. The read and the save are
    /// one locked [`Secrets::update`], so a change from another process is never lost; the
    /// `saving` guard keeps this process's changes, and the tables they hold, in order.
    fn change(
        &self,
        _saving: &MutexGuard<'_, ()>,
        edit: impl FnOnce(&mut BTreeMap<MachineId, Entry>) -> Result<(), JoinError>,
    ) -> Result<(), JoinError> {
        let machines = change_table(&*self.inner.secrets, edit)?;
        self.held().machines = machines;
        Ok(())
    }

    fn held(&self) -> MutexGuard<'_, Held> {
        lock(&self.inner.held)
    }
}

impl Held {
    fn forget_expired_codes(&mut self) {
        self.codes.retain(Shown::valid);
    }
}

/// The messages a link opens with, before the hello.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Handshake {
    /// Dialer to acceptor: who is dialing, and a challenge for the acceptor to sign.
    Challenge { machine: MachineId, nonce: String },
    /// Acceptor to dialer: who answered, its signature over the dialer's challenge, and a
    /// challenge for the dialer to sign.
    Answer {
        machine: MachineId,
        nonce: String,
        proof: String,
    },
    /// Dialer to acceptor: its signature over the acceptor's challenge.
    Proof { proof: String },
}

/// What a link proof signs: the signer's role, the signer, the machine it proves itself to
/// and that machine's challenge.
fn link_message(role: &str, signer: MachineId, verifier: MachineId, nonce: &str) -> Vec<u8> {
    format!("foundry link proof\n{role}\n{signer}\n{verifier}\n{nonce}").into_bytes()
}

/// A fresh challenge, as hex.
fn nonce() -> Result<String, ring::error::Unspecified> {
    let mut bytes = [0; NONCE_LENGTH];
    SystemRandom::new().fill(&mut bytes)?;
    Ok(hex(&bytes))
}

/// The next handshake message, or `None` when the link closes, stays silent for
/// [`OFFLINE_AFTER`] or sends something else.
async fn read(recv: &mut Receiver<String>) -> Option<Handshake> {
    let text = timeout(OFFLINE_AFTER, recv.recv()).await.ok()??;
    serde_json::from_str(&text).ok()
}

/// Sends a handshake message, and says whether the link is still open.
async fn write(send: &Sender<String>, message: &Handshake) -> bool {
    let text = serde_json::to_string(message).expect("ids and hex strings always serialize");
    matches!(timeout(OFFLINE_AFTER, send.send(text)).await, Ok(Ok(())))
}

/// Opens links with `dialer` and proves both ends' keys before handing a link on. A machine
/// this one holds no active key for is not dialed, and a link to one closes within
/// [`RECHECK`] of its revocation.
pub struct KeyedDialer {
    dialer: Arc<dyn Dialer>,
    keys: Keys,
}

impl KeyedDialer {
    pub fn new(dialer: Arc<dyn Dialer>, keys: Keys) -> Self {
        Self { dialer, keys }
    }
}

impl Dialer for KeyedDialer {
    fn dial(&self, peer: &Machine) -> BoxFuture<'static, io::Result<Conn>> {
        let (id, name) = (peer.id, peer.name.clone());
        let refused = move |refused: Refused| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{name} ({id}) refused: {refused}"),
            )
        };
        if let Err(no) = self.keys.admitted(id) {
            let error = refused(no);
            return Box::pin(async move { Err(error) });
        }
        let dialing = self.dialer.dial(peer);
        let keys = self.keys.clone();
        Box::pin(async move {
            let mut conn = dialing.await?;
            let closed = || io::Error::new(io::ErrorKind::BrokenPipe, "the link closed");
            let me = keys.me();
            let ours =
                nonce().map_err(|_| io::Error::other("the system gave no random numbers"))?;
            let challenge = Handshake::Challenge {
                machine: me,
                nonce: ours.clone(),
            };
            if !write(&conn.send, &challenge).await {
                return Err(closed());
            }
            let Some(Handshake::Answer {
                machine,
                nonce: theirs,
                proof,
            }) = read(&mut conn.recv).await
            else {
                return Err(refused(Refused::NoKey));
            };
            if machine != id {
                return Err(refused(Refused::WrongKey));
            }
            keys.verify(id, &link_message(ACCEPTING, id, me, &ours), &proof)
                .map_err(&refused)?;
            let proof = Handshake::Proof {
                proof: keys.sign(&link_message(DIALING, me, id, &theirs)),
            };
            if !write(&conn.send, &proof).await {
                return Err(closed());
            }
            let (near, far) = Conn::pair();
            tokio::spawn(relay(conn, near, keys, id));
            Ok(far)
        })
    }
}

/// Carries messages both ways between `outer` and `inner` until either closes or `keys` no
/// longer admits `peer`, checked every [`RECHECK`]. The two directions run side by side, so a
/// send that waits on one end never stops the other.
async fn relay(outer: Conn, inner: Conn, keys: Keys, peer: MachineId) {
    let Conn {
        send: out_send,
        recv: mut out_recv,
    } = outer;
    let Conn {
        send: in_send,
        recv: mut in_recv,
    } = inner;
    tokio::select! {
        () = forward(&mut out_recv, &in_send) => {}
        () = forward(&mut in_recv, &out_send) => {}
        () = revoked(&keys, peer) => {}
    }
}

/// Returns once `keys` no longer admits `peer`, checked every [`RECHECK`], so a link with it
/// selected against this closes.
async fn revoked(keys: &Keys, peer: MachineId) {
    let mut every = tokio::time::interval(RECHECK);
    loop {
        every.tick().await;
        if let Err(refused) = keys.admitted(peer) {
            tracing::info!("join: link with {peer} closed: {refused}");
            return;
        }
    }
}

async fn forward(from: &mut Receiver<String>, to: &Sender<String>) {
    while let Some(text) = from.recv().await {
        if to.send(text).await.is_err() {
            return;
        }
    }
}

/// The route a joining machine posts its code to. It answers with this machine's id and
/// public key when the machine is admitted, 403 to a code that is wrong, used or expired, 409
/// to a machine revoked or held with another key, and 400 to a key that is not an Ed25519
/// public key.
pub fn router(keys: Keys) -> Router {
    Router::new()
        .route(JOIN_PATH, post(redeem))
        .with_state(keys)
}

/// The admitting machine's answer: who it is and its public key.
#[derive(Serialize, Deserialize)]
struct JoinAnswer {
    machine: MachineId,
    key: PublicKey,
}

#[derive(Serialize, Deserialize)]
struct JoinRequest {
    code: String,
    machine: MachineId,
    key: PublicKey,
}

async fn redeem(State(keys): State<Keys>, Json(request): Json<JoinRequest>) -> Response {
    let answer = JoinAnswer {
        machine: keys.me(),
        key: keys.public_key(),
    };
    // Saving the key table can wait on the disk.
    let redeemed = tokio::task::spawn_blocking(move || {
        keys.redeem(&request.code, request.machine, &request.key)
    })
    .await;
    match redeemed {
        Ok(Ok(())) => Json(answer).into_response(),
        Ok(Err(error @ JoinError::BadCode)) => {
            (StatusCode::FORBIDDEN, error.to_string()).into_response()
        }
        Ok(Err(error @ JoinError::Taken(_))) => {
            (StatusCode::CONFLICT, error.to_string()).into_response()
        }
        Ok(Err(error @ JoinError::BadKey)) => {
            (StatusCode::BAD_REQUEST, error.to_string()).into_response()
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

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect()
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
    use crate::secrets::{Edit, FileSecrets, MemorySecrets, Secrets, SecretsError};
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
                return Err(SecretsError::Io {
                    path: "secrets.json".into(),
                    source: std::io::ErrorKind::PermissionDenied.into(),
                });
            }
            self.inner.set(name, value)
        }
        fn update(&self, name: &str, edit: Edit<'_>) -> Result<(), SecretsError> {
            match edit(self.get(name)?) {
                Some(value) => self.set(name, &value),
                None => Ok(()),
            }
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
    async fn a_watched_code_says_which_machine_used_it() {
        let (host, peer) = (machine_keys(), machine_keys());
        let (code, used) = host.new_watched_code().unwrap();
        host.redeem(&code.to_string(), peer.me(), &peer.public_key())
            .unwrap();
        assert_eq!(used.await.unwrap(), peer.me());
    }

    #[tokio::test(start_paused = true)]
    async fn a_watched_code_says_nothing_until_it_is_used_and_nothing_for_a_failed_save() {
        let flaky = Arc::new(Flaky::default());
        let host = Keys::open(flaky.clone()).unwrap();
        let peer = machine_keys();
        let (code, mut used) = host.new_watched_code().unwrap();
        // Another code used, or this one refused to another machine's stolen claim, says nothing.
        admit(&host, &machine_keys());
        flaky.failing.store(true, Ordering::SeqCst);
        assert!(
            host.redeem(&code.to_string(), peer.me(), &peer.public_key())
                .is_err()
        );
        assert!(used.try_recv().is_err());
        flaky.failing.store(false, Ordering::SeqCst);
        host.redeem(&code.to_string(), peer.me(), &peer.public_key())
            .unwrap();
        assert_eq!(used.await.unwrap(), peer.me());
    }

    #[tokio::test(start_paused = true)]
    async fn a_watched_code_that_expires_unused_ends_its_watch() {
        let host = machine_keys();
        let (_code, used) = host.new_watched_code().unwrap();
        tokio::time::advance(CODE_VALID_FOR).await;
        // Any later code forgets the expired one.
        host.new_code().unwrap();
        assert!(used.await.is_err());
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

    #[tokio::test(start_paused = true)]
    async fn a_machine_already_admitted_may_use_a_new_code_with_the_same_key_and_spends_it() {
        let (host, peer, other) = (machine_keys(), machine_keys(), machine_keys());
        admit(&host, &peer);
        let code = host.new_code().unwrap().to_string();
        host.redeem(&code, peer.me(), &peer.public_key()).unwrap();
        assert_eq!(host.state(peer.me()), KeyState::Active);
        let spent = host.redeem(&code, other.me(), &other.public_key());
        assert!(matches!(spent, Err(JoinError::BadCode)), "{spent:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_key_table_change_keeps_what_another_process_saved_meanwhile() {
        let secrets = Arc::new(MemorySecrets::default());
        let daemon = Keys::open(secrets.clone()).unwrap();
        let (h, m) = (machine_keys(), machine_keys());
        admit(&daemon, &m);
        // `foundryd join` in another process keeps its host's key in the same store.
        hold_key(&*secrets, h.me(), h.public_key()).unwrap();
        daemon.revoke(m.me()).unwrap();
        assert_eq!(daemon.state(h.me()), KeyState::Active);
        let reopened = Keys::open(secrets).unwrap();
        assert_eq!(reopened.state(h.me()), KeyState::Active);
        assert_eq!(reopened.state(m.me()), KeyState::Revoked);
    }

    #[tokio::test(start_paused = true)]
    async fn joining_never_replaces_a_key_or_revocation_already_held() {
        let secrets = Arc::new(MemorySecrets::default());
        let me = Keys::open(secrets.clone()).unwrap();
        let (held, revoked, stranger) = (machine_keys(), machine_keys(), machine_keys());
        admit(&me, &held);
        admit(&me, &revoked);
        me.revoke(revoked.me()).unwrap();
        hold_key(&*secrets, held.me(), held.public_key()).unwrap();
        let before = secrets.get(KEYS_SECRET).unwrap();
        for (machine, key) in [
            (held.me(), stranger.public_key()),
            (revoked.me(), revoked.public_key()),
        ] {
            let kept = hold_key(&*secrets, machine, key);
            assert!(
                matches!(kept, Err(JoinError::HostRefusedHere(m)) if m == machine),
                "{kept:?}"
            );
            assert_eq!(secrets.get(KEYS_SECRET).unwrap(), before);
        }
        secrets.set(KEYS_SECRET, "not json").unwrap();
        let unreadable = hold_key(&*secrets, stranger.me(), stranger.public_key());
        assert!(matches!(
            unreadable,
            Err(JoinError::Unreadable(KEYS_SECRET))
        ));
        assert_eq!(
            secrets.get(KEYS_SECRET).unwrap().as_deref(),
            Some("not json")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_code_expires_by_the_wall_clock_too_as_after_the_host_slept() {
        let (host, peer) = (machine_keys(), machine_keys());
        for shift in [
            -(CODE_VALID_FOR.as_secs() as i64) - 60,
            // A wall clock stepped back past the code's showing cannot vouch for it either.
            60,
        ] {
            let code = host.new_code().unwrap().to_string();
            {
                let mut held = host.held();
                let shown = held.codes.last_mut().unwrap();
                shown.wall = if shift < 0 {
                    shown.wall - Duration::from_secs(shift.unsigned_abs())
                } else {
                    shown.wall + Duration::from_secs(shift as u64)
                };
            }
            let late = host.redeem(&code, peer.me(), &peer.public_key());
            assert!(matches!(late, Err(JoinError::BadCode)), "{shift}: {late:?}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_unreadable_key_table_or_identity_is_refused_and_left_as_it_was() {
        for (name, other) in [(KEYS_SECRET, None), (IDENTITY_SECRET, Some(KEYS_SECRET))] {
            let secrets = Arc::new(MemorySecrets::default());
            Keys::open(secrets.clone()).unwrap();
            secrets.set(name, "not json").unwrap();
            let opened = Keys::open(secrets.clone());
            assert!(
                matches!(opened, Err(JoinError::Unreadable(n)) if n == name),
                "{name}: {:?}",
                opened.err()
            );
            assert_eq!(secrets.get(name).unwrap().as_deref(), Some("not json"));
            if other.is_some() {
                let joined = join_at("127.0.0.1:1", DEFAULT_PORT, "CODE", &*secrets);
                assert!(matches!(joined, Err(JoinError::Unreadable(n)) if n == name));
                assert_eq!(secrets.get(name).unwrap().as_deref(), Some("not json"));
            }
        }
    }

    /// A secret store whose saves of the key table wait, once armed, until the test opens
    /// the gate.
    #[derive(Default)]
    struct Gated {
        inner: MemorySecrets,
        armed: AtomicBool,
        open: (Mutex<bool>, std::sync::Condvar),
        waiting: std::sync::atomic::AtomicUsize,
    }

    impl Secrets for Gated {
        fn get(&self, name: &str) -> Result<Option<String>, SecretsError> {
            self.inner.get(name)
        }
        fn set(&self, name: &str, value: &str) -> Result<(), SecretsError> {
            if name == KEYS_SECRET && self.armed.load(Ordering::SeqCst) {
                self.waiting.fetch_add(1, Ordering::SeqCst);
                let (open, opened) = &self.open;
                let mut open = open.lock().unwrap();
                while !*open {
                    open = opened.wait(open).unwrap();
                }
            }
            self.inner.set(name, value)
        }
        fn update(&self, name: &str, edit: Edit<'_>) -> Result<(), SecretsError> {
            match edit(self.get(name)?) {
                Some(value) => self.set(name, &value),
                None => Ok(()),
            }
        }
    }

    #[test]
    fn a_change_waiting_past_the_wait_behind_another_gives_busy_and_changes_nothing() {
        let keys = machine_keys();
        let machine = MachineId::random();
        let held = keys.saving().unwrap();
        std::thread::scope(|scope| {
            let started = std::time::Instant::now();
            let revoked = scope.spawn(|| keys.revoke(machine)).join().unwrap();
            assert!(started.elapsed() >= secrets::LOCK_WAIT);
            assert!(matches!(revoked, Err(JoinError::Busy)), "{revoked:?}");
        });
        drop(held);
        assert_eq!(keys.state(machine), KeyState::NoKey);
        keys.revoke(machine).unwrap();
        assert_eq!(keys.state(machine), KeyState::Revoked);
    }

    #[test]
    fn a_change_behind_others_that_keep_getting_through_waits_on() {
        let keys = machine_keys();
        let held = keys.saving().unwrap();
        std::thread::scope(|scope| {
            let waiting = scope.spawn(|| keys.saving_within(Duration::from_millis(500)).map(drop));
            // Past the wait in all, but each step counts as another change taking the lock.
            for _ in 0..8 {
                std::thread::sleep(Duration::from_millis(200));
                keys.inner.taken.fetch_add(1, Ordering::SeqCst);
            }
            drop(held);
            waiting.join().unwrap().unwrap();
        });
    }

    #[test]
    fn taking_the_saving_lock_counts_as_progress_for_waiters() {
        let keys = machine_keys();
        let before = keys.inner.taken.load(Ordering::SeqCst);
        drop(keys.saving().unwrap());
        assert!(keys.inner.taken.load(Ordering::SeqCst) > before);
    }

    #[test]
    fn a_change_let_through_within_the_wait_goes_ahead() {
        let keys = machine_keys();
        let machine = MachineId::random();
        let held = keys.saving().unwrap();
        std::thread::scope(|scope| {
            let revoking = scope.spawn(|| keys.revoke(machine));
            std::thread::sleep(Duration::from_millis(300));
            drop(held);
            revoking.join().unwrap().unwrap();
        });
        assert_eq!(keys.state(machine), KeyState::Revoked);
    }

    #[test]
    fn a_revocation_and_a_join_saved_at_once_by_two_processes_both_stay() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.json");
        // Each side has its own store over one file, as the daemon and `foundryd join` do.
        let daemon = Keys::open(Arc::new(FileSecrets::new(&path))).unwrap();
        let revoked: Vec<MachineId> = (0..40).map(|_| MachineId::random()).collect();
        let joined: Vec<(MachineId, PublicKey)> = (0..40)
            .map(|_| (MachineId::random(), machine_keys().public_key()))
            .collect();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for machine in &revoked {
                    daemon.revoke(*machine).unwrap();
                }
            });
            scope.spawn(|| {
                let join = FileSecrets::new(&path);
                for (machine, key) in &joined {
                    hold_key(&join, *machine, key.clone()).unwrap();
                }
            });
        });
        let table = read_table(&FileSecrets::new(&path)).unwrap();
        for machine in &revoked {
            assert!(
                matches!(table.get(machine), Some(Entry::Revoked)),
                "lost the revocation of {machine}"
            );
        }
        for (machine, key) in &joined {
            assert!(
                matches!(table.get(machine), Some(Entry::Active { key: held }) if held == key),
                "lost the key of {machine}"
            );
        }
    }

    #[test]
    fn two_processes_making_this_machines_identity_at_once_make_one() {
        for _ in 0..20 {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("secrets.json");
            let start = std::sync::Barrier::new(2);
            let [a, b] = std::thread::scope(|scope| {
                let make = || {
                    let secrets = FileSecrets::new(&path);
                    start.wait();
                    Identity::load_or_create(&secrets).unwrap().machine
                };
                let (a, b) = (scope.spawn(make), scope.spawn(make));
                [a.join().unwrap(), b.join().unwrap()]
            });
            assert_eq!(a, b);
            let kept = Identity::load_or_create(&FileSecrets::new(&path)).unwrap();
            assert_eq!(kept.machine, a);
        }
    }

    #[test]
    fn two_machines_sending_one_code_at_once_admit_only_one() {
        let gated = Arc::new(Gated::default());
        let host = Keys::open(gated.clone()).unwrap();
        let (x, y) = (machine_keys(), machine_keys());
        let code = host.new_code().unwrap().to_string();
        gated.armed.store(true, Ordering::SeqCst);
        let started = AtomicBool::new(false);
        let results = std::thread::scope(|scope| {
            let first = scope.spawn(|| host.redeem(&code, x.me(), &x.public_key()));
            while gated.waiting.load(Ordering::SeqCst) == 0 {
                std::thread::yield_now();
            }
            let second = scope.spawn(|| {
                started.store(true, Ordering::SeqCst);
                host.redeem(&code, y.me(), &y.public_key())
            });
            while !started.load(Ordering::SeqCst) {
                std::thread::yield_now();
            }
            std::thread::sleep(Duration::from_millis(200));
            *gated.open.0.lock().unwrap() = true;
            gated.open.1.notify_all();
            [first.join().unwrap(), second.join().unwrap()]
        });
        assert_eq!(
            results.iter().filter(|r| r.is_ok()).count(),
            1,
            "{results:?}"
        );
        let active = [x.me(), y.me()]
            .iter()
            .filter(|m| host.state(**m) == KeyState::Active)
            .count();
        assert_eq!(active, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_code_cannot_claim_a_machine_another_process_stored() {
        let secrets = Arc::new(MemorySecrets::default());
        let daemon = Keys::open(secrets.clone()).unwrap();
        let (held, revoked, thief, newcomer) = (
            machine_keys(),
            machine_keys(),
            machine_keys(),
            machine_keys(),
        );
        // Another process on this machine keeps one key and revokes another machine.
        hold_key(&*secrets, held.me(), held.public_key()).unwrap();
        Keys::open(secrets.clone())
            .unwrap()
            .revoke(revoked.me())
            .unwrap();
        let before = secrets.get(KEYS_SECRET).unwrap();
        let code = daemon.new_code().unwrap().to_string();
        for (machine, key) in [
            (held.me(), thief.public_key()),
            (revoked.me(), revoked.public_key()),
        ] {
            let claimed = daemon.redeem(&code, machine, &key);
            assert!(
                matches!(claimed, Err(JoinError::Taken(m)) if m == machine),
                "{claimed:?}"
            );
            assert_eq!(secrets.get(KEYS_SECRET).unwrap(), before);
        }
        daemon
            .redeem(&code, newcomer.me(), &newcomer.public_key())
            .unwrap();
        let reopened = Keys::open(secrets).unwrap();
        assert_eq!(reopened.state(held.me()), KeyState::Active);
        assert_eq!(reopened.state(revoked.me()), KeyState::Revoked);
        assert_eq!(reopened.state(newcomer.me()), KeyState::Active);
    }

    #[tokio::test(start_paused = true)]
    async fn a_change_to_an_unreadable_stored_table_is_refused_and_spends_no_code() {
        let secrets = Arc::new(MemorySecrets::default());
        let keys = Keys::open(secrets.clone()).unwrap();
        let (peer, newcomer) = (machine_keys(), machine_keys());
        admit(&keys, &peer);
        let table = secrets.get(KEYS_SECRET).unwrap().unwrap();
        let code = keys.new_code().unwrap().to_string();
        secrets.set(KEYS_SECRET, "not json").unwrap();
        let revoked = keys.revoke(peer.me());
        assert!(
            matches!(revoked, Err(JoinError::Unreadable(KEYS_SECRET))),
            "{revoked:?}"
        );
        let redeemed = keys.redeem(&code, newcomer.me(), &newcomer.public_key());
        assert!(
            matches!(redeemed, Err(JoinError::Unreadable(KEYS_SECRET))),
            "{redeemed:?}"
        );
        assert_eq!(
            secrets.get(KEYS_SECRET).unwrap().as_deref(),
            Some("not json")
        );
        secrets.set(KEYS_SECRET, &table).unwrap();
        keys.redeem(&code, newcomer.me(), &newcomer.public_key())
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_machine_always_holds_its_own_key_for_itself() {
        let secrets = Arc::new(MemorySecrets::default());
        let me = Keys::open(secrets.clone()).unwrap();
        let stranger = machine_keys();
        let foreign: BTreeMap<MachineId, Entry> = [(
            me.me(),
            Entry::Active {
                key: stranger.public_key(),
            },
        )]
        .into();
        secrets.set(KEYS_SECRET, &write_table(&foreign)).unwrap();
        let reopened = Keys::open(secrets.clone()).unwrap();
        let stored: serde_json::Value =
            serde_json::from_str(&secrets.get(KEYS_SECRET).unwrap().unwrap()).unwrap();
        assert_eq!(
            stored[me.me().to_string()]["key"].as_str(),
            Some(reopened.public_key().0.as_str())
        );
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

    async fn send(conn: &Conn, message: &Handshake) {
        let text = serde_json::to_string(message).unwrap();
        conn.send.send(text).await.unwrap();
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

    /// Runs the dialing end of the handshake on `conn` as `who`, to `host`.
    async fn handshake_as(conn: &mut Conn, who: &Keys, host: &Keys) {
        send(
            conn,
            &Handshake::Challenge {
                machine: who.me(),
                nonce: nonce().unwrap(),
            },
        )
        .await;
        let Handshake::Answer { nonce, .. } = read(conn).await else {
            panic!("no answer");
        };
        let proof = who.sign(&link_message(DIALING, who.me(), host.me(), &nonce));
        send(conn, &Handshake::Proof { proof }).await;
    }

    #[tokio::test]
    async fn a_hello_naming_another_machine_than_the_one_proved_is_refused() {
        for revoke_b in [false, true] {
            let (h, a, b) = (machine_keys(), machine_keys(), machine_keys());
            admit(&h, &a);
            admit(&h, &b);
            if revoke_b {
                h.revoke(b.me()).unwrap();
            }
            let host = host(&h, &[a.me(), b.me()]);
            let mut conn = host.raw();
            handshake_as(&mut conn, &a, &h).await;
            conn.send.send(hello(b.me())).await.unwrap();
            closed(&mut conn).await;
            let online = host.peers.online();
            assert!(!online.contains(&a.me()) && !online.contains(&b.me()));
        }
    }

    #[tokio::test]
    async fn an_acceptors_signature_cannot_pass_as_a_dialers_proof() {
        let (h, a) = pair();
        let (to_h_host, to_a_host) = (host(&h, &[a.me()]), host(&a, &[h.me()]));
        // A stranger opens a link to H as A, and keeps H's challenge.
        let mut to_h = to_h_host.raw();
        let opening = Handshake::Challenge {
            machine: a.me(),
            nonce: nonce().unwrap(),
        };
        send(&to_h, &opening).await;
        let Handshake::Answer {
            nonce: h_challenge, ..
        } = read(&mut to_h).await
        else {
            panic!("H did not answer");
        };
        // It dials A as H, with H's challenge as its own, and A signs it as the accepting end.
        let mut to_a = to_a_host.raw();
        let relayed = Handshake::Challenge {
            machine: h.me(),
            nonce: h_challenge,
        };
        send(&to_a, &relayed).await;
        let Handshake::Answer { proof, .. } = read(&mut to_a).await else {
            panic!("A did not answer");
        };
        // A's accepting signature, offered to H as A's dialing proof.
        send(&to_h, &Handshake::Proof { proof }).await;
        let _ = to_h.send.send(hello(a.me())).await;
        closed(&mut to_h).await;
        assert!(!to_h_host.peers.online().contains(&a.me()));
    }

    #[tokio::test]
    async fn a_link_proved_by_hand_is_served() {
        let (h, a) = pair();
        let host = host(&h, &[a.me()]);
        let mut conn = host.raw();
        handshake_as(&mut conn, &a, &h).await;
        served(&mut conn, a.me()).await;
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
        let r_id = r.me();
        let dialing = tokio::spawn(async move { dialer.dial(&machine(r_id)).await.map(drop) });
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

    #[tokio::test]
    async fn a_link_whose_hello_was_parsed_is_answered_without_reading_it_again() {
        let (h, p) = pair();
        let host = host(&h, &[p.me()]);
        let (mut near, far) = Conn::pair();
        let first: PeerMessage = serde_json::from_str(&hello(p.me())).unwrap();
        let peers = host.peers.clone();
        tokio::spawn(async move { peers.accept_opened(far, first).await });
        let answer = next(&mut near).await.expect("the link was refused");
        assert!(is_hello(&answer), "{answer}");
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
    async fn a_joined_machine_holds_the_hosts_key_and_their_link_is_served() {
        let h = machine_keys();
        let port = serve(h.clone()).await;
        let code = h.new_code().unwrap().to_string();
        let secrets = Arc::new(MemorySecrets::default());
        join_over_http(port, &code, secrets.clone()).await.unwrap();
        let joined = Keys::open(secrets).unwrap();
        assert_eq!(joined.state(h.me()), KeyState::Active);
        let host = host(&h, &[joined.me()]);
        let mut conn = host.dialed_by(&joined).await.unwrap();
        served(&mut conn, joined.me()).await;
    }

    #[tokio::test]
    async fn a_key_that_is_not_ed25519_is_answered_400_and_the_code_still_works() {
        let host = machine_keys();
        let port = serve(host.clone()).await;
        let code = host.new_code().unwrap().to_string();
        let machine = MachineId::random();
        let request = JoinRequest {
            code: code.clone(),
            machine,
            key: PublicKey("ab".into()),
        };
        let url = format!("http://127.0.0.1:{port}{JOIN_PATH}");
        let answer =
            tokio::task::spawn_blocking(move || ureq::post(&url).send_json(&request).map(drop))
                .await
                .unwrap();
        assert!(
            matches!(answer, Err(ureq::Error::StatusCode(400))),
            "{answer:?}"
        );
        assert_eq!(host.state(machine), KeyState::NoKey);
        join_over_http(port, &code, Arc::new(MemorySecrets::default()))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn an_answer_naming_this_machine_is_refused_and_kept_out_of_its_table() {
        let secrets = Arc::new(MemorySecrets::default());
        let me = Keys::open(secrets.clone()).unwrap();
        let stranger = machine_keys();
        let answer = JoinAnswer {
            machine: me.me(),
            key: stranger.public_key(),
        };
        let answer = serde_json::to_value(&answer).unwrap();
        let app = Router::new().route(
            JOIN_PATH,
            post(move || {
                let answer = answer.clone();
                async move { Json(answer) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await });
        let before = secrets.get(KEYS_SECRET).unwrap();
        let joined = join_over_http(port, "ABCDEFGHJK", secrets.clone()).await;
        assert!(
            matches!(joined, Err(JoinError::AnsweredAsThisMachine(_))),
            "{joined:?}"
        );
        assert_eq!(secrets.get(KEYS_SECRET).unwrap(), before);
    }

    #[tokio::test(start_paused = true)]
    async fn a_host_key_that_is_not_ed25519_is_never_held() {
        let secrets = Arc::new(MemorySecrets::default());
        Keys::open(secrets.clone()).unwrap();
        let before = secrets.get(KEYS_SECRET).unwrap();
        for bad in ["", "ab", "zz"] {
            let held = hold_key(&*secrets, MachineId::random(), PublicKey(bad.into()));
            assert!(matches!(held, Err(JoinError::BadKey)), "{bad:?}: {held:?}");
            assert_eq!(secrets.get(KEYS_SECRET).unwrap(), before);
        }
    }

    #[tokio::test]
    async fn a_host_this_machine_holds_revoked_is_named_in_the_join_error() {
        let h = machine_keys();
        let port = serve(h.clone()).await;
        let secrets = Arc::new(MemorySecrets::default());
        Keys::open(secrets.clone()).unwrap().revoke(h.me()).unwrap();
        let code = h.new_code().unwrap().to_string();
        let joined = join_over_http(port, &code, secrets).await;
        assert!(
            matches!(joined, Err(JoinError::HostRefusedHere(m)) if m == h.me()),
            "{joined:?}"
        );
    }

    #[tokio::test]
    async fn a_revoked_machine_joining_again_is_told_it_is_revoked() {
        let host = machine_keys();
        let port = serve(host.clone()).await;
        let secrets = Arc::new(MemorySecrets::default());
        let code = host.new_code().unwrap().to_string();
        let joined = join_over_http(port, &code, secrets.clone()).await.unwrap();
        host.revoke(joined).unwrap();
        let code = host.new_code().unwrap().to_string();
        let again = join_over_http(port, &code, secrets).await;
        assert!(
            matches!(again, Err(JoinError::Taken(m)) if m == joined),
            "{again:?}"
        );
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
        for (address, authority) in [
            ("100.64.0.1", "100.64.0.1:7433"),
            ("100.64.0.1:7777", "100.64.0.1:7777"),
            ("fd7a:115c:a1e0::1", "[fd7a:115c:a1e0::1]:7433"),
            ("[fd7a:115c:a1e0::1]", "[fd7a:115c:a1e0::1]:7433"),
            ("[fd7a:115c:a1e0::1]:7777", "[fd7a:115c:a1e0::1]:7777"),
            ("trilogy-mac", "trilogy-mac:7433"),
            ("trilogy-mac:80", "trilogy-mac:80"),
        ] {
            assert_eq!(join_authority(address, 7433), authority);
        }
    }

    #[test]
    fn only_tailscale_and_loopback_addresses_are_joinable() {
        for (ip, ok) in [
            ("100.64.0.0", true),
            ("100.127.255.255", true),
            ("100.63.255.255", false),
            ("100.128.0.0", false),
            ("127.0.0.1", true),
            ("192.168.1.20", false),
            ("::1", true),
            ("fd7a:115c:a1e0::1", true),
            ("fd7a:115c:a1e0:ffff::1", true),
            ("fd7a:115c:a1e1::1", false),
            ("2001:db8::1", false),
        ] {
            assert_eq!(joinable(ip.parse().unwrap()), ok, "{ip}");
        }
    }

    #[test]
    fn a_join_to_an_address_outside_tailscale_is_refused_and_keeps_nothing() {
        for address in [
            "192.0.2.1",
            "198.51.100.7:7433",
            "100.128.0.1",
            "[2001:db8::1]",
            "fd7a:115c:a1e1::1",
        ] {
            let secrets = MemorySecrets::default();
            let joined = join_at(address, DEFAULT_PORT, "CODE", &secrets);
            assert!(
                matches!(&joined, Err(JoinError::NotTailscale(a)) if a == address),
                "{address}: {joined:?}"
            );
            assert_eq!(secrets.get(IDENTITY_SECRET).unwrap(), None, "{address}");
            assert_eq!(secrets.get(KEYS_SECRET).unwrap(), None, "{address}");
        }
    }

    #[tokio::test]
    async fn a_host_name_is_resolved_and_joined_at_its_loopback_address() {
        let host = machine_keys();
        let port = serve(host.clone()).await;
        let code = host.new_code().unwrap().to_string();
        // Where localhost resolves to ::1 first (macOS), nothing listens there, so this also
        // shows a join moving on to the next address.
        let joined = tokio::task::spawn_blocking(move || {
            let secrets = MemorySecrets::default();
            join_at(&format!("localhost:{port}"), DEFAULT_PORT, &code, &secrets)
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(host.state(joined), KeyState::Active);
    }
}
