//! Gate commands, their results, and the backoff between tries.
//!
//! A gate is a workspace of its own, `fac/<N>-<attempt>-g<k>`, detached at the pull request's
//! head commit with no branch. Its setup is the default branch's `provision` and then `gate`,
//! each command stopped after 30 minutes, so a pull request cannot change what gates it: the
//! commands come from the config the caller gives, never from the worktree. The setup
//! terminal's exit code is the gate's result. Its tail, which the pull request's code writes,
//! only says which of the gate's commands failed and what it printed.
//!
//! A gate that could not run (its create failed, its machine was gone for 5 minutes, or its
//! setup terminal ended with no exit code) is not a gate failure. It is tried again after 1
//! pass, then 2, 4 and 8; the fifth failure has no retry. A `provision` that fails in the gate
//! is a gate failure.
//!
//! The wording of the results comment is the runner's `src/gate.ts`, less its merge: the
//! runner gated the head merged with the base branch's tip, and Foundry's gate checks out the
//! head alone, so the comment says the base was not merged.

use std::time::Duration;

use foundry_proto::factory::RunId;
use foundry_proto::ids::ProjectId;
use foundry_proto::state::TerminalState;

use crate::factory::config::ProjectConfig;
use crate::factory::messages::Ended;
use crate::factory::text::{command_line, short_sha};
use crate::factory::worker::{CreateWorkspace, WorkerError};
use crate::workspaces::{Checkout, FactoryTag, Setup};

/// How long each `provision` and `gate` command may run: the runner's 30 minutes.
pub const COMMAND_LIMIT: Duration = Duration::from_secs(30 * 60);

/// The exit code of a setup whose command ran past [`COMMAND_LIMIT`] and was stopped.
pub const TIMED_OUT: i32 = 124;

/// How a command that was stopped at [`COMMAND_LIMIT`] ended, in words.
pub const RAN_TOO_LONG: &str = "ran longer than 30 minutes";

/// How long a gate's machine may be unreachable before the gate could not run.
pub const MACHINE_GONE_AFTER: Duration = Duration::from_secs(5 * 60);

/// The passes to wait after each failure to run, in order. A failure past these has no retry.
const RETRY_PASSES: [u32; 4] = [1, 2, 4, 8];

/// The `k`-th gate workspace's name: `fac/<N>-<attempt>-g<k>`.
pub fn name(run: &RunId, k: u32) -> String {
    format!("fac/{}-{}-g{k}", run.issue(), run.attempt())
}

/// The workspace for the `k`-th gate of `run` on commit `sha` of `project`: detached at `sha`
/// with no branch, its setup `config.provision` then `config.gate`, each stopped after
/// [`COMMAND_LIMIT`]. `config` is the default branch's, never the worktree's.
pub fn request(
    config: &ProjectConfig,
    project: ProjectId,
    sha: &str,
    run: &RunId,
    k: u32,
) -> CreateWorkspace {
    CreateWorkspace {
        project,
        name: name(run, k),
        checkout: Checkout::Commit {
            sha: sha.to_string(),
        },
        setup: Setup::Commands {
            commands: config
                .provision
                .iter()
                .chain(&config.gate)
                .cloned()
                .collect(),
            limit_secs: COMMAND_LIMIT.as_secs(),
        },
        tag: FactoryTag {
            project,
            run: run.to_string(),
        },
    }
}

/// What a gate's setup terminal says about the gate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GateResult {
    /// No verdict yet: the terminal has not exited, its machine cannot be reached to say, or it
    /// exited with no exit code. [`could_not_run`] says when the gate could not run.
    Running,
    /// The setup exited 0: every command did.
    Passed,
    /// The setup exited with another code: a `provision` or `gate` command failed. `command`
    /// is the one of the gate's commands the tail names, `None` when it names none; `how` is
    /// how it ended in words, and `output` what it printed.
    Failed {
        command: Option<String>,
        how: String,
        output: String,
    },
}

/// The gate's result from its setup terminal's state and exit code, with the failing command
/// and its output read from the terminal's plain-text tail. `commands` are the ones the gate
/// ran, the `provision` and `gate` lists [`request`] gave it.
///
/// The verdict is the exit code's alone: 0 is a pass, and any other code a failure, a failed
/// `provision` included, with [`TIMED_OUT`] read as [`RAN_TOO_LONG`]. A terminal that has not
/// exited, whose machine cannot be reached, or that exited with no exit code gives none.
///
/// The tail is read as `SETUP_LIMITED` in `workspaces/mod.rs` prints it: `$ <command>` before
/// each command, then `setup failed: <command> exited <code>`, or `ran past <limit> s and was
/// stopped`, after the one that failed. The pull request's code writes the same tail and can
/// fake such lines or erase the real one, so only a line that names one of `commands` with an
/// ending that fits the exit code counts, the last such in the tail. With none, no command is
/// named and the output is the whole tail. Output that names one of the gate's own commands
/// can still be blamed in its place, but never one the gate did not run.
pub fn result(
    state: TerminalState,
    exit_code: Option<i32>,
    tail: &str,
    commands: &[String],
) -> GateResult {
    let code = match (state, exit_code) {
        (TerminalState::Exited, Some(0)) => return GateResult::Passed,
        (TerminalState::Exited, Some(code)) => code,
        _ => return GateResult::Running,
    };
    let how = if code == TIMED_OUT {
        RAN_TOO_LONG.to_string()
    } else {
        format!("exit code {code}")
    };
    let (command, output) = match failure_line(tail, code, commands) {
        Some((at, command)) => (Some(command.to_string()), output_of(&tail[..at], command)),
        None => (None, tail),
    };
    GateResult::Failed {
        command,
        how,
        output: output.to_string(),
    }
}

/// How a failed gate's command ended, for the message that sends the failure back
/// ([`messages::CommandFailure`](crate::factory::messages::CommandFailure)): stopped at
/// [`COMMAND_LIMIT`] for [`TIMED_OUT`], else exited with `exit_code`.
pub fn ended(exit_code: i32) -> Ended {
    if exit_code == TIMED_OUT {
        Ended::TimedOut {
            after_ms: u64::try_from(COMMAND_LIMIT.as_millis()).ok(),
        }
    } else {
        Ended::Exit(exit_code.into())
    }
}

/// The byte offset in `tail` of the setup's line for the failed command, and that command:
/// the last `setup failed: <command> <ending>` line whose command is one of `commands` and
/// whose ending fits `code`. The real line may start mid-line, after output with no final
/// newline, and anything written after the setup exited may follow it.
fn failure_line<'a>(tail: &str, code: i32, commands: &'a [String]) -> Option<(usize, &'a str)> {
    let limit = COMMAND_LIMIT.as_secs();
    commands
        .iter()
        .flat_map(|command| {
            let exited = format!("setup failed: {command} exited {code}\n");
            let stopped = (code == TIMED_OUT)
                .then(|| format!("setup failed: {command} ran past {limit} s and was stopped\n"));
            std::iter::once(exited)
                .chain(stopped)
                .filter_map(|line| tail.rfind(&line))
                .map(move |at| (at, command.as_str()))
        })
        .max_by_key(|&(at, _)| at)
}

/// The output of `command` in `before`, the tail up to the setup's line for it: what follows the
/// last `$ <command>` line, which may start mid-line after output with no final newline. All of
/// `before` when the tail was cut after that line's start.
fn output_of<'a>(before: &'a str, command: &str) -> &'a str {
    let started = format!("$ {command}\n");
    match before.rfind(&started) {
        Some(at) => &before[at + started.len()..],
        None => before,
    }
}

/// Why a gate could not run, or `None` while it still can: `create` is the error its workspace
/// create answered with, `unreachable_for` how long its machine has been unreachable on the
/// sleep-counting clock, and `terminal` its setup terminal's state and exit code when it has
/// one. A terminal that exited with no exit code lost its holder before the setup said how it
/// went; the code under test can do that too, by killing the holder, which makes its gate one
/// that could not run, never one that passed. A failed create is named first, then a gone
/// machine.
pub fn could_not_run(
    create: Option<&WorkerError>,
    unreachable_for: Duration,
    terminal: Option<(TerminalState, Option<i32>)>,
) -> Option<String> {
    if let Some(error) = create {
        return Some(format!("the gate workspace could not be made: {error}"));
    }
    if unreachable_for >= MACHINE_GONE_AFTER {
        return Some(format!(
            "the gate's machine was unreachable for {} minutes",
            MACHINE_GONE_AFTER.as_secs() / 60
        ));
    }
    (terminal == Some((TerminalState::Exited, None)))
        .then(|| "the gate's setup terminal ended with no exit code".to_string())
}

/// How many passes to wait before trying a gate again after `failures` failures to run: 1, 2, 4
/// and 8, then `None`, and the run is stuck. No failure waits for nothing.
pub fn backoff(failures: u32) -> Option<u32> {
    match failures {
        0 => Some(0),
        n => RETRY_PASSES.get(usize::try_from(n - 1).ok()?).copied(),
    }
}

/// Whether the `.dw/config.yml` at the pull request's head, `None` when it has none, is not the
/// default branch's, byte for byte, so the gate ran commands the pull request may not have.
pub fn config_differs(default: &str, at_head: Option<&str>) -> bool {
    at_head != Some(default)
}

/// One command of a gate as its results comment shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandResult {
    pub command: String,
    pub exit_code: i64,
    pub duration_ms: u64,
}

/// A command as [`command_line`] cleans it, in a code span nothing can leave. For prose outside
/// a table.
pub fn command_code(command: &str) -> String {
    code_span(&command_line(command))
}

/// `command` as one line in a code span for a table cell: the span's backticks are longer than
/// any run in the command, and `|` is escaped so it does not end the cell.
fn table_code(command: &str) -> String {
    code_span(&command_line(command).replace('|', "\\|"))
}

/// `line` in a code span whose backticks outnumber the longest run of them in it.
fn code_span(line: &str) -> String {
    let longest = line.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let ticks = "`".repeat(longest + 1);
    // A span may not start or end with a backtick of its own, so a space goes in between.
    let pad = if line.starts_with('`') || line.ends_with('`') {
        " "
    } else {
        ""
    };
    format!("{ticks}{pad}{line}{pad}{ticks}")
}

/// A duration in seconds with one decimal, rounded half up: 12 345 ms is `12.3`.
fn seconds(duration_ms: u64) -> String {
    let tenths = duration_ms.saturating_add(50) / 100;
    format!("{}.{}", tenths / 10, tenths % 10)
}

/// The comment for a pull request whose gate ended with `verdict`. `head_sha` is the commit the
/// gate ran on and `base_sha` the tip of the base branch when it ran, which the gate did not
/// merge. The commands are config text, so each is cleaned like the gate's own command line,
/// then put in a code span nothing can leave.
fn results_comment(
    verdict: &str,
    results: &[CommandResult],
    attempt: u32,
    head_sha: &str,
    base_sha: &str,
) -> String {
    let head = short_sha(head_sha);
    let base = match short_sha(base_sha) {
        short if short.is_empty() => "unknown".to_string(),
        short => short,
    };
    let mut text = format!(
        "**Factory gate: {verdict}** (attempt {attempt}, run by the factory in a fresh checkout of the pull request's head `{head}`, not merged with the base branch, whose tip was `{base}`)\n\n| Command | Exit | Time |\n|---|---|---|"
    );
    for result in results {
        text.push_str(&format!(
            "\n| {} | {} | {} s |",
            table_code(&result.command),
            result.exit_code,
            seconds(result.duration_ms)
        ));
    }
    text
}

/// The comment for a pull request whose gate passed.
pub fn gate_results_comment(
    results: &[CommandResult],
    attempt: u32,
    head_sha: &str,
    base_sha: &str,
) -> String {
    results_comment("passed", results, attempt, head_sha, base_sha)
}

/// The comment for a pull request that was ready already and passed the gate again on a head
/// that had moved: the same table, headed `passed again`.
pub fn regate_results_comment(
    results: &[CommandResult],
    attempt: u32,
    head_sha: &str,
    base_sha: &str,
) -> String {
    results_comment("passed again", results, attempt, head_sha, base_sha)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::factory::config::Limits;

    const SOME_SHA: &str = "0123456789abcdef0123456789abcdef01234567";
    const BASE: &str = "89abcdef0123456789abcdef0123456789abcdef";
    const SHORT: &str = "0123456";
    const BASE_SHORT: &str = "89abcde";

    fn strs(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn project(n: u128) -> ProjectId {
        ProjectId(uuid::Uuid::from_u128(n))
    }

    fn config(provision: &[&str], gate: &[&str]) -> ProjectConfig {
        ProjectConfig {
            instructors: strs(&["tom"]),
            base: "main".into(),
            provision: strs(provision),
            gate: strs(gate),
            required_checks: Vec::new(),
            limits: Limits {
                minutes: 180,
                gate_retries: 2,
                checks_minutes: 30,
                interruption_resumes: 2,
            },
            owner_set: false,
        }
    }

    fn run(issue: u64, attempt: u32) -> RunId {
        RunId::new("Owner/Repo", issue, attempt).unwrap()
    }

    mod request {
        use super::*;

        #[test]
        fn names_the_workspace_for_the_run_and_the_gate() {
            assert_eq!(name(&run(12, 1), 3), "fac/12-1-g3");
            let made = request(
                &config(&[], &["make"]),
                project(1),
                SOME_SHA,
                &run(153, 2),
                1,
            );
            assert_eq!(made.name, "fac/153-2-g1");
        }

        #[test]
        fn checks_out_the_commit_with_no_branch() {
            let made = request(
                &config(&[], &["make"]),
                project(1),
                SOME_SHA,
                &run(12, 1),
                1,
            );
            assert_eq!(
                made.checkout,
                Checkout::Commit {
                    sha: SOME_SHA.into()
                }
            );
        }

        #[test]
        fn runs_provision_then_gate_each_for_30_minutes() {
            let made = request(
                &config(
                    &["npm ci", "cargo fetch"],
                    &["cargo fmt --check", "cargo test"],
                ),
                project(1),
                SOME_SHA,
                &run(12, 1),
                1,
            );
            assert_eq!(
                made.setup,
                Setup::Commands {
                    commands: strs(&["npm ci", "cargo fetch", "cargo fmt --check", "cargo test"]),
                    limit_secs: 1800,
                }
            );
            assert_eq!(COMMAND_LIMIT.as_secs(), 1800);
        }

        #[test]
        fn is_tagged_with_the_project_and_the_run() {
            let made = request(
                &config(&[], &["make"]),
                project(7),
                SOME_SHA,
                &run(12, 1),
                1,
            );
            assert_eq!(made.project, project(7));
            assert_eq!(
                made.tag,
                FactoryTag {
                    project: project(7),
                    run: "owner__repo__12__1".into(),
                }
            );
        }

        #[test]
        fn the_commands_come_from_the_config_it_is_given_never_from_the_worktree() {
            for (provision, gate) in [
                (&[][..], &["make check"][..]),
                (&["./setup.sh"][..], &["make", "make lint"][..]),
            ] {
                let made = request(
                    &config(provision, gate),
                    project(1),
                    SOME_SHA,
                    &run(12, 1),
                    2,
                );
                let expected: Vec<String> = provision
                    .iter()
                    .chain(gate)
                    .map(|c| c.to_string())
                    .collect();
                match made.setup {
                    Setup::Commands {
                        commands,
                        limit_secs,
                    } => {
                        assert_eq!(commands, expected);
                        assert_eq!(limit_secs, 1800);
                    }
                    other => panic!("the setup is not the config's commands: {other:?}"),
                }
            }
        }
    }

    mod results {
        use super::*;

        /// The tail a gate's setup terminal shows: each command's line, its output, and the
        /// setup's last line, as `workspaces` prints them.
        fn tail(ran: &[(&str, &str)], last: &str) -> String {
            let mut text = String::new();
            for (command, output) in ran {
                text.push_str(&format!("$ {command}\n{output}"));
            }
            text.push_str(last);
            text
        }

        /// A failure naming `command`, or no command when it is empty.
        fn failed(command: &str, how: &str, output: &str) -> GateResult {
            GateResult::Failed {
                command: (!command.is_empty()).then(|| command.to_string()),
                how: how.into(),
                output: output.into(),
            }
        }

        #[test]
        fn a_running_terminal_is_running() {
            assert_eq!(
                result(
                    TerminalState::Running,
                    None,
                    "$ cargo test\n",
                    &strs(&["cargo test"])
                ),
                GateResult::Running
            );
        }

        #[test]
        fn an_unreachable_terminal_is_running_until_its_machine_is_gone() {
            assert_eq!(
                result(
                    TerminalState::Unreachable,
                    Some(3),
                    "$ cargo test\n",
                    &strs(&["cargo test"])
                ),
                GateResult::Running
            );
            // An exit code it last showed is no verdict while it cannot be reached.
            assert_eq!(
                result(
                    TerminalState::Unreachable,
                    Some(0),
                    "setup passed\n",
                    &strs(&["cargo test"])
                ),
                GateResult::Running
            );
        }

        #[test]
        fn an_exit_with_no_code_gives_no_verdict_and_could_not_run() {
            // The terminal's holder went away: the gate did not finish, and is tried again.
            let text = tail(&[("make", "partial\n")], "");
            assert_eq!(
                result(TerminalState::Exited, None, &text, &strs(&["make"])),
                GateResult::Running
            );
            assert_eq!(
                could_not_run(None, Duration::ZERO, Some((TerminalState::Exited, None))),
                Some("the gate's setup terminal ended with no exit code".into())
            );
        }

        #[test]
        fn an_exit_code_of_0_is_passed() {
            let text = tail(&[("make", "ok\n")], "setup passed\n");
            assert_eq!(
                result(TerminalState::Exited, Some(0), &text, &strs(&["make"])),
                GateResult::Passed
            );
        }

        #[test]
        fn a_failed_gate_command_names_the_command_how_it_ended_and_its_output() {
            let text = tail(
                &[
                    ("npm ci", "added 3 packages\n"),
                    ("cargo test", "running 2 tests\ntest a ... FAILED\n"),
                ],
                "setup failed: cargo test exited 101\n",
            );
            assert_eq!(
                result(
                    TerminalState::Exited,
                    Some(101),
                    &text,
                    &strs(&["npm ci", "cargo test"])
                ),
                failed(
                    "cargo test",
                    "exit code 101",
                    "running 2 tests\ntest a ... FAILED\n"
                )
            );
        }

        #[test]
        fn exit_code_124_reads_as_ran_longer_than_30_minutes() {
            let stopped = tail(
                &[("cargo test", "still going\n")],
                "setup failed: cargo test ran past 1800 s and was stopped\n",
            );
            assert_eq!(
                result(
                    TerminalState::Exited,
                    Some(124),
                    &stopped,
                    &strs(&["cargo test"])
                ),
                failed("cargo test", "ran longer than 30 minutes", "still going\n")
            );
            assert_eq!(RAN_TOO_LONG, "ran longer than 30 minutes");
            // A command that exits 124 itself reads the same: the code is what the gate goes by.
            let own = tail(&[("exit 124", "")], "setup failed: exit 124 exited 124\n");
            assert_eq!(
                result(TerminalState::Exited, Some(124), &own, &strs(&["exit 124"])),
                failed("exit 124", "ran longer than 30 minutes", "")
            );
            // With no line to read, the code alone still says so.
            assert_eq!(
                result(TerminalState::Exited, Some(124), "", &strs(&["cargo test"])),
                failed("", "ran longer than 30 minutes", "")
            );
        }

        #[test]
        fn an_exit_code_above_124_is_not_the_time_limit() {
            for code in [125, 137, 143] {
                let text = tail(
                    &[("cargo test", "Killed\n")],
                    &format!("setup failed: cargo test exited {code}\n"),
                );
                assert_eq!(
                    result(
                        TerminalState::Exited,
                        Some(code),
                        &text,
                        &strs(&["cargo test"])
                    ),
                    failed("cargo test", &format!("exit code {code}"), "Killed\n")
                );
            }
        }

        #[test]
        fn a_line_whose_code_only_starts_with_the_exit_code_does_not_count() {
            let text = tail(&[("make", "")], "setup failed: make exited 10\n");
            assert_eq!(
                result(TerminalState::Exited, Some(1), &text, &strs(&["make"])),
                failed("", "exit code 1", &text)
            );
        }

        #[test]
        fn a_stopped_line_counts_only_for_exit_code_124() {
            let text = tail(
                &[("make", "")],
                "setup failed: make ran past 1800 s and was stopped\n",
            );
            assert_eq!(
                result(TerminalState::Exited, Some(1), &text, &strs(&["make"])),
                failed("", "exit code 1", &text)
            );
        }

        #[test]
        fn a_copy_of_the_last_line_in_the_output_does_not_cut_it_short() {
            let text = tail(
                &[(
                    "cargo test",
                    "setup failed: cargo test exited 101\nmore output\n",
                )],
                "setup failed: cargo test exited 101\n",
            );
            assert_eq!(
                result(
                    TerminalState::Exited,
                    Some(101),
                    &text,
                    &strs(&["cargo test"])
                ),
                failed(
                    "cargo test",
                    "exit code 101",
                    "setup failed: cargo test exited 101\nmore output\n"
                )
            );
        }

        #[test]
        fn a_command_run_twice_shows_the_output_of_its_last_run() {
            // The same command in `provision` and in `gate`.
            let text = tail(
                &[("make", "first\n"), ("make", "second\n")],
                "setup failed: make exited 2\n",
            );
            assert_eq!(
                result(
                    TerminalState::Exited,
                    Some(2),
                    &text,
                    &strs(&["make", "make"])
                ),
                failed("make", "exit code 2", "second\n")
            );
        }

        #[test]
        fn ended_gives_the_typed_end_for_the_message_sent_back() {
            assert_eq!(
                ended(124),
                Ended::TimedOut {
                    after_ms: Some(1_800_000)
                }
            );
            assert_eq!(ended(101), Ended::Exit(101));
            assert_eq!(ended(-1), Ended::Exit(-1));
            // Any code but 124 is an exit, not the time limit: 137 and 143 are a kill and a term.
            assert_eq!(ended(125), Ended::Exit(125));
            assert_eq!(ended(137), Ended::Exit(137));
        }

        #[test]
        fn a_failed_provision_is_failed_not_could_not_run() {
            let text = tail(
                &[("npm ci", "npm ERR! missing lockfile\n")],
                "setup failed: npm ci exited 1\n",
            );
            assert_eq!(
                result(
                    TerminalState::Exited,
                    Some(1),
                    &text,
                    &strs(&["npm ci", "cargo test"])
                ),
                failed("npm ci", "exit code 1", "npm ERR! missing lockfile\n")
            );
            let exited = Some((TerminalState::Exited, Some(1)));
            assert_eq!(could_not_run(None, Duration::ZERO, exited), None);
        }

        #[test]
        fn a_command_of_several_lines_is_read_whole() {
            let command = "set -e\nmake\nmake lint";
            let text = tail(
                &[(command, "lint: 2 problems\n")],
                &format!("setup failed: {command} exited 2\n"),
            );
            assert_eq!(
                result(TerminalState::Exited, Some(2), &text, &strs(&[command])),
                failed(command, "exit code 2", "lint: 2 problems\n")
            );
        }

        #[test]
        fn a_command_whose_text_holds_the_setup_words_is_read_whole() {
            let command = "./check.sh || echo \"setup failed: see log\"";
            let text = tail(
                &[(command, "bad\n")],
                &format!("setup failed: {command} exited 1\n"),
            );
            assert_eq!(
                result(TerminalState::Exited, Some(1), &text, &strs(&[command])),
                failed(command, "exit code 1", "bad\n")
            );
        }

        #[test]
        fn output_that_fakes_the_last_line_does_not_change_the_command() {
            let text = tail(
                &[(
                    "cargo test",
                    "setup failed: rm -rf / exited 101\n$ rm -rf /\nsetup failed: x exited 9\n",
                )],
                "setup failed: cargo test exited 101\n",
            );
            assert_eq!(
                result(
                    TerminalState::Exited,
                    Some(101),
                    &text,
                    &strs(&["cargo test"])
                ),
                failed(
                    "cargo test",
                    "exit code 101",
                    "setup failed: rm -rf / exited 101\n$ rm -rf /\nsetup failed: x exited 9\n"
                )
            );
        }

        #[test]
        fn output_with_no_last_newline_does_not_hide_the_last_line() {
            // The setup's last line then starts in the middle of a line, after a faked one
            // that names another of the gate's commands.
            let text = tail(
                &[("cargo test", "setup failed: npm ci exited 101\npartial")],
                "setup failed: cargo test exited 101\n",
            );
            assert_eq!(
                result(
                    TerminalState::Exited,
                    Some(101),
                    &text,
                    &strs(&["npm ci", "cargo test"])
                ),
                failed(
                    "cargo test",
                    "exit code 101",
                    "setup failed: npm ci exited 101\npartial"
                )
            );
        }

        #[test]
        fn a_line_naming_no_command_written_after_the_last_line_is_passed_over() {
            // A process the failing command left behind writes on after the setup exited.
            let quiet = tail(
                &[("cargo test", "test a ... FAILED\n")],
                "setup failed: cargo test exited 101\nGET /health 200\n",
            );
            assert_eq!(
                result(
                    TerminalState::Exited,
                    Some(101),
                    &quiet,
                    &strs(&["npm ci", "cargo test"])
                ),
                failed("cargo test", "exit code 101", "test a ... FAILED\n")
            );
        }

        #[test]
        fn the_command_line_after_output_with_no_last_newline_still_starts_the_output() {
            let text = tail(
                &[
                    ("npm ci", "progress 100%"),
                    ("cargo test", "test a ... FAILED\n"),
                ],
                "setup failed: cargo test exited 101\n",
            );
            assert_eq!(
                result(
                    TerminalState::Exited,
                    Some(101),
                    &text,
                    &strs(&["npm ci", "cargo test"])
                ),
                failed("cargo test", "exit code 101", "test a ... FAILED\n")
            );
        }

        #[test]
        fn a_tail_cut_before_the_command_line_keeps_all_of_its_output() {
            let text = "of the output\nsetup failed: cargo test exited 101\n";
            assert_eq!(
                result(
                    TerminalState::Exited,
                    Some(101),
                    text,
                    &strs(&["cargo test"])
                ),
                failed("cargo test", "exit code 101", "of the output\n")
            );
        }

        #[test]
        fn a_failed_setup_whose_tail_names_none_of_its_commands_is_still_failed() {
            // Stopped by a signal from the command it ran, the setup exits 1 and says nothing.
            let text = tail(
                &[("make", "ok\n"), ("make lint", "$ rm -rf /\nhalf a line")],
                "",
            );
            assert_eq!(
                result(
                    TerminalState::Exited,
                    Some(1),
                    &text,
                    &strs(&["make", "make lint"])
                ),
                failed("", "exit code 1", &text)
            );
            // A line that names a command the gate did not run names nothing.
            let faked = tail(&[("npm test", "setup failed: evil exited 1\n")], "\n");
            assert_eq!(
                result(TerminalState::Exited, Some(1), &faked, &strs(&["npm test"])),
                failed("", "exit code 1", &faked)
            );
            // A line whose ending does not fit the exit code names nothing either.
            let other = tail(&[("make", "")], "setup failed: make exited 2\n");
            assert_eq!(
                result(TerminalState::Exited, Some(1), &other, &strs(&["make"])),
                failed("", "exit code 1", &other)
            );
            let exited = Some((TerminalState::Exited, Some(1)));
            assert_eq!(could_not_run(None, Duration::ZERO, exited), None);
        }

        /// What the real setup script, `SETUP_LIMITED` in `workspaces/mod.rs`, prints when
        /// `/bin/sh` runs `commands` with it, each stopped past [`COMMAND_LIMIT`] as a gate's
        /// are, with its error output folded in as in a terminal, and the code it exits with.
        /// `stub` is shell code run before the script, in the script's own shell.
        fn run_setup(stub: &str, commands: &[&str]) -> (String, i32) {
            let script = format!("exec 2>&1\n{stub}\n{}", crate::workspaces::setup_limited());
            let ran = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(script)
                .arg("setup")
                .arg(COMMAND_LIMIT.as_secs().to_string())
                .args(commands)
                .stdin(std::process::Stdio::null())
                .output()
                .unwrap();
            let tail = String::from_utf8(ran.stdout).unwrap();
            let code = ran.status.code().unwrap();
            (tail, code)
        }

        #[test]
        fn the_real_setup_s_line_for_a_command_that_exits_non_zero_names_it_and_its_code() {
            let commands = ["echo fine", "echo boom; exit 3", "echo never"];
            let (tail, code) = run_setup("", &commands);
            assert_eq!(code, 3, "{tail:?}");
            let GateResult::Failed {
                command,
                how,
                output,
            } = result(TerminalState::Exited, Some(code), &tail, &strs(&commands))
            else {
                panic!("not a failure: {tail:?}");
            };
            assert_eq!(
                (command.as_deref(), how.as_str()),
                (Some("echo boom; exit 3"), "exit code 3"),
                "{tail:?}"
            );
            // The command's own output comes first. Some shells, such as macOS's `/bin/sh`, add
            // a line of their own when the stopped watcher is reaped, so the rest is not asserted.
            assert!(output.starts_with("boom\n"), "{tail:?}");
        }

        #[test]
        fn the_real_setup_s_line_for_a_command_past_its_limit_names_it_and_the_limit() {
            // `sleep` in the script's own shell returns at once, so its watcher stops the command
            // now, not 30 minutes from now. The command runs in a shell of its own, with the
            // real `sleep`.
            let commands = ["sleep 3187"];
            let (tail, code) = run_setup("sleep() { :; }", &commands);
            assert_eq!(code, TIMED_OUT, "{tail:?}");
            // Its output is not asserted: some shells print a line of their own for the kill.
            let GateResult::Failed { command, how, .. } =
                result(TerminalState::Exited, Some(code), &tail, &strs(&commands))
            else {
                panic!("not a failure: {tail:?}");
            };
            assert_eq!(
                (command.as_deref(), how.as_str()),
                (Some("sleep 3187"), RAN_TOO_LONG),
                "{tail:?}"
            );
        }
    }

    mod could_not_run {
        use super::*;

        #[test]
        fn a_create_that_failed_could_not_run() {
            let error = WorkerError::Failed("no space left on device".into());
            let made = Some(
                "the gate workspace could not be made: the call failed: no space left on device"
                    .to_string(),
            );
            assert_eq!(could_not_run(Some(&error), Duration::ZERO, None), made);
            // A failed create is named first, also when the machine is gone too.
            assert_eq!(could_not_run(Some(&error), MACHINE_GONE_AFTER, None), made);
        }

        #[test]
        fn a_machine_gone_for_5_minutes_could_not_run() {
            let gone = could_not_run(None, MACHINE_GONE_AFTER, None);
            assert_eq!(
                gone,
                Some("the gate's machine was unreachable for 5 minutes".into())
            );
            let unreachable = Some((TerminalState::Unreachable, None));
            assert_eq!(
                could_not_run(None, Duration::from_secs(60 * 60), unreachable),
                gone
            );
        }

        #[test]
        fn a_machine_gone_for_less_than_5_minutes_can_still_run() {
            let unreachable = Some((TerminalState::Unreachable, None));
            assert_eq!(could_not_run(None, Duration::ZERO, None), None);
            assert_eq!(
                could_not_run(
                    None,
                    MACHINE_GONE_AFTER - Duration::from_millis(1),
                    unreachable
                ),
                None
            );
        }

        #[test]
        fn a_machine_gone_is_named_before_a_terminal_with_no_exit_code() {
            assert_eq!(
                could_not_run(
                    None,
                    MACHINE_GONE_AFTER,
                    Some((TerminalState::Exited, None))
                ),
                Some("the gate's machine was unreachable for 5 minutes".into())
            );
        }

        #[test]
        fn a_running_or_finished_setup_can_still_run() {
            for terminal in [
                (TerminalState::Running, None),
                (TerminalState::Exited, Some(0)),
                (TerminalState::Exited, Some(101)),
            ] {
                assert_eq!(could_not_run(None, Duration::ZERO, Some(terminal)), None);
            }
        }
    }

    mod backoff {
        use super::*;

        #[test]
        fn waits_1_2_4_and_8_passes() {
            assert_eq!(backoff(1), Some(1));
            assert_eq!(backoff(2), Some(2));
            assert_eq!(backoff(3), Some(4));
            assert_eq!(backoff(4), Some(8));
        }

        #[test]
        fn the_fifth_failure_to_run_gives_none() {
            assert_eq!(backoff(5), None);
            assert_eq!(backoff(6), None);
            assert_eq!(backoff(u32::MAX), None);
        }

        #[test]
        fn no_failure_waits_for_nothing() {
            assert_eq!(backoff(0), Some(0));
        }
    }

    mod config_differs {
        use super::*;

        const DEFAULT: &str = "factory:\n  gate: [make]\n";

        #[test]
        fn the_same_file_does_not_differ() {
            assert!(!config_differs(DEFAULT, Some(DEFAULT)));
        }

        #[test]
        fn a_changed_file_differs() {
            assert!(config_differs(
                DEFAULT,
                Some("factory:\n  gate: [make, make lint]\n")
            ));
            assert!(config_differs(
                DEFAULT,
                Some("factory:\n  gate: [make]\n\n")
            ));
        }

        #[test]
        fn a_file_missing_at_the_head_differs() {
            assert!(config_differs(DEFAULT, None));
        }
    }

    /// The runner's results-comment cases from `src/gate.test.ts`, same cases and names.
    mod wording {
        use super::*;

        fn result(command: &str, exit_code: i64, duration_ms: u64) -> CommandResult {
            CommandResult {
                command: command.into(),
                exit_code,
                duration_ms,
            }
        }

        fn last_rows(comment: &str, n: usize) -> Vec<&str> {
            let rows: Vec<&str> = comment.split('\n').collect();
            rows[rows.len() - n..].to_vec()
        }

        #[test]
        fn the_results_comment_says_the_commands_ran_in_a_fresh_checkout_of_the_pr_head() {
            assert_eq!(
                gate_results_comment(&[result("cargo test", 0, 1_000)], 2, SOME_SHA, BASE),
                format!(
                    "**Factory gate: passed** (attempt 2, run by the factory in a fresh checkout of the pull request's head `{SHORT}`, not merged with the base branch, whose tip was `{BASE_SHORT}`)\n\n| Command | Exit | Time |\n|---|---|---|\n| `cargo test` | 0 | 1.0 s |"
                )
            );
        }

        #[test]
        fn the_comment_for_a_pass_on_a_pull_request_that_was_ready_already_says_it_passed_again() {
            assert_eq!(
                regate_results_comment(&[result("cargo test", 0, 1_000)], 2, SOME_SHA, BASE),
                format!(
                    "**Factory gate: passed again** (attempt 2, run by the factory in a fresh checkout of the pull request's head `{SHORT}`, not merged with the base branch, whose tip was `{BASE_SHORT}`)\n\n| Command | Exit | Time |\n|---|---|---|\n| `cargo test` | 0 | 1.0 s |"
                )
            );
            // The same table as the first pass's comment: only the verdict differs.
            let first = gate_results_comment(&[result("a | b", 1, 12_345)], 1, SOME_SHA, "");
            assert_eq!(
                regate_results_comment(&[result("a | b", 1, 12_345)], 1, SOME_SHA, ""),
                first.replacen("passed**", "passed again**", 1)
            );
        }

        #[test]
        fn the_results_comment_rounds_the_time_to_a_tenth_and_says_unknown_for_a_missing_base() {
            let comment = gate_results_comment(
                &[
                    result("a", 0, 49),
                    result("b", 0, 50),
                    result("c", 1, 12_345),
                    result("d", -1, 0),
                ],
                1,
                SOME_SHA,
                "",
            );
            assert!(
                comment.contains("not merged with the base branch, whose tip was `unknown`)"),
                "{comment}"
            );
            assert_eq!(
                last_rows(&comment, 4),
                [
                    "| `a` | 0 | 0.0 s |",
                    "| `b` | 0 | 0.1 s |",
                    "| `c` | 1 | 12.3 s |",
                    "| `d` | -1 | 0.0 s |",
                ]
            );
        }

        #[test]
        fn the_results_comment_keeps_a_command_inside_its_code_span() {
            let long = "x".repeat(400);
            let comment = gate_results_comment(
                &[
                    result("echo a | cat", 0, 0),
                    result("echo `date`", 0, 0),
                    result("`lead", 0, 0),
                    result(&long, 0, 0),
                    result("two\nlines\tand tab", 0, 0),
                    result("hidden\u{202e}text", 0, 0),
                ],
                1,
                SOME_SHA,
                BASE,
            );
            let rows = last_rows(&comment, 6);
            assert_eq!(rows[0], "| `echo a \\| cat` | 0 | 0.0 s |");
            assert_eq!(rows[1], "| `` echo `date` `` | 0 | 0.0 s |");
            assert_eq!(rows[2], "| `` `lead `` | 0 | 0.0 s |");
            assert_eq!(
                rows[3],
                format!("| `{}\u{2026}` | 0 | 0.0 s |", "x".repeat(300))
            );
            assert_eq!(rows[4], "| `two lines and tab` | 0 | 0.0 s |");
            assert_eq!(rows[5], "| `hidden text` | 0 | 0.0 s |");
        }

        #[test]
        fn a_command_code_is_a_span_no_backtick_in_the_command_can_end() {
            assert_eq!(command_code("cargo test"), "`cargo test`");
            assert_eq!(command_code("echo `date`"), "`` echo `date` ``");
            assert_eq!(command_code("a ``` b"), "````a ``` b````");
            // Outside a table, `|` needs no escape.
            assert_eq!(command_code("a | b"), "`a | b`");
            assert_eq!(command_code("two\nlines"), "`two lines`");
        }
    }
}
