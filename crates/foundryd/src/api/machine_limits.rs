//! Machine limits: the maximum sessions, the maximum load per core and the minimum free disk
//! placement keeps a machine under, set and cleared from the machines board. Both routes go
//! through the acting coordinator, as every shared-settings change does.
//!
//! - `POST /api/machines/{id}/limits` `{max_sessions, max_load_per_core, min_free_disk_gb}`:
//!   sets a listed machine's limits, `204`. The disk limit is stored in bytes, rounded down. A
//!   `max_sessions` that is not a whole number of at least 1, a `max_load_per_core` that is not
//!   finite and above 0, or a `min_free_disk_gb` that is not finite and at least 0 is `422`.
//! - `POST /api/machines/{id}/limits/clear`: removes the machine's limits, `204`.
//!
//! An id the machine list does not hold is `404`. Nothing is sent when a request is refused,
//! and an edit the coordinator could not make answers through [`edit_status`].

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use foundry_proto::ids::MachineId;
use foundry_proto::settings::{Edit, Limits};
use serde::Deserialize;

use super::{ApiError, Body, edit_status, on_store, parse_id, unprocessable};
use crate::coordinator::Coordinator;
use crate::store::Store;

/// What the limits routes act on: the coordinator that sends the edit and the store whose
/// machine list says whether to send it.
struct Held {
    coordinator: Arc<Coordinator>,
    store: Arc<Mutex<Store>>,
}

/// The limits routes. The caller guards them as it guards the rest of the API.
pub fn router(coordinator: Arc<Coordinator>, store: Arc<Mutex<Store>>) -> Router {
    Router::new()
        .route("/api/machines/{id}/limits", post(set))
        .route("/api/machines/{id}/limits/clear", post(clear))
        .with_state(Arc::new(Held { coordinator, store }))
}

#[derive(Deserialize)]
struct Wanted {
    max_sessions: f64,
    max_load_per_core: f64,
    min_free_disk_gb: f64,
}

/// `gb` gigabytes (10^9 bytes) as bytes, rounded down, read from the number's shortest decimal
/// text so no float error creeps in. `None` when it is negative, not finite or past `u64`.
fn gb_to_bytes(gb: f64) -> Option<u64> {
    if !gb.is_finite() || gb < 0.0 {
        return None;
    }
    if gb == 0.0 {
        // Also -0.0, which prints as "-0".
        return Some(0);
    }
    // Display never uses an exponent for an f64.
    let text = format!("{gb}");
    let (whole, fraction) = text.split_once('.').unwrap_or((&text, ""));
    let mut digits: String = fraction.chars().take(9).collect();
    while digits.len() < 9 {
        digits.push('0');
    }
    let whole: u64 = whole.parse().ok()?;
    let fraction: u64 = digits.parse().ok()?;
    whole.checked_mul(1_000_000_000)?.checked_add(fraction)
}

/// The limits `wanted` asks for, or why they are refused.
fn checked(wanted: &Wanted) -> Result<Limits, ApiError> {
    let sessions = wanted.max_sessions;
    if !sessions.is_finite()
        || sessions.fract() != 0.0
        || sessions < 1.0
        || sessions > f64::from(u32::MAX)
    {
        return Err(unprocessable(format!(
            "the maximum sessions is a whole number from 1 to {}",
            u32::MAX
        )));
    }
    let load = wanted.max_load_per_core;
    if !load.is_finite() || load <= 0.0 {
        return Err(unprocessable(
            "the maximum load per core is a number above 0".to_owned(),
        ));
    }
    let Some(min_free_disk) = gb_to_bytes(wanted.min_free_disk_gb) else {
        return Err(unprocessable(
            "the minimum free disk is a number of gigabytes, 0 or more".to_owned(),
        ));
    };
    Ok(Limits {
        // Whole and in range, checked above.
        max_sessions: sessions as u32,
        max_load_per_core: load,
        min_free_disk,
    })
}

/// `404` unless the machine list holds `id`.
async fn listed(held: &Held, id: &str) -> Result<MachineId, ApiError> {
    let id = parse_id(id, "machine", MachineId)?;
    let settings = on_store(&held.store, |store| store.settings()).await?;
    if !settings.machines.iter().any(|m| m.id == id) {
        return Err(ApiError::NotFound(format!(
            "no machine {id} in the machine list"
        )));
    }
    Ok(id)
}

async fn send(held: &Held, edit: Edit) -> Result<Response, ApiError> {
    match held.coordinator.edit(edit).await {
        Ok(_) => Ok(StatusCode::NO_CONTENT.into_response()),
        Err(e) => Err(ApiError::Rejected(edit_status(&e), e.to_string())),
    }
}

async fn set(
    State(held): State<Arc<Held>>,
    Path(id): Path<String>,
    Body(wanted): Body<Wanted>,
) -> Result<Response, ApiError> {
    let machine = listed(&held, &id).await?;
    let limits = checked(&wanted)?;
    send(&held, Edit::SetLimits { machine, limits }).await
}

async fn clear(
    State(held): State<Arc<Held>>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let machine = listed(&held, &id).await?;
    send(&held, Edit::RemoveLimits { machine }).await
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::io;
    use std::time::Duration;

    use foundry_proto::messages::{PROTOCOL, PeerMessage};
    use foundry_proto::record::{AnyRecord, MachineSample, Record};
    use foundry_proto::settings::{Machine, Project, SharedSettings};
    use futures::future::BoxFuture;
    use serde_json::{Value, json};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::*;
    use crate::sync::{Conn, Dialer, Peers};

    struct NoDial;

    impl Dialer for NoDial {
        fn dial(&self, _: &Machine) -> BoxFuture<'static, io::Result<Conn>> {
            Box::pin(async { Err(io::Error::other("no peers in these tests")) })
        }
    }

    /// One machine's coordinator and store, its limits routes served on a loopback port, and,
    /// when asked for, the rest of the API beside them.
    struct Served {
        _data: tempfile::TempDir,
        me: MachineId,
        store: Arc<Mutex<Store>>,
        peers: Peers,
        addr: std::net::SocketAddr,
        _server: tokio::task::JoinHandle<()>,
    }

    impl Served {
        /// A machine that acts alone, holding the settings `settings` makes and listing itself
        /// as `studio`.
        async fn new(settings: impl FnOnce(MachineId, &mut SharedSettings)) -> Self {
            Self::start(settings, false).await
        }

        /// [`Self::new`] with the API's routes served beside the limits routes.
        async fn with_api(settings: impl FnOnce(MachineId, &mut SharedSettings)) -> Self {
            Self::start(settings, true).await
        }

        async fn start(settings: impl FnOnce(MachineId, &mut SharedSettings), api: bool) -> Self {
            let data = tempfile::tempdir().unwrap();
            let me = MachineId::random();
            let mut store = Store::open(data.path().join("foundry.db")).unwrap();
            let mut held = store.settings().unwrap();
            held.seq += 1;
            held.machines.push(Machine {
                id: me,
                name: "studio".into(),
                address: "100.64.0.1".into(),
                key: String::new(),
            });
            settings(me, &mut held);
            store.apply_settings(held).unwrap();
            let store = Arc::new(Mutex::new(store));
            let peers = Peers::new(me, store.clone(), Arc::new(NoDial));
            let coordinator = Arc::new(Coordinator::new(me, store.clone(), peers.clone()));
            peers.coordinate(coordinator.clone());
            let mut app = router(coordinator, store.clone());
            if api {
                // Nothing is started: no holder or agent runs, the paths are never used.
                let terminals = Arc::new(crate::terminals::Terminals::new(
                    me,
                    store.clone(),
                    data.path(),
                    "foundry-holder".into(),
                ));
                let workspaces = Arc::new(crate::workspaces::Workspaces::new(
                    me,
                    store.clone(),
                    data.path(),
                    terminals.clone(),
                ));
                let config = crate::sessions::Config {
                    claude: vec!["claude".into()],
                    claude_json: data.path().join(".claude.json"),
                    hook_port: 9,
                    gateway: crate::sessions::Gateway {
                        base_url: "http://127.0.0.1:9/gateway".into(),
                        key: "gw-key".into(),
                    },
                };
                let sessions = Arc::new(crate::sessions::Sessions::new(
                    me,
                    store.clone(),
                    terminals.clone(),
                    data.path(),
                    config,
                ));
                let api = crate::api::Api::new(
                    store.clone(),
                    peers.clone(),
                    terminals,
                    workspaces,
                    sessions,
                    vec![],
                );
                app = app.merge(crate::api::router(api));
            }
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            Self {
                _data: data,
                me,
                store,
                peers,
                addr,
                _server: server,
            }
        }

        fn settings(&self) -> SharedSettings {
            self.store.lock().unwrap().settings().unwrap()
        }

        /// `(status, body)` of a POST of `body` (none when `None`) to `path`.
        async fn post(&self, path: &str, body: Option<&str>) -> (u16, String) {
            let mut stream = tokio::net::TcpStream::connect(self.addr).await.unwrap();
            let body = body.unwrap_or_default();
            let request = format!(
                "POST {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\
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

        /// The limits route of `id` given `body`.
        async fn limit(&self, id: impl std::fmt::Display, body: &Value) -> (u16, String) {
            self.post(
                &format!("/api/machines/{id}/limits"),
                Some(&body.to_string()),
            )
            .await
        }

        async fn clear(&self, id: impl std::fmt::Display) -> (u16, String) {
            self.post(&format!("/api/machines/{id}/limits/clear"), None)
                .await
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

        /// Stores `owner`'s newest sample, as its sampler or sync would.
        fn sample(&self, owner: MachineId, sessions: u32, load: f64, free_disk: u64) {
            let mut store = self.store.lock().unwrap();
            let version = if owner == self.me {
                store.own_next_version().unwrap()
            } else {
                store.versions().unwrap().get(&owner).copied().unwrap_or(0) + 1
            };
            let record = AnyRecord::MachineSample(Record {
                owner,
                version,
                deleted: false,
                body: MachineSample {
                    load_per_core: load,
                    free_memory: 8_000_000_000,
                    free_disk,
                    sessions,
                    update_refused: None,
                },
            });
            assert_eq!(
                store.apply(&record, owner).unwrap(),
                crate::store::Applied::Stored
            );
        }

        /// The samples of `owners` the store holds, in that order, as `place` takes them.
        fn samples(&self, owners: &[MachineId]) -> Vec<Record<MachineSample>> {
            let store = self.store.lock().unwrap();
            owners
                .iter()
                .flat_map(|owner| store.since(*owner, 0).unwrap())
                .filter_map(|r| match r {
                    AnyRecord::MachineSample(s) => Some(s),
                    _ => None,
                })
                .collect()
        }
    }

    fn error_of(body: &str) -> String {
        let json: Value = serde_json::from_str(body).unwrap();
        json["error"].as_str().unwrap().to_owned()
    }

    fn wanted(sessions: Value, load: Value, disk: Value) -> Value {
        json!({ "max_sessions": sessions, "max_load_per_core": load, "min_free_disk_gb": disk })
    }

    #[test]
    fn gigabytes_become_exact_bytes_rounded_down() {
        assert_eq!(gb_to_bytes(20.0), Some(20_000_000_000));
        assert_eq!(gb_to_bytes(0.0), Some(0));
        assert_eq!(gb_to_bytes(-0.0), Some(0));
        assert_eq!(gb_to_bytes(4.35), Some(4_350_000_000));
        assert_eq!(gb_to_bytes(0.1), Some(100_000_000));
        assert_eq!(gb_to_bytes(0.000_000_001), Some(1));
        assert_eq!(gb_to_bytes(0.000_000_001_9), Some(1));
        assert_eq!(gb_to_bytes(0.000_000_000_9), Some(0));
        assert_eq!(
            gb_to_bytes(18_446_744_073.0),
            Some(18_446_744_073_000_000_000)
        );
    }

    #[test]
    fn gigabytes_that_are_negative_not_finite_or_past_u64_have_no_bytes() {
        for gb in [
            -1.0,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            18_446_744_074.0,
            1e30,
        ] {
            assert_eq!(gb_to_bytes(gb), None, "{gb}");
        }
    }

    #[tokio::test]
    async fn limits_set_are_stored_with_the_disk_in_bytes_and_answer_204() {
        let served = Served::new(|_, _| {}).await;

        let (status, body) = served
            .limit(served.me, &wanted(json!(4), json!(1.5), json!(20)))
            .await;

        assert_eq!(status, 204, "{body}");
        assert_eq!(
            served.settings().limits[&served.me],
            Limits {
                max_sessions: 4,
                max_load_per_core: 1.5,
                min_free_disk: 20_000_000_000,
            }
        );
    }

    #[tokio::test]
    async fn a_whole_number_written_with_a_fraction_is_taken_and_a_fractional_disk_is_exact() {
        let served = Served::new(|_, _| {}).await;

        let (status, body) = served
            .limit(served.me, &wanted(json!(3.0), json!(0.25), json!(4.35)))
            .await;

        assert_eq!(status, 204, "{body}");
        let limits = &served.settings().limits[&served.me];
        assert_eq!(limits.max_sessions, 3);
        assert_eq!(limits.min_free_disk, 4_350_000_000);
    }

    #[tokio::test]
    async fn a_set_then_a_clear_leaves_the_machine_with_no_limits() {
        let served = Served::new(|_, _| {}).await;
        let (status, body) = served
            .limit(served.me, &wanted(json!(2), json!(1), json!(0)))
            .await;
        assert_eq!(status, 204, "{body}");
        assert!(served.settings().limits.contains_key(&served.me));

        let (status, body) = served.clear(served.me).await;

        assert_eq!(status, 204, "{body}");
        assert!(!served.settings().limits.contains_key(&served.me));
    }

    #[tokio::test]
    async fn limits_that_are_refused_answer_422_with_an_error_and_send_nothing() {
        let served = Served::new(|_, _| {}).await;
        let seq = served.settings().seq;
        let bad = [
            wanted(json!(0), json!(1), json!(1)),
            wanted(json!(2.5), json!(1), json!(1)),
            wanted(json!(-1), json!(1), json!(1)),
            wanted(json!(4_294_967_296_u64), json!(1), json!(1)),
            wanted(json!(2), json!(0), json!(1)),
            wanted(json!(2), json!(-0.5), json!(1)),
            wanted(json!(2), json!(1), json!(-1)),
            wanted(json!(2), json!(1), json!(1e30)),
            wanted(json!("two"), json!(1), json!(1)),
            wanted(json!(2), json!(null), json!(1)),
            wanted(json!(2), json!(1), json!(null)),
            json!({ "max_sessions": 2, "max_load_per_core": 1 }),
            json!({}),
        ];
        for body in &bad {
            let (status, answer) = served.limit(served.me, body).await;
            assert_eq!(status, 422, "{body}: {answer}");
            assert!(!error_of(&answer).is_empty(), "{body}: {answer}");
        }
        assert_eq!(served.settings().seq, seq);
        assert!(served.settings().limits.is_empty());
    }

    #[tokio::test]
    async fn a_machine_the_list_does_not_hold_is_404_and_nothing_is_sent() {
        let served = Served::new(|_, _| {}).await;
        let seq = served.settings().seq;
        for id in [MachineId::random().to_string(), "not-an-id".to_owned()] {
            let (status, body) = served
                .limit(&id, &wanted(json!(2), json!(1), json!(1)))
                .await;
            assert_eq!(status, 404, "{id}: {body}");
            assert!(!error_of(&body).is_empty(), "{body}");
            let (status, body) = served.clear(&id).await;
            assert_eq!(status, 404, "{id}: {body}");
            assert!(!error_of(&body).is_empty(), "{body}");
        }
        assert_eq!(served.settings().seq, seq);
    }

    #[tokio::test]
    async fn limits_the_coordinator_cannot_send_answer_503_and_change_nothing() {
        let peer = MachineId::random();
        let served = Served::new(move |me, settings| {
            settings.machines.push(Machine {
                id: peer,
                name: "trilogy-mac".into(),
                address: "100.64.0.2".into(),
                key: String::new(),
            });
            settings.priority = vec![peer, me];
        })
        .await;
        served.hear(peer).await;
        let seq = served.settings().seq;

        let (status, body) = served
            .limit(served.me, &wanted(json!(2), json!(1), json!(1)))
            .await;
        assert_eq!(status, 503, "{body}");
        assert!(error_of(&body).contains(&peer.to_string()), "{body}");
        let (status, body) = served.clear(served.me).await;
        assert_eq!(status, 503, "{body}");

        assert_eq!(served.settings().seq, seq);
        assert!(served.settings().limits.is_empty());
    }

    #[tokio::test]
    async fn a_machine_over_the_limits_set_through_the_route_is_skipped() {
        let other = MachineId::random();
        let served = Served::new(|_, _| {}).await;
        let (status, body) = served
            .limit(served.me, &wanted(json!(2), json!(1), json!(20)))
            .await;
        assert_eq!(status, 204, "{body}");
        // This machine is at its session limit; the other has no limits to be over.
        served.sample(served.me, 2, 0.1, 100_000_000_000);
        served.sample(other, 9, 5.0, 1);
        let online = BTreeSet::from([served.me, other]);

        let placed = crate::placement::place(
            &served.samples(&[served.me, other]),
            &served.settings().limits,
            &online,
        );

        assert_eq!(placed, Ok(other));
    }

    #[tokio::test]
    async fn a_machine_under_the_limits_set_through_the_route_is_picked() {
        let served = Served::new(|_, _| {}).await;
        let (status, body) = served
            .limit(served.me, &wanted(json!(2), json!(1), json!(20)))
            .await;
        assert_eq!(status, 204, "{body}");
        served.sample(served.me, 1, 0.1, 100_000_000_000);
        let online = BTreeSet::from([served.me]);

        let placed = crate::placement::place(
            &served.samples(&[served.me]),
            &served.settings().limits,
            &online,
        );

        assert_eq!(placed, Ok(served.me));
    }

    #[tokio::test]
    async fn with_every_machine_over_its_limits_a_choose_waits_with_reasons_in_the_ui_wording() {
        let project = foundry_proto::ids::ProjectId::random();
        let served = Served::with_api(move |_, settings| {
            settings.projects.push(Project {
                id: project,
                name: "demo".into(),
                repo: "/tmp/demo.git".into(),
            });
        })
        .await;
        let (status, body) = served
            .limit(served.me, &wanted(json!(2), json!(1), json!(20)))
            .await;
        assert_eq!(status, 204, "{body}");
        served.sample(served.me, 2, 0.1, 100_000_000_000);

        let (status, body) = served
            .post(
                "/api/workspaces",
                Some(
                    &json!({
                        "project": project, "base": "main", "name": "x",
                        "machine": "choose", "model": "claude-sonnet-5-5",
                    })
                    .to_string(),
                ),
            )
            .await;

        assert_eq!(status, 202, "{body}");
        let answer: Value = serde_json::from_str(&body).unwrap();
        let online = BTreeSet::from([served.me]);
        let Err(reasons) = crate::placement::place(
            &served.samples(&[served.me]),
            &served.settings().limits,
            &online,
        ) else {
            panic!("a machine at its limit was placed");
        };
        let worded: Vec<String> = reasons.iter().map(|r| r.text("studio")).collect();
        assert_eq!(worded, ["studio: sessions 2 at limit 2"]);
        assert_eq!(answer["reasons"], json!(worded));
    }
}
