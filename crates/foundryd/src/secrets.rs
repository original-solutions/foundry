//! Secrets in a 0600 file, on every OS.
//!
//! [`Secrets`] is the one interface and [`FileSecrets`] backs it: [`for_this_machine`] gives
//! the file `secrets.json` in the data directory. Earlier releases kept a Mac's secrets in the
//! login keychain. The first open on such a Mac with no update on trial copies them into the
//! file; none is made while an update is on trial, so an automatic rollback never runs an older
//! release on a keychain the file has moved on from. The keychain items are
//! never deleted, so a release that reads only the keychain still finds them. A secret's value
//! never reaches an error message: errors name the secret or the file, never what it holds.

use std::collections::BTreeMap;
#[cfg(any(target_os = "macos", test))]
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
/// waits on; one behind a holder that is stuck gives up.
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
    /// The names of the secrets held, sorted, and never a value. The default, for a store that
    /// cannot list its names, gives [`SecretsError::CannotList`]; a store that can list but
    /// fails to read gives its read error. Neither is ever taken for an empty store:
    /// [`hide_known`] then fails closed and the held record writes nothing.
    fn names(&self) -> Result<Vec<String>, SecretsError> {
        Err(SecretsError::CannotList)
    }
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
    #[error(
        "the secrets are still in the login keychain, and they are not copied while an update is \
         on trial; once that update has rolled back, install this version by hand and answer the \
         keychain prompts (docs/install.md)"
    )]
    NotMoved,
    #[error("this secret store cannot list its names")]
    CannotList,
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

/// The prefix of the secret a GitHub token is held under: `github:<login>`.
pub const GITHUB_PREFIX: &str = "github:";

/// The secret the machine's default GitHub login is held under. It holds a login, not a token.
pub const GITHUB_DEFAULT: &str = "github-default";

/// What [`hide_known`] gives when the secret store cannot be read: nothing of the text can be
/// told safe, so none of it is shown.
pub const HIDDEN_UNREADABLE: &str = "[hidden: the secret store could not be read]";

/// The longest GitHub login. A 40-character string is a legacy token, never a login.
const LOGIN_MAX: usize = 39;

/// Whether `login` is a GitHub login as foundry takes it: letters, digits and `-`, not empty and
/// at most 39 characters (GitHub's own limit).
pub fn is_login(login: &str) -> bool {
    !login.is_empty()
        && login.len() <= LOGIN_MAX
        && login.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// The logins a GitHub token is held for, sorted. A `github:` name whose rest is no login is
/// not listed.
pub fn github_logins(secrets: &dyn Secrets) -> Result<Vec<String>, SecretsError> {
    let mut logins: Vec<String> = secrets
        .names()?
        .iter()
        .filter_map(|name| name.strip_prefix(GITHUB_PREFIX))
        .filter(|login| is_login(login))
        .map(str::to_owned)
        .collect();
    logins.sort();
    Ok(logins)
}

/// The token held for `login`: `None` when none is set, and for a `login` that is none, which
/// reads nothing.
pub fn github_token(secrets: &dyn Secrets, login: &str) -> Result<Option<String>, SecretsError> {
    if !is_login(login) {
        return Ok(None);
    }
    secrets.get(&format!("{GITHUB_PREFIX}{login}"))
}

/// This machine's default GitHub login: `None` when none is stored, when what is stored is no
/// login, and when the store cannot be read (which is logged).
pub fn github_default(secrets: &dyn Secrets) -> Option<String> {
    match secrets.get(GITHUB_DEFAULT) {
        Ok(login) => login.filter(|login| is_login(login)),
        Err(error) => {
            tracing::warn!(%error, "the default GitHub login could not be read");
            None
        }
    }
}

/// `text` with the gateway keys and every `github:*` token this store holds now hidden as `***`
/// (whatever follows `github:`, a login or not), read at this call so a token set after the
/// daemon started is hidden too, and then the passwords of URLs as [`redact`] hides them. When
/// the store cannot be read, none of `text` is shown: [`HIDDEN_UNREADABLE`] is given instead.
pub fn hide_known(text: &str, secrets: &dyn Secrets) -> String {
    match known_values(secrets) {
        Ok(mut values) => {
            // The longest first, so a token that is the start of another leaves no tail of it.
            values.sort_by_key(|value| std::cmp::Reverse(value.len()));
            let mut hidden = text.to_owned();
            for value in values {
                hidden = hidden.replace(&value, "***");
            }
            redact(&hidden)
        }
        Err(error) => {
            tracing::warn!(%error, "text hidden: the secret store could not be read");
            HIDDEN_UNREADABLE.to_owned()
        }
    }
}

/// The values of the secrets `hide_known` hides, none of them empty.
fn known_values(secrets: &dyn Secrets) -> Result<Vec<String>, SecretsError> {
    let mut names = vec![GATEWAY.to_owned(), GATEWAY_MANAGEMENT.to_owned()];
    names.extend(
        secrets
            .names()?
            .into_iter()
            .filter(|name| name.starts_with(GITHUB_PREFIX)),
    );
    let mut values = Vec::new();
    for name in names {
        if let Some(value) = secrets.get(&name)?.filter(|value| !value.is_empty()) {
            values.push(value);
        }
    }
    Ok(values)
}

/// The secret store this machine uses: the file `secrets.json` in `home`, readable only by the
/// service user, on every OS.
///
/// On macOS, when that file does not exist, the secrets a keychain-era release stored are first
/// copied into it from the login keychain ([`move_secrets`]), except while an update is on
/// trial, which gives [`SecretsError::NotMoved`] instead. Each
/// keychain read can bring up a prompt, which this call waits on. A failed read gives an error
/// and writes nothing, so the caller stops rather than start with an empty store, and the next
/// start tries again. The keychain items are left as they were. In a debug build
/// `FOUNDRY_SECRETS=file` skips the keychain, which is how tests keep out of it. A release
/// build ignores it.
pub fn for_this_machine(home: &Path) -> Result<Arc<dyn Secrets>, SecretsError> {
    open(home, keychain().as_deref())
}

/// Whether `path` exists; an error other than its absence is passed on.
fn exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// The keychain a keychain-era release kept a Mac's secrets in: on macOS, unless this debug
/// run skips it with `FOUNDRY_SECRETS=file`; elsewhere none.
fn keychain() -> Option<Box<dyn OldStore>> {
    #[cfg(target_os = "macos")]
    if !skips_keychain(std::env::var_os("FOUNDRY_SECRETS").as_deref(), OVERRIDE) {
        return Some(Box::new(Keychain { service: "foundry" }));
    }
    None
}

/// The file store `secrets.json` in `home`, after [`move_secrets`] from `old` when there is
/// one. While an update is on trial ([`crate::update::trial_on`]) `old` is never read: the file
/// is used when an earlier copy made it, and the open is refused otherwise. The daemon,
/// `foundryd join` and `foundryd secret set` all open the store here, so none of them copies
/// during a trial, and the release a rollback goes back to finds the keychain still the one
/// store.
fn open(home: &Path, old: Option<&dyn OldStore>) -> Result<Arc<dyn Secrets>, SecretsError> {
    let secrets = FileSecrets::new(home.join("secrets.json"));
    if let Some(old) = old {
        if crate::update::trial_on(home) {
            if !exists(&secrets.path).map_err(|e| secrets.io(e))? {
                return Err(SecretsError::NotMoved);
            }
        } else {
            // Every keychain write a keychain-era release makes holds this lock, so none lands
            // between the copy's reads and the file it writes.
            let _locked = lock_file(&home.join("secrets.lock"), LOCK_WAIT)?;
            move_secrets(old, &secrets)?;
        }
    }
    Ok(Arc::new(secrets))
}

/// Whether this build takes `FOUNDRY_SECRETS`: only a debug build, so a release Mac always
/// copies its identity and keys out of the keychain.
#[cfg(any(target_os = "macos", test))]
const OVERRIDE: bool = cfg!(debug_assertions);

/// Whether a `FOUNDRY_SECRETS` value skips the keychain: only exactly `file`, and only when
/// `taken` (the build takes the override).
#[cfg(any(target_os = "macos", test))]
fn skips_keychain(choice: Option<&OsStr>, taken: bool) -> bool {
    taken && choice == Some(OsStr::new("file"))
}

/// The names a keychain-era release stored: the machine's identity, its key table and the two
/// gateway keys.
const KEYCHAIN_NAMES: [&str; 4] = [
    crate::join::IDENTITY_SECRET,
    crate::join::KEYS_SECRET,
    GATEWAY,
    GATEWAY_MANAGEMENT,
];

/// A store secrets are copied out of, as far as the copy needs it: the login keychain, or a
/// fake in tests. It is only read.
trait OldStore {
    /// The secret called `name`, or `None` when the store holds none.
    fn read(&self, name: &str) -> Result<Option<String>, SecretsError>;
}

/// Copies every one of [`KEYCHAIN_NAMES`] that `old` holds into `new`, once: only while `new`'s
/// file does not exist, and under its lock, so two processes starting at once copy them once.
/// Every read comes first; any failure returns before anything is written, and the secrets are
/// written in one go. `old` is left as it was. When `old` holds none, nothing is written.
fn move_secrets(old: &dyn OldStore, new: &FileSecrets) -> Result<(), SecretsError> {
    let _locked = new.lock()?;
    if exists(&new.path).map_err(|e| new.io(e))? {
        return Ok(());
    }
    let mut found = BTreeMap::new();
    for name in KEYCHAIN_NAMES {
        if let Some(value) = old.read(name)? {
            found.insert(name.to_owned(), value);
        }
    }
    if found.is_empty() {
        return Ok(());
    }
    new.write(&found)
}

#[derive(Debug, thiserror::Error)]
pub enum SetError {
    #[error(
        "the name is not a secret to set here; the names are {GATEWAY}, {GATEWAY_MANAGEMENT} and \
         {GITHUB_PREFIX}<login>, where a login is letters, digits and -, at most 39 characters"
    )]
    // No field: the refused name may be a token pasted in place of a login, so it is never quoted.
    UnknownName,
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
        Some(home) => for_this_machine(&home)
            .map_err(SetError::from)
            .and_then(|secrets| set_from(&*secrets, name, io::BufReader::new(io::stdin()))),
        None => {
            eprintln!("foundryd secret set: set FOUNDRY_HOME or HOME");
            return ExitCode::FAILURE;
        }
    };
    match result {
        Ok(()) => {
            if name.starts_with(GITHUB_PREFIX) {
                println!("foundryd secret set {name}: stored; no restart is needed");
            } else {
                println!(
                    "foundryd secret set {name}: stored; restart foundryd so new sessions use it"
                );
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("foundryd secret set: {error}");
            ExitCode::FAILURE
        }
    }
}

/// `foundryd github default <login>`: stores `login` as this machine's default GitHub login.
/// A `login` that is none is refused and nothing is stored.
pub fn github_default_command(login: &str) -> ExitCode {
    if !is_login(login) {
        // The argument is not echoed: it may be a token pasted in place of a login.
        eprintln!(
            "foundryd github default: the argument is not a GitHub login; a login is letters, \
             digits and -, at most 39 characters"
        );
        return ExitCode::FAILURE;
    }
    let Some(home) = crate::daemon::home() else {
        eprintln!("foundryd github default: set FOUNDRY_HOME or HOME");
        return ExitCode::FAILURE;
    };
    match for_this_machine(&home).and_then(|secrets| secrets.set(GITHUB_DEFAULT, login)) {
        Ok(()) => {
            println!("foundryd github default: {login} is this machine's default login");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("foundryd github default: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Stores the first line of `input` as the secret `name`, without its line ending. Only the
/// gateway secrets and `github:<login>` tokens can be set: the name is checked before `input` is
/// read, so any other name leaves the store untouched, and the machine's own identity and key
/// table stay out of reach.
pub fn set_from(
    secrets: &dyn Secrets,
    name: &str,
    mut input: impl BufRead,
) -> Result<(), SetError> {
    let github = name.strip_prefix(GITHUB_PREFIX).is_some_and(is_login);
    if name != GATEWAY && name != GATEWAY_MANAGEMENT && !github {
        return Err(SetError::UnknownName);
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

    fn names(&self) -> Result<Vec<String>, SecretsError> {
        Ok(self.read()?.into_keys().collect())
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

/// The generic passwords a keychain-era release stored in the login keychain, one per name,
/// under `service`: only read, by [`move_secrets`].
#[cfg(target_os = "macos")]
struct Keychain {
    service: &'static str,
}

/// `errSecItemNotFound`: the keychain holds no such item.
#[cfg(target_os = "macos")]
const NOT_FOUND: i32 = -25300;

#[cfg(target_os = "macos")]
impl OldStore for Keychain {
    fn read(&self, name: &str) -> Result<Option<String>, SecretsError> {
        let failed = |reason: String| SecretsError::Keychain {
            action: "read",
            name: name.to_owned(),
            reason,
        };
        match security_framework::passwords::get_generic_password(self.service, name) {
            Ok(bytes) => String::from_utf8(bytes)
                .map(Some)
                .map_err(|_| failed("it is not UTF-8".into())),
            Err(error) if error.code() == NOT_FOUND => Ok(None),
            Err(error) => Err(failed(error.to_string())),
        }
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

    fn names(&self) -> Result<Vec<String>, SecretsError> {
        Ok(self.lock().keys().cloned().collect())
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
    fn is_login_takes_39_characters_and_refuses_40() {
        assert!(is_login(&"a".repeat(39)));
        assert!(!is_login(&"a".repeat(40)));
    }

    #[test]
    fn set_from_refuses_a_40_hex_login_before_reading_stdin() {
        struct NoRead;
        impl io::Read for NoRead {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                panic!("stdin was read");
            }
        }
        let (_dir, secrets) = file();
        let name = format!(
            "{GITHUB_PREFIX}{}",
            "0123456789abcdef".repeat(3).get(..40).unwrap()
        );
        let result = set_from(&secrets, &name, io::BufReader::new(NoRead));
        assert!(matches!(result, Err(SetError::UnknownName)), "{result:?}");
        assert_eq!(secrets.names().unwrap(), Vec::<String>::new());
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
    fn the_keychain_reads_what_a_keychain_era_release_stored() {
        use security_framework::passwords::{delete_generic_password, set_generic_password};
        let keychain = Keychain {
            service: "foundry-test",
        };
        let name = format!("probe-{}", uuid::Uuid::new_v4());
        assert_eq!(keychain.read(&name).unwrap(), None);
        set_generic_password("foundry-test", &name, b"sk-one").unwrap();
        assert_eq!(keychain.read(&name).unwrap().as_deref(), Some("sk-one"));
        delete_generic_password("foundry-test", &name).unwrap();
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

    impl Denying {
        fn reads(inner: MemorySecrets, failing: &'static str) -> Self {
            Self { inner, failing }
        }
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
    }

    /// The names are the ones `join` and the gateway keep their secrets under.
    #[test]
    fn the_names_copied_are_the_four_foundry_stores() {
        assert_eq!(
            KEYCHAIN_NAMES,
            ["machine", "machine-keys", "gateway", "gateway-management"]
        );
    }

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

    /// The move only copies: a keychain-era release, after a rollback or a go-back by hand,
    /// must still find its identity and keys in the keychain.
    #[test]
    fn the_move_copies_all_four_secrets_and_leaves_the_keychain_as_it_was() {
        let (dir, new) = file();
        let old = old_store(&KEYCHAIN_NAMES);
        Secrets::set(&old, "unrelated", "kept").unwrap();
        move_secrets(&old, &new).unwrap();
        for name in KEYCHAIN_NAMES {
            let expected = format!("value-of-{name}");
            assert_eq!(
                new.get(name).unwrap().as_deref(),
                Some(&*expected),
                "{name}"
            );
        }
        let mut all = KEYCHAIN_NAMES.map(str::to_owned).to_vec();
        all.push("unrelated".into());
        all.sort();
        assert_eq!(held_in(&old), all);
        assert_eq!(new.get("unrelated").unwrap(), None);
        assert_eq!(mode(&dir.path().join("secrets.json")), 0o600);
    }

    #[test]
    fn the_move_takes_only_the_secrets_the_keychain_holds() {
        let (_dir, new) = file();
        let old = old_store(&["machine", GATEWAY]);
        move_secrets(&old, &new).unwrap();
        assert_eq!(
            new.get("machine").unwrap().as_deref(),
            Some("value-of-machine")
        );
        assert_eq!(
            new.get(GATEWAY).unwrap().as_deref(),
            Some("value-of-gateway")
        );
        assert_eq!(new.get("machine-keys").unwrap(), None);
        assert_eq!(new.get(GATEWAY_MANAGEMENT).unwrap(), None);
        assert_eq!(held_in(&old), ["gateway", "machine"]);
    }

    #[test]
    fn the_move_writes_no_file_when_the_keychain_holds_none() {
        let (dir, new) = file();
        move_secrets(&MemorySecrets::default(), &new).unwrap();
        assert!(!dir.path().join("secrets.json").exists());
    }

    #[test]
    fn a_failed_read_moves_nothing_and_names_the_secret_not_its_value() {
        for failing in KEYCHAIN_NAMES {
            let (dir, new) = file();
            let old = Denying::reads(old_store(&KEYCHAIN_NAMES), failing);
            let error = move_secrets(&old, &new).unwrap_err();
            assert!(error.to_string().contains(failing), "{error}");
            assert!(!error.to_string().contains("value-of-"), "{error}");
            assert!(!dir.path().join("secrets.json").exists(), "{failing}");
            assert_eq!(
                held_in(&old.inner),
                held_in(&old_store(&KEYCHAIN_NAMES)),
                "{failing}"
            );
        }
    }

    #[test]
    fn an_existing_secrets_json_is_left_alone_and_stops_the_move() {
        let (dir, new) = file();
        new.set(GATEWAY, "sk-file").unwrap();
        let before = fs::read(dir.path().join("secrets.json")).unwrap();
        let old = old_store(&KEYCHAIN_NAMES);
        move_secrets(&old, &new).unwrap();
        assert_eq!(fs::read(dir.path().join("secrets.json")).unwrap(), before);
        assert_eq!(held_in(&old), held_in(&old_store(&KEYCHAIN_NAMES)));
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

    #[test]
    fn a_failed_write_gives_the_error_and_leaves_the_keychain_as_it_was() {
        let (dir, new) = file();
        // A directory where the temporary file goes: removing it fails, so the write does.
        fs::create_dir(dir.path().join("secrets.json.tmp")).unwrap();
        let old = old_store(&KEYCHAIN_NAMES);
        assert!(move_secrets(&old, &new).is_err());
        assert!(!dir.path().join("secrets.json").exists());
        assert_eq!(held_in(&old), held_in(&old_store(&KEYCHAIN_NAMES)));
    }

    #[test]
    fn the_move_waits_on_the_secrets_lock_and_moves_nothing_while_another_holds_it() {
        let (dir, new) = file();
        let held = lock_file(&dir.path().join("secrets.json.lock"), LOCK_WAIT).unwrap();
        let old = old_store(&KEYCHAIN_NAMES);
        let started = Instant::now();
        let error = move_secrets(&old, &new).unwrap_err();
        assert!(started.elapsed() >= LOCK_WAIT);
        assert!(matches!(error, SecretsError::Locked { .. }), "{error:?}");
        assert!(!dir.path().join("secrets.json").exists());
        drop(held);
    }

    #[test]
    fn opening_with_a_keychain_and_no_file_copies_its_secrets_into_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let old = old_store(&KEYCHAIN_NAMES);
        let store = open(dir.path(), Some(&old)).unwrap();
        assert_eq!(
            store.get("machine").unwrap().as_deref(),
            Some("value-of-machine")
        );
        let file = FileSecrets::new(dir.path().join("secrets.json"));
        assert_eq!(
            file.get("machine-keys").unwrap().as_deref(),
            Some("value-of-machine-keys")
        );
    }

    /// The daemon, `join` and `secret set` all stop on this error rather than start with an
    /// empty store and make a new identity.
    #[test]
    fn opening_when_a_keychain_read_fails_gives_the_error_and_writes_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let old = Denying::reads(old_store(&KEYCHAIN_NAMES), "machine");
        let error = match open(dir.path(), Some(&old)) {
            Ok(_) => panic!("opened a store although the keychain read failed"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("machine"), "{error}");
        assert!(!dir.path().join("secrets.json").exists());
    }

    #[test]
    fn opening_with_no_keychain_moves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = open(dir.path(), None).unwrap();
        assert_eq!(store.get("machine").unwrap(), None);
        assert!(!dir.path().join("secrets.json").exists());
    }

    /// A keychain stand-in that must not be read at all.
    struct Untouchable;

    impl OldStore for Untouchable {
        fn read(&self, name: &str) -> Result<Option<String>, SecretsError> {
            panic!("{name} was read from the keychain");
        }
    }

    /// `dir` with an update on trial, as `update.json` records one.
    fn on_trial(dir: &Path) {
        fs::write(
            dir.join("update.json"),
            r#"{"trial": {"version": "0.2.0", "previous": "0.1.0", "starts": 1}}"#,
        )
        .unwrap();
    }

    /// No copy is made during a trial, so an automatic rollback never leaves an older release
    /// on a keychain that the file has moved on from.
    #[test]
    fn during_a_trial_the_keychain_is_never_read_and_the_open_is_refused_with_no_file() {
        let dir = tempfile::tempdir().unwrap();
        on_trial(dir.path());

        let error = open(dir.path(), Some(&Untouchable))
            .err()
            .expect("no copy during a trial");

        assert!(matches!(error, SecretsError::NotMoved), "{error}");
        assert!(!dir.path().join("secrets.json").exists());
    }

    #[test]
    fn during_a_trial_the_file_an_earlier_copy_made_is_used() {
        let dir = tempfile::tempdir().unwrap();
        on_trial(dir.path());
        FileSecrets::new(dir.path().join("secrets.json"))
            .set(GATEWAY, "sk-copied")
            .unwrap();

        let store = open(dir.path(), Some(&Untouchable)).unwrap();

        assert_eq!(store.get(GATEWAY).unwrap().as_deref(), Some("sk-copied"));
    }

    /// A machine with no keychain, such as Linux, takes its updates with no file yet.
    #[test]
    fn during_a_trial_a_machine_with_no_keychain_opens_with_no_file() {
        let dir = tempfile::tempdir().unwrap();
        on_trial(dir.path());

        let store = open(dir.path(), None).unwrap();

        assert_eq!(store.get("machine").unwrap(), None);
        assert!(!dir.path().join("secrets.json").exists());
    }

    /// When it cannot tell whether a trial is on, it does not copy.
    #[test]
    fn an_unreadable_update_state_refuses_the_copy() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("update.json"), "not json").unwrap();

        let error = open(dir.path(), Some(&Untouchable))
            .err()
            .expect("no copy when the trial cannot be read");

        assert!(matches!(error, SecretsError::NotMoved), "{error}");
    }

    /// A keychain-era release's keychain writes hold `secrets.lock`, so the copy waits on it.
    #[test]
    fn the_copy_waits_on_the_keychain_era_lock_and_copies_nothing_while_another_holds_it() {
        let dir = tempfile::tempdir().unwrap();
        let held = lock_file(&dir.path().join("secrets.lock"), LOCK_WAIT).unwrap();

        let error = open(dir.path(), Some(&old_store(&KEYCHAIN_NAMES)))
            .err()
            .expect("the lock is held");

        assert!(matches!(error, SecretsError::Locked { .. }), "{error:?}");
        assert!(!dir.path().join("secrets.json").exists());
        drop(held);
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
    fn only_the_exact_word_file_skips_the_keychain() {
        use std::ffi::OsStr;
        assert!(skips_keychain(Some(OsStr::new("file")), true));
        for choice in [None, Some(OsStr::new("keychain")), Some(OsStr::new("File"))] {
            assert!(!skips_keychain(choice, true), "{choice:?}");
        }
    }

    /// A release build has no override, so a variable copied from the tests into a service
    /// file cannot stop a Mac moving its identity and keys out of the keychain.
    #[test]
    fn a_release_build_ignores_foundry_secrets() {
        use std::ffi::OsStr;
        for choice in [None, Some(OsStr::new("file"))] {
            assert!(!skips_keychain(choice, false), "{choice:?}");
        }
    }

    /// `FOUNDRY_SECRETS=file` in a debug build, the one the tests run, skips the move.
    #[cfg(debug_assertions)]
    #[test]
    fn this_debug_build_takes_foundry_secrets() {
        use std::ffi::OsStr;
        assert!(skips_keychain(Some(OsStr::new("file")), OVERRIDE));
    }

    /// The same in a release build, which ignores it. CI runs this one in a release build.
    #[cfg(not(debug_assertions))]
    #[test]
    fn this_release_build_ignores_foundry_secrets() {
        use std::ffi::OsStr;
        assert!(!OVERRIDE);
        assert!(!skips_keychain(Some(OsStr::new("file")), OVERRIDE));
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
            assert!(!text.contains(name), "{text}");
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

    #[test]
    fn set_from_accepts_a_github_login() {
        let (_dir, secrets) = file();
        for login in ["octocat", "a-b-1", "A9"] {
            let name = format!("github:{login}");
            set_from(&secrets, &name, "ghp_x\n".as_bytes()).unwrap();
            assert_eq!(stored(&secrets, &name).as_deref(), Some("ghp_x"), "{name}");
        }
    }

    #[test]
    fn set_from_refuses_a_bad_login_before_it_reads_stdin() {
        struct Unreadable;
        impl io::Read for Unreadable {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                panic!("stdin was read");
            }
        }
        let (_dir, secrets) = file();
        for name in [
            "github:bad/name",
            "github:",
            "github:\u{fc}",
            "github:a b",
            "github-default",
        ] {
            let input = io::BufReader::new(Unreadable);
            let error = set_from(&secrets, name, input).unwrap_err();
            assert!(matches!(error, SetError::UnknownName), "{name}");
            assert!(error.to_string().contains("github:<login>"), "{error}");
            if name != "github:" {
                assert!(!error.to_string().contains(name), "{error}");
            }
            assert_eq!(stored(&secrets, name), None, "{name}");
        }
        assert!(secrets.names().unwrap().is_empty());
    }

    #[test]
    fn names_lists_what_was_set_sorted_and_no_value() {
        let (_dir, secrets) = file();
        assert!(secrets.names().unwrap().is_empty());
        secrets.set("gateway", "value-one").unwrap();
        secrets.set("github:octocat", "value-two").unwrap();
        secrets.set("github:alice", "value-three").unwrap();
        let names = secrets.names().unwrap();
        assert_eq!(names, ["gateway", "github:alice", "github:octocat"]);
        let text = names.join(" ");
        assert!(!text.contains("value-"), "{text}");
    }

    #[test]
    fn names_of_an_open_file_is_an_error_not_an_empty_list() {
        let (dir, secrets) = file();
        secrets.set("gateway", "sk-one").unwrap();
        let path = dir.path().join("secrets.json");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            secrets.names().unwrap_err(),
            SecretsError::TooOpen { .. }
        ));
    }

    #[test]
    fn the_in_memory_fake_lists_its_names() {
        let secrets = MemorySecrets::default();
        secrets.set("b", "1").unwrap();
        secrets.set("a", "2").unwrap();
        assert_eq!(secrets.names().unwrap(), ["a", "b"]);
    }

    #[test]
    fn github_logins_lists_only_valid_github_names() {
        let (_dir, secrets) = file();
        for name in [
            "gateway",
            "github:zed",
            "github:octocat",
            "github:bad/name",
            "github:",
            "github-default",
        ] {
            secrets.set(name, "v").unwrap();
        }
        assert_eq!(github_logins(&secrets).unwrap(), ["octocat", "zed"]);
    }

    #[test]
    fn github_token_gets_the_token_and_none_for_unset_or_invalid() {
        let (_dir, secrets) = file();
        secrets.set("github:octocat", "ghp_tok").unwrap();
        secrets.set("github:bad/name", "ghp_other").unwrap();
        assert_eq!(
            github_token(&secrets, "octocat").unwrap().as_deref(),
            Some("ghp_tok")
        );
        assert_eq!(github_token(&secrets, "nobody").unwrap(), None);
        assert_eq!(github_token(&secrets, "bad/name").unwrap(), None);
        assert_eq!(github_token(&secrets, "").unwrap(), None);
    }

    #[test]
    fn github_default_is_none_until_stored_then_the_login() {
        let (_dir, secrets) = file();
        assert_eq!(github_default(&secrets), None);
        secrets.set(GITHUB_DEFAULT, "octocat").unwrap();
        assert_eq!(github_default(&secrets).as_deref(), Some("octocat"));
    }

    #[test]
    fn github_default_is_none_for_a_stored_value_that_is_no_login() {
        let (_dir, secrets) = file();
        for value in ["bad/name", "", "ghp_a b"] {
            secrets.set(GITHUB_DEFAULT, value).unwrap();
            assert_eq!(github_default(&secrets), None, "{value:?}");
        }
    }

    #[test]
    fn github_default_is_not_a_name_secret_set_takes() {
        let (_dir, secrets) = file();
        assert!(set_from(&secrets, GITHUB_DEFAULT, "octocat\n".as_bytes()).is_err());
        assert_eq!(stored(&secrets, GITHUB_DEFAULT), None);
    }

    #[test]
    fn hide_known_hides_the_gateway_keys_and_every_github_token() {
        let (_dir, secrets) = file();
        secrets.set(GATEWAY, "gw-key-1").unwrap();
        secrets.set(GATEWAY_MANAGEMENT, "gm-key-2").unwrap();
        // The shorter token sorts first by name, so only the longest-first order hides both.
        secrets.set("github:alice", "ghp_abc").unwrap();
        secrets.set("github:zed", "ghp_abcdef").unwrap();
        let text = "a gw-key-1 b gm-key-2 c ghp_abc d ghp_abcdef e";
        assert_eq!(hide_known(text, &secrets), "a *** b *** c *** d *** e");
    }

    #[test]
    fn hide_known_hides_a_token_stored_under_a_name_that_is_no_login() {
        let (_dir, secrets) = file();
        secrets.set("github:bad/name", "ghp_odd_name").unwrap();
        assert_eq!(
            hide_known("carrying ghp_odd_name", &secrets),
            "carrying ***"
        );
    }

    /// A store that implements only what the trait requires.
    struct CannotList;
    impl Secrets for CannotList {
        fn get(&self, _: &str) -> Result<Option<String>, SecretsError> {
            Ok(None)
        }
        fn set(&self, _: &str, _: &str) -> Result<(), SecretsError> {
            Ok(())
        }
        fn update(&self, _: &str, _: Edit<'_>) -> Result<(), SecretsError> {
            Ok(())
        }
    }

    /// A store that lists a name but cannot read any value.
    struct CannotGet {
        fail_on: &'static str,
    }
    impl Secrets for CannotGet {
        fn get(&self, name: &str) -> Result<Option<String>, SecretsError> {
            if name == self.fail_on {
                Err(SecretsError::Unreadable {
                    path: "secrets.json".into(),
                })
            } else {
                Ok(None)
            }
        }
        fn set(&self, _: &str, _: &str) -> Result<(), SecretsError> {
            Ok(())
        }
        fn update(&self, _: &str, _: Edit<'_>) -> Result<(), SecretsError> {
            Ok(())
        }
        fn names(&self) -> Result<Vec<String>, SecretsError> {
            Ok(vec!["github:octocat".into()])
        }
    }

    #[test]
    fn hide_known_fails_closed_when_a_listed_secret_cannot_be_read() {
        for fail_on in ["github:octocat", GATEWAY] {
            let store = CannotGet { fail_on };
            assert_eq!(
                hide_known("carrying ghp_x", &store),
                HIDDEN_UNREADABLE,
                "{fail_on}"
            );
        }
    }

    #[test]
    fn a_store_that_cannot_list_fails_closed() {
        assert!(CannotList.names().is_err());
        assert_eq!(hide_known("carrying ghp_x", &CannotList), HIDDEN_UNREADABLE);
        assert!(github_logins(&CannotList).is_err());
    }

    #[test]
    fn hide_known_hides_a_token_set_after_the_store_was_opened() {
        let (dir, daemon) = file();
        daemon.set(GATEWAY, "gw-key-1").unwrap();
        let text = "pushing with ghp_late now";
        assert_eq!(hide_known(text, &daemon), text);
        let cli = FileSecrets::new(dir.path().join("secrets.json"));
        set_from(&cli, "github:octocat", "ghp_late\n".as_bytes()).unwrap();
        assert_eq!(hide_known(text, &daemon), "pushing with *** now");
    }

    #[test]
    fn hide_known_leaves_a_commit_id_and_a_pull_request_url_alone() {
        let (_dir, secrets) = file();
        secrets.set("github:octocat", "ghp_abc").unwrap();
        let commit = "0123456789abcdef0123456789abcdef01234567";
        let url = "https://github.com/original-solutions/foundry/pull/134";
        let text = format!("commit {commit} is in {url}");
        assert_eq!(commit.len(), 40);
        assert_eq!(hide_known(&text, &secrets), text);
    }

    #[test]
    fn hide_known_hides_a_url_password_as_redact_does() {
        let (_dir, secrets) = file();
        let text = "clone https://user:pw123@github.com/x/y.git";
        assert_eq!(hide_known(text, &secrets), redact(text));
        assert!(!hide_known(text, &secrets).contains("pw123"));
    }

    #[test]
    fn hide_known_with_an_empty_stored_value_hides_nothing() {
        let secrets = MemorySecrets::default();
        secrets.set(GATEWAY, "").unwrap();
        secrets.set("github:octocat", "").unwrap();
        assert_eq!(hide_known("plain text", &secrets), "plain text");
    }

    #[test]
    fn hide_known_with_an_unreadable_store_gives_the_fixed_text_never_the_input() {
        let logged = crate::test_log::Logged::everywhere();
        let (dir, secrets) = file();
        secrets.set("github:octocat", "ghp_unreadable").unwrap();
        let path = dir.path().join("secrets.json");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let text = "carrying ghp_unreadable";
        let hidden = hide_known(text, &secrets);
        assert_eq!(hidden, HIDDEN_UNREADABLE);
        assert!(!hidden.contains("ghp_unreadable"));
        let text = logged.text();
        assert!(text.contains("could not be read"), "{text}");
        assert!(!text.contains("ghp_unreadable"), "{text}");
    }
}
