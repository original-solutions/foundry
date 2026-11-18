//! The holder: one PTY, its child and a Unix socket. The library half lets `foundryd` speak the
//! same protocol as the `foundry-holder` binary.

pub mod protocol;
pub mod pty;
pub mod scrollback;
