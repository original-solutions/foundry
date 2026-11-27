//! A call to another machine's API, carried over a link of its own.
//!
//! Machine A carries a call for a workspace or terminal machine B owns with [`Peers::call`]. It
//! opens a link to B as a relay does ([`Inner::open_link`]), so the link proves both machines'
//! keys as every peer link does, and opens it with a [`PeerMessage::Call`] in place of a hello.
//! B's [`Peers::accept`] hands that link to [`serve`], which makes the call to B's own API on
//! loopback, marked [`CARRIED`] so that API runs it only for B and never carries it on, and
//! sends back one answer: the status and body B's API gave, and the newest version of B's own
//! records once it had. B beats every [`TICK`] while the call runs, and A takes a link silent
//! for [`OFFLINE_AFTER`] as down.
//!
//! A gives the answer back once it holds B's records up to that version, or [`OFFLINE_AFTER`]
//! has passed, so a call that follows on from this one, such as starting a session in the
//! workspace it made, finds what it made already here.
//!
//! A call goes only to a machine in the machine list, heard from within [`OFFLINE_AFTER`], with
//! an open record link that speaks the protocol its route needs ([`needs`]): [`UPDATES_SINCE`]
//! for a machine's install, [`CALLS_SINCE`] for every other. Any other is refused before a link
//! opens, so the refusal changes nothing anywhere and an older machine is never sent one. No end
//! ever logs a body.
//!
//! A route whose answer must leave before this daemon stops, a machine's install, asks for the
//! stop with [`STOP_AFTER`] on its answer to a carried call. B then fires the [`Stop`] its
//! peers were given ([`Peers::stop_after_calls`]) once that answer is on the link and A has
//! closed it, or the link is gone ([`answer`]). That answer names no version of B's records,
//! since B stops before it could send them, so A waits for none. The route fires the same stop
//! by itself [`STOP_WITHIN`] after it asked, so an answer the link cannot take still stops B,
//! and a [`Stop`] acts only once.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use foundry_proto::ids::MachineId;
use foundry_proto::messages::{CALLS_SINCE, Call, PROTOCOL, PeerMessage, UPDATES_SINCE};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::time::{Instant, timeout};

use super::link::listed;
use super::relay::{beat, hex, last_word, unhex};
use super::{Conn, Inner, OFFLINE_AFTER, Peers, Unopened, lock, on_store};

#[cfg(doc)]
use super::TICK;

/// The header that marks a request to this machine's API as a call another machine carried
/// here: the API runs it only for this machine and never carries it on.
pub const CARRIED: &str = "x-foundry-carried";

/// How long the owner's API may take to answer a carried call: longer than any git step a
/// create runs, which the workspaces bound at ten minutes.
const CALL_LIMIT: Duration = Duration::from_secs(15 * 60);

/// How often a caller checks whether the records a call wrote have arrived.
const SYNCED_EVERY: Duration = Duration::from_millis(50);

/// The header on a route's answer that asks for this daemon to stop once that answer has left:
/// [`serve`] fires the machine's [`Stop`] after it sent it on the link.
pub const STOP_AFTER: &str = "x-foundry-stop-after";

/// How long after a route asked for a stop the daemon stops anyway, when its answer could not
/// be sent.
pub const STOP_WITHIN: Duration = Duration::from_secs(5);

/// The protocol a link must speak for a call to `path`: [`UPDATES_SINCE`] for a machine's
/// install, which an older machine does not serve, and [`CALLS_SINCE`] for every other route.
fn needs(path: &str) -> u16 {
    let parts: Vec<&str> = path.split('/').collect();
    match parts.as_slice() {
        ["", "api", "machines", _, "update"] => UPDATES_SINCE,
        _ => CALLS_SINCE,
    }
}

/// Stops this daemon once, however many ask: the route that asked, after its answer left or
/// [`STOP_WITHIN`] later, and a carried call's link after the answer it sent.
#[derive(Clone)]
pub struct Stop(Arc<Stopping>);

struct Stopping {
    fired: AtomicBool,
    action: Box<dyn Fn() + Send + Sync>,
}

impl Stop {
    /// A stop that runs `action` the first time it fires, and never again.
    pub fn new(action: impl Fn() + Send + Sync + 'static) -> Self {
        Self(Arc::new(Stopping {
            fired: AtomicBool::new(false),
            action: Box::new(action),
        }))
    }

    /// Raises SIGTERM on this process, as the updater stopped the daemon before #272: the
    /// daemon stops as it does for the service's own SIGTERM, and the service starts it again.
    pub fn sigterm() -> Self {
        Self::new(|| {
            tracing::info!("update: stopping, so the service starts the installed version");
            if let Err(error) = nix::sys::signal::raise(nix::sys::signal::Signal::SIGTERM) {
                tracing::error!("update: cannot raise SIGTERM: {error}");
            }
        })
    }

    /// Stops, unless it already did.
    pub fn fire(&self) {
        if !self.0.fired.swap(true, Ordering::SeqCst) {
            (self.0.action)();
        }
    }

    /// Whether it has stopped.
    pub fn fired(&self) -> bool {
        self.0.fired.load(Ordering::SeqCst)
    }

    /// Stops `limit` from now, unless it stopped before. Call it inside the runtime.
    pub fn fire_within(&self, limit: Duration) {
        let stop = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(limit).await;
            stop.fire();
        });
    }
}

/// The stop each machine's carried calls fire after an answer that asks for one, by machine.
/// Kept beside the peers rather than in them, so a daemon that gives none stops for no call.
static STOPS: Mutex<BTreeMap<MachineId, Stop>> = Mutex::new(BTreeMap::new());

/// The stop the peers of machine `me` were given, if any.
fn stop_of(me: MachineId) -> Option<Stop> {
    lock(&STOPS).get(&me).cloned()
}

/// One message on a call link past the call that opened it, owner to caller.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Frame {
    /// The owner's API's answer: its status, its body's bytes as hex, and the newest version of
    /// the owner's own records once it answered, or 0 when the owner stops after this answer.
    Answer {
        status: u16,
        body: String,
        version: u64,
    },
    /// Every [`TICK`] while the call runs: the relay's beat, written the same way.
    Beat,
}

/// What the owner's API answered.
#[derive(Debug)]
pub struct Answer {
    pub status: u16,
    pub body: Bytes,
}

/// Why a call was not answered by its owner.
#[derive(Debug, thiserror::Error)]
pub enum CallError {
    /// The owner is not in the machine list. Nothing was sent.
    #[error("machine {0} is not in the machine list")]
    NotListed(MachineId),
    /// The owner, named here, has not been heard from within [`OFFLINE_AFTER`]. Nothing was
    /// sent.
    #[error("{0} is offline")]
    Offline(String),
    /// The owner, named here, speaks a protocol older than [`CALLS_SINCE`]. Nothing was sent.
    #[error("{0} runs an older Foundry, which cannot take this call")]
    Older(String),
    /// The link to the owner, named here, could not be opened. Nothing was sent.
    #[error("the link to {0} is down")]
    Down(String),
    /// The link to the owner, named here, went down after the call was sent: it may have run
    /// there, and what it changed shows once the link is back.
    #[error("the link to {0} went down before it answered; the call may have run there")]
    Lost(String),
    /// The owner refused the link, or this machine cannot make the call; the reason says why.
    #[error("{0}")]
    Refused(String),
}

impl Peers {
    /// This machine's id.
    pub fn me(&self) -> MachineId {
        self.inner.me
    }

    /// Makes calls carried to this machine against its API at `api`, a loopback address. Until
    /// this is called, a carried call is answered `503`.
    pub fn serve_calls(&self, api: SocketAddr) {
        *lock(&self.inner.calls) = Some(api);
    }

    /// Fires `stop` after a carried call's answer that asks for one ([`STOP_AFTER`]) has left
    /// on its link. Until this is called, such an answer stops nothing here: the route that
    /// asked stops by itself [`STOP_WITHIN`] later.
    pub fn stop_after_calls(&self, stop: Stop) {
        lock(&STOPS).insert(self.inner.me, stop);
    }

    /// Carries a `POST` of `body` to `path`, with `content_type`, to machine `owner`'s API, and
    /// gives back its answer once this machine holds what the call wrote, as the module says.
    pub async fn call(
        &self,
        owner: MachineId,
        path: &str,
        content_type: Option<&str>,
        body: &[u8],
    ) -> Result<Answer, CallError> {
        let settings = on_store(&self.inner.store, |store| store.settings())
            .await
            .map_err(|e| CallError::Refused(format!("cannot read the machine list: {e}")))?;
        let Some(peer) = settings.machines.into_iter().find(|m| m.id == owner) else {
            return Err(CallError::NotListed(owner));
        };
        if !self.online().contains(&owner) {
            return Err(CallError::Offline(peer.name));
        }
        match self.speaks(owner) {
            Some(speak) if speak >= needs(path) => {}
            Some(_) => return Err(CallError::Older(peer.name)),
            // Nothing says the machine can take a call until a record link agrees a protocol.
            None => return Err(CallError::Down(peer.name)),
        }
        let open = PeerMessage::Call {
            protocol: PROTOCOL,
            machine: self.inner.me,
            call: Call {
                path: path.to_owned(),
                content_type: content_type.map(str::to_owned),
                body: hex(body),
            },
        };
        let Conn { send, mut recv } = match self.inner.open_link(&peer, &open).await {
            Ok(conn) => conn,
            Err(Unopened::Refused(reason)) => return Err(CallError::Refused(reason)),
            Err(Unopened::Down) => return Err(CallError::Down(peer.name)),
        };
        let (status, body, version) = loop {
            let Ok(Some(text)) = timeout(OFFLINE_AFTER, recv.recv()).await else {
                return Err(CallError::Lost(peer.name));
            };
            // Anything else is a beat, or a frame this build does not know.
            if let Ok(Frame::Answer {
                status,
                body,
                version,
            }) = serde_json::from_str(&text)
            {
                break (status, body, version);
            }
        };
        drop(send);
        let Some(body) = unhex(&body) else {
            return Err(CallError::Refused(format!(
                "{} sent an answer that cannot be read",
                peer.name
            )));
        };
        self.synced(owner, version).await;
        Ok(Answer { status, body })
    }

    /// The newest protocol an open record link to `machine` speaks, or `None` with no link open.
    fn speaks(&self, machine: MachineId) -> Option<u16> {
        lock(&self.inner.links)
            .get(&machine)
            .and_then(|open| open.values().map(|link| link.speak).max())
    }

    /// Waits until this machine holds `owner`'s records up to `version`, at most
    /// [`OFFLINE_AFTER`].
    async fn synced(&self, owner: MachineId, version: u64) {
        let until = Instant::now() + OFFLINE_AFTER;
        loop {
            let held = on_store(&self.inner.store, move |store| store.versions())
                .await
                .ok()
                .and_then(|versions| versions.get(&owner).copied())
                .unwrap_or(0);
            if held >= version || Instant::now() >= until {
                return;
            }
            tokio::time::sleep(SYNCED_EVERY).await;
        }
    }
}

/// Serves a call that `machine` carried here, then waits, at most [`OFFLINE_AFTER`], for the
/// caller to close the link, so the answer is never lost to a link torn down under it. A
/// machine that is not a peer is answered `403`, a path that is not one of the routes the API
/// carries `400`, and a call to a `Peers` serving no calls `503`. The call runs to its end even
/// when the caller goes. An answer that asks for a stop ([`STOP_AFTER`]) fires this machine's
/// [`Stop`] once it has left ([`answer`]).
pub(super) async fn serve(inner: &Inner, machine: MachineId, call: Call, conn: Conn) {
    let Conn { send, mut recv } = conn;
    let api = *lock(&inner.calls);
    let (status, body, stop) = if machine == inner.me || !listed(inner, machine).await {
        tracing::warn!("sync: refused a call from {machine}: not a peer of this machine");
        refusal(
            403,
            format!("machine {machine} is not a peer of this machine"),
        )
    } else if !crate::api::carries(&call.path) {
        refusal(
            400,
            "a carried call names a route that is not carried".to_owned(),
        )
    } else if let Some(api) = api {
        let _beat = beat(send.clone());
        match unhex(&call.body) {
            Some(body) => run(api, call.path, call.content_type, body).await,
            None => refusal(400, "the carried body cannot be read".to_owned()),
        }
    } else {
        refusal(503, "this machine serves no calls".to_owned())
    };
    let me = inner.me;
    // This machine stops once the caller has the answer, before it could send the records it
    // wrote since: the caller is told to wait for none.
    let version = if stop {
        0
    } else {
        on_store(&inner.store, move |store| store.versions())
            .await
            .ok()
            .and_then(|versions| versions.get(&me).copied())
            .unwrap_or(0)
    };
    let frame = Frame::Answer {
        status,
        body: hex(&body),
        version,
    };
    let stop = if stop {
        let stop = stop_of(me);
        if stop.is_none() {
            tracing::warn!("sync: a carried call asked for a stop, and this machine has none");
        }
        stop
    } else {
        None
    };
    answer(&send, &mut recv, &frame, stop).await;
}

/// Sends `frame`, the call's answer, as its last word ([`last_word`]), then fires `stop`, when
/// the route asked for one: once the caller has closed the link, the answer in hand, or the
/// link could not take it.
async fn answer(
    send: &Sender<String>,
    recv: &mut Receiver<String>,
    frame: &Frame,
    stop: Option<Stop>,
) {
    last_word(send, recv, frame).await;
    if let Some(stop) = stop {
        stop.fire();
    }
}

/// `builder` as the client a carried call is made to this machine's API with: no proxy,
/// whatever the environment says, and no redirect, so the call never leaves this machine, and at
/// most [`CALL_LIMIT`].
fn loopback(
    builder: ureq::config::ConfigBuilder<ureq::typestate::AgentScope>,
) -> ureq::config::ConfigBuilder<ureq::typestate::AgentScope> {
    builder
        .http_status_as_error(false)
        .proxy(None)
        .max_redirects(0)
        .timeout_global(Some(CALL_LIMIT))
}

/// Makes the call to this machine's API at `api` and gives back its status, its body and whether
/// it asked for a stop after its answer ([`STOP_AFTER`]). The request runs on the blocking pool
/// and to its end, whatever happens to the link.
async fn run(
    api: SocketAddr,
    path: String,
    content_type: Option<String>,
    body: Bytes,
) -> (u16, Bytes, bool) {
    let made = tokio::task::spawn_blocking(move || {
        let agent: ureq::Agent = loopback(ureq::Agent::config_builder()).build().into();
        let mut request = agent
            .post(format!("http://{api}{path}"))
            .header(CARRIED, "1");
        if let Some(content_type) = content_type {
            request = request.header("content-type", content_type);
        }
        let mut response = request.send(&body[..])?;
        let status = response.status().as_u16();
        let stop = response.headers().contains_key(STOP_AFTER);
        let body = response.body_mut().read_to_vec()?;
        Ok::<_, ureq::Error>((status, Bytes::from(body), stop))
    })
    .await;
    match made {
        Ok(Ok(answered)) => answered,
        Ok(Err(ureq::Error::Timeout(_))) => refusal(
            504,
            format!("this machine's API did not answer within {CALL_LIMIT:?}"),
        ),
        Ok(Err(error)) => {
            tracing::warn!("sync: a carried call could not reach this machine's API: {error}");
            refusal(
                502,
                format!("this machine's API could not be reached: {error}"),
            )
        }
        Err(_) => refusal(
            500,
            "the carried call stopped before it answered".to_owned(),
        ),
    }
}

/// An answer this end gives itself: `status` with `{"error": message}`, asking for no stop.
fn refusal(status: u16, message: String) -> (u16, Bytes, bool) {
    let body = serde_json::json!({ "error": message }).to_string();
    (status, Bytes::from(body), false)
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use foundry_proto::ids::{TerminalId, WorkspaceId};
    use foundry_proto::messages::UPDATES_SINCE;
    use foundry_proto::settings::{Machine, SharedSettings};
    use futures::future::BoxFuture;
    use tokio::sync::mpsc;

    use super::*;
    use crate::store::Store;
    use crate::sync::Dialer;

    /// Opens a link for each dial and keeps its far end, where the test answers as the owner.
    struct Opens(Arc<Mutex<Vec<Conn>>>);

    impl Dialer for Opens {
        fn dial(&self, _: &Machine) -> BoxFuture<'static, io::Result<Conn>> {
            let (near, far) = Conn::pair();
            lock(&self.0).push(far);
            Box::pin(async { Ok(near) })
        }
    }

    /// Peers of machine `me` whose machine list also holds `peer`, named `peer`, with the far
    /// end of each link they dial.
    fn listing(
        me: MachineId,
        peer: MachineId,
    ) -> (tempfile::TempDir, Peers, Arc<Mutex<Vec<Conn>>>) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("foundry.db")).unwrap();
        let settings = SharedSettings {
            seq: 1,
            machines: [(me, "me"), (peer, "peer")]
                .into_iter()
                .map(|(id, name)| Machine {
                    id,
                    name: name.into(),
                    address: "100.64.0.9".into(),
                    key: String::new(),
                })
                .collect(),
            ..SharedSettings::default()
        };
        assert!(store.apply_settings(settings).unwrap());
        let dialed = Arc::new(Mutex::new(Vec::new()));
        let peers = Peers::new(
            me,
            Arc::new(std::sync::Mutex::new(store)),
            Arc::new(Opens(dialed.clone())),
        );
        (dir, peers, dialed)
    }

    #[tokio::test]
    async fn an_install_is_never_sent_on_a_link_below_updates_since_and_a_workspace_call_still_is()
    {
        let (me, old) = (MachineId::random(), MachineId::random());
        let (_dir, peers, dialed) = listing(me, old);
        peers.inner.heard_from(old);
        let (link, _unread) = mpsc::channel(4);
        let _open = peers.inner.opened(old, UPDATES_SINCE - 1, link);

        let install = format!("/api/machines/{old}/update");
        let body = br#"{"tag":"v0.9.1"}"#;
        match peers
            .call(old, &install, Some("application/json"), body)
            .await
        {
            Err(CallError::Older(name)) => assert_eq!(name, "peer"),
            other => panic!("not refused as older: {other:?}"),
        }
        assert!(lock(&dialed).is_empty(), "the install was sent");

        let shells = format!("/api/workspaces/{}/shells", WorkspaceId::random());
        let calling = tokio::spawn({
            let (peers, shells) = (peers.clone(), shells.clone());
            async move { peers.call(old, &shells, None, b"").await }
        });
        let mut far = loop {
            if let Some(far) = lock(&dialed).pop() {
                break far;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        let opened = far.recv.recv().await.unwrap();
        match serde_json::from_str::<PeerMessage>(&opened).unwrap() {
            PeerMessage::Call { call, .. } => assert_eq!(call.path, shells),
            other => panic!("not a call: {other:?}"),
        }
        let answer = Frame::Answer {
            status: 201,
            body: hex(b"{}"),
            version: 0,
        };
        far.send
            .send(serde_json::to_string(&answer).unwrap())
            .await
            .unwrap();
        let answered = calling.await.unwrap().unwrap();
        assert_eq!(answered.status, 201);
    }

    #[test]
    fn only_the_install_needs_updates_since_and_every_other_route_calls_since() {
        let id = uuid::Uuid::new_v4();
        assert_eq!(needs(&format!("/api/machines/{id}/update")), UPDATES_SINCE);
        for other in [
            "/api/workspaces".to_owned(),
            format!("/api/workspaces/{id}/sessions"),
            format!("/api/terminals/{id}/close"),
            format!("/api/machines/{id}/remove"),
        ] {
            assert_eq!(needs(&other), CALLS_SINCE, "{other}");
        }
    }

    /// A stop that counts how many times it acted.
    fn counted() -> (Stop, Arc<AtomicUsize>) {
        let stops = Arc::new(AtomicUsize::new(0));
        let counting = stops.clone();
        let stop = Stop::new(move || {
            counting.fetch_add(1, Ordering::SeqCst);
        });
        (stop, stops)
    }

    fn accepted() -> Frame {
        Frame::Answer {
            status: 202,
            body: String::new(),
            version: 3,
        }
    }

    #[tokio::test]
    async fn the_stop_a_carried_answer_asked_for_fires_only_once_the_answer_is_on_the_link() {
        let (stop, stops) = counted();
        let (owner, mut caller) = Conn::pair();
        let answering = tokio::spawn(async move {
            let Conn { send, mut recv } = owner;
            answer(&send, &mut recv, &accepted(), Some(stop)).await;
        });

        let text = caller.recv.recv().await.unwrap();
        assert!(
            matches!(
                serde_json::from_str(&text),
                Ok(Frame::Answer { status: 202, .. })
            ),
            "{text}"
        );
        assert_eq!(stops.load(Ordering::SeqCst), 0, "stopped before the answer");
        drop(caller);
        answering.await.unwrap();
        assert_eq!(stops.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn an_answer_that_asked_for_no_stop_stops_nothing() {
        let (stop, stops) = counted();
        let (owner, caller) = Conn::pair();
        drop(caller);
        let Conn { send, mut recv } = owner;
        answer(&send, &mut recv, &accepted(), None).await;
        drop(stop);
        assert_eq!(stops.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn an_answer_the_link_cannot_take_still_stops_within_the_limit_and_once() {
        let (stop, stops) = counted();
        // As the route does once the install is made.
        stop.fire_within(STOP_WITHIN);
        // A link whose buffer is full and never read: the answer cannot be sent.
        let (send, _unread) = mpsc::channel(1);
        send.send("full".to_owned()).await.unwrap();
        let (_open, mut recv) = mpsc::channel(1);
        let started = Instant::now();
        let answering = tokio::spawn(async move {
            answer(&send, &mut recv, &accepted(), Some(stop)).await;
        });

        tokio::time::sleep_until(started + STOP_WITHIN + Duration::from_millis(1)).await;
        assert_eq!(
            stops.load(Ordering::SeqCst),
            1,
            "not stopped within the limit"
        );
        answering.await.unwrap();
        assert_eq!(stops.load(Ordering::SeqCst), 1, "stopped twice");
    }

    #[tokio::test(start_paused = true)]
    async fn a_link_already_gone_stops_at_once_and_the_fallback_does_not_stop_again() {
        let (stop, stops) = counted();
        stop.fire_within(STOP_WITHIN);
        let (owner, caller) = Conn::pair();
        drop(caller);
        let Conn { send, mut recv } = owner;
        let started = Instant::now();

        answer(&send, &mut recv, &accepted(), Some(stop.clone())).await;

        assert_eq!(stops.load(Ordering::SeqCst), 1);
        assert!(started.elapsed() < STOP_WITHIN, "{:?}", started.elapsed());
        tokio::time::sleep(STOP_WITHIN * 2).await;
        assert_eq!(stops.load(Ordering::SeqCst), 1, "stopped twice");
        assert!(stop.fired());
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_asked_for_by_nobody_fires_by_itself_within_its_limit() {
        let (stop, stops) = counted();
        stop.fire_within(STOP_WITHIN);
        tokio::time::sleep(STOP_WITHIN - Duration::from_millis(1)).await;
        assert_eq!(stops.load(Ordering::SeqCst), 0, "fired early");
        tokio::time::sleep(Duration::from_millis(2)).await;
        assert_eq!(stops.load(Ordering::SeqCst), 1);
        stop.fire();
        assert_eq!(stops.load(Ordering::SeqCst), 1, "fired twice");
    }

    #[tokio::test]
    async fn the_owners_api_asks_for_a_stop_only_with_its_header() {
        let app = axum::Router::new()
            .route(
                "/stops",
                axum::routing::post(|| async {
                    (axum::http::StatusCode::ACCEPTED, [(STOP_AFTER, "1")])
                }),
            )
            .route(
                "/goes-on",
                axum::routing::post(|| async { axum::http::StatusCode::ACCEPTED }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let stops = run(api, "/stops".into(), None, Bytes::new()).await;
        let goes_on = run(api, "/goes-on".into(), None, Bytes::new()).await;

        server.abort();
        assert_eq!((stops.0, stops.1.is_empty(), stops.2), (202, true, true));
        assert_eq!((goes_on.0, goes_on.2), (202, false));
    }

    /// An owner's API: the install answers `202` and asks for a stop, a create `201` and a
    /// terminal's close `409`, neither asking.
    async fn owners_api() -> (SocketAddr, tokio::task::JoinHandle<()>) {
        use axum::http::StatusCode;
        use axum::routing::post;
        let app = axum::Router::new()
            .route(
                "/api/machines/{id}/update",
                post(|| async { (StatusCode::ACCEPTED, [(STOP_AFTER, "1")]) }),
            )
            .route("/api/workspaces", post(|| async { StatusCode::CREATED }))
            .route(
                "/api/terminals/{id}/close",
                post(|| async { StatusCode::CONFLICT }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (api, server)
    }

    /// Opens a call link from `caller` to `owner` that carries a `POST` to `path`, and gives
    /// back the caller's end of the link once the owner's answer arrived, with its status and
    /// version.
    async fn carried(owner: &Peers, caller: MachineId, path: &str) -> (Conn, u16, u64) {
        let (mut near, far) = Conn::pair();
        let accepting = owner.clone();
        tokio::spawn(async move { accepting.accept(far).await });
        let open = PeerMessage::Call {
            protocol: PROTOCOL,
            machine: caller,
            call: Call {
                path: path.to_owned(),
                content_type: None,
                body: hex(b""),
            },
        };
        near.send
            .send(serde_json::to_string(&open).unwrap())
            .await
            .unwrap();
        loop {
            let text = near.recv.recv().await.expect("an answer");
            if let Ok(Frame::Answer {
                status, version, ..
            }) = serde_json::from_str(&text)
            {
                return (near, status, version);
            }
        }
    }

    #[tokio::test]
    async fn a_carried_call_stops_its_owner_once_the_caller_has_the_answer_that_asked_and_only_then()
     {
        let (me, caller) = (MachineId::random(), MachineId::random());
        let (_dir, owner, _) = listing(me, caller);
        {
            let mut store = lock(&owner.inner.store);
            let version = store.own_next_version().unwrap();
            let sample =
                foundry_proto::record::AnyRecord::MachineSample(foundry_proto::record::Record {
                    owner: me,
                    version,
                    deleted: false,
                    body: foundry_proto::record::MachineSample {
                        load_per_core: 0.5,
                        free_memory: 1,
                        free_disk: 1,
                        sessions: 0,
                        update_refused: None,
                        version: None,
                        update_ready: None,
                    },
                });
            store.apply(&sample, me).unwrap();
        }
        let (api, server) = owners_api().await;
        owner.serve_calls(api);
        let (stop, stops) = counted();
        owner.stop_after_calls(stop);

        for path in [
            "/api/workspaces".to_owned(),
            format!("/api/terminals/{}/close", TerminalId::random()),
        ] {
            let (link, status, version) = carried(&owner, caller, &path).await;
            assert!([201, 409].contains(&status), "{path}: {status}");
            assert!(version > 0, "{path}: the answer names no records");
            drop(link);
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert_eq!(stops.load(Ordering::SeqCst), 0, "{path} stopped the owner");
        }

        let install = format!("/api/machines/{me}/update");
        let (link, status, version) = carried(&owner, caller, &install).await;
        assert_eq!(status, 202);
        // The owner stops before it could send what it wrote since: the caller waits for none.
        assert_eq!(
            version, 0,
            "the caller would wait for records that never come"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            stops.load(Ordering::SeqCst),
            0,
            "stopped before the caller closed the link"
        );
        drop(link);
        let until = Instant::now() + Duration::from_secs(5);
        while stops.load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < until, "the owner never stopped");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(stops.load(Ordering::SeqCst), 1, "stopped twice");
        server.abort();
    }

    #[test]
    fn the_stop_of_a_machine_is_the_one_its_peers_were_given() {
        let (me, other) = (MachineId::random(), MachineId::random());
        let (_dir, peers, _) = listing(me, other);
        assert!(stop_of(me).is_none());
        let (stop, stops) = counted();
        peers.stop_after_calls(stop);
        stop_of(me).expect("a stop").fire();
        assert_eq!(stops.load(Ordering::SeqCst), 1);
        assert!(stop_of(other).is_none());
    }

    #[tokio::test]
    async fn the_beat_the_owner_sends_reads_as_a_call_links_beat() {
        let (send, mut recv) = tokio::sync::mpsc::channel(4);
        let _beat = beat(send);
        let text = recv.recv().await.unwrap();
        assert!(
            matches!(serde_json::from_str(&text), Ok(Frame::Beat)),
            "{text}"
        );
    }

    /// A proxy in the daemon's environment, or a redirect, would take a carried call off
    /// this machine. The builder starts with a proxy, as one read from the environment would.
    #[test]
    fn the_loopback_call_uses_no_proxy_follows_no_redirect_and_has_a_limit() {
        let proxy = ureq::Proxy::new("http://127.0.0.1:9").unwrap();
        let config = loopback(ureq::Agent::config_builder().proxy(Some(proxy))).build();
        assert!(config.proxy().is_none());
        assert_eq!(config.max_redirects(), 0);
        assert_eq!(config.timeouts().global, Some(CALL_LIMIT));
    }

    #[test]
    fn an_answer_reads_from_its_wire_form() {
        let text = r#"{"type":"answer","status":201,"body":"7b7d","version":9}"#;
        let Frame::Answer {
            status,
            body,
            version,
        } = serde_json::from_str(text).unwrap()
        else {
            panic!("not an answer");
        };
        assert_eq!(
            (status, unhex(&body).as_deref(), version),
            (201, Some(&b"{}"[..]), 9)
        );
    }
}
