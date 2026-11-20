//! `foundryd`: the command line. Each command calls the module that owns it; one whose plan task
//! is not done yet exits 2.

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use foundryd::{daemon, join, secrets, service, status};

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
    /// Keep a secret in this machine's secret store.
    Secret {
        #[command(subcommand)]
        command: SecretCommand,
    },
}

#[derive(Subcommand)]
enum SecretCommand {
    /// Store the gateway key (`gateway`) or its management key (`gateway-management`), read as
    /// one line from stdin.
    Set {
        /// The secret's name: `gateway` or `gateway-management`.
        name: String,
    },
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Run => daemon::run(),
        Command::Status => status::status(),
        Command::Install => service::install(),
        Command::Uninstall => service::uninstall(),
        Command::Join { address, code } => join::join(&address, &code),
        Command::Secret {
            command: SecretCommand::Set { name },
        } => secrets::set_command(&name),
    }
}
