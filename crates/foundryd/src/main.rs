//! `foundryd`: the command line. Each command calls the module that owns it; one whose plan task
//! is not done yet exits 2.

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

use foundryd::{daemon, dashboard, factory, join, secrets, service, status};

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
    /// Open this machine's web UI in the default browser.
    Dashboard,
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
    /// The GitHub logins this machine holds for the factory (`foundryd secret set github:<login>`).
    Github {
        #[command(subcommand)]
        command: GithubCommand,
    },
    /// The factory's runs.
    Factory {
        #[command(subcommand)]
        command: FactoryCommand,
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

#[derive(Subcommand)]
enum GithubCommand {
    /// Name `login` as this machine's default, read when a project's factory is turned on while
    /// this machine is coordinator.
    Default {
        /// The GitHub login.
        login: String,
    },
}

#[derive(Subcommand)]
enum FactoryCommand {
    /// Write every run this machine holds into `dir`, in the runner's layout.
    Export {
        /// The directory to write into.
        dir: PathBuf,
    },
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Run => daemon::run(),
        Command::Status => status::status(),
        Command::Dashboard => dashboard::dashboard(),
        Command::Install => service::install(),
        Command::Uninstall => service::uninstall(),
        Command::Join { address, code } => join::join(&address, &code),
        Command::Secret {
            command: SecretCommand::Set { name },
        } => secrets::set_command(&name),
        Command::Github {
            command: GithubCommand::Default { login },
        } => secrets::github_default_command(&login),
        Command::Factory {
            command: FactoryCommand::Export { dir },
        } => factory::ledger::export::command(&dir),
    }
}
