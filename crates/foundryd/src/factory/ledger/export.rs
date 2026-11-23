//! `foundryd factory export <dir>`: every run this machine holds, written in the runner's layout.

use std::path::Path;
use std::process::ExitCode;

/// `foundryd factory export <dir>`. Not built yet: exits 2.
pub fn command(_dir: &Path) -> ExitCode {
    crate::not_built("factory export")
}
