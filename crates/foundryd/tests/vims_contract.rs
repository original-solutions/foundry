//! The contract with Vims that `factory::vims` rests on, checked against the real thing.
//!
//! One workspace, made from the template named in `FOUNDRY_VIMS_TEMPLATE` through the real
//! `vims` command line, signed in with the token in `FOUNDRY_VIMS_TOKEN` as the daemon signs in.
//! The test reads the template's source key, makes the workspace, runs `true` with
//! `exec --no-wait`, reads its exit code by operation, and removes the workspace. The removal is
//! armed as soon as `vims new` answers, so a step that fails after it still removes the
//! workspace; when the workspace cannot be found, the test prints the create's operation and key
//! so it can be removed by hand.
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
use foundryd::factory::vims::{Egress, Exec, OpState, Operation, Role, Step, Vims, VimsCli, key};

/// How long a create or a command may take to finish.
const WAIT: Duration = Duration::from_secs(600);
/// How often an operation is read while it runs.
const POLL: Duration = Duration::from_secs(3);

/// Removes the workspace a create made when the test ends, however it ends: the one it names
/// when it is known, else the one the create's operation names by then.
struct Removal<'a> {
    vims: &'a VimsCli,
    /// The create's operation and key.
    create: String,
    create_key: String,
    workspace: Option<String>,
    key: String,
}

impl Drop for Removal<'_> {
    fn drop(&mut self) {
        let workspace = self.workspace.clone().or_else(|| {
            self.vims
                .operation(&self.create)
                .ok()
                .and_then(|operation| operation.workspace)
        });
        let Some(workspace) = workspace else {
            eprintln!(
                "vims_contract: no workspace found for the create {} (key {}); look for it with \
                 `vims ls` and remove it by hand",
                self.create, self.create_key
            );
            return;
        };
        match self.vims.remove(&workspace, &self.key) {
            Ok(change) => eprintln!("vims_contract: removed {workspace}: {change:?}"),
            Err(error) => {
                eprintln!("vims_contract: could not remove {workspace}; remove it by hand: {error}")
            }
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
    eprintln!(
        "vims_contract: create {} (key {create}) for {:?}",
        made.id, made.workspace
    );
    let mut removal = Removal {
        vims: &vims,
        create: made.id.clone(),
        create_key: create.clone(),
        workspace: made.workspace.clone(),
        key: key_of(Step::Remove),
    };

    let made = ended(&vims, &made.id);
    if removal.workspace.is_none() {
        removal.workspace = made.workspace.clone();
    }
    assert_eq!(made.state, OpState::Finished, "{made:?}");
    let workspace = removal
        .workspace
        .clone()
        .unwrap_or_else(|| panic!("the create names no workspace: {made:?}"));
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
