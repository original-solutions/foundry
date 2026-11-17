//! `foundryd`: the command line. Each command calls the module that owns it, whose stub exits 2
//! until its plan task fills it.

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use foundryd::{join, not_built, service};

#[derive(Parser)]
#[command(name = "foundryd", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon.
    Run,
    /// Install the daemon as a launchd agent or a systemd unit.
    Install,
    /// Remove the launchd agent or systemd unit.
    Uninstall,
    /// Show this machine's daemon and its peers.
    Status,
    /// Join a fleet with a one-time code from a machine already in it.
    Join {
        /// The address of a machine already in the fleet.
        address: String,
        /// The one-time code shown in that machine's UI.
        code: String,
    },
}

fn main() -> ExitCode {
    match Cli::parse().command {
        // The plan gives `run` and `status` to no task yet.
        Command::Run => not_built("run"),
        Command::Status => not_built("status"),
        Command::Install => service::install(),
        Command::Uninstall => service::uninstall(),
        Command::Join { address, code } => join::join(&address, &code),
    }
}
