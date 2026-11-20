//! Gateway health and account cooldown.
//!
//! Every [`READ_EVERY`] this machine reads the gateway's management API, CLIProxyAPI's
//! `GET <FOUNDRY_GATEWAY_URL>/v0/management/auth-files`, with the management key (the secret
//! [`GATEWAY_MANAGEMENT`]) as a bearer token. The gateway is unhealthy while it does not answer
//! with its accounts, or answers with none free: every account cooling down or disabled. It is
//! healthy again once it answers with an account free. Health is this machine's own read, so
//! the item it raises shows on this machine's list only.
//!
//! The key is read from the secret store once and kept. It is read again only while it is not
//! set, so a key set with `foundryd secret set` counts from the next read, and after the
//! gateway refuses it, when it is sent again once it has changed or after [`RETRY_REFUSED`]. A store that cannot read it (a denied keychain prompt, say) is not asked
//! again until the daemon restarts, so it never prompts every few seconds. Neither the key nor a
//! password in the URL reaches an item or a log line.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::time::MissedTickBehavior;

use crate::attention::{Fleet, Item, Kind, Source};
use crate::secrets::{GATEWAY_MANAGEMENT, Secrets, SecretsError, redact};

/// How often the gateway is read.
pub const READ_EVERY: Duration = Duration::from_secs(10);

/// How long a refused key waits before it is sent again unchanged. CLIProxyAPI bans an address
/// after five refused keys and forgets a refusal after two idle hours, so this never adds up to
/// a ban, and a gateway fixed to take the key is seen within it.
pub const RETRY_REFUSED: Duration = Duration::from_secs(3 * 60 * 60);

/// How long the gateway has to answer a read.
pub const ANSWER_WITHIN: Duration = Duration::from_secs(5);

/// What one read of the gateway found.
#[derive(Clone, Debug, PartialEq)]
pub enum Health {
    /// It answered with an account free.
    Healthy,
    /// It cannot be read: these are not set, named as the person sets them.
    NotSet(Vec<&'static str>),
    /// The management key could not be read from the secret store; the log says why.
    KeyUnreadable,
    /// It did not answer with its accounts, for this reason.
    Unanswered(String),
    /// It refused the management key: it answered 401.
    KeyRefused,
    /// It answered with no account that is not disabled.
    NoAccounts,
    /// Every account that is not disabled is cooling down. `free_in` is how long until the
    /// first is free again, when the gateway said.
    CoolingDown {
        accounts: usize,
        free_in: Option<Duration>,
    },
}

/// The part of CLIProxyAPI's auth-files answer this reads.
#[derive(Deserialize)]
struct AuthFiles {
    files: Vec<AuthFile>,
}

#[derive(Deserialize)]
struct AuthFile {
    #[serde(default)]
    disabled: bool,
    /// The account's unexpired cooldowns. `null` when the gateway does not know them.
    #[serde(default)]
    cooldowns: Option<Vec<Cooldown>>,
}

#[derive(Deserialize)]
struct Cooldown {
    #[serde(default)]
    remaining_seconds: Option<u64>,
}

/// Health from the gateway's answer `files`: healthy while one account that is not disabled
/// has no cooldown.
fn judged(files: &[AuthFile]) -> Health {
    let usable: Vec<&AuthFile> = files.iter().filter(|f| !f.disabled).collect();
    if usable.is_empty() {
        return Health::NoAccounts;
    }
    if usable
        .iter()
        .any(|f| f.cooldowns.as_ref().is_none_or(Vec::is_empty))
    {
        return Health::Healthy;
    }
    // An account is free once its last cooldown ends; the first to be free counts.
    let free_in = usable
        .iter()
        .filter_map(|f| {
            f.cooldowns
                .iter()
                .flatten()
                .map(|c| c.remaining_seconds)
                .collect::<Option<Vec<u64>>>()?
                .into_iter()
                .max()
        })
        .min()
        .map(Duration::from_secs);
    Health::CoolingDown {
        accounts: usable.len(),
        free_in,
    }
}

/// The most of the gateway's own reason an item shows.
const REASON_AT_MOST: usize = 200;

/// The part of an error answer from the gateway this reads.
#[derive(Deserialize)]
struct Refusal {
    error: String,
}

/// Reads the gateway at `url` with the management key `key`. Blocks for up to
/// [`ANSWER_WITHIN`].
pub fn read(url: &str, key: &str) -> Health {
    let endpoint = format!("{}/v0/management/auth-files", url.trim_end_matches('/'));
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(ANSWER_WITHIN))
        .http_status_as_error(false)
        .build()
        .into();
    let answer = agent
        .get(&endpoint)
        .header("Authorization", format!("Bearer {key}"))
        .call();
    let why = match answer {
        Ok(response) if response.status() == 401 => return Health::KeyRefused,
        Ok(mut response) if response.status().is_success() => {
            match response.body_mut().read_json::<AuthFiles>() {
                Ok(answer) => return judged(&answer.files),
                Err(_) => "its answer is not a list of accounts".to_owned(),
            }
        }
        // CLIProxyAPI says why in `error`: remote management disabled, an address banned
        // after failed keys, and so on.
        Ok(mut response) => {
            let status = response.status().as_u16();
            match response.body_mut().read_json::<Refusal>() {
                Ok(refusal) => {
                    // Hidden before the cut, so a secret across it cannot show in part.
                    let hidden = redact(&refusal.error).replace(key, "***");
                    let reason: String = hidden.chars().take(REASON_AT_MOST).collect();
                    format!("it answered {status}: {reason}")
                }
                Err(_) => format!("it answered {status}"),
            }
        }
        Err(error) => error.to_string(),
    };
    Health::Unanswered(redact(&why).replace(key, "***"))
}

/// Reads the gateway at `url`, when set, with the management key `key` as the secret store
/// gave it.
pub fn check(url: Option<&str>, key: Result<Option<&str>, &SecretsError>) -> Health {
    let url = url.filter(|url| !url.is_empty());
    let key = match key {
        Ok(key) => key.filter(|key| !key.is_empty()),
        Err(error) => {
            tracing::warn!("gateway: the secret {GATEWAY_MANAGEMENT} cannot be read: {error}");
            return Health::KeyUnreadable;
        }
    };
    match (url, key) {
        (Some(url), Some(key)) => read(url, key),
        (url, key) => {
            let mut missing = Vec::new();
            if url.is_none() {
                missing.push("FOUNDRY_GATEWAY_URL");
            }
            if key.is_none() {
                missing.push(GATEWAY_MANAGEMENT);
            }
            Health::NotSet(missing)
        }
    }
}

/// The management key as the reader holds it.
enum Key {
    /// Not read yet, or not set when last read: read it next time.
    Unread,
    Held(String),
    /// The gateway refused this key at `at`. It is read again each time and sent again once it
    /// has changed, or unchanged after [`RETRY_REFUSED`], since CLIProxyAPI bans an address
    /// after five refused keys.
    Refused {
        key: String,
        at: Instant,
    },
    /// The secret store could not read it; not asked again.
    Unreadable,
}

/// This machine's reader of the gateway, and the newest health it read.
pub struct Reader {
    url: Option<String>,
    secrets: Arc<dyn Secrets>,
    key: Mutex<Key>,
    /// [`RETRY_REFUSED`], shorter in tests.
    retry_refused: Duration,
    /// `None` until the first read ends.
    health: Mutex<Option<Health>>,
}

impl Reader {
    /// Reads the gateway at `url` (`FOUNDRY_GATEWAY_URL`) with the management key in `secrets`.
    pub fn new(url: Option<String>, secrets: Arc<dyn Secrets>) -> Self {
        Self {
            url,
            secrets,
            key: Mutex::new(Key::Unread),
            retry_refused: RETRY_REFUSED,
            health: Mutex::new(None),
        }
    }

    /// The newest health read, or `None` before the first read ends.
    pub fn health(&self) -> Option<Health> {
        self.health
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Reads the gateway once, on the blocking pool, and keeps what it found.
    pub async fn read_once(self: &Arc<Self>) {
        let reader = Arc::clone(self);
        let health = tokio::task::spawn_blocking(move || reader.read_blocking()).await;
        match health {
            Ok(health) => {
                *self.health.lock().unwrap_or_else(PoisonError::into_inner) = Some(health);
            }
            Err(error) => tracing::warn!("gateway: the read did not finish: {error}"),
        }
    }

    /// One read: the key as held, read from the store when it is not, then the gateway.
    fn read_blocking(&self) -> Health {
        let mut held = self.key.lock().unwrap_or_else(PoisonError::into_inner);
        let key = match &*held {
            Key::Held(key) => Ok(Some(key.clone())),
            Key::Unreadable => return Health::KeyUnreadable,
            Key::Unread | Key::Refused { .. } => self.secrets.get(GATEWAY_MANAGEMENT),
        };
        if let (Key::Refused { key: refused, at }, Ok(Some(key))) = (&*held, &key)
            && refused == key
            && at.elapsed() < self.retry_refused
        {
            return Health::KeyRefused;
        }
        let health = check(self.url.as_deref(), key.as_ref().map(Option::as_deref));
        *held = match (key, &health) {
            (_, Health::KeyUnreadable) => Key::Unreadable,
            (Ok(Some(key)), Health::KeyRefused) => Key::Refused {
                key,
                at: Instant::now(),
            },
            (Ok(Some(key)), _) if !key.is_empty() => Key::Held(key),
            _ => Key::Unread,
        };
        health
    }

    /// Reads the gateway every [`READ_EVERY`], the first at once. A read waits for the one
    /// before it, so a slow gateway or keychain never piles reads up.
    pub async fn run(self: Arc<Self>) {
        let mut every = tokio::time::interval(READ_EVERY);
        every.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            every.tick().await;
            self.read_once().await;
        }
    }

    /// The URL as an item may show it, with any password hidden.
    fn shown_url(&self) -> String {
        redact(self.url.as_deref().unwrap_or_default())
    }
}

impl Source for Reader {
    fn items(&self, fleet: &Fleet) -> Vec<Item> {
        let (what, todo) = match self.health() {
            None | Some(Health::Healthy) => return Vec::new(),
            Some(Health::NotSet(missing)) => {
                let what = missing
                    .iter()
                    .map(|name| match *name {
                        GATEWAY_MANAGEMENT => format!("the secret {GATEWAY_MANAGEMENT} is not set"),
                        name => format!("{name} is not set"),
                    })
                    .collect::<Vec<_>>()
                    .join(" and ");
                let todo = missing
                    .iter()
                    .map(|name| match *name {
                        GATEWAY_MANAGEMENT => {
                            format!("run `foundryd secret set {GATEWAY_MANAGEMENT}` on it")
                        }
                        name => format!("set {name} for its foundryd and restart it"),
                    })
                    .collect::<Vec<_>>()
                    .join(", and ");
                (
                    format!("The gateway cannot be checked: {what}"),
                    capitalized(&todo),
                )
            }
            Some(Health::KeyUnreadable) => (
                format!(
                    "The gateway cannot be checked: the secret {GATEWAY_MANAGEMENT} could not be \
                     read"
                ),
                "See the daemon log for why, fix it, then restart foundryd".to_owned(),
            ),
            Some(Health::KeyRefused) => (
                format!(
                    "The gateway at {} refused the management key: it answered 401",
                    self.shown_url()
                ),
                format!(
                    "Run `foundryd secret set {GATEWAY_MANAGEMENT}` with the right key; if the \
                     gateway was fixed to take this one, restart foundryd"
                ),
            ),
            Some(Health::Unanswered(why)) => (
                format!("The gateway at {} does not answer: {why}", self.shown_url()),
                "Check that the gateway runs and that this machine can reach it".to_owned(),
            ),
            Some(Health::NoAccounts) => (
                format!("The gateway at {} has no account to use", self.shown_url()),
                "Add an account to the gateway, or enable one".to_owned(),
            ),
            Some(Health::CoolingDown { accounts, free_in }) => {
                let soonest = free_in.map_or_else(String::new, |wait| {
                    format!("; the first is free again in {}", minutes(wait))
                });
                (
                    format!(
                        "Every account of the gateway is cooling down ({accounts} of \
                         them){soonest}"
                    ),
                    "Wait for one to come back, or add an account to the gateway".to_owned(),
                )
            }
        };
        vec![fleet.item(Kind::GatewayUnhealthy, fleet.me, fleet.me, what, todo)]
    }
}

fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

/// `wait` in whole minutes, rounded up, as `1 minute` or `12 minutes`.
fn minutes(wait: Duration) -> String {
    let minutes = wait.as_secs().div_ceil(60).max(1);
    if minutes == 1 {
        "1 minute".to_owned()
    } else {
        format!("{minutes} minutes")
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use axum::Router;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::get;
    use foundry_proto::ids::MachineId;
    use foundry_proto::settings::SharedSettings;

    use super::*;
    use crate::secrets::FileSecrets;

    const KEY: &str = "mgmt-key-0123456789abcdef";

    /// A gateway that answers `files` to a read with the right key, and 401 to any other.
    async fn gateway(files: serde_json::Value) -> String {
        let app = Router::new().route(
            "/v0/management/auth-files",
            get(move |headers: HeaderMap| {
                let files = files.clone();
                async move {
                    let auth = headers.get("authorization").and_then(|v| v.to_str().ok());
                    if auth != Some(&format!("Bearer {KEY}")) {
                        return (StatusCode::UNAUTHORIZED, format!("bad key {auth:?}"))
                            .into_response();
                    }
                    axum::Json(serde_json::json!({ "observed_at": "now", "files": files }))
                        .into_response()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    /// An address nothing listens on.
    fn refusing() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        format!("http://{addr}")
    }

    fn account(cooldowns: serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "name": "claude-a.json", "disabled": false, "cooldowns": cooldowns })
    }

    fn cooling(remaining: u64) -> serde_json::Value {
        account(serde_json::json!([{
            "scope": "credential",
            "reason": "quota",
            "retry_at": "2026-11-20T13:00:00Z",
            "remaining_seconds": remaining
        }]))
    }

    struct Machine {
        _home: tempfile::TempDir,
        secrets: Arc<dyn Secrets>,
        me: MachineId,
    }

    impl Machine {
        fn new(key: Option<&str>) -> Self {
            let home = tempfile::tempdir().unwrap();
            let secrets: Arc<dyn Secrets> =
                Arc::new(FileSecrets::new(home.path().join("secrets.json")));
            if let Some(key) = key {
                secrets.set(GATEWAY_MANAGEMENT, key).unwrap();
            }
            Self {
                _home: home,
                secrets,
                me: MachineId::random(),
            }
        }

        /// The items a reader of `url` raises after one read.
        async fn items(&self, url: Option<String>) -> Vec<Item> {
            let reader = Arc::new(Reader::new(url, self.secrets.clone()));
            reader.read_once().await;
            reader.items(&self.fleet())
        }

        fn fleet(&self) -> Fleet {
            Fleet {
                me: self.me,
                records: Vec::new(),
                settings: SharedSettings::default(),
                online: BTreeSet::from([self.me]),
            }
        }
    }

    fn one(items: &[Item]) -> &Item {
        assert_eq!(items.len(), 1, "{items:?}");
        assert_eq!(items[0].kind, Kind::GatewayUnhealthy);
        &items[0]
    }

    #[tokio::test]
    async fn a_gateway_that_refuses_the_connection_raises_gateway_unhealthy() {
        let machine = Machine::new(Some(KEY));
        let url = refusing();
        let items = machine.items(Some(url.clone())).await;
        let item = one(&items);
        assert_eq!(item.id, format!("gateway_unhealthy:{}", machine.me));
        assert_eq!(item.machine, machine.me);
        assert!(item.what.contains(&url), "{}", item.what);
        assert!(item.what.contains("does not answer"), "{}", item.what);
    }

    #[tokio::test]
    async fn a_gateway_with_every_account_cooling_down_raises_gateway_unhealthy() {
        let machine = Machine::new(Some(KEY));
        let url = gateway(serde_json::json!([cooling(600), cooling(90)])).await;
        let items = machine.items(Some(url)).await;
        let item = one(&items);
        assert!(
            item.what.contains("cooling down (2 of them)"),
            "{}",
            item.what
        );
        assert!(
            item.what.contains("free again in 2 minutes"),
            "{}",
            item.what
        );
    }

    #[tokio::test]
    async fn a_gateway_that_answers_with_an_account_free_clears_gateway_unhealthy() {
        let machine = Machine::new(None);
        let url = gateway(serde_json::json!([
            cooling(600),
            account(serde_json::json!([])),
            account(serde_json::Value::Null),
        ]))
        .await;
        let reader = Arc::new(Reader::new(Some(url), machine.secrets.clone()));
        reader.read_once().await;
        assert_eq!(reader.items(&machine.fleet()).len(), 1);

        // The same reader, once the key is set: the gateway answers with accounts free.
        machine.secrets.set(GATEWAY_MANAGEMENT, KEY).unwrap();
        reader.read_once().await;
        assert_eq!(reader.health(), Some(Health::Healthy));
        assert!(reader.items(&machine.fleet()).is_empty());
    }

    #[tokio::test]
    async fn a_gateway_that_takes_the_connection_and_never_answers_is_unhealthy() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let held = tokio::spawn(async move {
            let mut open = Vec::new();
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                open.push(socket);
            }
        });
        let read = tokio::task::spawn_blocking(move || read(&url, KEY));
        let health = tokio::time::timeout(ANSWER_WITHIN + Duration::from_secs(3), read)
            .await
            .expect("the read gave up in time")
            .unwrap();
        held.abort();
        assert!(matches!(health, Health::Unanswered(_)), "{health:?}");
    }

    /// A secret store that counts its reads, and fails them while `fail` is set.
    struct Counting {
        inner: FileSecrets,
        reads: std::sync::atomic::AtomicUsize,
        fail: std::sync::atomic::AtomicBool,
    }

    impl Secrets for Counting {
        fn get(&self, name: &str) -> Result<Option<String>, SecretsError> {
            self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(SecretsError::Keychain {
                    action: "read",
                    name: name.to_owned(),
                    reason: "denied".into(),
                });
            }
            self.inner.get(name)
        }
        fn set(&self, name: &str, value: &str) -> Result<(), SecretsError> {
            self.inner.set(name, value)
        }
        fn update(&self, name: &str, edit: crate::secrets::Edit<'_>) -> Result<(), SecretsError> {
            self.inner.update(name, edit)
        }
    }

    fn counting(home: &tempfile::TempDir) -> Arc<Counting> {
        Arc::new(Counting {
            inner: FileSecrets::new(home.path().join("secrets.json")),
            reads: Default::default(),
            fail: Default::default(),
        })
    }

    fn reads(secrets: &Counting) -> usize {
        secrets.reads.load(std::sync::atomic::Ordering::SeqCst)
    }

    #[tokio::test]
    async fn the_key_is_read_once_and_again_only_after_the_gateway_refuses_it() {
        let home = tempfile::tempdir().unwrap();
        let secrets = counting(&home);
        secrets.set(GATEWAY_MANAGEMENT, KEY).unwrap();
        let url = gateway(serde_json::json!([account(serde_json::json!([]))])).await;
        let reader = Arc::new(Reader::new(Some(url.clone()), secrets.clone()));
        for _ in 0..3 {
            reader.read_once().await;
        }
        assert_eq!(reader.health(), Some(Health::Healthy));
        assert_eq!(reads(&secrets), 1);

        secrets
            .set(GATEWAY_MANAGEMENT, "a-wrong-key-0123456789")
            .unwrap();
        let reader = Arc::new(Reader::new(Some(url), secrets.clone()));
        reader.read_once().await;
        assert!(
            matches!(reader.health(), Some(Health::KeyRefused)),
            "{:?}",
            reader.health()
        );
        secrets.set(GATEWAY_MANAGEMENT, KEY).unwrap();
        reader.read_once().await;
        assert_eq!(reader.health(), Some(Health::Healthy));
        assert_eq!(reads(&secrets), 3);
    }

    /// A gateway that answers every read with `status` and `body`, and counts the reads.
    async fn answering(
        status: StatusCode,
        body: serde_json::Value,
    ) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = hits.clone();
        let app = Router::new().route(
            "/v0/management/auth-files",
            get(move || {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let body = body.clone();
                async move { (status, axum::Json(body)).into_response() }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), hits)
    }

    #[tokio::test]
    async fn a_refused_key_is_not_sent_again_until_it_changes() {
        let (url, hits) = answering(
            StatusCode::UNAUTHORIZED,
            serde_json::json!({ "error": "invalid management key" }),
        )
        .await;
        let machine = Machine::new(Some("a-wrong-key-0123456789"));
        let reader = Arc::new(Reader::new(Some(url), machine.secrets.clone()));
        for _ in 0..3 {
            reader.read_once().await;
        }
        assert_eq!(reader.health(), Some(Health::KeyRefused));
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);

        machine
            .secrets
            .set(GATEWAY_MANAGEMENT, "another-key-0123456789")
            .unwrap();
        reader.read_once().await;
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_gateway_that_refuses_for_another_reason_says_why() {
        for reason in [
            "remote management disabled",
            "IP banned due to too many failed attempts. Try again in 29m",
        ] {
            let (url, _) = answering(
                StatusCode::FORBIDDEN,
                serde_json::json!({ "error": reason }),
            )
            .await;
            let machine = Machine::new(Some(KEY));
            let items = machine.items(Some(url)).await;
            let item = one(&items);
            assert!(item.what.contains("it answered 403"), "{}", item.what);
            assert!(item.what.contains(reason), "{}", item.what);
            assert!(
                !item.what.contains("refused the management key"),
                "{}",
                item.what
            );
        }
    }

    #[tokio::test]
    async fn a_gateways_answer_never_shows_the_key() {
        let echo = format!("bad key {KEY} for https://tom:hunter2@gw");
        let (url, _) = answering(
            StatusCode::BAD_GATEWAY,
            serde_json::json!({ "error": echo }),
        )
        .await;
        let machine = Machine::new(Some(KEY));
        let text = serde_json::to_string(one(&machine.items(Some(url)).await)).unwrap();
        assert!(!text.contains(KEY) && !text.contains("hunter2"), "{text}");
    }

    #[tokio::test]
    async fn a_secret_across_the_cut_of_the_gateways_reason_never_shows() {
        let filler = "x".repeat(REASON_AT_MOST - 5);
        for echo in [
            format!("{filler}{KEY}"),
            format!("{filler} https://tom:hunter2@gw/x"),
        ] {
            let (url, _) = answering(
                StatusCode::BAD_GATEWAY,
                serde_json::json!({ "error": echo }),
            )
            .await;
            let machine = Machine::new(Some(KEY));
            let text = serde_json::to_string(one(&machine.items(Some(url)).await)).unwrap();
            assert!(!text.contains(&KEY[..6]), "{text}");
            assert!(!text.contains("hunt"), "{text}");
        }
    }

    #[tokio::test]
    async fn a_refused_key_is_sent_again_after_a_long_wait_and_clears_once_accepted() {
        let status = Arc::new(std::sync::atomic::AtomicU16::new(401));
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let app = Router::new().route(
            "/v0/management/auth-files",
            get({
                let (status, hits) = (status.clone(), hits.clone());
                move || {
                    hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let status = status.load(std::sync::atomic::Ordering::SeqCst);
                    async move {
                        let files =
                            serde_json::json!({ "files": [account(serde_json::json!([]))] });
                        (StatusCode::from_u16(status).unwrap(), axum::Json(files)).into_response()
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let machine = Machine::new(Some(KEY));
        let mut reader = Reader::new(Some(url), machine.secrets.clone());
        reader.retry_refused = Duration::from_millis(300);
        let reader = Arc::new(reader);

        reader.read_once().await;
        reader.read_once().await;
        assert_eq!(reader.health(), Some(Health::KeyRefused));
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
        let item = reader.items(&machine.fleet());
        assert!(one(&item).todo.contains("restart"), "{}", item[0].todo);

        // The gateway is fixed to take the same key.
        status.store(200, std::sync::atomic::Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(400)).await;
        reader.read_once().await;
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(reader.health(), Some(Health::Healthy));
    }

    #[test]
    fn a_refused_key_is_retried_rarely_enough_never_to_be_banned() {
        // CLIProxyAPI bans an address after 5 refused keys and forgets one after 2 idle hours.
        assert!(RETRY_REFUSED > Duration::from_secs(2 * 60 * 60));
    }

    #[tokio::test]
    async fn a_key_the_store_cannot_read_is_not_asked_for_again() {
        let home = tempfile::tempdir().unwrap();
        let secrets = counting(&home);
        secrets
            .fail
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let reader = Arc::new(Reader::new(Some(refusing()), secrets.clone()));
        for _ in 0..3 {
            reader.read_once().await;
        }
        assert_eq!(reader.health(), Some(Health::KeyUnreadable));
        assert_eq!(reads(&secrets), 1);
        let machine = Machine::new(None);
        let items = reader.items(&machine.fleet());
        assert!(one(&items).todo.contains("restart"), "{}", items[0].todo);
    }

    #[tokio::test]
    async fn a_disabled_account_is_not_free() {
        let machine = Machine::new(Some(KEY));
        let disabled = serde_json::json!({ "name": "b.json", "disabled": true, "cooldowns": [] });
        let url = gateway(serde_json::json!([cooling(60), disabled.clone()])).await;
        let item = machine.items(Some(url)).await;
        assert!(one(&item).what.contains("(1 of them)"), "{}", item[0].what);

        let url = gateway(serde_json::json!([disabled])).await;
        let item = machine.items(Some(url)).await;
        assert!(one(&item).what.contains("no account"), "{}", item[0].what);
    }

    #[tokio::test]
    async fn the_management_key_is_sent_and_a_refused_key_never_shows() {
        let machine = Machine::new(Some("not-the-key-abcdef0123"));
        let url = gateway(serde_json::json!([account(serde_json::json!([]))])).await;
        let items = machine.items(Some(url)).await;
        let item = one(&items);
        assert!(item.what.contains("it answered 401"), "{}", item.what);
        let text = serde_json::to_string(item).unwrap();
        assert!(!text.contains("not-the-key"), "{text}");
    }

    #[tokio::test]
    async fn a_missing_url_or_key_is_named_in_the_item() {
        let url = gateway(serde_json::json!([account(serde_json::json!([]))])).await;
        let both = ["FOUNDRY_GATEWAY_URL", "gateway-management"];
        let cases = [
            (None, Some(KEY), &both[..1], Some(both[1])),
            (Some(url), None, &both[1..], Some(both[0])),
            (None, None, &both[..], None),
        ];
        for (url, key, named, unnamed) in cases {
            let machine = Machine::new(key);
            let items = machine.items(url).await;
            let item = one(&items);
            for name in named {
                assert!(item.what.contains(name), "{name}: {}", item.what);
                assert!(item.todo.contains(name), "{name}: {}", item.todo);
            }
            if let Some(unnamed) = unnamed {
                assert!(!item.what.contains(unnamed), "{}", item.what);
            }
        }
    }

    #[tokio::test]
    async fn a_password_in_the_url_never_shows() {
        let machine = Machine::new(Some(KEY));
        let url = refusing().replace("http://", "http://tom:hunter2@");
        let items = machine.items(Some(url)).await;
        let text = serde_json::to_string(one(&items)).unwrap();
        assert!(!text.contains("hunter2"), "{text}");
    }

    #[test]
    fn nothing_is_raised_before_the_first_read() {
        let machine = Machine::new(Some(KEY));
        let reader = Reader::new(Some(refusing()), machine.secrets.clone());
        assert_eq!(reader.health(), None);
        assert!(reader.items(&machine.fleet()).is_empty());
    }
}
