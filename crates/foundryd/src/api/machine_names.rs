//! Machine names: each machine adds itself to the shared machine list under its Tailscale
//! short name, and anyone can rename a listed machine from the UI. Both go through the acting
//! coordinator, as every shared-settings change does.
//!
//! - [`Listing`] keeps this machine in the list. Fed what [`entry`](crate::tailscale::entry)
//!   reads from `tailscale status --self --json`, it adds the machine with its public key when
//!   the list does not hold it, moves its address when that changed, puts its key back when the
//!   entry holds none or another, and never touches a name it has seen set.
//! - `POST /api/machines/{id}/rename` `{name}`: sets a listed machine's name, trimmed, and
//!   keeps its address, `204`. A name that is blank, over [`NAME_AT_MOST`] characters or holds
//!   a control character is `422`; a name another machine has, compared trimmed and in any
//!   case, is `409`; an id the list does not hold is `404`. Nothing is sent when a rename is
//!   refused, and an edit the coordinator could not make answers through [`edit_status`].
//!
//! Both decide from this machine's copy of the settings and send the whole entry, as
//! `SetMachine` carries it. So the listing gives peers' copies time to arrive before it
//! decides: it sends an edit only from a look at least the settle time after the look that
//! first found it needed, and that wait starts over when Tailscale was away. A machine renamed
//! elsewhere while it was away, or added to the list by a peer, finds that entry when a peer
//! can reach it within that time. What stays open: a rename sent from a copy that has not yet
//! heard of the machine's new address puts the old address back until the machine's next look
//! moves it again, and an edit numbered elsewhere between a look's read and its send is
//! replaced. The wait runs on a clock that stops while the machine sleeps, so a wait begun just
//! before a sleep can end just after waking, when Tailscale answers at once and before peers
//! have linked again. The default name is not checked as a rename is: a default another machine already
//! has, or one over [`NAME_AT_MOST`] characters, is listed as it is, and a rename sets it right.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use foundry_proto::ids::MachineId;
use foundry_proto::settings::{Edit, Machine};
use serde::Deserialize;
use tokio::time::Instant;

use super::{ApiError, Body, edit_status, on_store, parse_id, unprocessable};
use crate::coordinator::{Coordinator, CoordinatorError};
use crate::store::Store;
use crate::tailscale::Entry;

/// The most characters a machine's name holds.
pub const NAME_AT_MOST: usize = 60;

/// How long after sending its key alone a machine waits before sending it again when its entry
/// still holds none. A machine from before keys were listed drops the key from an edit it
/// numbers, so while one acts, a key sent at once each look would go round forever.
pub const KEY_AGAIN_AFTER: Duration = Duration::from_secs(10 * 60);

/// Keeps this machine in the shared machine list, at the address Tailscale gives it.
pub struct Listing {
    me: MachineId,
    /// This machine's public key, as hex: what its entry carries.
    key: String,
    coordinator: Arc<Coordinator>,
    store: Arc<Mutex<Store>>,
    /// Whether the last look failed, so a failure is warned about once until a look succeeds.
    warned: bool,
    /// How long peers' copies get to arrive before a look acts.
    settle: Duration,
    /// Since when the looks have found an edit to send; `None` while there is none.
    found: Option<Instant>,
    /// Set by the address watch when an ask of Tailscale fails, read and cleared by a look.
    away: Arc<AtomicBool>,
    /// When this machine last sent an edit whose only change was its key.
    key_sent: Option<Instant>,
}

impl Listing {
    /// A listing that gives peers' copies `settle` to arrive: the daemon passes
    /// [`OFFLINE_AFTER`](crate::sync::OFFLINE_AFTER), the time a peer has to be heard from.
    /// `away` is the address watch's mark that Tailscale was away. `key` is this machine's
    /// public key, as hex.
    pub fn new(
        me: MachineId,
        key: String,
        coordinator: Arc<Coordinator>,
        store: Arc<Mutex<Store>>,
        settle: Duration,
        away: Arc<AtomicBool>,
    ) -> Self {
        Self {
            me,
            key,
            coordinator,
            store,
            warned: false,
            settle,
            found: None,
            away,
            key_sent: None,
        }
    }

    /// Adds this machine to the list under `entry`'s name, with its key, when the list does not
    /// hold it, and moves it to `entry`'s address and its own key, keeping its name, when the
    /// address or the key it holds differs. A key alone is sent again only [`KEY_AGAIN_AFTER`]
    /// after the last time.
    /// A failure, or no entry, is logged once until a look succeeds; the next look tries again.
    ///
    /// The look that first finds an edit to send sends nothing: one at least the settle time
    /// later that still finds it sends it, so peers' copies can arrive first. A look that finds
    /// nothing to send, or sends, ends the wait, and so does a send that may have been numbered
    /// though it failed (no answer came); one the coordinator says it did not send keeps the
    /// wait over, so the next look tries again. A look after the `away` mark was set starts the wait over: no peer could link
    /// while Tailscale was away, so an edit found before waits again.
    pub async fn list(&mut self, entry: Result<Entry, String>) {
        if self.away.swap(false, Ordering::Relaxed) {
            self.found = None;
        }
        match self.look(entry).await {
            Ok(()) => self.warned = false,
            Err(why) => {
                if !self.warned {
                    tracing::warn!(
                        "cannot keep this machine in the machine list, trying again on the next Tailscale rescan: {why}"
                    );
                }
                self.warned = true;
            }
        }
    }

    async fn look(&mut self, entry: Result<Entry, String>) -> Result<(), String> {
        let entry = entry?;
        let settings = on_store(&self.store, |store| store.settings())
            .await
            .map_err(|e| e.to_string())?;
        let machine = match settings.machines.iter().find(|m| m.id == self.me) {
            None => Machine {
                id: self.me,
                name: entry.name,
                address: entry.address,
                key: self.key.clone(),
            },
            Some(held)
                if held.address != entry.address
                    || held.key != self.key && !self.key_sent_lately() =>
            {
                Machine {
                    address: entry.address,
                    key: self.key.clone(),
                    ..held.clone()
                }
            }
            Some(_) => {
                self.found = None;
                return Ok(());
            }
        };
        let found = *self.found.get_or_insert_with(Instant::now);
        if found.elapsed() < self.settle {
            return Ok(());
        }
        let key_only = settings
            .machines
            .iter()
            .any(|held| held.id == self.me && held.address == machine.address);
        let sent = self.coordinator.edit(Edit::SetMachine { machine }).await;
        // A send that may have reached the coordinator waits again, as a sent one does, for
        // the copy that holds it; one that says it was not sent is tried on the next look.
        if !matches!(
            sent,
            Err(CoordinatorError::NotLinked(_)
                | CoordinatorError::Older { .. }
                | CoordinatorError::Unwritable(_))
        ) {
            self.found = None;
            if key_only {
                self.key_sent = Some(Instant::now());
            }
        }
        sent.map(drop).map_err(|e| e.to_string())
    }
}

impl Listing {
    /// Whether this machine sent its key alone within [`KEY_AGAIN_AFTER`].
    fn key_sent_lately(&self) -> bool {
        self.key_sent
            .is_some_and(|sent| sent.elapsed() < KEY_AGAIN_AFTER)
    }
}

/// What the rename route acts on: the coordinator that sends the edit and the store whose
/// machine list says whether to send it.
struct Names {
    coordinator: Arc<Coordinator>,
    store: Arc<Mutex<Store>>,
}

/// `POST /api/machines/{id}/rename`. The caller guards it as it guards the rest of the API.
pub fn router(coordinator: Arc<Coordinator>, store: Arc<Mutex<Store>>) -> Router {
    Router::new()
        .route("/api/machines/{id}/rename", post(rename))
        .with_state(Arc::new(Names { coordinator, store }))
}

/// `raw` trimmed, or `422` when it is blank (answered with `blank`), over [`NAME_AT_MOST`]
/// characters or holds a control character: the one rule for a machine's name, on a rename
/// and on an add.
pub(super) fn checked_name<'a>(raw: &'a str, blank: &str) -> Result<&'a str, ApiError> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(unprocessable(blank.to_owned()));
    }
    if name.chars().count() > NAME_AT_MOST {
        return Err(unprocessable(format!(
            "a machine's name is at most {NAME_AT_MOST} characters"
        )));
    }
    if name.chars().any(char::is_control) {
        return Err(unprocessable(
            "a machine's name holds no line breaks, tabs or other control characters".to_owned(),
        ));
    }
    Ok(name)
}

#[derive(Deserialize)]
struct Rename {
    name: String,
}

async fn rename(
    State(names): State<Arc<Names>>,
    Path(id): Path<String>,
    Body(rename): Body<Rename>,
) -> Result<Response, ApiError> {
    let id = parse_id(&id, "machine", MachineId)?;
    let name = checked_name(&rename.name, "a machine needs a name")?;
    let settings = on_store(&names.store, |store| store.settings()).await?;
    let Some(held) = settings.machines.iter().find(|m| m.id == id) else {
        return Err(ApiError::NotFound(format!(
            "no machine {id} in the machine list"
        )));
    };
    let wanted = name.to_lowercase();
    if let Some(other) = settings
        .machines
        .iter()
        .find(|m| m.id != id && m.name.trim().to_lowercase() == wanted)
    {
        return Err(ApiError::Rejected(
            StatusCode::CONFLICT,
            format!("another machine is named {:?}", other.name),
        ));
    }
    let machine = Machine {
        name: name.to_owned(),
        ..held.clone()
    };
    match names.coordinator.edit(Edit::SetMachine { machine }).await {
        Ok(_) => Ok(StatusCode::NO_CONTENT.into_response()),
        Err(e) => Err(ApiError::Rejected(edit_status(&e), e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io;
    use std::time::Duration;

    use foundry_proto::ids::MachineId;
    use foundry_proto::messages::{PROTOCOL, PeerMessage};
    use foundry_proto::settings::{Machine, SharedSettings};
    use futures::future::BoxFuture;
    use serde_json::{Value, json};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::*;
    use crate::sync::{Conn, Dialer, Peers};
    use crate::test_log::Logged;

    struct NoDial;

    impl Dialer for NoDial {
        fn dial(&self, _: &Machine) -> BoxFuture<'static, io::Result<Conn>> {
            Box::pin(async { Err(io::Error::other("no peers in these tests")) })
        }
    }

    /// One machine's coordinator and store, its rename route served on a loopback port.
    struct Served {
        _data: tempfile::TempDir,
        me: MachineId,
        store: Arc<Mutex<Store>>,
        peers: Peers,
        coordinator: Arc<Coordinator>,
        addr: std::net::SocketAddr,
        _server: tokio::task::JoinHandle<()>,
    }

    impl Served {
        /// A machine that acts alone, holding the settings `settings` makes.
        async fn new(settings: impl FnOnce(MachineId, &mut SharedSettings)) -> Self {
            let data = tempfile::tempdir().unwrap();
            let me = MachineId::random();
            let mut store = Store::open(data.path().join("foundry.db")).unwrap();
            let mut held = store.settings().unwrap();
            held.seq += 1;
            settings(me, &mut held);
            store.apply_settings(held).unwrap();
            let store = Arc::new(Mutex::new(store));
            let peers = Peers::new(me, store.clone(), Arc::new(NoDial));
            let coordinator = Arc::new(Coordinator::new(me, store.clone(), peers.clone()));
            peers.coordinate(coordinator.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let app = router(coordinator.clone(), store.clone());
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            Self {
                _data: data,
                me,
                store,
                peers,
                coordinator,
                addr,
                _server: server,
            }
        }

        /// A listing that looks from the start, with no wait for peers' copies.
        fn listing(&self) -> Listing {
            self.listing_after(Duration::ZERO)
        }

        fn listing_after(&self, settle: Duration) -> Listing {
            self.listing_marked(settle, Arc::new(AtomicBool::new(false)))
        }

        /// A listing that reads `away`, the mark the address watch sets when Tailscale fails.
        fn listing_marked(&self, settle: Duration, away: Arc<AtomicBool>) -> Listing {
            Listing::new(
                self.me,
                KEY.to_owned(),
                self.coordinator.clone(),
                self.store.clone(),
                settle,
                away,
            )
        }

        fn settings(&self) -> SharedSettings {
            self.store.lock().unwrap().settings().unwrap()
        }

        /// `(status, body)` of a rename of `id` to `name`.
        async fn rename(&self, id: impl std::fmt::Display, name: &str) -> (u16, String) {
            let mut stream = tokio::net::TcpStream::connect(self.addr).await.unwrap();
            let body = json!({ "name": name }).to_string();
            let request = format!(
                "POST /api/machines/{id}/rename HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\
                 Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                self.addr,
                body.len()
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

        /// `peer` says hello and sends nothing, so it is heard from, then its link drops: it
        /// acts, and an edit has nowhere to go.
        async fn hear(&self, peer: MachineId) {
            let (theirs, accepting) = self.link(peer).await;
            drop(theirs);
            accepting.await.unwrap();
        }

        /// `peer` says hello and sends nothing, so it is heard from, and its link stays open
        /// while the returned end is held: an edit reaches it and is never answered.
        async fn link(&self, peer: MachineId) -> (Conn, tokio::task::JoinHandle<()>) {
            let (ours, theirs) = Conn::pair();
            let accepting = tokio::spawn({
                let peers = self.peers.clone();
                async move { peers.accept(ours).await }
            });
            let hello = PeerMessage::Hello {
                protocol: PROTOCOL,
                machine: peer,
                versions: BTreeMap::new(),
            };
            let nothing = PeerMessage::Records {
                protocol: PROTOCOL,
                records: Vec::new(),
            };
            for message in [hello, nothing] {
                theirs
                    .send
                    .send(serde_json::to_string(&message).unwrap())
                    .await
                    .unwrap();
            }
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            while !self.peers.online().contains(&peer) {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the peer was not heard"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            (theirs, accepting)
        }
    }

    fn machine(id: MachineId, name: &str, address: &str) -> Machine {
        Machine {
            id,
            name: name.into(),
            address: address.into(),
            key: String::new(),
        }
    }

    /// This machine's public key in these tests, as hex.
    const KEY: &str = "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c";

    /// This machine's own entry, as its listing writes it: with [`KEY`].
    fn own(id: MachineId, name: &str, address: &str) -> Machine {
        Machine {
            key: KEY.to_owned(),
            ..machine(id, name, address)
        }
    }

    fn here(name: &str, address: &str) -> Result<Entry, String> {
        Ok(Entry {
            name: name.into(),
            address: address.into(),
        })
    }

    fn error_of(body: &str) -> String {
        let json: Value = serde_json::from_str(body).unwrap();
        json["error"].as_str().unwrap().to_owned()
    }

    // --- listing this machine ---

    #[tokio::test]
    async fn a_machine_missing_from_an_empty_list_adds_itself_once() {
        let served = Served::new(|_, _| {}).await;
        let mut listing = served.listing();

        listing.list(here("personal-mac", "100.91.54.18")).await;
        let seq = served.settings().seq;
        listing.list(here("personal-mac", "100.91.54.18")).await;

        assert_eq!(
            served.settings().machines,
            [own(served.me, "personal-mac", "100.91.54.18")]
        );
        assert_eq!(served.settings().seq, seq, "the second look sent nothing");
    }

    #[tokio::test]
    async fn a_machine_missing_from_a_list_of_others_adds_itself_beside_them() {
        let other = machine(MachineId::random(), "trilogy-mac", "100.64.0.2");
        let served = Served::new({
            let other = other.clone();
            move |_, settings| settings.machines.push(other)
        })
        .await;

        served
            .listing()
            .list(here("personal-mac", "100.91.54.18"))
            .await;

        let machines = served.settings().machines;
        assert_eq!(machines.len(), 2, "{machines:?}");
        assert!(machines.contains(&other));
        assert!(machines.contains(&own(served.me, "personal-mac", "100.91.54.18")));
    }

    #[tokio::test]
    async fn a_listed_machine_whose_address_changed_keeps_its_name() {
        let served = Served::new(|me, settings| {
            settings.machines.push(own(me, "Tom's desk", "100.64.0.9"));
        })
        .await;

        served
            .listing()
            .list(here("personal-mac", "100.91.54.18"))
            .await;

        assert_eq!(
            served.settings().machines,
            [own(served.me, "Tom's desk", "100.91.54.18")]
        );
    }

    #[tokio::test]
    async fn a_listed_machine_at_the_same_address_sends_nothing() {
        let served = Served::new(|me, settings| {
            settings
                .machines
                .push(own(me, "Tom's desk", "100.91.54.18"));
        })
        .await;
        let seq = served.settings().seq;

        served
            .listing()
            .list(here("personal-mac", "100.91.54.18"))
            .await;

        assert_eq!(served.settings().seq, seq);
        assert_eq!(
            served.settings().machines,
            [own(served.me, "Tom's desk", "100.91.54.18")]
        );
    }

    #[tokio::test]
    async fn a_listed_machine_with_no_key_sends_its_own_and_keeps_its_name_and_address() {
        let served = Served::new(|me, settings| {
            settings
                .machines
                .push(machine(me, "Tom's desk", "100.91.54.18"));
        })
        .await;

        served
            .listing()
            .list(here("personal-mac", "100.91.54.18"))
            .await;

        assert_eq!(
            served.settings().machines,
            [own(served.me, "Tom's desk", "100.91.54.18")]
        );
    }

    #[tokio::test]
    async fn a_listed_machine_held_with_another_key_sends_its_own_and_keeps_its_name() {
        let served = Served::new(|me, settings| {
            settings.machines.push(Machine {
                key: "ab".repeat(32),
                ..machine(me, "Tom's desk", "100.64.0.9")
            });
        })
        .await;

        served
            .listing()
            .list(here("personal-mac", "100.91.54.18"))
            .await;

        assert_eq!(
            served.settings().machines,
            [own(served.me, "Tom's desk", "100.91.54.18")]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_key_the_list_drops_again_is_sent_again_only_after_a_while() {
        let served = Served::new(|me, settings| {
            settings
                .machines
                .push(machine(me, "Tom's desk", "100.91.54.18"));
        })
        .await;
        let mut listing = served.listing();
        listing.list(here("personal-mac", "100.91.54.18")).await;
        assert_eq!(
            served.settings().machines,
            [own(served.me, "Tom's desk", "100.91.54.18")]
        );
        // An older acting machine numbered it, and its copy came back without the key.
        let mut stripped = served.settings();
        stripped.seq += 1;
        stripped.machines[0].key.clear();
        served
            .store
            .lock()
            .unwrap()
            .apply_settings(stripped)
            .unwrap();
        let seq = served.settings().seq;

        listing.list(here("personal-mac", "100.91.54.18")).await;
        tokio::time::sleep(KEY_AGAIN_AFTER / 2).await;
        listing.list(here("personal-mac", "100.91.54.18")).await;
        assert_eq!(
            served.settings().seq,
            seq,
            "the key is not sent again at once"
        );

        tokio::time::sleep(KEY_AGAIN_AFTER / 2).await;
        listing.list(here("personal-mac", "100.91.54.18")).await;
        assert_eq!(
            served.settings().machines,
            [own(served.me, "Tom's desk", "100.91.54.18")]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_address_move_is_sent_at_once_while_a_dropped_key_waits() {
        let served = Served::new(|me, settings| {
            settings
                .machines
                .push(machine(me, "Tom's desk", "100.91.54.18"));
        })
        .await;
        let mut listing = served.listing();
        listing.list(here("personal-mac", "100.91.54.18")).await;
        let mut stripped = served.settings();
        stripped.seq += 1;
        stripped.machines[0].key.clear();
        served
            .store
            .lock()
            .unwrap()
            .apply_settings(stripped)
            .unwrap();

        listing.list(here("personal-mac", "100.64.0.9")).await;

        assert_eq!(
            served.settings().machines,
            [own(served.me, "Tom's desk", "100.64.0.9")]
        );
    }

    #[tokio::test]
    async fn an_add_the_coordinator_cannot_send_is_warned_once_and_tried_again_each_look() {
        let logged = Logged::default();
        let _capture = logged.capture();
        let peer = MachineId::random();
        let served = Served::new(|me, settings| {
            settings
                .machines
                .push(machine(peer, "trilogy-mac", "100.64.0.2"));
            settings.priority = vec![peer, me];
        })
        .await;
        served.hear(peer).await;
        let mut listing = served.listing();

        listing.list(here("personal-mac", "100.91.54.18")).await;
        listing.list(here("personal-mac", "100.91.54.18")).await;

        let warnings = logged.warnings();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains(&peer.to_string()), "{warnings:?}");
        assert!(!served.settings().machines.iter().any(|m| m.id == served.me));
    }

    const SETTLE: Duration = Duration::from_millis(400);

    /// Looks are this far apart while Tailscale answers, as the daemon's rescans are well
    /// inside its settle time.
    const STEP: Duration = Duration::from_millis(250);

    #[tokio::test]
    async fn the_first_look_waits_the_settle_time_however_long_after_the_start_it_comes() {
        let served = Served::new(|_, _| {}).await;
        let seq = served.settings().seq;
        let mut listing = served.listing_after(SETTLE);
        // Tailscale answers only well after the daemon started: no peer could link before.
        tokio::time::sleep(SETTLE * 2).await;

        listing.list(here("personal-mac", "100.91.54.18")).await;
        assert_eq!(served.settings().seq, seq, "the first look sends nothing");
        tokio::time::sleep(STEP).await;
        listing.list(here("personal-mac", "100.91.54.18")).await;
        assert_eq!(
            served.settings().seq,
            seq,
            "nor does one inside the settle time"
        );

        tokio::time::sleep(STEP).await;
        listing.list(here("personal-mac", "100.91.54.18")).await;
        assert_eq!(
            served.settings().machines,
            [own(served.me, "personal-mac", "100.91.54.18")]
        );
    }

    #[tokio::test]
    async fn an_address_that_moves_between_two_close_looks_waits_the_settle_time() {
        let served = Served::new(|me, settings| {
            settings.machines.push(own(me, "Tom's desk", "100.64.0.9"));
        })
        .await;
        let mut listing = served.listing_after(SETTLE);
        for _ in 0..3 {
            listing.list(here("personal-mac", "100.64.0.9")).await;
            tokio::time::sleep(STEP).await;
        }
        let seq = served.settings().seq;

        // No gap between looks, as after a sleep the clock does not count: the address moved.
        listing.list(here("personal-mac", "100.91.54.18")).await;
        assert_eq!(
            served.settings().seq,
            seq,
            "the look that finds the move sends nothing"
        );
        tokio::time::sleep(STEP).await;
        listing.list(here("personal-mac", "100.91.54.18")).await;
        assert_eq!(
            served.settings().seq,
            seq,
            "nor does one inside the settle time"
        );

        tokio::time::sleep(STEP).await;
        listing.list(here("personal-mac", "100.91.54.18")).await;
        assert_eq!(
            served.settings().machines,
            [own(served.me, "Tom's desk", "100.91.54.18")]
        );
    }

    #[tokio::test]
    async fn a_look_that_finds_nothing_to_send_starts_the_wait_over() {
        let served = Served::new(|me, settings| {
            settings.machines.push(own(me, "Tom's desk", "100.64.0.9"));
        })
        .await;
        let seq = served.settings().seq;
        let mut listing = served.listing_after(SETTLE);

        listing.list(here("personal-mac", "100.91.54.18")).await;
        tokio::time::sleep(STEP).await;
        // The address came back: nothing to send, so the wait is over.
        listing.list(here("personal-mac", "100.64.0.9")).await;
        tokio::time::sleep(STEP).await;
        listing.list(here("personal-mac", "100.91.54.18")).await;
        assert_eq!(
            served.settings().seq,
            seq,
            "a new move waits from where it was found"
        );
    }

    #[tokio::test]
    async fn a_wait_found_before_tailscale_went_away_starts_over_when_it_answers_again() {
        let served = Served::new(|me, settings| {
            settings.machines.push(own(me, "Tom's desk", "100.64.0.9"));
        })
        .await;
        let seq = served.settings().seq;
        let away = Arc::new(AtomicBool::new(false));
        let mut listing = served.listing_marked(SETTLE, away.clone());
        listing.list(here("personal-mac", "100.91.54.18")).await;

        // Tailscale stops answering for longer than the settle time, so no look runs, and the
        // address watch marks it away.
        tokio::time::sleep(SETTLE * 2).await;
        away.store(true, Ordering::Relaxed);
        listing.list(here("personal-mac", "100.91.54.18")).await;
        assert_eq!(
            served.settings().seq,
            seq,
            "the first look after it is back sends nothing"
        );
        tokio::time::sleep(STEP).await;
        listing.list(here("personal-mac", "100.91.54.18")).await;
        assert_eq!(
            served.settings().seq,
            seq,
            "nor does one inside the settle time"
        );

        tokio::time::sleep(STEP).await;
        listing.list(here("personal-mac", "100.91.54.18")).await;
        assert_eq!(
            served.settings().machines,
            [own(served.me, "Tom's desk", "100.91.54.18")]
        );
    }

    #[tokio::test]
    async fn a_send_the_coordinator_refused_is_sent_again_on_the_very_next_look() {
        let peer = MachineId::random();
        let served = Served::new(|me, settings| {
            settings
                .machines
                .push(machine(peer, "trilogy-mac", "100.64.0.2"));
            settings.priority = vec![peer, me];
        })
        .await;
        served.hear(peer).await;
        let mut listing = served.listing_after(SETTLE);
        for _ in 0..3 {
            listing.list(here("personal-mac", "100.91.54.18")).await;
            tokio::time::sleep(STEP).await;
        }
        assert!(
            !served.settings().machines.iter().any(|m| m.id == served.me),
            "the peer acts and cannot be reached, so the send failed"
        );

        // This machine comes to act; the next look, with no wait, sends again.
        let mut acting = served.settings();
        acting.seq += 1;
        acting.priority = vec![served.me, peer];
        served.store.lock().unwrap().apply_settings(acting).unwrap();
        listing.list(here("personal-mac", "100.91.54.18")).await;
        assert!(served.settings().machines.contains(&own(
            served.me,
            "personal-mac",
            "100.91.54.18"
        )));
    }

    #[tokio::test(start_paused = true)]
    async fn a_send_that_got_no_answer_waits_the_settle_time_before_it_is_sent_again() {
        let logged = Logged::default();
        let _capture = logged.capture();
        let peer = MachineId::random();
        let served = Served::new(|me, settings| {
            settings
                .machines
                .push(machine(peer, "trilogy-mac", "100.64.0.2"));
            settings.priority = vec![peer, me];
        })
        .await;
        // The peer acts, keeps its link up, takes the edit and never answers it, so it may
        // have numbered it.
        let (theirs, _accepting) = served.link(peer).await;
        let Conn { send, mut recv } = theirs;
        let _taking = tokio::spawn(async move { while recv.recv().await.is_some() {} });
        let _talking = tokio::spawn(async move {
            let nothing = serde_json::to_string(&PeerMessage::Records {
                protocol: PROTOCOL,
                records: Vec::new(),
            })
            .unwrap();
            while send.send(nothing.clone()).await.is_ok() {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
        let mut listing = served.listing_after(SETTLE);
        listing.list(here("personal-mac", "100.91.54.18")).await;
        tokio::time::sleep(SETTLE).await;
        listing.list(here("personal-mac", "100.91.54.18")).await;
        let warnings = logged.warnings();
        assert!(
            warnings.iter().any(|w| w.contains("no answer came")),
            "{warnings:?}"
        );

        // This machine comes to act, while its copy does not hold the entry the peer may have.
        let mut acting = served.settings();
        acting.seq += 1;
        acting.priority = vec![served.me, peer];
        served.store.lock().unwrap().apply_settings(acting).unwrap();
        let seq = served.settings().seq;
        listing.list(here("personal-mac", "100.91.54.18")).await;
        assert_eq!(
            served.settings().seq,
            seq,
            "the look right after a send that got no answer sends nothing"
        );

        tokio::time::sleep(SETTLE).await;
        listing.list(here("personal-mac", "100.91.54.18")).await;
        assert!(served.settings().machines.contains(&own(
            served.me,
            "personal-mac",
            "100.91.54.18"
        )));
    }

    #[tokio::test]
    async fn after_a_send_the_next_look_waits_for_the_copy_that_holds_it() {
        let served = Served::new(|_, _| {}).await;
        let mut listing = served.listing_after(SETTLE);
        for _ in 0..3 {
            listing.list(here("personal-mac", "100.91.54.18")).await;
            tokio::time::sleep(STEP).await;
        }
        assert_eq!(served.settings().machines.len(), 1);

        // This copy does not hold the entry yet, as a forwarded edit's copy can lag its answer.
        let mut lagging = served.settings();
        lagging.seq += 1;
        lagging.machines.clear();
        served
            .store
            .lock()
            .unwrap()
            .apply_settings(lagging)
            .unwrap();
        let seq = served.settings().seq;

        listing.list(here("personal-mac", "100.91.54.18")).await;
        assert_eq!(
            served.settings().seq,
            seq,
            "the look right after a send sends nothing"
        );
        tokio::time::sleep(STEP).await;
        listing.list(here("personal-mac", "100.91.54.18")).await;
        assert_eq!(
            served.settings().seq,
            seq,
            "nor does one inside the settle time"
        );
        tokio::time::sleep(STEP).await;
        listing.list(here("personal-mac", "100.91.54.18")).await;
        assert_eq!(
            served.settings().machines.len(),
            1,
            "a later look lists it again"
        );
    }

    #[tokio::test]
    async fn once_the_settle_time_has_run_a_failed_look_is_followed_at_once_by_one_that_sends() {
        let served = Served::new(|_, _| {}).await;
        let mut listing = served.listing_after(SETTLE);
        for _ in 0..2 {
            listing.list(here("personal-mac", "100.91.54.18")).await;
            tokio::time::sleep(STEP).await;
        }
        assert!(served.settings().machines.is_empty());

        listing.list(Err("status did not answer".to_owned())).await;
        listing.list(here("personal-mac", "100.91.54.18")).await;
        assert_eq!(served.settings().machines.len(), 1);
    }

    #[tokio::test]
    async fn a_failed_look_is_tried_again_on_the_next_and_a_new_failure_is_warned_again() {
        let logged = Logged::default();
        let _capture = logged.capture();
        let served = Served::new(|_, _| {}).await;
        let mut listing = served.listing();

        listing.list(Err("no IPv4 here".to_owned())).await;
        listing.list(here("personal-mac", "100.91.54.18")).await;
        assert_eq!(
            served.settings().machines,
            [own(served.me, "personal-mac", "100.91.54.18")]
        );
        listing.list(Err("logged out".to_owned())).await;

        let warnings = logged.warnings();
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings[1].contains("logged out"), "{warnings:?}");
    }

    #[tokio::test]
    async fn a_status_that_gives_no_entry_is_warned_once_and_sends_nothing() {
        let logged = Logged::default();
        let _capture = logged.capture();
        let served = Served::new(|_, _| {}).await;
        let seq = served.settings().seq;
        let mut listing = served.listing();

        listing.list(Err("no IPv4 here".to_owned())).await;
        listing.list(Err("no IPv4 here".to_owned())).await;

        let warnings = logged.warnings();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("no IPv4 here"), "{warnings:?}");
        assert_eq!(served.settings().seq, seq);
    }

    // --- renaming ---

    #[tokio::test]
    async fn a_rename_sets_the_trimmed_name_and_keeps_the_address() {
        let served = Served::new(|me, settings| {
            settings
                .machines
                .push(own(me, "personal-mac", "100.91.54.18"));
        })
        .await;

        let (status, body) = served.rename(served.me, "  Tom's desk \n").await;

        assert_eq!(status, 204, "{body}");
        assert_eq!(
            served.settings().machines,
            [own(served.me, "Tom's desk", "100.91.54.18")]
        );
    }

    #[tokio::test]
    async fn another_machine_can_be_renamed_from_here() {
        let other = MachineId::random();
        let served = Served::new(move |_, settings| {
            settings
                .machines
                .push(machine(other, "trilogy-mac", "100.64.0.2"));
        })
        .await;

        let (status, body) = served.rename(other, "work laptop").await;

        assert_eq!(status, 204, "{body}");
        assert_eq!(
            served.settings().machines,
            [machine(other, "work laptop", "100.64.0.2")]
        );
    }

    #[tokio::test]
    async fn a_machine_can_take_its_own_name_in_another_case() {
        let served = Served::new(|me, settings| {
            settings
                .machines
                .push(own(me, "personal-mac", "100.91.54.18"));
        })
        .await;

        let (status, body) = served.rename(served.me, "Personal-Mac").await;

        assert_eq!(status, 204, "{body}");
        assert_eq!(served.settings().machines[0].name, "Personal-Mac");
    }

    #[tokio::test]
    async fn a_name_of_60_characters_is_taken_and_one_of_61_is_not() {
        let served = Served::new(|me, settings| {
            settings
                .machines
                .push(own(me, "personal-mac", "100.91.54.18"));
        })
        .await;
        let (status, body) = served.rename(served.me, &"é".repeat(60)).await;
        assert_eq!(status, 204, "{body}");
        let (status, body) = served.rename(served.me, &"x".repeat(61)).await;
        assert_eq!(status, 422, "{body}");
    }

    #[tokio::test]
    async fn a_blank_long_or_control_character_name_is_422_and_sends_nothing() {
        let served = Served::new(|me, settings| {
            settings
                .machines
                .push(own(me, "personal-mac", "100.91.54.18"));
        })
        .await;
        let seq = served.settings().seq;
        let long = "n".repeat(NAME_AT_MOST + 1);
        for name in [
            "",
            "  \t ",
            long.as_str(),
            "desk\u{0}",
            "my\ndesk",
            "a\u{7}b",
            "a\u{1b}[31mb",
        ] {
            let (status, body) = served.rename(served.me, name).await;
            assert_eq!(status, 422, "{name:?}: {body}");
            assert!(!error_of(&body).is_empty(), "{body}");
        }
        assert_eq!(served.settings().seq, seq);
        assert_eq!(served.settings().machines[0].name, "personal-mac");
    }

    #[tokio::test]
    async fn a_name_another_machine_has_in_any_case_or_spacing_is_409_and_sends_nothing() {
        let other = MachineId::random();
        let served = Served::new(move |me, settings| {
            settings
                .machines
                .push(own(me, "personal-mac", "100.91.54.18"));
            settings
                .machines
                .push(machine(other, " Trilogy-Mac ", "100.64.0.2"));
        })
        .await;
        let seq = served.settings().seq;

        let (status, body) = served.rename(served.me, "  trilogy-MAC ").await;

        assert_eq!(status, 409, "{body}");
        assert!(error_of(&body).contains("Trilogy-Mac"), "{body}");
        assert_eq!(served.settings().seq, seq);
    }

    #[tokio::test]
    async fn a_machine_not_in_the_list_is_404_and_sends_nothing() {
        let served = Served::new(|me, settings| {
            settings
                .machines
                .push(own(me, "personal-mac", "100.91.54.18"));
        })
        .await;
        let seq = served.settings().seq;
        for id in [MachineId::random().to_string(), "nonsense".to_owned()] {
            let (status, body) = served.rename(&id, "desk").await;
            assert_eq!(status, 404, "{id}: {body}");
            assert!(!error_of(&body).is_empty(), "{body}");
        }
        assert_eq!(served.settings().seq, seq);
    }

    #[tokio::test]
    async fn a_rename_the_coordinator_cannot_send_answers_503_and_keeps_the_name() {
        let peer = MachineId::random();
        let served = Served::new(move |me, settings| {
            settings
                .machines
                .push(own(me, "personal-mac", "100.91.54.18"));
            settings
                .machines
                .push(machine(peer, "trilogy-mac", "100.64.0.2"));
            settings.priority = vec![peer, me];
        })
        .await;
        served.hear(peer).await;

        let (status, body) = served.rename(served.me, "desk").await;

        assert_eq!(status, 503, "{body}");
        assert!(error_of(&body).contains(&peer.to_string()), "{body}");
        let mine = served
            .settings()
            .machines
            .into_iter()
            .find(|m| m.id == served.me);
        assert_eq!(mine.unwrap().name, "personal-mac");
    }
}
