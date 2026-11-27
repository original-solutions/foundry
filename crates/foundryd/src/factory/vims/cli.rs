//! The `vims` command line, each call ended within 30 seconds.
//!
//! [`VimsCli`] runs `vims --token-ref env:FOUNDRY_VIMS_TOKEN …` with the machine's Vims token
//! in that variable, in the child's environment only: the token is never an argument, a log
//! line or an error, and output that echoes it has it hidden before it is read. A call still
//! running 30 seconds after it started, on the factory's clock, is killed with everything it
//! started and is [`VimsError::Timeout`]. [`finish`] reads how a call ended, for the real
//! `vims` and the fake alike.

use std::fmt;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use serde_json::Value;

use super::{Call, Refusal, Vims, VimsError, one_line};
use crate::clock::Clock;

/// How long one `vims` call may run before it is killed.
pub const LIMIT: Duration = Duration::from_secs(30);

/// The most `vims` may print on stdout or stderr. Output is never cut short: past this the call
/// gives no answer that can be read.
pub const OUTPUT_LIMIT: usize = 8 * 1024 * 1024;

/// The variable that holds the token in the child's environment.
pub const TOKEN_VAR: &str = "FOUNDRY_VIMS_TOKEN";

/// What `--token-ref` is given: the variable, never the token.
pub const TOKEN_REF: &str = "env:FOUNDRY_VIMS_TOKEN";

/// The most of `vims`'s output an error quotes, from its end.
const QUOTED: usize = 400;

/// How often a running call is looked at.
const POLL: Duration = Duration::from_millis(10);

/// How one `vims` call ended, before it is read as an answer or a [`VimsError`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// `vims` exited: `status` is `None` when a signal ended it.
    Exit {
        status: Option<i32>,
        stdout: String,
        stderr: String,
    },
    /// `vims` was still running at [`LIMIT`] and was killed.
    TimedOut,
    /// `vims` printed more than [`OUTPUT_LIMIT`] bytes.
    TooLarge,
    /// `vims` could not be started, and why.
    NotStarted(String),
}

/// Reads how `call` ended: its stdout, or the error it is.
pub fn finish(call: &Call, outcome: Outcome) -> Result<String, VimsError> {
    let command = call.text();
    match outcome {
        Outcome::NotStarted(reason) => Err(VimsError::NotRun { command, reason }),
        Outcome::TimedOut => Err(VimsError::Timeout {
            command,
            seconds: LIMIT.as_secs(),
        }),
        Outcome::TooLarge => Err(VimsError::Unknown {
            command,
            reason: format!("it printed more than {OUTPUT_LIMIT} bytes"),
            code: None,
        }),
        Outcome::Exit {
            status: Some(0),
            stdout,
            ..
        } => Ok(stdout),
        Outcome::Exit {
            status,
            stdout,
            stderr,
        } => {
            let coded = coded(&stdout).or_else(|| coded(&stderr));
            if let Some(refusal) = coded.as_ref().filter(|coded| coded.is_vims_s()) {
                return Err(VimsError::Refused {
                    command,
                    refusal: refusal.clone(),
                });
            }
            let said = quoted(match stderr.trim() {
                "" => stdout.trim(),
                said => said,
            });
            let reason = match status {
                Some(status) => format!("it exited with status {status}: {said}"),
                None => format!("a signal ended it: {said}"),
            };
            Err(VimsError::Unknown {
                command,
                reason: reason.trim_end_matches([':', ' ']).to_owned(),
                code: coded.map(|coded| coded.code),
            })
        }
    }
}

/// The end of `text`, at most [`QUOTED`] bytes, on one line.
fn quoted(text: &str) -> String {
    let mut start = text.len().saturating_sub(QUOTED);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    let cut = if start > 0 { "…" } else { "" };
    format!("{cut}{}", one_line(&text[start..]))
}

/// The code and message of the failure `text` reports, when it names one: JSON with
/// `error.code` (or `code`, or an `error` that is a code), or a line that starts with a code,
/// such as `Error: PRECONDITION_FAILED: …`. The checks saw the codes and not the shape they came
/// in, so both are guesses. Whether the code is a refusal of Vims's is
/// [`Refusal::is_vims_s`]'s to say.
fn coded(text: &str) -> Option<Refusal> {
    let text = text.trim();
    let json = serde_json::from_str::<Value>(text).ok().or_else(|| {
        text.lines()
            .rev()
            .find_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
    });
    if let Some(value) = json.filter(Value::is_object) {
        let field = |path: &[&str]| {
            path.iter()
                .try_fold(&value, |value, name| value.get(name))
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(str::to_owned)
        };
        let code = [&["error", "code"][..], &["code"], &["error"]]
            .into_iter()
            .filter_map(&field)
            .find(|code| is_code(code))?;
        let message = field(&["error", "message"])
            .or_else(|| field(&["message"]))
            .unwrap_or_default();
        return Some(Refusal::new(code, message));
    }
    text.lines().find_map(|line| {
        let line = line.trim();
        let line = ["Error: ", "error: "]
            .iter()
            .find_map(|prefix| line.strip_prefix(prefix))
            .unwrap_or(line);
        let (code, message) = line.split_once(':').unwrap_or((line, ""));
        // In free text a code is a word with an `_` in it, so a capitalised word is not one.
        (is_code(code) && code.contains('_')).then(|| Refusal::new(code, message.trim()))
    })
}

/// Whether `text` reads as a code: capitals, digits and `_`.
fn is_code(text: &str) -> bool {
    text.len() >= 3
        && text.starts_with(|c: char| c.is_ascii_uppercase())
        && text
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// The `vims` command line, signed in with the machine's Vims token.
pub struct VimsCli {
    token: String,
    clock: Arc<dyn Clock>,
    program: PathBuf,
}

impl fmt::Debug for VimsCli {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VimsCli")
            .field("program", &self.program)
            .finish_non_exhaustive()
    }
}

impl VimsCli {
    /// The command line signed in with `token`, timing each call on `clock`.
    pub fn new(token: impl Into<String>, clock: Arc<dyn Clock>) -> Self {
        Self {
            token: token.into(),
            clock,
            program: PathBuf::from("vims"),
        }
    }

    /// The same command line running `program` in place of `vims`, for tests.
    pub fn with_program(mut self, program: impl Into<PathBuf>) -> Self {
        self.program = program.into();
        self
    }

    /// The command for one call: the token in its environment and named by reference in its
    /// arguments.
    fn command(&self, call: &Call) -> Command {
        let mut command = Command::new(&self.program);
        command
            .arg("--token-ref")
            .arg(TOKEN_REF)
            .args(call.argv())
            .env(TOKEN_VAR, &self.token)
            .stdin(if call.stdin().is_empty() {
                Stdio::null()
            } else {
                Stdio::piped()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Its own process group, so a call killed at the limit takes what it started too.
            .process_group(0);
        command
    }

    /// Runs `vims` once for `call`. The call is killed, with everything it started, once
    /// [`LIMIT`] has passed on the clock before `vims` has exited and closed its output.
    fn run(&self, call: &Call) -> Outcome {
        // Without a token `vims` could sign in as whoever its own configuration names.
        if self.token.trim().is_empty() {
            return Outcome::NotStarted("no Vims token is set".into());
        }
        if call.argv().iter().any(|arg| arg.contains('\0')) {
            return Outcome::NotStarted("an argument contains a NUL byte".into());
        }
        let started = self.clock.now();
        let mut child = match self.command(call).spawn() {
            Ok(child) => child,
            Err(error) => return Outcome::NotStarted(error.to_string()),
        };
        // The input goes in on a thread of its own, which is never waited on, so a `vims` that
        // does not read it cannot hold the call past its limit. The pipe closes when the input
        // is all written, or fails once nothing holds its other end.
        if let Some(mut pipe) = child.stdin.take() {
            let bytes = call.stdin().to_vec();
            thread::spawn(move || {
                let _ = pipe.write_all(&bytes);
            });
        }
        let stdout = drain(child.stdout.take());
        let stderr = drain(child.stderr.take());
        let mut status = None;
        loop {
            if status.is_none() {
                match child.try_wait() {
                    Ok(found) => status = found,
                    Err(error) => {
                        kill_group(&mut child);
                        return Outcome::NotStarted(error.to_string());
                    }
                }
            }
            if status.is_some() && stdout.is_finished() && stderr.is_finished() {
                break;
            }
            if self.clock.now().saturating_sub(started) >= LIMIT {
                kill_group(&mut child);
                return Outcome::TimedOut;
            }
            thread::sleep(POLL);
        }
        let (stdout, over_out) = stdout.join().unwrap_or_default();
        let (stderr, over_err) = stderr.join().unwrap_or_default();
        if over_out || over_err {
            return Outcome::TooLarge;
        }
        Outcome::Exit {
            status: status.and_then(|status| status.code()),
            stdout: self.hidden(&stdout),
            stderr: self.hidden(&stderr),
        }
    }

    /// `bytes` as text with the token hidden as `***`.
    fn hidden(&self, bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).replace(&self.token, "***")
    }
}

impl Vims for VimsCli {
    fn call(&self, call: &Call) -> Result<String, VimsError> {
        finish(call, self.run(call))
    }
}

/// Kills the child's process group and reaps the child.
fn kill_group(child: &mut Child) {
    if let Ok(pid) = i32::try_from(child.id()) {
        let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Reads `pipe` to its end on a thread of its own, keeping at most [`OUTPUT_LIMIT`] bytes, and
/// says whether there was more.
fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> JoinHandle<(Vec<u8>, bool)> {
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
                    let room = OUTPUT_LIMIT.saturating_sub(kept.len());
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
    use std::ffi::OsStr;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::mpsc;
    use std::time::Instant;

    use nix::unistd::Pid;

    use super::*;
    use crate::clock::{FakeClock, SystemClock};
    use crate::factory::vims::{Egress, Exec, Limit, Vims};

    const TOKEN: &str = "vims_madeUpToken1234567890abcdefABCDEF";

    /// A `vims` at `dir/vims` that runs `body` with `sh`.
    fn fake_vims(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("vims");
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn vims_at(program: &Path, clock: Arc<dyn Clock>) -> VimsCli {
        VimsCli::new(TOKEN, clock).with_program(program)
    }

    fn exec_call() -> Call {
        Call::new("exec")
            .arg("wsp_1")
            .arg("--no-wait")
            .keyed("k1")
            .after(["bash", "-lc", "true"])
            .input(b"body".to_vec())
    }

    fn assert_no_token(error: &VimsError) {
        assert!(!error.to_string().contains(TOKEN), "{error}");
        assert!(!format!("{error:?}").contains(TOKEN), "{error:?}");
    }

    #[test]
    fn the_command_line_names_the_token_by_reference_and_holds_it_in_no_argument() {
        let vims = VimsCli::new(TOKEN, Arc::new(SystemClock));
        let command = vims.command(&exec_call());
        let args: Vec<&OsStr> = command.get_args().collect();
        assert_eq!(
            args,
            [
                "--token-ref",
                "env:FOUNDRY_VIMS_TOKEN",
                "exec",
                "wsp_1",
                "--no-wait",
                "--idempotency-key",
                "k1",
                "--json",
                "--",
                "bash",
                "-lc",
                "true",
            ]
        );
        assert!(
            args.iter()
                .all(|arg| !arg.to_string_lossy().contains(TOKEN))
        );
        assert_eq!(command.get_program(), "vims");
        let token: Vec<_> = command
            .get_envs()
            .filter(|(name, _)| *name == OsStr::new(TOKEN_VAR))
            .collect();
        assert_eq!(token, [(OsStr::new(TOKEN_VAR), Some(OsStr::new(TOKEN)))]);
    }

    #[test]
    fn the_token_reaches_the_child_s_environment_and_the_input_its_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let seen = dir.path().join("seen");
        let program = fake_vims(
            dir.path(),
            &format!(
                "printf '%s\\n' \"$FOUNDRY_VIMS_TOKEN\" > '{seen}'\nprintf '%s\\n' \"$@\" >> '{seen}'\ncat >> '{seen}'\necho '{{\"id\":\"op_1\",\"status\":\"running\"}}'",
                seen = seen.display()
            ),
        );
        let vims = vims_at(&program, Arc::new(SystemClock));
        let exec = Exec {
            cwd: Some("/workspace/repo".into()),
            timeout_seconds: Some(1800),
            no_wait: true,
            argv: vec!["bash".into(), "-lc".into(), "cat > /tmp/x".into()],
            stdin: b"the body\nline two".to_vec(),
        };
        let operation = vims.exec("wsp_1", &exec, "k1").unwrap();
        assert_eq!(operation.id, "op_1");
        assert_eq!(
            fs::read_to_string(&seen).unwrap(),
            format!(
                "{TOKEN}\n--token-ref\nenv:FOUNDRY_VIMS_TOKEN\nexec\nwsp_1\n--no-wait\n--cwd\nworkspace/repo\n--timeout\n1800\n--idempotency-key\nk1\n--json\n--\nbash\n-lc\ncat > /tmp/x\nthe body\nline two"
            )
        );
    }

    #[test]
    fn a_call_with_no_input_gets_an_empty_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let program = fake_vims(dir.path(), "cat\necho done");
        let vims = vims_at(&program, Arc::new(SystemClock));
        assert_eq!(
            vims.call(&Call::new("show").arg("wsp_1")).unwrap(),
            "done\n"
        );
    }

    #[test]
    fn a_refusal_carries_vims_s_code_and_message() {
        let dir = tempfile::tempdir().unwrap();
        let program = fake_vims(
            dir.path(),
            "echo '{\"error\":{\"code\":\"PRECONDITION_FAILED\",\"message\":\"workspace is paused\"}}'\nexit 1",
        );
        let vims = vims_at(&program, Arc::new(SystemClock));
        let error = vims.call(&exec_call()).unwrap_err();
        let refusal = error.refusal().expect("a refusal");
        assert_eq!(refusal.code, "PRECONDITION_FAILED");
        assert_eq!(refusal.message, "workspace is paused");
        assert_eq!(refusal.limit, None);
        assert_eq!(
            error.to_string(),
            "`vims exec wsp_1 --no-wait --idempotency-key k1 --json -- bash -lc true <4 bytes on stdin>` was refused by Vims: PRECONDITION_FAILED: workspace is paused"
        );
    }

    #[test]
    fn a_refusal_on_stderr_is_read_too() {
        let dir = tempfile::tempdir().unwrap();
        let program = fake_vims(
            dir.path(),
            "echo '{\"code\":\"IDEMPOTENCY_KEY_REUSED\",\"message\":\"key k1 was used\"}' >&2\nexit 2",
        );
        let vims = vims_at(&program, Arc::new(SystemClock));
        let error = vims.call(&exec_call()).unwrap_err();
        assert_eq!(error.code(), Some("IDEMPOTENCY_KEY_REUSED"));
    }

    #[test]
    fn a_refusal_in_plain_text_is_read_by_its_code() {
        let dir = tempfile::tempdir().unwrap();
        let program = fake_vims(
            dir.path(),
            "echo 'Error: HOST_COMMAND_TIMEOUT: the command ran past 1800 s' >&2\nexit 1",
        );
        let vims = vims_at(&program, Arc::new(SystemClock));
        let error = vims.call(&exec_call()).unwrap_err();
        assert_eq!(error.code(), Some("HOST_COMMAND_TIMEOUT"));
    }

    #[test]
    fn a_refusal_for_quota_credit_or_a_member_s_limit_says_which() {
        for (code, limit) in [
            ("QUOTA_EXCEEDED", Some(Limit::Quota)),
            ("VCPU_QUOTA_EXCEEDED", Some(Limit::Quota)),
            ("INSUFFICIENT_CREDIT", Some(Limit::Credit)),
            ("MEMBER_LIMIT_EXCEEDED", Some(Limit::Member)),
            ("CREDIT_EXHAUSTED", Some(Limit::Credit)),
            ("PRECONDITION_FAILED", None),
            ("MEMBER_NOT_FOUND", None),
            ("NOT_A_MEMBER", None),
            ("CREDIT_CARD_DECLINED", None),
        ] {
            let refusal = Refusal::new(code, "");
            assert_eq!(refusal.limit, limit, "{code}");
        }
    }

    #[test]
    fn a_failure_with_no_code_of_vims_s_is_no_answer_that_can_be_read() {
        let dir = tempfile::tempdir().unwrap();
        let program = fake_vims(dir.path(), "echo 'fetch failed: ECONNRESET' >&2\nexit 1");
        let vims = vims_at(&program, Arc::new(SystemClock));
        let error = vims.call(&exec_call()).unwrap_err();
        assert!(matches!(error, VimsError::Unknown { .. }), "{error:?}");
        assert!(error.to_string().contains("ECONNRESET"), "{error}");
    }

    #[test]
    fn a_vims_that_cannot_be_found_could_not_run() {
        let vims = VimsCli::new(TOKEN, Arc::new(SystemClock)).with_program("/no/such/vims");
        let error = vims.call(&exec_call()).unwrap_err();
        assert!(matches!(error, VimsError::NotRun { .. }), "{error:?}");
        assert_no_token(&error);
    }

    #[test]
    fn an_exit_of_126_or_127_from_a_started_vims_is_no_answer_that_can_be_read() {
        // `vims` is started with no shell between, so these come from it or what it ran.
        let dir = tempfile::tempdir().unwrap();
        for status in [126, 127] {
            let program = fake_vims(dir.path(), &format!("echo 'not found' >&2\nexit {status}"));
            let vims = vims_at(&program, Arc::new(SystemClock));
            assert!(matches!(
                vims.call(&exec_call()),
                Err(VimsError::Unknown { .. })
            ));
        }
    }

    #[test]
    fn a_transport_failure_in_vims_s_error_shape_is_no_answer_that_can_be_read() {
        let dir = tempfile::tempdir().unwrap();
        for script in [
            "echo '{\"error\":{\"code\":\"ECONNRESET\",\"message\":\"socket hang up\"}}'\nexit 1",
            "echo '{\"code\":\"econnreset\"}' >&2\nexit 1",
            "echo '{\"error\":{\"code\":\"ERR_SOCKET_CONNECTION_TIMEOUT\"}}'\nexit 1",
            "echo '{\"error\":{\"code\":\"UND_ERR_SOCKET\"}}'\nexit 1",
            "echo 'Error: UND_ERR_SOCKET: other side closed' >&2\nexit 1",
            "echo 'Error: EAI_AGAIN: getaddrinfo vims.example' >&2\nexit 1",
        ] {
            let program = fake_vims(dir.path(), script);
            let error = vims_at(&program, Arc::new(SystemClock))
                .call(&exec_call())
                .unwrap_err();
            assert!(
                matches!(error, VimsError::Unknown { .. }),
                "{script}: {error:?}"
            );
        }
    }

    #[test]
    fn a_code_vims_is_not_known_to_refuse_with_is_no_answer_and_keeps_its_code() {
        let dir = tempfile::tempdir().unwrap();
        for (script, code) in [
            (
                "echo '{\"error\":{\"code\":\"SERVICE_UNAVAILABLE\",\"message\":\"upstream\"}}'\nexit 1",
                Some("SERVICE_UNAVAILABLE"),
            ),
            (
                "echo '{\"error\":{\"code\":\"GATEWAY_TIMEOUT\"}}'\nexit 1",
                Some("GATEWAY_TIMEOUT"),
            ),
            (
                "echo '{\"error\":{\"code\":\"NETWORK_ERROR\"}}'\nexit 1",
                Some("NETWORK_ERROR"),
            ),
            (
                "echo '{\"code\":\"INTERNAL_ERROR\"}' >&2\nexit 1",
                Some("INTERNAL_ERROR"),
            ),
            (
                "echo 'Error: CONNECTION_RESET: other side closed' >&2\nexit 1",
                Some("CONNECTION_RESET"),
            ),
            ("echo 'FATAL: connection lost' >&2\nexit 1", None),
            ("echo 'WARNING: retrying' >&2\nexit 1", None),
            // Each says Vims did, or is doing, the work, so none is "took nothing".
            (
                "echo '{\"error\":{\"code\":\"CONFLICT\"}}'\nexit 1",
                Some("CONFLICT"),
            ),
            (
                "echo '{\"error\":{\"code\":\"ALREADY_EXISTS\"}}'\nexit 1",
                Some("ALREADY_EXISTS"),
            ),
            (
                "echo 'Error: HOST_COMMAND_TIMEOUT: the command ran past 1800 s' >&2\nexit 1",
                Some("HOST_COMMAND_TIMEOUT"),
            ),
        ] {
            let program = fake_vims(dir.path(), script);
            let error = vims_at(&program, Arc::new(SystemClock))
                .call(&exec_call())
                .unwrap_err();
            match &error {
                VimsError::Unknown { code: found, .. } => {
                    assert_eq!(found.as_deref(), code, "{script}")
                }
                other => panic!("{script}: {other:?}"),
            }
            assert_eq!(error.refusal(), None, "{script}");
            assert_eq!(error.code(), code, "{script}");
        }
    }

    #[test]
    fn a_code_that_says_vims_refused_is_a_refusal() {
        let dir = tempfile::tempdir().unwrap();
        for code in [
            "IDEMPOTENCY_KEY_REUSED",
            "PRECONDITION_FAILED",
            "QUOTA_EXCEEDED",
            "NOT_FOUND",
            "UNAUTHORIZED",
            "FORBIDDEN",
            "INVALID_ARGUMENT",
        ] {
            let program = fake_vims(
                dir.path(),
                &format!("echo '{{\"error\":{{\"code\":\"{code}\"}}}}'\nexit 1"),
            );
            let error = vims_at(&program, Arc::new(SystemClock))
                .call(&exec_call())
                .unwrap_err();
            assert_eq!(
                error.refusal().map(|refusal| refusal.code.as_str()),
                Some(code)
            );
        }
    }

    #[test]
    fn an_error_cut_inside_a_character_is_cut_after_it() {
        let dir = tempfile::tempdir().unwrap();
        let program = fake_vims(
            dir.path(),
            "i=0; while [ $i -lt 500 ]; do printf 'é'; i=$((i+1)); done\nprintf 'x' \nexit 1",
        );
        let text = vims_at(&program, Arc::new(SystemClock))
            .call(&exec_call())
            .unwrap_err()
            .to_string();
        // 1001 bytes; the cut at byte 601 falls inside an `é`, so it moves on to 602.
        assert!(
            text.ends_with(&format!(": …{}x", "é".repeat(199))),
            "{text}"
        );
    }

    #[test]
    fn a_refusal_as_a_bare_error_code_or_after_other_output_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let program = fake_vims(
            dir.path(),
            "echo '{\"error\":\"PRECONDITION_FAILED\"}'\nexit 1",
        );
        let error = vims_at(&program, Arc::new(SystemClock))
            .call(&exec_call())
            .unwrap_err();
        assert_eq!(error.code(), Some("PRECONDITION_FAILED"));
        let program = fake_vims(
            dir.path(),
            "echo 'connecting...' >&2\necho '{\"error\":{\"code\":\"QUOTA_EXCEEDED\"}}' >&2\nexit 1",
        );
        let error = vims_at(&program, Arc::new(SystemClock))
            .call(&exec_call())
            .unwrap_err();
        assert_eq!(error.code(), Some("QUOTA_EXCEEDED"));
    }

    #[test]
    fn the_output_an_error_quotes_is_one_line_and_cut_to_its_end() {
        let dir = tempfile::tempdir().unwrap();
        let program = fake_vims(
            dir.path(),
            "i=0; while [ $i -lt 2000 ]; do echo \"line $i of the output\"; i=$((i+1)); done\necho 'the end'\nexit 1",
        );
        let error = vims_at(&program, Arc::new(SystemClock))
            .call(&exec_call())
            .unwrap_err();
        let text = error.to_string();
        assert!(matches!(error, VimsError::Unknown { .. }), "{error:?}");
        assert!(!text.contains('\n'), "{text}");
        assert!(text.len() < 1000, "{} bytes", text.len());
        assert!(text.contains("the end"), "{text}");
    }

    #[test]
    fn an_empty_token_never_starts_vims() {
        let dir = tempfile::tempdir().unwrap();
        let ran = dir.path().join("ran");
        let program = fake_vims(dir.path(), &format!("touch '{}'", ran.display()));
        let vims = VimsCli::new(" ", Arc::new(SystemClock)).with_program(&program);
        assert!(matches!(
            vims.call(&exec_call()),
            Err(VimsError::NotRun { .. })
        ));
        assert!(!ran.exists());
    }

    #[test]
    fn a_vims_that_prints_the_token_never_puts_it_in_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let program = fake_vims(
            dir.path(),
            "echo \"bad token $FOUNDRY_VIMS_TOKEN\" >&2\necho \"{\\\"error\\\":{\\\"code\\\":\\\"UNAUTHORIZED\\\",\\\"message\\\":\\\"bad token $FOUNDRY_VIMS_TOKEN\\\"}}\"\nexit 1",
        );
        let vims = vims_at(&program, Arc::new(SystemClock));
        let error = vims.call(&exec_call()).unwrap_err();
        assert_eq!(error.code(), Some("UNAUTHORIZED"));
        assert_no_token(&error);
        let program = fake_vims(dir.path(), "echo \"no $FOUNDRY_VIMS_TOKEN\" >&2\nexit 1");
        let error = vims_at(&program, Arc::new(SystemClock))
            .call(&exec_call())
            .unwrap_err();
        assert_no_token(&error);
        assert!(error.to_string().contains("no ***"), "{error}");
    }

    #[test]
    fn debug_never_shows_the_token() {
        let vims = VimsCli::new(TOKEN, Arc::new(SystemClock));
        assert!(!format!("{vims:?}").contains(TOKEN));
    }

    #[test]
    fn output_past_the_cap_is_no_answer_that_can_be_read() {
        let dir = tempfile::tempdir().unwrap();
        let program = fake_vims(
            dir.path(),
            &format!("head -c {} /dev/zero", OUTPUT_LIMIT + 1),
        );
        let vims = vims_at(&program, Arc::new(SystemClock));
        assert!(matches!(
            vims.call(&exec_call()),
            Err(VimsError::Unknown { .. })
        ));
    }

    #[test]
    fn a_signal_killed_vims_is_no_answer_that_can_be_read() {
        let dir = tempfile::tempdir().unwrap();
        let program = fake_vims(dir.path(), "kill -9 $$");
        let vims = vims_at(&program, Arc::new(SystemClock));
        assert!(matches!(
            vims.call(&exec_call()),
            Err(VimsError::Unknown { .. })
        ));
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
    fn a_vims_with_no_answer_is_killed_at_30_seconds_on_the_clock() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let child_file = dir.path().join("child");
        // A distinctive duration, so a stray is easy to tell from any other sleep.
        let program = fake_vims(
            dir.path(),
            &format!(
                "echo $$ > '{}'\nsleep 53.4171 &\necho $! > '{}'\nwait",
                pid_file.display(),
                child_file.display()
            ),
        );
        let clock = Arc::new(FakeClock::new());
        let vims = vims_at(&program, clock.clone());
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(vims.call(&Call::new("show").arg("wsp_1")));
        });
        let pid = pid_in(&pid_file);
        let child = pid_in(&child_file);
        clock.advance(Duration::from_millis(29_999));
        assert!(
            rx.recv_timeout(Duration::from_millis(300)).is_err(),
            "killed before 30 s on the clock"
        );
        clock.advance(Duration::from_millis(1));
        let error = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("killed once 30 s passed on the clock")
            .unwrap_err();
        assert_eq!(
            error,
            VimsError::Timeout {
                command: "vims show wsp_1 --json".into(),
                seconds: 30
            }
        );
        assert!(goes_away(pid), "vims outlived the call");
        assert!(goes_away(child), "what vims started outlived the call");
    }

    #[test]
    fn a_vims_that_never_reads_a_large_input_is_still_killed_at_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let program = fake_vims(
            dir.path(),
            &format!("echo $$ > '{}'\nsleep 53.4172", pid_file.display()),
        );
        let clock = Arc::new(FakeClock::new());
        let vims = vims_at(&program, clock.clone());
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let call = Call::new("exec")
                .arg("wsp_1")
                .input(vec![b'x'; 1024 * 1024]);
            let _ = tx.send(vims.call(&call));
        });
        let pid = pid_in(&pid_file);
        clock.advance(LIMIT);
        let result = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("killed once 30 s passed on the clock");
        assert!(
            matches!(result, Err(VimsError::Timeout { .. })),
            "{result:?}"
        );
        assert!(goes_away(pid), "vims outlived the call");
    }

    #[test]
    fn a_vims_that_exits_while_a_child_holds_its_output_is_killed_at_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let child_file = dir.path().join("child");
        let program = fake_vims(
            dir.path(),
            &format!(
                "sleep 53.4173 &\necho $! > '{}'\nexit 0",
                child_file.display()
            ),
        );
        let clock = Arc::new(FakeClock::new());
        let vims = vims_at(&program, clock.clone());
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(vims.call(&Call::new("show").arg("wsp_1")));
        });
        let child = pid_in(&child_file);
        assert!(
            rx.recv_timeout(Duration::from_millis(300)).is_err(),
            "answered while a child held the output"
        );
        clock.advance(LIMIT);
        let result = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("killed once 30 s passed on the clock");
        assert!(
            matches!(result, Err(VimsError::Timeout { .. })),
            "{result:?}"
        );
        assert!(goes_away(child), "the child outlived the call");
    }

    #[test]
    fn a_nul_byte_in_an_argument_never_starts_vims() {
        let vims = VimsCli::new(TOKEN, Arc::new(SystemClock));
        assert!(matches!(
            vims.call(&Call::new("show").arg("wsp\0")),
            Err(VimsError::NotRun { .. })
        ));
    }

    #[test]
    fn a_new_workspace_answered_by_a_real_process_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let program = fake_vims(
            dir.path(),
            "echo '{\"operation\":{\"id\":\"op_1\",\"status\":\"queued\",\"workspaceId\":\"wsp_7\"}}'",
        );
        let vims = vims_at(&program, Arc::new(SystemClock));
        let operation = vims
            .new_workspace("golden:abc", Egress::Closed, "k1")
            .unwrap();
        assert_eq!(operation.id, "op_1");
        assert_eq!(operation.workspace.as_deref(), Some("wsp_7"));
    }
}
