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
//! The key table is held in this machine's secret store. It holds only public keys. Each
//! machine's public key also travels in its machine-list entry, and every machine admits the
//! keys its list carries ([`Keys::follow`]), so a machine admitted by one is admitted by all.
//! A machine removed from the list is revoked by every machine that holds its key
//! ([`Keys::refuse_removed`]), so its links close; listing its key again never re-admits it.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError};
use std::time::{Duration, SystemTime};

use axum::extract::{ConnectInfo, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use foundry_proto::ids::MachineId;
use foundry_proto::messages::PeerMessage;
use foundry_proto::settings::{Machine, SettingKey, SharedSettings, StampKey};
use futures::future::BoxFuture;
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::time::{Instant, timeout};

use crate::api::machine_add::Lister;
use crate::api::machine_names;
use crate::secrets::{self, Secrets, SecretsError};
use crate::store::Store;
use crate::sync::{Conn, Dialer, OFFLINE_AFTER, Peers, on_store};

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
    let own = match own_entry() {
        Ok(own) => Some(own),
        Err(why) => {
            eprintln!(
                "foundryd join: {why}; the machine that admits this one lists it under the name \
                 typed there, which it keeps until it is renamed on the machines board"
            );
            None
        }
    };
    match join_at(address, port, code, &*secrets, own.as_ref()) {
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

/// What this machine lists itself under, read from Tailscale as the daemon reads it.
fn own_entry() -> Result<machine_names::Entry, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("cannot ask Tailscale what this machine is called: {e}"))?
        .block_on(crate::daemon::own_entry())
}

/// The data directory: `FOUNDRY_HOME`, else `~/.local/share/foundry`.
fn home() -> Option<PathBuf> {
    match std::env::var_os("FOUNDRY_HOME") {
        Some(home) => Some(home.into()),
        None => std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share/foundry")),
    }
}

/// Posts `code` with this machine's id and public key, and the entry it lists itself under
/// (`own`) when there is one, to the machine at `address` (`port` unless the address names
/// one), keeps the admitting machine's key in this machine's key table, and returns this
/// machine's id. The admitting machine lists this one before it answers and before it keeps
/// this one's key; when it cannot, it keeps nothing here and the same command can be run again. The identity in `secrets` is
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
    own: Option<&machine_names::Entry>,
) -> Result<MachineId, JoinError> {
    let targets = join_targets(address, port)?;
    let identity = Identity::load_or_create(secrets)?;
    let request = JoinRequest {
        code: code.to_owned(),
        machine: identity.machine,
        key: identity.key_pair()?.public(),
        name: own.map(|own| own.name.clone()),
        address: own.map(|own| own.address.clone()),
    };
    // The answer waits for the admitting machine to list this one, which can wait on its
    // acting coordinator for half a minute.
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_global(Some(Duration::from_secs(90)))
        .http_status_as_error(false)
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
    let mut response = match agent.post(url).send_json(request) {
        Ok(response) => response,
        Err(error) => {
            return Err(JoinError::Unreachable {
                url: url.to_owned(),
                reason: error.to_string(),
            });
        }
    };
    match response.status().as_u16() {
        200 => response
            .body_mut()
            .read_json()
            .map_err(|_| JoinError::NotAdmitted {
                url: url.to_owned(),
                status: 200,
            }),
        403 => Err(JoinError::BadCode),
        409 => Err(JoinError::Taken(request.machine)),
        422 => Err(JoinError::EntryRefused {
            url: url.to_owned(),
            reason: reason_given(&mut response),
        }),
        503 => Err(JoinError::NotListed {
            url: url.to_owned(),
            reason: reason_given(&mut response),
        }),
        status => Err(JoinError::NotAdmitted {
            url: url.to_owned(),
            status,
        }),
    }
}

/// The `error` of a join answer's `{"error"}` body, without its control characters: what
/// another machine says is printed, so it moves no terminal's cursor.
fn reason_given(response: &mut ureq::http::Response<ureq::Body>) -> String {
    #[derive(Deserialize)]
    struct Refusal {
        error: String,
    }
    response.body_mut().read_json::<Refusal>().map_or_else(
        |_| "no reason was given".to_owned(),
        |refusal| refusal.error.chars().filter(|c| !c.is_control()).collect(),
    )
}

/// The Tailscale and loopback addresses `address` names or resolves to, at `port` unless it
/// names one. [`JoinError::NotTailscale`] when it has none.
pub(crate) fn join_targets(address: &str, port: u16) -> Result<Vec<SocketAddr>, JoinError> {
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
    #[error("the code is wrong, used or expired, or made for another machine; ask for a new one")]
    BadCode,
    #[error(
        "the code was made for another machine; run the command on the machine named in the \
         Add machine dialog"
    )]
    OtherMachine,
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
    #[error(
        "{url} could not add this machine to the machine list: {reason}; run the same command \
         again, which also finishes the join if the machine shows on the board all the same"
    )]
    NotListed { url: String, reason: String },
    #[error("{url} will not list this machine under the name and address it sent: {reason}")]
    EntryRefused { url: String, reason: String },
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
    /// The key as hex, as a machine-list entry carries it.
    pub fn hex(&self) -> &str {
        &self.0
    }

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

/// The machine a code was made for, in the Add machine dialog: the name typed there and the
/// first address it resolved to. A redeem of the code lists the machine under these when the
/// joining machine does not say what it lists itself under.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Named {
    pub name: String,
    pub address: IpAddr,
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
    /// The machines listed with a key that does not parse, each warned about once.
    unparsed: Mutex<BTreeSet<MachineId>>,
    /// Counts the key tables saved, so a link waiting to learn its machine was revoked hears of
    /// it at once instead of at its next check.
    changed: tokio::sync::watch::Sender<u64>,
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
    /// For a code made for a named machine: the name typed and the address it resolved to.
    named: Option<Named>,
    /// The addresses the code may be used from, or empty for any. A code made for a named
    /// machine works only from an address that name resolved to.
    from: Vec<IpAddr>,
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

/// The machines `settings` shows removed from the machine list: each with a `Machine` stamp
/// and no list entry. The one definition of removed: a machine removed and added again has the
/// stamp and an entry, and is not.
pub fn removed(settings: &SharedSettings) -> impl Iterator<Item = MachineId> + '_ {
    settings.stamps.keys().filter_map(|key| match key {
        StampKey::Setting(SettingKey::Machine(id))
            if !settings.machines.iter().any(|m| m.id == *id) =>
        {
            Some(*id)
        }
        _ => None,
    })
}

/// Whether `settings` shows `machine` removed from the machine list: see [`removed`].
pub fn is_removed(settings: &SharedSettings, machine: MachineId) -> bool {
    removed(settings).any(|id| id == machine)
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
                unparsed: Mutex::new(BTreeSet::new()),
                changed: tokio::sync::watch::channel(0).0,
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
        self.issue(None, Vec::new())
    }

    /// A new code, as [`Keys::new_code`], made for the machine `named`, that works only from
    /// one of `from`. Each redeem of it gives `named` back.
    pub(crate) fn new_named_code(
        &self,
        named: Named,
        from: Vec<IpAddr>,
    ) -> Result<Code, JoinError> {
        self.issue(Some(named), from)
    }

    fn issue(&self, named: Option<Named>, from: Vec<IpAddr>) -> Result<Code, JoinError> {
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
            named,
            from: from.into_iter().map(|ip| ip.to_canonical()).collect(),
        });
        Ok(Code(code))
    }

    /// Holds `code`, sent from address `from`, for `machine` with public key `key`, and saves
    /// nothing. It refuses all that admitting the machine would: a code that is wrong, expired
    /// or another machine's, a code made for a named machine sent from any other address, a
    /// key that is not Ed25519, and, on the stored table, a machine held revoked or with
    /// another key ([`admit_into`]). From then on the code works for no other machine, and
    /// this machine may send it again while it is valid, as a join whose answer was lost
    /// does. Gives the machine the code was made for, for a code made for a named machine.
    ///
    /// The join route holds the code, lists the machine, and only then keeps its key
    /// ([`Keys::keep`]). So a join that is not listed, or whose sender hangs up while the
    /// listing waits, has saved no key.
    ///
    /// Blocks on the secret store, as [`Keys::open`] does.
    pub(crate) fn reserve(
        &self,
        code: &str,
        machine: MachineId,
        key: &PublicKey,
        from: IpAddr,
    ) -> Result<Option<Named>, JoinError> {
        let typed = code.trim().to_ascii_uppercase();
        // Read before the codes are locked, so a slow secret store never holds up a link.
        let mut stored = read_table(&*self.inner.secrets)?;
        let mut held = self.held();
        held.forget_expired_codes();
        let shown = held.shown(&typed).ok_or(JoinError::BadCode)?;
        if shown.used_by.is_some_and(|user| user != machine) {
            return Err(JoinError::BadCode);
        }
        if !shown.from.is_empty() && !shown.from.contains(&from.to_canonical()) {
            return Err(JoinError::OtherMachine);
        }
        // On a copy: this only asks whether the machine could be admitted.
        admit_into(&mut stored, machine, key)?;
        shown.used_by = Some(machine);
        Ok(shown.named.clone())
    }

    /// Keeps `key` for `machine`, by [`admit_into`]'s rule on the stored table. It asks for no
    /// code: the caller holds one for the machine ([`Keys::reserve`]), and a code that
    /// expires while the machine is listed must not undo the join.
    ///
    /// Blocks on the secret store, as [`Keys::open`] does, and behind any other change to the
    /// key table in this process.
    pub(crate) fn keep(&self, machine: MachineId, key: &PublicKey) -> Result<(), JoinError> {
        let saving = self.saving()?;
        self.change(&saving, |machines| admit_into(machines, machine, key))
    }

    /// Uses `code`, sent from address `from`, to admit `machine` with public key `key`:
    /// [`Keys::reserve`], then [`Keys::keep`]. When the key cannot be saved, the code stays
    /// that machine's to send again.
    ///
    /// Blocks on the secret store, as [`Keys::open`] does, and behind any other change to the
    /// key table in this process.
    pub fn redeem(
        &self,
        code: &str,
        machine: MachineId,
        key: &PublicKey,
        from: IpAddr,
    ) -> Result<Option<Named>, JoinError> {
        let named = self.reserve(code, machine, key, from)?;
        self.keep(machine, key)?;
        Ok(named)
    }

    /// Admits, by [`admit_into`]'s rule, each machine in `machines` listed with a key that
    /// parses as an Ed25519 public key and that the key table does not hold. A revocation or
    /// another key held is never replaced. An entry with no key, from a machine before keys
    /// were listed, is skipped; one whose key does not parse is skipped with a warning, once
    /// per machine, that names the machine and not the key. Saves only when there is a machine
    /// to admit.
    ///
    /// Blocks on the secret store, as [`Keys::open`] does, and behind any other change to the
    /// key table in this process.
    pub fn learn(&self, machines: &[Machine]) -> Result<(), JoinError> {
        let unheld: Vec<(MachineId, PublicKey)> = {
            let held = self.held();
            machines
                .iter()
                .filter(|m| !m.key.is_empty() && !held.machines.contains_key(&m.id))
                .map(|m| (m.id, PublicKey(m.key.clone())))
                .collect()
        };
        let mut admitting = Vec::new();
        for (machine, key) in unheld {
            if key.verifier().is_some() {
                admitting.push((machine, key));
            } else if lock(&self.inner.unparsed).insert(machine) {
                tracing::warn!(
                    "join: machine {machine} is listed with a key that is not an Ed25519 public \
                     key, so it is not admitted"
                );
            }
        }
        if admitting.is_empty() {
            return Ok(());
        }
        let saving = self.saving()?;
        self.change(&saving, |machines| {
            for (machine, key) in &admitting {
                // A revocation or another key held stays as it is.
                let _ = admit_into(machines, *machine, key);
            }
            Ok(())
        })
    }

    /// Revokes each machine `settings` shows removed from the machine list, a machine with a
    /// `Machine` stamp and no list entry, that the key table holds active. This machine is never
    /// revoked, and a machine held no key for, or already revoked, stays as it is. All are
    /// revoked in one save, and nothing is saved when there is nothing to revoke.
    ///
    /// Blocks on the secret store, as [`Keys::open`] does, and behind any other change to the
    /// key table in this process.
    pub fn refuse_removed(&self, settings: &SharedSettings) -> Result<(), JoinError> {
        let me = self.me();
        let held: Vec<MachineId> = {
            let table = self.held();
            removed(settings)
                .filter(|id| *id != me)
                .filter(|id| matches!(table.machines.get(id), Some(Entry::Active { .. })))
                .collect()
        };
        if held.is_empty() {
            return Ok(());
        }
        let saving = self.saving()?;
        self.change(&saving, |machines| {
            for id in &held {
                // Only a key held active is revoked, as the stored table has it now.
                if matches!(machines.get(id), Some(Entry::Active { .. })) {
                    machines.insert(*id, Entry::Revoked);
                }
            }
            Ok(())
        })
    }

    /// Acts on the machine list in `store` whenever the settings change, checked every
    /// [`RECHECK`], until the returned future is dropped: first [`Keys::refuse_removed`], then
    /// admits the keys the list carries by [`Keys::learn`], so a machine removed is never
    /// admitted again from the list. A change it could not save is tried again on the next
    /// check, and warned about once until one succeeds.
    pub async fn follow(&self, store: Arc<Mutex<Store>>) {
        let mut seen = None;
        let mut warned = false;
        let mut every = tokio::time::interval(RECHECK);
        loop {
            every.tick().await;
            let prev = seen;
            let changed = on_store(&store, move |store| store.settings_since(prev)).await;
            let settings = match changed {
                Ok(Some(settings)) => settings,
                Ok(None) => continue,
                Err(error) => {
                    tracing::warn!("join: cannot read the machine list for its keys: {error}");
                    continue;
                }
            };
            let seq = settings.seq;
            let keys = self.clone();
            let acted = tokio::task::spawn_blocking(move || {
                keys.refuse_removed(&settings)?;
                keys.learn(&settings.machines)
            })
            .await;
            match acted {
                Ok(Ok(())) => {
                    seen = Some(seq);
                    warned = false;
                }
                Ok(Err(error)) => {
                    if !warned {
                        tracing::warn!(
                            "join: cannot update the keys from the machine list, trying again: \
                             {error}"
                        );
                    }
                    warned = true;
                }
                Err(error) => {
                    if !warned {
                        tracing::warn!(
                            "join: updating the keys from the machine list stopped, trying \
                             again: {error}"
                        );
                    }
                    warned = true;
                }
            }
        }
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
        self.inner.changed.send_modify(|saved| *saved += 1);
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

    /// The code shown as `typed`, compared in constant time.
    fn shown(&mut self, typed: &str) -> Option<&mut Shown> {
        self.codes
            .iter_mut()
            .find(|shown| same(shown.code.as_bytes(), typed.as_bytes()))
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

/// Returns once `keys` no longer admits `peer`, checked every [`RECHECK`] and whenever this
/// process saves the key table, so a link with it selected against this closes as soon as the
/// machine is revoked here.
async fn revoked(keys: &Keys, peer: MachineId) {
    let mut every = tokio::time::interval(RECHECK);
    let mut saved = keys.inner.changed.subscribe();
    loop {
        tokio::select! {
            _ = every.tick() => {}
            _ = saved.changed() => {}
        }
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
/// public key when the machine is admitted, 403 to a code that is wrong, used or expired, or
/// made for a machine at another address, 409 to a machine revoked or held with another key,
/// and 400 to a key that is not an Ed25519 public key. It reads the sender's address, so it
/// must be served with `into_make_service_with_connect_info::<SocketAddr>()`, as the daemon
/// serves every route.
///
/// A code made for a named machine, by the add route, also lists the machine through
/// `lister` before the answer, with its key: under the name and address the request sends, the
/// entry the joining machine lists itself under, or, from a `foundryd join` that sends no name
/// or no address, under the name typed and the address it resolved to. A listing that fails is
/// `503` `{"error"}`. The machine's key is kept only once it is listed, so a join answered
/// `503`, or one whose sender hangs up first, has saved no key, and the code stays valid for
/// that machine. An edit that got no answer may be numbered after all: the machine is then
/// listed, every machine admits its key from the list, and the same join sent again finishes.
/// A key table that cannot be read or saved is `503` `{"error"}` too, for any code: the code
/// is still that machine's, and a machine listed by then stays listed. A sent name with a control character, or a sent
/// address that is not a Tailscale or loopback IP, which a machine's own entry never holds, is
/// `422` `{"error"}`, before the code is held or anything is listed: sending the same again
/// cannot work. A code made by [`Keys::new_code`] lists nothing.
pub fn router(keys: Keys, lister: Lister) -> Router {
    Router::new()
        .route(JOIN_PATH, post(redeem))
        .with_state(Arc::new(Joining { keys, lister }))
}

/// What the join route acts on.
struct Joining {
    keys: Keys,
    lister: Lister,
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
    /// The name the joining machine lists itself under; none from a `foundryd join` before it
    /// sent one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    /// The address the joining machine lists itself at; none as for `name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    address: Option<String>,
}

impl JoinRequest {
    /// The entry the machine lists itself under, as it sent it, or none when it sent no name
    /// or no address (a blank one counts as none). The rule is the one a machine's own entry
    /// keeps ([`entry`](crate::api::machine_names::entry)): a name of any length with no
    /// control character, at a Tailscale or loopback IP. One that breaks it gives why.
    fn sent(&self) -> Result<Option<Machine>, String> {
        let sent = |field: &Option<String>| field.clone().filter(|f| !f.trim().is_empty());
        let (Some(name), Some(address)) = (sent(&self.name), sent(&self.address)) else {
            return Ok(None);
        };
        if name.chars().any(char::is_control) {
            return Err(format!(
                "the name this machine sent, {name:?}, holds a control character"
            ));
        }
        let joins = address
            .parse::<IpAddr>()
            .is_ok_and(|ip| joinable(ip.to_canonical()));
        if !joins {
            return Err(format!(
                "the address this machine sent, {address:?}, is not a Tailscale or loopback IP"
            ));
        }
        Ok(Some(self.listed(name, address)))
    }

    fn listed(&self, name: String, address: String) -> Machine {
        Machine {
            id: self.machine,
            name,
            address,
            key: self.key.hex().to_owned(),
        }
    }
}

async fn redeem(
    State(joining): State<Arc<Joining>>,
    ConnectInfo(sender): ConnectInfo<SocketAddr>,
    Json(request): Json<JoinRequest>,
) -> Response {
    // Refused before the code is held for the machine, so a retry can only find what it found.
    let sent = match request.sent() {
        Ok(sent) => sent,
        Err(why) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(serde_json::json!({ "error": why })),
            )
                .into_response();
        }
    };
    let keys = joining.keys.clone();
    let answer = JoinAnswer {
        machine: keys.me(),
        key: keys.public_key(),
    };
    // Reading and saving the key table can wait on the disk.
    let (code, machine, key) = (request.code.clone(), request.machine, request.key.clone());
    let held = {
        let keys = keys.clone();
        tokio::task::spawn_blocking(move || keys.reserve(&code, machine, &key, sender.ip())).await
    };
    let kept = match held {
        Ok(Ok(named)) => {
            // Listed before the key is kept: a join that is not listed keeps nothing.
            if let Some(named) = named {
                let entry =
                    sent.unwrap_or_else(|| request.listed(named.name, named.address.to_string()));
                if let Err(error) = joining.lister.list(entry).await {
                    return (
                        StatusCode::SERVICE_UNAVAILABLE,
                        Json(serde_json::json!({ "error": error })),
                    )
                        .into_response();
                }
            }
            let key = request.key.clone();
            tokio::task::spawn_blocking(move || keys.keep(machine, &key)).await
        }
        Ok(Err(error)) => Ok(Err(error)),
        Err(error) => Err(error),
    };
    match kept {
        Ok(Ok(())) => Json(answer).into_response(),
        Ok(Err(error @ (JoinError::BadCode | JoinError::OtherMachine))) => {
            (StatusCode::FORBIDDEN, error.to_string()).into_response()
        }
        Ok(Err(error @ JoinError::Taken(_))) => {
            (StatusCode::CONFLICT, error.to_string()).into_response()
        }
        Ok(Err(error @ JoinError::BadKey)) => {
            (StatusCode::BAD_REQUEST, error.to_string()).into_response()
        }
        // The code is still that machine's, and it may be listed by now: sent again, the
        // same join finishes.
        Ok(Err(error)) => {
            tracing::warn!("join: a machine's key could not be stored: {error}");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({ "error": "it could not store this machine's key" })),
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
    use foundry_proto::settings::{Machine, SettingKey, SharedSettings, Stamp, StampKey};
    use futures::future::BoxFuture;
    use tokio::time::timeout;

    use super::*;
    use crate::secrets::{Edit, FileSecrets, MemorySecrets, Secrets, SecretsError};
    use crate::store::Store;
    use crate::sync::{Conn, Dialer, Peers};

    /// Where a redeem in these tests comes from.
    const HERE: IpAddr = IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);

    /// A machine's keys over a fresh in-memory secret store.
    fn machine_keys() -> Keys {
        Keys::open(Arc::new(MemorySecrets::default())).unwrap()
    }

    /// `host` admits `peer` with a fresh code, as `foundryd join` does.
    fn admit(host: &Keys, peer: &Keys) {
        let code = host.new_code().unwrap().to_string();
        host.redeem(&code, peer.me(), &peer.public_key(), HERE)
            .unwrap();
    }

    /// Two machines that hold each other's key, as they will once keys reach every machine.
    fn pair() -> (Keys, Keys) {
        let (a, b) = (machine_keys(), machine_keys());
        admit(&a, &b);
        admit(&b, &a);
        (a, b)
    }

    /// A secret store whose writes fail while `failing` is set, and that runs `during_save`
    /// once, in the middle of the next write.
    #[derive(Default)]
    struct Flaky {
        inner: MemorySecrets,
        failing: AtomicBool,
        during_save: Mutex<Option<Box<dyn FnOnce() + Send>>>,
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
            let during = self.during_save.lock().unwrap().take();
            if let Some(during) = during {
                during();
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
        host.redeem(&code, peer.me(), &peer.public_key(), HERE)
            .unwrap();
        assert_eq!(host.state(peer.me()), KeyState::Active);
        let again = host.redeem(&code, other.me(), &other.public_key(), HERE);
        assert!(matches!(again, Err(JoinError::BadCode)), "{again:?}");
        assert_eq!(host.state(other.me()), KeyState::NoKey);
    }

    #[tokio::test(start_paused = true)]
    async fn the_machine_that_used_a_code_may_send_it_again_when_the_answer_was_lost() {
        let (host, peer) = (machine_keys(), machine_keys());
        let code = host.new_code().unwrap().to_string();
        host.redeem(&code, peer.me(), &peer.public_key(), HERE)
            .unwrap();
        host.redeem(&code, peer.me(), &peer.public_key(), HERE)
            .unwrap();
        assert_eq!(host.state(peer.me()), KeyState::Active);
    }

    #[tokio::test(start_paused = true)]
    async fn a_code_works_for_ten_minutes_and_not_after() {
        let (host, early, late) = (machine_keys(), machine_keys(), machine_keys());
        let early_code = host.new_code().unwrap().to_string();
        let late_code = host.new_code().unwrap().to_string();
        tokio::time::advance(CODE_VALID_FOR - Duration::from_secs(1)).await;
        host.redeem(&early_code, early.me(), &early.public_key(), HERE)
            .unwrap();
        tokio::time::advance(Duration::from_secs(1)).await;
        let expired = host.redeem(&late_code, late.me(), &late.public_key(), HERE);
        assert!(matches!(expired, Err(JoinError::BadCode)), "{expired:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_code_typed_in_lower_case_with_spaces_around_it_works() {
        let (host, peer) = (machine_keys(), machine_keys());
        let code = host.new_code().unwrap().to_string();
        let typed = format!(" {} ", code.to_lowercase());
        host.redeem(&typed, peer.me(), &peer.public_key(), HERE)
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_code_never_issued_is_refused() {
        let (host, peer) = (machine_keys(), machine_keys());
        host.new_code().unwrap();
        for wrong in ["ABCDEFGHJK", ""] {
            let refused = host.redeem(wrong, peer.me(), &peer.public_key(), HERE);
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
            let claimed = host.redeem(&code, taken, &thief.public_key(), HERE);
            assert!(matches!(claimed, Err(JoinError::Taken(_))), "{claimed:?}");
        }
        host.revoke(peer.me()).unwrap();
        let code = host.new_code().unwrap().to_string();
        let rejoined = host.redeem(&code, peer.me(), &peer.public_key(), HERE);
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

    /// The name typed in the Add machine dialog for a code, and the address it resolved to.
    fn named(address: IpAddr) -> Named {
        Named {
            name: "localhost".into(),
            address,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_named_code_gives_its_name_at_every_redeem_and_a_plain_code_gives_none() {
        let (host, peer) = (machine_keys(), machine_keys());
        let code = host.new_named_code(named(HERE), vec![HERE]).unwrap();
        let code = code.to_string();

        for _ in 0..2 {
            let used = host.redeem(&code, peer.me(), &peer.public_key(), HERE);
            assert_eq!(used.unwrap(), Some(named(HERE)));
        }
        let plain = host.new_code().unwrap().to_string();
        let other = machine_keys();
        let used = host.redeem(&plain, other.me(), &other.public_key(), HERE);
        assert_eq!(used.unwrap(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_code_held_for_a_machine_saves_no_key_and_works_for_no_other_machine() {
        let secrets = Arc::new(MemorySecrets::default());
        let host = Keys::open(secrets.clone()).unwrap();
        let (peer, other) = (machine_keys(), machine_keys());
        let before = secrets.get(KEYS_SECRET).unwrap();
        let code = host.new_named_code(named(HERE), vec![HERE]).unwrap();
        let code = code.to_string();

        let held = host.reserve(&code, peer.me(), &peer.public_key(), HERE);

        assert_eq!(held.unwrap(), Some(named(HERE)));
        assert_eq!(host.state(peer.me()), KeyState::NoKey);
        assert_eq!(secrets.get(KEYS_SECRET).unwrap(), before);
        let claimed = host.reserve(&code, other.me(), &other.public_key(), HERE);
        assert!(matches!(claimed, Err(JoinError::BadCode)), "{claimed:?}");
        // The machine it is held for sends it again, as after a lost answer.
        let again = host.reserve(&code, peer.me(), &peer.public_key(), HERE);
        assert_eq!(again.unwrap(), Some(named(HERE)));
    }

    #[tokio::test(start_paused = true)]
    async fn a_code_is_held_for_no_machine_that_could_not_be_admitted() {
        let secrets = Arc::new(MemorySecrets::default());
        let host = Keys::open(secrets.clone()).unwrap();
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
        let elsewhere: IpAddr = "100.64.0.5".parse().unwrap();
        let code = host.new_named_code(named(HERE), vec![HERE]).unwrap();
        let code = code.to_string();

        for (machine, key, from) in [
            (held.me(), thief.public_key(), HERE),
            (revoked.me(), revoked.public_key(), HERE),
            (newcomer.me(), PublicKey("ab".into()), HERE),
            (newcomer.me(), newcomer.public_key(), elsewhere),
        ] {
            let refused = host.reserve(&code, machine, &key, from);
            assert!(
                matches!(
                    refused,
                    Err(JoinError::Taken(_) | JoinError::BadKey | JoinError::OtherMachine)
                ),
                "{machine} from {from}: {refused:?}"
            );
        }
        // None of them took the code.
        let free = host.reserve(&code, newcomer.me(), &newcomer.public_key(), HERE);
        assert_eq!(free.unwrap(), Some(named(HERE)));
    }

    #[tokio::test(start_paused = true)]
    async fn a_key_is_kept_for_a_machine_whose_code_expired_while_it_was_listed() {
        let (host, peer) = (machine_keys(), machine_keys());
        let code = host.new_code().unwrap().to_string();
        host.reserve(&code, peer.me(), &peer.public_key(), HERE)
            .unwrap();
        tokio::time::advance(CODE_VALID_FOR + Duration::from_secs(1)).await;

        host.keep(peer.me(), &peer.public_key()).unwrap();

        assert_eq!(host.state(peer.me()), KeyState::Active);
    }

    #[tokio::test(start_paused = true)]
    async fn a_named_code_whose_save_failed_gives_its_name_once_a_save_succeeds() {
        let flaky = Arc::new(Flaky::default());
        let host = Keys::open(flaky.clone()).unwrap();
        let peer = machine_keys();
        let code = host.new_named_code(named(HERE), vec![HERE]).unwrap();
        flaky.failing.store(true, Ordering::SeqCst);
        assert!(
            host.redeem(&code.to_string(), peer.me(), &peer.public_key(), HERE)
                .is_err()
        );
        flaky.failing.store(false, Ordering::SeqCst);
        let used = host.redeem(&code.to_string(), peer.me(), &peer.public_key(), HERE);
        assert_eq!(used.unwrap(), Some(named(HERE)));
    }

    #[tokio::test(start_paused = true)]
    async fn a_named_code_that_expires_while_its_key_is_saved_still_gives_its_name() {
        let flaky = Arc::new(Flaky::default());
        let host = Keys::open(flaky.clone()).unwrap();
        let peer = machine_keys();
        let code = host.new_named_code(named(HERE), vec![HERE]).unwrap();
        // While the key is saved, the code expires and an add forgets it.
        let adding = host.clone();
        *flaky.during_save.lock().unwrap() = Some(Box::new(move || {
            for shown in &mut adding.held().codes {
                shown.wall -= CODE_VALID_FOR + Duration::from_secs(1);
            }
            adding.new_code().unwrap();
        }));

        let used = host.redeem(&code.to_string(), peer.me(), &peer.public_key(), HERE);

        assert_eq!(used.unwrap(), Some(named(HERE)));
        assert_eq!(host.state(peer.me()), KeyState::Active);
    }

    #[tokio::test(start_paused = true)]
    async fn a_code_made_for_a_named_machine_is_refused_from_another_address_and_saves_nothing() {
        let (host, peer) = (machine_keys(), machine_keys());
        let named: IpAddr = "100.64.0.5".parse().unwrap();
        let code = host
            .new_named_code(
                super::Named {
                    name: "m".into(),
                    address: named,
                },
                vec![named],
            )
            .unwrap();
        let code = code.to_string();

        for elsewhere in ["127.0.0.1", "100.64.0.9", "::ffff:100.64.0.9"] {
            let refused = host.redeem(
                &code,
                peer.me(),
                &peer.public_key(),
                elsewhere.parse().unwrap(),
            );
            assert!(
                matches!(refused, Err(JoinError::OtherMachine)),
                "{elsewhere}: {refused:?}"
            );
        }
        assert_eq!(host.state(peer.me()), KeyState::NoKey);

        // The code is unspent, and works from the named machine, over IPv4 or mapped IPv6.
        let mapped: IpAddr = "::ffff:100.64.0.5".parse().unwrap();
        host.redeem(&code, peer.me(), &peer.public_key(), mapped)
            .unwrap();
        assert_eq!(host.state(peer.me()), KeyState::Active);
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_save_admits_nobody_and_the_same_machine_may_send_the_code_again() {
        let flaky = Arc::new(Flaky::default());
        let host = Keys::open(flaky.clone()).unwrap();
        let peer = machine_keys();
        let code = host.new_code().unwrap().to_string();
        flaky.failing.store(true, Ordering::SeqCst);
        let failed = host.redeem(&code, peer.me(), &peer.public_key(), HERE);
        assert!(matches!(failed, Err(JoinError::Secrets(_))), "{failed:?}");
        assert_eq!(host.state(peer.me()), KeyState::NoKey);
        flaky.failing.store(false, Ordering::SeqCst);
        host.redeem(&code, peer.me(), &peer.public_key(), HERE)
            .unwrap();
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
                scope.spawn(move || {
                    host.redeem(code, peer.me(), &peer.public_key(), HERE)
                        .unwrap()
                });
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
        host.redeem(&code, peer.me(), &peer.public_key(), HERE)
            .unwrap();
        assert_eq!(host.state(peer.me()), KeyState::Active);
        let spent = host.redeem(&code, other.me(), &other.public_key(), HERE);
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
            let late = host.redeem(&code, peer.me(), &peer.public_key(), HERE);
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
                let joined = join_at("127.0.0.1:1", DEFAULT_PORT, "CODE", &*secrets, None);
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
            let first = scope.spawn(|| host.redeem(&code, x.me(), &x.public_key(), HERE));
            while gated.waiting.load(Ordering::SeqCst) == 0 {
                std::thread::yield_now();
            }
            let second = scope.spawn(|| {
                started.store(true, Ordering::SeqCst);
                host.redeem(&code, y.me(), &y.public_key(), HERE)
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
            let claimed = daemon.redeem(&code, machine, &key, HERE);
            assert!(
                matches!(claimed, Err(JoinError::Taken(m)) if m == machine),
                "{claimed:?}"
            );
            assert_eq!(secrets.get(KEYS_SECRET).unwrap(), before);
        }
        daemon
            .redeem(&code, newcomer.me(), &newcomer.public_key(), HERE)
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
        let redeemed = keys.redeem(&code, newcomer.me(), &newcomer.public_key(), HERE);
        assert!(
            matches!(redeemed, Err(JoinError::Unreadable(KEYS_SECRET))),
            "{redeemed:?}"
        );
        assert_eq!(
            secrets.get(KEYS_SECRET).unwrap().as_deref(),
            Some("not json")
        );
        secrets.set(KEYS_SECRET, &table).unwrap();
        keys.redeem(&code, newcomer.me(), &newcomer.public_key(), HERE)
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

    // ---- keys learned from the machine list ----

    /// An entry in the machine list for `id`, carrying `key`.
    fn listed(id: MachineId, key: &str) -> Machine {
        Machine {
            key: key.to_owned(),
            ..machine(id)
        }
    }

    /// The key `keys` holds for `machine`, if it holds one active.
    fn held_key(keys: &Keys, machine: MachineId) -> Option<PublicKey> {
        match keys.held().machines.get(&machine) {
            Some(Entry::Active { key }) => Some(key.clone()),
            _ => None,
        }
    }

    /// Every warning logged while the returned guard is held, a line each.
    fn capture_warnings() -> (Arc<Mutex<Vec<u8>>>, tracing::subscriber::DefaultGuard) {
        #[derive(Clone)]
        struct Into(Arc<Mutex<Vec<u8>>>);
        impl io::Write for Into {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let logged = Arc::new(Mutex::new(Vec::new()));
        let writer = Into(logged.clone());
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .without_time()
            .finish();
        (logged, tracing::subscriber::set_default(subscriber))
    }

    fn warnings(logged: &Mutex<Vec<u8>>) -> Vec<String> {
        String::from_utf8_lossy(&logged.lock().unwrap())
            .lines()
            .filter(|line| line.contains("WARN"))
            .map(str::to_owned)
            .collect()
    }

    #[tokio::test(start_paused = true)]
    async fn a_key_in_the_machine_list_is_admitted_and_saved() {
        let secrets = Arc::new(MemorySecrets::default());
        let host = Keys::open(secrets.clone()).unwrap();
        let peer = machine_keys();

        host.learn(&[listed(peer.me(), peer.public_key().hex())])
            .unwrap();

        assert_eq!(held_key(&host, peer.me()), Some(peer.public_key()));
        let reopened = Keys::open(secrets).unwrap();
        assert_eq!(held_key(&reopened, peer.me()), Some(peer.public_key()));
    }

    #[tokio::test]
    async fn a_listed_key_whose_save_failed_is_admitted_once_a_save_succeeds() {
        let flaky = Arc::new(Flaky::default());
        let host = Keys::open(flaky.clone()).unwrap();
        let peer = machine_keys();
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("foundry.db")).unwrap();
        let settings = SharedSettings {
            seq: 1,
            machines: vec![listed(peer.me(), peer.public_key().hex())],
            ..SharedSettings::default()
        };
        assert!(store.apply_settings(settings).unwrap());
        let store = Arc::new(Mutex::new(store));
        flaky.failing.store(true, Ordering::SeqCst);
        let following = tokio::spawn({
            let host = host.clone();
            async move { host.follow(store).await }
        });

        tokio::time::sleep(RECHECK * 3).await;
        assert_eq!(host.state(peer.me()), KeyState::NoKey);
        // The settings do not change again: the next check tries the same list.
        flaky.failing.store(false, Ordering::SeqCst);
        let deadline = tokio::time::Instant::now() + RECHECK * 5;
        while host.state(peer.me()) != KeyState::Active {
            assert!(tokio::time::Instant::now() < deadline, "never admitted");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        following.abort();
    }

    /// Settings whose list holds `listed` and that stamp every machine in `removed` as removed
    /// from it.
    fn settings_with(seq: u64, listed: Vec<Machine>, removed: &[MachineId]) -> SharedSettings {
        let mut settings = SharedSettings {
            seq,
            machines: listed,
            ..SharedSettings::default()
        };
        for (n, id) in removed.iter().enumerate() {
            settings.stamps.insert(
                StampKey::Setting(SettingKey::Machine(*id)),
                Stamp {
                    seq: n as u64 + 1,
                    by: *id,
                },
            );
        }
        settings
    }

    #[test]
    fn a_machine_removed_from_the_list_and_held_active_is_revoked_and_saved() {
        let secrets = Arc::new(MemorySecrets::default());
        let host = Keys::open(secrets.clone()).unwrap();
        let (gone, kept) = (machine_keys(), machine_keys());
        admit(&host, &gone);
        admit(&host, &kept);
        let settings = settings_with(
            2,
            vec![listed(kept.me(), kept.public_key().hex())],
            &[gone.me()],
        );

        host.refuse_removed(&settings).unwrap();

        assert_eq!(host.state(gone.me()), KeyState::Revoked);
        assert_eq!(host.state(kept.me()), KeyState::Active);
        let reopened = Keys::open(secrets).unwrap();
        assert_eq!(reopened.state(gone.me()), KeyState::Revoked);
    }

    #[test]
    fn this_machine_never_revokes_itself_even_when_stamped_removed() {
        let host = machine_keys();
        let settings = settings_with(2, vec![], &[host.me()]);

        host.refuse_removed(&settings).unwrap();

        assert_eq!(host.state(host.me()), KeyState::Active);
    }

    #[test]
    fn a_removed_machine_with_no_key_or_a_revocation_is_left_as_it_is() {
        let host = machine_keys();
        let (unknown, revoked) = (MachineId::random(), machine_keys());
        admit(&host, &revoked);
        host.revoke(revoked.me()).unwrap();
        let settings = settings_with(2, vec![], &[unknown, revoked.me()]);

        host.refuse_removed(&settings).unwrap();

        assert_eq!(host.state(unknown), KeyState::NoKey);
        assert_eq!(host.state(revoked.me()), KeyState::Revoked);
    }

    #[test]
    fn nothing_is_saved_when_no_removed_machine_is_held_active() {
        let flaky = Arc::new(Flaky::default());
        let host = Keys::open(flaky.clone()).unwrap();
        let revoked = machine_keys();
        admit(&host, &revoked);
        host.revoke(revoked.me()).unwrap();
        let settings = settings_with(2, vec![], &[MachineId::random(), revoked.me(), host.me()]);
        let mut saved = host.inner.changed.subscribe();
        // Any save would fail, and bump the signal when it did not.
        flaky.failing.store(true, Ordering::SeqCst);

        host.refuse_removed(&settings).unwrap();

        assert!(!saved.has_changed().unwrap());
        assert_eq!(host.state(host.me()), KeyState::Active);
    }

    #[test]
    fn a_machine_still_in_the_list_is_not_revoked_whatever_its_stamp() {
        let host = machine_keys();
        let peer = machine_keys();
        admit(&host, &peer);
        // Removed, then added again: the stamp stays, the list holds it.
        let settings = settings_with(
            3,
            vec![listed(peer.me(), peer.public_key().hex())],
            &[peer.me()],
        );

        host.refuse_removed(&settings).unwrap();

        assert_eq!(host.state(peer.me()), KeyState::Active);
    }

    #[tokio::test]
    async fn a_machine_revoked_on_removal_stays_revoked_when_listed_again_with_its_key() {
        let host = machine_keys();
        let peer = machine_keys();
        admit(&host, &peer);
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("foundry.db")).unwrap();
        assert!(
            store
                .apply_settings(settings_with(2, vec![], &[peer.me()]))
                .unwrap()
        );
        let store = Arc::new(Mutex::new(store));
        let following = tokio::spawn({
            let host = host.clone();
            let store = store.clone();
            async move { host.follow(store).await }
        });
        let deadline = tokio::time::Instant::now() + RECHECK * 5;
        while host.state(peer.me()) != KeyState::Revoked {
            assert!(tokio::time::Instant::now() < deadline, "never revoked");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // A later copy lists it again with its key.
        let later = settings_with(
            3,
            vec![listed(peer.me(), peer.public_key().hex())],
            &[peer.me()],
        );
        assert!(store.lock().unwrap().apply_settings(later.clone()).unwrap());
        tokio::time::sleep(RECHECK * 3).await;
        assert_eq!(host.state(peer.me()), KeyState::Revoked);
        host.learn(&later.machines).unwrap();
        host.refuse_removed(&later).unwrap();
        assert_eq!(host.state(peer.me()), KeyState::Revoked);
        following.abort();
    }

    #[tokio::test]
    async fn a_revocation_that_cannot_be_saved_is_tried_again_on_the_next_check() {
        let flaky = Arc::new(Flaky::default());
        let host = Keys::open(flaky.clone()).unwrap();
        let gone = machine_keys();
        admit(&host, &gone);
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("foundry.db")).unwrap();
        assert!(
            store
                .apply_settings(settings_with(2, vec![], &[gone.me()]))
                .unwrap()
        );
        let store = Arc::new(Mutex::new(store));
        flaky.failing.store(true, Ordering::SeqCst);
        let following = tokio::spawn({
            let host = host.clone();
            async move { host.follow(store).await }
        });

        tokio::time::sleep(RECHECK * 3).await;
        assert_eq!(host.state(gone.me()), KeyState::Active);
        flaky.failing.store(false, Ordering::SeqCst);
        let deadline = tokio::time::Instant::now() + RECHECK * 5;
        while host.state(gone.me()) != KeyState::Revoked {
            assert!(tokio::time::Instant::now() < deadline, "never revoked");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        following.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn a_link_closes_at_once_when_its_machine_is_revoked_not_at_the_next_check() {
        let host = machine_keys();
        let peer = machine_keys();
        admit(&host, &peer);
        let closing = tokio::spawn({
            let (host, peer) = (host.clone(), peer.me());
            async move { revoked(&host, peer).await }
        });
        // Past the first check, so the next is a RECHECK away.
        tokio::time::sleep(RECHECK / 4).await;
        assert!(!closing.is_finished());

        host.revoke(peer.me()).unwrap();

        timeout(RECHECK / 4, closing)
            .await
            .expect("the link closes without waiting for the next check")
            .unwrap();
    }

    #[test]
    fn removed_names_the_machines_stamped_removed_and_not_listed() {
        let (gone, kept) = (MachineId::random(), MachineId::random());
        let settings = settings_with(3, vec![machine(kept)], &[gone, kept]);

        assert_eq!(removed(&settings).collect::<Vec<_>>(), [gone]);
        assert!(is_removed(&settings, gone));
        assert!(!is_removed(&settings, kept));
        assert!(!is_removed(&settings, MachineId::random()));
    }

    #[tokio::test(start_paused = true)]
    async fn a_listed_key_never_replaces_a_revocation_or_another_key_held() {
        let host = machine_keys();
        let (revoked, held, stranger) = (machine_keys(), machine_keys(), machine_keys());
        admit(&host, &revoked);
        host.revoke(revoked.me()).unwrap();
        admit(&host, &held);

        host.learn(&[
            listed(revoked.me(), revoked.public_key().hex()),
            listed(held.me(), stranger.public_key().hex()),
        ])
        .unwrap();

        assert_eq!(host.state(revoked.me()), KeyState::Revoked);
        assert_eq!(held_key(&host, held.me()), Some(held.public_key()));
    }

    #[tokio::test(start_paused = true)]
    async fn a_listed_key_never_replaces_a_revocation_another_process_stored() {
        let secrets = Arc::new(MemorySecrets::default());
        let host = Keys::open(secrets.clone()).unwrap();
        let peer = machine_keys();
        // Another process, after this one read the table, stored the peer revoked.
        let mut stored = parse_table(secrets.get(KEYS_SECRET).unwrap()).unwrap();
        stored.insert(peer.me(), Entry::Revoked);
        secrets.set(KEYS_SECRET, &write_table(&stored)).unwrap();

        host.learn(&[listed(peer.me(), peer.public_key().hex())])
            .unwrap();

        assert_eq!(host.state(peer.me()), KeyState::Revoked);
        let kept = parse_table(secrets.get(KEYS_SECRET).unwrap()).unwrap();
        assert!(matches!(kept.get(&peer.me()), Some(Entry::Revoked)));
    }

    #[tokio::test(start_paused = true)]
    async fn a_listed_key_that_does_not_parse_is_skipped_with_one_warning_naming_the_machine() {
        let (logged, _capture) = capture_warnings();
        let host = machine_keys();
        let (odd, older, peer) = (MachineId::random(), MachineId::random(), machine_keys());
        // Hex, but 16 bytes: not an Ed25519 public key.
        let bad = "c0ffee".repeat(5) + "c0";
        let list = [
            listed(odd, &bad),
            listed(older, ""),
            listed(peer.me(), peer.public_key().hex()),
        ];

        host.learn(&list).unwrap();
        host.learn(&list).unwrap();

        assert_eq!(host.state(odd), KeyState::NoKey);
        assert_eq!(host.state(older), KeyState::NoKey);
        assert_eq!(host.state(peer.me()), KeyState::Active);
        let warned = warnings(&logged);
        assert_eq!(warned.len(), 1, "{warned:?}");
        assert!(warned[0].contains(&odd.to_string()), "{warned:?}");
        assert!(
            !warned[0].contains(&bad),
            "the key reached the log: {warned:?}"
        );
    }

    fn machine(id: MachineId) -> Machine {
        Machine {
            id,
            name: format!("m-{id}"),
            address: "127.0.0.1".into(),
            key: String::new(),
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
        serve_acting(keys).await.0
    }

    /// The port of `keys`' join route, on a machine that is listed and acts alone, so it
    /// numbers the edit that lists a joining machine, and its store.
    async fn serve_acting(keys: Keys) -> (u16, Arc<Mutex<Store>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("foundry.db")).unwrap();
        let acting = SharedSettings {
            seq: 1,
            machines: vec![Machine {
                id: keys.me(),
                name: "host".into(),
                address: "100.64.0.1".into(),
                key: keys.public_key().hex().to_owned(),
            }],
            priority: vec![keys.me()],
            ..SharedSettings::default()
        };
        assert!(store.apply_settings(acting).unwrap());
        let store = Arc::new(Mutex::new(store));
        let peers = Peers::new(keys.me(), store.clone(), Arc::new(Pairs::default()));
        let coordinator = Arc::new(crate::coordinator::Coordinator::new(
            keys.me(),
            store.clone(),
            peers.clone(),
        ));
        peers.coordinate(coordinator.clone());
        let lister = Lister::new(coordinator, store.clone());
        tokio::spawn(async move {
            let _dir = dir;
            axum::serve(
                listener,
                router(keys, lister).into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
        });
        (port, store)
    }

    async fn join_over_http(
        port: u16,
        code: &str,
        secrets: Arc<MemorySecrets>,
    ) -> Result<MachineId, JoinError> {
        let code = code.to_owned();
        tokio::task::spawn_blocking(move || {
            join_at(
                &format!("127.0.0.1:{port}"),
                DEFAULT_PORT,
                &code,
                &*secrets,
                None,
            )
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
            name: None,
            address: None,
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
            matches!(failed, Err(JoinError::NotListed { .. })),
            "{failed:?}"
        );
    }

    #[tokio::test]
    async fn a_machine_listed_whose_key_could_not_be_saved_is_told_to_run_it_again_and_then_joins()
    {
        let flaky = Arc::new(Flaky::default());
        let host = Keys::open(flaky.clone()).unwrap();
        let (port, store) = serve_acting(host.clone()).await;
        let code = host.new_named_code(named(HERE), vec![HERE]).unwrap();
        let code = code.to_string();
        let secrets = Arc::new(MemorySecrets::default());
        flaky.failing.store(true, Ordering::SeqCst);

        let failed = join_over_http(port, &code, secrets.clone()).await;

        let Err(error @ JoinError::NotListed { .. }) = failed else {
            panic!("{failed:?}");
        };
        let shown = error.to_string();
        assert!(shown.contains("could not store"), "{shown}");
        assert!(shown.contains("run the same command again"), "{shown}");
        // Listed with its key all the same, and neither machine holds the other's yet.
        let me = Keys::open(secrets.clone()).unwrap();
        let listed = lock(&store).settings().unwrap().machines;
        assert!(
            listed
                .iter()
                .any(|m| m.id == me.me() && m.key == me.public_key().hex()),
            "{listed:?}"
        );
        assert_eq!(host.state(me.me()), KeyState::NoKey);
        assert_eq!(me.state(host.me()), KeyState::NoKey);

        flaky.failing.store(false, Ordering::SeqCst);
        let joined = join_over_http(port, &code, secrets.clone()).await;
        assert_eq!(joined.unwrap(), me.me());
        assert_eq!(host.state(me.me()), KeyState::Active);
        let me = Keys::open(secrets).unwrap();
        assert_eq!(me.state(host.me()), KeyState::Active);
    }

    /// The port of a join route that answers every join with `status` and `body`.
    async fn answering(status: u16, body: &'static str) -> u16 {
        let app = Router::new().route(
            JOIN_PATH,
            post(move || async move { (StatusCode::from_u16(status).unwrap(), body) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await });
        port
    }

    /// An answer's error with a terminal title, a bell, a screen clear and a line break in it.
    const LOUD: &str = r#"{"error":"\u001b]0;pwned\u0007\u001b[2Jthe list\nis busy"}"#;
    const QUIET: &str = "]0;pwned[2Jthe listis busy";

    #[tokio::test]
    async fn a_503_reason_is_given_without_its_control_characters_and_says_to_run_it_again() {
        let port = answering(503, LOUD).await;
        let joined = join_over_http(port, "ABCDEFGHJK", Arc::default()).await;
        let Err(error @ JoinError::NotListed { .. }) = joined else {
            panic!("{joined:?}");
        };
        assert!(
            matches!(&error, JoinError::NotListed { reason, .. } if reason == QUIET),
            "{error:?}"
        );
        let shown = error.to_string();
        assert!(!shown.chars().any(char::is_control), "{shown:?}");
        assert!(shown.contains("run the same command again"), "{shown}");
    }

    #[tokio::test]
    async fn a_503_or_a_422_whose_body_is_not_the_error_object_says_no_reason_was_given() {
        for status in [503, 422] {
            let port = answering(status, "busy\u{1b}[2J").await;
            let joined = join_over_http(port, "ABCDEFGHJK", Arc::default()).await;
            assert!(
                matches!(
                    &joined,
                    Err(JoinError::NotListed { reason, .. } | JoinError::EntryRefused { reason, .. })
                        if reason == "no reason was given"
                ),
                "{status}: {joined:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_422_reason_is_given_without_its_control_characters_and_not_as_worth_a_retry() {
        let port = answering(422, LOUD).await;
        let joined = join_over_http(port, "ABCDEFGHJK", Arc::default()).await;
        let Err(error @ JoinError::EntryRefused { .. }) = joined else {
            panic!("{joined:?}");
        };
        assert!(
            matches!(&error, JoinError::EntryRefused { reason, .. } if reason == QUIET),
            "{error:?}"
        );
        let shown = error.to_string();
        assert!(!shown.chars().any(char::is_control), "{shown:?}");
        assert!(!shown.contains("again"), "{shown}");
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
            let joined = join_at(address, DEFAULT_PORT, "CODE", &secrets, None);
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
            join_at(
                &format!("localhost:{port}"),
                DEFAULT_PORT,
                &code,
                &secrets,
                None,
            )
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(host.state(joined), KeyState::Active);
    }
}
