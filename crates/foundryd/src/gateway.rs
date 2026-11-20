//! Gateway health and account cooldown.
//!
//! Every [`READ_EVERY`] this machine reads the gateway's management API, CLIProxyAPI's
//! `GET <FOUNDRY_GATEWAY_URL>/v0/management/auth-files`, with the management key (the secret
//! [`GATEWAY_MANAGEMENT`]) as a bearer token. The gateway is unhealthy while it does not answer
//! with its accounts, or answers with none free: every account cooling down or disabled. It is
//! healthy again once it answers with an account free. Health is this machine's own read, so
//! the item it raises shows on this machine's list only.
//!
//! The URL and the key are read on every read, so a key set with `foundryd secret set` counts
//! from the next one. Neither the key nor a password in the URL reaches an item or a log line.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde::Deserialize;
use tokio::time::MissedTickBehavior;

use crate::attention::{Fleet, Item, Kind, Source};
use crate::secrets::{GATEWAY_MANAGEMENT, Secrets, SecretsError, redact};

/// How often the gateway is read.
pub const READ_EVERY: Duration = Duration::from_secs(10);

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

/// Reads the gateway at `url` with the management key `key`. Blocks for up to
/// [`ANSWER_WITHIN`].
pub fn read(url: &str, key: &str) -> Health {
    let endpoint = format!("{}/v0/management/auth-files", url.trim_end_matches('/'));
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(ANSWER_WITHIN))
        .build()
        .into();
    let answer = agent
        .get(&endpoint)
        .header("Authorization", format!("Bearer {key}"))
        .call();
    let why = match answer {
        Ok(mut response) => match response.body_mut().read_json::<AuthFiles>() {
            Ok(answer) => return judged(&answer.files),
            Err(_) => "its answer is not a list of accounts".to_owned(),
        },
        Err(ureq::Error::StatusCode(status)) => format!("it answered {status}"),
        Err(error) => error.to_string(),
    };
    Health::Unanswered(redact(&why).replace(key, "***"))
}

/// Reads the gateway at `url`, when set, with the management key `key` as the secret store
/// gave it.
pub fn check(url: Option<&str>, key: Result<Option<String>, SecretsError>) -> Health {
    let url = url.filter(|url| !url.is_empty());
    let key = match key {
        Ok(key) => key.filter(|key| !key.is_empty()),
        Err(error) => {
            tracing::warn!("gateway: the secret {GATEWAY_MANAGEMENT} cannot be read: {error}");
            return Health::KeyUnreadable;
        }
    };
    match (url, key) {
        (Some(url), Some(key)) => read(url, &key),
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

/// This machine's reader of the gateway, and the newest health it read.
pub struct Reader {
    url: Option<String>,
    secrets: Arc<dyn Secrets>,
    /// `None` until the first read ends.
    health: Mutex<Option<Health>>,
}

impl Reader {
    /// Reads the gateway at `url` (`FOUNDRY_GATEWAY_URL`) with the management key in `secrets`.
    pub fn new(url: Option<String>, secrets: Arc<dyn Secrets>) -> Self {
        Self {
            url,
            secrets,
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
        let health = tokio::task::spawn_blocking(move || {
            check(
                reader.url.as_deref(),
                reader.secrets.get(GATEWAY_MANAGEMENT),
            )
        })
        .await;
        match health {
            Ok(health) => {
                *self.health.lock().unwrap_or_else(PoisonError::into_inner) = Some(health);
            }
            Err(error) => tracing::warn!("gateway: the read did not finish: {error}"),
        }
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
    fn items(&self, _fleet: &Fleet) -> Vec<Item> {
        Vec::new()
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
        assert!(item.what.contains("cooling down (2 of them)"), "{}", item.what);
        assert!(item.what.contains("free again in 2 minutes"), "{}", item.what);
    }

    #[tokio::test]
    async fn a_gateway_that_answers_with_an_account_free_clears_gateway_unhealthy() {
        let machine = Machine::new(Some(KEY));
        let reader = Arc::new(Reader::new(Some(refusing()), machine.secrets.clone()));
        reader.read_once().await;
        assert_eq!(reader.items(&machine.fleet()).len(), 1);

        let url = gateway(serde_json::json!([
            cooling(600),
            account(serde_json::json!([])),
            account(serde_json::Value::Null),
        ]))
        .await;
        let reader = Arc::new(Reader::new(Some(url), machine.secrets.clone()));
        reader.read_once().await;
        assert_eq!(reader.health(), Some(Health::Healthy));
        assert!(reader.items(&machine.fleet()).is_empty());
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
