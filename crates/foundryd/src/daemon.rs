//! Starts every long-running module. Filled by plan task 20.

use std::process::ExitCode;

/// `foundryd run`.
pub fn run() -> ExitCode {
    crate::not_built("run")
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::net::{IpAddr, SocketAddr};
    use std::path::PathBuf;
    use std::time::Duration;

    use axum::Router;
    use axum::routing::{get, post};

    use super::*;
    use crate::secrets::SecretsError;

    fn os(text: &str) -> Option<OsString> {
        Some(text.into())
    }

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn foundry_home_wins_over_home() {
        assert_eq!(
            home_from(os("/data/foundry"), os("/home/tom")),
            Some(PathBuf::from("/data/foundry"))
        );
    }

    #[test]
    fn without_foundry_home_the_data_directory_is_under_home() {
        assert_eq!(
            home_from(None, os("/home/tom")),
            Some(PathBuf::from("/home/tom/.local/share/foundry"))
        );
    }

    #[test]
    fn with_neither_set_there_is_no_data_directory() {
        assert_eq!(home_from(None, None), None);
    }

    #[test]
    fn the_port_defaults_to_7433() {
        assert_eq!(port_from(None), Ok(7433));
    }

    #[test]
    fn the_port_is_foundry_port_when_set() {
        assert_eq!(port_from(os("7500")), Ok(7500));
    }

    #[test]
    fn a_port_that_is_not_a_number_names_foundry_port() {
        let error = port_from(os("not-a-port")).unwrap_err();
        assert!(error.contains("FOUNDRY_PORT"), "{error}");
        assert!(error.contains("not-a-port"), "{error}");
    }

    #[test]
    fn the_daemon_listens_on_loopback_first_then_tailscale_addresses() {
        let found = [
            ip("192.168.1.20"),
            ip("100.101.102.103"),
            ip("fd7a:115c:a1e0::1"),
            ip("127.0.0.1"),
            ip("0.0.0.0"),
            ip("fe80::1"),
            ip("8.8.8.8"),
            ip("100.101.102.103"),
        ];
        let listening: Vec<SocketAddr> = listen_on(7433, found);

        assert_eq!(
            listening,
            vec![
                "127.0.0.1:7433".parse().unwrap(),
                "100.101.102.103:7433".parse().unwrap(),
                "[fd7a:115c:a1e0::1]:7433".parse().unwrap(),
            ]
        );
    }

    #[test]
    fn with_no_tailscale_the_daemon_listens_on_loopback_only() {
        assert_eq!(
            listen_on(9000, [ip("10.0.0.5")]),
            vec!["127.0.0.1:9000".parse().unwrap()]
        );
    }

    const KEY: &str = "sk-distinct-0123456789";

    #[test]
    fn a_url_and_a_key_make_the_gateway() {
        let gateway = gateway(Some("http://gw.test".into()), Ok(Some(KEY.into()))).unwrap();
        assert_eq!(gateway.base_url, "http://gw.test");
        assert_eq!(gateway.key, KEY);
    }

    #[test]
    fn a_missing_url_names_foundry_gateway_url() {
        for url in [None, Some(String::new())] {
            let missing = gateway(url, Ok(Some(KEY.into()))).unwrap_err();
            assert!(missing.contains("FOUNDRY_GATEWAY_URL"), "{missing}");
            assert!(!missing.contains(KEY), "{missing}");
        }
    }

    #[test]
    fn a_missing_key_names_the_secret_and_how_to_set_it() {
        let missing = gateway(Some("http://gw.test".into()), Ok(None)).unwrap_err();
        assert!(missing.contains("foundryd secret set gateway"), "{missing}");
        assert!(!missing.contains("FOUNDRY_GATEWAY_URL"), "{missing}");
    }

    #[test]
    fn both_missing_names_both() {
        let missing = gateway(None, Ok(None)).unwrap_err();
        assert!(missing.contains("FOUNDRY_GATEWAY_URL"), "{missing}");
        assert!(missing.contains("foundryd secret set gateway"), "{missing}");
    }

    #[test]
    fn a_secrets_error_names_the_secret_and_the_error() {
        let error = SecretsError::Unreadable {
            path: PathBuf::from("/data/secrets.json"),
        };
        let missing = gateway(Some("http://gw.test".into()), Err(error)).unwrap_err();
        assert!(missing.contains("gateway"), "{missing}");
        assert!(missing.contains("/data/secrets.json"), "{missing}");
    }

    /// What the stub router answers, so a request that got through is told from a refusal.
    fn stub() -> Router {
        Router::new()
            .route(
                "/api/workspaces/{id}/sessions",
                post(|| async { "stub sessions" }),
            )
            .route(
                "/api/workspaces/{id}/shells",
                post(|| async { "stub shells" }),
            )
            .route("/api/state", get(|| async { "stub state" }))
    }

    /// Serves `router` on a loopback port and returns the port.
    async fn serve_stub(router: Router) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        port
    }

    /// `(status, body)` of `method` on `url`.
    fn ask(method: &str, url: &str) -> (u16, String) {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(10)))
            .build()
            .into();
        let mut response = match method {
            "POST" => agent.post(url).send_empty(),
            _ => agent.get(url).call(),
        }
        .unwrap();
        let body = response.body_mut().read_to_string().unwrap();
        (response.status().as_u16(), body)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_starts_are_refused_with_what_is_missing_and_nothing_else_is() {
        let port = serve_stub(refuse_sessions(stub(), "set FOUNDRY_GATEWAY_URL".into())).await;
        let base = format!("http://127.0.0.1:{port}");

        let (status, body) = ask("POST", &format!("{base}/api/workspaces/x/sessions"));
        assert_eq!(status, 503);
        let body: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["error"], "set FOUNDRY_GATEWAY_URL");

        let shells = ask("POST", &format!("{base}/api/workspaces/x/shells"));
        assert_eq!(shells, (200, "stub shells".into()));
        let state = ask("GET", &format!("{base}/api/state"));
        assert_eq!(state, (200, "stub state".into()));
    }
}
