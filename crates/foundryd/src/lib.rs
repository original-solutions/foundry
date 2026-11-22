//! The Foundry daemon. Each module has one job; `main.rs` is only the command line.

use std::process::ExitCode;

pub mod api;
pub mod attention;
pub mod coordinator;
pub mod daemon;
pub mod dashboard;
pub mod gateway;
pub mod join;
pub mod lister;
pub mod machine;
pub mod placement;
pub mod secrets;
pub mod service;
pub mod sessions;
pub mod status;
pub mod store;
pub mod sync;
pub mod tailscale;
pub mod terminals;
#[cfg(test)]
mod test_log;
pub mod update;
pub mod workspaces;

/// Says `command` is not built yet and returns exit code 2. A command's stub returns this until
/// the plan task that owns it replaces the stub.
pub fn not_built(command: &str) -> ExitCode {
    eprintln!("foundryd {command}: not built yet");
    ExitCode::from(2)
}
