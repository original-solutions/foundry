//! The factory: the `Factory` handle and its service, run by the acting coordinator for each
//! project whose lease it holds.

pub mod config;
pub mod cutoff;
pub mod fleet;
pub mod gate;
pub mod github;
pub mod held;
pub mod lease;
pub mod ledger;
pub mod lifecycle;
pub mod messages;
pub mod pass;
pub mod placing;
pub mod runs;
pub mod status;
pub mod text;
pub mod worker;
