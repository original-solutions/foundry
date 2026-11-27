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
            .key("k1")
            .after(["bash", "-lc", "true"])
            .stdin(b"body".to_vec())
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
        assert_eq!(vims.call(&Call::new("show").arg("wsp_1")).unwrap(), "done\n");
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
            ("PRECONDITION_FAILED", None),
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
    fn a_shell_that_cannot_find_vims_could_not_run() {
        let dir = tempfile::tempdir().unwrap();
        let program = fake_vims(dir.path(), "echo 'vims: not found' >&2\nexit 127");
        let vims = vims_at(&program, Arc::new(SystemClock));
        assert!(matches!(
            vims.call(&exec_call()),
            Err(VimsError::NotRun { .. })
        ));
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
            let call = Call::new("exec").arg("wsp_1").stdin(vec![b'x'; 1024 * 1024]);
            let _ = tx.send(vims.call(&call));
        });
        let pid = pid_in(&pid_file);
        clock.advance(LIMIT);
        let result = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("killed once 30 s passed on the clock");
        assert!(matches!(result, Err(VimsError::Timeout { .. })), "{result:?}");
        assert!(goes_away(pid), "vims outlived the call");
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
