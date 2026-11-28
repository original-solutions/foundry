//! Fleet tests of workers: the machines that build runs and gate them.
//!
//! Each test takes a worker away in one way, a restart, a cut or a command that hangs, at one
//! moment of a run, and checks the run carries on as the spec says. What the harness has no
//! handle for, these tests add here:
//!
//! - [`restart`] stops a worker's daemon and starts it again on the same data folder, as
//!   `foundryd` restarts: what it held in memory is gone, its terminals and sessions are taken up
//!   again.
//! - [`hurry`] moves the factory's clock through the 5, 10 and 15 minutes the factory waits.
//! - [`cut_off_at_claim`] cuts a machine off between its claim and the call that asks it for the
//!   build.
//! - [`hold_turns`] keeps a session's `session_turn` from the coordinator, as a record that
//!   syncs apart from the session's own can arrive late.
//! - [`shorten_command_limit`] makes the gate's 30-minute limit on a command a few seconds.

use std::time::Duration;

use foundry_proto::factory::{FactoryBody, FactoryRecord, SessionTurn};
use foundry_proto::ids::{MachineId, SessionId};
use foundry_proto::record::{AnyRecord, Record, Session, Workspace};
use foundryd::factory::SCAN;
use foundryd::factory::gate::COMMAND_LIMIT;
use foundryd::factory::messages::TIME_LIMIT;
use foundryd::factory::runs::gating::GATE_COULD_NOT_RUN;
use foundryd::factory::runs::progress::LAUNCH_RESENDING;
use foundryd::factory::runs::resume::CLAIM_MOVED;
use foundryd::factory::runs::start::LAUNCH_TAKEN;
use foundryd::store::Applied;

use super::{Harness, Held, Machine, Options, WAIT, config, isolated, live};
use crate::{Daemon, lock, sample, until};

/// The pull request every run here reports.
const PR: u64 = 101;

/// The line the launch message of issue 12 starts with.
const LAUNCH: &str = "/factory-build acme/widgets#12";

/// The steps of an agent that reports pull request [`PR`] and then waits.
const DONE: &str = r#"- stop: {last_assistant_message: "Opened the pull request.\nFACTORY-DONE: https://github.com/acme/widgets/pull/101"}
- wait_input"#;

/// A script that commits the feature and pushes the run's branch.
const COMMITS: &str =
    "commit feature.txt 'the feature' 'add the feature'\ngit push -q origin HEAD\n";

/// A script that waits until the test writes the mark `name`.
fn waits_for(name: &str) -> String {
    format!("while [ ! -e \"$MARKS/{name}\" ]; do sleep 0.1; done\n")
}

/// A script that writes the mark `name` for the test.
fn marks(name: &str) -> String {
    format!(": > \"$MARKS/{name}\"\n")
}

/// Cuts off the machine a claim of issue `issue` names before the claim's pass asks it for the
/// build, and answers that machine.
///
/// The `claimed` event is in the coordinator's store before the claim's GitHub edits, and the
/// pass takes that store several times more, to write its next events and to read the machine
/// list, before it dials the machine; a cut machine's dial is refused. The watcher reads the
/// event and cuts the machine while it holds the store, on a thread of its own that takes each
/// store again at once, so the cut lands before the dial unless that thread is kept off the CPU
/// for all of those steps. Nothing makes the pass wait for it: the window is narrowed, not
/// closed. A test that waits for the claim comment on GitHub, polled on the runtime, lost to the
/// pass on a loaded host.
fn cut_off_at_claim(h: &Harness, issue: u64) -> std::thread::JoinHandle<MachineId> {
    let daemons: Vec<_> = h
        .fleet
        .daemons
        .iter()
        .map(|daemon| (daemon.id, daemon.store.clone()))
        .collect();
    let net = h.fleet.net.clone();
    std::thread::spawn(move || {
        let start = std::time::Instant::now();
        let mut read = vec![0; daemons.len()];
        loop {
            for (at, (me, store)) in daemons.iter().enumerate() {
                let store = lock(store);
                for record in store.factory_since(*me, read[at], usize::MAX).unwrap() {
                    read[at] = read[at].max(record.version);
                    let FactoryBody::RunEvent(event) = record.body else {
                        continue;
                    };
                    if event.event != "claimed" || event.run.issue() != issue {
                        continue;
                    }
                    let named = &event.data["machine"];
                    let machine = daemons
                        .iter()
                        .map(|(id, _)| *id)
                        .find(|id| named.as_str() == Some(id.to_string().as_str()))
                        .expect("the claim names a machine of the fleet");
                    eprintln!("fleet: cutting {machine} off at the claim");
                    net.cut.send_modify(|cut| {
                        cut.insert(machine);
                    });
                    return machine;
                }
            }
            assert!(start.elapsed() < WAIT, "issue #{issue} is claimed");
            std::thread::yield_now();
        }
    })
}

/// Moves the factory's clock on by `by` beside the ticker, two seconds every 50 ms, so every
/// pass, lease renewal and wait sees the time go by as in a running fleet, only faster.
async fn hurry(h: &Harness, by: Duration) {
    eprintln!("fleet: hurrying the clock on by {by:?}");
    let step = Duration::from_secs(2);
    let mut left = by;
    while !left.is_zero() {
        let now = left.min(step);
        h.clock.advance(now);
        left -= now;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The index of `machine` in the fleet.
fn index(h: &Harness, machine: MachineId) -> usize {
    h.fleet
        .daemons
        .iter()
        .position(|d| d.id == machine)
        .expect("a machine of the fleet")
}

/// The builder of `builders` that is not `i`.
fn other(builders: [usize; 2], i: usize) -> usize {
    if builders[0] == i {
        builders[1]
    } else {
        builders[0]
    }
}

/// The steps attempt 1 of `issue` recorded, in order.
fn steps(h: &Harness, issue: u64) -> Vec<String> {
    h.events(issue, 1)
        .into_iter()
        .filter(|e| e.event == "step_changed")
        .filter_map(|e| e.data["step"].as_str().map(str::to_owned))
        .collect()
}

/// The events of attempt 1 of `issue` named `name`.
fn named(h: &Harness, issue: u64, name: &str) -> Vec<serde_json::Value> {
    h.events(issue, 1)
        .into_iter()
        .filter(|e| e.event == name)
        .map(|e| e.data)
        .collect()
}

/// How many launch messages of issue 12 a session took.
fn launches(h: &Harness) -> usize {
    h.sent()
        .iter()
        .filter(|m| m.line.starts_with(LAUNCH))
        .count()
}

/// The newest session record any daemon holds in `workspace`, once there is one.
async fn session_in(h: &Harness, workspace: &Record<Workspace>) -> Record<Session> {
    until(WAIT, "the build session starts", || {
        !h.sessions_in(workspace.body.id).is_empty()
    })
    .await;
    h.sessions_in(workspace.body.id).remove(0)
}

/// The session records of `workspace` that `daemon` holds itself.
fn sessions_held(daemon: &Daemon, workspace: &Record<Workspace>) -> Vec<Record<Session>> {
    super::all(daemon)
        .into_iter()
        .filter_map(|record| match record {
            AnyRecord::Session(s) if s.body.workspace == workspace.body.id => Some(s),
            _ => None,
        })
        .collect()
}

/// The newest `session_turn` of `session` that `daemon` wrote itself.
fn own_turn(daemon: &Daemon, session: SessionId) -> Option<(u64, SessionTurn)> {
    let records = lock(&daemon.store)
        .factory_since(daemon.id, 0, usize::MAX)
        .unwrap();
    records
        .into_iter()
        .filter_map(|record| match record.body {
            FactoryBody::Turn(turn) if turn.session == session => Some((record.version, turn)),
            _ => None,
        })
        .max_by_key(|(version, _)| *version)
}

/// A version above any the worker writes in a test: a `session_turn` under it holds back every
/// real one of that session at the daemon it is stored in.
const HELD_BACK: u64 = 1 << 40;

/// Keeps the `session_turn` records of `session`, of the worker `worker`, from the coordinator:
/// it stores one that shows no turn and no prompt under [`HELD_BACK`], so each the worker sends
/// is older and refused, as one not arrived yet. [`release_turns`] lets the newest through.
fn hold_turns(
    h: &Harness,
    worker: usize,
    session: &Record<Session>,
    workspace: &Record<Workspace>,
) {
    let owner = h.fleet.daemons[worker].id;
    let held = FactoryRecord {
        owner,
        version: HELD_BACK,
        body: FactoryBody::Turn(SessionTurn {
            session: session.body.id,
            workspace: workspace.body.id,
            project: workspace.body.project,
            turn: 0,
            prompts: 0,
            question: false,
        }),
    };
    let applied = lock(&h.fleet.daemons[0].store)
        .apply_factory(&held, owner)
        .unwrap();
    assert_eq!(applied, Applied::Stored);
}

/// The newest `session_turn` the worker wrote of `session` reaches the coordinator at last.
fn release_turns(h: &Harness, worker: usize, session: SessionId) {
    let daemon = &h.fleet.daemons[worker];
    let (_, turn) = own_turn(daemon, session).expect("the worker wrote a turn record");
    let released = FactoryRecord {
        owner: daemon.id,
        version: HELD_BACK + 1,
        body: FactoryBody::Turn(turn),
    };
    let applied = lock(&h.fleet.daemons[0].store)
        .apply_factory(&released, daemon.id)
        .unwrap();
    assert_eq!(applied, Applied::Stored);
}

/// Makes the gate's limit on each command, [`COMMAND_LIMIT`], pass in `secs` seconds: the
/// gate's setup waits out the limit with `sleep`, and a `sleep` put first on `PATH`, in the
/// harness's folder of programs, sleeps `secs` when it is asked for the limit. Every other
/// `sleep` is the real one.
fn shorten_command_limit(secs: u64) {
    let path = std::env::var_os("PATH").unwrap();
    let mut dirs = std::env::split_paths(&path);
    let bin = dirs.next().unwrap();
    assert!(
        bin.join("git").is_file(),
        "{} is the harness's folder of programs",
        bin.display()
    );
    let real = dirs
        .map(|dir| dir.join("sleep"))
        .find(|path| path.is_file())
        .expect("sleep on PATH");
    super::write_program(
        &bin.join("sleep"),
        &format!(
            "#!/bin/sh\nif [ \"$1\" = {limit} ]; then exec '{real}' {secs}; fi\nexec '{real}' \"$@\"\n",
            limit = COMMAND_LIMIT.as_secs(),
            real = real.display(),
        ),
    );
}

/// Lays `max_gates` over the factory settings every daemon holds, until they all hold it.
async fn set_max_gates(h: &Harness, max_gates: u32) {
    until(WAIT, "every daemon holds the new max_gates", || {
        let held: Vec<_> = h.fleet.daemons.iter().map(Daemon::settings).collect();
        let holds = |s: &foundry_proto::settings::SharedSettings| {
            s.factory.as_ref().is_some_and(|f| f.max_gates == max_gates)
        };
        if held.iter().all(holds) {
            return true;
        }
        let mut settings = held.into_iter().max_by_key(|s| s.seq).unwrap();
        settings.seq += 1;
        settings.factory.as_mut().unwrap().max_gates = max_gates;
        for daemon in &h.fleet.daemons {
            lock(&daemon.store)
                .apply_settings(settings.clone())
                .unwrap();
        }
        false
    })
    .await;
}

/// Restarts the daemon of worker `i` as `foundryd` restarts: its links close, its store, peers
/// and coordinator start again from disk with nothing held in memory, and its terminals,
/// workspaces, sessions, API and factory service start again on the same data folder. The
/// terminal holders run on through it, and the new daemon takes them up, and the sessions in
/// them, as `foundryd` does at its start.
async fn restart(h: &mut Harness, i: usize) {
    eprintln!("fleet: restarting machine {i}");
    let (data, home) = {
        let old = &mut h._machines[i];
        for server in old.servers.drain(..) {
            server.abort();
        }
        // The stopped daemon's terminals are not closed: its drop finds none in an empty folder.
        let data = std::mem::replace(&mut old.data, tempfile::tempdir().unwrap());
        let home = std::mem::replace(&mut old._home, tempfile::tempdir().unwrap());
        (data, home)
    };
    h.fleet.restart(i).await;
    // A worker that holds the token and the skill, on the data folder the daemon before it used.
    let machine = Machine::start(
        &h.fleet.daemons[i],
        Held::new(true),
        &h.scripts,
        &h.marks,
        h.github.clone(),
        h.clock.clone(),
        h.sent.clone(),
        Some((data, home)),
    )
    .await;
    h._machines[i] = machine;
}

#[test]
fn a_worker_restarted_before_the_launch_message_was_typed_is_sent_it_again() {
    isolated(
        module_path!(),
        "a_worker_restarted_before_the_launch_message_was_typed_is_sent_it_again",
        || async {
            let mut h = Harness::start(Options {
                builders: vec![1],
                ..Options::default()
            })
            .await;
            h.github.pull(PR, "fac/12-1", 12);
            // Claude Code takes no input until the test lets it start.
            h.script(
                12,
                &format!(
                    "{}{}{COMMITS}agent '- hook: {{event: SessionStart}}\n- wait_prompt\n{DONE}'\n",
                    marks("up"),
                    waits_for("go"),
                ),
            );
            h.ready(12, "Make the widgets faster");
            h.reaches(12, "factory:building").await;
            until(WAIT, "the session runs", || h.marks.join("up").exists()).await;
            assert_eq!(launches(&h), 1, "the launch went to the worker");

            // The worker holds the first prompt in memory until it is typed: a restart loses it.
            restart(&mut h, 1).await;
            std::fs::write(h.marks.join("go"), "").unwrap();

            h.reaches(12, "factory:pr-open").await;
            assert!(
                steps(&h, 12)
                    .iter()
                    .any(|s| s.starts_with(LAUNCH_RESENDING)),
                "the launch is asked for again by count: {:?}",
                steps(&h, 12)
            );
            assert_eq!(
                launches(&h),
                2,
                "the launch is sent again once: {:?}",
                h.sent()
            );
            let build = h.made("fac/12-1").await;
            let session = h.sessions_in(build.body.id).remove(0);
            let (_, turn) = own_turn(&h.fleet.daemons[1], session.body.id).unwrap();
            assert_eq!(turn.prompts, 1, "the session took the launch once");
            assert!(named(&h, 12, "session_interrupted").is_empty());
            h.no_unknown_calls();
        },
    );
}

#[test]
fn a_turn_that_ends_before_its_session_turn_reaches_the_coordinator_is_not_typed_twice() {
    isolated(
        module_path!(),
        "a_turn_that_ends_before_its_session_turn_reaches_the_coordinator_is_not_typed_twice",
        || async {
            let h = Harness::start(Options {
                builders: vec![1],
                ..Options::default()
            })
            .await;
            h.github.pull(PR, "fac/12-1", 12);
            h.script(
                12,
                &format!(
                    "{}{COMMITS}agent '- hook: {{event: SessionStart}}\n- wait_prompt\n{DONE}'\n",
                    waits_for("go"),
                ),
            );
            h.ready(12, "Make the widgets faster");
            h.reaches(12, "factory:building").await;
            let build = h.made("fac/12-1").await;
            let session = session_in(&h, &build).await;
            hold_turns(&h, 1, &session, &build);
            std::fs::write(h.marks.join("go"), "").unwrap();

            // The turn ends on the worker; the coordinator sees the session idle and no turn.
            until(WAIT, "the turn ends on the worker", || {
                own_turn(&h.fleet.daemons[1], session.body.id).is_some_and(|(_, t)| t.turn >= 1)
            })
            .await;
            let resends = || {
                steps(&h, 12)
                    .iter()
                    .filter(|s| s.starts_with(LAUNCH_RESENDING))
                    .count()
            };
            // The worker refuses the launch asked for again, since the count rose: the ledger
            // records it taken, and no later pass asks again while the turn is held back.
            until(WAIT, "the worker's refusal is recorded", || {
                steps(&h, 12).iter().any(|s| s.starts_with(LAUNCH_TAKEN))
            })
            .await;
            let asked = resends();
            let from = h.clock_now();
            until(WAIT, "the factory passes over the held run a while", || {
                h.clock_now() >= from + 6 * SCAN
            })
            .await;
            assert_eq!(
                resends(),
                asked,
                "asked again after it was taken: {:?}",
                steps(&h, 12)
            );
            assert_eq!(
                h.state(12),
                std::collections::BTreeSet::from(["factory:building".to_owned()])
            );
            assert_eq!(launches(&h), 1, "no resend was typed: {:?}", h.sent());

            release_turns(&h, 1, session.body.id);
            h.reaches(12, "factory:pr-open").await;
            assert_eq!(launches(&h), 1, "the launch was typed once: {:?}", h.sent());
            let (_, turn) = own_turn(&h.fleet.daemons[1], session.body.id).unwrap();
            assert_eq!(turn.prompts, 1, "the session had one prompt");
            h.no_unknown_calls();
        },
    );
}

#[test]
fn a_worker_restarted_during_a_build_still_has_its_result_line_read() {
    isolated(
        module_path!(),
        "a_worker_restarted_during_a_build_still_has_its_result_line_read",
        || async {
            let mut h = Harness::start(Options {
                builders: vec![1],
                ..Options::default()
            })
            .await;
            h.github.pull(PR, "fac/12-1", 12);
            // The session takes the launch and works until the test lets it finish.
            h.script(
                12,
                &format!(
                    "agent '- hook: {{event: SessionStart}}\n- wait_prompt'\n{}{}{COMMITS}agent '{DONE}'\n",
                    marks("working"),
                    waits_for("go"),
                ),
            );
            h.ready(12, "Make the widgets faster");
            h.reaches(12, "factory:building").await;
            until(WAIT, "the session works", || {
                h.marks.join("working").exists()
            })
            .await;

            restart(&mut h, 1).await;
            std::fs::write(h.marks.join("go"), "").unwrap();

            h.reaches(12, "factory:pr-open").await;
            assert_eq!(
                named(&h, 12, "gate_checkout").len(),
                1,
                "the result line moved the run to its gate"
            );
            assert!(
                named(&h, 12, "session_interrupted").is_empty(),
                "the restart is no interruption"
            );
            assert_eq!(
                h.workspaces_named("fac/12-1").len(),
                1,
                "one build workspace"
            );
            assert!(h.workspaces_named("fac/12-1-r1").is_empty(), "no resume");
            assert_eq!(launches(&h), 1, "{:?}", h.sent());
            h.no_unknown_calls();
        },
    );
}

#[test]
fn a_worker_cut_off_from_the_lease_holder_ends_its_session_and_the_run_resumes_elsewhere() {
    isolated(
        module_path!(),
        "a_worker_cut_off_from_the_lease_holder_ends_its_session_and_the_run_resumes_elsewhere",
        || async {
            let h = Harness::start(Options::default()).await;
            h.github.pull(PR, "fac/12-1", 12);
            // The build pushes its branch and then works on for ever.
            h.script(
                12,
                &format!(
                    "{COMMITS}agent '- hook: {{event: SessionStart}}\n- wait_prompt'\n{}agent '- wait_input'\n",
                    marks("working"),
                ),
            );
            // The resume finishes the work.
            h.script(
                12,
                &format!(
                    "commit more.txt 'more' 'finish the feature'\ngit push -q origin HEAD\nagent '- hook: {{event: SessionStart}}\n- wait_prompt\n{DONE}'\n"
                ),
            );
            h.ready(12, "Make the widgets faster");
            let build = h.made("fac/12-1").await;
            let worker = index(&h, build.owner);
            let elsewhere = other([1, 2], worker);
            until(WAIT, "the session works", || {
                h.marks.join("working").exists()
            })
            .await;
            assert!(
                sessions_held(&h.fleet.daemons[worker], &build)
                    .iter()
                    .any(live)
            );

            h.fleet.cut(worker);
            // Ten minutes with no word from the lease holder: the worker ends its session.
            hurry(&h, Duration::from_secs(10 * 60 + 30)).await;
            until(WAIT, "the cut-off worker ends its session", || {
                let held = sessions_held(&h.fleet.daemons[worker], &build);
                !held.is_empty() && !held.iter().any(live)
            })
            .await;
            assert!(
                h.workspaces_named("fac/12-1-r1").is_empty(),
                "no resume before the worker has been unreachable for 15 minutes"
            );
            assert!(named(&h, 12, "session_interrupted").is_empty());

            // Fifteen minutes unreachable: an interruption, resumed on the other builder.
            hurry(&h, Duration::from_secs(5 * 60)).await;
            let resume = h.made("fac/12-1-r1").await;
            assert_eq!(
                resume.owner, h.fleet.daemons[elsewhere].id,
                "the run resumes on the machine that was not cut off"
            );
            h.reaches(12, "factory:pr-open").await;
            assert_eq!(named(&h, 12, "session_interrupted").len(), 1);
            super::git(&h.origin, &["cat-file", "-e", "fac/12-1:more.txt"]);
            h.no_unknown_calls();
        },
    );
}

#[test]
fn a_claim_whose_machine_never_answers_is_launched_on_another() {
    isolated(
        module_path!(),
        "a_claim_whose_machine_never_answers_is_launched_on_another",
        || async {
            let h = Harness::start(Options::default()).await;
            h.github.pull(PR, "fac/12-1", 12);
            h.script(
                12,
                &format!(
                    "{COMMITS}agent '- hook: {{event: SessionStart}}\n- wait_prompt\n{DONE}'\n"
                ),
            );
            // The claim names a machine, which is cut off before the claim's pass asks it for
            // the build.
            let cutting = cut_off_at_claim(&h, 12);
            h.ready(12, "Make the widgets faster");
            until(WAIT, "the claimed machine is cut off", || {
                cutting.is_finished()
            })
            .await;
            let cut_off = cutting
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
            let claimed = index(&h, cut_off);
            let elsewhere = other([1, 2], claimed);
            until(WAIT, "the claim comment names the machine cut off", || {
                h.github
                    .comments_on(12)
                    .iter()
                    .any(|c| c.contains("<!-- foundry:claim ") && c.contains(&cut_off.to_string()))
            })
            .await;

            hurry(&h, Duration::from_secs(15 * 60 + 30)).await;
            let build = h.made("fac/12-1").await;
            assert_eq!(
                build.owner, h.fleet.daemons[elsewhere].id,
                "the build is launched on the machine that answers"
            );
            h.reaches(12, "factory:pr-open").await;
            assert!(
                h.workspaces_named("fac/12-1")
                    .iter()
                    .all(|w| w.owner != cut_off),
                "the machine cut off never made the build"
            );
            let moved = format!("{CLAIM_MOVED}{}", h.fleet.daemons[elsewhere].id);
            assert!(
                steps(&h, 12).contains(&moved),
                "the claim is moved: {:?}",
                steps(&h, 12)
            );
            assert!(
                named(&h, 12, "session_interrupted").is_empty(),
                "a moved claim spends no resume"
            );
            h.no_unknown_calls();
        },
    );
}

#[test]
fn a_gate_whose_machine_goes_offline_is_run_again_on_another() {
    isolated(
        module_path!(),
        "a_gate_whose_machine_goes_offline_is_run_again_on_another",
        || gate_machine_cut_off(false),
    );
}

#[test]
fn a_gate_on_the_build_machine_cut_off_is_run_again_on_another() {
    isolated(
        module_path!(),
        "a_gate_on_the_build_machine_cut_off_is_run_again_on_another",
        || gate_machine_cut_off(true),
    );
}

/// A gate that hangs, placed on the build machine (`on_build`) or on the other builder, whose
/// machine is cut off mid-gate: within the five minutes the factory waits for a machine, the
/// gate could not run and runs again on the machine that answers.
async fn gate_machine_cut_off(on_build: bool) {
    // The first gate hangs; any gate after it passes.
    let h = Harness::start(Options {
        config: config(
            &[],
            &[
                "mkdir {marks}/gate-hangs 2>/dev/null && sleep 100000 || true",
                "test -f feature.txt",
            ],
            2,
        ),
        ..Options::default()
    })
    .await;
    h.github.pull(PR, "fac/12-1", 12);
    h.script(
        12,
        &format!("{COMMITS}agent '- hook: {{event: SessionStart}}\n- wait_prompt\n{DONE}'\n"),
    );
    h.ready(12, "Make the widgets faster");
    // The machine that is not to take the gate shows a session, so the gate goes to the other.
    let build = h.made("fac/12-1").await;
    let built_on = index(&h, build.owner);
    let busier = if on_build {
        other([1, 2], built_on)
    } else {
        built_on
    };
    h.fleet.daemons[busier].write(sample(1));

    let first = h.made("fac/12-1-g1").await;
    assert_eq!(
        first.owner == build.owner,
        on_build,
        "the gate runs on the machine it was sent to"
    );
    until(
        WAIT,
        "the first gate runs, and the coordinator knows it",
        || {
            h.marks.join("gate-hangs").exists()
                && steps(&h, 12)
                    .iter()
                    .any(|s| s.starts_with("gate-created 1 "))
                && h.fleet.daemons[0]
                    .held(|r| matches!(r, AnyRecord::Workspace(w) if w.body.id == first.body.id))
                    .is_some()
        },
    )
    .await;
    let gone = index(&h, first.owner);
    h.fleet.cut(gone);

    // Five minutes unreachable: the gate could not run and is tried elsewhere.
    hurry(&h, Duration::from_secs(5 * 60 + 30)).await;
    let second = h.made("fac/12-1-g2").await;
    assert_ne!(
        second.owner, first.owner,
        "the gate runs again on another machine"
    );
    assert_eq!(second.owner, h.fleet.daemons[other([1, 2], gone)].id);
    assert!(
        steps(&h, 12)
            .iter()
            .any(|s| s.starts_with(GATE_COULD_NOT_RUN) && s.contains("unreachable")),
        "the gate is marked could not run: {:?}",
        steps(&h, 12)
    );
    if on_build {
        // The build session is ended at the pass: its machine is gone after fifteen minutes.
        hurry(&h, Duration::from_secs(10 * 60)).await;
    }
    h.reaches(12, "factory:pr-open").await;
    assert!(
        named(&h, 12, "gate_sent_back").is_empty(),
        "a gate that could not run is no gate failure"
    );
    assert!(
        !h.sent()
            .iter()
            .any(|m| m.line.starts_with("The factory gate failed")),
        "{:?}",
        h.sent()
    );
    h.no_unknown_calls();
}

#[test]
fn a_gate_command_that_hangs_is_killed_at_its_limit_counted_as_a_failure_and_frees_its_slot() {
    isolated(
        module_path!(),
        "a_gate_command_that_hangs_is_killed_at_its_limit_counted_as_a_failure_and_frees_its_slot",
        || async {
            shorten_command_limit(3);
            let h = Harness::start(Options {
                config: config(
                    &[],
                    &[
                        "mkdir {marks}/gate-hangs 2>/dev/null && sleep 100000 || true",
                        "test -f feature.txt",
                    ],
                    2,
                ),
                ..Options::default()
            })
            .await;
            // One gate slot: the gate after the failure starts only once the first's is free.
            set_max_gates(&h, 1).await;
            h.github.pull(PR, "fac/12-1", 12);
            h.script(
                12,
                &format!(
                    "{COMMITS}agent '- hook: {{event: SessionStart}}\n- wait_prompt\n- stop: {{last_assistant_message: \"Opened the pull request.\\nFACTORY-DONE: https://github.com/acme/widgets/pull/101\"}}\n- wait_prompt\n{DONE}'\n"
                ),
            );
            h.ready(12, "Make the widgets faster");

            h.reaches(12, "factory:pr-open").await;
            let failures: Vec<_> = h
                .sent()
                .into_iter()
                .filter(|m| m.line.starts_with("The factory gate failed"))
                .collect();
            assert_eq!(failures.len(), 1, "one failure sent back: {:?}", h.sent());
            let text = failures[0].text();
            assert!(
                text.contains("mkdir") && text.contains("sleep 100000"),
                "the failure names the command that hung: {text}"
            );
            assert!(
                text.contains(&COMMAND_LIMIT.as_secs().to_string()) || text.contains("30 minutes"),
                "the failure says the command ran past its limit: {text}"
            );
            assert_eq!(named(&h, 12, "gate_sent_back").len(), 1);
            let gates: Vec<u64> = named(&h, 12, "gate_checkout")
                .iter()
                .map(|d| d["attempt"].as_u64().unwrap())
                .collect();
            assert_eq!(gates, vec![1, 2], "the failed gate and the one after it");
            assert!(
                h.workspaces_named("fac/12-1-g1").iter().all(|w| w.deleted),
                "the gate that hung is archived"
            );
            h.no_unknown_calls();
        },
    );
}

#[test]
fn a_session_that_hangs_on_a_gate_failure_sent_back_is_ended_by_limits_minutes() {
    isolated(
        module_path!(),
        "a_session_that_hangs_on_a_gate_failure_sent_back_is_ended_by_limits_minutes",
        || async {
            // A minute to build: the session takes the failure sent back and never answers.
            let h = Harness::start(Options {
                builders: vec![1],
                config: config(&[], &["test -f fixed.txt"], 2)
                    .replace("minutes: 180", "minutes: 1"),
                ..Options::default()
            })
            .await;
            h.github.pull(PR, "fac/12-1", 12);
            h.script(
                12,
                &format!(
                    "{COMMITS}agent '- hook: {{event: SessionStart}}\n- wait_prompt\n- stop: {{last_assistant_message: \"Opened the pull request.\\nFACTORY-DONE: https://github.com/acme/widgets/pull/101\"}}\n- wait_prompt\n- wait_input'\n"
                ),
            );
            h.ready(12, "Make the widgets faster");

            h.reaches(12, "factory:stuck").await;
            assert!(
                h.sent()
                    .iter()
                    .any(|m| m.line.starts_with("The factory gate failed")),
                "the failure was sent back: {:?}",
                h.sent()
            );
            let comments = h.github.comments_on(12);
            assert!(
                comments.iter().any(|c| c.contains(TIME_LIMIT)),
                "the stuck comment says the run passed its time limit: {comments:#?}"
            );
            let build = h.made("fac/12-1").await;
            until(WAIT, "the stuck run's session is ended", || {
                let sessions = h.sessions_in(build.body.id);
                !sessions.is_empty() && !sessions.iter().any(live)
            })
            .await;
            assert!(
                h.workspaces_named("fac/12-1").iter().all(|w| !w.deleted),
                "a stuck run's workspace is kept"
            );
            h.no_unknown_calls();
        },
    );
}
