//! `foundryd`: the command line. Each command is wired to its module by the plan task that builds
//! it; until then it exits 2.

use std::process::ExitCode;

use clap::{Parser, Subcommand};

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

impl Command {
    fn name(&self) -> &'static str {
        match self {
            Command::Run => "run",
            Command::Install => "install",
            Command::Uninstall => "uninstall",
            Command::Status => "status",
            Command::Join { .. } => "join",
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    eprintln!("foundryd {}: not built yet", cli.command.name());
    ExitCode::from(2)
}
