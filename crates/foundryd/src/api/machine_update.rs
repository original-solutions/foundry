//! Installing a machine's ready release, from the UI of any machine.
//!
//! - `POST /api/machines/{id}/update` `{"tag": "v0.9.1"}`: installs the ready release on
//!   machine `{id}` ([`update::install_ready`]) and answers `202` with no body. The daemon then
//!   stops, and the service starts the installed version on trial.
//!
//! A call naming another machine is carried there ([`carry`](super::carry)): a machine not in
//! the machine list is `404`, one offline, whose link is down or that speaks a protocol below
//! [`UPDATES_SINCE`](foundry_proto::messages::UPDATES_SINCE) is `503` and names it, and one whose
//! link went down after it was sent is `504`. On the machine itself:
//!
//! - an id that is not this machine's, which only a carried call reaches, is `421`, as a
//!   carried create naming another machine is;
//! - a body with no `tag` string is `400`;
//! - a tag the install refuses is `409` `{"error": "<why>", "ready": "<tag>" | null}`, `ready`
//!   being the tag the machine's sample reports ready;
//! - an install that could not be made is `500` `{"error": "..."}`, and the daemon goes on.
//!
//! After a `202` the daemon stops only once the answer has left: for a request made here, once
//! its response is written; for a carried call, once the link has sent it ([`STOP_AFTER`]). It
//! stops anyway [`STOP_WITHIN`] after the install, when the answer could not be sent.

use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use foundry_proto::ids::MachineId;
use serde::Deserialize;

use super::{Api, ApiError, carry, error, parse_id};
use crate::sync::call::{CARRIED, STOP_AFTER, STOP_WITHIN, Stop};
use crate::update::{self, InstallError};

/// What the install route acts on: this machine's id, its data directory, the version it runs
/// and the program it runs from, and the stop it fires after a `202`.
struct Held {
    me: MachineId,
    home: PathBuf,
    running: String,
    exe: PathBuf,
    stop: Stop,
    /// How long after an install the daemon stops anyway: [`STOP_WITHIN`].
    within: Duration,
}

/// The install route of the machine `api` serves, whose data directory is `home`, behind the
/// layer that carries a call naming another machine there. It fires `stop` once after a `202`.
/// The caller guards it as it guards the rest of the API.
pub fn router(api: Api, home: PathBuf, stop: Stop) -> Router {
    // A program that cannot be found is not run from `current`, which the install refuses.
    let exe = std::env::current_exe().unwrap_or_default();
    routes(Held {
        me: api.peers.me(),
        home,
        running: env!("CARGO_PKG_VERSION").to_owned(),
        exe,
        stop,
        within: STOP_WITHIN,
    })
    .layer(middleware::from_fn_with_state(api, carry::carry))
}

/// The install route on `held`, with no carry layer.
fn routes(held: Held) -> Router {
    Router::new()
        .route("/api/machines/{id}/update", post(install))
        .with_state(Arc::new(held))
}

/// The body the route takes.
#[derive(Deserialize)]
struct Install {
    tag: String,
}

async fn install(
    State(held): State<Arc<Held>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let id = parse_id(&id, "machine", MachineId)?;
    if id != held.me {
        // A call naming another machine is carried there before this runs, so one that
        // reaches here was carried in, and is never carried on.
        return Ok(error(
            StatusCode::MISDIRECTED_REQUEST,
            format!("this call names machine {id}, not this one, and is not carried on"),
        ));
    }
    let Ok(Install { tag }) = serde_json::from_slice(&body) else {
        return Ok(error(
            StatusCode::BAD_REQUEST,
            "the body must be {\"tag\": \"<the ready tag>\"}".to_owned(),
        ));
    };
    let installed = tokio::task::spawn_blocking({
        let held = held.clone();
        move || update::install_ready(&held.home, &tag, &held.running, &held.exe)
    })
    .await;
    match installed {
        Ok(Ok(())) => {
            held.stop.fire_within(held.within);
            if headers.contains_key(CARRIED) {
                // The link that carried the call stops this daemon once it has sent the answer.
                Ok((StatusCode::ACCEPTED, [(STOP_AFTER, "1")]).into_response())
            } else {
                Ok((StatusCode::ACCEPTED, stops_once_written(held.stop.clone())).into_response())
            }
        }
        Ok(Err(InstallError::NotReady { ready, reason })) => {
            let body = serde_json::json!({ "error": reason, "ready": ready });
            Ok((StatusCode::CONFLICT, Json(body)).into_response())
        }
        Ok(Err(InstallError::Failed(failed))) => {
            tracing::warn!("api: the ready release was not installed: {failed}");
            Ok(error(StatusCode::INTERNAL_SERVER_ERROR, failed.to_string()))
        }
        Err(_) => Ok(error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the install stopped before it answered".to_owned(),
        )),
    }
}

/// Fires its stop when dropped.
struct StopOnDrop(Stop);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.fire();
    }
}

/// An empty body that fires `stop` once the server is done with it: when the response is
/// written, or the connection is gone.
fn stops_once_written(stop: Stop) -> Body {
    let guard = StopOnDrop(stop);
    Body::from_stream(futures::stream::poll_fn(move |_| {
        let _held = &guard;
        Poll::Ready(None::<Result<Bytes, Infallible>>)
    }))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::net::SocketAddr;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;

    /// A home on 0.1.0 whose daemon runs from `current`, with `v0.2.0` ready: the exe that
    /// runs it is `versions/0.1.0/foundryd`.
    fn ready_home(state: &str) -> (tempfile::TempDir, PathBuf) {
        let home = tempfile::tempdir().unwrap();
        for version in ["0.1.0", "0.2.0"] {
            let dir = home.path().join("versions").join(version);
            fs::create_dir_all(&dir).unwrap();
            for name in ["foundryd", "foundry-holder"] {
                fs::write(dir.join(name), version).unwrap();
            }
        }
        update::point_current(home.path(), "0.1.0").unwrap();
        fs::write(home.path().join("update.json"), state).unwrap();
        let exe = home.path().join("versions/0.1.0/foundryd");
        (home, exe)
    }

    const READY: &str = r#"{"ready":"v0.2.0"}"#;

    /// What the route must leave alone when it refuses: where `current` points and the state.
    fn snapshot(home: &Path) -> (PathBuf, update::State) {
        (
            fs::read_link(home.join("current")).unwrap(),
            update::state(home).unwrap(),
        )
    }

    /// The install route of machine `me`, on 0.1.0, served on a loopback port, with a stop
    /// that counts and fires by itself after `within`.
    struct Served {
        me: MachineId,
        addr: SocketAddr,
        stops: Arc<AtomicUsize>,
        _server: tokio::task::JoinHandle<()>,
    }

    impl Served {
        async fn new(home: &Path, exe: PathBuf, within: Duration) -> Self {
            let me = MachineId::random();
            let stops = Arc::new(AtomicUsize::new(0));
            let counting = stops.clone();
            let stop = Stop::new(move || {
                counting.fetch_add(1, Ordering::SeqCst);
            });
            let app = routes(Held {
                me,
                home: home.to_owned(),
                running: "0.1.0".to_owned(),
                exe,
                stop,
                within,
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            Self {
                me,
                addr,
                stops,
                _server: server,
            }
        }

        fn stops(&self) -> usize {
            self.stops.load(Ordering::SeqCst)
        }

        /// `(status, whether it asked for a stop after it, body)` of a `POST` of `body` to the
        /// install route of `id`, carried here when `carried`.
        async fn post(
            &self,
            id: impl std::fmt::Display,
            body: &str,
            carried: bool,
        ) -> (u16, bool, String) {
            let url = format!("http://{}/api/machines/{id}/update", self.addr);
            let body = body.to_owned();
            tokio::task::spawn_blocking(move || {
                let agent: ureq::Agent = ureq::Agent::config_builder()
                    .http_status_as_error(false)
                    .timeout_global(Some(Duration::from_secs(20)))
                    .build()
                    .into();
                let mut request = agent.post(url).header("content-type", "application/json");
                if carried {
                    request = request.header(CARRIED, "1");
                }
                let mut response = request.send(body.as_bytes()).unwrap();
                let asked = response.headers().contains_key(STOP_AFTER);
                let text = response.body_mut().read_to_string().unwrap();
                (response.status().as_u16(), asked, text)
            })
            .await
            .unwrap()
        }

        /// Waits up to `limit` for the stop count to reach `count`.
        async fn stopped(&self, count: usize, limit: Duration) -> bool {
            let until = tokio::time::Instant::now() + limit;
            while self.stops() < count {
                if tokio::time::Instant::now() >= until {
                    return false;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            true
        }
    }

    const LONG: Duration = Duration::from_secs(600);
    const BRIEF: Duration = Duration::from_millis(300);

    #[tokio::test]
    async fn a_body_with_no_tag_string_is_400_and_changes_and_stops_nothing() {
        let (home, exe) = ready_home(READY);
        let served = Served::new(home.path(), exe, BRIEF).await;
        let before = snapshot(home.path());

        for body in [
            "",
            "{}",
            r#"{"tag":3}"#,
            r#"{"tag":null}"#,
            "not json",
            "[]",
        ] {
            let (status, asked, text) = served.post(served.me, body, false).await;
            assert_eq!(status, 400, "{body:?}: {text}");
            assert!(!asked, "{body:?}");
            let answer: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert!(answer["error"].as_str().unwrap().contains("tag"), "{text}");
        }

        assert_eq!(snapshot(home.path()), before);
        tokio::time::sleep(BRIEF * 2).await;
        assert_eq!(served.stops(), 0);
    }

    #[tokio::test]
    async fn each_case_the_install_refuses_is_409_with_the_ready_tag_and_stops_nothing() {
        let other = |home: &Path| home.join("elsewhere");
        let trial =
            r#"{"ready":"v0.2.0","trial":{"version":"0.1.0","previous":"0.0.9","starts":0}}"#;
        let cases: [(&str, &str, bool, Option<&str>, &str); 4] = [
            (READY, "v0.3.0", false, Some("v0.2.0"), "not the ready tag"),
            (READY, "v0.2.0", true, None, "not run from"),
            (
                r#"{"ready":"v0.2.0","refused":"v0.2.0"}"#,
                "v0.2.0",
                false,
                None,
                "refused",
            ),
            (
                trial,
                "v0.2.0",
                false,
                Some("v0.2.0"),
                "a trial of 0.1.0 is on",
            ),
        ];
        for (state, tag, elsewhere, ready, reason) in cases {
            let (home, exe) = ready_home(state);
            let exe = if elsewhere {
                fs::write(other(home.path()), "0.1.0").unwrap();
                other(home.path())
            } else {
                exe
            };
            let served = Served::new(home.path(), exe, BRIEF).await;
            let before = snapshot(home.path());

            let body = format!(r#"{{"tag":"{tag}"}}"#);
            let (status, asked, text) = served.post(served.me, &body, false).await;

            assert_eq!(status, 409, "{reason}: {text}");
            assert!(!asked, "{reason}");
            let answer: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert!(
                answer["error"].as_str().unwrap().contains(reason),
                "{reason}: {text}"
            );
            let field = answer.get("ready").expect("a ready field, null or a tag");
            assert_eq!(field.as_str(), ready, "{reason}: {text}");
            assert_eq!(snapshot(home.path()), before, "{reason}");
            tokio::time::sleep(BRIEF * 2).await;
            assert_eq!(served.stops(), 0, "{reason}");
        }
    }

    #[tokio::test]
    async fn an_install_that_fails_is_500_with_the_error_and_stops_nothing() {
        let (home, exe) = ready_home(READY);
        // A directory with something in it cannot be removed as the old link, so `current`
        // cannot be pointed.
        fs::create_dir_all(home.path().join("current.new")).unwrap();
        fs::write(home.path().join("current.new/in-the-way"), "x").unwrap();
        let served = Served::new(home.path(), exe, BRIEF).await;
        let before = snapshot(home.path());

        let (status, asked, text) = served.post(served.me, r#"{"tag":"v0.2.0"}"#, false).await;

        assert_eq!(status, 500, "{text}");
        assert!(!asked);
        let answer: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(
            answer["error"].as_str().unwrap().contains("current"),
            "{text}"
        );
        assert_eq!(snapshot(home.path()), before);
        tokio::time::sleep(BRIEF * 2).await;
        assert_eq!(served.stops(), 0);
    }

    #[tokio::test]
    async fn an_install_is_202_with_no_body_and_stops_once_after_the_answer() {
        let (home, exe) = ready_home(READY);
        let served = Served::new(home.path(), exe, LONG).await;

        let (status, asked, text) = served.post(served.me, r#"{"tag":"v0.2.0"}"#, false).await;

        assert_eq!((status, asked, text.as_str()), (202, false, ""));
        assert_eq!(
            fs::read_link(home.path().join("current")).unwrap(),
            Path::new("versions/0.2.0")
        );
        let state: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(home.path().join("update.json")).unwrap())
                .unwrap();
        assert_eq!(state["trial"]["previous"], "0.1.0", "{state}");
        assert!(state["ready"].is_null(), "{state}");
        assert!(
            served.stopped(1, Duration::from_secs(5)).await,
            "it did not stop once the answer was written"
        );
        tokio::time::sleep(BRIEF).await;
        assert_eq!(served.stops(), 1, "it stopped twice");
    }

    #[tokio::test]
    async fn a_carried_install_leaves_its_stop_to_the_link_and_stops_by_itself_within_the_limit() {
        let (home, exe) = ready_home(READY);
        let served = Served::new(home.path(), exe, BRIEF).await;

        let (status, asked, text) = served.post(served.me, r#"{"tag":"v0.2.0"}"#, true).await;

        assert_eq!((status, asked, text.as_str()), (202, true, ""));
        assert_eq!(
            served.stops(),
            0,
            "it stopped before the link sent the answer"
        );
        assert!(
            served.stopped(1, Duration::from_secs(5)).await,
            "it did not stop by itself"
        );
        tokio::time::sleep(BRIEF * 2).await;
        assert_eq!(served.stops(), 1, "it stopped twice");
    }

    #[tokio::test]
    async fn an_install_naming_another_machine_is_not_carried_on_and_an_id_that_is_none_is_404() {
        let (home, exe) = ready_home(READY);
        let served = Served::new(home.path(), exe, BRIEF).await;
        let before = snapshot(home.path());
        let other = MachineId::random();

        for carried in [false, true] {
            let (status, _, text) = served.post(other, r#"{"tag":"v0.2.0"}"#, carried).await;
            assert_eq!(status, 421, "{text}");
            assert!(text.contains(&other.to_string()), "{text}");
        }
        let (status, _, text) = served.post("not-an-id", r#"{"tag":"v0.2.0"}"#, false).await;
        assert_eq!(status, 404, "{text}");

        assert_eq!(snapshot(home.path()), before);
        tokio::time::sleep(BRIEF * 2).await;
        assert_eq!(served.stops(), 0);
    }
}
