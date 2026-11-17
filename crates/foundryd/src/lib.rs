//! The Foundry daemon. Each module has one job; `main.rs` is only the command line.

pub mod api;
pub mod attention;
pub mod coordinator;
pub mod gateway;
pub mod join;
pub mod machine;
pub mod placement;
pub mod secrets;
pub mod service;
pub mod sessions;
pub mod store;
pub mod sync;
pub mod terminals;
pub mod update;
pub mod workspaces;
