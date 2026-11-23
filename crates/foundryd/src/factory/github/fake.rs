//! A tracker in memory, for tests.
//!
//! [`FakeTracker`] answers each `gh` call from a script, as the runner's tests script `gh`:
//! [`FakeTracker::on`] answers a call with exactly the arguments given, and
//! [`FakeTracker::then`] answers, in order, the calls no script for their arguments answers.
//! Its answers go through [`finish`], so a scripted failure reads as the real `gh`'s would. A
//! call nothing answers fails as a `gh` that could not start. Every call is recorded, in order.

use std::collections::VecDeque;
use std::sync::{Mutex, PoisonError};

use super::gh::{Answer, command_text, finish};
use super::{Tracker, TrackerError};

/// A tracker that answers from a script and records every call.
#[derive(Debug, Default)]
pub struct FakeTracker {
    script: Mutex<Script>,
}

#[derive(Debug, Default)]
struct Script {
    /// Answers for exact argument lists, each answered in order.
    by_args: Vec<(Vec<String>, VecDeque<Answer>)>,
    /// Answers for any call no `by_args` entry answers, in order.
    next: VecDeque<Answer>,
    calls: Vec<Vec<String>>,
}

impl FakeTracker {
    /// A tracker with nothing scripted.
    pub fn new() -> Self {
        Self::default()
    }

    /// A tracker that answers the calls in order with `answers`, whatever their arguments.
    pub fn answering(answers: impl IntoIterator<Item = Answer>) -> Self {
        let fake = Self::new();
        for answer in answers {
            fake.then(answer);
        }
        fake
    }

    /// Answers the next call with exactly `args` with `answer`. Several answers for the same
    /// arguments answer their calls in the order they were scripted.
    pub fn on<I, S>(&self, args: I, answer: Answer) -> &Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let args: Vec<String> = args.into_iter().map(Into::into).collect();
        let mut script = self.lock();
        match script.by_args.iter_mut().find(|(known, _)| *known == args) {
            Some((_, answers)) => answers.push_back(answer),
            None => script.by_args.push((args, VecDeque::from([answer]))),
        }
        self
    }

    /// Answers the next call that no script for its arguments answers with `answer`.
    pub fn then(&self, answer: Answer) -> &Self {
        self.lock().next.push_back(answer);
        self
    }

    /// Every call so far, in order.
    pub fn calls(&self) -> Vec<Vec<String>> {
        self.lock().calls.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Script> {
        self.script.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Tracker for FakeTracker {
    fn gh(&self, args: &[String]) -> Result<String, TrackerError> {
        let answer = {
            let mut script = self.lock();
            script.calls.push(args.to_vec());
            let scripted = script
                .by_args
                .iter_mut()
                .find(|(known, answers)| known.as_slice() == args && !answers.is_empty())
                .and_then(|(_, answers)| answers.pop_front());
            scripted.or_else(|| script.next.pop_front())
        };
        let answer = answer.unwrap_or_else(|| {
            Answer::NotStarted(format!("no answer scripted for `{}`", command_text(args)))
        });
        finish(args, answer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| part.to_string()).collect()
    }

    #[test]
    fn a_call_is_answered_by_the_script_for_its_arguments() {
        let fake = FakeTracker::new();
        fake.on(["api", "user"], Answer::ok("{\"login\":\"rawphp\"}"))
            .on(["api", "repos/o/r"], Answer::ok("{}"));
        assert_eq!(fake.gh(&args(&["api", "repos/o/r"])).unwrap(), "{}");
        assert_eq!(
            fake.gh(&args(&["api", "user"])).unwrap(),
            "{\"login\":\"rawphp\"}"
        );
    }

    #[test]
    fn answers_for_the_same_arguments_come_in_the_order_scripted() {
        let fake = FakeTracker::new();
        fake.on(["api", "user"], Answer::ok("one"))
            .on(["api", "user"], Answer::ok("two"));
        assert_eq!(fake.gh(&args(&["api", "user"])).unwrap(), "one");
        assert_eq!(fake.gh(&args(&["api", "user"])).unwrap(), "two");
    }

    #[test]
    fn a_call_no_script_answers_takes_the_next_answer_in_order() {
        let fake = FakeTracker::answering([Answer::ok("first"), Answer::ok("second")]);
        fake.on(["api", "user"], Answer::ok("by args"));
        assert_eq!(fake.gh(&args(&["issue", "list"])).unwrap(), "first");
        assert_eq!(fake.gh(&args(&["api", "user"])).unwrap(), "by args");
        assert_eq!(fake.gh(&args(&["api", "user"])).unwrap(), "second");
    }

    #[test]
    fn a_call_nothing_answers_is_a_gh_that_could_not_start() {
        let fake = FakeTracker::new();
        assert_eq!(
            fake.gh(&args(&["api", "user"])).unwrap_err(),
            TrackerError::Other(
                "cannot start `gh api user`: no answer scripted for `gh api user`".into()
            )
        );
    }

    #[test]
    fn a_scripted_failure_reads_as_the_real_ghs_would() {
        let fake = FakeTracker::answering([
            Answer::fail(1, "gh: Too Many Requests (HTTP 429)"),
            Answer::TimedOut,
            Answer::fail(1, "HTTP 404: Not Found"),
        ]);
        let call = args(&["api", "user"]);
        assert!(matches!(
            fake.gh(&call).unwrap_err(),
            TrackerError::RateLimited { .. }
        ));
        assert!(matches!(
            fake.gh(&call).unwrap_err(),
            TrackerError::Timeout { seconds: 30, .. }
        ));
        assert!(matches!(
            fake.gh(&call).unwrap_err(),
            TrackerError::NotFound { .. }
        ));
    }

    #[test]
    fn every_call_is_recorded_in_order() {
        let fake = FakeTracker::answering([Answer::ok(""), Answer::ok("")]);
        fake.gh(&args(&["issue", "list"])).unwrap();
        fake.gh(&args(&["api", "user"])).unwrap();
        let _ = fake.gh(&args(&["unanswered"]));
        assert_eq!(
            fake.calls(),
            vec![
                args(&["issue", "list"]),
                args(&["api", "user"]),
                args(&["unanswered"])
            ]
        );
    }

    #[test]
    fn a_fake_is_a_tracker_other_tasks_can_share() {
        let fake = std::sync::Arc::new(FakeTracker::new());
        let tracker: std::sync::Arc<dyn Tracker> = fake.clone();
        fake.then(Answer::ok("shared"));
        assert_eq!(tracker.gh(&args(&["api", "user"])).unwrap(), "shared");
        assert_eq!(fake.calls().len(), 1);
    }
}
