//! A login's token for one git call, with credential helpers and hooks off.
//!
//! [`GitAuth::for_login`] reads the token held for a login and makes sure the askpass script
//! is in the data directory. [`GitAuth::apply`] gives one git child the token through that
//! script and turns off what could answer or act in its place: other credential helpers,
//! hooks, and the system and global config, as the runner's git hardening does. The token is
//! only ever in that child's environment. [`helpers_off_env`] is for a terminal, which gets no
//! token: it turns the machine's credential helpers off, so its keychain answers for nothing.

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::secrets::{self, Secrets, SecretsError};

/// The variable the askpass script reads the token from. It is set on one child only.
pub const TOKEN_VAR: &str = "FOUNDRY_GIT_TOKEN";

/// The askpass script's name in the data directory.
pub const ASKPASS: &str = "git-askpass";

/// The askpass script. git runs it with its prompt as the one argument and reads the answer
/// from its output. It holds no token: it prints the one in [`TOKEN_VAR`], which only the git
/// child it was set on and that child's own children have. GitHub takes a token as the
/// password whatever the user name, so the user name is a fixed one.
const SCRIPT: &str = "#!/bin/sh
# Written by foundryd for the git calls it gives a GitHub token to. It holds no token.
case \"$1\" in
Username*) printf '%s\\n' x-access-token ;;
*) printf '%s\\n' \"$FOUNDRY_GIT_TOKEN\" ;;
esac
";

/// The `-c` settings of every call: no credential helper but the askpass, no hooks, and the
/// rest of the runner's `HARDENED_GIT_CONFIG` (no fsmonitor, eol conversion, attributes or
/// excludes files, `ext` transport or alternate refs command).
const CONFIG: [&str; 8] = [
    // An empty value drops every helper named before it, in the system, global or repo config.
    "credential.helper=",
    "core.hooksPath=/dev/null",
    "core.fsmonitor=false",
    "core.autocrlf=false",
    "core.attributesFile=/dev/null",
    "core.excludesFile=/dev/null",
    "protocol.ext.allow=never",
    "core.alternateRefsCommand=",
];

/// The environment of every call, the runner's `HARDENED_GIT_ENV`: `-c` cannot turn the system
/// and global config off, and with no terminal prompt a refused token fails the call at once.
const ENV: [(&str, &str); 7] = [
    ("GIT_TERMINAL_PROMPT", "0"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_NO_REPLACE_OBJECTS", "1"),
    ("GIT_LFS_SKIP_SMUDGE", "1"),
    // A partial clone fetches a missing object by running `git fetch` with the repo's config.
    ("GIT_NO_LAZY_FETCH", "1"),
    // Git's messages in English, whatever the locale the daemon started in.
    ("LC_ALL", "C"),
];

/// Config the daemon's own environment may carry in from whatever started it. Each could name
/// a helper after the call's own `credential.helper=`, so none is passed on.
const INHERITED_CONFIG: [&str; 2] = ["GIT_CONFIG_PARAMETERS", "GIT_CONFIG_COUNT"];

/// The token for one login, ready to give to a git child. Its debug form shows the login and
/// never the token.
pub struct GitAuth {
    login: String,
    token: String,
    askpass: PathBuf,
}

/// Why a git call cannot have a login's token. No variant holds a token.
#[derive(Debug, thiserror::Error)]
pub enum NoToken {
    #[error("this machine holds no usable GitHub token for {login}")]
    Unset { login: String },
    #[error("the GitHub token for {login} could not be read: {source}")]
    Store { login: String, source: SecretsError },
    #[error("the git askpass script {} could not be written: {source}", path.display())]
    Askpass { path: PathBuf, source: io::Error },
}

impl GitAuth {
    /// The token held for `login` in `secrets`, read now, with the askpass script written to
    /// `data_dir` when it is not there as it should be. A login with no token, or one whose
    /// token is blank or holds a control character that an environment variable or git's
    /// answer could not carry whole, is [`NoToken::Unset`], and nothing is written.
    pub fn for_login(
        secrets: &dyn Secrets,
        login: &str,
        data_dir: &Path,
    ) -> Result<GitAuth, NoToken> {
        let unset = || NoToken::Unset {
            login: login.to_owned(),
        };
        let token = secrets::github_token(secrets, login)
            .map_err(|source| NoToken::Store {
                login: login.to_owned(),
                source,
            })?
            .ok_or_else(unset)?;
        let token = token.trim();
        if token.is_empty() || token.chars().any(char::is_control) {
            return Err(unset());
        }
        let askpass = data_dir.join(ASKPASS);
        write_askpass(data_dir, &askpass).map_err(|source| NoToken::Askpass {
            path: askpass.clone(),
            source,
        })?;
        Ok(GitAuth {
            login: login.to_owned(),
            token: token.to_owned(),
            askpass,
        })
    }

    /// The login the token is held for.
    pub fn login(&self) -> &str {
        &self.login
    }

    /// Gives `command`, a git call with no arguments yet, the token and the hardening: the
    /// `-c` settings go first, so the caller adds the subcommand and its arguments after this.
    /// `GIT_ASKPASS` names the script and [`TOKEN_VAR`] holds the token, on this command's
    /// environment only; the daemon's own is never changed. A tokio command takes it through
    /// `as_std_mut()`.
    pub fn apply(&self, command: &mut Command) {
        for setting in CONFIG {
            command.arg("-c").arg(setting);
        }
        for name in INHERITED_CONFIG {
            command.env_remove(name);
        }
        command
            .envs(ENV)
            .env("GIT_ASKPASS", &self.askpass)
            .env(TOKEN_VAR, &self.token);
    }
}

impl fmt::Debug for GitAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GitAuth")
            .field("login", &self.login)
            .field("askpass", &self.askpass)
            .finish_non_exhaustive()
    }
}

/// Writes [`SCRIPT`] to `path` in `dir`, 0700, unless a regular file with that mode and
/// content is there already. A new script goes to a file of its own in `dir` first and is
/// renamed over `path`, so a git call running the old one never sees half of it, and a symlink
/// at `path` is replaced, never followed.
fn write_askpass(dir: &Path, path: &Path) -> io::Result<()> {
    if let Ok(meta) = fs::symlink_metadata(path)
        && meta.file_type().is_file()
        && meta.permissions().mode() & 0o7777 == 0o700
        && fs::read(path).is_ok_and(|held| held == SCRIPT.as_bytes())
    {
        return Ok(());
    }
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let staged = dir.join(format!(
        ".{ASKPASS}.{}.{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let written = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o700)
            .open(&staged)?;
        // The mode a umask may have narrowed, made exact.
        file.set_permissions(fs::Permissions::from_mode(0o700))?;
        file.write_all(SCRIPT.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&staged, path)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&staged);
    }
    written
}

/// The environment that turns git's credential helpers off for a terminal: no system or global
/// config, where the machine's helpers (a keychain, `gh`) are named, and no terminal prompt, so
/// a git call that needs a login fails at once. It holds no token, and names no askpass.
pub fn helpers_off_env() -> Vec<(String, String)> {
    [
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_TERMINAL_PROMPT", "0"),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value.to_owned()))
    .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::fs;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command, Stdio};

    use super::*;
    use crate::secrets::MemorySecrets;

    const LOGIN: &str = "rawphp";
    const TOKEN: &str = "ghp_madeUpToken1234567890abcdefABCDEF";
    const OTHER_TOKEN: &str = "ghp_theKeychainsToken0987654321zyxwvZYXW";

    fn secrets_with(login: &str, token: &str) -> MemorySecrets {
        let secrets = MemorySecrets::default();
        secrets.set(&format!("github:{login}"), token).unwrap();
        secrets
    }

    fn auth(data_dir: &Path) -> GitAuth {
        GitAuth::for_login(&secrets_with(LOGIN, TOKEN), LOGIN, data_dir).unwrap()
    }

    /// What a fake git saw: its arguments, one per line, and its environment.
    struct Seen {
        argv: String,
        env: BTreeMap<String, String>,
    }

    /// Runs a fake git through `sh`, with `auth` applied and then `args`, and gives back what
    /// it saw. `sh <script>` takes the script's arguments as git takes its own, so the order
    /// of the hardening and the subcommand is what a real git would get.
    fn run_fake_git(auth: &GitAuth, args: &[&str]) -> Seen {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("git");
        let out = dir.path().join("out");
        fs::write(
            &script,
            format!(
                "printf '%s\\n' \"$@\" > '{out}.argv'\nenv -0 > '{out}.env'\n",
                out = out.display()
            ),
        )
        .unwrap();
        let mut command = Command::new("sh");
        command.arg(&script);
        auth.apply(&mut command);
        let status = command.args(args).status().unwrap();
        assert!(status.success());
        let argv = fs::read_to_string(format!("{}.argv", out.display())).unwrap();
        let env = fs::read(format!("{}.env", out.display())).unwrap();
        let env = env
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
            .map(|entry| {
                let entry = String::from_utf8_lossy(entry);
                let (key, value) = entry.split_once('=').unwrap_or((&entry, ""));
                (key.to_owned(), value.to_owned())
            })
            .collect();
        Seen { argv, env }
    }

    fn git(dir: &Path, home: &Path, args: &[&str]) -> Command {
        let mut command = Command::new("git");
        command
            .current_dir(dir)
            .env("HOME", home)
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("GIT_CONFIG_GLOBAL")
            .env_remove("GIT_CONFIG_NOSYSTEM")
            .env_remove("GIT_CONFIG_PARAMETERS")
            .env_remove("GIT_CONFIG_COUNT")
            .args(args);
        command
    }

    /// A home whose global config has a `store` helper that holds `OTHER_TOKEN` for
    /// github.com, and a repository in it whose own config names that helper too.
    fn home_with_a_helper(root: &Path) -> (PathBuf, PathBuf) {
        let home = root.join("home");
        fs::create_dir(&home).unwrap();
        let store = home.join(".git-credentials");
        fs::write(
            &store,
            format!("https://someone-else:{OTHER_TOKEN}@github.com\n"),
        )
        .unwrap();
        let helper = format!("store --file={}", store.display());
        fs::write(
            home.join(".gitconfig"),
            format!("[credential]\n\thelper = {helper}\n"),
        )
        .unwrap();
        let repo = root.join("repo");
        fs::create_dir(&repo).unwrap();
        assert!(
            git(&repo, &home, &["init", "-q"])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            git(&repo, &home, &["config", "credential.helper", &helper])
                .status()
                .unwrap()
                .success()
        );
        (home, repo)
    }

    #[test]
    fn apply_runs_git_with_helpers_hooks_and_the_system_and_global_config_off() {
        let data = tempfile::tempdir().unwrap();
        let auth = auth(data.path());
        let seen = run_fake_git(&auth, &["fetch", "--", "origin"]);
        let argv: Vec<&str> = seen.argv.lines().collect();
        assert_eq!(
            argv,
            [
                "-c",
                "credential.helper=",
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.autocrlf=false",
                "-c",
                "core.attributesFile=/dev/null",
                "-c",
                "core.excludesFile=/dev/null",
                "-c",
                "protocol.ext.allow=never",
                "-c",
                "core.alternateRefsCommand=",
                "fetch",
                "--",
                "origin",
            ]
        );
        let askpass = data.path().join(ASKPASS);
        for (key, value) in [
            ("GIT_ASKPASS", askpass.to_str().unwrap()),
            ("GIT_TERMINAL_PROMPT", "0"),
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_NO_REPLACE_OBJECTS", "1"),
            ("GIT_LFS_SKIP_SMUDGE", "1"),
            ("GIT_NO_LAZY_FETCH", "1"),
            ("LC_ALL", "C"),
        ] {
            assert_eq!(seen.env.get(key).map(String::as_str), Some(value), "{key}");
        }
    }

    #[test]
    fn the_token_is_in_the_childs_environment_and_in_no_argument() {
        let data = tempfile::tempdir().unwrap();
        let seen = run_fake_git(&auth(data.path()), &["ls-remote", "--", "origin"]);
        assert_eq!(seen.env.get(TOKEN_VAR).map(String::as_str), Some(TOKEN));
        assert!(!seen.argv.contains(TOKEN), "{}", seen.argv);
        let elsewhere: Vec<&String> = seen
            .env
            .iter()
            .filter(|(key, value)| key.as_str() != TOKEN_VAR && value.contains(TOKEN))
            .map(|(key, _)| key)
            .collect();
        assert!(elsewhere.is_empty(), "the token is also in {elsewhere:?}");
        // Set on that child only: this process never holds it.
        assert!(std::env::var_os(TOKEN_VAR).is_none());
    }

    #[test]
    fn config_from_the_environment_the_daemon_started_with_is_not_passed_on() {
        let data = tempfile::tempdir().unwrap();
        let auth = auth(data.path());
        let mut command = Command::new("true");
        command
            .env("GIT_CONFIG_PARAMETERS", "'credential.helper'='osxkeychain'")
            .env("GIT_CONFIG_COUNT", "1");
        auth.apply(&mut command);
        let envs: BTreeMap<OsString, Option<OsString>> = command
            .get_envs()
            .map(|(key, value)| (key.to_owned(), value.map(ToOwned::to_owned)))
            .collect();
        for key in ["GIT_CONFIG_PARAMETERS", "GIT_CONFIG_COUNT"] {
            assert_eq!(envs.get(&OsString::from(key)), Some(&None), "{key}");
        }
    }

    #[test]
    fn the_askpass_script_is_0700_in_the_data_directory_and_holds_no_token() {
        let data = tempfile::tempdir().unwrap();
        auth(data.path());
        let askpass = data.path().join(ASKPASS);
        let meta = fs::symlink_metadata(&askpass).unwrap();
        assert!(meta.file_type().is_file());
        assert_eq!(meta.permissions().mode() & 0o7777, 0o700);
        let script = fs::read_to_string(&askpass).unwrap();
        assert!(!script.contains(TOKEN), "{script}");
        assert!(script.contains(TOKEN_VAR), "{script}");
    }

    #[test]
    fn the_askpass_script_answers_the_password_prompt_with_the_token_of_its_child() {
        let data = tempfile::tempdir().unwrap();
        auth(data.path());
        let askpass = data.path().join(ASKPASS);
        let ask = |prompt: &str| {
            let out = Command::new("sh")
                .arg(&askpass)
                .arg(prompt)
                .env(TOKEN_VAR, TOKEN)
                .output()
                .unwrap();
            assert!(out.status.success());
            String::from_utf8(out.stdout).unwrap()
        };
        assert_eq!(
            ask("Password for 'https://x-access-token@github.com': "),
            format!("{TOKEN}\n")
        );
        assert_eq!(
            ask("Username for 'https://github.com': "),
            "x-access-token\n"
        );
    }

    #[test]
    fn real_git_gets_the_token_from_askpass_and_not_from_a_global_or_repo_helper() {
        let root = tempfile::tempdir().unwrap();
        let (home, repo) = home_with_a_helper(root.path());
        // The setup works: without the hardening, the helper answers with its own token.
        let unhardened = git(&repo, &home, &["credential", "fill"])
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let answer = fill(unhardened);
        assert!(answer.contains(OTHER_TOKEN), "{answer}");

        let data = tempfile::tempdir().unwrap();
        let auth = auth(data.path());
        let mut command = Command::new("git");
        command.current_dir(&repo).env("HOME", &home);
        auth.apply(&mut command);
        let hardened = command
            .args(["credential", "fill"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let answer = fill(hardened);
        assert!(answer.contains(&format!("password={TOKEN}\n")), "{answer}");
        assert!(!answer.contains(OTHER_TOKEN), "{answer}");
    }

    /// Asks `git credential fill`, already started, for github.com over HTTPS.
    fn fill(mut child: std::process::Child) -> String {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"protocol=https\nhost=github.com\n\n")
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{out:?}");
        String::from_utf8(out.stdout).unwrap()
    }

    #[test]
    fn a_login_with_no_token_is_refused_and_nothing_is_written() {
        let data = tempfile::tempdir().unwrap();
        let secrets = secrets_with("someone-else", TOKEN);
        for login in ["rawphp", "bad/name", ""] {
            let error = GitAuth::for_login(&secrets, login, data.path()).unwrap_err();
            assert!(matches!(error, NoToken::Unset { .. }), "{error:?}");
        }
        let blank = secrets_with(LOGIN, "  \n");
        let error = GitAuth::for_login(&blank, LOGIN, data.path()).unwrap_err();
        assert!(matches!(error, NoToken::Unset { .. }), "{error:?}");
        assert!(!data.path().join(ASKPASS).exists());
    }

    #[test]
    fn a_token_git_could_not_be_given_whole_is_refused_and_named_by_login_only() {
        let data = tempfile::tempdir().unwrap();
        for token in [format!("{TOKEN}\nmore"), format!("{TOKEN}\0")] {
            let error =
                GitAuth::for_login(&secrets_with(LOGIN, &token), LOGIN, data.path()).unwrap_err();
            assert!(matches!(error, NoToken::Unset { .. }), "{error:?}");
            assert!(!error.to_string().contains(TOKEN), "{error}");
            assert!(!format!("{error:?}").contains(TOKEN), "{error:?}");
        }
    }

    #[test]
    fn a_token_around_whitespace_is_given_trimmed() {
        let data = tempfile::tempdir().unwrap();
        let auth = GitAuth::for_login(
            &secrets_with(LOGIN, &format!(" {TOKEN}\n")),
            LOGIN,
            data.path(),
        )
        .unwrap();
        let seen = run_fake_git(&auth, &["fetch"]);
        assert_eq!(seen.env.get(TOKEN_VAR).map(String::as_str), Some(TOKEN));
    }

    #[test]
    fn a_missing_data_directory_is_an_askpass_error_that_names_no_token() {
        let data = tempfile::tempdir().unwrap();
        let missing = data.path().join("missing");
        let error = GitAuth::for_login(&secrets_with(LOGIN, TOKEN), LOGIN, &missing).unwrap_err();
        assert!(matches!(error, NoToken::Askpass { .. }), "{error:?}");
        assert!(!error.to_string().contains(TOKEN), "{error}");
    }

    #[test]
    fn neither_the_debug_form_nor_an_error_shows_the_token() {
        let data = tempfile::tempdir().unwrap();
        let shown = format!("{:?}", auth(data.path()));
        assert!(!shown.contains(TOKEN), "{shown}");
        assert!(shown.contains(LOGIN), "{shown}");
    }

    #[test]
    fn a_stale_askpass_script_is_replaced() {
        let data = tempfile::tempdir().unwrap();
        let askpass = data.path().join(ASKPASS);
        fs::write(&askpass, "#!/bin/sh\necho stale\n").unwrap();
        fs::set_permissions(&askpass, fs::Permissions::from_mode(0o777)).unwrap();
        auth(data.path());
        let meta = fs::symlink_metadata(&askpass).unwrap();
        assert_eq!(meta.permissions().mode() & 0o7777, 0o700);
        assert!(fs::read_to_string(&askpass).unwrap().contains(TOKEN_VAR));
    }

    #[test]
    fn a_symlink_in_place_of_the_askpass_script_is_replaced_not_followed() {
        let data = tempfile::tempdir().unwrap();
        let target = data.path().join("elsewhere");
        fs::write(&target, "left alone\n").unwrap();
        std::os::unix::fs::symlink(&target, data.path().join(ASKPASS)).unwrap();
        auth(data.path());
        assert_eq!(fs::read_to_string(&target).unwrap(), "left alone\n");
        let meta = fs::symlink_metadata(data.path().join(ASKPASS)).unwrap();
        assert!(meta.file_type().is_file());
    }

    #[test]
    fn two_logins_share_the_script_and_each_child_gets_its_own_token() {
        let data = tempfile::tempdir().unwrap();
        let secrets = secrets_with(LOGIN, TOKEN);
        secrets.set("github:other-login", OTHER_TOKEN).unwrap();
        let first = GitAuth::for_login(&secrets, LOGIN, data.path()).unwrap();
        let second = GitAuth::for_login(&secrets, "other-login", data.path()).unwrap();
        let seen_first = run_fake_git(&first, &["fetch"]);
        let seen_second = run_fake_git(&second, &["fetch"]);
        assert_eq!(
            seen_first.env.get(TOKEN_VAR).map(String::as_str),
            Some(TOKEN)
        );
        assert_eq!(
            seen_second.env.get(TOKEN_VAR).map(String::as_str),
            Some(OTHER_TOKEN)
        );
        assert_eq!(
            seen_first.env.get("GIT_ASKPASS"),
            seen_second.env.get("GIT_ASKPASS")
        );
    }

    #[test]
    fn helpers_off_env_holds_no_token() {
        let env = helpers_off_env();
        assert!(!env.is_empty());
        for (key, value) in &env {
            assert!(
                !matches!(
                    key.as_str(),
                    TOKEN_VAR | "GIT_ASKPASS" | "GH_TOKEN" | "GITHUB_TOKEN"
                ),
                "{key}"
            );
            assert!(!value.contains(TOKEN), "{key}");
        }
    }

    #[test]
    fn helpers_off_env_leaves_git_no_credential_helper_from_the_global_config() {
        let root = tempfile::tempdir().unwrap();
        let (home, repo) = home_with_a_helper(root.path());
        // The repo's own helper is the repo's; only the global one is the machine's.
        assert!(
            git(&repo, &home, &["config", "--unset", "credential.helper"])
                .status()
                .unwrap()
                .success()
        );
        let get = |env: &[(String, String)]| {
            let out = git(&repo, &home, &["config", "--get-all", "credential.helper"])
                .envs(env.iter().map(|(key, value)| (key, value)))
                .output()
                .unwrap();
            String::from_utf8(out.stdout).unwrap()
        };
        assert!(get(&[]).starts_with("store"), "the setup has no helper");
        assert_eq!(get(&helpers_off_env()), "");
    }

    #[test]
    fn helpers_off_env_keeps_a_terminals_git_from_asking_anyone() {
        let root = tempfile::tempdir().unwrap();
        let (home, repo) = home_with_a_helper(root.path());
        assert!(
            git(&repo, &home, &["config", "--unset", "credential.helper"])
                .status()
                .unwrap()
                .success()
        );
        let child = git(&repo, &home, &["credential", "fill"])
            .envs(helpers_off_env())
            .env_remove("GIT_ASKPASS")
            .env_remove("SSH_ASKPASS")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut child = child;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"protocol=https\nhost=github.com\n\n")
            .unwrap();
        let out = child.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(!out.status.success(), "{stdout}");
        assert!(!stdout.contains(OTHER_TOKEN), "{stdout}");
    }
}
