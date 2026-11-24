//! A login's token for one git call, with credential helpers and hooks off.
//!
//! [`GitAuth::for_login`] reads the token held for a login and makes sure the askpass script
//! is in the data directory. [`GitAuth::apply`] gives one git child the token through that
//! script and turns off what could answer or act in its place: other credential helpers,
//! hooks, the system and global config, and every transport but HTTPS, as the runner's git
//! hardening does. The script answers only git's prompts for github.com over HTTPS. The token
//! is only ever in that child's environment, and the child's own children inherit it. The
//! repository's own config is still read, so a call with the token runs in a repository
//! whose config only the daemon writes, as the runner's gate fetch does (see
//! [`GitAuth::apply`]). [`helpers_off_env`] is for a terminal, which gets no token: it turns
//! the machine's credential helpers, any askpass and any inherited token off, so its keychain
//! answers for nothing.

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
/// child it was set on and that child's own children have. It answers only git's two prompts
/// for `https://github.com`, each matched whole, so a call that a repository's config sends
/// to another host, or over plain HTTP, gets nothing and fails. GitHub takes a token as the
/// password whatever the user name, so the user name is a fixed one, and the password prompt
/// names it.
const SCRIPT: &str = "#!/bin/sh
# Written by foundryd for the git calls it gives a GitHub token to. It holds no token, and
# answers github.com over HTTPS only.
case \"$1\" in
\"Username for 'https://github.com': \") printf '%s\\n' x-access-token ;;
\"Password for 'https://x-access-token@github.com': \") printf '%s\\n' \"$FOUNDRY_GIT_TOKEN\" ;;
*) exit 1 ;;
esac
";

/// The `-c` settings of every call: no credential helper but the askpass, no hooks, the rest
/// of the runner's `HARDENED_GIT_CONFIG` (no fsmonitor, eol conversion, attributes or excludes
/// files, `ext` transport or alternate refs command), then TLS verified. HTTPS as the only
/// transport is `GIT_ALLOW_PROTOCOL` in [`ENV`], which no `protocol.*` config can widen.
const CONFIG: [&str; 9] = [
    // An empty value drops every helper named before it, in the system, global or repo config,
    // for every URL.
    "credential.helper=",
    "core.hooksPath=/dev/null",
    "core.fsmonitor=false",
    "core.autocrlf=false",
    "core.attributesFile=/dev/null",
    "core.excludesFile=/dev/null",
    "protocol.ext.allow=never",
    "core.alternateRefsCommand=",
    "http.sslVerify=true",
];

/// The environment of every call: the runner's `HARDENED_GIT_ENV`, since `-c` cannot turn the
/// system and global config off and with no terminal prompt a refused token fails the call at
/// once, and HTTPS as the only transport.
const ENV: [(&str, &str); 8] = [
    ("GIT_TERMINAL_PROMPT", "0"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_NO_REPLACE_OBJECTS", "1"),
    ("GIT_LFS_SKIP_SMUDGE", "1"),
    // A partial clone fetches a missing object by running `git fetch` with the repo's config.
    ("GIT_NO_LAZY_FETCH", "1"),
    // Git's messages in English, whatever the locale the daemon started in.
    ("LC_ALL", "C"),
    // git takes this over every `protocol.<name>.allow` and `protocol.allow` in any config, so
    // a repository cannot turn ssh, `git://`, the local transport or a remote helper back on,
    // and no `core.sshCommand`, `core.gitProxy`, `remote.<name>.uploadpack` or
    // `remote.<name>.vcs` that its config names runs with the token.
    ("GIT_ALLOW_PROTOCOL", "https"),
];

/// What the daemon's own environment may carry in from whatever started it and is not passed
/// on: config that could set any key the call's `-c` settings do not (git reads it before
/// them, so the `-c` ones still win), and the switch that turns TLS verification off.
const INHERITED: [&str; 3] = [
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_SSL_NO_VERIFY",
];

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

    /// Gives `command`, a git call with no arguments yet, the token and the hardening. The
    /// `-c` settings must come before the subcommand (after it, `git clone -c` would write
    /// them into the new repository's config), so a command that has an argument already is a
    /// mistake in the caller, and this panics. `GIT_ASKPASS` names the script and
    /// [`TOKEN_VAR`] holds the token, on this command's environment only; the daemon's own is
    /// never changed. A caller that adds `GIT_CONFIG_COUNT` pins does so after this. A tokio
    /// command takes it through `as_std_mut()`.
    ///
    /// The command then holds the token: its `Debug` form shows it, so it is never logged or
    /// put in an error.
    ///
    /// git still reads the config of the repository the call runs in, for every key the `-c`
    /// settings do not set, and a URL-scoped `http.<url>.*` key there wins over them: such a
    /// config could turn TLS verification off for github.com, or name its own CA and a proxy,
    /// and read the token. So a call with the token runs only where no config that untrusted
    /// code can write is read: a `git clone` into a new folder, or a fetch by URL into a
    /// repository the daemon made for it, as the runner's gate does (`git init --template=`
    /// then `git fetch <url>`). Never a fetch in a repository that a worktree, a session or a
    /// gate shares.
    ///
    /// Only HTTPS is allowed. A caller whose remote is a repository on this machine, as a
    /// test's is, sets `GIT_ALLOW_PROTOCOL=https:file` on the command after this, as the runner
    /// allows a `file://` base; both repositories must then be ones only the daemon writes,
    /// since the calling one can name an `uploadpack` to run with the token.
    pub fn apply(&self, command: &mut Command) {
        assert!(
            command.get_args().len() == 0,
            "GitAuth::apply goes before any argument of the git call"
        );
        for setting in CONFIG {
            command.arg("-c").arg(setting);
        }
        for name in INHERITED {
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
/// config, where the machine's helpers (a keychain, `gh`) are named; no config the daemon's
/// own environment may carry in; an empty `GIT_ASKPASS`, with which git asks neither
/// `core.askPass` nor `SSH_ASKPASS`, and an empty `SSH_ASKPASS` for ssh itself; no token
/// `gh` would take from the daemon's environment, and no `gh` config, so `gh` finds no stored
/// login either (and so neither does `gh auth git-credential`); and no terminal prompt, so a
/// git call that needs a login fails at once. Each is a value, since a terminal's environment is set over
/// the daemon's and cannot remove a variable: git and `gh` take an empty one as none. It
/// holds no token. A helper that code in the terminal writes into the repository's own config
/// is that code's doing: it could as well run the helper itself, which no environment
/// prevents.
pub fn helpers_off_env() -> Vec<(String, String)> {
    [
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_CONFIG_COUNT", "0"),
        ("GIT_CONFIG_PARAMETERS", ""),
        ("GIT_ASKPASS", ""),
        ("SSH_ASKPASS", ""),
        ("GH_TOKEN", ""),
        ("GITHUB_TOKEN", ""),
        ("GH_ENTERPRISE_TOKEN", ""),
        ("GITHUB_ENTERPRISE_TOKEN", ""),
        // gh reads its logins from this directory: here there is none to read.
        ("GH_CONFIG_DIR", "/dev/null"),
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
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::process::{Child, Command, Output, Stdio};

    use super::*;
    use crate::secrets::{Edit, MemorySecrets};

    const LOGIN: &str = "rawphp";
    const TOKEN: &str = "ghp_madeUpToken1234567890abcdefABCDEF";
    const OTHER_TOKEN: &str = "ghp_theKeychainsToken0987654321zyxwvZYXW";

    fn secrets_with(login: &str, token: &str) -> MemorySecrets {
        let secrets = MemorySecrets::default();
        secrets.set(&format!("github:{login}"), token).unwrap();
        secrets
    }

    fn auth(data_dir: &Path) -> GitAuth {
        let auth = GitAuth::for_login(&secrets_with(LOGIN, TOKEN), LOGIN, data_dir).unwrap();
        // A prompt it does not answer, so the run has no effect.
        settle(Command::new(data_dir.join(ASKPASS)).arg("probe"));
        auth
    }

    /// Writes an executable `sh` script at `path`, which exits at once when `PROBE` is set.
    fn script(path: &Path, body: &str) {
        fs::write(
            path,
            format!("#!/bin/sh\n[ -z \"$PROBE\" ] || exit 0\n{body}\n"),
        )
        .unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        settle(Command::new(path).env("PROBE", "1"));
    }

    /// Runs `probe`, a script just written, once through [`spawn`]. A child that another test
    /// forked while the script was open for writing holds it open until that child execs, and
    /// git cannot run the script meanwhile. Once one run has started, no such child is left,
    /// and nothing opens the script for writing again, so git can run it from then on.
    fn settle(probe: &mut Command) {
        spawn(probe.stdout(Stdio::null()).stderr(Stdio::null()))
            .wait()
            .unwrap();
    }

    /// Spawns `command`, again while the OS says its program is still open for writing:
    /// another test's child can hold a script this test just wrote until that child execs.
    fn spawn(command: &mut Command) -> Child {
        for _ in 0..50 {
            match command.spawn() {
                Err(error) if error.raw_os_error() == Some(nix::errno::Errno::ETXTBSY as i32) => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                spawned => return spawned.unwrap(),
            }
        }
        panic!("the program stayed busy");
    }

    /// What a fake git saw: its arguments, one per line, and its environment.
    struct Seen {
        argv: String,
        env: BTreeMap<String, String>,
    }

    /// Runs a fake git, with `auth` applied and then `args`, and gives back what it saw.
    fn run_fake_git(auth: &GitAuth, args: &[&str]) -> Seen {
        let dir = tempfile::tempdir().unwrap();
        let git = dir.path().join("git");
        let out = dir.path().join("out");
        script(
            &git,
            &format!(
                "printf '%s\\n' \"$@\" > '{out}.argv'\nenv -0 > '{out}.env'",
                out = out.display()
            ),
        );
        let mut command = Command::new(&git);
        auth.apply(&mut command);
        command.args(args);
        assert!(spawn(&mut command).wait().unwrap().success());
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

    /// A machine for real git: a home whose global config, and a system config, each name a
    /// `store` helper that holds `OTHER_TOKEN` for github.com, and a repository whose own
    /// config names that helper too, once for every host and once for github.com.
    struct Machine {
        _root: tempfile::TempDir,
        home: PathBuf,
        system: PathBuf,
        repo: PathBuf,
        helper: String,
    }

    impl Machine {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let home = root.path().join("home");
            fs::create_dir(&home).unwrap();
            let store = root.path().join("git-credentials");
            fs::write(
                &store,
                format!("https://someone-else:{OTHER_TOKEN}@github.com\n"),
            )
            .unwrap();
            let helper = format!("store --file={}", store.display());
            let config = format!("[credential]\n\thelper = {helper}\n");
            fs::write(home.join(".gitconfig"), &config).unwrap();
            let system = root.path().join("system-gitconfig");
            fs::write(&system, &config).unwrap();
            let repo = root.path().join("repo");
            fs::create_dir(&repo).unwrap();
            let machine = Machine {
                home,
                system,
                repo,
                helper,
                _root: root,
            };
            machine.git_ok(&["init", "-q"]);
            machine.git_ok(&["config", "credential.helper", &machine.helper]);
            machine.git_ok(&[
                "config",
                "credential.https://github.com.helper",
                &machine.helper,
            ]);
            machine
        }

        /// git in the repository, with this machine's home and system config and nothing
        /// from the test's own environment that changes which config git reads.
        fn git(&self, args: &[&str]) -> Command {
            let mut command = Command::new("git");
            command
                .current_dir(&self.repo)
                .env("HOME", &self.home)
                .env("GIT_CONFIG_SYSTEM", &self.system)
                .env_remove("XDG_CONFIG_HOME")
                .env_remove("GIT_CONFIG_GLOBAL")
                .env_remove("GIT_CONFIG_NOSYSTEM")
                .env_remove("GIT_CONFIG_PARAMETERS")
                .env_remove("GIT_CONFIG_COUNT")
                .env_remove("GIT_ASKPASS")
                .env_remove("SSH_ASKPASS")
                .env("GIT_TERMINAL_PROMPT", "0")
                .args(args);
            command
        }

        fn git_ok(&self, args: &[&str]) {
            assert!(self.git(args).status().unwrap().success(), "git {args:?}");
        }

        /// The same environment as [`Machine::git`], for a command `apply` hardens, which
        /// must have no arguments yet.
        fn hardened(&self, auth: &GitAuth) -> Command {
            let mut command = self.git(&[]);
            auth.apply(&mut command);
            command
        }
    }

    /// Asks `git credential fill`, given as a command with no `stdin` set, for `host` over
    /// HTTPS.
    fn fill(command: &mut Command, host: &str) -> Output {
        command
            .args(["credential", "fill"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = spawn(command);
        child
            .stdin
            .take()
            .unwrap()
            .write_all(format!("protocol=https\nhost={host}\n\n").as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    /// Which of the two tokens `out` holds, and never the text itself, so no assertion prints
    /// a credential that a real helper may have given.
    fn tokens_in(out: &Output) -> (bool, bool) {
        let text = String::from_utf8_lossy(&out.stdout);
        (text.contains(TOKEN), text.contains(OTHER_TOKEN))
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
                "-c",
                "http.sslVerify=true",
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
            ("GIT_ALLOW_PROTOCOL", "https"),
        ] {
            assert_eq!(seen.env.get(key).map(String::as_str), Some(value), "{key}");
        }
    }

    #[test]
    #[should_panic(expected = "before any argument")]
    fn apply_refuses_a_command_that_already_has_arguments() {
        let data = tempfile::tempdir().unwrap();
        let mut command = Command::new("git");
        command.arg("clone");
        auth(data.path()).apply(&mut command);
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
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_SSL_NO_VERIFY", "1");
        auth.apply(&mut command);
        let envs: BTreeMap<OsString, Option<OsString>> = command
            .get_envs()
            .map(|(key, value)| (key.to_owned(), value.map(ToOwned::to_owned)))
            .collect();
        for key in [
            "GIT_CONFIG_PARAMETERS",
            "GIT_CONFIG_COUNT",
            "GIT_SSL_NO_VERIFY",
        ] {
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
    fn the_askpass_script_answers_github_over_https_and_no_other_prompt() {
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
            (out.status.success(), String::from_utf8(out.stdout).unwrap())
        };
        assert_eq!(
            ask("Password for 'https://x-access-token@github.com': "),
            (true, format!("{TOKEN}\n"))
        );
        assert_eq!(
            ask("Username for 'https://github.com': "),
            (true, "x-access-token\n".to_owned())
        );
        for prompt in [
            "Password for 'https://x-access-token@evil.example': ",
            "Password for 'https://x-access-token@github.com.evil.example': ",
            "Password for 'https://x-access-token@evil.example/@github.com': ",
            "Password for 'http://x-access-token@github.com': ",
            "Password for 'https://someone@github.com': ",
            "Username for 'https://evil.example': ",
            "Are you sure you want to continue connecting (yes/no)? ",
            "",
        ] {
            let (ok, out) = ask(prompt);
            assert!(!ok, "{prompt:?} was answered");
            assert_eq!(out, "", "{prompt:?}");
        }
    }

    #[test]
    fn real_git_gets_the_token_from_askpass_and_from_no_helper_of_any_config() {
        let machine = Machine::new();
        // The setup works: without the hardening, a helper answers with its own token.
        let unhardened = fill(&mut machine.git(&[]), "github.com");
        assert_eq!(tokens_in(&unhardened), (false, true), "no helper answered");

        let data = tempfile::tempdir().unwrap();
        let hardened = fill(&mut machine.hardened(&auth(data.path())), "github.com");
        assert!(hardened.status.success(), "{:?}", hardened.status);
        assert_eq!(tokens_in(&hardened), (true, false));
        assert!(String::from_utf8_lossy(&hardened.stdout).contains("username=x-access-token\n"));
    }

    #[test]
    fn real_git_gives_the_token_to_no_host_but_github() {
        let machine = Machine::new();
        let data = tempfile::tempdir().unwrap();
        let out = fill(&mut machine.hardened(&auth(data.path())), "evil.example");
        assert!(!out.status.success(), "{:?}", out.status);
        assert_eq!(tokens_in(&out), (false, false));
    }

    #[test]
    fn a_command_the_repos_config_names_never_runs_with_the_token() {
        let machine = Machine::new();
        let dir = tempfile::tempdir().unwrap();
        let spy = dir.path().join("spy");
        let seen = dir.path().join("seen");
        script(&spy, &format!("env > '{}'\nexit 1", seen.display()));
        // git runs `core.gitProxy` itself and the other two through a shell.
        let spy_command = spy.to_str().unwrap();
        let upload_pack = format!("sh {spy_command}; git-upload-pack");
        // The repository turns each transport back on by name, too.
        for (key, value) in [
            ("core.sshCommand", spy_command),
            ("core.gitProxy", spy_command),
            ("protocol.ssh.allow", "always"),
            ("protocol.git.allow", "always"),
            ("protocol.file.allow", "always"),
            ("remote.origin.url", "ssh://example.invalid/x.git"),
            ("remote.plain.url", "git://example.invalid/x.git"),
            ("remote.local.url", machine.repo.to_str().unwrap()),
            ("remote.local.uploadpack", upload_pack.as_str()),
        ] {
            machine.git_ok(&["config", key, value]);
        }
        // The setup works: without the hardening, each remote runs the repository's command.
        for remote in ["origin", "plain", "local"] {
            let _ = machine.git(&["fetch", "--", remote]).output().unwrap();
            assert!(seen.exists(), "{remote}: the setup");
            fs::remove_file(&seen).unwrap();
        }
        let data = tempfile::tempdir().unwrap();
        let auth = auth(data.path());
        for remote in ["origin", "plain", "local"] {
            let out = machine
                .hardened(&auth)
                .args(["fetch", "--", remote])
                .output()
                .unwrap();
            assert!(!out.status.success(), "{remote}");
            assert!(!seen.exists(), "{remote}: the repo's command ran");
        }
    }

    #[test]
    fn a_fetch_from_a_local_repository_needs_the_file_transport_set_after_apply() {
        let machine = Machine::new();
        let origin = machine.repo.to_str().unwrap();
        let data = tempfile::tempdir().unwrap();
        let auth = auth(data.path());
        let refused = machine
            .hardened(&auth)
            .args(["ls-remote", "--", origin])
            .output()
            .unwrap();
        assert!(!refused.status.success());
        assert!(
            String::from_utf8_lossy(&refused.stderr).contains("transport 'file' not allowed"),
            "{refused:?}"
        );
        let allowed = machine
            .hardened(&auth)
            .env("GIT_ALLOW_PROTOCOL", "https:file")
            .args(["ls-remote", "--", origin])
            .output()
            .unwrap();
        assert!(allowed.status.success(), "{allowed:?}");
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

    /// A store whose every read fails.
    struct Unreadable;

    impl Secrets for Unreadable {
        fn get(&self, _: &str) -> Result<Option<String>, SecretsError> {
            Err(SecretsError::Unreadable {
                path: PathBuf::from("/secrets.json"),
            })
        }
        fn set(&self, _: &str, _: &str) -> Result<(), SecretsError> {
            unreachable!()
        }
        fn update(&self, _: &str, _: Edit<'_>) -> Result<(), SecretsError> {
            unreachable!()
        }
    }

    #[test]
    fn a_store_that_cannot_be_read_is_a_store_error_and_nothing_is_written() {
        let data = tempfile::tempdir().unwrap();
        let error = GitAuth::for_login(&Unreadable, LOGIN, data.path()).unwrap_err();
        assert!(matches!(error, NoToken::Store { .. }), "{error:?}");
        assert!(error.to_string().contains(LOGIN), "{error}");
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
    fn the_debug_form_shows_the_login_and_not_the_token() {
        let data = tempfile::tempdir().unwrap();
        let shown = format!("{:?}", auth(data.path()));
        assert!(!shown.contains(TOKEN), "{shown}");
        assert!(shown.contains(LOGIN), "{shown}");
    }

    #[test]
    fn an_askpass_script_as_it_should_be_is_left_as_it_is() {
        let data = tempfile::tempdir().unwrap();
        auth(data.path());
        let askpass = data.path().join(ASKPASS);
        let before = fs::symlink_metadata(&askpass).unwrap().ino();
        auth(data.path());
        assert_eq!(fs::symlink_metadata(&askpass).unwrap().ino(), before);
    }

    #[test]
    fn an_askpass_script_with_another_mode_or_content_is_replaced() {
        let data = tempfile::tempdir().unwrap();
        auth(data.path());
        let askpass = data.path().join(ASKPASS);
        let script = fs::read(&askpass).unwrap();
        for (mode, content) in [
            (0o755, script.clone()),
            (0o700, b"#!/bin/sh\necho stale\n".to_vec()),
        ] {
            fs::remove_file(&askpass).unwrap();
            fs::write(&askpass, &content).unwrap();
            fs::set_permissions(&askpass, fs::Permissions::from_mode(mode)).unwrap();
            auth(data.path());
            let meta = fs::symlink_metadata(&askpass).unwrap();
            assert_eq!(meta.permissions().mode() & 0o7777, 0o700, "{mode:o}");
            assert_eq!(fs::read(&askpass).unwrap(), script, "{mode:o}");
        }
    }

    #[test]
    fn a_symlink_in_place_of_the_askpass_script_is_replaced_not_followed() {
        let data = tempfile::tempdir().unwrap();
        auth(data.path());
        let askpass = data.path().join(ASKPASS);
        // A link to a copy of the script itself, 0700, is still not the script.
        let target = data.path().join("elsewhere");
        fs::rename(&askpass, &target).unwrap();
        let held = fs::read(&target).unwrap();
        std::os::unix::fs::symlink(&target, &askpass).unwrap();
        auth(data.path());
        assert!(
            fs::symlink_metadata(&askpass)
                .unwrap()
                .file_type()
                .is_file()
        );
        assert_eq!(fs::read(&target).unwrap(), held);
        assert_eq!(
            fs::symlink_metadata(&target).unwrap().permissions().mode() & 0o7777,
            0o700
        );
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
    fn helpers_off_env_holds_no_token_and_blanks_every_askpass_and_token_it_could_inherit() {
        let env: BTreeMap<String, String> = helpers_off_env().into_iter().collect();
        assert!(!env.contains_key(TOKEN_VAR));
        for (key, value) in &env {
            assert!(!value.contains(TOKEN), "{key}");
        }
        for (key, value) in [
            ("GH_TOKEN", ""),
            ("GITHUB_TOKEN", ""),
            ("GH_ENTERPRISE_TOKEN", ""),
            ("GITHUB_ENTERPRISE_TOKEN", ""),
            ("GH_CONFIG_DIR", "/dev/null"),
            ("GIT_ASKPASS", ""),
            ("SSH_ASKPASS", ""),
            ("GIT_TERMINAL_PROMPT", "0"),
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_CONFIG_COUNT", "0"),
            ("GIT_CONFIG_PARAMETERS", ""),
        ] {
            assert_eq!(env.get(key).map(String::as_str), Some(value), "{key}");
        }
    }

    #[test]
    fn helpers_off_env_leaves_git_no_credential_helper_from_the_global_or_system_config() {
        let machine = Machine::new();
        // The repository's own helpers are not the machine's config; these are.
        machine.git_ok(&["config", "--unset", "credential.helper"]);
        machine.git_ok(&["config", "--unset", "credential.https://github.com.helper"]);
        let get = |env: &[(String, String)]| {
            let out = machine
                .git(&["config", "--get-all", "credential.helper"])
                .envs(env.iter().map(|(key, value)| (key, value)))
                .output()
                .unwrap();
            String::from_utf8(out.stdout).unwrap()
        };
        let helper = &machine.helper;
        assert_eq!(get(&[]), format!("{helper}\n{helper}\n"), "the setup");
        for (key, value) in [
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ] {
            let one = [(key.to_owned(), value.to_owned())];
            assert_eq!(get(&one), format!("{helper}\n"), "{key} alone");
        }
        assert_eq!(get(&helpers_off_env()), "");
    }

    #[test]
    fn helpers_off_env_keeps_a_terminals_git_from_an_askpass_or_helper_it_inherits() {
        let machine = Machine::new();
        machine.git_ok(&["config", "--unset", "credential.helper"]);
        machine.git_ok(&["config", "--unset", "credential.https://github.com.helper"]);
        let dir = tempfile::tempdir().unwrap();
        let answers = dir.path().join("answers");
        script(&answers, &format!("printf '%s\\n' '{OTHER_TOKEN}'"));
        let answers = answers.to_str().unwrap().to_owned();
        let helper = machine.helper.clone();
        let inherited: [&[(&str, &str)]; 4] = [
            &[("GIT_ASKPASS", &answers)],
            &[("SSH_ASKPASS", &answers)],
            &[
                ("GIT_CONFIG_COUNT", "1"),
                ("GIT_CONFIG_KEY_0", "credential.helper"),
                ("GIT_CONFIG_VALUE_0", &helper),
            ],
            &[(
                "GIT_CONFIG_PARAMETERS",
                &format!("'credential.helper'='{helper}'"),
            )],
        ];
        for env in inherited {
            // In the daemon's environment, with the global and system config off.
            let terminal = || {
                let mut command = machine.git(&[]);
                command
                    .env("GIT_CONFIG_NOSYSTEM", "1")
                    .env("GIT_CONFIG_GLOBAL", "/dev/null")
                    .envs(env.iter().copied());
                command
            };
            let unguarded = fill(&mut terminal(), "github.com");
            assert_eq!(tokens_in(&unguarded), (false, true), "{env:?}: the setup");
            let guarded = fill(terminal().envs(helpers_off_env()), "github.com");
            assert!(!guarded.status.success(), "{env:?}");
            assert_eq!(tokens_in(&guarded), (false, false), "{env:?}");
        }
    }
}
