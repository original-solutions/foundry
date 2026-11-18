//! Asks the local daemon for its state and prints it. Filled by plan task 20.

use std::process::ExitCode;

/// `foundryd status`.
pub fn status() -> ExitCode {
    crate::not_built("status")
}
