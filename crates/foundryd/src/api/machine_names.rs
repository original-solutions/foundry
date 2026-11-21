//! Machine names: each machine adds itself to the shared machine list under its Tailscale
//! short name, and anyone can rename a listed machine from the UI. Both go through the acting
//! coordinator, as every shared-settings change does.
//!
//! - [`Listing`] keeps this machine in the list. Fed what [`entry`] reads from
//!   `tailscale status --self --json`, it adds the machine when the list does not hold it,
//!   moves its address when that changed, and never touches a name already set.
//! - `POST /api/machines/{id}/rename` `{name}`: sets a listed machine's name, trimmed, and
//!   keeps its address, `204`. A name that is blank, over [`NAME_AT_MOST`] characters or holds
//!   a control character is `422`; a name another machine has, compared trimmed and in any
//!   case, is `409`; an id the list does not hold is `404`. Nothing is sent when a rename is
//!   refused, and an edit the coordinator could not make answers through [`edit_status`].
//!
//! Both decide from this machine's copy of the settings and send the whole entry, as
//! `SetMachine` carries it. So the listing waits until peers have had time to send their
//! copies before its first look: a machine renamed elsewhere while it was away, or added to the
//! list by a peer, finds that entry before it decides anything. What stays open: a rename sent
//! from a copy that has not yet heard of the machine's new address puts the old address back
//! until the machine's next look moves it again, and an edit numbered elsewhere between a look's
//! read and its send is replaced. The listing's default name is not checked as a rename is: two
//! machines that fall back to one host name are both listed under it.

use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use foundry_proto::ids::MachineId;
use foundry_proto::settings::{Edit, Machine};
use serde::Deserialize;

use super::{ApiError, Body, edit_status, on_store, parse_id, unprocessable};
use crate::coordinator::Coordinator;
use crate::store::Store;

/// The most characters a machine's name holds.
pub const NAME_AT_MOST: usize = 60;

/// What Tailscale says this machine is called and where it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Its default name: the first label of its MagicDNS name.
    pub name: String,
    /// Its Tailscale IPv4 address.
    pub address: String,
}

/// Reads `tailscale status --self --json`: the first label of `Self.DNSName` as the name
/// (`personal-mac.tail459612.ts.net.` gives `personal-mac`), else `host`, cut to
/// [`NAME_AT_MOST`] characters, and the first IPv4 in `Self.TailscaleIPs` as the address.
/// `Self.HostName` is a display name with spaces, so it is not used.
pub fn entry(status: &[u8], host: Option<&str>) -> Result<Entry, String> {
    #[derive(Deserialize)]
    struct Status {
        #[serde(rename = "Self")]
        me: Node,
    }
    #[derive(Deserialize)]
    struct Node {
        #[serde(rename = "DNSName", default)]
        dns_name: String,
        #[serde(rename = "TailscaleIPs", default)]
        ips: Vec<String>,
    }
    let status: Status = serde_json::from_slice(status).map_err(|e| {
        format!("`tailscale status --self --json` did not say what this machine is: {e}")
    })?;
    let name = status
        .me
        .dns_name
        .split('.')
        .next()
        .filter(|label| !label.is_empty())
        .or(host.filter(|host| !host.is_empty()))
        .ok_or("Tailscale gives this machine no DNS name and it has no host name")?;
    let address = status
        .me
        .ips
        .iter()
        .find(|ip| ip.parse::<Ipv4Addr>().is_ok())
        .ok_or("Tailscale reports no IPv4 address for this machine")?;
    Ok(Entry {
        name: name.chars().take(NAME_AT_MOST).collect(),
        address: address.clone(),
    })
}

/// Keeps this machine in the shared machine list, at the address Tailscale gives it.
pub struct Listing {
    me: MachineId,
    coordinator: Arc<Coordinator>,
    store: Arc<Mutex<Store>>,
    /// Whether the last look failed, so a failure is warned about once until a look succeeds.
    warned: bool,
    /// Until when a look does nothing, so peers' copies of the settings can arrive first.
    settled: tokio::time::Instant,
}

impl Listing {
    /// A listing whose looks do nothing for `settle` from now: the daemon passes
    /// [`OFFLINE_AFTER`](crate::sync::OFFLINE_AFTER), the time a peer has to be heard from.
    pub fn new(
        me: MachineId,
        coordinator: Arc<Coordinator>,
        store: Arc<Mutex<Store>>,
        settle: std::time::Duration,
    ) -> Self {
        Self {
            me,
            coordinator,
            store,
            warned: false,
            settled: tokio::time::Instant::now() + settle,
        }
    }

    /// Adds this machine to the list under `entry`'s name when the list does not hold it, and
    /// moves it to `entry`'s address, keeping its name, when the address it holds differs.
    /// A failure, or no entry, is logged once until a look succeeds; the next look tries again.
    /// A look before the settle time does nothing.
    pub async fn list(&mut self, entry: Result<Entry, String>) {
        if tokio::time::Instant::now() < self.settled {
            return;
        }
        match self.send(entry).await {
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

    async fn send(&self, entry: Result<Entry, String>) -> Result<(), String> {
        let entry = entry?;
        let settings = on_store(&self.store, |store| store.settings())
            .await
            .map_err(|e| e.to_string())?;
        let machine = match settings.machines.iter().find(|m| m.id == self.me) {
            None => Machine {
                id: self.me,
                name: entry.name,
                address: entry.address,
            },
            Some(held) if held.address != entry.address => Machine {
                address: entry.address,
                ..held.clone()
            },
            Some(_) => return Ok(()),
        };
        self.coordinator
            .edit(Edit::SetMachine { machine })
            .await
            .map(drop)
            .map_err(|e| e.to_string())
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
    let name = rename.name.trim();
    if name.is_empty() {
        return Err(unprocessable("a machine needs a name".to_owned()));
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

    /// `tailscale status --self --json` from a real Mac, cut to the fields read and a few
    /// beside them.
    const STATUS: &str = r#"{
        "Version": "1.90.6",
        "BackendState": "Running",
        "Self": {
            "ID": "n1",
            "HostName": "Personal Mac",
            "DNSName": "personal-mac.tail459612.ts.net.",
            "OS": "macOS",
            "TailscaleIPs": ["100.91.54.18", "fd7a:115c:a1e0::e832:3612"],
            "Online": true
        },
        "MagicDNSSuffix": "tail459612.ts.net"
    }"#;

    #[test]
    fn the_name_is_the_first_label_of_the_dns_name_and_the_address_the_first_ipv4() {
        assert_eq!(
            entry(STATUS.as_bytes(), Some("personal-mac-host")),
            Ok(Entry {
                name: "personal-mac".into(),
                address: "100.91.54.18".into(),
            })
        );
    }

    #[test]
    fn the_ipv4_is_taken_wherever_it_sits_among_the_addresses() {
        let status = STATUS.replace(
            r#"["100.91.54.18", "fd7a:115c:a1e0::e832:3612"]"#,
            r#"["fd7a:115c:a1e0::e832:3612", "100.91.54.18"]"#,
        );
        assert_eq!(
            entry(status.as_bytes(), None).map(|e| e.address),
            Ok("100.91.54.18".to_owned())
        );
    }

    #[test]
    fn with_no_dns_name_the_host_name_is_the_name() {
        for blank in [r#""DNSName": "","#, ""] {
            let status = STATUS.replace(r#""DNSName": "personal-mac.tail459612.ts.net.","#, blank);
            assert_eq!(
                entry(status.as_bytes(), Some("personal-mac-host")),
                Ok(Entry {
                    name: "personal-mac-host".into(),
                    address: "100.91.54.18".into(),
                }),
                "{blank:?}"
            );
        }
    }

    #[test]
    fn with_no_dns_name_and_no_host_name_there_is_no_entry() {
        let status = STATUS.replace(r#""DNSName": "personal-mac.tail459612.ts.net.","#, "");
        assert!(entry(status.as_bytes(), None).is_err());
    }

    #[test]
    fn the_name_is_the_whole_first_label_however_long() {
        let label = "m".repeat(63);
        let status = STATUS.replace("personal-mac.tail", &format!("{label}.tail"));
        assert_eq!(entry(status.as_bytes(), None).map(|e| e.name), Ok(label));
    }

    #[test]
    fn with_no_ipv4_there_is_no_entry() {
        let status = STATUS.replace(r#""100.91.54.18", "#, "");
        let why = entry(status.as_bytes(), Some("host")).unwrap_err();
        assert!(why.contains("IPv4"), "{why}");
    }

    #[test]
    fn an_answer_that_is_not_status_json_is_no_entry() {
        for answer in ["", "Tailscale is stopped.", r#"{"Version": "1.90.6"}"#] {
            assert!(
                entry(answer.as_bytes(), Some("host")).is_err(),
                "{answer:?}"
            );
        }
    }

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
            Listing::new(
                self.me,
                self.coordinator.clone(),
                self.store.clone(),
                settle,
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
            drop(theirs);
            accepting.await.unwrap();
        }
    }

    fn machine(id: MachineId, name: &str, address: &str) -> Machine {
        Machine {
            id,
            name: name.into(),
            address: address.into(),
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

    /// Everything logged while it is set, as plain text.
    #[derive(Clone, Default)]
    struct Logged(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Logged {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Logged {
        /// Captures this thread's log until the guard drops.
        fn capture(&self) -> tracing::subscriber::DefaultGuard {
            let logged = self.clone();
            let subscriber = tracing_subscriber::fmt()
                .with_writer(move || logged.clone())
                .with_ansi(false)
                .without_time()
                .finish();
            tracing::subscriber::set_default(subscriber)
        }

        fn warnings(&self) -> Vec<String> {
            String::from_utf8_lossy(&self.0.lock().unwrap())
                .lines()
                .filter(|line| line.contains("WARN"))
                .map(str::to_owned)
                .collect()
        }
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
            [machine(served.me, "personal-mac", "100.91.54.18")]
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
        assert!(machines.contains(&machine(served.me, "personal-mac", "100.91.54.18")));
    }

    #[tokio::test]
    async fn a_listed_machine_whose_address_changed_keeps_its_name() {
        let served = Served::new(|me, settings| {
            settings
                .machines
                .push(machine(me, "Tom's desk", "100.64.0.9"));
        })
        .await;

        served
            .listing()
            .list(here("personal-mac", "100.91.54.18"))
            .await;

        assert_eq!(
            served.settings().machines,
            [machine(served.me, "Tom's desk", "100.91.54.18")]
        );
    }

    #[tokio::test]
    async fn a_listed_machine_at_the_same_address_sends_nothing() {
        let served = Served::new(|me, settings| {
            settings
                .machines
                .push(machine(me, "Tom's desk", "100.91.54.18"));
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
            [machine(served.me, "Tom's desk", "100.91.54.18")]
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

    const SETTLE: Duration = Duration::from_millis(300);

    #[tokio::test]
    async fn the_first_look_waits_the_settle_time_however_long_after_the_start_it_comes() {
        let served = Served::new(|_, _| {}).await;
        let seq = served.settings().seq;
        let mut listing = served.listing_after(SETTLE);
        // Tailscale answers only well after the daemon started: no peer could link before.
        tokio::time::sleep(SETTLE * 2).await;

        listing.list(here("personal-mac", "100.91.54.18")).await;
        assert_eq!(served.settings().seq, seq, "the first look sends nothing");

        tokio::time::sleep(SETTLE / 2).await;
        listing.list(here("personal-mac", "100.91.54.18")).await;
        assert_eq!(
            served.settings().seq,
            seq,
            "nor does one inside the settle time"
        );

        tokio::time::sleep(SETTLE).await;
        listing.list(here("personal-mac", "100.91.54.18")).await;
        assert_eq!(
            served.settings().machines,
            [machine(served.me, "personal-mac", "100.91.54.18")]
        );
    }

    #[tokio::test]
    async fn a_look_after_a_gap_longer_than_the_settle_time_waits_again() {
        let served = Served::new(|_, _| {}).await;
        let seq = served.settings().seq;
        let mut listing = served.listing_after(SETTLE);
        listing.list(Err("not yet".to_owned())).await;
        tokio::time::sleep(SETTLE + SETTLE / 2).await;
        listing.list(Err("still not".to_owned())).await;

        // Tailscale went away: no look for longer than the settle time.
        tokio::time::sleep(SETTLE * 2).await;
        listing.list(here("personal-mac", "100.91.54.18")).await;
        assert_eq!(
            served.settings().seq,
            seq,
            "the first look after the gap sends nothing"
        );

        tokio::time::sleep(SETTLE + SETTLE / 4).await;
        listing.list(here("personal-mac", "100.91.54.18")).await;
        assert_eq!(served.settings().machines.len(), 1);
    }

    #[tokio::test]
    async fn after_a_send_the_next_look_waits_for_the_copy_that_holds_it() {
        let served = Served::new(|_, _| {}).await;
        let mut listing = served.listing_after(SETTLE);
        listing.list(here("personal-mac", "100.91.54.18")).await;
        tokio::time::sleep(SETTLE + SETTLE / 4).await;
        listing.list(here("personal-mac", "100.91.54.18")).await;
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
            [machine(served.me, "personal-mac", "100.91.54.18")]
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
                .push(machine(me, "personal-mac", "100.91.54.18"));
        })
        .await;

        let (status, body) = served.rename(served.me, "  Tom's desk \n").await;

        assert_eq!(status, 204, "{body}");
        assert_eq!(
            served.settings().machines,
            [machine(served.me, "Tom's desk", "100.91.54.18")]
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
                .push(machine(me, "personal-mac", "100.91.54.18"));
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
                .push(machine(me, "personal-mac", "100.91.54.18"));
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
                .push(machine(me, "personal-mac", "100.91.54.18"));
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
                .push(machine(me, "personal-mac", "100.91.54.18"));
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
                .push(machine(me, "personal-mac", "100.91.54.18"));
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
                .push(machine(me, "personal-mac", "100.91.54.18"));
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
