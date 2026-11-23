//! The tracker through `gh`, each call ended within 30 seconds.
//!
//! [`Gh`] runs `gh` with the login's token in `GH_TOKEN`, in the child's environment only: the
//! token is never an argument, a log line or an error. A call still running 30 seconds after it
//! started, on the factory's clock, is killed with everything it started and is
//! [`TrackerError::Timeout`]. [`finish`] reads how a call ended, for the real `gh` and for the
//! fake alike.

use std::fmt;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;

use super::{Tracker, TrackerError};
use crate::clock::Clock;

/// How long one `gh` call may run before it is killed.
pub const LIMIT: Duration = Duration::from_secs(30);

/// The most `gh` may print on stdout. Output is never cut short: past this the call fails.
pub const OUTPUT_LIMIT: usize = 8 * 1024 * 1024;

/// How often a running call is looked at.
const POLL: Duration = Duration::from_millis(10);

/// How one `gh` call ended, before it is read as an answer or a [`TrackerError`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    /// `gh` exited: `status` is `None` when a signal ended it.
    Exit {
        status: Option<i32>,
        stdout: String,
        stderr: String,
    },
    /// `gh` was still running at [`LIMIT`] and was killed.
    TimedOut,
    /// `gh` printed more than [`OUTPUT_LIMIT`] bytes on stdout.
    TooLarge,
    /// `gh` could not be started, and why.
    NotStarted(String),
}

impl Answer {
    /// A call that exited 0 and printed `stdout`.
    pub fn ok(stdout: impl Into<String>) -> Self {
        Answer::Exit {
            status: Some(0),
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    /// A call that exited 0, printed `stdout` and warned `stderr`.
    pub fn ok_warning(stdout: impl Into<String>, stderr: impl Into<String>) -> Self {
        Answer::Exit {
            status: Some(0),
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }

    /// A call that exited with `status` and said `stderr`.
    pub fn fail(status: i32, stderr: impl Into<String>) -> Self {
        Answer::Exit {
            status: Some(status),
            stdout: String::new(),
            stderr: stderr.into(),
        }
    }

    /// A call a signal ended.
    pub fn signal_killed() -> Self {
        Answer::Exit {
            status: None,
            stdout: String::new(),
            stderr: String::new(),
        }
    }
}

/// The command for error messages. Comment bodies (the value after `--body`, or a `body=` raw
/// field) are shown as their size in bytes, and newlines are escaped, so one error stays one
/// line.
pub fn command_text(args: &[String]) -> String {
    let mut shown = vec!["gh".to_string()];
    let mut after_body_flag = false;
    for arg in args {
        if after_body_flag {
            shown.push(format!("<{} bytes>", arg.len()));
        } else if let Some(body) = arg.strip_prefix("body=") {
            shown.push(format!("body=<{} bytes>", body.len()));
        } else {
            shown.push(one_line(arg));
        }
        after_body_flag = arg == "--body";
    }
    shown.join(" ")
}

/// `text` with `\r` and `\n` written as escapes.
pub fn one_line(text: &str) -> String {
    text.replace('\r', "\\r").replace('\n', "\\n")
}

/// The error for a call whose output was not what it expected.
pub fn parse_error(args: &[String], expected: &str) -> TrackerError {
    TrackerError::Other(format!(
        "`{}` returned unexpected output: {expected}",
        command_text(args)
    ))
}

/// The error for an argument refused before any call, since it could leave the path it is
/// put in.
pub fn invalid_argument(argument: &str, expected: &str) -> TrackerError {
    TrackerError::Other(format!(
        "refused \"{}\" before calling gh: expected {expected}",
        one_line(argument)
    ))
}

/// Whether `stderr` says GitHub is rate limiting the login.
fn rate_limited(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    ["api rate limit", "secondary rate limit", "http 429"]
        .iter()
        .any(|said| lower.contains(said))
}

/// Whether `stderr` says the login's credentials were refused or missing.
fn auth_refused(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    [
        "http 401",
        "bad credentials",
        "gh auth login",
        "requires authentication",
    ]
    .iter()
    .any(|said| lower.contains(said))
}

/// Reads how the call with `args` ended: its stdout, or the error it is.
pub fn finish(args: &[String], answer: Answer) -> Result<String, TrackerError> {
    let command = command_text(args);
    match answer {
        Answer::NotStarted(why) => Err(TrackerError::Other(format!(
            "cannot start `{command}`: {why}"
        ))),
        Answer::TimedOut => Err(TrackerError::Timeout {
            command,
            seconds: LIMIT.as_secs(),
        }),
        Answer::TooLarge => Err(TrackerError::Other(format!(
            "`{command}` printed more than {OUTPUT_LIMIT} bytes"
        ))),
        Answer::Exit {
            status: Some(0),
            stdout,
            ..
        } => Ok(stdout),
        // A shell's "command not found" and "not executable"; gh itself never exits so.
        Answer::Exit {
            status: Some(status @ (126 | 127)),
            stderr,
            ..
        } => {
            let why = match stderr.trim() {
                "" => format!("the shell exited with status {status}"),
                said => said.to_string(),
            };
            Err(TrackerError::Other(format!(
                "cannot start `{command}`: {why}"
            )))
        }
        Answer::Exit { status, stderr, .. } => {
            let status = status.unwrap_or(-1);
            if rate_limited(&stderr) {
                Err(TrackerError::RateLimited { command, stderr })
            } else if auth_refused(&stderr) {
                Err(TrackerError::Auth { command, stderr })
            } else if stderr.contains("HTTP 404") {
                Err(TrackerError::NotFound {
                    command,
                    status,
                    stderr,
                })
            } else {
                Err(TrackerError::Other(format!(
                    "`{command}` exited with status {status}: {}",
                    stderr.trim_end()
                )))
            }
        }
    }
}

/// The tracker for one GitHub login, through the `gh` CLI.
pub struct Gh {
    login: String,
    token: String,
    clock: Arc<dyn Clock>,
    program: PathBuf,
}

impl fmt::Debug for Gh {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Gh")
            .field("login", &self.login)
            .field("program", &self.program)
            .finish_non_exhaustive()
    }
}

impl Gh {
    /// The tracker that acts as `login` with `token`, timing each call on `clock`.
    pub fn new(login: impl Into<String>, token: impl Into<String>, clock: Arc<dyn Clock>) -> Self {
        Self {
            login: login.into(),
            token: token.into(),
            clock,
            program: PathBuf::from("gh"),
        }
    }

    /// The same tracker running `program` in place of `gh`, for tests.
    pub fn with_program(mut self, program: impl Into<PathBuf>) -> Self {
        self.program = program.into();
        self
    }

    /// The login the calls act as.
    pub fn login(&self) -> &str {
        &self.login
    }

    /// The command for one call: the token and the settings that keep `gh` from prompting, and
    /// no other host or token that the daemon's environment may hold.
    fn command(&self, args: &[String]) -> Command {
        let mut command = Command::new(&self.program);
        command
            .args(args)
            .env("GH_TOKEN", &self.token)
            .env("GH_PROMPT_DISABLED", "1")
            .env("GH_NO_UPDATE_NOTIFIER", "1")
            // Another host or token in the daemon's environment would act as someone else.
            .env_remove("GH_HOST")
            .env_remove("GH_ENTERPRISE_TOKEN")
            .env_remove("GITHUB_ENTERPRISE_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Its own process group, so a call killed at the limit takes what it started too.
            .process_group(0);
        command
    }

    /// Runs the program once with `args` and the token in its environment. The call is killed,
    /// with everything it started, once [`LIMIT`] has passed on the clock before `gh` has
    /// exited and closed its output.
    fn run(&self, args: &[String]) -> Answer {
        // Without a token `gh` would use `GITHUB_TOKEN` or the machine's stored login.
        if self.token.trim().is_empty() {
            return Answer::NotStarted(format!("no token for {}", self.login));
        }
        // The OS refuses such an argument with an error that quotes it, and it could be a body.
        if args.iter().any(|arg| arg.contains('\0')) {
            return Answer::NotStarted("an argument contains a NUL byte".into());
        }
        let started = self.clock.now();
        let mut child = match self.command(args).spawn() {
            Ok(child) => child,
            Err(error) => return Answer::NotStarted(error.to_string()),
        };
        let stdout = drain(child.stdout.take(), OUTPUT_LIMIT);
        let stderr = drain(child.stderr.take(), OUTPUT_LIMIT);
        // The leader's exit does not end the call: a descendant may still hold the pipes, and the
        // limit holds until both are read to their end.
        let mut status = None;
        loop {
            if status.is_none() {
                match child.try_wait() {
                    Ok(found) => status = found,
                    Err(error) => {
                        kill_group(&mut child);
                        return Answer::NotStarted(error.to_string());
                    }
                }
            }
            if status.is_some() && stdout.is_finished() && stderr.is_finished() {
                break;
            }
            if self.clock.now().saturating_sub(started) >= LIMIT {
                kill_group(&mut child);
                return Answer::TimedOut;
            }
            thread::sleep(POLL);
        }
        let (stdout, over) = stdout.join().unwrap_or_default();
        let (stderr, _) = stderr.join().unwrap_or_default();
        if over {
            return Answer::TooLarge;
        }
        Answer::Exit {
            status: status.and_then(|status| status.code()),
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        }
    }
}

impl Tracker for Gh {
    fn gh(&self, args: &[String]) -> Result<String, TrackerError> {
        finish(args, self.run(args))
    }
}

/// Kills the child's process group and reaps the child. The child is not reaped before the
/// kill, so its id still names its group.
fn kill_group(child: &mut Child) {
    if let Ok(pid) = i32::try_from(child.id()) {
        let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Reads `pipe` to its end on a thread of its own, keeping at most `limit` bytes, and says
/// whether there was more.
fn drain<R: Read + Send + 'static>(pipe: Option<R>, limit: usize) -> JoinHandle<(Vec<u8>, bool)> {
    thread::spawn(move || {
        let mut kept = Vec::new();
        let mut over = false;
        let Some(mut pipe) = pipe else {
            return (kept, over);
        };
        let mut buf = [0u8; 64 * 1024];
        loop {
            match pipe.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let room = limit.saturating_sub(kept.len());
                    kept.extend_from_slice(&buf[..n.min(room)]);
                    over |= n > room;
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        (kept, over)
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::clock::FakeClock;

    const TOKEN: &str = "ghp_madeUpToken1234567890abcdefABCDEF";

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| part.to_string()).collect()
    }

    /// A `gh` at `dir/gh` that runs `body` with `sh`.
    fn fake_gh(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("gh");
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn gh_at(program: &Path, clock: Arc<dyn Clock>) -> Gh {
        Gh::new("rawphp", TOKEN, clock).with_program(program)
    }

    // The runner's "runs gh with the config dir, a 60 s limit and separate stderr on every
    // call": Foundry gives the token in the environment and a 30 s limit.
    #[test]
    fn runs_gh_with_the_token_in_its_environment_a_30_s_limit_and_separate_stderr_on_every_call() {
        let dir = tempfile::tempdir().unwrap();
        let seen = dir.path().join("seen");
        let program = fake_gh(
            dir.path(),
            &format!(
                "for a in \"$@\"; do printf '%s\\n' \"$a\" >> '{seen}.args'; done\n\
                 printf '%s' \"$GH_TOKEN\" > '{seen}.token'\n\
                 printf 'warning' >&2\n\
                 printf '[]'",
                seen = seen.display()
            ),
        );
        let gh = gh_at(&program, Arc::new(FakeClock::new()));
        let call = args(&["issue", "list", "--repo", "rawphp/softfac"]);
        assert_eq!(gh.gh(&call).unwrap(), "[]");
        assert_eq!(
            fs::read_to_string(format!("{}.token", seen.display())).unwrap(),
            TOKEN
        );
        let argv = fs::read_to_string(format!("{}.args", seen.display())).unwrap();
        assert_eq!(argv, "issue\nlist\n--repo\nrawphp/softfac\n");
        assert_eq!(LIMIT, Duration::from_secs(30));
        assert_eq!(OUTPUT_LIMIT, 8 * 1024 * 1024);
    }

    #[test]
    fn the_token_is_in_no_argument() {
        let dir = tempfile::tempdir().unwrap();
        let seen = dir.path().join("argv");
        let program = fake_gh(
            dir.path(),
            &format!("printf '%s\\n' \"$0\" \"$@\" > '{}'", seen.display()),
        );
        let gh = gh_at(&program, Arc::new(FakeClock::new()));
        gh.gh(&args(&["api", "user"])).unwrap();
        let argv = fs::read_to_string(&seen).unwrap();
        assert!(!argv.contains(TOKEN), "{argv}");
        assert!(argv.ends_with("api\nuser\n"), "{argv}");
    }

    /// The token is in neither the message nor the debug form of `error`.
    fn assert_no_token(error: &TrackerError) {
        assert!(!error.to_string().contains(TOKEN), "{error}");
        assert!(!format!("{error:?}").contains(TOKEN), "{error:?}");
    }

    #[test]
    fn a_gh_that_sleeps_is_killed_at_30_seconds_on_the_fake_clock_and_is_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let gpid_file = dir.path().join("gpid");
        // A distinctive duration, so a stray is easy to tell from any other sleep.
        let program = fake_gh(
            dir.path(),
            &format!(
                "echo $$ > '{}'\nsleep 47.3191 &\necho $! > '{}'\nwait\necho late",
                pid_file.display(),
                gpid_file.display()
            ),
        );
        let clock = Arc::new(FakeClock::new());
        let gh = gh_at(&program, clock.clone());
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(gh.gh(&args(&["issue", "list", "--repo", "rawphp/softfac"])));
        });

        let pid = pid_in(&pid_file);
        let gpid = pid_in(&gpid_file);
        // The sleeper is in the shell's group, so the kill that takes the shell takes it.
        assert_eq!(
            nix::unistd::getpgid(Some(Pid::from_raw(gpid))).unwrap(),
            Pid::from_raw(pid)
        );
        clock.advance(Duration::from_millis(29_999));
        assert!(
            rx.recv_timeout(Duration::from_millis(300)).is_err(),
            "killed before 30 s on the clock"
        );

        clock.advance(Duration::from_millis(1));
        let result = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("killed once 30 s passed on the clock");
        let error = result.unwrap_err();
        assert_no_token(&error);
        assert_eq!(
            error,
            TrackerError::Timeout {
                command: "gh issue list --repo rawphp/softfac".into(),
                seconds: 30,
            }
        );
        assert_eq!(
            error.to_string(),
            "`gh issue list --repo rawphp/softfac` timed out after 30 s"
        );

        // The group went with it: neither the shell's pid nor the sleeper's answers a signal.
        assert!(goes_away(pid), "the shell outlived the call");
        assert!(goes_away(gpid), "the grandchild outlived the call");
    }

    /// The pid the script wrote to `file`, once it has.
    fn pid_in(file: &Path) -> i32 {
        let begun = Instant::now();
        while begun.elapsed() < Duration::from_secs(10) {
            if let Some(pid) = fs::read_to_string(file)
                .ok()
                .and_then(|text| text.trim().parse().ok())
            {
                return pid;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("no pid in {}", file.display());
    }

    /// Whether `pid` answers no signal within 5 seconds.
    fn goes_away(pid: i32) -> bool {
        let begun = Instant::now();
        while nix::sys::signal::kill(Pid::from_raw(pid), None).is_ok()
            && begun.elapsed() < Duration::from_secs(5)
        {
            thread::sleep(Duration::from_millis(10));
        }
        nix::sys::signal::kill(Pid::from_raw(pid), None).is_err()
    }

    #[test]
    fn a_descendant_holding_the_pipes_after_the_leader_exits_is_still_killed_at_30_seconds() {
        let dir = tempfile::tempdir().unwrap();
        let gpid_file = dir.path().join("gpid");
        // The leader exits at once; the background sleep keeps stdout open.
        let program = fake_gh(
            dir.path(),
            &format!(
                "sleep 47.3191 &\necho $! > '{}'\nprintf ok",
                gpid_file.display()
            ),
        );
        let clock = Arc::new(FakeClock::new());
        let gh = gh_at(&program, clock.clone());
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(gh.gh(&args(&["issue", "list", "--repo", "rawphp/softfac"])));
        });
        let gpid = pid_in(&gpid_file);

        clock.advance(Duration::from_millis(29_999));
        assert!(
            rx.recv_timeout(Duration::from_millis(300)).is_err(),
            "answered before 30 s on the clock"
        );
        clock.advance(Duration::from_millis(1));
        let error = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("answered once 30 s passed on the clock")
            .unwrap_err();
        assert!(matches!(error, TrackerError::Timeout { .. }), "{error:?}");
        assert!(goes_away(gpid), "the descendant outlived the call");
    }

    #[test]
    fn the_command_sets_the_token_and_clears_every_other_source_of_credentials() {
        let gh = Gh::new("rawphp", TOKEN, Arc::new(FakeClock::new()));
        let command = gh.command(&args(&["api", "user"]));
        let envs: std::collections::HashMap<_, _> = command
            .get_envs()
            .map(|(key, value)| (key.to_owned(), value.map(|v| v.to_owned())))
            .collect();
        let set = |name: &str| envs.get(std::ffi::OsStr::new(name)).cloned();
        assert_eq!(set("GH_TOKEN"), Some(Some(TOKEN.into())));
        assert_eq!(set("GH_PROMPT_DISABLED"), Some(Some("1".into())));
        assert_eq!(set("GH_NO_UPDATE_NOTIFIER"), Some(Some("1".into())));
        for removed in [
            "GH_HOST",
            "GH_ENTERPRISE_TOKEN",
            "GITHUB_ENTERPRISE_TOKEN",
            "GITHUB_TOKEN",
        ] {
            assert_eq!(set(removed), Some(None), "{removed} is not removed");
        }
    }

    #[test]
    fn an_empty_token_never_starts_gh() {
        for token in ["", "  ", "\t\n"] {
            let dir = tempfile::tempdir().unwrap();
            let ran = dir.path().join("ran");
            let program = fake_gh(dir.path(), &format!("touch '{}'", ran.display()));
            let gh = Gh::new("rawphp", token, Arc::new(FakeClock::new())).with_program(&program);
            let error = gh.gh(&args(&["api", "user"])).unwrap_err();
            let TrackerError::Other(message) = error else {
                panic!("not Other: {error:?}");
            };
            assert!(message.contains("no token for rawphp"), "{message}");
            assert!(!ran.exists(), "gh ran with token {token:?}");
        }
    }

    #[test]
    fn a_gh_on_the_system_clock_answers_well_inside_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let program = fake_gh(dir.path(), "printf ok");
        let gh = gh_at(&program, crate::clock::system());
        assert_eq!(gh.gh(&args(&["api", "user"])).unwrap(), "ok");
    }

    #[test]
    fn output_past_the_cap_is_an_error_never_a_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let program = fake_gh(
            dir.path(),
            &format!("head -c {} /dev/zero", OUTPUT_LIMIT + 1),
        );
        let gh = gh_at(&program, Arc::new(FakeClock::new()));
        let error = gh.gh(&args(&["issue", "list"])).unwrap_err();
        assert_no_token(&error);
        assert_eq!(
            error.to_string(),
            format!("`gh issue list` printed more than {OUTPUT_LIMIT} bytes")
        );

        let exact = fake_gh(dir.path(), &format!("head -c {OUTPUT_LIMIT} /dev/zero"));
        let gh = gh_at(&exact, Arc::new(FakeClock::new()));
        assert_eq!(
            gh.gh(&args(&["issue", "list"])).unwrap().len(),
            OUTPUT_LIMIT
        );
    }

    #[test]
    fn a_gh_that_cannot_be_found_is_a_spawn_error() {
        let dir = tempfile::tempdir().unwrap();
        let gh = gh_at(&dir.path().join("no-such-gh"), Arc::new(FakeClock::new()));
        let error = gh.gh(&args(&["api", "user"])).unwrap_err();
        assert_no_token(&error);
        let TrackerError::Other(message) = error else {
            panic!("not Other");
        };
        assert!(
            message.starts_with("cannot start `gh api user`: "),
            "{message}"
        );
    }

    #[test]
    fn a_signal_killed_gh_is_a_failure_with_status_minus_1() {
        let dir = tempfile::tempdir().unwrap();
        let program = fake_gh(dir.path(), "kill -9 $$");
        let gh = gh_at(&program, Arc::new(FakeClock::new()));
        let error = gh.gh(&args(&["issue", "edit", "6"])).unwrap_err();
        assert_eq!(
            error,
            TrackerError::Other("`gh issue edit 6` exited with status -1: ".into())
        );
    }

    #[test]
    fn a_nul_byte_in_an_argument_is_refused_before_gh_starts_and_never_reaches_the_message() {
        let dir = tempfile::tempdir().unwrap();
        let ran = dir.path().join("ran");
        let program = fake_gh(dir.path(), &format!("touch '{}'", ran.display()));
        let gh = gh_at(&program, Arc::new(FakeClock::new()));
        let error = gh
            .gh(&args(&["issue", "comment", "6", "--body", "secret\0body"]))
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "cannot start `gh issue comment 6 --body <11 bytes>`: an argument contains a NUL byte"
        );
        assert!(!error.to_string().contains("secret"), "{error}");
        assert!(!ran.exists());
    }

    #[test]
    fn neither_debug_nor_an_error_shows_the_token() {
        let dir = tempfile::tempdir().unwrap();
        let program = fake_gh(
            dir.path(),
            "echo \"bad: $GH_TOKEN\" >/dev/null; echo nope >&2; exit 1",
        );
        let gh = gh_at(&program, Arc::new(FakeClock::new()));
        let shown = format!("{gh:?}");
        assert!(!shown.contains(TOKEN), "{shown}");
        assert!(shown.contains("rawphp"), "{shown}");
        assert_eq!(gh.login(), "rawphp");
        let error = gh.gh(&args(&["api", "user"])).unwrap_err();
        assert!(!error.to_string().contains(TOKEN));
        assert!(!format!("{error:?}").contains(TOKEN));
    }

    // finish: how a call's end is read, for the real gh and the fake alike.

    #[test]
    fn a_non_zero_exit_carries_args_status_and_stderr() {
        let call = args(&["issue", "list", "--repo", "rawphp/softfac"]);
        let error = finish(&call, Answer::fail(4, "unknown flag --x\n")).unwrap_err();
        assert_eq!(
            error.to_string(),
            "`gh issue list --repo rawphp/softfac` exited with status 4: unknown flag --x"
        );
    }

    #[test]
    fn only_a_failure_with_http_404_in_stderr_is_not_found() {
        let call = args(&["api", "repos/rawphp/softfac/branches/x"]);
        for stderr in ["gh: Not Found (HTTP 404)", "HTTP 404: Not Found\n"] {
            let error = finish(&call, Answer::fail(1, stderr)).unwrap_err();
            assert_eq!(
                error,
                TrackerError::NotFound {
                    command: "gh api repos/rawphp/softfac/branches/x".into(),
                    status: 1,
                    stderr: stderr.into(),
                }
            );
        }
        let error = finish(&call, Answer::fail(4, "HTTP 404: Not Found\n")).unwrap_err();
        assert_eq!(
            error,
            TrackerError::NotFound {
                command: "gh api repos/rawphp/softfac/branches/x".into(),
                status: 4,
                stderr: "HTTP 404: Not Found\n".into(),
            }
        );
        for other in [
            Answer::fail(1, "gh: Server Error (HTTP 500)"),
            Answer::fail(1, "gh: Forbidden (HTTP 403)"),
            Answer::TimedOut,
            Answer::NotStarted("HTTP 404".into()),
            Answer::TooLarge,
            Answer::fail(1, "HTTP 404 API rate limit"),
        ] {
            let error = finish(&call, other.clone()).unwrap_err();
            assert!(
                !matches!(error, TrackerError::NotFound { .. }),
                "{other:?} is {error:?}"
            );
        }
    }

    #[test]
    fn a_gh_the_shell_cannot_find_or_run_is_a_spawn_error_not_a_failed_call() {
        let call = args(&["issue", "list", "--repo", "rawphp/softfac"]);
        for (status, stderr) in [
            (127, "sh: gh: not found\n"),
            (126, "sh: /usr/local/bin/gh: Permission denied\n"),
        ] {
            let error = finish(&call, Answer::fail(status, stderr)).unwrap_err();
            assert_eq!(
                error,
                TrackerError::Other(format!(
                    "cannot start `gh issue list --repo rawphp/softfac`: {}",
                    stderr.trim()
                ))
            );
        }
        let error = finish(&call, Answer::fail(127, "")).unwrap_err();
        assert_eq!(
            error.to_string(),
            "cannot start `gh issue list --repo rawphp/softfac`: the shell exited with status 127"
        );
        let other = finish(&call, Answer::fail(2, "unknown flag")).unwrap_err();
        assert_eq!(
            other.to_string(),
            "`gh issue list --repo rawphp/softfac` exited with status 2: unknown flag"
        );
    }

    #[test]
    fn a_timed_out_gh_is_a_timeout_error() {
        let call = args(&["issue", "list", "--repo", "rawphp/softfac"]);
        let error = finish(&call, Answer::TimedOut).unwrap_err();
        assert_eq!(
            error,
            TrackerError::Timeout {
                command: "gh issue list --repo rawphp/softfac".into(),
                seconds: 30,
            }
        );
    }

    #[test]
    fn only_the_rate_limit_is_read_as_a_rate_limit() {
        let call = args(&["issue", "list", "--repo", "rawphp/softfac"]);
        for stderr in [
            "GraphQL: API rate limit already exceeded for user ID 1.",
            "gh: API rate limit exceeded (HTTP 403)",
            "HTTP 403: You have exceeded a secondary rate limit. Please wait a few minutes before you try again.",
            "gh: Too Many Requests (HTTP 429)",
        ] {
            let error = finish(&call, Answer::fail(1, stderr)).unwrap_err();
            assert_eq!(
                error,
                TrackerError::RateLimited {
                    command: "gh issue list --repo rawphp/softfac".into(),
                    stderr: stderr.into(),
                }
            );
            assert_eq!(
                error.to_string(),
                format!("`gh issue list --repo rawphp/softfac` hit GitHub's rate limit: {stderr}")
            );
        }
        let other = finish(&call, Answer::fail(1, "HTTP 404: Not Found")).unwrap_err();
        assert!(matches!(other, TrackerError::NotFound { .. }), "{other:?}");
    }

    #[test]
    fn a_success_that_mentions_the_rate_limit_is_not_an_error() {
        let call = args(&["issue", "list"]);
        assert_eq!(
            finish(&call, Answer::ok_warning("[]", "API rate limit")).unwrap(),
            "[]"
        );
    }

    #[test]
    fn refused_or_missing_credentials_are_auth() {
        let call = args(&["api", "user"]);
        for stderr in [
            "gh: Bad credentials (HTTP 401)",
            "HTTP 401: Bad credentials (https://api.github.com/graphql)",
            "To get started with GitHub CLI, please run:  gh auth login\n",
            "gh: This API operation needs the \"repo\" scope. Requires authentication (HTTP 401)",
        ] {
            let error = finish(&call, Answer::fail(1, stderr)).unwrap_err();
            assert_eq!(
                error,
                TrackerError::Auth {
                    command: "gh api user".into(),
                    stderr: stderr.into(),
                }
            );
            assert_eq!(
                error.to_string(),
                format!(
                    "`gh api user` was refused GitHub's authentication: {}",
                    stderr.trim()
                )
            );
        }
    }

    #[test]
    fn output_past_the_cap_is_too_large_and_a_spawn_failure_names_the_command() {
        let call = args(&["issue", "list", "--repo", "rawphp/softfac"]);
        assert_eq!(
            finish(&call, Answer::TooLarge).unwrap_err().to_string(),
            format!("`gh issue list --repo rawphp/softfac` printed more than {OUTPUT_LIMIT} bytes")
        );
        assert_eq!(
            finish(&call, Answer::NotStarted("gh not found".into()))
                .unwrap_err()
                .to_string(),
            "cannot start `gh issue list --repo rawphp/softfac`: gh not found"
        );
    }

    #[test]
    fn error_text_hides_comment_bodies_but_the_error_keeps_them() {
        // "hello\nworld" + "\n\n" + the 16-byte marker = 29 bytes.
        let call = args(&[
            "issue",
            "comment",
            "6",
            "--repo",
            "rawphp/softfac",
            "--body",
            "hello\nworld\n\n<!-- softfac -->",
        ]);
        assert_eq!(
            finish(&call, Answer::fail(1, "boom"))
                .unwrap_err()
                .to_string(),
            "`gh issue comment 6 --repo rawphp/softfac --body <29 bytes>` exited with status 1: boom"
        );
        // Counted in bytes, not characters: "héllo ✓" is 7 chars and 10 bytes, plus 18.
        let utf8 = args(&[
            "issue",
            "comment",
            "6",
            "--body",
            "héllo ✓\n\n<!-- softfac -->",
        ]);
        let shown = finish(&utf8, Answer::fail(1, "boom"))
            .unwrap_err()
            .to_string();
        assert!(shown.contains("--body <28 bytes>"), "{shown}");
        assert!(!shown.contains("llo"), "{shown}");
    }

    #[test]
    fn error_text_hides_raw_field_bodies() {
        let call = args(&[
            "api",
            "-X",
            "PATCH",
            "repos/rawphp/softfac/issues/comments/77",
            "-f",
            "body=private status\n\n<!-- softfac:status -->",
        ]);
        let shown = finish(&call, Answer::fail(1, "gone"))
            .unwrap_err()
            .to_string();
        assert!(
            shown.contains("repos/rawphp/softfac/issues/comments/77 -f body=<39 bytes>"),
            "{shown}"
        );
        assert!(!shown.contains("private"), "{shown}");
    }

    #[test]
    fn error_text_escapes_newlines_in_other_arguments() {
        assert_eq!(
            parse_error(&args(&["issue", "edit", "a\nb\r\nc"]), "x").to_string(),
            "`gh issue edit a\\nb\\r\\nc` returned unexpected output: x"
        );
        assert_eq!(
            invalid_argument("a\nb", "a plain file path").to_string(),
            "refused \"a\\nb\" before calling gh: expected a plain file path"
        );
        assert_eq!(
            finish(&args(&["--body"]), Answer::NotStarted("nope".into()))
                .unwrap_err()
                .to_string(),
            "cannot start `gh --body`: nope"
        );
    }
}
