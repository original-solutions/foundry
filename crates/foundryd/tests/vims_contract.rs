//! The contract with Vims that `factory::vims` rests on, checked against the real thing.
//!
//! One workspace, made from the template named in `FOUNDRY_VIMS_TEMPLATE` through the real
//! `vims` command line, signed in with the token in `FOUNDRY_VIMS_TOKEN` as the daemon signs in.
//! The test reads the template's source key, makes the workspace with a key, finds the create's
//! operation again by that key, runs `true` with `exec --no-wait`, reads its exit code by
//! operation, and removes the workspace, also when a step before it fails.
//!
//! It spends Vims credit and makes no model call, so it is ignored and runs only with
//! `FOUNDRY_VIMS_CONTRACT=1` and `vims` on `PATH`:
//!
//! ```text
//! FOUNDRY_VIMS_CONTRACT=1 FOUNDRY_VIMS_TEMPLATE=<template> FOUNDRY_VIMS_TOKEN=<token> \
//!   cargo test -p foundryd --test vims_contract -- --ignored --nocapture
//! ```

use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use foundry_proto::factory::RunId;
use foundryd::clock::SystemClock;
use foundryd::factory::vims::{
    Egress, Exec, OpState, Operation, Role, Step, Vims, VimsCli, VimsError, key,
};

/// How long a create or a command may take to finish.
const WAIT: Duration = Duration::from_secs(600);
/// How often an operation is read while it runs.
const POLL: Duration = Duration::from_secs(3);

/// Removes the workspace when the test ends, however it ends.
struct Removal<'a> {
    vims: &'a VimsCli,
    workspace: String,
    key: String,
}

impl Drop for Removal<'_> {
    fn drop(&mut self) {
        match self.vims.remove(&self.workspace, &self.key) {
            Ok(change) => eprintln!("vims_contract: removed {}: {change:?}", self.workspace),
            Err(error) => eprintln!(
                "vims_contract: could not remove {}; remove it by hand: {error}",
                self.workspace
            ),
        }
    }
}

/// The operation `id` once it has ended.
fn ended(vims: &VimsCli, id: &str) -> Operation {
    let begun = Instant::now();
    loop {
        let operation = vims.operation(id).expect("operation show");
        if operation.state != OpState::Running {
            return operation;
        }
        assert!(begun.elapsed() < WAIT, "{id} still running after {WAIT:?}");
        thread::sleep(POLL);
    }
}

#[test]
#[ignore = "spends Vims credit: run with FOUNDRY_VIMS_CONTRACT=1 and --ignored"]
fn a_workspace_runs_a_command_read_by_operation_and_is_removed() {
    if std::env::var("FOUNDRY_VIMS_CONTRACT").as_deref() != Ok("1") {
        eprintln!("vims_contract: skipped, FOUNDRY_VIMS_CONTRACT is not 1");
        return;
    }
    let template = std::env::var("FOUNDRY_VIMS_TEMPLATE").expect("FOUNDRY_VIMS_TEMPLATE");
    let token = std::env::var("FOUNDRY_VIMS_TOKEN").expect("FOUNDRY_VIMS_TOKEN");
    let vims = VimsCli::new(token, Arc::new(SystemClock));

    // A run of its own each time, so no key is one an earlier run of this test sent.
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let run = RunId::new(
        "original-solutions/foundry",
        356,
        u32::try_from(now.as_secs() % u64::from(u32::MAX)).unwrap(),
    )
    .unwrap();
    let key_of = |step: Step| key(&run, Role::Build, &step, 1);

    let template = vims.template(&template).expect("templates show");
    let source_key = template.source_key.expect("the template's source key");
    eprintln!("vims_contract: making a workspace from {source_key}");

    let create = key_of(Step::Create);
    let made = vims
        .new_workspace(&source_key, Egress::Open, &create)
        .expect("new");
    let found = vims.operations_by_key(&create).expect("operation list");
    assert_eq!(
        found
            .iter()
            .map(|operation| &operation.id)
            .collect::<Vec<_>>(),
        [&made.id],
        "the create is found by its key"
    );
    let again = vims
        .new_workspace(&source_key, Egress::Open, &create)
        .expect("new again");
    assert_eq!(again.id, made.id, "a repeated key makes nothing new");
    let refused = vims
        .new_workspace(&source_key, Egress::Closed, &create)
        .unwrap_err();
    assert!(
        matches!(&refused, VimsError::Refused { refusal, .. } if refusal.code == "IDEMPOTENCY_KEY_REUSED"),
        "{refused}"
    );

    let made = ended(&vims, &made.id);
    let workspace = made
        .workspace
        .clone()
        .unwrap_or_else(|| panic!("the create names no workspace: {made:?}"));
    let _removal = Removal {
        vims: &vims,
        workspace: workspace.clone(),
        key: key_of(Step::Remove),
    };
    assert_eq!(made.state, OpState::Finished, "{made:?}");
    let shown = vims.show(&workspace).expect("show");
    eprintln!("vims_contract: {workspace} is {:?}", shown.state);

    let exec = Exec {
        cwd: None,
        timeout_seconds: Some(60),
        no_wait: true,
        argv: vec!["true".into()],
        stdin: Vec::new(),
    };
    let ran = vims
        .exec(&workspace, &exec, &key_of(Step::Provision(1)))
        .expect("exec");
    let ran = ended(&vims, &ran.id);
    assert_eq!(ran.state, OpState::Finished, "{ran:?}");
    assert_eq!(ran.exit_code, Some(0), "{ran:?}");
}
