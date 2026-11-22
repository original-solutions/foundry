//! `foundryd run`: takes the data directory's lock, opens the store, brings the terminals
//! back, starts every long-running module and stops on SIGTERM or SIGINT. The holders are
//! sessions of their own, so a stopped daemon leaves every terminal's child running.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::{DirBuilder, File, OpenOptions};
use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::middleware;
use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};
use tokio::signal::unix::{SignalKind, signal};
use tokio::task::{AbortHandle, JoinSet};
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

use crate::api::{self, Api, BindError};
use crate::attention::Attention;
use crate::gateway;
use crate::join::{self, JoinError, KeyedDialer, Keys};
use crate::machine;
use crate::placement;
use crate::secrets::{self, GATEWAY, SecretsError};
use crate::sessions::{self, Gateway, Sessions};
use crate::store::{Store, StoreError};
use crate::sync::Peers;
use crate::sync::ws::{self, WsDialer};
use crate::terminals::{TerminalError, Terminals};
use crate::update;
use crate::workspaces::{WorkspaceError, Workspaces};

/// How long the runtime waits for tasks that will not stop once the daemon has decided to
/// exit, so one stuck task cannot hang the exit.
const EXIT_WITHIN: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub(crate) enum DaemonError {
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
    #[error(
        "the data directory {} is open to other users (mode {mode:o}); run `chmod 700 {}`",
        .home.display(),
        .home.display()
    )]
    HomeOpen { home: PathBuf, mode: u32 },
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Secrets(#[from] SecretsError),
    #[error(transparent)]
    Keys(#[from] JoinError),
    #[error(transparent)]
    Terminals(#[from] TerminalError),
    #[error(transparent)]
    Workspaces(#[from] WorkspaceError),
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
    let filter = log_filter(std::env::var("RUST_LOG").ok().as_deref());
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_writer(io::stderr))
        .try_init();
    if !update::starting(&home) {
        return Ok(());
    }
    let runtime = tokio::runtime::Runtime::new().map_err(io_error("cannot start the runtime"))?;
    let result = runtime.block_on(serve(home, port));
    runtime.shutdown_timeout(EXIT_WITHIN);
    result
}

/// Parses `RUST_LOG` into a target/level filter, defaulting to `info`.
fn log_filter(rust_log: Option<&str>) -> Targets {
    rust_log
        .and_then(|spec| spec.parse().ok())
        .unwrap_or_else(|| Targets::new().with_default(LevelFilter::INFO))
}

/// Makes the data directory, readable only by this user, as the terminals' directory is. A
/// directory that already exists is never changed: it may be shared, or the user's home. One
/// that other users can reach (`foundryd join` and `foundryd secret set` make it with the
/// default mode) is refused, naming the `chmod` that fixes it.
pub(crate) fn make_home(home: &Path) -> Result<(), DaemonError> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(home)
        .map_err(io_error(format!("cannot make {}", home.display())))?;
    let mode = std::fs::metadata(home)
        .map_err(io_error(format!("cannot read {}", home.display())))?
        .permissions()
        .mode()
        & 0o777;
    if mode & 0o077 != 0 {
        return Err(DaemonError::HomeOpen {
            home: home.to_owned(),
            mode,
        });
    }
    Ok(())
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
    let store = Arc::new(Mutex::new(Store::open(home.join("store.sqlite"))?));
    // SIGTERM and SIGINT are not watched until startup is done, so until then they stop the
    // daemon as usual. On a Mac a keychain-era release ran on, opening the store with no update on
    // trial first copies its secrets out of the login keychain, which can wait on a prompt for
    // each.
    tracing::info!("opening the secret store");
    let secrets = {
        let home = home.clone();
        tokio::task::spawn_blocking(move || secrets::for_this_machine(&home)).await??
    };
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
    workspaces.sweep().await?;

    let (gateway, missing) = match gateway(std::env::var("FOUNDRY_GATEWAY_URL").ok(), key) {
        Ok(gateway) => (gateway, None),
        Err(missing) => {
            tracing::warn!("{}", missing.log);
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
    sessions.adopt().await;

    let dialer = KeyedDialer::new(Arc::new(WsDialer { port }), keys.clone());
    let peers = Peers::new(me, store.clone(), Arc::new(dialer));
    peers.serve_terminals(terminals.clone());
    peers.serve_calls(loopback(port));
    // Peers hold the coordinator weakly; this binding keeps it for the daemon's life.
    let coordinator = Arc::new(crate::coordinator::Coordinator::new(
        me,
        store.clone(),
        peers.clone(),
    ));
    peers.coordinate(coordinator.clone());
    let mut api = Api::new(
        store.clone(),
        peers.clone(),
        terminals.clone(),
        workspaces.clone(),
        sessions.clone(),
        host_names(),
    );
    // Refused in the API, behind the layer that carries a start for another machine's
    // workspace there: shells, sync and that machine's sessions still work.
    api.sessions_refused = missing.map(|missing| missing.said);
    let gateway_reader = Arc::new(gateway::Reader::new(
        std::env::var("FOUNDRY_GATEWAY_URL").ok(),
        secrets.clone(),
    ));
    let attention = attention(
        me,
        &store,
        &peers,
        coordinator.clone(),
        gateway_reader.clone(),
        &api.placements,
    );
    let placing = api.clone();
    let attention_routes = attention
        .router()
        .layer(middleware::from_fn_with_state(api.clone(), api::same_site));
    let project_routes = api::projects::router(coordinator.clone(), store.clone())
        .layer(middleware::from_fn_with_state(api.clone(), api::same_site));
    let machine_name_routes = api::machine_names::router(coordinator.clone(), store.clone())
        .layer(middleware::from_fn_with_state(api.clone(), api::same_site));
    let join_routes = join::router(keys.clone())
        .layer(middleware::from_fn_with_state(api.clone(), api::same_site));
    let machine_add_routes =
        api::machine_add::router(keys.clone(), coordinator.clone(), store.clone())
            .layer(middleware::from_fn_with_state(api.clone(), api::same_site));
    let app = api::router(api)
        .merge(sessions.router())
        .merge(attention_routes)
        .merge(project_routes)
        .merge(machine_name_routes)
        .merge(join_routes)
        .merge(machine_add_routes)
        .merge(ws::keyed_router(peers.clone(), keys.clone()));

    let mut terminate =
        signal(SignalKind::terminate()).map_err(io_error("cannot watch SIGTERM"))?;
    let mut interrupt = signal(SignalKind::interrupt()).map_err(io_error("cannot watch SIGINT"))?;
    let local = loopback(port);
    let listener = api::bind(local).await?;
    tracing::info!("listening on {local}");
    let mut tailscale = Tailscale::new(port, app.clone());
    let listing = tailscale.listing(me, coordinator.clone(), store.clone());
    // Taken before the first scan, so the listing hears its answer.
    let answered = tailscale.answered.subscribe();
    tailscale.scan(true, tailscale_ips().await).await;
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
    services.spawn(named("the API server", serve_on(listener, app.clone())));
    services.spawn(named("the Tailscale address watch", tailscale.watch()));
    services.spawn(named(
        "the machine list entry",
        list_this_machine(answered, listing),
    ));
    services.spawn(named("updates", update::run(home.clone())));
    services.spawn(named("gateway health", gateway_reader.run()));
    services.spawn(named("placement", placement::run(placing.clone())));

    let stopped = tokio::select! {
        _ = terminate.recv() => {
            tracing::info!("SIGTERM: stopping; terminals keep running");
            Ok(())
        }
        _ = interrupt.recv() => {
            tracing::info!("SIGINT: stopping; terminals keep running");
            Ok(())
        }
        ended = services.join_next() => match ended {
            Some(Ok(name)) => {
                tracing::error!("{name} stopped on its own");
                Err(DaemonError::Ended(name))
            }
            Some(Err(error)) => Err(error.into()),
            None => Err(DaemonError::Ended("every service")),
        },
    };
    // The requests waiting for a machine live only in memory: a restart drops each, logged.
    placing.placements.drop_all();
    stopped
}

/// This machine's attention list: the fleet's items, the gateway's and the placement queue's.
fn attention(
    me: foundry_proto::ids::MachineId,
    store: &Arc<Mutex<Store>>,
    peers: &Peers,
    coordinator: Arc<crate::coordinator::Coordinator>,
    gateway: Arc<gateway::Reader>,
    placements: &Arc<placement::Queue>,
) -> Arc<Attention> {
    Arc::new(
        Attention::new(me, store.clone(), peers.clone(), coordinator)
            .with_source(gateway)
            .with_source(placements.clone()),
    )
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

/// The loopback address the daemon always listens on.
fn loopback(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

/// The addresses to listen on besides loopback, each once: every address Tailscale reports
/// for this machine that one of its interfaces (`found`) carries, in Tailscale's range. Neither
/// the range nor the interface's name is enough on its own: a VPN on macOS uses a `utun` too,
/// and some (Cloudflare WARP) hand out addresses in the same carrier-grade NAT range.
fn listen_on(
    port: u16,
    reported: impl IntoIterator<Item = IpAddr>,
    found: &BTreeSet<IpAddr>,
) -> Vec<SocketAddr> {
    let mut addrs = Vec::new();
    for ip in reported {
        let addr = SocketAddr::new(ip, port);
        if found.contains(&ip) && api::allowed(ip) && !ip.is_loopback() && !addrs.contains(&addr) {
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

/// The addresses to listen on that are not in `served` yet, by [`listen_on`]'s rule.
fn new_addresses(
    port: u16,
    reported: impl IntoIterator<Item = IpAddr>,
    found: &BTreeSet<IpAddr>,
    served: &BTreeSet<SocketAddr>,
) -> Vec<SocketAddr> {
    listen_on(port, reported, found)
        .into_iter()
        .filter(|addr| !served.contains(addr))
        .collect()
}

/// The `served` addresses whose IP Tailscale no longer reports: it logged out or moved to
/// another tailnet. An interface may still carry one, and another VPN can hand it out next.
fn withdrawn(
    reported: &BTreeSet<IpAddr>,
    served: impl IntoIterator<Item = SocketAddr>,
) -> Vec<SocketAddr> {
    served
        .into_iter()
        .filter(|addr| !reported.contains(&addr.ip()))
        .collect()
}

/// The Tailscale CLI, by [`choose_cli`]. In a debug build `FOUNDRY_TAILSCALE` names it, which
/// is how tests keep the daemon away from the real one. A release build ignores it, so what
/// the daemon listens on always comes from Tailscale itself.
fn tailscale_cli() -> PathBuf {
    cli_for(std::env::var_os("FOUNDRY_TAILSCALE"))
}

/// The CLI this build asks for a `FOUNDRY_TAILSCALE` value.
fn cli_for(set: Option<OsString>) -> PathBuf {
    choose_cli(set, TAILSCALE_OVERRIDE, KNOWN_CLIS)
}

/// Whether this build takes `FOUNDRY_TAILSCALE`: only a debug build.
const TAILSCALE_OVERRIDE: bool = cfg!(debug_assertions);

/// Where the Tailscale CLI is when it is not on a service's `PATH`: on macOS inside the app,
/// which does not put it on `PATH`, then Homebrew's, which launchd's `PATH` leaves out. On
/// Linux it is on `PATH`.
const KNOWN_CLIS: &[&str] = if cfg!(target_os = "macos") {
    &[
        "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
        "/opt/homebrew/bin/tailscale",
        "/usr/local/bin/tailscale",
    ]
} else {
    &[]
};

/// The CLI to ask: `set` when `taken` (the build takes the override) and it is not empty,
/// else the first of `known` that is there, else `tailscale` on `PATH`.
fn choose_cli(set: Option<OsString>, taken: bool, known: &[&str]) -> PathBuf {
    if let Some(cli) = set.filter(|cli| taken && !cli.is_empty()) {
        return PathBuf::from(cli);
    }
    known
        .iter()
        .map(Path::new)
        .find(|cli| cli.exists())
        .map_or_else(|| PathBuf::from("tailscale"), Path::to_path_buf)
}

/// How long `tailscale ip` may take before it is stopped and Tailscale counts as not answering.
const ASK_WITHIN: Duration = Duration::from_secs(3);

/// The addresses Tailscale reports for this machine, or why it cannot say.
async fn tailscale_ips() -> Result<BTreeSet<IpAddr>, String> {
    ask_cli(&tailscale_cli(), ASK_WITHIN).await
}

/// Runs `<cli> ip`, which prints one of this machine's Tailscale addresses a line, and reads
/// them. A CLI that fails, is missing, prints anything else or runs past `within` gives why.
async fn ask_cli(cli: &Path, within: Duration) -> Result<BTreeSet<IpAddr>, String> {
    String::from_utf8_lossy(&run_cli(cli, &["ip"], within).await?)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            line.parse()
                .map_err(|_| format!("`{} ip` printed {line:?}, not an address", cli.display()))
        })
        .collect()
}

/// Runs `<cli> status --self --json`, which says what Tailscale calls this machine and its
/// addresses, and gives what it printed, or why it cannot, as [`run_cli`] does.
async fn ask_status(cli: &Path, within: Duration) -> Result<Vec<u8>, String> {
    run_cli(cli, &["status", "--self", "--json"], within).await
}

/// Runs `<cli> <args>` and gives what it printed. A CLI that fails, is missing or runs past
/// `within` gives why.
async fn run_cli(cli: &Path, args: &[&str], within: Duration) -> Result<Vec<u8>, String> {
    let asked = format!("{} {}", cli.display(), args.join(" "));
    let run = tokio::process::Command::new(cli)
        .args(args)
        // The CLI inside Tailscale.app answers only when `TERM` is set; without it, as under
        // launchd, it tries to start the app's GUI and fails.
        .env("TERM", "dumb")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let out = match tokio::time::timeout(within, run).await {
        Ok(Ok(out)) => out,
        Ok(Err(error)) => return Err(format!("{} could not be run: {error}", cli.display())),
        Err(_) => {
            return Err(format!(
                "`{asked}` did not answer within {} s",
                within.as_secs_f32()
            ));
        }
    };
    if !out.status.success() {
        return Err(format!(
            "`{asked}` failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(out.stdout)
}

/// Keeps this machine in the shared machine list: each time the address watch hears from
/// Tailscale (`answered`, with the addresses `ip` gave), hands what Tailscale calls this machine
/// to `listing`, which also reads the watch's mark that Tailscale was away. It runs apart from
/// the watch, so an edit waiting on the coordinator never holds up binding an address.
///
/// `tailscale status` is asked only when [`Told`] has no answer for what `ip` said. Each look
/// still reads this machine's copy of the settings: that is what moves back an address another
/// machine's edit put back.
async fn list_this_machine(
    mut answered: tokio::sync::watch::Receiver<BTreeSet<IpAddr>>,
    mut listing: api::machine_names::Listing,
) {
    let mut told = Told::default();
    while answered.changed().await.is_ok() {
        let ips = answered.borrow_and_update().clone();
        let entry = told
            .entry(&ips, || async {
                ask_status(&tailscale_cli(), ASK_WITHIN)
                    .await
                    .and_then(|status| {
                        api::machine_names::entry(&status, host_names().first().map(String::as_str))
                    })
            })
            .await;
        listing.list(entry).await;
    }
}

/// What `tailscale status` last said this machine is, and the addresses `ip` gave just before,
/// so an unchanged `ip` answer is not followed by another `status` process every rescan.
#[derive(Default)]
struct Told {
    ips: BTreeSet<IpAddr>,
    entry: Option<api::machine_names::Entry>,
}

impl Told {
    /// The entry for `ips`, `ip`'s answer: the one kept for the same answer, else what `ask`
    /// gives. An entry is kept only when `ip` gave its address too, so a `status` read while the
    /// address moved, or one that failed, is asked again on the next answer.
    async fn entry<F>(
        &mut self,
        ips: &BTreeSet<IpAddr>,
        ask: impl FnOnce() -> F,
    ) -> Result<api::machine_names::Entry, String>
    where
        F: Future<Output = Result<api::machine_names::Entry, String>>,
    {
        if let Some(entry) = &self.entry
            && self.ips == *ips
        {
            return Ok(entry.clone());
        }
        let asked = ask().await;
        self.ips = ips.clone();
        self.entry = asked
            .as_ref()
            .ok()
            .filter(|entry| {
                entry
                    .address
                    .parse()
                    .is_ok_and(|address: IpAddr| ips.contains(&address))
            })
            .cloned();
        asked
    }
}

/// How often Tailscale is asked again for an address that has come up.
const RESCAN: Duration = Duration::from_secs(5);

/// Serves the app on every address Tailscale reports for this machine, now and as they
/// appear, and stops serving one it no longer reports: the daemon can start before Tailscale
/// does, and Tailscale can log out or move to another tailnet while it runs.
struct Tailscale {
    port: u16,
    app: Router,
    /// The Tailscale addresses being served, each with its server's task.
    served: BTreeMap<SocketAddr, AbortHandle>,
    /// The addresses that failed to bind, so each is warned about once.
    failed: BTreeSet<SocketAddr>,
    /// Whether the last ask of Tailscale failed, so a failure is warned about once until it
    /// answers again.
    unanswered: bool,
    servers: JoinSet<SocketAddr>,
    /// Given `ip`'s answer each time Tailscale answers, so the machine list entry is kept up
    /// to date.
    answered: tokio::sync::watch::Sender<BTreeSet<IpAddr>>,
    /// Set when an ask of Tailscale fails, until the machine list entry's [`Listing`] reads it.
    ///
    /// [`Listing`]: api::machine_names::Listing
    away: Arc<AtomicBool>,
}

impl Tailscale {
    fn new(port: u16, app: Router) -> Self {
        Self {
            port,
            app,
            served: BTreeMap::new(),
            failed: BTreeSet::new(),
            unanswered: false,
            servers: JoinSet::new(),
            answered: tokio::sync::watch::Sender::new(BTreeSet::new()),
            away: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The machine list entry this watch keeps up to date. It gives peers' copies
    /// [`OFFLINE_AFTER`](crate::sync::OFFLINE_AFTER) to arrive, and reads the mark this watch
    /// sets when Tailscale fails, so it waits again after Tailscale was away.
    fn listing(
        &self,
        me: foundry_proto::ids::MachineId,
        coordinator: Arc<crate::coordinator::Coordinator>,
        store: Arc<Mutex<Store>>,
    ) -> api::machine_names::Listing {
        api::machine_names::Listing::new(
            me,
            coordinator,
            store,
            crate::sync::OFFLINE_AFTER,
            self.away.clone(),
        )
    }

    /// Binds each address in Tailscale's answer, `reported`, not served yet, and stops the
    /// server on each served address the answer leaves out, so it takes no new connection; one
    /// already open is not cut. A server that ended is served again. When Tailscale cannot be
    /// asked, it counts as reporting nothing: every server is stopped and none is bound until
    /// it answers. `first` says this is the start, when having none is worth a warning.
    async fn scan(&mut self, first: bool, reported: Result<BTreeSet<IpAddr>, String>) {
        while let Some(ended) = self.servers.try_join_next() {
            if let Ok(addr) = ended {
                self.served.remove(&addr);
            }
        }
        let reported = match reported {
            Ok(reported) => {
                self.unanswered = false;
                self.answered.send_replace(reported.clone());
                reported
            }
            Err(why) => {
                if !self.unanswered {
                    tracing::warn!(
                        "cannot ask Tailscale for this machine's addresses, so none is listened on until it answers: {why}"
                    );
                }
                self.unanswered = true;
                self.away.store(true, Ordering::Relaxed);
                BTreeSet::new()
            }
        };
        for addr in withdrawn(&reported, self.served.keys().copied()) {
            if let Some(server) = self.served.remove(&addr) {
                server.abort();
                tracing::info!("stopped listening on {addr}: Tailscale no longer reports it");
            }
        }
        if self.unanswered {
            return;
        }
        let found = interface_addresses();
        let served = self.served.keys().copied().collect();
        for addr in new_addresses(self.port, reported, &found, &served) {
            match api::bind(addr).await {
                Ok(listener) => {
                    tracing::info!("listening on {addr}");
                    self.failed.remove(&addr);
                    let app = self.app.clone();
                    let server = self.servers.spawn(async move {
                        serve_on(listener, app).await;
                        addr
                    });
                    self.served.insert(addr, server);
                }
                Err(error) => {
                    if self.failed.insert(addr) {
                        tracing::warn!("{error}");
                    }
                }
            }
        }
        if first && self.served.is_empty() {
            tracing::warn!(
                "no Tailscale address is bound yet, so peers cannot reach this machine until one appears"
            );
        }
    }

    /// Asks again every [`RESCAN`], for ever.
    async fn watch(mut self) {
        loop {
            tokio::time::sleep(RESCAN).await;
            self.scan(false, tailscale_ips().await).await;
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

    fn ips<const N: usize>(texts: [&str; N]) -> BTreeSet<IpAddr> {
        texts.into_iter().map(ip).collect()
    }

    #[test]
    fn the_daemon_listens_on_tailscale_addresses_only_each_once() {
        let reported = [
            ip("100.101.102.103"),
            ip("fd7a:115c:a1e0::1"),
            ip("127.0.0.1"),
            ip("0.0.0.0"),
            ip("fe80::1"),
            ip("8.8.8.8"),
            ip("100.101.102.103"),
        ];
        let found = ips([
            "192.168.1.20",
            "100.101.102.103",
            "fd7a:115c:a1e0::1",
            "127.0.0.1",
            "0.0.0.0",
            "fe80::1",
            "8.8.8.8",
        ]);
        let listening: Vec<SocketAddr> = listen_on(7433, reported, &found);

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
        let found = ips(["10.0.0.5", "127.0.0.1", "100.64.0.1"]);
        assert!(listen_on(9000, [], &found).is_empty());
    }

    /// Cloudflare WARP hands out 100.96.x.x on a `utun`: in Tailscale's range, on the kind of
    /// interface Tailscale uses on macOS, yet every device on that VPN could reach the API.
    #[test]
    fn a_vpn_address_on_a_utun_that_tailscale_does_not_report_is_not_listened_on() {
        // utun5 has WARP's 100.96.0.5, utun4 has Tailscale's 100.101.102.103.
        let found = ips(["100.96.0.5", "100.101.102.103"]);
        let reported = [ip("100.101.102.103")];

        assert_eq!(
            listen_on(7433, reported, &found),
            vec![addr("100.101.102.103:7433")]
        );
    }

    /// A carrier-grade NAT or phone hotspot hands out addresses in Tailscale's range on an
    /// ordinary interface, where anyone on that network could reach the API.
    #[test]
    fn a_tailscale_range_address_tailscale_does_not_report_is_not_listened_on() {
        let found = ips(["100.72.10.5", "100.72.10.6", "fd7a:115c:a1e0::2"]);
        assert!(listen_on(7433, [ip("100.64.0.1")], &found).is_empty());
    }

    /// Tailscale can report an address before this machine's tunnel carries it.
    #[test]
    fn a_reported_address_no_interface_has_yet_is_not_listened_on() {
        assert!(listen_on(7433, [ip("100.64.0.1")], &BTreeSet::new()).is_empty());
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

    /// A directory that exists may be shared or the user's own home: it is never chmodded.
    #[test]
    fn a_wider_existing_data_directory_is_refused_and_left_as_it_is() {
        use std::os::unix::fs::PermissionsExt as _;
        let parent = tempfile::tempdir().unwrap();
        let home = parent.path().join("home");
        std::fs::create_dir(&home).unwrap();
        for wider in [0o755, 0o750, 0o701] {
            std::fs::set_permissions(&home, std::fs::Permissions::from_mode(wider)).unwrap();

            let error = make_home(&home).unwrap_err().to_string();

            assert!(error.contains(&home.display().to_string()), "{error}");
            assert!(error.contains("chmod 700"), "{error}");
            let mode = std::fs::metadata(&home).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, wider, "{wider:o}");
        }
    }

    #[test]
    fn an_existing_private_data_directory_is_used() {
        use std::os::unix::fs::PermissionsExt as _;
        let parent = tempfile::tempdir().unwrap();
        let home = parent.path().join("home");
        std::fs::create_dir(&home).unwrap();
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();

        make_home(&home).unwrap();
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
        let served: BTreeSet<SocketAddr> = [addr("100.101.102.103:7433")].into();
        let reported = [
            ip("100.101.102.103"),
            ip("100.64.0.9"),
            ip("fd7a:115c:a1e0::1"),
            ip("100.64.0.9"),
        ];
        let found = ips([
            "100.101.102.103",
            "100.64.0.9",
            "192.168.1.20",
            "fd7a:115c:a1e0::1",
            "127.0.0.1",
        ]);

        assert_eq!(
            new_addresses(7433, reported, &found, &served),
            vec![addr("100.64.0.9:7433"), addr("[fd7a:115c:a1e0::1]:7433")]
        );
    }

    #[test]
    fn with_everything_served_nothing_is_new() {
        let served: BTreeSet<SocketAddr> = [addr("100.64.0.9:7433")].into();
        let found = ips(["10.0.0.5", "100.64.0.9"]);

        assert!(new_addresses(7433, [ip("100.64.0.9")], &found, &served).is_empty());
    }

    /// The daemon can start before Tailscale: an address it reports later is new then, once.
    #[test]
    fn an_address_tailscale_reports_after_the_start_is_new_once() {
        let found = ips(["100.64.0.9"]);
        let mut served = BTreeSet::new();
        assert!(new_addresses(7433, [], &found, &served).is_empty());

        let later = new_addresses(7433, [ip("100.64.0.9")], &found, &served);
        assert_eq!(later, vec![addr("100.64.0.9:7433")]);

        served.extend(later);
        assert!(new_addresses(7433, [ip("100.64.0.9")], &found, &served).is_empty());
    }

    /// Tailscale logged out or moved to another tailnet: an address it no longer reports is
    /// withdrawn, though an interface may still carry it, and one it still reports stays.
    #[test]
    fn a_served_address_tailscale_no_longer_reports_is_withdrawn() {
        let served = [addr("100.101.102.103:7433"), addr("100.64.0.9:7433")];

        assert_eq!(
            withdrawn(&ips(["100.64.0.9"]), served),
            vec![addr("100.101.102.103:7433")]
        );
        assert!(withdrawn(&ips(["100.101.102.103", "100.64.0.9"]), served).is_empty());
    }

    /// A watcher serving `addr` with a server that runs until it is stopped.
    fn serving(addr: SocketAddr) -> Tailscale {
        let mut tailscale = Tailscale::new(7433, Router::new());
        let server = tailscale.servers.spawn(async move {
            std::future::pending::<()>().await;
            addr
        });
        tailscale.served.insert(addr, server);
        tailscale
    }

    /// Waits for the watcher's one server to end, and says whether it was stopped.
    async fn stopped(mut tailscale: Tailscale) -> bool {
        let ended = tokio::time::timeout(Duration::from_secs(5), tailscale.servers.join_next())
            .await
            .expect("the server was not stopped")
            .unwrap();
        ended.is_err_and(|error| error.is_cancelled())
    }

    /// A server for an address Tailscale stops reporting is stopped, so nothing listens there
    /// if the address turns up again on another VPN.
    #[tokio::test]
    async fn the_server_on_an_address_tailscale_no_longer_reports_is_stopped() {
        let gone = addr("100.101.102.103:7433");
        let mut tailscale = serving(gone);

        tailscale.scan(false, Ok(ips(["100.64.0.9"]))).await;

        assert!(!tailscale.served.contains_key(&gone));
        assert!(stopped(tailscale).await);
    }

    /// When Tailscale cannot be asked, it counts as reporting nothing: every server is stopped
    /// until it answers again, and the failure is still warned about once.
    #[tokio::test]
    async fn when_tailscale_cannot_be_asked_every_server_is_stopped() {
        let logged = Logged::default();
        let _capture = logged.capture();
        let mut tailscale = serving(addr("100.101.102.103:7433"));

        tailscale.scan(false, Err("logged out".to_owned())).await;
        tailscale.scan(false, Err("logged out".to_owned())).await;

        assert!(tailscale.served.is_empty());
        assert_eq!(logged.warnings().len(), 1, "{:?}", logged.warnings());
        assert!(stopped(tailscale).await);
    }

    /// Everything the daemon logs while it is set, as plain text.
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

    /// Not installed, not running or logged out: nothing besides loopback is bound, and the
    /// reason is logged once, not on every rescan.
    #[tokio::test]
    async fn when_tailscale_cannot_be_asked_nothing_is_bound_and_one_warning_says_why() {
        let logged = Logged::default();
        let _capture = logged.capture();
        let mut tailscale = Tailscale::new(7433, Router::new());
        let cannot = || Err::<BTreeSet<IpAddr>, _>("Tailscale is stopped.".to_owned());

        tailscale.scan(true, cannot()).await;
        tailscale.scan(false, cannot()).await;
        tailscale.scan(false, cannot()).await;

        assert!(tailscale.served.is_empty());
        assert!(tailscale.servers.is_empty());
        let warnings = logged.warnings();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("Tailscale is stopped."),
            "{warnings:?}"
        );
    }

    /// The listing restarts its wait after Tailscale was away: the watch says so until it is
    /// read, however many answers come before.
    #[tokio::test]
    async fn an_ask_that_fails_marks_tailscale_away_until_the_mark_is_read() {
        // Captured as the warning tests capture it: a test that hits this warning with no
        // subscriber can register its call site as off while another test is counting it.
        let _capture = Logged::default().capture();
        let mut tailscale = Tailscale::new(7433, Router::new());
        assert!(!tailscale.away.load(Ordering::Relaxed));

        tailscale.scan(false, Err("logged out".to_owned())).await;
        tailscale.scan(false, Ok(BTreeSet::new())).await;

        assert!(tailscale.away.swap(false, Ordering::Relaxed));
        tailscale.scan(false, Ok(BTreeSet::new())).await;
        assert!(!tailscale.away.load(Ordering::Relaxed));
    }

    /// The listing the watch hands out reads the watch's own away mark: an edit found before
    /// an ask fails waits its whole settle time again once Tailscale answers.
    #[tokio::test(start_paused = true)]
    async fn the_listing_waits_again_after_its_watch_found_tailscale_away() {
        use api::machine_names::Entry;

        // Captured as the warning tests capture it: a test that hits this warning with no
        // subscriber can register its call site as off while another test is counting it.
        let _capture = Logged::default().capture();
        let data = tempfile::tempdir().unwrap();
        let me = foundry_proto::ids::MachineId::random();
        let store = Arc::new(Mutex::new(
            Store::open(data.path().join("foundry.db")).unwrap(),
        ));
        let peers = Peers::new(me, store.clone(), Arc::new(WsDialer { port: 9 }));
        let coordinator = Arc::new(crate::coordinator::Coordinator::new(
            me,
            store.clone(),
            peers.clone(),
        ));
        peers.coordinate(coordinator.clone());
        let mut tailscale = Tailscale::new(7433, Router::new());
        let mut listing = tailscale.listing(me, coordinator, store.clone());
        let here = || {
            Ok(Entry {
                name: "personal-mac".into(),
                address: "100.91.54.18".into(),
            })
        };
        let listed = || {
            store
                .lock()
                .unwrap()
                .settings()
                .unwrap()
                .machines
                .iter()
                .any(|m| m.id == me)
        };
        listing.list(here()).await;

        tokio::time::sleep(crate::sync::OFFLINE_AFTER).await;
        tailscale.scan(false, Err("logged out".to_owned())).await;
        listing.list(here()).await;
        assert!(!listed(), "the look after Tailscale was away sends nothing");

        tokio::time::sleep(crate::sync::OFFLINE_AFTER).await;
        listing.list(here()).await;
        assert!(listed());
    }

    /// Each answer, the same as the last or not, is handed to the listing with its addresses;
    /// an ask that fails is not.
    #[tokio::test]
    async fn each_answer_is_handed_on_with_its_addresses_and_a_failure_is_not() {
        let _capture = Logged::default().capture();
        let mut tailscale = Tailscale::new(7433, Router::new());
        let mut answered = tailscale.answered.subscribe();
        // Addresses no interface here carries, so nothing is bound.
        let ips = BTreeSet::from([ip("100.127.254.253")]);

        for _ in 0..2 {
            tailscale.scan(false, Ok(ips.clone())).await;
            assert!(answered.has_changed().unwrap());
            assert_eq!(*answered.borrow_and_update(), ips);
        }
        tailscale.scan(false, Err("logged out".to_owned())).await;
        assert!(!answered.has_changed().unwrap());
    }

    /// `tailscale status` is asked again only when `ip`'s answer changed, the last ask gave no
    /// entry, or the entry's address is not among the addresses `ip` gave.
    #[tokio::test]
    async fn tailscale_status_is_asked_again_only_when_what_ip_said_changed() {
        use api::machine_names::Entry;
        use std::sync::atomic::AtomicUsize;

        let asks = AtomicUsize::new(0);
        let at = |address: &str| {
            let address = address.to_owned();
            let asks = &asks;
            move || {
                asks.fetch_add(1, Ordering::Relaxed);
                async move {
                    Ok(Entry {
                        name: "personal-mac".into(),
                        address,
                    })
                }
            }
        };
        let failing = || {
            asks.fetch_add(1, Ordering::Relaxed);
            async { Err("status failed".to_owned()) }
        };
        let ips = |text: &str| BTreeSet::from([ip(text)]);
        let mut told = Told::default();

        let first = told.entry(&ips("100.91.54.18"), at("100.91.54.18")).await;
        let again = told.entry(&ips("100.91.54.18"), at("100.91.54.18")).await;
        assert_eq!(again, first);
        assert_eq!(asks.load(Ordering::Relaxed), 1, "the same answer asks once");

        let moved = told.entry(&ips("100.91.54.19"), at("100.91.54.19")).await;
        assert_eq!(moved.map(|e| e.address), Ok("100.91.54.19".to_owned()));
        assert_eq!(asks.load(Ordering::Relaxed), 2, "a new answer asks again");

        assert!(told.entry(&ips("100.91.54.20"), failing).await.is_err());
        told.entry(&ips("100.91.54.20"), at("100.91.54.20"))
            .await
            .unwrap();
        assert_eq!(
            asks.load(Ordering::Relaxed),
            4,
            "a failed ask is asked again"
        );

        // `status` read before the move: its address is not one `ip` gave, so it is not kept.
        told.entry(&ips("100.91.54.21"), at("100.91.54.20"))
            .await
            .unwrap();
        let read = told.entry(&ips("100.91.54.21"), at("100.91.54.21")).await;
        assert_eq!(read.map(|e| e.address), Ok("100.91.54.21".to_owned()));
        assert_eq!(
            asks.load(Ordering::Relaxed),
            6,
            "a status ip disagrees with is asked again"
        );
    }

    /// After Tailscale answers again, a new failure is worth a new warning.
    #[tokio::test]
    async fn a_failure_after_tailscale_answered_again_is_warned_again() {
        let logged = Logged::default();
        let _capture = logged.capture();
        let mut tailscale = Tailscale::new(7433, Router::new());

        tailscale.scan(false, Err("not running".to_owned())).await;
        tailscale.scan(false, Ok(BTreeSet::new())).await;
        tailscale.scan(false, Err("logged out".to_owned())).await;

        let warnings = logged.warnings();
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings[1].contains("logged out"), "{warnings:?}");
    }

    #[test]
    fn foundry_tailscale_names_the_cli_when_set() {
        let dir = tempfile::tempdir().unwrap();
        let known = dir.path().join("known");
        std::fs::write(&known, "").unwrap();
        let known = known.to_str().unwrap();

        assert_eq!(
            choose_cli(os("/opt/fake/tailscale"), true, &[known]),
            PathBuf::from("/opt/fake/tailscale")
        );
    }

    /// A release build ignores `FOUNDRY_TAILSCALE`: whatever the variable names, the daemon asks
    /// the CLI where Tailscale puts it.
    #[test]
    fn foundry_tailscale_is_ignored_when_the_build_does_not_take_it() {
        let dir = tempfile::tempdir().unwrap();
        let known = dir.path().join("known");
        std::fs::write(&known, "").unwrap();

        assert_eq!(
            choose_cli(os("/opt/fake/tailscale"), false, &[known.to_str().unwrap()]),
            known
        );
    }

    /// The debug build the tests run takes `FOUNDRY_TAILSCALE`.
    #[cfg(debug_assertions)]
    #[test]
    fn this_debug_build_takes_foundry_tailscale() {
        assert_eq!(
            cli_for(os("/opt/fake/tailscale")),
            PathBuf::from("/opt/fake/tailscale")
        );
    }

    /// A release build does not. CI runs this one in a release build.
    #[cfg(not(debug_assertions))]
    #[test]
    fn this_release_build_ignores_foundry_tailscale() {
        assert_eq!(cli_for(os("/opt/fake/tailscale")), cli_for(None));
    }

    #[test]
    fn without_foundry_tailscale_the_first_known_cli_that_is_there_is_used() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        let there = dir.path().join("there");
        let also = dir.path().join("also");
        std::fs::write(&there, "").unwrap();
        std::fs::write(&also, "").unwrap();
        let known = [
            missing.to_str().unwrap(),
            there.to_str().unwrap(),
            also.to_str().unwrap(),
        ];

        assert_eq!(choose_cli(None, true, &known), there);
        assert_eq!(choose_cli(os(""), true, &known), there);
    }

    #[test]
    fn with_no_known_cli_there_tailscale_on_path_is_used() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");

        assert_eq!(
            choose_cli(None, true, &[missing.to_str().unwrap()]),
            PathBuf::from("tailscale")
        );
    }

    /// A service's `PATH` under launchd holds neither the app's CLI nor Homebrew's.
    #[test]
    fn on_macos_the_app_cli_comes_first_then_homebrew_s() {
        let expected: &[&str] = if cfg!(target_os = "macos") {
            &[
                "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
                "/opt/homebrew/bin/tailscale",
                "/usr/local/bin/tailscale",
            ]
        } else {
            &[]
        };
        assert_eq!(KNOWN_CLIS, expected);
    }

    /// Writes an executable stand-in for the Tailscale CLI that runs `body`.
    fn fake_cli(dir: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let cli = dir.join("tailscale");
        std::fs::write(&cli, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        cli
    }

    #[tokio::test]
    async fn the_cli_is_asked_for_ip_and_its_addresses_are_read() {
        let dir = tempfile::tempdir().unwrap();
        let cli = fake_cli(
            dir.path(),
            r#"[ "$1" = ip ] || exit 9; printf '100.101.102.103\nfd7a:115c:a1e0::1\n'"#,
        );

        let reported = ask_cli(&cli, Duration::from_secs(10)).await;

        assert_eq!(reported, Ok(ips(["100.101.102.103", "fd7a:115c:a1e0::1"])));
    }

    /// The CLI inside Tailscale.app starts the app's GUI instead of answering when `TERM` is not
    /// set, as under launchd. The test's own `TERM` is whatever the shell gave it, so the stand-in
    /// answers only to the value the daemon sets.
    #[tokio::test]
    async fn the_cli_is_given_a_term_so_the_app_cli_answers_under_launchd() {
        let dir = tempfile::tempdir().unwrap();
        let cli = fake_cli(
            dir.path(),
            r#"[ "$TERM" = dumb ] || { echo 'The Tailscale GUI failed to start' >&2; exit 1; }; echo 100.101.102.103"#,
        );

        let reported = ask_cli(&cli, Duration::from_secs(10)).await;

        assert_eq!(reported, Ok(ips(["100.101.102.103"])));
    }

    #[tokio::test]
    async fn a_cli_that_fails_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let cli = fake_cli(dir.path(), "echo 'Tailscale is stopped.' >&2; exit 1");

        let why = ask_cli(&cli, Duration::from_secs(10)).await.unwrap_err();

        assert!(why.contains("Tailscale is stopped."), "{why}");
    }

    #[tokio::test]
    async fn a_missing_cli_says_which_one_could_not_be_run() {
        let dir = tempfile::tempdir().unwrap();
        let cli = dir.path().join("tailscale");

        let why = ask_cli(&cli, Duration::from_secs(10)).await.unwrap_err();

        assert!(why.contains(&cli.display().to_string()), "{why}");
    }

    #[tokio::test]
    async fn output_that_is_not_addresses_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let cli = fake_cli(dir.path(), "echo 'not an address'");

        let why = ask_cli(&cli, Duration::from_secs(10)).await.unwrap_err();

        assert!(why.contains("not an address"), "{why}");
    }

    #[tokio::test]
    async fn a_cli_that_hangs_is_stopped_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        // A distinctive duration, so a stray is easy to find.
        let cli = fake_cli(dir.path(), "exec sleep 7.317");

        let why = ask_cli(&cli, Duration::from_millis(200)).await.unwrap_err();

        assert!(why.contains("did not answer"), "{why}");
    }

    #[test]
    fn log_filter_parses_rust_log_and_falls_back_to_info() {
        use tracing::Level;

        let default = log_filter(None);
        assert!(default.would_enable("foundryd", &Level::INFO));
        assert!(!default.would_enable("foundryd", &Level::DEBUG));

        let custom = log_filter(Some("warn,foundryd=debug"));
        assert!(custom.would_enable("foundryd::sync", &Level::DEBUG));
        assert!(!custom.would_enable("hyper", &Level::INFO));
        assert!(custom.would_enable("hyper", &Level::WARN));

        let invalid = log_filter(Some("not=a=valid=level"));
        assert!(invalid.would_enable("foundryd", &Level::INFO));
        assert!(!invalid.would_enable("foundryd", &Level::DEBUG));
    }

    #[tokio::test(start_paused = true)]
    async fn the_attention_list_holds_the_requests_waiting_for_a_machine() {
        use crate::attention::Kind;
        use crate::placement::{Queue, WAITING_ALERT_AFTER, Want};

        let data = tempfile::tempdir().unwrap();
        let me = foundry_proto::ids::MachineId::random();
        let store = Arc::new(Mutex::new(
            Store::open(data.path().join("foundry.db")).unwrap(),
        ));
        let peers = Peers::new(me, store.clone(), Arc::new(WsDialer { port: 9 }));
        let coordinator = Arc::new(crate::coordinator::Coordinator::new(
            me,
            store.clone(),
            peers.clone(),
        ));
        let gateway = Arc::new(gateway::Reader::new(
            None,
            Arc::new(crate::secrets::MemorySecrets::default()),
        ));
        let placements = Arc::new(Queue::new(me));
        let attention = attention(me, &store, &peers, coordinator, gateway, &placements);
        placements.wait(
            Want {
                project: foundry_proto::ids::ProjectId::random(),
                base: "main".into(),
                name: "fix login".into(),
                model: "opus".into(),
                prompt: None,
            },
            Vec::new(),
        );
        tokio::time::advance(WAITING_ALERT_AFTER).await;
        let items = attention.items().await.unwrap();
        assert!(
            items.iter().any(|item| item.kind == Kind::PlacementWaiting),
            "{items:?}"
        );
    }
}
