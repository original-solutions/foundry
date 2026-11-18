//! One-time join codes and machine keys. Filled by plan task 13.

use std::process::ExitCode;

/// `foundryd join <address> <code>`.
pub fn join(_address: &str, _code: &str) -> ExitCode {
    crate::not_built("join")
}
