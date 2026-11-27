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
    fn snapshot(home: &Path) -> (PathBuf, String) {
        (
            fs::read_link(home.join("current")).unwrap(),
            fs::read_to_string(home.join("update.json")).unwrap(),
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

        for body in ["", "{}", r#"{"tag":3}"#, r#"{"tag":null}"#, "not json", "[]"] {
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
        let trial = r#"{"ready":"v0.2.0","trial":{"version":"0.1.0","previous":"0.0.9","starts":0}}"#;
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
            (trial, "v0.2.0", false, Some("v0.2.0"), "a trial of 0.1.0 is on"),
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
        assert!(answer["error"].as_str().unwrap().contains("current"), "{text}");
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
        assert_eq!(served.stops(), 0, "it stopped before the link sent the answer");
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
