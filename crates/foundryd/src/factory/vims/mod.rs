//! The `vims` command line behind a trait: the real one, or a scripted fake in tests.
//!
//! [`Vims`] has one required call, [`Vims::call`], which runs `vims` once. Every command P2c
//! uses is a provided method built on it, so the real [`VimsCli`] and the [`FakeVims`] build each
//! command line and read each answer by the same code, as `factory::github` does for `gh`. Each
//! call is one `vims … --json`; one that makes or changes something carries its idempotency key
//! ([`keys`]). A call answers within 30 seconds or is [`VimsError::Timeout`].
//!
//! The real client signs in with the machine's Vims token, `secrets::VIMS`, which reaches `vims`
//! only through the child's environment, named by `--token-ref env:FOUNDRY_VIMS_TOKEN`.
//!
//! Vims is the first host of several (P2c, "Hosts"). What is here is Vims's own. Nothing calls
//! it yet: the Vims host does, once it is built.

pub mod answer;
pub mod cli;
pub mod fake;
pub mod keys;

use std::fmt;

pub use answer::{
    Change, Connection, GithubLink, GithubLinks, OpState, Operation, Template, TemplateSource,
    Workspace, WorkspaceState,
};
pub use cli::{LIMIT, Outcome, TOKEN_REF, TOKEN_VAR, VimsCli};
pub use fake::{FakeVims, Reply};
pub use keys::{Role, Step, key};

use crate::factory::github::gh::one_line;

/// One `vims` call: its command, its arguments, its idempotency key, the argv it runs after
/// `--`, and the bytes for its standard input. Every call answers in JSON.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Call {
    command: &'static str,
    args: Vec<String>,
    key: Option<String>,
    after: Vec<String>,
    input: Vec<u8>,
}

impl Call {
    /// A call of `command`, its words separated by one space, such as `operation show`.
    pub fn new(command: &'static str) -> Self {
        Self {
            command,
            args: Vec::new(),
            key: None,
            after: Vec::new(),
            input: Vec::new(),
        }
    }

    /// The call with `arg` after the arguments it has.
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// The call carrying the idempotency key `key`.
    pub fn keyed(mut self, key: impl Into<String>) -> Self {
        self.key = Some(key.into());
        self
    }

    /// The call running `argv` after `--`.
    pub fn after<I, S>(mut self, argv: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.after = argv.into_iter().map(Into::into).collect();
        self
    }

    /// The call with `input` on its standard input.
    pub fn input(mut self, input: Vec<u8>) -> Self {
        self.input = input;
        self
    }

    /// The command, such as `operation show`.
    pub fn command(&self) -> &str {
        self.command
    }

    /// The idempotency key the call carries.
    pub fn key(&self) -> Option<&str> {
        self.key.as_deref()
    }

    /// The bytes for its standard input.
    pub fn stdin(&self) -> &[u8] {
        &self.input
    }

    /// The arguments after `vims`: the command's words, its arguments, its key, `--json`, and
    /// then `--` and the argv it runs.
    pub fn argv(&self) -> Vec<String> {
        let mut argv: Vec<String> = self.command.split(' ').map(str::to_owned).collect();
        argv.extend(self.args.iter().cloned());
        if let Some(key) = &self.key {
            argv.push("--idempotency-key".into());
            argv.push(key.clone());
        }
        argv.push("--json".into());
        if !self.after.is_empty() {
            argv.push("--".into());
            argv.extend(self.after.iter().cloned());
        }
        argv
    }

    /// Whether `other` asks for the same thing, whatever key either carries.
    pub fn same_request(&self, other: &Call) -> bool {
        self.command == other.command
            && self.args == other.args
            && self.after == other.after
            && self.input == other.input
    }

    /// The command for messages, on one line, with its input shown as its size.
    pub fn text(&self) -> String {
        let mut shown = vec!["vims".to_string()];
        shown.extend(self.argv().iter().map(|arg| one_line(arg)));
        if !self.input.is_empty() {
            shown.push(format!("<{} bytes on stdin>", self.input.len()));
        }
        shown.join(" ")
    }
}

/// Why a `vims` call gave no answer to act on. The text names the command, never the token,
/// which reaches `vims` only through its environment, and never a call's input.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum VimsError {
    /// Vims answered and refused the call, with its code.
    #[error("`{command}` was refused by Vims: {refusal}")]
    Refused { command: String, refusal: Refusal },
    /// No answer within [`LIMIT`]: `vims` was killed with everything it started. Vims may have
    /// taken the call; its key finds out.
    #[error("`{command}` gave no answer within {seconds} s")]
    Timeout { command: String, seconds: u64 },
    /// `vims` could not be run, so Vims never saw the call.
    #[error("cannot run `{command}`: {reason}")]
    NotRun { command: String, reason: String },
    /// `vims` ran and gave no answer that can be read: it failed with no code of Vims's, a
    /// signal ended it, or it printed too much or what the call did not expect. Vims may have
    /// taken the call; its key finds out.
    #[error("`{command}` gave no answer Foundry can read: {reason}")]
    Unknown { command: String, reason: String },
}

impl VimsError {
    /// Vims's refusal, when it refused.
    pub fn refusal(&self) -> Option<&Refusal> {
        match self {
            VimsError::Refused { refusal, .. } => Some(refusal),
            _ => None,
        }
    }

    /// Vims's code, when it refused.
    pub fn code(&self) -> Option<&str> {
        self.refusal().map(|refusal| refusal.code.as_str())
    }
}

/// Vims's refusal of a call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// Vims's code, such as `IDEMPOTENCY_KEY_REUSED`, `PRECONDITION_FAILED` or
    /// `HOST_COMMAND_TIMEOUT`.
    pub code: String,
    /// What Vims said.
    pub message: String,
    /// The limit it was refused for, when it was refused for quota, credit or a member's limit.
    pub limit: Option<Limit>,
}

impl Refusal {
    /// The refusal `code` with `message`, its limit read from the code.
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        let code = code.into();
        let limit = Limit::of(&code);
        Self {
            code,
            message: message.into(),
            limit,
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.message.is_empty() {
            f.write_str(&self.code)
        } else {
            write!(f, "{}: {}", self.code, self.message)
        }
    }
}

/// The limit a refusal was for. A run that meets one waits and spends no retry (P2c, V10).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Limit {
    /// The account's quota, such as its vCPU.
    Quota,
    /// The account's credit.
    Credit,
    /// A member's own limit.
    Member,
}

impl Limit {
    /// The limit `code` names. P2c saw none of these codes, so this is a guess: a code that
    /// speaks of `QUOTA`, `CREDIT` or a `MEMBER`.
    fn of(code: &str) -> Option<Self> {
        let code = code.to_ascii_uppercase();
        if code.contains("QUOTA") {
            Some(Limit::Quota)
        } else if code.contains("CREDIT") {
            Some(Limit::Credit)
        } else if code.contains("MEMBER") {
            Some(Limit::Member)
        } else {
            None
        }
    }
}

/// Whether a new workspace may reach the network.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Egress {
    /// The template's own egress.
    Open,
    /// `--egress closed`: only through a grant and Vims's package caches.
    Closed,
}

/// A command for `vims exec` to run in a workspace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Exec {
    /// The folder it runs in, as a path in the workspace. Vims takes it without its leading
    /// `/`, which the call drops.
    pub cwd: Option<String>,
    /// How long it may run, in seconds.
    pub timeout_seconds: Option<u64>,
    /// `--no-wait`: answer with the operation at once.
    pub no_wait: bool,
    /// The argv, run with no shell.
    pub argv: Vec<String>,
    /// The bytes for its standard input.
    pub stdin: Vec<u8>,
}

/// The `vims` command line. Each provided method is one command P2c uses.
pub trait Vims: Send + Sync {
    /// Runs `vims` once for `call` and answers its stdout, or why it gave none.
    fn call(&self, call: &Call) -> Result<String, VimsError>;

    /// `vims new <source key>`: a workspace made from `source_key` (`golden:<digest>`).
    fn new_workspace(
        &self,
        source_key: &str,
        egress: Egress,
        key: &str,
    ) -> Result<Operation, VimsError> {
        let mut call = Call::new("new").arg(source_key);
        if egress == Egress::Closed {
            call = call.arg("--egress").arg("closed");
        }
        let call = call.keyed(key);
        read(&call, self.call(&call)?, Operation::read, "an operation")
    }

    /// `vims exec <workspace>`: runs `exec`'s argv, with its input on standard input.
    fn exec(&self, workspace: &str, exec: &Exec, key: &str) -> Result<Operation, VimsError> {
        let mut call = Call::new("exec").arg(workspace);
        if exec.no_wait {
            call = call.arg("--no-wait");
        }
        if let Some(cwd) = &exec.cwd {
            call = call.arg("--cwd").arg(cwd.trim_start_matches('/'));
        }
        if let Some(seconds) = exec.timeout_seconds {
            call = call.arg("--timeout").arg(seconds.to_string());
        }
        let call = call
            .keyed(key)
            .after(exec.argv.iter().cloned())
            .input(exec.stdin.clone());
        read(&call, self.call(&call)?, Operation::read, "an operation")
    }

    /// `vims operation show <operation>`.
    fn operation(&self, operation: &str) -> Result<Operation, VimsError> {
        let call = Call::new("operation show").arg(operation);
        read(&call, self.call(&call)?, Operation::read, "an operation")
    }

    /// `vims operation list` of the operations a key made: none for a key Vims never took.
    fn operations_by_key(&self, key: &str) -> Result<Vec<Operation>, VimsError> {
        // Guessed: the list is filtered by the same flag that carries a key.
        let call = Call::new("operation list")
            .arg("--idempotency-key")
            .arg(key);
        read(
            &call,
            self.call(&call)?,
            Operation::read_list,
            "a list of operations",
        )
    }

    /// `vims operation logs <operation>`: its output.
    fn operation_logs(&self, operation: &str) -> Result<String, VimsError> {
        let call = Call::new("operation logs").arg(operation);
        Ok(answer::read_logs(&self.call(&call)?))
    }

    /// `vims operation cancel <operation>`.
    fn cancel(&self, operation: &str, key: &str) -> Result<Change, VimsError> {
        let call = Call::new("operation cancel").arg(operation).keyed(key);
        read(
            &call,
            self.call(&call)?,
            Change::read,
            "an operation or nothing",
        )
    }

    /// `vims pause <workspace>`.
    fn pause(&self, workspace: &str, key: &str) -> Result<Change, VimsError> {
        change(self, "pause", workspace, key)
    }

    /// `vims start <workspace>`.
    fn start(&self, workspace: &str, key: &str) -> Result<Change, VimsError> {
        change(self, "start", workspace, key)
    }

    /// `vims stop <workspace>`.
    fn stop(&self, workspace: &str, key: &str) -> Result<Change, VimsError> {
        change(self, "stop", workspace, key)
    }

    /// `vims remove <workspace> --in-use`.
    fn remove(&self, workspace: &str, key: &str) -> Result<Change, VimsError> {
        let call = Call::new("remove")
            .arg(workspace)
            .arg("--in-use")
            .keyed(key);
        read(
            &call,
            self.call(&call)?,
            Change::read,
            "an operation or nothing",
        )
    }

    /// `vims hold <workspace> <name>`, and with `off` when `on` is false. A hold is its own key
    /// and safe to repeat, so it carries no idempotency key.
    fn hold(&self, workspace: &str, name: &str, on: bool) -> Result<(), VimsError> {
        let mut call = Call::new("hold").arg(workspace).arg(name);
        if !on {
            call = call.arg("off");
        }
        self.call(&call).map(drop)
    }

    /// `vims show <workspace>`.
    fn show(&self, workspace: &str) -> Result<Workspace, VimsError> {
        let call = Call::new("show").arg(workspace);
        read(&call, self.call(&call)?, Workspace::read, "a workspace")
    }

    /// `vims templates show <template>`.
    fn template(&self, template: &str) -> Result<Template, VimsError> {
        let call = Call::new("templates show").arg(template);
        read(&call, self.call(&call)?, Template::read, "a template")
    }

    /// `vims workspace turn-grant put <workspace> --turn <turn> --read-only <connection> --for
    /// <for_>`: read-only access through `connection` for `for_`, such as `30m`.
    fn grant(
        &self,
        workspace: &str,
        turn: &str,
        connection: &str,
        for_: &str,
        key: &str,
    ) -> Result<Change, VimsError> {
        let call = Call::new("workspace turn-grant put")
            .arg(workspace)
            .arg("--turn")
            .arg(turn)
            .arg("--read-only")
            .arg(connection)
            .arg("--for")
            .arg(for_)
            .keyed(key);
        read(
            &call,
            self.call(&call)?,
            Change::read,
            "an operation or nothing",
        )
    }

    /// `vims workspace turn-grant rm <workspace> --turn <turn> <connection>`: the grant taken
    /// away.
    fn ungrant(
        &self,
        workspace: &str,
        turn: &str,
        connection: &str,
        key: &str,
    ) -> Result<Change, VimsError> {
        // Guessed: P2c gives `rm` no shape; this one mirrors `put`.
        let call = Call::new("workspace turn-grant rm")
            .arg(workspace)
            .arg("--turn")
            .arg(turn)
            .arg(connection)
            .keyed(key);
        read(
            &call,
            self.call(&call)?,
            Change::read,
            "an operation or nothing",
        )
    }

    /// `vims connection list`.
    fn connections(&self) -> Result<Vec<Connection>, VimsError> {
        let call = Call::new("connection list");
        read(
            &call,
            self.call(&call)?,
            Connection::read_list,
            "a list of connections",
        )
    }

    /// `vims github`: the GitHub logins Vims acts as.
    fn github(&self) -> Result<GithubLinks, VimsError> {
        let call = Call::new("github");
        read(&call, self.call(&call)?, GithubLinks::read, "GitHub logins")
    }

    /// Whether the Vims profile holds the skill `skill`, such as `factory-build`.
    fn profile_has_skill(&self, skill: &str) -> Result<bool, VimsError> {
        // Guessed: P2c names `vims profile save` only; the read is `profile show`.
        let call = Call::new("profile show");
        let stdout = self.call(&call)?;
        read(
            &call,
            stdout,
            |stdout| answer::profile_has_skill(stdout, skill),
            "a profile",
        )
    }
}

/// `vims <command> <workspace>` with `key`, for a change with no other argument.
fn change<V: Vims + ?Sized>(
    vims: &V,
    command: &'static str,
    workspace: &str,
    key: &str,
) -> Result<Change, VimsError> {
    let call = Call::new(command).arg(workspace).keyed(key);
    read(
        &call,
        vims.call(&call)?,
        Change::read,
        "an operation or nothing",
    )
}

/// `stdout` read by `parse`, or the call's answer that cannot be read.
fn read<T>(
    call: &Call,
    stdout: String,
    parse: impl FnOnce(&str) -> Option<T>,
    expected: &str,
) -> Result<T, VimsError> {
    parse(&stdout).ok_or_else(|| VimsError::Unknown {
        command: call.text(),
        reason: format!(
            "expected {expected} in its {} bytes of output",
            stdout.len()
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(fake: &FakeVims) -> Vec<String> {
        fake.calls().last().expect("a call").argv()
    }

    fn strings(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| part.to_string()).collect()
    }

    #[test]
    fn new_makes_a_workspace_from_a_source_key_with_egress_closed_only_when_asked() {
        let fake = FakeVims::new();
        fake.on("new", Reply::ok(r#"{"id":"op_1"}"#))
            .on("new", Reply::ok(r#"{"id":"op_2"}"#));
        fake.new_workspace("golden:abc", Egress::Open, "k1")
            .unwrap();
        assert_eq!(
            argv(&fake),
            strings(&["new", "golden:abc", "--idempotency-key", "k1", "--json"])
        );
        fake.new_workspace("golden:abc", Egress::Closed, "k2")
            .unwrap();
        assert_eq!(
            argv(&fake),
            strings(&[
                "new",
                "golden:abc",
                "--egress",
                "closed",
                "--idempotency-key",
                "k2",
                "--json"
            ])
        );
    }

    #[test]
    fn exec_runs_an_argv_after_dashes_with_its_input_on_stdin_and_the_cwd_without_its_slash() {
        let fake = FakeVims::new();
        fake.on("exec", Reply::ok(r#"{"id":"op_1"}"#))
            .on("exec", Reply::ok(r#"{"id":"op_2"}"#));
        let exec = Exec {
            cwd: Some("/workspace/tc-supplier-portal-v2".into()),
            timeout_seconds: Some(1800),
            no_wait: true,
            argv: strings(&["bash", "-lc", "composer install --no-interaction"]),
            stdin: b"the body".to_vec(),
        };
        fake.exec("wsp_1", &exec, "k1").unwrap();
        assert_eq!(
            argv(&fake),
            strings(&[
                "exec",
                "wsp_1",
                "--no-wait",
                "--cwd",
                "workspace/tc-supplier-portal-v2",
                "--timeout",
                "1800",
                "--idempotency-key",
                "k1",
                "--json",
                "--",
                "bash",
                "-lc",
                "composer install --no-interaction",
            ])
        );
        assert_eq!(fake.calls()[0].stdin(), b"the body");

        let plain = Exec {
            cwd: None,
            timeout_seconds: None,
            no_wait: false,
            argv: strings(&["true"]),
            stdin: Vec::new(),
        };
        fake.exec("wsp_1", &plain, "k2").unwrap();
        assert_eq!(
            argv(&fake),
            strings(&[
                "exec",
                "wsp_1",
                "--idempotency-key",
                "k2",
                "--json",
                "--",
                "true"
            ])
        );
    }

    #[test]
    fn each_operation_command_has_its_shape() {
        let fake = FakeVims::new();
        fake.on("operation show", Reply::ok(r#"{"id":"op_1"}"#))
            .on("operation list", Reply::ok("[]"))
            .on("operation logs", Reply::ok("line one\n"))
            .on("operation cancel", Reply::ok(r#"{"id":"op_1"}"#));
        fake.operation("op_1").unwrap();
        assert_eq!(
            argv(&fake),
            strings(&["operation", "show", "op_1", "--json"])
        );
        fake.operations_by_key("k1").unwrap();
        assert_eq!(
            argv(&fake),
            strings(&["operation", "list", "--idempotency-key", "k1", "--json"])
        );
        assert_eq!(fake.calls().last().unwrap().key(), None);
        fake.operation_logs("op_1").unwrap();
        assert_eq!(
            argv(&fake),
            strings(&["operation", "logs", "op_1", "--json"])
        );
        fake.cancel("op_1", "k2").unwrap();
        assert_eq!(
            argv(&fake),
            strings(&[
                "operation",
                "cancel",
                "op_1",
                "--idempotency-key",
                "k2",
                "--json"
            ])
        );
    }

    #[test]
    fn each_workspace_change_carries_its_key() {
        let fake = FakeVims::new();
        for command in ["pause", "start", "stop", "remove"] {
            fake.on(command, Reply::ok("{}"));
        }
        fake.pause("wsp_1", "k1").unwrap();
        assert_eq!(
            argv(&fake),
            strings(&["pause", "wsp_1", "--idempotency-key", "k1", "--json"])
        );
        fake.start("wsp_1", "k2").unwrap();
        assert_eq!(
            argv(&fake),
            strings(&["start", "wsp_1", "--idempotency-key", "k2", "--json"])
        );
        fake.stop("wsp_1", "k3").unwrap();
        assert_eq!(
            argv(&fake),
            strings(&["stop", "wsp_1", "--idempotency-key", "k3", "--json"])
        );
        fake.remove("wsp_1", "k4").unwrap();
        assert_eq!(
            argv(&fake),
            strings(&[
                "remove",
                "wsp_1",
                "--in-use",
                "--idempotency-key",
                "k4",
                "--json"
            ])
        );
    }

    #[test]
    fn hold_is_put_on_and_let_go_with_no_key() {
        let fake = FakeVims::new();
        fake.on("hold", Reply::ok("{}")).on("hold", Reply::ok("{}"));
        fake.hold("wsp_1", "foundry", true).unwrap();
        assert_eq!(
            argv(&fake),
            strings(&["hold", "wsp_1", "foundry", "--json"])
        );
        fake.hold("wsp_1", "foundry", false).unwrap();
        assert_eq!(
            argv(&fake),
            strings(&["hold", "wsp_1", "foundry", "off", "--json"])
        );
        assert!(fake.calls().iter().all(|call| call.key().is_none()));
    }

    #[test]
    fn a_grant_is_read_only_and_put_and_removed_with_its_key() {
        let fake = FakeVims::new();
        fake.on("workspace turn-grant put", Reply::ok("{}"))
            .on("workspace turn-grant rm", Reply::ok("{}"));
        fake.grant("wsp_1", "fetch", "conn_gh", "30m", "k1")
            .unwrap();
        assert_eq!(
            argv(&fake),
            strings(&[
                "workspace",
                "turn-grant",
                "put",
                "wsp_1",
                "--turn",
                "fetch",
                "--read-only",
                "conn_gh",
                "--for",
                "30m",
                "--idempotency-key",
                "k1",
                "--json"
            ])
        );
        fake.ungrant("wsp_1", "fetch", "conn_gh", "k2").unwrap();
        assert_eq!(
            argv(&fake),
            strings(&[
                "workspace",
                "turn-grant",
                "rm",
                "wsp_1",
                "--turn",
                "fetch",
                "conn_gh",
                "--idempotency-key",
                "k2",
                "--json"
            ])
        );
    }

    #[test]
    fn each_read_has_its_shape() {
        let fake = FakeVims::new();
        fake.on("show", Reply::ok(r#"{"id":"wsp_1"}"#))
            .on("templates show", Reply::ok(r#"{"name":"t"}"#))
            .on("connection list", Reply::ok("[]"))
            .on("github", Reply::ok("{}"))
            .on("profile show", Reply::ok("{}"));
        fake.show("wsp_1").unwrap();
        assert_eq!(argv(&fake), strings(&["show", "wsp_1", "--json"]));
        fake.template("tc-supplier-portal-v2").unwrap();
        assert_eq!(
            argv(&fake),
            strings(&["templates", "show", "tc-supplier-portal-v2", "--json"])
        );
        fake.connections().unwrap();
        assert_eq!(argv(&fake), strings(&["connection", "list", "--json"]));
        fake.github().unwrap();
        assert_eq!(argv(&fake), strings(&["github", "--json"]));
        fake.profile_has_skill("factory-build").unwrap();
        assert_eq!(argv(&fake), strings(&["profile", "show", "--json"]));
        assert!(fake.calls().iter().all(|call| call.key().is_none()));
    }

    #[test]
    fn an_exec_operation_gives_its_exit_code() {
        let fake = FakeVims::new();
        fake.on(
            "operation show",
            Reply::ok(
                r#"{"id":"op_3","status":"succeeded","result":{"exitCode":2,"output":"tail"}}"#,
            ),
        );
        let operation = fake.operation("op_3").unwrap();
        assert_eq!(operation.state, OpState::Finished);
        assert_eq!(operation.exit_code, Some(2));
        assert_eq!(operation.error, None);
    }

    #[test]
    fn a_command_past_its_limit_is_a_failed_operation_with_host_command_timeout() {
        let fake = FakeVims::new();
        fake.on(
            "operation show",
            Reply::ok(r#"{"id":"op_3","status":"failed","error":{"code":"HOST_COMMAND_TIMEOUT","message":"timed out"}}"#),
        );
        let operation = fake.operation("op_3").unwrap();
        assert_eq!(operation.state, OpState::Failed);
        assert_eq!(operation.exit_code, None);
        assert_eq!(operation.error.as_deref(), Some("HOST_COMMAND_TIMEOUT"));
    }

    #[test]
    fn each_operation_state_is_read() {
        for (status, state) in [
            ("queued", OpState::Running),
            ("pending", OpState::Running),
            ("running", OpState::Running),
            ("succeeded", OpState::Finished),
            ("completed", OpState::Finished),
            ("failed", OpState::Failed),
            ("cancelled", OpState::Cancelled),
            ("canceled", OpState::Cancelled),
            ("Weird", OpState::Other("weird".into())),
        ] {
            let fake = FakeVims::new();
            fake.on(
                "operation show",
                Reply::ok(format!(r#"{{"id":"op_1","status":"{status}"}}"#)),
            );
            assert_eq!(fake.operation("op_1").unwrap().state, state, "{status}");
        }
    }

    #[test]
    fn an_operation_list_is_read_as_an_array_or_under_operations() {
        let fake = FakeVims::new();
        fake.on("operation list", Reply::ok(r#"[{"id":"op_1"}]"#))
            .on(
                "operation list",
                Reply::ok(r#"{"operations":[{"id":"op_2"}]}"#),
            )
            .on("operation list", Reply::ok(r#"{"operations":[]}"#));
        assert_eq!(fake.operations_by_key("k").unwrap()[0].id, "op_1");
        assert_eq!(fake.operations_by_key("k").unwrap()[0].id, "op_2");
        assert!(fake.operations_by_key("k").unwrap().is_empty());
    }

    #[test]
    fn operation_logs_are_read_as_text_or_from_json() {
        let fake = FakeVims::new();
        fake.on("operation logs", Reply::ok(r#"{"logs":"one\ntwo\n"}"#))
            .on("operation logs", Reply::ok("plain\n"));
        assert_eq!(fake.operation_logs("op_1").unwrap(), "one\ntwo\n");
        assert_eq!(fake.operation_logs("op_1").unwrap(), "plain\n");
    }

    #[test]
    fn a_change_with_an_operation_gives_it_and_one_without_is_done() {
        let fake = FakeVims::new();
        fake.on(
            "pause",
            Reply::ok(r#"{"operation":{"id":"op_4","status":"running"}}"#),
        )
        .on(
            "pause",
            Reply::ok(r#"{"workspace":{"id":"wsp_1","status":"paused"}}"#),
        )
        .on("pause", Reply::ok(""));
        assert!(matches!(
            fake.pause("wsp_1", "k1").unwrap(),
            Change::Operation(operation) if operation.id == "op_4"
        ));
        assert!(matches!(
            fake.pause("wsp_1", "k2").unwrap(),
            Change::Workspace(workspace) if workspace.state == WorkspaceState::Paused
        ));
        assert_eq!(fake.pause("wsp_1", "k3").unwrap(), Change::Done);
    }

    #[test]
    fn an_operation_at_the_top_of_a_change_is_read_in_each_spelling() {
        let fake = FakeVims::new();
        for answer in [
            r#"{"id":"op_5"}"#,
            r#"{"operationId":"op_5","status":"queued"}"#,
            r#"{"operation_id":"op_5","status":"queued"}"#,
        ] {
            fake.on("remove", Reply::ok(answer));
        }
        for key in ["k1", "k2", "k3"] {
            assert!(
                matches!(
                    fake.remove("wsp_1", key).unwrap(),
                    Change::Operation(operation) if operation.id == "op_5"
                ),
                "{key}"
            );
        }
    }

    #[test]
    fn a_workspace_at_the_top_of_a_change_gives_its_state() {
        let fake = FakeVims::new();
        fake.on("stop", Reply::ok(r#"{"id":"wsp_1","status":"stopped"}"#))
            .on("stop", Reply::ok("{}"));
        assert!(matches!(
            fake.stop("wsp_1", "k1").unwrap(),
            Change::Workspace(workspace) if workspace.state == WorkspaceState::Stopped
        ));
        assert_eq!(fake.stop("wsp_1", "k2").unwrap(), Change::Done);
    }

    #[test]
    fn a_change_answer_that_names_no_operation_or_workspace_cannot_be_read() {
        for answer in [
            r#"{"error":{"code":"PRECONDITION_FAILED","message":"paused"}}"#,
            r#"{"status":"queued","workspaceId":"wsp_1"}"#,
            r#"{"operation":"op_9"}"#,
            "null",
            "[]",
            "not json",
        ] {
            let fake = FakeVims::new();
            fake.on("remove", Reply::ok(answer));
            assert!(
                matches!(fake.remove("wsp_1", "k1"), Err(VimsError::Unknown { .. })),
                "{answer}"
            );
        }
    }

    #[test]
    fn show_gives_the_workspace_s_state_and_its_source_key() {
        let fake = FakeVims::new();
        fake.on(
            "show",
            Reply::ok(r#"{"workspace":{"id":"wsp_1","name":"brave-otter","status":"Paused","versions":{"base":{"variantDigest":"sha256:e355a3c80fa7"}}}}"#),
        );
        let workspace = fake.show("wsp_1").unwrap();
        assert_eq!(workspace.id, "wsp_1");
        assert_eq!(workspace.name.as_deref(), Some("brave-otter"));
        assert_eq!(workspace.state, WorkspaceState::Paused);
        assert_eq!(workspace.source_key.as_deref(), Some("golden:e355a3c80fa7"));
    }

    #[test]
    fn each_workspace_state_is_read() {
        for (status, state) in [
            ("running", WorkspaceState::Running),
            ("Paused", WorkspaceState::Paused),
            ("stopped", WorkspaceState::Stopped),
            ("sleeping", WorkspaceState::Asleep),
            ("asleep", WorkspaceState::Asleep),
            ("starting", WorkspaceState::Other("starting".into())),
        ] {
            let fake = FakeVims::new();
            fake.on(
                "show",
                Reply::ok(format!(r#"{{"id":"wsp_1","status":"{status}"}}"#)),
            );
            assert_eq!(fake.show("wsp_1").unwrap().state, state, "{status}");
        }
    }

    #[test]
    fn a_template_gives_its_source_key_checkout_path_size_and_whether_it_is_retired() {
        let fake = FakeVims::new();
        fake.on(
            "templates show",
            Reply::ok(r#"{"template":{"name":"tc-supplier-portal-v2","sourceKey":"golden:abc123","vcpu":4,"retired":false,"sources":[{"repo":"Trilogy-Care/tc-supplier-portal-v2","path":"/workspace/tc-supplier-portal-v2"}]}}"#),
        );
        let template = fake.template("tc-supplier-portal-v2").unwrap();
        assert_eq!(template.name, "tc-supplier-portal-v2");
        assert_eq!(template.source_key.as_deref(), Some("golden:abc123"));
        assert_eq!(template.vcpu, Some(4));
        assert!(!template.retired);
        assert_eq!(
            template.sources,
            [TemplateSource {
                repo: Some("Trilogy-Care/tc-supplier-portal-v2".into()),
                path: "/workspace/tc-supplier-portal-v2".into(),
            }]
        );
    }

    #[test]
    fn a_template_with_one_golden_key_anywhere_gives_it_and_with_two_gives_none() {
        let fake = FakeVims::new();
        fake.on(
            "templates show",
            Reply::ok(r#"{"name":"t","current":{"build":{"key":"golden:one"}}}"#),
        )
        .on(
            "templates show",
            Reply::ok(r#"{"name":"t","builds":[{"key":"golden:one"},{"key":"golden:two"}]}"#),
        )
        .on(
            "templates show",
            Reply::ok(r#"{"name":"t","status":"retired"}"#),
        );
        assert_eq!(
            fake.template("t").unwrap().source_key.as_deref(),
            Some("golden:one")
        );
        assert_eq!(fake.template("t").unwrap().source_key, None);
        assert!(fake.template("t").unwrap().retired);
    }

    #[test]
    fn connections_are_listed() {
        let fake = FakeVims::new();
        fake.on(
            "connection list",
            Reply::ok(
                r#"{"connections":[{"id":"conn_1","provider":"github","name":"Trilogy GitHub"}]}"#,
            ),
        );
        assert_eq!(
            fake.connections().unwrap(),
            [Connection {
                id: "conn_1".into(),
                provider: Some("github".into()),
                name: Some("Trilogy GitHub".into()),
            }]
        );
    }

    #[test]
    fn github_gives_the_login_for_an_owner() {
        let fake = FakeVims::new();
        fake.on(
            "github",
            Reply::ok(
                r#"{"installations":[{"account":{"login":"Trilogy-Care"},"login":"tomkaczocha"}]}"#,
            ),
        )
        .on("github", Reply::ok(r#"{"login":"tomkaczocha"}"#));
        let links = fake.github().unwrap();
        assert_eq!(links.login_for("trilogy-care"), Some("tomkaczocha"));
        assert_eq!(links.login_for("rawphp"), None);
        let one = fake.github().unwrap();
        assert_eq!(one.login_for("anyone"), Some("tomkaczocha"));
    }

    #[test]
    fn a_login_at_the_top_is_for_every_owner_only_when_no_owner_is_named() {
        let fake = FakeVims::new();
        fake.on(
            "github",
            Reply::ok(
                r#"{"login":"tomkaczocha","installations":[{"account":{"login":"Trilogy-Care"},"login":"tomkaczocha"}]}"#,
            ),
        );
        let links = fake.github().unwrap();
        assert_eq!(links.login_for("Trilogy-Care"), Some("tomkaczocha"));
        assert_eq!(links.login_for("rawphp"), None);
    }

    #[test]
    fn the_profile_says_whether_it_holds_a_skill() {
        let fake = FakeVims::new();
        fake.on(
            "profile show",
            Reply::ok(r#"{"profile":{"skills":[{"name":"factory-build"},{"name":"other"}]}}"#),
        )
        .on("profile show", Reply::ok(r#"{"skills":["other"]}"#))
        .on(
            "profile show",
            Reply::ok(r#"{"claude":{"skills":["factory-build"]}}"#),
        );
        assert!(fake.profile_has_skill("factory-build").unwrap());
        assert!(!fake.profile_has_skill("factory-build").unwrap());
        assert!(fake.profile_has_skill("factory-build").unwrap());
    }

    #[test]
    fn an_answer_that_cannot_be_read_is_unknown_and_names_the_command() {
        let fake = FakeVims::new();
        fake.on("operation show", Reply::ok("not json"))
            .on("new", Reply::ok(r#"{"status":"running"}"#));
        let error = fake.operation("op_1").unwrap_err();
        assert!(matches!(error, VimsError::Unknown { .. }), "{error:?}");
        assert!(
            error
                .to_string()
                .contains("vims operation show op_1 --json"),
            "{error}"
        );
        assert!(matches!(
            fake.new_workspace("golden:abc", Egress::Open, "k1"),
            Err(VimsError::Unknown { .. })
        ));
    }
}
