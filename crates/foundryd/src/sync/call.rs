//! A call to another machine's API, carried over a link of its own.
//!
//! Machine A carries a call for a workspace or terminal machine B owns with [`Peers::call`]. It
//! dials B with the [`super::Dialer`] the record links use, so the link proves both machines'
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
//! A call goes only to a machine in the machine list, heard from within [`OFFLINE_AFTER`],
//! whose record links speak [`CALLS_SINCE`] or later; any other is refused before a link opens,
//! so the refusal changes nothing anywhere. No end ever logs a body.

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use bytes::Bytes;
use foundry_proto::ids::MachineId;
use foundry_proto::messages::{CALLS_SINCE, Call, PROTOCOL, PeerMessage};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::time::{Instant, timeout};

use super::link::{listed, send_message};
use super::relay::{beat, hex, unhex};
use super::{Conn, DIAL_TIMEOUT, Inner, OFFLINE_AFTER, Peers, lock, on_store};

#[cfg(doc)]
use super::TICK;

/// The header that marks a request to this machine's API as a call another machine carried
/// here: the API runs it only for this machine and never carries it on.
pub const CARRIED: &str = "x-foundry-carried";

/// How often a caller checks whether the records a call wrote have arrived.
const SYNCED_EVERY: Duration = Duration::from_millis(50);

/// One message on a call link past the call that opened it, owner to caller.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Frame {
    /// The owner's API's answer: its status, its body's bytes as hex, and the newest version of
    /// the owner's own records once it answered.
    Answer {
        status: u16,
        body: String,
        version: u64,
    },
    /// Every [`TICK`] while the call runs.
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
    /// The owner is not in the machine list.
    #[error("machine {0} is not in the machine list")]
    NotListed(MachineId),
    /// The owner, named here, has not been heard from within [`OFFLINE_AFTER`].
    #[error("{0} is offline")]
    Offline(String),
    /// The owner, named here, speaks a protocol older than [`CALLS_SINCE`].
    #[error("{0} runs an older Foundry, which cannot take this call")]
    Older(String),
    /// The link to the owner, named here, could not be opened.
    #[error("the link to {0} is down")]
    Down(String),
    /// The link to the owner, named here, went down after the call was sent: it may have run.
    #[error("the link to {0} went down before it answered")]
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
        if self.speaks_before(owner, CALLS_SINCE) {
            return Err(CallError::Older(peer.name));
        }
        let Conn { send, mut recv } =
            match timeout(DIAL_TIMEOUT, self.inner.dialer.dial(&peer)).await {
                Ok(Ok(conn)) => conn,
                // The owner refused this machine's key.
                Ok(Err(e)) if e.kind() == io::ErrorKind::PermissionDenied => {
                    return Err(CallError::Refused(e.to_string()));
                }
                Ok(Err(_)) | Err(_) => return Err(CallError::Down(peer.name)),
            };
        let open = PeerMessage::Call {
            protocol: PROTOCOL,
            machine: self.inner.me,
            call: Call {
                path: path.to_owned(),
                content_type: content_type.map(str::to_owned),
                body: hex(body),
            },
        };
        if !send_message(&send, &open).await {
            return Err(CallError::Down(peer.name));
        }
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

    /// Whether every open record link to `machine` speaks a protocol older than `since`. A
    /// machine with no open link says nothing, so it is not taken as older.
    fn speaks_before(&self, machine: MachineId, since: u16) -> bool {
        lock(&self.inner.links)
            .get(&machine)
            .is_some_and(|open| open.values().all(|link| link.speak < since))
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
/// machine that is not a peer is answered `403`, a path outside the API `400`, and a call to a
/// `Peers` serving no calls `503`. The call runs to its end even when the caller goes.
pub(super) async fn serve(inner: &Inner, machine: MachineId, call: Call, conn: Conn) {
    let Conn { send, mut recv } = conn;
    let api = *lock(&inner.calls);
    let (status, body) = if machine == inner.me || !listed(inner, machine).await {
        tracing::warn!("sync: refused a call from {machine}: not a peer of this machine");
        refusal(
            403,
            format!("machine {machine} is not a peer of this machine"),
        )
    } else if !call.path.starts_with("/api/") {
        refusal(
            400,
            "a carried call names a path outside the API".to_owned(),
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
    let version = on_store(&inner.store, move |store| store.versions())
        .await
        .ok()
        .and_then(|versions| versions.get(&me).copied())
        .unwrap_or(0);
    let answer = Frame::Answer {
        status,
        body: hex(&body),
        version,
    };
    finish(&send, &mut recv, &answer).await;
}

/// Makes the call to this machine's API at `api` and gives back its status and body. The
/// request runs on the blocking pool and to its end, whatever happens to the link.
async fn run(
    api: SocketAddr,
    path: String,
    content_type: Option<String>,
    body: Bytes,
) -> (u16, Bytes) {
    let made = tokio::task::spawn_blocking(move || {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into();
        let mut request = agent
            .post(format!("http://{api}{path}"))
            .header(CARRIED, "1");
        if let Some(content_type) = content_type {
            request = request.header("content-type", content_type);
        }
        let mut response = request.send(&body[..])?;
        let status = response.status().as_u16();
        let body = response.body_mut().read_to_vec()?;
        Ok::<_, ureq::Error>((status, Bytes::from(body)))
    })
    .await;
    match made {
        Ok(Ok(answered)) => answered,
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

/// An answer this end gives itself: `status` with `{"error": message}`.
fn refusal(status: u16, message: String) -> (u16, Bytes) {
    let body = serde_json::json!({ "error": message }).to_string();
    (status, Bytes::from(body))
}

/// Sends `answer`, then waits for the caller to close the link, as [`super::relay`] does.
async fn finish(send: &Sender<String>, recv: &mut Receiver<String>, answer: &Frame) {
    if send_message(send, answer).await {
        let _ = timeout(OFFLINE_AFTER, async {
            while recv.recv().await.is_some() {}
        })
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_beat_on_a_call_link_is_the_relays_beat() {
        let beat: Frame = serde_json::from_str(r#"{"type":"beat"}"#).unwrap();
        assert!(matches!(beat, Frame::Beat));
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
