//! Secrets in the macOS keychain or a 0600 file on Linux.
//!
//! [`Secrets`] is the one interface: [`Keychain`] backs it on macOS and [`FileSecrets`] on
//! Linux, and [`for_this_machine`] picks the one this machine uses. A secret's value never
//! reaches an error message: errors name the secret or the file, never what it holds.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
#[cfg(test)]
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};

/// How long a set or an update waits on the secrets lock with no other change getting through
/// before it gives up with [`SecretsError::Locked`]. A waiter behind holders that keep finishing
/// waits on; one behind a holder that is stuck (on a keychain prompt, say) gives up.
pub const LOCK_WAIT: Duration = Duration::from_secs(5);

/// `text` with the user and password of every URL in it hidden: `https://user:token@host`
/// becomes `https://***@host`. A git address with no scheme, `git@host:repo`, holds no secret
/// and is left as it is.
pub fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("://") {
        let (before, after) = rest.split_at(at + 3);
        out.push_str(before);
        let authority = after
            .find(|c: char| {
                matches!(c, '/' | '?' | '#') || c.is_whitespace() || c == '\'' || c == '"'
            })
            .unwrap_or(after.len());
        match after[..authority].rfind('@') {
            Some(user_end) => {
                out.push_str("***");
                out.push_str(&after[user_end..authority]);
            }
            None => out.push_str(&after[..authority]),
        }
        rest = &after[authority..];
    }
    out.push_str(rest);
    out
}

/// Named secrets, such as the gateway key or this machine's key.
pub trait Secrets: Send + Sync {
    /// The secret called `name`, or `None` when none is set.
    fn get(&self, name: &str) -> Result<Option<String>, SecretsError>;
    /// Sets the secret called `name`, replacing any value it had.
    fn set(&self, name: &str, value: &str) -> Result<(), SecretsError>;
    /// Changes the secret called `name` in one step: `edit` is given its value now and returns
    /// the value to set, or `None` to leave the store as it is. No other change to the store,
    /// from this process or another, lands between the read and the write.
    fn update(&self, name: &str, edit: Edit<'_>) -> Result<(), SecretsError>;
}

/// What [`Secrets::update`] does to a secret: given its value now, the value to set, or `None`
/// to set nothing.
pub type Edit<'a> = Box<dyn FnOnce(Option<String>) -> Option<String> + 'a>;

#[derive(Debug, thiserror::Error)]
pub enum SecretsError {
    #[error(
        "the secrets file {} can be read or written by others (mode {mode:o}); make it 0600",
        path.display()
    )]
    TooOpen { path: PathBuf, mode: u32 },
    #[error("the secrets file {} does not hold secrets in the expected form", path.display())]
    Unreadable { path: PathBuf },
    #[error("the secrets file {}: {source}", path.display())]
    Io { path: PathBuf, source: io::Error },
    #[error(
        "no change has got through the secrets lock {} for {} s; another process may be stuck \
         holding it; try again",
        path.display(),
        LOCK_WAIT.as_secs()
    )]
    Locked { path: PathBuf },
    #[error("the keychain could not {action} the secret {name}: {reason}")]
    Keychain {
        action: &'static str,
        name: String,
        reason: String,
    },
}

/// The secret the model gateway's key is held under: sessions send it as
/// `ANTHROPIC_AUTH_TOKEN`.
pub const GATEWAY: &str = "gateway";

/// The secret the gateway's management key is held under.
pub const GATEWAY_MANAGEMENT: &str = "gateway-management";

/// The secret store this machine uses: the login keychain on macOS, its updates locked by
/// the file `secrets.lock` in `home`, and on Linux the file `secrets.json` in `home`, readable
/// only by the service user. In a debug build `FOUNDRY_SECRETS=file` picks the file on every
/// OS, which is how tests keep out of the keychain. A release build ignores it.
pub fn for_this_machine(home: &Path) -> Arc<dyn Secrets> {
    store_for(home, store(std::env::var_os("FOUNDRY_SECRETS").as_deref()))
}

/// Whether this build takes `FOUNDRY_SECRETS`: only a debug build, so a release Mac always
/// keeps its identity and keys in the keychain.
const OVERRIDE: bool = cfg!(debug_assertions);

/// Which secret store a machine uses.
#[derive(Debug, PartialEq, Eq)]
enum Store {
    File,
    Keychain,
}

/// The store for a `FOUNDRY_SECRETS` value: the file for exactly `file` when `taken` (the build
/// takes the override), else this OS's default, the keychain on macOS and the file elsewhere.
fn choose(choice: Option<&OsStr>, taken: bool) -> Store {
    if (taken && choice == Some(OsStr::new("file"))) || !cfg!(target_os = "macos") {
        Store::File
    } else {
        Store::Keychain
    }
}

/// The store this build picks for a `FOUNDRY_SECRETS` value.
fn store(choice: Option<&OsStr>) -> Store {
    choose(choice, OVERRIDE)
}

/// The secret store of kind `store` for the data directory `home`.
fn store_for(home: &Path, store: Store) -> Arc<dyn Secrets> {
    match store {
        Store::File => Arc::new(FileSecrets::new(home.join("secrets.json"))),
        #[cfg(target_os = "macos")]
        Store::Keychain => Arc::new(Keychain::new("foundry", home.join("secrets.lock"))),
        #[cfg(not(target_os = "macos"))]
        Store::Keychain => unreachable!("only macOS chooses the keychain"),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SetError {
    #[error("{name} is not a secret to set here; the names are {GATEWAY} and {GATEWAY_MANAGEMENT}")]
    UnknownName { name: String },
    #[error("no value on stdin for {name}")]
    NoValue { name: String },
    #[error("cannot read stdin for {name}: {source}")]
    Stdin { name: String, source: io::Error },
    #[error(transparent)]
    Secrets(#[from] SecretsError),
}

/// `foundryd secret set <name>`: reads one line from stdin and stores it in this machine's
/// secret store. It never prints the value.
pub fn set_command(name: &str) -> ExitCode {
    let result = match crate::daemon::home() {
        Some(home) => set_from(
            &*for_this_machine(&home),
            name,
            io::BufReader::new(io::stdin()),
        ),
        None => {
            eprintln!("foundryd secret set: set FOUNDRY_HOME or HOME");
            return ExitCode::FAILURE;
        }
    };
    match result {
        Ok(()) => {
            println!("foundryd secret set {name}: stored; restart foundryd so new sessions use it");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("foundryd secret set: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Stores the first line of `input` as the secret `name`, without its line ending. Only the
/// gateway secrets can be set: the name is checked before `input` is read, so any other name
/// leaves the store untouched, and the machine's own identity and key table stay out of reach.
pub fn set_from(
    secrets: &dyn Secrets,
    name: &str,
    mut input: impl BufRead,
) -> Result<(), SetError> {
    if name != GATEWAY && name != GATEWAY_MANAGEMENT {
        return Err(SetError::UnknownName {
            name: name.to_owned(),
        });
    }
    let mut line = String::new();
    input
        .read_line(&mut line)
        .map_err(|source| SetError::Stdin {
            name: name.to_owned(),
            source,
        })?;
    let line = line.strip_suffix('\n').unwrap_or(&line);
    let value = line.strip_suffix('\r').unwrap_or(line);
    if value.is_empty() {
        return Err(SetError::NoValue {
            name: name.to_owned(),
        });
    }
    Ok(secrets.set(name, value)?)
}

/// Secrets in one JSON file that only its owner may read or write. The file is created 0600,
/// and one that others can read or write is refused, at read and before a write.
///
/// A set or an update holds an exclusive lock on the file `<path>.lock` from its read to its
/// write, so two stores over one file, in one process or two (the daemon and `foundryd join`),
/// never lose each other's secrets.
pub struct FileSecrets {
    path: PathBuf,
}

impl FileSecrets {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// `<path><suffix>`, beside the secrets file.
    fn beside(&self, suffix: &str) -> PathBuf {
        let mut name = self.path.clone().into_os_string();
        name.push(suffix);
        PathBuf::from(name)
    }

    /// Takes the lock every set and update holds, waiting until [`LOCK_WAIT`] passes with no
    /// other holder taking it. It is released when the value is dropped.
    fn lock(&self) -> Result<Flock<File>, SecretsError> {
        lock_file(&self.beside(".lock"), LOCK_WAIT)
    }

    /// Every secret in the file, or none when there is no file yet.
    fn read(&self) -> Result<BTreeMap<String, String>, SecretsError> {
        let mut file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
            Err(error) => return Err(self.io(error)),
        };
        // The mode of the file opened, not of whatever the path names a moment later.
        let mode = file
            .metadata()
            .map_err(|e| self.io(e))?
            .permissions()
            .mode()
            & 0o777;
        if mode & 0o077 != 0 {
            return Err(SecretsError::TooOpen {
                path: self.path.clone(),
                mode,
            });
        }
        let mut text = String::new();
        file.read_to_string(&mut text).map_err(|e| self.io(e))?;
        // A parse error can quote what it could not parse, so it is not passed on.
        serde_json::from_str(&text).map_err(|_| SecretsError::Unreadable {
            path: self.path.clone(),
        })
    }

    /// Writes `secrets` to a new 0600 file beside the old one, then moves it into place, so
    /// a crash leaves either the old file or the new one.
    /// The caller holds [`FileSecrets::lock`], so no other writer uses the temporary file.
    fn write(&self, secrets: &BTreeMap<String, String>) -> Result<(), SecretsError> {
        let temporary = self.beside(".tmp");
        // A leftover temporary file keeps its own mode when opened, so it is removed first.
        match fs::remove_file(&temporary) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(self.io(error)),
            _ => {}
        }
        let text = serde_json::to_string(secrets).map_err(|_| SecretsError::Unreadable {
            path: self.path.clone(),
        })?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|e| self.io(e))?;
        file.write_all(text.as_bytes()).map_err(|e| self.io(e))?;
        file.sync_all().map_err(|e| self.io(e))?;
        fs::rename(&temporary, &self.path).map_err(|e| self.io(e))?;
        // The rename is durable only once the directory that records it is.
        if let Some(dir) = self.path.parent() {
            File::open(dir)
                .and_then(|dir| dir.sync_all())
                .map_err(|e| self.io(e))?;
        }
        Ok(())
    }

    fn io(&self, source: io::Error) -> SecretsError {
        SecretsError::Io {
            path: self.path.clone(),
            source,
        }
    }
}

impl Secrets for FileSecrets {
    fn get(&self, name: &str) -> Result<Option<String>, SecretsError> {
        Ok(self.read()?.remove(name))
    }

    fn set(&self, name: &str, value: &str) -> Result<(), SecretsError> {
        let value = value.to_owned();
        self.update(name, Box::new(|_| Some(value)))
    }

    fn update(&self, name: &str, edit: Edit<'_>) -> Result<(), SecretsError> {
        let _locked = self.lock()?;
        let mut secrets = self.read()?;
        let Some(value) = edit(secrets.get(name).cloned()) else {
            return Ok(());
        };
        secrets.insert(name.to_owned(), value);
        self.write(&secrets)
    }
}

/// An exclusive lock on the file at `path`, made 0600 with its directory (0700, as `foundryd
/// run` wants the data directory) if needed, held until
/// the value is dropped. It is a `flock`, so it holds between processes, and between two opens
/// of the file in one process. Each holder stamps the file's modified time as it takes it, and
/// a waiter counts a new stamp as progress: it gives [`SecretsError::Locked`] only once `wait`
/// passes with no new holder, so a stuck process stalls nobody for long and busy ones fail
/// nobody.
fn lock_file(path: &Path, wait: Duration) -> Result<Flock<File>, SecretsError> {
    let io = |source: io::Error| SecretsError::Io {
        path: path.to_owned(),
        source,
    };
    if let Some(dir) = path.parent() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(io)?;
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .map_err(io)?;
    let stamp = |file: &File| file.metadata().and_then(|m| m.modified()).ok();
    let mut seen = stamp(&file);
    let mut deadline = Instant::now() + wait;
    loop {
        match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(locked) => {
                locked.set_modified(SystemTime::now()).map_err(io)?;
                return Ok(locked);
            }
            Err((held, Errno::EWOULDBLOCK)) => {
                let now = stamp(&held);
                if now != seen {
                    seen = now;
                    deadline = Instant::now() + wait;
                } else if Instant::now() >= deadline {
                    return Err(SecretsError::Locked {
                        path: path.to_owned(),
                    });
                }
                file = held;
                std::thread::sleep(Duration::from_millis(10));
            }
            Err((_, errno)) => return Err(io(errno.into())),
        }
    }
}

/// An update of one secret in a store with no lock of its own: under the lock file `lock`,
/// `edit` is given what `get` reads and what it returns is passed to `set`.
#[cfg(any(target_os = "macos", test))]
fn update_locked(
    lock: &Path,
    edit: Edit<'_>,
    get: impl FnOnce() -> Result<Option<String>, SecretsError>,
    set: impl FnOnce(String) -> Result<(), SecretsError>,
) -> Result<(), SecretsError> {
    let _locked = lock_file(lock, LOCK_WAIT)?;
    match edit(get()?) {
        Some(value) => set(value),
        None => Ok(()),
    }
}

/// Secrets as generic passwords in the login keychain, one per name, under `service`. The
/// keychain has no lock of its own, so a set or an update holds an exclusive lock on the file
/// `lock` from its read to its write. A read or write that brings up a keychain prompt holds
/// that lock until someone answers it; another process gives up after [`LOCK_WAIT`].
#[cfg(target_os = "macos")]
pub struct Keychain {
    service: String,
    lock: PathBuf,
}

#[cfg(target_os = "macos")]
impl Keychain {
    pub fn new(service: impl Into<String>, lock: impl Into<PathBuf>) -> Self {
        Self {
            service: service.into(),
            lock: lock.into(),
        }
    }
}

#[cfg(target_os = "macos")]
impl Secrets for Keychain {
    fn get(&self, name: &str) -> Result<Option<String>, SecretsError> {
        /// `errSecItemNotFound`: the keychain holds no such item.
        const NOT_FOUND: i32 = -25300;
        let failed = |reason: String| SecretsError::Keychain {
            action: "read",
            name: name.to_owned(),
            reason,
        };
        match security_framework::passwords::get_generic_password(&self.service, name) {
            Ok(bytes) => String::from_utf8(bytes)
                .map(Some)
                .map_err(|_| failed("it is not UTF-8".into())),
            Err(error) if error.code() == NOT_FOUND => Ok(None),
            Err(error) => Err(failed(error.to_string())),
        }
    }

    fn set(&self, name: &str, value: &str) -> Result<(), SecretsError> {
        let value = value.to_owned();
        self.update(name, Box::new(|_| Some(value)))
    }

    fn update(&self, name: &str, edit: Edit<'_>) -> Result<(), SecretsError> {
        let store = |value: String| {
            security_framework::passwords::set_generic_password(
                &self.service,
                name,
                value.as_bytes(),
            )
            .map_err(|error| SecretsError::Keychain {
                action: "store",
                name: name.to_owned(),
                reason: error.to_string(),
            })
        };
        update_locked(&self.lock, edit, || self.get(name), store)
    }
}

/// Secrets held in memory only: the fake other modules' tests use.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct MemorySecrets(Mutex<BTreeMap<String, String>>);

#[cfg(test)]
impl Secrets for MemorySecrets {
    fn get(&self, name: &str) -> Result<Option<String>, SecretsError> {
        Ok(self.lock().get(name).cloned())
    }

    fn set(&self, name: &str, value: &str) -> Result<(), SecretsError> {
        self.lock().insert(name.to_owned(), value.to_owned());
        Ok(())
    }

    fn update(&self, name: &str, edit: Edit<'_>) -> Result<(), SecretsError> {
        let mut secrets = self.lock();
        if let Some(value) = edit(secrets.get(name).cloned()) {
            secrets.insert(name.to_owned(), value);
        }
        Ok(())
    }
}

#[cfg(test)]
impl MemorySecrets {
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, String>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The fake keychain the move's tests use.
#[cfg(test)]
impl OldStore for MemorySecrets {
    fn read(&self, name: &str) -> Result<Option<String>, SecretsError> {
        Ok(self.lock().get(name).cloned())
    }

    fn remove(&self, name: &str) -> Result<(), SecretsError> {
        self.lock().remove(name);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn file() -> (tempfile::TempDir, FileSecrets) {
        let dir = tempfile::tempdir().unwrap();
        let secrets = FileSecrets::new(dir.path().join("secrets.json"));
        (dir, secrets)
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn a_secret_set_is_got_back() {
        let (_dir, secrets) = file();
        assert_eq!(secrets.get("gateway").unwrap(), None);
        secrets.set("gateway", "sk-one").unwrap();
        secrets.set("management", "sk-two").unwrap();
        secrets.set("gateway", "sk-three").unwrap();
        assert_eq!(secrets.get("gateway").unwrap().as_deref(), Some("sk-three"));
        assert_eq!(
            secrets.get("management").unwrap().as_deref(),
            Some("sk-two")
        );
    }

    #[test]
    fn the_file_is_created_0600() {
        let (dir, secrets) = file();
        secrets.set("gateway", "sk-one").unwrap();
        assert_eq!(mode(&dir.path().join("secrets.json")), 0o600);
    }

    #[test]
    fn the_file_is_created_0600_whatever_mode_a_leftover_temporary_file_had() {
        let (dir, secrets) = file();
        let leftover = dir.path().join("secrets.json.tmp");
        fs::write(&leftover, "{}").unwrap();
        fs::set_permissions(&leftover, fs::Permissions::from_mode(0o644)).unwrap();
        secrets.set("gateway", "sk-one").unwrap();
        assert_eq!(mode(&dir.path().join("secrets.json")), 0o600);
    }

    #[test]
    fn a_file_others_can_read_or_write_is_refused_at_read() {
        for wider in [0o640, 0o604, 0o620, 0o602, 0o644] {
            let (dir, secrets) = file();
            secrets.set("gateway", "sk-one").unwrap();
            let path = dir.path().join("secrets.json");
            fs::set_permissions(&path, fs::Permissions::from_mode(wider)).unwrap();
            let error = secrets.get("gateway").unwrap_err();
            assert!(
                matches!(error, SecretsError::TooOpen { .. }),
                "{wider:o}: {error}"
            );
            assert!(error.to_string().contains("0600"), "{error}");
            assert!(secrets.set("other", "x").is_err(), "{wider:o}: set");
        }
    }

    #[test]
    fn an_unreadable_file_names_no_secret_in_its_error() {
        let (dir, secrets) = file();
        secrets.set("gateway", "sk-one").unwrap();
        let path = dir.path().join("secrets.json");
        // serde_json quotes a string of the wrong type in its error.
        let leaked = "\"sk-leaked\"";
        assert!(
            serde_json::from_str::<BTreeMap<String, String>>(leaked)
                .unwrap_err()
                .to_string()
                .contains("sk-leaked")
        );
        fs::write(&path, leaked).unwrap();
        let error = secrets.get("gateway").unwrap_err();
        assert!(!error.to_string().contains("sk-leaked"), "{error}");
        assert!(!format!("{error:?}").contains("sk-leaked"), "{error:?}");
    }

    #[test]
    fn a_missing_directory_is_created() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b/secrets.json");
        FileSecrets::new(&path).set("gateway", "sk-one").unwrap();
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn two_stores_over_one_file_lose_none_of_each_others_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.json");
        let writers: Vec<_> = (0..4)
            .map(|w| {
                let path = path.clone();
                std::thread::spawn(move || {
                    // Each writer is its own store, as the daemon and `foundryd join` are.
                    let secrets = FileSecrets::new(path);
                    for i in 0..25 {
                        secrets.set(&format!("w{w}-{i}"), "x").unwrap();
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }
        let secrets = FileSecrets::new(&path);
        for w in 0..4 {
            for i in 0..25 {
                assert!(
                    secrets.get(&format!("w{w}-{i}")).unwrap().is_some(),
                    "w{w}-{i}"
                );
            }
        }
    }

    #[test]
    fn two_stores_updating_one_secret_at_once_lose_no_update() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.json");
        let writers: Vec<_> = (0..4)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let secrets = FileSecrets::new(path);
                    for _ in 0..25 {
                        let add_one = |count: Option<String>| {
                            let count: u32 = count.map_or(0, |c| c.parse().unwrap());
                            Some((count + 1).to_string())
                        };
                        secrets.update("count", Box::new(add_one)).unwrap();
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }
        let count = FileSecrets::new(&path).get("count").unwrap();
        assert_eq!(count.as_deref(), Some("100"));
    }

    #[test]
    fn an_update_that_sets_nothing_leaves_the_store_as_it_was() {
        let (dir, secrets) = file();
        secrets
            .update(
                "gateway",
                Box::new(|value| {
                    assert_eq!(value, None);
                    None
                }),
            )
            .unwrap();
        assert!(!dir.path().join("secrets.json").exists());
        secrets.set("gateway", "sk-one").unwrap();
        secrets
            .update(
                "gateway",
                Box::new(|value| {
                    assert_eq!(value.as_deref(), Some("sk-one"));
                    None
                }),
            )
            .unwrap();
        assert_eq!(secrets.get("gateway").unwrap().as_deref(), Some("sk-one"));
    }

    #[test]
    fn a_locked_update_over_a_store_with_no_lock_loses_no_update() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("secrets.lock");
        // Each read and each write locks only itself, as one keychain call does.
        let store = Mutex::new(0u32);
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    for _ in 0..25 {
                        let add_one = |n: Option<String>| {
                            Some((n.unwrap().parse::<u32>().unwrap() + 1).to_string())
                        };
                        let get = || {
                            let n = *store.lock().unwrap();
                            std::thread::yield_now();
                            Ok(Some(n.to_string()))
                        };
                        let set = |n: String| {
                            *store.lock().unwrap() = n.parse().unwrap();
                            Ok(())
                        };
                        update_locked(&lock, Box::new(add_one), get, set).unwrap();
                    }
                });
            }
        });
        assert_eq!(*store.lock().unwrap(), 100);
    }

    #[test]
    fn a_lock_held_past_the_wait_gives_locked_and_changes_nothing() {
        let (dir, secrets) = file();
        secrets.set("gateway", "sk-one").unwrap();
        let held = lock_file(&dir.path().join("secrets.json.lock"), LOCK_WAIT).unwrap();
        let started = Instant::now();
        let failed = secrets.update("gateway", Box::new(|_| Some("sk-two".into())));
        assert!(started.elapsed() >= LOCK_WAIT);
        let error = failed.unwrap_err();
        assert!(matches!(error, SecretsError::Locked { .. }), "{error:?}");
        assert!(!error.to_string().contains("sk-"), "{error}");
        drop(held);
        assert_eq!(secrets.get("gateway").unwrap().as_deref(), Some("sk-one"));
    }

    #[test]
    fn a_waiter_behind_holders_that_keep_taking_the_lock_waits_on() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.lock");
        let held = lock_file(&path, LOCK_WAIT).unwrap();
        std::thread::scope(|scope| {
            let waiting = scope.spawn(|| lock_file(&path, Duration::from_millis(500)).map(drop));
            // Past the wait in all, but each step stamps the lock as a new holder taking it does.
            for _ in 0..8 {
                std::thread::sleep(Duration::from_millis(200));
                held.set_modified(SystemTime::now()).unwrap();
            }
            drop(held);
            waiting.join().unwrap().unwrap();
        });
    }

    #[test]
    fn taking_the_lock_stamps_it_as_progress_for_waiters() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.lock");
        drop(lock_file(&path, LOCK_WAIT).unwrap());
        let first = fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(Duration::from_millis(20));
        drop(lock_file(&path, LOCK_WAIT).unwrap());
        assert_ne!(fs::metadata(&path).unwrap().modified().unwrap(), first);
    }

    #[test]
    fn a_lock_let_go_within_the_wait_is_taken() {
        let (dir, secrets) = file();
        let held = lock_file(&dir.path().join("secrets.json.lock"), LOCK_WAIT).unwrap();
        let letting_go = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(held);
        });
        secrets.set("gateway", "sk-one").unwrap();
        letting_go.join().unwrap();
        assert_eq!(secrets.get("gateway").unwrap().as_deref(), Some("sk-one"));
    }

    #[test]
    fn the_in_memory_fake_updates_what_it_holds() {
        let secrets = MemorySecrets::default();
        secrets.update("n", Box::new(|_| Some("1".into()))).unwrap();
        secrets
            .update("n", Box::new(|n| Some(format!("{}2", n.unwrap()))))
            .unwrap();
        secrets.update("n", Box::new(|_| None)).unwrap();
        assert_eq!(secrets.get("n").unwrap().as_deref(), Some("12"));
    }

    #[test]
    fn the_in_memory_fake_keeps_what_is_set() {
        let secrets = MemorySecrets::default();
        assert_eq!(secrets.get("gateway").unwrap(), None);
        secrets.set("gateway", "sk-one").unwrap();
        assert_eq!(secrets.get("gateway").unwrap().as_deref(), Some("sk-one"));
    }

    /// Touches the real keychain, which may ask a person to allow it: a background service
    /// cannot answer that, so this runs only by hand.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore]
    fn the_keychain_keeps_what_is_set() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = Keychain::new("foundry-test", dir.path().join("secrets.lock"));
        let name = format!("probe-{}", uuid::Uuid::new_v4());
        assert_eq!(secrets.get(&name).unwrap(), None);
        secrets.set(&name, "sk-one").unwrap();
        secrets.set(&name, "sk-two").unwrap();
        assert_eq!(secrets.get(&name).unwrap().as_deref(), Some("sk-two"));
        secrets
            .update(
                &name,
                Box::new(|value| Some(format!("{}!", value.unwrap()))),
            )
            .unwrap();
        assert_eq!(secrets.get(&name).unwrap().as_deref(), Some("sk-two!"));
        assert!(dir.path().join("secrets.lock").exists());
        security_framework::passwords::delete_generic_password("foundry-test", &name).unwrap();
    }

    /// The store every machine uses is `secrets.json` under home. The file exists before the
    /// call, so on macOS nothing is moved and the real keychain is never touched.
    #[test]
    fn this_machine_keeps_secrets_in_secrets_json_under_home() {
        let dir = tempfile::tempdir().unwrap();
        let held = FileSecrets::new(dir.path().join("secrets.json"));
        held.set("gateway", "sk-one").unwrap();
        let store = for_this_machine(dir.path()).unwrap();
        assert_eq!(store.get("gateway").unwrap().as_deref(), Some("sk-one"));
        store.set("gateway-management", "sk-two").unwrap();
        assert_eq!(
            held.get("gateway-management").unwrap().as_deref(),
            Some("sk-two")
        );
    }

    /// On macOS as on Linux, a `secrets.json` others can read or write is refused at read.
    #[test]
    fn this_machine_refuses_a_secrets_json_others_can_read() {
        for wider in [0o640, 0o604, 0o644] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("secrets.json");
            FileSecrets::new(&path).set("gateway", "sk-one").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(wider)).unwrap();
            let store = for_this_machine(dir.path()).unwrap();
            let error = store.get("gateway").unwrap_err();
            assert!(
                matches!(error, SecretsError::TooOpen { .. }),
                "{wider:o}: {error}"
            );
        }
    }

    /// A keychain stand-in whose reads of `failing` fail, as a denied keychain prompt does.
    struct Denying {
        inner: MemorySecrets,
        failing: &'static str,
    }

    impl OldStore for Denying {
        fn read(&self, name: &str) -> Result<Option<String>, SecretsError> {
            if name == self.failing {
                return Err(SecretsError::Keychain {
                    action: "read",
                    name: name.to_owned(),
                    reason: "denied".into(),
                });
            }
            OldStore::read(&self.inner, name)
        }

        fn remove(&self, name: &str) -> Result<(), SecretsError> {
            OldStore::remove(&self.inner, name)
        }
    }

    const MOVED: [&str; 4] = ["machine", "machine-keys", GATEWAY, GATEWAY_MANAGEMENT];

    fn old_store(names: &[&str]) -> MemorySecrets {
        let old = MemorySecrets::default();
        for name in names {
            Secrets::set(&old, name, &format!("value-of-{name}")).unwrap();
        }
        old
    }

    fn held_in(old: &MemorySecrets) -> Vec<String> {
        old.lock().keys().cloned().collect()
    }

    #[test]
    fn the_move_takes_all_four_secrets_and_removes_them_from_the_keychain() {
        let (dir, new) = file();
        let old = old_store(&MOVED);
        Secrets::set(&old, "unrelated", "kept").unwrap();
        move_secrets(&old, &new).unwrap();
        for name in MOVED {
            let expected = format!("value-of-{name}");
            assert_eq!(new.get(name).unwrap().as_deref(), Some(&*expected), "{name}");
        }
        assert_eq!(held_in(&old), ["unrelated"]);
        assert_eq!(new.get("unrelated").unwrap(), None);
        assert_eq!(mode(&dir.path().join("secrets.json")), 0o600);
    }

    #[test]
    fn the_move_takes_only_the_secrets_the_keychain_holds() {
        let (_dir, new) = file();
        let old = old_store(&["machine", GATEWAY]);
        move_secrets(&old, &new).unwrap();
        assert_eq!(new.get("machine").unwrap().as_deref(), Some("value-of-machine"));
        assert_eq!(new.get(GATEWAY).unwrap().as_deref(), Some("value-of-gateway"));
        assert_eq!(new.get("machine-keys").unwrap(), None);
        assert_eq!(new.get(GATEWAY_MANAGEMENT).unwrap(), None);
        assert!(held_in(&old).is_empty());
    }

    #[test]
    fn the_move_writes_no_file_when_the_keychain_holds_none() {
        let (dir, new) = file();
        move_secrets(&MemorySecrets::default(), &new).unwrap();
        assert!(!dir.path().join("secrets.json").exists());
    }

    #[test]
    fn a_failed_read_moves_nothing_and_names_the_secret_not_its_value() {
        for failing in MOVED {
            let (dir, new) = file();
            let old = Denying {
                inner: old_store(&MOVED),
                failing,
            };
            let error = move_secrets(&old, &new).unwrap_err();
            assert!(error.to_string().contains(failing), "{error}");
            assert!(!error.to_string().contains("value-of-"), "{error}");
            assert!(!dir.path().join("secrets.json").exists(), "{failing}");
            assert_eq!(held_in(&old.inner), held_in(&old_store(&MOVED)), "{failing}");
        }
    }

    #[test]
    fn an_existing_secrets_json_is_left_alone_and_stops_the_move() {
        let (dir, new) = file();
        new.set(GATEWAY, "sk-file").unwrap();
        let before = fs::read(dir.path().join("secrets.json")).unwrap();
        let old = old_store(&MOVED);
        move_secrets(&old, &new).unwrap();
        assert_eq!(fs::read(dir.path().join("secrets.json")).unwrap(), before);
        assert_eq!(held_in(&old), held_in(&old_store(&MOVED)));
    }

    #[test]
    fn the_move_makes_a_missing_data_directory_private() {
        let parent = tempfile::tempdir().unwrap();
        let home = parent.path().join("home");
        let new = FileSecrets::new(home.join("secrets.json"));
        move_secrets(&old_store(&[GATEWAY]), &new).unwrap();
        assert_eq!(mode(&home), 0o700);
        assert_eq!(mode(&home.join("secrets.json")), 0o600);
    }

    /// `foundryd secret set` can run before the first `foundryd run`, which refuses a data
    /// directory other users can reach.
    #[test]
    fn a_set_makes_a_missing_data_directory_private() {
        let parent = tempfile::tempdir().unwrap();
        let home = parent.path().join("a/home");
        FileSecrets::new(home.join("secrets.json"))
            .set("gateway", "sk-one")
            .unwrap();
        assert_eq!(mode(&home), 0o700);
        assert_eq!(mode(&parent.path().join("a")), 0o700);
    }

    #[test]
    fn only_the_exact_word_file_skips_the_move() {
        use std::ffi::OsStr;
        assert!(skips_move(Some(OsStr::new("file")), true));
        for choice in [None, Some(OsStr::new("keychain")), Some(OsStr::new("File"))] {
            assert!(!skips_move(choice, true), "{choice:?}");
        }
    }

    /// A release build has no override, so a variable copied from the tests into a service
    /// file cannot stop a Mac moving its identity and keys out of the keychain.
    #[test]
    fn a_release_build_ignores_foundry_secrets() {
        use std::ffi::OsStr;
        for choice in [None, Some(OsStr::new("file"))] {
            assert!(!skips_move(choice, false), "{choice:?}");
        }
    }

    /// `FOUNDRY_SECRETS=file` in a debug build, the one the tests run, skips the move.
    #[cfg(debug_assertions)]
    #[test]
    fn this_debug_build_takes_foundry_secrets() {
        use std::ffi::OsStr;
        assert!(skips_move(Some(OsStr::new("file")), OVERRIDE));
    }

    /// The same in a release build, which ignores it. CI runs this one in a release build.
    #[cfg(not(debug_assertions))]
    #[test]
    fn this_release_build_ignores_foundry_secrets() {
        use std::ffi::OsStr;
        assert!(!OVERRIDE);
        assert!(!skips_move(Some(OsStr::new("file")), OVERRIDE));
    }

    /// What `set_from` stored under `name`, read back from a file store.
    fn stored(secrets: &FileSecrets, name: &str) -> Option<String> {
        secrets.get(name).unwrap()
    }

    #[test]
    fn set_from_stores_the_line_without_its_newline() {
        let (_dir, secrets) = file();
        set_from(&secrets, GATEWAY, "sk-one\n".as_bytes()).unwrap();
        set_from(&secrets, GATEWAY_MANAGEMENT, "sk-two\r\n".as_bytes()).unwrap();
        assert_eq!(stored(&secrets, "gateway").as_deref(), Some("sk-one"));
        assert_eq!(
            stored(&secrets, "gateway-management").as_deref(),
            Some("sk-two")
        );
    }

    #[test]
    fn set_from_keeps_only_the_first_line() {
        let (_dir, secrets) = file();
        set_from(&secrets, GATEWAY, "sk-one\nsk-two\n".as_bytes()).unwrap();
        assert_eq!(stored(&secrets, "gateway").as_deref(), Some("sk-one"));
    }

    #[test]
    fn set_from_refuses_an_empty_value_and_stores_nothing() {
        let (_dir, secrets) = file();
        for input in ["", "\n", "\r\n"] {
            let error = set_from(&secrets, GATEWAY, input.as_bytes()).unwrap_err();
            assert!(error.to_string().contains("no value on stdin"), "{error}");
        }
        assert_eq!(stored(&secrets, "gateway"), None);
    }

    #[test]
    fn set_from_refuses_any_other_name_and_stores_nothing() {
        let (_dir, secrets) = file();
        for name in ["machine", "machine-keys", "other"] {
            let error = set_from(&secrets, name, "x\n".as_bytes()).unwrap_err();
            let text = error.to_string();
            assert!(text.contains(name), "{text}");
            assert!(text.contains("gateway-management"), "{text}");
            assert_eq!(stored(&secrets, name), None);
        }
    }

    #[test]
    fn set_from_refuses_a_bad_name_before_it_reads_stdin() {
        struct Unreadable;
        impl io::Read for Unreadable {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                panic!("stdin was read");
            }
        }
        let secrets = MemorySecrets::default();
        let input = io::BufReader::new(Unreadable);
        assert!(set_from(&secrets, "machine", input).is_err());
    }
}
