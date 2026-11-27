//! The idempotency key of each `vims` call that makes or changes something (P2c, "Keys").
//!
//! A key is made only from the run, which names the repo, the workspace's role and round, the
//! step and the attempt, so a new lease holder can work out every key the one before it could
//! have sent. It reads `foundry/<run>/<role>/<step>/<attempt>`, for example
//! `foundry/trilogy-care__tc-supplier-portal-v2__1680__2/build/provision:3/1`. The attempt is
//! last and is a number, so no two calls share a key even when a step carries an operation id.
//! Vims has said nothing of a limit on a key's characters or length; these keep to letters,
//! digits and `_ - : /`.

use std::fmt;

use foundry_proto::factory::RunId;

/// A workspace's role in its run, and its round.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Role {
    /// The run's build workspace: `build`.
    Build,
    /// The k-th gate's workspace: `g<k>`.
    Gate(u32),
    /// The k-th resume's workspace: `r<k>`.
    Resume(u32),
    /// The k-th feedback round's workspace: `f<k>`.
    Feedback(u32),
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Role::Build => f.write_str("build"),
            Role::Gate(k) => write!(f, "g{k}"),
            Role::Resume(k) => write!(f, "r{k}"),
            Role::Feedback(k) => write!(f, "f{k}"),
        }
    }
}

/// One step done to a workspace. Anything done more than once to one workspace has its count in
/// the step, so no two actions share a key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Step {
    /// `vims new`: `create`.
    Create,
    /// The fetch of the repo: `fetch`.
    Fetch,
    /// The checkout of the branch or commit: `checkout`.
    Checkout,
    /// The i-th `provision` command: `provision:<i>`.
    Provision(usize),
    /// The i-th `gate` command: `gate:<i>`.
    Gate(usize),
    /// The body written for turn n: `body:<n>`.
    Body(u32),
    /// The launch of turn n: `turn:<n>`.
    Turn(u32),
    /// The gate's read-only grant: `grant`.
    Grant,
    /// The grant taken away: `ungrant`.
    Ungrant,
    /// The workspace's k-th pause: `pause:<k>`.
    Pause(u32),
    /// The workspace's k-th start: `start:<k>`.
    Start(u32),
    /// The cancel of an operation: `cancel:<operation>`.
    Cancel(String),
    /// The end of turn n: `end-turn:<n>`.
    EndTurn(u32),
    /// `vims stop`: `stop`.
    Stop,
    /// `vims remove`: `remove`.
    Remove,
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Step::Create => f.write_str("create"),
            Step::Fetch => f.write_str("fetch"),
            Step::Checkout => f.write_str("checkout"),
            Step::Provision(i) => write!(f, "provision:{i}"),
            Step::Gate(i) => write!(f, "gate:{i}"),
            Step::Body(n) => write!(f, "body:{n}"),
            Step::Turn(n) => write!(f, "turn:{n}"),
            Step::Grant => f.write_str("grant"),
            Step::Ungrant => f.write_str("ungrant"),
            Step::Pause(k) => write!(f, "pause:{k}"),
            Step::Start(k) => write!(f, "start:{k}"),
            Step::Cancel(operation) => write!(f, "cancel:{operation}"),
            Step::EndTurn(n) => write!(f, "end-turn:{n}"),
            Step::Stop => f.write_str("stop"),
            Step::Remove => f.write_str("remove"),
        }
    }
}

/// The idempotency key of `step` on the workspace of `role` in `run`, at `attempt` (the first
/// is 1). The run names the repo, the issue and the run's own attempt.
pub fn key(run: &RunId, role: Role, step: &Step, attempt: u32) -> String {
    format!("foundry/{run}/{role}/{step}/{attempt}")
}

#[cfg(test)]
mod tests {
    use foundry_proto::factory::RunId;

    use super::*;

    fn run() -> RunId {
        RunId::new("Trilogy-Care/tc-supplier-portal-v2", 1680, 2).unwrap()
    }

    fn build(step: Step) -> String {
        key(&run(), Role::Build, &step, 1)
    }

    const RUN: &str = "trilogy-care__tc-supplier-portal-v2__1680__2";

    #[test]
    fn create() {
        assert_eq!(build(Step::Create), format!("foundry/{RUN}/build/create/1"));
    }

    #[test]
    fn fetch() {
        assert_eq!(build(Step::Fetch), format!("foundry/{RUN}/build/fetch/1"));
    }

    #[test]
    fn checkout() {
        assert_eq!(
            build(Step::Checkout),
            format!("foundry/{RUN}/build/checkout/1")
        );
    }

    #[test]
    fn provision_carries_the_command_s_number() {
        assert_eq!(
            build(Step::Provision(3)),
            format!("foundry/{RUN}/build/provision:3/1")
        );
    }

    #[test]
    fn gate_carries_the_command_s_number() {
        assert_eq!(
            key(&run(), Role::Gate(1), &Step::Gate(2), 1),
            format!("foundry/{RUN}/g1/gate:2/1")
        );
    }

    #[test]
    fn body_carries_the_turn() {
        assert_eq!(
            build(Step::Body(2)),
            format!("foundry/{RUN}/build/body:2/1")
        );
    }

    #[test]
    fn turn_carries_its_number() {
        assert_eq!(
            build(Step::Turn(1)),
            format!("foundry/{RUN}/build/turn:1/1")
        );
    }

    #[test]
    fn grant() {
        assert_eq!(
            key(&run(), Role::Gate(2), &Step::Grant, 1),
            format!("foundry/{RUN}/g2/grant/1")
        );
    }

    #[test]
    fn ungrant() {
        assert_eq!(
            key(&run(), Role::Gate(2), &Step::Ungrant, 1),
            format!("foundry/{RUN}/g2/ungrant/1")
        );
    }

    #[test]
    fn pause_carries_its_count() {
        assert_eq!(
            build(Step::Pause(2)),
            format!("foundry/{RUN}/build/pause:2/1")
        );
    }

    #[test]
    fn start_carries_its_count() {
        assert_eq!(
            build(Step::Start(1)),
            format!("foundry/{RUN}/build/start:1/1")
        );
    }

    #[test]
    fn cancel_carries_the_operation() {
        assert_eq!(
            build(Step::Cancel("op_8f2k".into())),
            format!("foundry/{RUN}/build/cancel:op_8f2k/1")
        );
    }

    #[test]
    fn end_turn_carries_the_turn() {
        assert_eq!(
            build(Step::EndTurn(3)),
            format!("foundry/{RUN}/build/end-turn:3/1")
        );
    }

    #[test]
    fn stop() {
        assert_eq!(build(Step::Stop), format!("foundry/{RUN}/build/stop/1"));
    }

    #[test]
    fn remove() {
        assert_eq!(build(Step::Remove), format!("foundry/{RUN}/build/remove/1"));
    }

    #[test]
    fn each_role_and_round_is_its_own() {
        let keys: Vec<String> = [
            Role::Build,
            Role::Gate(1),
            Role::Gate(2),
            Role::Resume(1),
            Role::Feedback(1),
        ]
        .into_iter()
        .map(|role| key(&run(), role, &Step::Create, 1))
        .collect();
        assert_eq!(
            keys,
            [
                format!("foundry/{RUN}/build/create/1"),
                format!("foundry/{RUN}/g1/create/1"),
                format!("foundry/{RUN}/g2/create/1"),
                format!("foundry/{RUN}/r1/create/1"),
                format!("foundry/{RUN}/f1/create/1"),
            ]
        );
    }

    #[test]
    fn the_attempt_is_last() {
        assert_eq!(
            key(&run(), Role::Build, &Step::Create, 3),
            format!("foundry/{RUN}/build/create/3")
        );
    }

    #[test]
    fn the_repo_and_the_run_are_in_the_key() {
        let other_issue = RunId::new("Trilogy-Care/tc-supplier-portal-v2", 1681, 2).unwrap();
        let other_attempt = RunId::new("Trilogy-Care/tc-supplier-portal-v2", 1680, 3).unwrap();
        let other_repo = RunId::new("Trilogy-Care/tc-people", 1680, 2).unwrap();
        let mine = key(&run(), Role::Build, &Step::Create, 1);
        for other in [other_issue, other_attempt, other_repo] {
            assert_ne!(key(&other, Role::Build, &Step::Create, 1), mine);
        }
    }
}
