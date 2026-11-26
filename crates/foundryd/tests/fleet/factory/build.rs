//! Fleet tests of a run from claim to `pr-open`.

use std::collections::BTreeSet;
use std::process::{Command, Stdio};
use std::time::Duration;

use foundryd::factory::runs::gating::CONFIG_DIFFERS;

use super::{HELPER_SECRET, Harness, INSTRUCTOR, Options, TOKEN, WAIT, config, isolated, live};
use crate::until;

/// The steps of an agent that takes the launch message and reports pull request `pr`.
fn opens(pr: u64) -> String {
    format!(
        r#"agent '- hook: {{event: SessionStart}}
- wait_prompt
- stop: {{last_assistant_message: "Opened the pull request.\nFACTORY-DONE: https://github.com/acme/widgets/pull/{pr}"}}
- wait_input'
"#
    )
}

/// A session that commits the feature, pushes the run's branch and reports pull request `pr`.
fn builds_and_opens(pr: u64) -> String {
    format!(
        "commit feature.txt 'the feature' 'add the feature'\ngit push -q origin HEAD\n{}",
        opens(pr)
    )
}

/// Waits until the workspaces named `name` are all archived and their sessions all ended.
async fn gone(h: &Harness, name: &str) {
    eprintln!("fleet: waiting for {name} to be gone");
    until(
        WAIT,
        &format!("{name} is archived with its sessions ended"),
        || {
            let made = h.workspaces_named(name);
            !made.is_empty()
                && made.iter().all(|w| w.deleted)
                && made.iter().all(|w| {
                    let sessions = h.sessions_in(w.body.id);
                    !sessions.is_empty() && !sessions.iter().any(live)
                })
        },
    )
    .await;
}

/// The events of attempt 1 of `issue` named `name`.
fn named(h: &Harness, issue: u64, name: &str) -> Vec<serde_json::Value> {
    h.events(issue, 1)
        .into_iter()
        .filter(|e| e.event == name)
        .map(|e| e.data)
        .collect()
}

#[test]
fn an_issue_reaches_pr_open_with_its_build_session_ended_and_its_workspace_gone() {
    isolated(
        module_path!(),
        "an_issue_reaches_pr_open_with_its_build_session_ended_and_its_workspace_gone",
        || async {
            let h = Harness::start(Options::default()).await;
            h.github.pull(101, "fac/12-1", 12);
            h.script(12, &builds_and_opens(101));
            h.ready(12, "Make the widgets faster");

            h.reaches(12, "factory:pr-open").await;

            assert!(!h.github.is_draft(101), "the pull request is marked ready");
            assert_eq!(
                h.workspaces_named("fac/12-1").len(),
                1,
                "one build workspace"
            );
            gone(&h, "fac/12-1").await;
            let builder = h.owner_of("fac/12-1").unwrap();
            assert_ne!(
                builder, h.fleet.daemons[0].id,
                "the coordinator builds nothing"
            );
            let gates = h.workspaces_named("fac/12-1-g1");
            assert!(
                !gates.is_empty() && gates.iter().all(|w| w.deleted),
                "the gate workspace is gone too: {gates:?}"
            );
            let first = &h.sent()[0];
            assert!(
                first.line.starts_with("/factory-build acme/widgets#12"),
                "{first:?}"
            );
            h.no_unknown_calls();
        },
    );
}

#[test]
fn a_failed_gate_is_sent_back_and_passes_on_the_retry_spending_one_of_gate_retries() {
    isolated(
        module_path!(),
        "a_failed_gate_is_sent_back_and_passes_on_the_retry_spending_one_of_gate_retries",
        || async {
            // One retry: a second failure would leave the run stuck.
            let h = Harness::start(Options {
                config: config(&[], &["test -f fixed.txt"], 1),
                ..Options::default()
            })
            .await;
            h.github.pull(102, "fac/12-1", 12);
            h.script(
                12,
                r#"commit feature.txt 'the feature' 'add the feature'
git push -q origin HEAD
agent '- hook: {event: SessionStart}
- wait_prompt
- stop: {last_assistant_message: "Opened the pull request.\nFACTORY-DONE: https://github.com/acme/widgets/pull/102"}'
agent '- wait_prompt'
commit fixed.txt 'fixed' 'make the gate pass'
git push -q origin HEAD
agent '- stop: {last_assistant_message: "Fixed the gate.\nFACTORY-DONE: https://github.com/acme/widgets/pull/102"}
- wait_input'
"#,
            );
            h.ready(12, "Make the widgets faster");

            h.reaches(12, "factory:pr-open").await;

            assert_eq!(
                named(&h, 12, "gate_sent_back").len(),
                1,
                "one failure sent back"
            );
            let gates: Vec<u64> = named(&h, 12, "gate_checkout")
                .iter()
                .map(|d| d["attempt"].as_u64().unwrap())
                .collect();
            assert_eq!(gates, vec![1, 2], "the first gate and one retry");
            let sent = h.sent();
            let failures: Vec<_> = sent
                .iter()
                .filter(|m| m.line.starts_with("The factory gate failed"))
                .collect();
            assert_eq!(failures.len(), 1, "one failure sent back: {sent:?}");
            assert!(
                failures[0].text().contains("test -f fixed.txt"),
                "the failure names the command: {:?}",
                failures[0]
            );
            assert!(
                !failures[0].text().contains(CONFIG_DIFFERS),
                "the configs are the same: {:?}",
                failures[0]
            );
            assert!(!h.github.is_draft(102));
            gone(&h, "fac/12-1").await;
            h.no_unknown_calls();
        },
    );
}

#[test]
fn a_session_that_needs_info_posts_its_questions_and_ends_the_run() {
    isolated(
        module_path!(),
        "a_session_that_needs_info_posts_its_questions_and_ends_the_run",
        || async {
            let h = Harness::start(Options::default()).await;
            h.script(
                12,
                r#"agent '- hook: {event: SessionStart}
- wait_prompt
- stop: {last_assistant_message: "1. Which widgets should be faster?\n2. How much faster?\nFACTORY-STOP: needs-info"}
- wait_input'
"#,
            );
            h.ready(12, "Make the widgets faster");

            h.reaches(12, "factory:needs-info").await;

            let comments = h.github.comments_on(12);
            let asked = comments
                .iter()
                .find(|c| c.contains("Which widgets should be faster?"))
                .unwrap_or_else(|| panic!("the questions are posted: {comments:?}"));
            assert!(asked.contains("How much faster?"), "{asked}");
            assert_eq!(named(&h, 12, "questions_posted").len(), 1);
            gone(&h, "fac/12-1").await;
            until(WAIT, "the run ends", || {
                named(&h, 12, "finished").len() == 1
            })
            .await;
            h.no_unknown_calls();
        },
    );
}

#[test]
fn a_feedback_round_waits_in_pr_open_while_every_run_slot_is_held_then_starts() {
    isolated(
        module_path!(),
        "a_feedback_round_waits_in_pr_open_while_every_run_slot_is_held_then_starts",
        || async {
            let h = Harness::start(Options {
                max_runs: 1,
                ..Options::default()
            })
            .await;
            h.github.pull(101, "fac/12-1", 12);
            h.script(12, &builds_and_opens(101));
            h.ready(12, "Make the widgets faster");
            h.reaches(12, "factory:pr-open").await;

            // Issue 13 takes the one run slot and keeps it until the test lets it go.
            h.github.pull(103, "fac/13-1", 13);
            h.script(
                13,
                    r#"agent '- hook: {event: SessionStart}
- wait_prompt'
while [ ! -e "$MARKS/release-13" ]; do sleep 0.1; done
commit feature.txt 'the other feature' 'add the other feature'
git push -q origin HEAD
agent '- stop: {last_assistant_message: "Opened the pull request.\nFACTORY-DONE: https://github.com/acme/widgets/pull/103"}
- wait_input'
"#,
            );
            h.ready(13, "Make the widgets lighter");
            h.reaches(13, "factory:building").await;

            // GitHub's times are to the second: the comment comes after the run's last read.
            h.script(
                12,
                &format!(
                    "commit blue.txt 'blue' 'make them blue'\ngit push -q origin HEAD\n{}",
                    opens(101)
                ),
            );
            tokio::time::sleep(Duration::from_millis(1100)).await;
            h.github
                .comment_as(101, INSTRUCTOR, "Please also make them blue.");

            // Many feedback reads go by, on the clock the factory reads, and the round waits.
            let start = h.clock_now();
            until(WAIT, "ten feedback periods pass", || {
                h.clock_now() - start > Duration::from_secs(100)
            })
            .await;
            assert_eq!(
                h.state(12),
                BTreeSet::from(["factory:pr-open".to_owned()]),
                "the round waits in pr-open"
            );
            assert!(
                h.workspaces_named("fac/12-1-f1").is_empty(),
                "no round workspace while the slot is held"
            );
            assert_eq!(h.state(13), BTreeSet::from(["factory:building".to_owned()]));
            // It read the comment, and has a round to start once a slot frees.
            assert!(
                named(&h, 12, "step_changed").iter().any(|d| {
                    let step = d["step"].as_str().unwrap_or_default();
                    step.starts_with("feedback-read ") && step.ends_with(" found")
                }),
                "the round found the feedback and waits"
            );

            std::fs::write(h.marks.join("release-13"), "").unwrap();
            h.reaches(13, "factory:pr-open").await;
            h.made("fac/12-1-f1").await;
            h.reaches(12, "factory:pr-open").await;
            // The round's commit is on the pull request's branch.
            super::git(&h.origin, &["cat-file", "-e", "fac/12-1:blue.txt"]);
            gone(&h, "fac/12-1-f1").await;
            let feedback = h
                .sent()
                .into_iter()
                .find(|m| m.text().contains("Please also make them blue."));
            assert!(feedback.is_some(), "the round's session got the comment");
            h.no_unknown_calls();
        },
    );
}

#[test]
fn a_feedback_round_on_the_machine_that_built_the_run_starts_from_origins_branch() {
    isolated(
        module_path!(),
        "a_feedback_round_on_the_machine_that_built_the_run_starts_from_origins_branch",
        || async {
            // Only machine 1 can build, so the round runs where the build ran.
            let h = Harness::start(Options {
                builders: vec![1],
                ..Options::default()
            })
            .await;
            h.github.pull(101, "fac/12-1", 12);
            h.script(12, &builds_and_opens(101));
            h.ready(12, "Make the widgets faster");
            h.reaches(12, "factory:pr-open").await;
            gone(&h, "fac/12-1").await;

            // A person pushes to the pull request's branch, then asks for more.
            let pushed = h.push_to("fac/12-1", "person.txt", "a person's change\n");
            h.script(
                12,
                &format!(
                    "git rev-parse HEAD > \"$MARKS/round-head\"\ncommit blue.txt 'blue' 'make them blue'\ngit push -q origin HEAD\n{}",
                    opens(101)
                ),
            );
            tokio::time::sleep(Duration::from_millis(1100)).await;
            h.github
                .comment_as(101, INSTRUCTOR, "Please also make them blue.");

            let round = h.made("fac/12-1-f1").await;
            h.reaches(12, "factory:pr-open").await;
            assert_eq!(
                round.owner, h.fleet.daemons[1].id,
                "the round runs on the builder"
            );
            assert_eq!(h.owner_of("fac/12-1"), Some(h.fleet.daemons[1].id));
            let head = std::fs::read_to_string(h.marks.join("round-head")).unwrap();
            assert_eq!(head.trim(), pushed, "the round starts at origin's branch");
            gone(&h, "fac/12-1-f1").await;
            h.no_unknown_calls();
        },
    );
}

#[test]
fn a_pull_request_that_changes_provision_and_gate_is_gated_with_the_default_branchs_commands() {
    isolated(
        module_path!(),
        "a_pull_request_that_changes_provision_and_gate_is_gated_with_the_default_branchs_commands",
        || async {
            let h = Harness::start(Options {
                config: config(
                    &["git symbolic-ref -q HEAD || echo detached >> {marks}/main-provision"],
                    &["touch {marks}/main-gate", "exit 1"],
                    1,
                ),
                ..Options::default()
            })
            .await;
            let changed = config(
                &["touch {marks}/branch-provision"],
                &["touch {marks}/branch-gate"],
                1,
            )
            .replace("{marks}", &h.marks.display().to_string());
            h.github.pull(101, "fac/12-1", 12);
            h.script(
                12,
                &format!(
                    "commit .dw/config.yml '{changed}' 'gate with something easier'\ngit push -q origin HEAD\n{}",
                    opens(101)
                ),
            );
            h.ready(12, "Make the widgets faster");

            let failure = || {
                h.sent()
                    .into_iter()
                    .find(|m| m.line.starts_with("The factory gate failed"))
            };
            until(WAIT, "the gate failure is sent back", || {
                failure().is_some()
            })
            .await;

            let failure = failure().unwrap();
            assert!(failure.text().contains(CONFIG_DIFFERS), "{failure:?}");
            assert!(failure.text().contains("exit 1"), "{failure:?}");
            // The build's own setup runs main's provision too; only a gate is on no branch.
            let provisioned = std::fs::read_to_string(h.marks.join("main-provision")).unwrap();
            assert!(
                provisioned.contains("detached"),
                "main's provision ran in the gate"
            );
            assert!(h.marks.join("main-gate").exists(), "main's gate ran");
            assert!(
                !h.marks.join("branch-provision").exists(),
                "the branch's did not"
            );
            assert!(
                !h.marks.join("branch-gate").exists(),
                "the branch's did not"
            );
            h.no_unknown_calls();
        },
    );
}

#[test]
fn a_gate_terminal_holds_no_token_and_its_git_gets_none_from_the_machines_helper() {
    isolated(
        module_path!(),
        "a_gate_terminal_holds_no_token_and_its_git_gets_none_from_the_machines_helper",
        || async {
            let h = Harness::start(Options {
                config: config(
                    &[],
                    &[
                        "env > {marks}/gate-env",
                        "printf 'protocol=https\\nhost=github.com\\n\\n' | git credential fill > {marks}/gate-credential 2>&1 || true",
                    ],
                    1,
                ),
                ..Options::default()
            })
            .await;
            // The helper is set for the machine: git outside a terminal gets its password.
            let mut fill = Command::new("git")
                .args(["credential", "fill"])
                .env("GIT_TERMINAL_PROMPT", "0")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            {
                use std::io::Write as _;
                let mut stdin = fill.stdin.take().unwrap();
                stdin
                    .write_all(b"protocol=https\nhost=github.com\n\n")
                    .unwrap();
            }
            let filled = fill.wait_with_output().unwrap();
            assert!(
                String::from_utf8_lossy(&filled.stdout).contains(HELPER_SECRET),
                "the machine's helper answers outside a terminal"
            );

            h.github.pull(101, "fac/12-1", 12);
            h.script(12, &builds_and_opens(101));
            h.ready(12, "Make the widgets faster");
            h.reaches(12, "factory:pr-open").await;

            let env = std::fs::read_to_string(h.marks.join("gate-env")).unwrap();
            assert!(
                !env.contains(TOKEN),
                "the gate's environment holds the token"
            );
            assert!(!env.contains(HELPER_SECRET));
            assert!(
                env.lines()
                    .all(|line| !line.starts_with("GH_TOKEN=") || line == "GH_TOKEN="),
                "{env}"
            );
            let credential = std::fs::read_to_string(h.marks.join("gate-credential")).unwrap();
            assert!(!credential.contains(HELPER_SECRET), "{credential}");
            assert!(!credential.contains(TOKEN), "{credential}");
            assert!(
                !credential.contains("password="),
                "the gate's git got a password: {credential}"
            );
            assert!(
                credential.contains("terminal prompts disabled")
                    || credential.contains("could not read Username"),
                "the gate's git ran and was refused: {credential}"
            );
            h.no_unknown_calls();
        },
    );
}
