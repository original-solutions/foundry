//! The HTTP and WebSocket API the UI talks to.
//!
//! - `GET /api/state`: the full synced state, as a [`UiMessage::State`].
//! - `GET /api/events`, a WebSocket: a [`UiMessage::State`] first, then a
//!   [`UiMessage::Record`] for each record that changes, deleted ones included, and a
//!   [`UiMessage::Settings`] when the shared settings change. When a machine goes offline or
//!   comes back, the whole state is sent again, so its sessions and terminals show as
//!   unreachable or as they are. The store is read every [`EVENTS_EVERY`], so a record written
//!   several times between two reads is sent once, at its newest version. A record this build
//!   cannot read, one a newer build wrote before a rollback, leaves out its owner's changes up
//!   to it, with a warning, and the socket goes on.
//! - `GET /api/terminals/{id}`, a WebSocket: see [`terminals`].
//! - `GET /api/machine`: `{id}`, the machine that serves the UI, see [`machine`].
//! - `GET /api/machines`: `{me, machines: [{id, name, online}]}`, this machine's id and each
//!   machine in the machine list, in its order, with whether it is online.
//! - `POST /api/workspaces` `{project, base, name, machine}`: makes a workspace, `201 {id}`, on
//!   the machine `machine` names, or on this machine when it is absent. When the workspace is
//!   made but its setup could not be started, it is still `201`, with an `error` beside the
//!   `id` saying so: the workspace exists, and the id is how to reach it. A machine named, or
//!   this one, is used even when it is over its limits, and the answer then has a `warning`
//!   naming each limit it is over.
//!
//!   `"machine": "choose"` lets [`placement`](crate::placement) pick the machine, and takes the
//!   `model` (and optional `prompt`) of the session a waiting request starts; no other create
//!   takes them. When a machine qualifies, the workspace is made there as a create naming it
//!   is, and the answer is that create's with `machine` beside it. When none does, it is `202
//!   {request, reasons}` and the request waits on this machine: see [`placement`](crate::placement)
//!   for it and for `GET /api/placements` and `POST /api/placements/{id}/cancel`.
//! - `POST /api/workspaces/{id}/archive` `{force}` (the body is optional): `204`, or `409` with
//!   the uncommitted files, unpushed commits and submodules that hold it up.
//! - `POST /api/workspaces/{id}/sessions` `{model, prompt}` (`prompt` is optional): starts
//!   Claude Code in the workspace, on `prompt` as its first prompt when one is given, `201 {id}`.
//!   A blank prompt is none; one with a NUL or over 20,000 characters is `422`.
//! - `POST /api/workspaces/{id}/shells`: starts the daemon's `$SHELL` in the workspace, `201
//!   {id}`.
//! - `POST /api/terminals/{id}/close`: `204`.
//!
//!   These five actions, on a workspace or terminal another machine owns or a create that
//!   names another machine, are carried to that machine and answered as it answers: see
//!   [`carry`].
//! - `POST /api/terminals/{id}/files?name=<name>`: keeps a file for the terminal and answers
//!   its path, see [`files`].
//! - `GET /api/workspaces/{id}/changes`: what the workspace changed since its base, see
//!   [`changes`]. Only a workspace of this machine; any other is `404`.
//! - `GET /api/workspaces/{id}/commits/{hash}`: what one commit on the workspace's branch
//!   changed, see [`changes`]. Only a workspace of this machine.
//! - `GET /api/workspaces/{id}/pr`: whether the workspace's branch is on `origin`, what is only
//!   on this machine, and its pull request, see [`pr`]. Only a workspace of this machine.
//! - `GET /api/briefs`: what each session of this machine is for and doing, see [`briefs`].
//! - `GET /api/sessions/{id}/trail`: every step a session of this machine took, see [`trail`].
//!
//! - Any other path outside `/api`: the web UI, see [`assets::serve`].
//!
//! An error is `{"error": "..."}` with a status that says what kind it was, a request axum
//! refuses (a body that is not the JSON wanted, an unknown route or method, a socket path asked
//! for without an upgrade) included.
//!
//! The server is bound by [`bind`], which refuses any address but loopback and Tailscale's.
//! A browser can still be made to call it from any page it has open, so every request must
//! name this machine in its `Host`: a loopback or Tailscale address, `localhost`, or a DNS name
//! in [`Api::names`] ([`trusted_host`]). No other name is trusted, so a page on a name an
//! attacker's DNS points at this machine is refused. A request a browser sends from another
//! site, which it marks with an `Origin` that is not the `Host`, is refused too. Without that, any web page could open a
//! terminal socket and type into a shell.
//!
//! Nothing asks who the caller is: every machine and process that can reach the address is
//! trusted, as the spec decides ("Access is anyone on the tailnet until a second person needs
//! the app"). The checks above keep out web pages, not peers.
//!
//! No response holds a secret: records and settings carry none, and the password or token in
//! a URL, as git puts one in an error or a project's repo may hold one, is hidden by
//! [`redact`].

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::rejection::JsonRejection;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{self, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequest, Path, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use foundry_proto::ids::{MachineId, ProjectId, SessionId, TerminalId, WorkspaceId};
use foundry_proto::messages::{PROTOCOL, UiMessage};
use foundry_proto::settings::{SettingValue, SharedSettings};
use serde::Deserialize;
use tokio::net::TcpListener;

use crate::coordinator::CoordinatorError;
use crate::placement::{self, Queue, Want};
use crate::secrets::redact;
use crate::sessions::{SessionError, Sessions};
use crate::store::{Store, StoreError};
use crate::sync::Peers;
use crate::sync::call::CallError;
use crate::terminals::{TerminalError, Terminals};
use crate::workspaces::{WorkspaceError, Workspaces};

pub mod assets;
mod briefs;
mod carry;
pub(crate) use carry::carries;
mod changes;
mod files;
mod machine;
pub mod machine_add;
pub mod machine_limits;
pub mod machine_names;
mod pr;
pub mod projects;
pub mod terminals;
#[cfg(test)]
mod tests;
mod trail;

/// How often the events socket reads the store for changes.
pub const EVENTS_EVERY: Duration = Duration::from_millis(200);

/// What the API acts on: this machine's store, peers, terminals, workspaces and sessions.
#[derive(Clone)]
pub struct Api {
    pub store: Arc<Mutex<Store>>,
    pub peers: Peers,
    pub terminals: Arc<Terminals>,
    pub workspaces: Arc<Workspaces>,
    pub sessions: Arc<Sessions>,
    /// The DNS names this machine answers to besides `localhost` and its addresses, such as
    /// its MagicDNS short name and its `<name>.<tailnet>.ts.net` name. The daemon fills it;
    /// empty means only addresses and `localhost` are trusted in a `Host`.
    pub names: Vec<String>,
    /// Why sessions cannot start on this machine, said as a `503` to a session start for one of
    /// its own workspaces: a daemon with no gateway. `None` until the daemon sets it. A start
    /// for another machine's workspace is still carried there.
    pub sessions_refused: Option<String>,
    /// The creates waiting for a machine to qualify, which [`placement::run`] places.
    pub placements: Arc<Queue>,
}

impl Api {
    pub fn new(
        store: Arc<Mutex<Store>>,
        peers: Peers,
        terminals: Arc<Terminals>,
        workspaces: Arc<Workspaces>,
        sessions: Arc<Sessions>,
        names: Vec<String>,
    ) -> Self {
        let placements = Arc::new(Queue::new(peers.me()));
        Self {
            store,
            peers,
            terminals,
            workspaces,
            sessions,
            sessions_refused: None,
            placements,
            names,
        }
    }
}

/// Every route of the API. Serve it on a listener from [`bind`].
pub fn router(api: Api) -> Router {
    Router::new()
        .route("/api/state", get(state))
        .route("/api/machines", get(machines))
        .route("/api/events", get(events))
        .route("/api/terminals/{id}", get(terminals::socket))
        .route("/api/terminals/{id}/close", post(close_terminal))
        .route(
            "/api/terminals/{id}/files",
            post(files::keep).layer(axum::extract::DefaultBodyLimit::max(files::MAX_FILE)),
        )
        .route("/api/workspaces", post(create_workspace))
        .route("/api/workspaces/{id}/archive", post(archive_workspace))
        .route("/api/workspaces/{id}/sessions", post(start_session))
        .route("/api/workspaces/{id}/shells", post(start_shell))
        .route("/api/workspaces/{id}/changes", get(changes::changes))
        .route("/api/workspaces/{id}/commits/{hash}", get(changes::commit))
        .route("/api/workspaces/{id}/pr", get(pr::shipping))
        .route("/api/briefs", get(briefs::briefs))
        .route("/api/machine", get(machine::machine))
        .route("/api/sessions/{id}/trail", get(trail::trail))
        .route("/api/placements", get(placement::list))
        .route("/api/placements/{id}/cancel", post(placement::cancel))
        .fallback(assets::serve)
        .method_not_allowed_fallback(wrong_method)
        .layer(middleware::from_fn_with_state(api.clone(), carry::carry))
        .layer(middleware::from_fn_with_state(api.clone(), same_site))
        .with_state(api)
}

async fn no_route() -> Response {
    error(StatusCode::NOT_FOUND, "no such route".to_owned())
}

async fn wrong_method() -> Response {
    error(
        StatusCode::METHOD_NOT_ALLOWED,
        "this route does not take that method".to_owned(),
    )
}

// --- binding ---

/// The rule [`bind`] keeps, as its error states it.
const BIND_RULE: &str = "foundryd listens only on loopback (127.0.0.0/8, ::1) or a Tailscale \
                         address (100.64.0.0/10, fd7a:115c:a1e0::/48)";

#[derive(Debug, thiserror::Error)]
pub enum BindError {
    #[error("{BIND_RULE}; {0} is neither")]
    NotAllowed(IpAddr),
    #[error("cannot listen on {addr}: {source}")]
    Io { addr: SocketAddr, source: io::Error },
}

/// True for a loopback address and an address in Tailscale's ranges, the only ones the daemon
/// listens on. An IPv4 address written as IPv6 is judged as IPv4.
pub fn allowed(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(v4) => v4.is_loopback() || in_v4(v4, Ipv4Addr::new(100, 64, 0, 0), 10),
        IpAddr::V6(v6) => {
            v6.is_loopback() || in_v6(v6, Ipv6Addr::new(0xfd7a, 0x115c, 0xa1e0, 0, 0, 0, 0, 0), 48)
        }
    }
}

fn in_v4(ip: Ipv4Addr, net: Ipv4Addr, bits: u32) -> bool {
    let mask = u32::MAX << (32 - bits);
    u32::from(ip) & mask == u32::from(net) & mask
}

fn in_v6(ip: Ipv6Addr, net: Ipv6Addr, bits: u32) -> bool {
    let mask = u128::MAX << (128 - bits);
    u128::from(ip) & mask == u128::from(net) & mask
}

/// Listens on `addr`, which must be [`allowed`]. Any other address, `0.0.0.0` included, is
/// refused with the rule.
pub async fn bind(addr: SocketAddr) -> Result<TcpListener, BindError> {
    if !allowed(addr.ip()) {
        return Err(BindError::NotAllowed(addr.ip()));
    }
    TcpListener::bind(addr)
        .await
        .map_err(|source| BindError::Io { addr, source })
}

// --- callers ---

/// True when `host`, a `Host` header, names this machine: an [`allowed`] address, `localhost`,
/// or one of `names` (ASCII case-insensitive, one trailing dot ignored). Any other name is
/// refused, a MagicDNS-looking one included: a page whose own name was pointed at this
/// machine's address, by a search domain or LLMNR, names that.
fn trusted_host(host: &str, names: &[String]) -> bool {
    let name = match host.strip_prefix('[') {
        Some(rest) => match rest.split_once(']') {
            Some((ip, _)) => ip,
            None => return false,
        },
        None => match host.rsplit_once(':') {
            Some((name, port)) if port.bytes().all(|b| b.is_ascii_digit()) => name,
            _ => host,
        },
    };
    if let Ok(ip) = name.parse::<IpAddr>() {
        return allowed(ip);
    }
    let name = name.strip_suffix('.').unwrap_or(name).to_ascii_lowercase();
    !name.is_empty()
        && (name == "localhost"
            || names.iter().any(|own| {
                own.strip_suffix('.')
                    .unwrap_or(own)
                    .eq_ignore_ascii_case(&name)
            }))
}

/// Refuses a request whose `Host` is not [`trusted_host`], and one a browser sent from another
/// site: an `Origin` whose host and port are not the `Host`. The daemon puts it on every route
/// the UI calls that [`router`] does not hold.
pub(crate) async fn same_site(State(api): State<Api>, request: Request, next: Next) -> Response {
    let headers = request.headers();
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !trusted_host(host, &api.names) {
        return refuse(format!(
            "foundryd answers only to a loopback or Tailscale address or one of its own names; \
             the request names {host:?}"
        ));
    }
    if let Some(origin) = headers.get(header::ORIGIN) {
        let origin = origin.to_str().unwrap_or("");
        let from = origin.split_once("://").map_or("", |(_, rest)| rest);
        if !from.eq_ignore_ascii_case(host) {
            return refuse(format!(
                "a request from another site is refused: it came from {origin:?}, not {host:?}"
            ));
        }
    }
    next.run(request).await
}

fn refuse(message: String) -> Response {
    error(StatusCode::FORBIDDEN, message)
}

// --- secrets ---

fn redacted(texts: &[String]) -> Vec<String> {
    texts.iter().map(|t| redact(t)).collect()
}

/// `settings` as the UI is shown them: each project's repo with [`redact`] applied, in the
/// projects list and in both values of a "setting overwritten" item.
fn shown(mut settings: SharedSettings) -> SharedSettings {
    for project in &mut settings.projects {
        project.repo = redact(&project.repo);
    }
    for item in settings.overwritten.values_mut() {
        for value in [&mut item.kept, &mut item.lost].into_iter().flatten() {
            if let SettingValue::Project(project) = value {
                project.repo = redact(&project.repo);
            }
        }
    }
    settings
}

// --- errors ---

/// Why a request failed, answered as `{"error": "..."}` with its status.
enum ApiError {
    /// A path names nothing here.
    NotFound(String),
    Workspace(WorkspaceError),
    Session(SessionError),
    Terminal(TerminalError),
    Store(StoreError),
    /// A request axum refused before a handler ran: a body that is not the JSON wanted, or a
    /// socket path asked for without an upgrade. Keeps axum's status.
    Rejected(StatusCode, String),
}

impl From<JsonRejection> for ApiError {
    fn from(e: JsonRejection) -> Self {
        Self::Rejected(e.status(), e.body_text())
    }
}

impl From<WebSocketUpgradeRejection> for ApiError {
    fn from(e: WebSocketUpgradeRejection) -> Self {
        Self::Rejected(e.status(), e.body_text())
    }
}

/// A JSON body of type `T`, whose refusal is answered as `{"error": "..."}`.
struct Body<T>(T);

impl<T, S> FromRequest<S> for Body<T>
where
    Json<T>: FromRequest<S, Rejection = JsonRejection>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Self, ApiError> {
        Ok(Self(Json::<T>::from_request(request, state).await?.0))
    }
}

impl From<WorkspaceError> for ApiError {
    fn from(e: WorkspaceError) -> Self {
        Self::Workspace(e)
    }
}

impl From<SessionError> for ApiError {
    fn from(e: SessionError) -> Self {
        Self::Session(e)
    }
}

impl From<TerminalError> for ApiError {
    fn from(e: TerminalError) -> Self {
        Self::Terminal(e)
    }
}

impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}

fn terminal_status(e: &TerminalError) -> StatusCode {
    match e {
        TerminalError::Unknown(_) => StatusCode::NOT_FOUND,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            Self::Workspace(WorkspaceError::Dirty(dirty)) => {
                let body = serde_json::json!({
                    "error": redact(&dirty.to_string()),
                    "uncommitted": redacted(&dirty.uncommitted),
                    "unpushed": redacted(&dirty.unpushed),
                    "submodules": redacted(&dirty.submodules),
                });
                return (StatusCode::CONFLICT, Json(body)).into_response();
            }
            Self::Workspace(e) => (
                match e {
                    WorkspaceError::Unknown(_) | WorkspaceError::UnknownCommit(_) => {
                        StatusCode::NOT_FOUND
                    }
                    WorkspaceError::UnknownProject(_) | WorkspaceError::UnknownBase(_) => {
                        StatusCode::UNPROCESSABLE_ENTITY
                    }
                    WorkspaceError::BranchExists(_) => StatusCode::CONFLICT,
                    WorkspaceError::Terminal(e) => terminal_status(e),
                    _ => StatusCode::INTERNAL_SERVER_ERROR,
                },
                e.to_string(),
            ),
            Self::Session(e) => (
                match e {
                    SessionError::Terminal(e) => terminal_status(e),
                    _ => StatusCode::INTERNAL_SERVER_ERROR,
                },
                e.to_string(),
            ),
            Self::NotFound(message) => (StatusCode::NOT_FOUND, message.clone()),
            Self::Terminal(e) => (terminal_status(e), e.to_string()),
            Self::Store(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
            Self::Rejected(status, message) => (*status, message.clone()),
        };
        let message = redact(&message);
        if status.is_server_error() {
            tracing::warn!("api: {message}");
        }
        error(status, message)
    }
}

fn error(status: StatusCode, message: String) -> Response {
    (status, Json(serde_json::json!({ "error": message }))).into_response()
}

/// The status a route answers with when an edit it sent through the coordinator failed: a
/// dismissal ([`crate::attention`]) and a project added or removed ([`projects`]). One mapping,
/// so every shared-settings route answers a failure alike.
pub(crate) fn edit_status(error: &CoordinatorError) -> StatusCode {
    match error {
        CoordinatorError::Refused { .. } => StatusCode::BAD_GATEWAY,
        CoordinatorError::Conflict { .. } => StatusCode::CONFLICT,
        // Sent, so it may have been numbered: not the 503 of an edit that changed nothing.
        CoordinatorError::NoAnswer { .. } => StatusCode::GATEWAY_TIMEOUT,
        CoordinatorError::NotLinked(_)
        | CoordinatorError::Older { .. }
        | CoordinatorError::Unsettled { .. } => StatusCode::SERVICE_UNAVAILABLE,
        CoordinatorError::Unwritable(_) | CoordinatorError::Store(_) => {
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

fn created(id: impl ToString) -> Response {
    (
        StatusCode::CREATED,
        Json(serde_json::json!({ "id": id.to_string() })),
    )
        .into_response()
}

/// An id in a path that does not parse names nothing here.
fn parse_id<T>(text: &str, what: &str, make: fn(uuid::Uuid) -> T) -> Result<T, ApiError> {
    text.parse()
        .map(make)
        .map_err(|_| ApiError::NotFound(format!("no {what} {text:?} here")))
}

/// Runs `work` with the store on the blocking pool, so a busy store never stalls an async
/// worker.
async fn on_store<T: Send + 'static>(
    store: &Arc<Mutex<Store>>,
    work: impl FnOnce(&mut Store) -> Result<T, StoreError> + Send + 'static,
) -> Result<T, StoreError> {
    let store = Arc::clone(store);
    match tokio::task::spawn_blocking(move || {
        work(&mut store.lock().unwrap_or_else(PoisonError::into_inner))
    })
    .await
    {
        Ok(result) => result,
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        // The runtime is shutting down, and drops the task that waits here with it.
        Err(_) => std::future::pending().await,
    }
}

// --- state and events ---

/// The full state as [`UiMessage::State`], and the highest version it holds per owner.
async fn full_state_for(
    api: &Api,
    online: &BTreeSet<MachineId>,
) -> Result<(UiMessage, BTreeMap<MachineId, u64>), StoreError> {
    let (versions, records, settings) = api.peers.state_snapshot_for(online).await?;
    let state = UiMessage::State {
        protocol: PROTOCOL,
        records,
        settings: shown(settings),
    };
    Ok((state, versions))
}

async fn state(State(api): State<Api>) -> Result<Json<UiMessage>, ApiError> {
    let online = api.peers.online();
    Ok(Json(full_state_for(&api, &online).await?.0))
}

async fn machines(State(api): State<Api>) -> Result<Json<serde_json::Value>, ApiError> {
    let settings = on_store(&api.store, |store| store.settings()).await?;
    let online = api.peers.online();
    let machines: Vec<serde_json::Value> = settings
        .machines
        .iter()
        .map(|m| serde_json::json!({"id": m.id, "name": m.name, "online": online.contains(&m.id)}))
        .collect();
    Ok(Json(
        serde_json::json!({ "me": api.peers.me(), "machines": machines }),
    ))
}

async fn events(
    State(api): State<Api>,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Result<Response, ApiError> {
    Ok(upgrade?.on_upgrade(move |socket| async move {
        if let Err(error) = send_events(&api, socket).await {
            tracing::warn!("api: events socket ended: {}", redact(&error.to_string()));
        }
    }))
}

/// What the events socket has sent: the highest version per owner, the settings' `seq` and
/// which machines were online.
struct Sent {
    versions: BTreeMap<MachineId, u64>,
    seq: u64,
    online: BTreeSet<MachineId>,
}

/// Sends the state, then every change, until the UI goes or a send fails.
async fn send_events(api: &Api, mut socket: WebSocket) -> Result<(), StoreError> {
    let Some(mut sent) = send_state(api, &mut socket).await? else {
        return Ok(());
    };
    let mut every = tokio::time::interval(EVENTS_EVERY);
    loop {
        tokio::select! {
            _ = every.tick() => {}
            incoming = socket.recv() => match incoming {
                Some(Ok(ws::Message::Close(_))) | Some(Err(_)) | None => return Ok(()),
                Some(Ok(_)) => continue,
            },
        }
        if api.peers.online() != sent.online {
            match send_state(api, &mut socket).await? {
                Some(again) => sent = again,
                None => return Ok(()),
            }
            continue;
        }
        let seen = sent.seq;
        let (held, settings) = on_store(&api.store, move |store| {
            Ok((store.versions()?, store.settings_since(Some(seen))?))
        })
        .await?;
        for (owner, version) in held {
            let from = sent.versions.get(&owner).copied().unwrap_or(0);
            if version <= from {
                continue;
            }
            let records = match on_store(&api.store, move |store| store.since(owner, from)).await {
                Ok(records) => records,
                // A record this build cannot read, one a newer build wrote before a rollback,
                // leaves out its owner's changes up to `version`, as `Peers::state` leaves the
                // owner out, rather than end the socket for every other owner.
                Err(error @ StoreError::Json(_)) => {
                    tracing::warn!("api: records of {owner} not sent: {error}");
                    sent.versions.insert(owner, version);
                    continue;
                }
                Err(error) => return Err(error),
            };
            for record in records {
                let version = record.version();
                let message = UiMessage::Record {
                    protocol: PROTOCOL,
                    record,
                };
                if !send(&mut socket, &message).await {
                    return Ok(());
                }
                sent.versions.insert(owner, version);
            }
        }
        if let Some(settings) = settings {
            sent.seq = settings.seq;
            let message = UiMessage::Settings {
                protocol: PROTOCOL,
                settings: shown(settings),
            };
            if !send(&mut socket, &message).await {
                return Ok(());
            }
        }
    }
}

/// Sends the full state and says what it covered, or `None` when the UI has gone.
async fn send_state(api: &Api, socket: &mut WebSocket) -> Result<Option<Sent>, StoreError> {
    let online = api.peers.online();
    let (state, versions) = full_state_for(api, &online).await?;
    let seq = match &state {
        UiMessage::State { settings, .. } => settings.seq,
        _ => 0,
    };
    Ok(send(socket, &state).await.then_some(Sent {
        versions,
        seq,
        online,
    }))
}

/// Sends `message` as a text frame, and says whether it went.
async fn send(socket: &mut WebSocket, message: &UiMessage) -> bool {
    match serde_json::to_string(message) {
        Ok(text) => socket.send(ws::Message::Text(text.into())).await.is_ok(),
        Err(error) => {
            tracing::warn!("api: an event cannot be sent: {error}");
            true
        }
    }
}

// --- actions ---

/// Where a create goes: a machine by id, or `"choose"` for placement to pick one.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Pick {
    Choose,
    Machine(MachineId),
}

impl<'de> Deserialize<'de> for Pick {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        if text == "choose" {
            return Ok(Self::Choose);
        }
        text.parse()
            .map(|id| Self::Machine(MachineId(id)))
            .map_err(|_| {
                serde::de::Error::custom(format!(
                    "machine is a machine id or \"choose\", not {text:?}"
                ))
            })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Create {
    project: ProjectId,
    base: String,
    name: String,
    /// The machine to make it on; absent is this machine.
    #[serde(default)]
    machine: Option<Pick>,
    /// With `"choose"` only: the model of the session a request that waits starts.
    #[serde(default)]
    model: Option<String>,
    /// With `"choose"` only: that session's first prompt.
    #[serde(default)]
    prompt: Option<String>,
}

fn unprocessable(message: String) -> ApiError {
    ApiError::Rejected(StatusCode::UNPROCESSABLE_ENTITY, message)
}

async fn create_workspace(
    State(api): State<Api>,
    headers: HeaderMap,
    Body(create): Body<Create>,
) -> Result<Response, ApiError> {
    let carried = headers.contains_key(crate::sync::call::CARRIED);
    if create.machine != Some(Pick::Choose) && (create.model.is_some() || create.prompt.is_some()) {
        return Err(unprocessable(
            "only a create with \"machine\": \"choose\" takes a model and a prompt; the caller of \
             any other starts its session itself"
                .to_owned(),
        ));
    }
    if create.machine == Some(Pick::Choose) && !carried {
        return choose(&api, create).await;
    }
    // A create naming another machine is carried there before this runs ([`carry`]), so one
    // that reaches here naming another was carried in, and is never carried on. One carried in
    // names this machine, as the caller's carry layer sends it.
    let me = api.peers.me();
    let machine = match create.machine {
        Some(Pick::Machine(machine)) => Some(machine),
        Some(Pick::Choose) | None => None,
    };
    if machine.is_some_and(|m| m != me) || (carried && machine != Some(me)) {
        let named = machine.map_or_else(|| "no machine".to_owned(), |m| format!("machine {m}"));
        return Ok(error(
            StatusCode::MISDIRECTED_REQUEST,
            format!("this call names {named}, not this one, and is not carried on"),
        ));
    }
    // A carried create is warned of by the machine that carried it, which the person talks to.
    let warning = if carried {
        None
    } else {
        api.placements.warning(&api, me).await?
    };
    let (status, mut answer) =
        create_here(&api, create.project, &create.base, &create.name).await?;
    if let (Some(warning), Some(fields)) = (warning, answer.as_object_mut()) {
        fields.insert("warning".to_owned(), warning.into());
    }
    Ok((status, Json(answer)).into_response())
}

/// Makes a workspace on this machine: `201 {id}`, with an `error` beside the id when its setup
/// could not be started.
async fn create_here(
    api: &Api,
    project: ProjectId,
    base: &str,
    name: &str,
) -> Result<(StatusCode, serde_json::Value), ApiError> {
    match api.workspaces.create(project, base, name).await {
        Ok(id) => Ok((
            StatusCode::CREATED,
            serde_json::json!({ "id": id.to_string() }),
        )),
        // The workspace exists, so the caller needs its id: a retry would only find the name
        // taken.
        Err(
            e @ (WorkspaceError::SetupNotStarted { workspace, .. }
            | WorkspaceError::SetupNotRecorded { workspace, .. }),
        ) => {
            let message = redact(&e.to_string());
            tracing::warn!("api: {message}");
            Ok((
                StatusCode::CREATED,
                serde_json::json!({ "id": workspace.to_string(), "error": message }),
            ))
        }
        Err(e) => Err(e.into()),
    }
}

/// A create with `"machine": "choose"`: made on the machine placement picks, or left waiting.
async fn choose(api: &Api, create: Create) -> Result<Response, ApiError> {
    let Some(model) = create.model.filter(|model| !model.trim().is_empty()) else {
        return Err(unprocessable(
            "a create with \"machine\": \"choose\" names the model of the session it starts \
             when it is placed later"
                .to_owned(),
        ));
    };
    check_prompt(create.prompt.as_deref())?;
    let settings = on_store(&api.store, |store| store.settings()).await?;
    if !settings.projects.iter().any(|p| p.id == create.project) {
        return Err(WorkspaceError::UnknownProject(create.project).into());
    }
    let placements = Arc::clone(&api.placements);
    match placements
        .create(api, create.project, &create.base, &create.name)
        .await?
    {
        Ok(sent) => {
            // The caller starts the session in a workspace made, as after any create.
            let made = matches!(sent.answer, Ok((StatusCode::CREATED, _)));
            placements
                .settle(api, sent.machine, sent.pending, made)
                .await;
            let (status, mut answer) = match sent.answer {
                Ok(answer) => answer,
                Err(lost) => read(carry::not_carried(&lost)).await,
            };
            if let Some(fields) = answer.as_object_mut() {
                fields.insert("machine".to_owned(), serde_json::json!(sent.machine));
            }
            Ok((status, Json(answer)).into_response())
        }
        Err(reasons) => {
            let want = Want {
                project: create.project,
                base: create.base,
                name: create.name,
                model,
                prompt: create.prompt,
            };
            let request = api.placements.wait(want, reasons.clone());
            Ok((
                StatusCode::ACCEPTED,
                Json(serde_json::json!({ "request": request, "reasons": reasons })),
            )
                .into_response())
        }
    }
}

/// Makes a workspace on `machine`: here, or carried there as a create naming it is. Gives the
/// status and body answered, or why the call was not answered.
pub(crate) async fn create_on(
    api: &Api,
    machine: MachineId,
    project: ProjectId,
    base: &str,
    name: &str,
) -> Result<(StatusCode, serde_json::Value), CallError> {
    if machine == api.peers.me() {
        return Ok(match create_here(api, project, base, name).await {
            Ok(answer) => answer,
            Err(e) => read(e.into_response()).await,
        });
    }
    let body =
        serde_json::json!({"project": project, "base": base, "name": name, "machine": machine});
    call_on(api, machine, "/api/workspaces", &body).await
}

/// Starts a session with `model` and `prompt` in `machine`'s workspace `workspace`: here, or
/// carried there. Gives the status and body answered, or why the call was not answered.
pub(crate) async fn start_on(
    api: &Api,
    machine: MachineId,
    workspace: WorkspaceId,
    model: &str,
    prompt: Option<&str>,
) -> Result<(StatusCode, serde_json::Value), CallError> {
    if machine == api.peers.me() {
        if let Some(said) = &api.sessions_refused {
            return Ok((
                StatusCode::SERVICE_UNAVAILABLE,
                serde_json::json!({ "error": said }),
            ));
        }
        let started = start_here(api, workspace, model.to_owned(), prompt.map(str::to_owned));
        return Ok(match started.await {
            Ok(session) => (
                StatusCode::CREATED,
                serde_json::json!({ "id": session.to_string() }),
            ),
            Err(e) => read(e.into_response()).await,
        });
    }
    let path = format!("/api/workspaces/{workspace}/sessions");
    let body = match prompt {
        Some(prompt) => serde_json::json!({"model": model, "prompt": prompt}),
        None => serde_json::json!({"model": model}),
    };
    call_on(api, machine, &path, &body).await
}

/// `POST`s `body` as JSON to `path` on machine `owner`, as the carry layer does.
async fn call_on(
    api: &Api,
    owner: MachineId,
    path: &str,
    body: &serde_json::Value,
) -> Result<(StatusCode, serde_json::Value), CallError> {
    let body = body.to_string();
    let answer = api
        .peers
        .call(owner, path, Some("application/json"), body.as_bytes())
        .await?;
    let status = StatusCode::from_u16(answer.status).unwrap_or(StatusCode::BAD_GATEWAY);
    let answer = serde_json::from_slice(&answer.body).unwrap_or(serde_json::Value::Null);
    Ok((status, answer))
}

/// A response's status and its body as JSON, `null` when it has none.
async fn read(response: Response) -> (StatusCode, serde_json::Value) {
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap_or_default();
    (
        status,
        serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null),
    )
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Archive {
    #[serde(default)]
    force: bool,
}

async fn archive_workspace(
    State(api): State<Api>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let id = parse_id(&id, "workspace", WorkspaceId)?;
    let archive: Archive = if body.is_empty() {
        Archive::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| {
            let status = if e.is_data() {
                StatusCode::UNPROCESSABLE_ENTITY
            } else {
                StatusCode::BAD_REQUEST
            };
            ApiError::Rejected(
                status,
                format!("the body is not what an archive takes: {e}"),
            )
        })?
    };
    api.workspaces.archive(id, archive.force).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Start {
    model: String,
    /// What Claude Code starts on, as if typed as its first prompt.
    #[serde(default)]
    prompt: Option<String>,
}

/// The longest first prompt taken, in characters: a task, not a document.
const MOST_PROMPT: usize = 20_000;

/// Refuses a first prompt that is too long or holds a NUL.
fn check_prompt(prompt: Option<&str>) -> Result<(), ApiError> {
    match prompt {
        Some(prompt) if prompt.contains('\0') || prompt.chars().count() > MOST_PROMPT => {
            Err(unprocessable(format!(
                "a first prompt is at most {MOST_PROMPT} characters, with no NUL"
            )))
        }
        _ => Ok(()),
    }
}

async fn start_session(
    State(api): State<Api>,
    Path(id): Path<String>,
    Body(start): Body<Start>,
) -> Result<Response, ApiError> {
    if let Some(said) = &api.sessions_refused {
        return Ok(error(StatusCode::SERVICE_UNAVAILABLE, said.clone()));
    }
    let id = parse_id(&id, "workspace", WorkspaceId)?;
    check_prompt(start.prompt.as_deref())?;
    let session = start_here(&api, id, start.model, start.prompt).await?;
    Ok(created(session))
}

/// Starts Claude Code with `model` and `prompt` in workspace `id` of this machine.
async fn start_here(
    api: &Api,
    id: WorkspaceId,
    model: String,
    prompt: Option<String>,
) -> Result<SessionId, ApiError> {
    let sessions = Arc::clone(&api.sessions);
    api.workspaces
        .start_in(id, move |worktree| async move {
            Ok::<_, ApiError>(
                sessions
                    .start_with(id, &worktree, &model, prompt.as_deref())
                    .await?,
            )
        })
        .await
}

/// The shell a new terminal runs: the daemon's `$SHELL`, else `/bin/sh`, as a login shell.
fn shell() -> Vec<String> {
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/bin/sh".to_owned());
    vec![shell, "-l".to_owned()]
}

async fn start_shell(State(api): State<Api>, Path(id): Path<String>) -> Result<Response, ApiError> {
    let id = parse_id(&id, "workspace", WorkspaceId)?;
    let terminals = Arc::clone(&api.terminals);
    let terminal = api
        .workspaces
        .start_in(id, move |worktree| async move {
            Ok::<_, ApiError>(terminals.start(id, &worktree, &shell(), &[]).await?)
        })
        .await?;
    Ok(created(terminal))
}

async fn close_terminal(
    State(api): State<Api>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let id = parse_id(&id, "terminal", TerminalId)?;
    api.terminals.close(id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}
