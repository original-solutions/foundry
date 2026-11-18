//! The launchd and systemd units. Filled by plan task 18.

use std::process::ExitCode;

/// `foundryd install`.
pub fn install() -> ExitCode {
    crate::not_built("install")
}

/// `foundryd uninstall`.
pub fn uninstall() -> ExitCode {
    crate::not_built("uninstall")
}
