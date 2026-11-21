//! `foundryd dashboard`: opens the local daemon's web UI in the default browser.

use std::process::{Command, ExitCode};

/// The program that opens a URL in the default browser.
const OPENER: &str = if cfg!(target_os = "macos") {
    "open"
} else {
    "xdg-open"
};

/// `foundryd dashboard`.
pub fn dashboard() -> ExitCode {
    let port = match crate::daemon::port() {
        Ok(port) => port,
        Err(error) => {
            eprintln!("foundryd dashboard: {error}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(error) = crate::status::fetch(port) {
        eprintln!("foundryd dashboard: {error}");
        return ExitCode::FAILURE;
    }
    let url = url(port);
    println!("{url}");
    match Command::new(OPENER).arg(&url).status() {
        Ok(status) if status.success() => ExitCode::SUCCESS,
        Ok(status) => {
            eprintln!("foundryd dashboard: `{OPENER} {url}` failed ({status}); open it yourself");
            ExitCode::FAILURE
        }
        Err(error) => {
            eprintln!("foundryd dashboard: cannot run {OPENER} ({error}); open {url} yourself");
            ExitCode::FAILURE
        }
    }
}

/// The UI's address on loopback.
fn url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ui_is_opened_on_loopback_at_the_daemon_s_port() {
        assert_eq!(url(7433), "http://127.0.0.1:7433/");
    }
}
