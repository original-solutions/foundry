//! The rules of an issue's life in the factory. Pure: no I/O.
//!
//! Ported from the runner's `src/domain/`: the state machine ([`state`]), the approval
//! verdicts and their words ([`approval`]), the `## Depends on` and `## Base` sections
//! ([`depends_on`], [`base`]) and a session's result line ([`result_line`]).

pub mod approval;
pub mod base;
pub mod depends_on;
pub mod result_line;
pub mod state;

pub use state::{Action, Event, InvalidTransition, State, transition};
