//! `foundryd run`: takes the data directory's lock, opens the store, brings the terminals
//! back, starts every long-running module and stops on SIGTERM or SIGINT. The holders are
//! sessions of their own, so a stopped daemon leaves every terminal's child running.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::IntoResponse;
use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};
use tokio::signal::unix::{SignalKind, signal};
use tokio::task::JoinSet;
use tracing_subscriber::EnvFilter;

use crate::api::{self, Api, BindError};
use crate::join::{self, JoinError, KeyedDialer, Keys};
use crate::machine;
use crate::secrets::{self, GATEWAY, SecretsError};
use crate::sessions::{self, Gateway, Sessions};
use crate::store::{Store, StoreError};
use crate::sync::Peers;
use crate::sync::ws::{self, WsDialer};
use crate::terminals::{TerminalError, Terminals};
use crate::workspaces::Workspaces;

/// How long the runtime waits for tasks that will not stop once the daemon has decided to
/// exit, so one stuck task cannot hang the exit.
const EXIT_WITHIN: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
enum DaemonError {
    #[error("set FOUNDRY_HOME or HOME to say where the data directory is")]
    NoHome,
    #[error("HOME is not set, so Claude Code's state file cannot be found")]
    NoClaudeState,
    #[error("{0}")]
    Port(String),
    #[error("{what}: {source}")]
    Io { what: String, source: io::Error },
    #[error("another daemon holds the lock {}", .0.display())]
    Locked(PathBuf),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Keys(#[from] JoinError),
    #[error(transparent)]
    Terminals(#[from] TerminalError),
    #[error(transparent)]
    Bind(#[from] BindError),
    #[error("a step run apart did not finish: {0}")]
    Task(#[from] tokio::task::JoinError),
    #[error("{0} stopped on its own")]
    Ended(&'static str),
}

/// Labels an I/O error with what was being done.
fn io_error(what: impl Into<String>) -> impl FnOnce(io::Error) -> DaemonError {
    let what = what.into();
    move |source| DaemonError::Io { what, source }
}

/// The data directory: `FOUNDRY_HOME`, else `~/.local/share/foundry`.
pub fn home() -> Option<PathBuf> {
    home_from(std::env::var_os("FOUNDRY_HOME"), std::env::var_os("HOME"))
}

fn home_from(foundry_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    match foundry_home {
        Some(home) => Some(home.into()),
        None => home.map(|h| PathBuf::from(h).join(".local/share/foundry")),
    }
}

/// The port the daemon listens on: `FOUNDRY_PORT`, else [`join::DEFAULT_PORT`]. The error
/// says what is wrong with the variable.
pub fn port() -> Result<u16, String> {
    port_from(std::env::var_os("FOUNDRY_PORT"))
}

fn port_from(value: Option<OsString>) -> Result<u16, String> {
    let Some(value) = value else {
        return Ok(join::DEFAULT_PORT);
    };
    value
        .to_str()
        .and_then(|text| text.parse().ok())
        .ok_or_else(|| {
            format!(
                "FOUNDRY_PORT is not a port number: {}",
                value.to_string_lossy()
            )
        })
}

/// `foundryd run`.
pub fn run() -> ExitCode {
    match start() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("foundryd run: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Runs until a signal stops the daemon (`Ok`) or startup, or a service, fails.
fn start() -> Result<(), DaemonError> {
    let home = home().ok_or(DaemonError::NoHome)?;
    let port = port().map_err(DaemonError::Port)?;
    std::fs::create_dir_all(&home).map_err(io_error(format!("cannot make {}", home.display())))?;
    // Held until this function returns.
    let _lock = lock(&home)?;
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(io::stderr)
        .try_init();
    let runtime = tokio::runtime::Runtime::new().map_err(io_error("cannot start the runtime"))?;
    let result = runtime.block_on(serve(home, port));
    runtime.shutdown_timeout(EXIT_WITHIN);
    result
}

/// Takes an exclusive lock on `<home>/daemon.lock`, so two daemons never share a data
/// directory. The file is not inherited by the holders, which outlive the daemon.
fn lock(home: &Path) -> Result<Flock<File>, DaemonError> {
    let path = home.join("daemon.lock");
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .map_err(io_error(format!("cannot open the lock {}", path.display())))?;
    Flock::lock(file, FlockArg::LockExclusiveNonblock).map_err(|(_, errno)| {
        if errno == Errno::EWOULDBLOCK {
            DaemonError::Locked(path.clone())
        } else {
            DaemonError::Io {
                what: format!("cannot lock {}", path.display()),
                source: errno.into(),
            }
        }
    })
}

/// Builds every module, starts them and waits for a signal or for one to end.
async fn serve(home: PathBuf, port: u16) -> Result<(), DaemonError> {
    let mut terminate =
        signal(SignalKind::terminate()).map_err(io_error("cannot watch SIGTERM"))?;
    let mut interrupt = signal(SignalKind::interrupt()).map_err(io_error("cannot watch SIGINT"))?;

    let store = Arc::new(Mutex::new(Store::open(home.join("store.sqlite"))?));
    let secrets = secrets::for_this_machine(&home);
    // Both read the secret store, which can block on a keychain prompt.
    let keys = {
        let secrets = secrets.clone();
        tokio::task::spawn_blocking(move || Keys::open(secrets)).await??
    };
    let key = {
        let secrets = secrets.clone();
        tokio::task::spawn_blocking(move || secrets.get(GATEWAY)).await?
    };
    let me = keys.me();

    let holder = std::env::current_exe()
        .map_err(io_error("cannot find this program"))?
        .with_file_name("foundry-holder");
    let terminals = Arc::new(Terminals::new(me, store.clone(), &home, holder));
    terminals.reattach_all().await?;
    let workspaces = Arc::new(Workspaces::new(me, store.clone(), &home, terminals.clone()));

    let (gateway, missing) = match gateway(std::env::var("FOUNDRY_GATEWAY_URL").ok(), key) {
        Ok(gateway) => (gateway, None),
        Err(missing) => {
            tracing::warn!("{missing}");
            let placeholder = Gateway {
                base_url: String::new(),
                key: String::new(),
            };
            (placeholder, Some(missing))
        }
    };
    let claude_json = sessions::claude::claude_json(
        std::env::var_os("CLAUDE_CONFIG_DIR"),
        std::env::var_os("HOME"),
    )
    .ok_or(DaemonError::NoClaudeState)?;
    let sessions = Arc::new(Sessions::new(
        me,
        store.clone(),
        terminals.clone(),
        &home,
        sessions::Config {
            claude: vec!["claude".into()],
            claude_json,
            hook_port: port,
            gateway,
        },
    ));

    let dialer = KeyedDialer::new(Arc::new(WsDialer { port }), keys.clone());
    let peers = Peers::new(me, store.clone(), Arc::new(dialer));
    let api = Api::new(
        store.clone(),
        peers.clone(),
        terminals,
        workspaces,
        sessions.clone(),
        host_names(),
    );
    let mut app = api::router(api)
        .merge(sessions.router())
        .merge(ws::keyed_router(peers.clone(), keys));
    if let Some(missing) = missing {
        app = refuse_sessions(app, missing);
    }

    let listeners = listen(port).await?;
    let mut services: JoinSet<&'static str> = JoinSet::new();
    // services
    services.spawn(named(
        "machine sampling",
        machine::run(store.clone(), me, home.clone()),
    ));
    services.spawn(named("peer sync", async move { peers.run().await }));
    for listener in listeners {
        services.spawn(named("the API server", serve_on(listener, app.clone())));
    }

    tokio::select! {
        _ = terminate.recv() => tracing::info!("SIGTERM: stopping; terminals keep running"),
        _ = interrupt.recv() => tracing::info!("SIGINT: stopping; terminals keep running"),
        ended = services.join_next() => {
            return match ended {
                Some(Ok(name)) => {
                    tracing::error!("{name} stopped on its own");
                    Err(DaemonError::Ended(name))
                }
                Some(Err(error)) => Err(error.into()),
                None => Err(DaemonError::Ended("every service")),
            };
        }
    }
    Ok(())
}

/// Runs `service` and names it once it ends, so the daemon can say which one stopped.
async fn named(name: &'static str, service: impl Future<Output = ()>) -> &'static str {
    service.await;
    name
}

/// What `FOUNDRY_GATEWAY_URL` and the `gateway` secret make: the gateway sessions use, or why
/// there is none. The reason names what is missing and never holds the key.
fn gateway(
    url: Option<String>,
    key: Result<Option<String>, SecretsError>,
) -> Result<Gateway, String> {
    let url = url.filter(|url| !url.is_empty());
    let (key, key_problem) = match key {
        Ok(key) => match key.filter(|key| !key.is_empty()) {
            Some(key) => (Some(key), None),
            None => (
                None,
                Some(format!(
                    "the secret {GATEWAY} is not set (run `foundryd secret set {GATEWAY}`)"
                )),
            ),
        },
        Err(error) => (
            None,
            Some(format!("the secret {GATEWAY} cannot be read: {error}")),
        ),
    };
    match (url, key) {
        (Some(base_url), Some(key)) => Ok(Gateway { base_url, key }),
        (url, _) => {
            let mut missing = Vec::new();
            if url.is_none() {
                missing.push("FOUNDRY_GATEWAY_URL is not set".to_owned());
            }
            missing.extend(key_problem);
            Err(format!("sessions cannot start: {}", missing.join("; ")))
        }
    }
}

/// Answers a request to start a session with `missing` as a 503, and passes every other
/// request on. For a daemon with no gateway: shells and sync still work.
fn refuse_sessions(router: Router, missing: String) -> Router {
    router.layer(middleware::from_fn(move |request: Request, next: Next| {
        let missing = missing.clone();
        async move {
            if starts_session(request.method(), request.uri().path()) {
                let body = serde_json::json!({ "error": missing });
                return (StatusCode::SERVICE_UNAVAILABLE, axum::Json(body)).into_response();
            }
            next.run(request).await
        }
    }))
}

/// True for `POST /api/workspaces/{id}/sessions`.
fn starts_session(method: &Method, path: &str) -> bool {
    let segments: Vec<&str> = path.split('/').collect();
    method == Method::POST
        && matches!(
            segments.as_slice(),
            ["", "api", "workspaces", _, "sessions"]
        )
}

/// This machine's short host name, lowercased: the API trusts it in a `Host` header as its
/// MagicDNS name.
fn host_names() -> Vec<String> {
    sysinfo::System::host_name()
        .ok()
        .and_then(|name| name.split('.').next().map(str::to_ascii_lowercase))
        .filter(|name| !name.is_empty())
        .into_iter()
        .collect()
}

/// The addresses to listen on: loopback first, then each of `found` that is a Tailscale
/// address, once.
fn listen_on(port: u16, found: impl IntoIterator<Item = IpAddr>) -> Vec<SocketAddr> {
    let mut addrs = vec![SocketAddr::from(([127, 0, 0, 1], port))];
    for ip in found {
        let addr = SocketAddr::new(ip, port);
        if api::allowed(ip) && !ip.is_loopback() && !addrs.contains(&addr) {
            addrs.push(addr);
        }
    }
    addrs
}

/// Binds loopback and every Tailscale address on this machine's interfaces. Loopback failing
/// is an error; a Tailscale address failing is logged and left out.
async fn listen(port: u16) -> Result<Vec<tokio::net::TcpListener>, DaemonError> {
    let found: BTreeSet<IpAddr> = match sysinfo::Networks::new_with_refreshed_list() {
        Ok(networks) => networks
            .values()
            .flat_map(|network| network.ip_networks().iter().map(|ip| ip.addr))
            .collect(),
        Err(error) => {
            tracing::warn!("cannot list this machine's addresses, so loopback only: {error}");
            BTreeSet::new()
        }
    };
    let mut listeners = Vec::new();
    for (i, addr) in listen_on(port, found).into_iter().enumerate() {
        match api::bind(addr).await {
            Ok(listener) => listeners.push(listener),
            Err(error) if i == 0 => return Err(error.into()),
            Err(error) => tracing::warn!("{error}"),
        }
    }
    Ok(listeners)
}

/// Serves `app` on `listener` until the server fails.
async fn serve_on(listener: tokio::net::TcpListener, app: Router) {
    let served = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await;
    if let Err(error) = served {
        tracing::error!("the server failed: {error}");
    }
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

    #[test]
    fn the_data_directory_is_made_private() {
        use std::os::unix::fs::PermissionsExt as _;
        let parent = tempfile::tempdir().unwrap();
        let home = parent.path().join("a/b");

        make_home(&home).unwrap();

        let mode = std::fs::metadata(&home).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
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
            let missing = gateway(url, Ok(Some(KEY.into()))).unwrap_err().said;
            assert!(missing.contains("FOUNDRY_GATEWAY_URL"), "{missing}");
            assert!(!missing.contains(KEY), "{missing}");
        }
    }

    #[test]
    fn a_missing_key_names_the_secret_and_how_to_set_it() {
        let missing = gateway(Some("http://gw.test".into()), Ok(None))
            .unwrap_err()
            .said;
        assert!(missing.contains("foundryd secret set gateway"), "{missing}");
        assert!(!missing.contains("FOUNDRY_GATEWAY_URL"), "{missing}");
    }

    #[test]
    fn both_missing_names_both() {
        let missing = gateway(None, Ok(None)).unwrap_err().said;
        assert!(missing.contains("FOUNDRY_GATEWAY_URL"), "{missing}");
        assert!(missing.contains("foundryd secret set gateway"), "{missing}");
    }

    #[test]
    fn a_secrets_error_is_named_in_the_log_but_not_in_what_a_client_is_told() {
        let error = SecretsError::Unreadable {
            path: PathBuf::from("/data/secrets.json"),
        };
        let missing = gateway(Some("http://gw.test".into()), Err(error)).unwrap_err();

        assert!(missing.said.contains("gateway"), "{}", missing.said);
        assert!(missing.said.contains("daemon log"), "{}", missing.said);
        assert!(!missing.said.contains("secrets.json"), "{}", missing.said);
        assert!(!missing.said.contains("/data"), "{}", missing.said);
        assert!(missing.log.contains("gateway"), "{}", missing.log);
        assert!(
            missing.log.contains("/data/secrets.json"),
            "{}",
            missing.log
        );
    }

    #[test]
    fn the_said_text_says_what_is_missing_in_plain_words() {
        let missing = gateway(None, Ok(None)).unwrap_err();

        assert!(
            missing.said.contains("FOUNDRY_GATEWAY_URL is not set"),
            "{}",
            missing.said
        );
        assert!(
            missing
                .said
                .contains("the secret gateway is not set (run `foundryd secret set gateway`)"),
            "{}",
            missing.said
        );
    }

    fn addr(text: &str) -> SocketAddr {
        text.parse().unwrap()
    }

    #[test]
    fn only_tailscale_addresses_not_yet_served_are_new() {
        let served: BTreeSet<SocketAddr> =
            [addr("127.0.0.1:7433"), addr("100.101.102.103:7433")].into();
        let found = [
            ip("100.101.102.103"),
            ip("100.64.0.9"),
            ip("192.168.1.20"),
            ip("fd7a:115c:a1e0::1"),
            ip("127.0.0.1"),
            ip("100.64.0.9"),
        ];

        assert_eq!(
            new_addresses(7433, found, &served),
            vec![addr("100.64.0.9:7433"), addr("[fd7a:115c:a1e0::1]:7433")]
        );
    }

    #[test]
    fn with_everything_served_nothing_is_new() {
        let served: BTreeSet<SocketAddr> = [addr("127.0.0.1:7433")].into();

        assert!(new_addresses(7433, [ip("10.0.0.5")], &served).is_empty());
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
