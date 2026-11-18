//! Secrets in the macOS keychain or a 0600 file on Linux.
//!
//! [`Secrets`] is the one interface: [`Keychain`] backs it on macOS and [`FileSecrets`] on
//! Linux, and [`for_this_machine`] picks the one this machine uses. A secret's value never
//! reaches an error message: errors name the secret or the file, never what it holds.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(test)]
use std::sync::{Mutex, PoisonError};

use nix::fcntl::{Flock, FlockArg};

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
    #[error("the keychain could not {action} the secret {name}: {reason}")]
    Keychain {
        action: &'static str,
        name: String,
        reason: String,
    },
}

/// The secret store this machine uses: the login keychain on macOS, its updates locked by
/// the file `secrets.lock` in `home`, and on Linux the file `secrets.json` in `home`, readable
/// only by the service user.
pub fn for_this_machine(home: &Path) -> Arc<dyn Secrets> {
    #[cfg(target_os = "macos")]
    {
        Arc::new(Keychain::new("foundry", home.join("secrets.lock")))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Arc::new(FileSecrets::new(home.join("secrets.json")))
    }
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

    /// Takes the lock every set and update holds. It is released when the value is dropped.
    fn lock(&self) -> Result<Flock<File>, SecretsError> {
        lock_file(&self.beside(".lock")).map_err(|e| self.io(e))
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

/// An exclusive lock on the file at `path`, made 0600 with its directory if needed, held until
/// the value is dropped. It is a `flock`, so it holds between processes, and between two opens
/// of the file in one process.
fn lock_file(path: &Path) -> io::Result<Flock<File>> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    Flock::lock(file, FlockArg::LockExclusive).map_err(|(_, errno)| errno.into())
}

/// Secrets as generic passwords in the login keychain, one per name, under `service`. The
/// keychain has no lock of its own, so a set or an update holds an exclusive lock on the file
/// `lock` from its read to its write.
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
        let _locked = lock_file(&self.lock).map_err(|source| SecretsError::Io {
            path: self.lock.clone(),
            source,
        })?;
        let Some(value) = edit(self.get(name)?) else {
            return Ok(());
        };
        security_framework::passwords::set_generic_password(&self.service, name, value.as_bytes())
            .map_err(|error| SecretsError::Keychain {
                action: "store",
                name: name.to_owned(),
                reason: error.to_string(),
            })
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
}
