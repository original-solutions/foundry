//! `foundryd run`: takes the data directory's lock, opens the store, brings the terminals
//! back, starts every long-running module and stops on SIGTERM or SIGINT. The holders are
//! sessions of their own, so a stopped daemon leaves every terminal's child running.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs::{DirBuilder, File, OpenOptions};
use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
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
    make_home(&home)?;
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

/// Makes the data directory, readable only by this user, as the terminals' directory is.
fn make_home(home: &Path) -> Result<(), DaemonError> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(home)
        .map_err(io_error(format!("cannot make {}", home.display())))
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
            tracing::warn!("{}", missing.log);
            let placeholder = Gateway {
                base_url: String::new(),
                key: String::new(),
            };
            (placeholder, Some(missing.said))
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
        terminals.clone(),
        workspaces.clone(),
        sessions.clone(),
        host_names(),
    );
    let mut app = api::router(api)
        .merge(sessions.router())
        .merge(ws::keyed_router(peers.clone(), keys.clone()));
    if let Some(missing) = missing {
        app = refuse_sessions(app, missing);
    }

    let loopback = api::bind(SocketAddr::from(([127, 0, 0, 1], port))).await?;
    tracing::info!("listening on {}", SocketAddr::from(([127, 0, 0, 1], port)));
    let mut tailscale = Tailscale::new(port, app.clone());
    tailscale.scan(true).await;
    let mut services: JoinSet<&'static str> = JoinSet::new();
    // services
    services.spawn(named(
        "machine sampling",
        machine::run(store.clone(), me, home.clone()),
    ));
    services.spawn(named("peer sync", {
        let peers = peers.clone();
        async move { peers.run().await }
    }));
    services.spawn(named("the API server", serve_on(loopback, app.clone())));
    services.spawn(named("the Tailscale address watch", tailscale.watch()));

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

/// Why there is no gateway: what is missing, said two ways.
#[derive(Debug)]
struct Missing {
    /// Plain words for a client that asked to start a session. It names the secret but holds
    /// nothing from the secret store's error, which can name a path.
    said: String,
    /// The full reason, for the daemon's log.
    log: String,
}

/// What `FOUNDRY_GATEWAY_URL` and the `gateway` secret make: the gateway sessions use, or why
/// there is none. Neither text holds the key.
fn gateway(
    url: Option<String>,
    key: Result<Option<String>, SecretsError>,
) -> Result<Gateway, Missing> {
    let url = url.filter(|url| !url.is_empty());
    let (key, key_said, key_log) = match key {
        Ok(key) => match key.filter(|key| !key.is_empty()) {
            Some(key) => (Some(key), String::new(), String::new()),
            None => {
                let text = format!(
                    "the secret {GATEWAY} is not set (run `foundryd secret set {GATEWAY}`)"
                );
                (None, text.clone(), text)
            }
        },
        Err(error) => (
            None,
            format!("the secret {GATEWAY} could not be read; see the daemon log"),
            format!("the secret {GATEWAY} cannot be read: {error}"),
        ),
    };
    match (url, key) {
        (Some(base_url), Some(key)) => Ok(Gateway { base_url, key }),
        (url, key) => {
            let (mut said, mut log) = (Vec::new(), Vec::new());
            if url.is_none() {
                said.push("FOUNDRY_GATEWAY_URL is not set".to_owned());
                log.push("FOUNDRY_GATEWAY_URL is not set".to_owned());
            }
            if key.is_none() {
                said.push(key_said);
                log.push(key_log);
            }
            Err(Missing {
                said: format!("sessions cannot start: {}", said.join("; ")),
                log: format!("sessions cannot start: {}", log.join("; ")),
            })
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

/// The addresses of this machine's interfaces.
fn interface_addresses() -> BTreeSet<IpAddr> {
    match sysinfo::Networks::new_with_refreshed_list() {
        Ok(networks) => networks
            .values()
            .flat_map(|network| network.ip_networks().iter().map(|ip| ip.addr))
            .collect(),
        Err(error) => {
            tracing::warn!("cannot list this machine's addresses: {error}");
            BTreeSet::new()
        }
    }
}

/// The addresses in `found` to listen on that are not in `served` yet, by [`listen_on`]'s rule.
fn new_addresses(
    port: u16,
    found: impl IntoIterator<Item = IpAddr>,
    served: &BTreeSet<SocketAddr>,
) -> Vec<SocketAddr> {
    listen_on(port, found)
        .into_iter()
        .filter(|addr| !served.contains(addr))
        .collect()
}

/// How often the machine's interfaces are looked at for a Tailscale address that has come up.
const RESCAN: Duration = Duration::from_secs(5);

/// Serves the app on every Tailscale address this machine has, now and as they appear: the
/// daemon can start before Tailscale does.
struct Tailscale {
    port: u16,
    app: Router,
    /// The addresses being served.
    served: BTreeSet<SocketAddr>,
    /// The addresses that failed to bind, so each is warned about once.
    failed: BTreeSet<SocketAddr>,
    servers: JoinSet<SocketAddr>,
}

impl Tailscale {
    fn new(port: u16, app: Router) -> Self {
        Self {
            port,
            app,
            served: [SocketAddr::from(([127, 0, 0, 1], port))].into(),
            failed: BTreeSet::new(),
            servers: JoinSet::new(),
        }
    }

    /// Binds each Tailscale address not served yet. A server that ended is served again.
    /// `first` says this is the start, when having none is worth a warning.
    async fn scan(&mut self, first: bool) {
        while let Some(ended) = self.servers.try_join_next() {
            if let Ok(addr) = ended {
                self.served.remove(&addr);
            }
        }
        for addr in new_addresses(self.port, interface_addresses(), &self.served) {
            match api::bind(addr).await {
                Ok(listener) => {
                    tracing::info!("listening on {addr}");
                    self.served.insert(addr);
                    self.failed.remove(&addr);
                    let app = self.app.clone();
                    self.servers.spawn(async move {
                        serve_on(listener, app).await;
                        addr
                    });
                }
                Err(error) => {
                    if self.failed.insert(addr) {
                        tracing::warn!("{error}");
                    }
                }
            }
        }
        if first && self.served.len() == 1 {
            tracing::warn!(
                "no Tailscale address is bound yet, so peers cannot reach this machine until one appears"
            );
        }
    }

    /// Looks again every [`RESCAN`], for ever.
    async fn watch(mut self) {
        loop {
            tokio::time::sleep(RESCAN).await;
            self.scan(false).await;
        }
    }
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
    fn the_daemon_listens_on_tailscale_addresses_only_each_once() {
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
                "100.101.102.103:7433".parse().unwrap(),
                "[fd7a:115c:a1e0::1]:7433".parse().unwrap(),
            ]
        );
    }

    #[test]
    fn with_no_tailscale_there_is_nothing_besides_loopback_to_listen_on() {
        assert!(listen_on(9000, [ip("10.0.0.5"), ip("127.0.0.1")]).is_empty());
    }

    #[test]
    fn loopback_is_127_0_0_1_on_the_port() {
        assert_eq!(loopback(9000), addr("127.0.0.1:9000"));
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

    #[test]
    fn a_wider_data_directory_made_by_another_command_is_tightened() {
        use std::os::unix::fs::PermissionsExt as _;
        let parent = tempfile::tempdir().unwrap();
        let home = parent.path().join("home");
        std::fs::create_dir(&home).unwrap();
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o755)).unwrap();

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
            [addr("100.101.102.103:7433")].into();
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
        let served: BTreeSet<SocketAddr> = [addr("100.64.0.9:7433")].into();

        assert!(new_addresses(7433, [ip("10.0.0.5"), ip("100.64.0.9")], &served).is_empty());
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
        let missing = Missing {
            said: "set FOUNDRY_GATEWAY_URL".into(),
            log: "log-only detail /data/secrets.json".into(),
        };
        let port = serve_stub(refuse_sessions(stub(), missing)).await;
        let base = format!("http://127.0.0.1:{port}");

        let (status, body) = ask("POST", &format!("{base}/api/workspaces/x/sessions"));
        assert_eq!(status, 503);
        let body: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["error"], "set FOUNDRY_GATEWAY_URL");
        assert!(!body.to_string().contains("log-only"), "{body}");

        let shells = ask("POST", &format!("{base}/api/workspaces/x/shells"));
        assert_eq!(shells, (200, "stub shells".into()));
        let state = ask("GET", &format!("{base}/api/state"));
        assert_eq!(state, (200, "stub state".into()));
    }
}
