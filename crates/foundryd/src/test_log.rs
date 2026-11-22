//! Capturing what the crate logs, for its unit tests: as plain text, with no colours and no
//! times, so a test can read its warnings back.

use std::io;
use std::sync::{Arc, Mutex, OnceLock};

use tracing::level_filters::LevelFilter;
use tracing::subscriber::DefaultGuard;

/// Everything logged where it is set, as plain text.
#[derive(Clone, Default)]
pub(crate) struct Logged(Arc<Mutex<Vec<u8>>>);

impl io::Write for Logged {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Logged {
    /// Captures what this thread logs, from `INFO` up, until the guard drops.
    pub(crate) fn capture(&self) -> DefaultGuard {
        tracing::subscriber::set_default(self.subscriber(LevelFilter::INFO))
    }

    /// What every thread logs, at every level, from the first call to the end of the test run,
    /// for a test whose lines are logged on tasks a [`capture`](Logged::capture) cannot see. A
    /// test binary has one global subscriber, so every call gives the same capture, and a
    /// thread with a capture of its own logs there instead.
    pub(crate) fn everywhere() -> Logged {
        static EVERYWHERE: OnceLock<Logged> = OnceLock::new();
        EVERYWHERE
            .get_or_init(|| {
                let logged = Logged::default();
                tracing::subscriber::set_global_default(logged.subscriber(LevelFilter::TRACE))
                    .expect("no other test installs a global tracing subscriber");
                logged
            })
            .clone()
    }

    fn subscriber(&self, level: LevelFilter) -> impl tracing::Subscriber + Send + Sync + 'static {
        let logged = self.clone();
        tracing_subscriber::fmt()
            .with_max_level(level)
            .with_writer(move || logged.clone())
            .with_ansi(false)
            .without_time()
            .finish()
    }

    /// Everything logged so far.
    pub(crate) fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }

    /// The warnings logged so far, a line each.
    pub(crate) fn warnings(&self) -> Vec<String> {
        self.text()
            .lines()
            .filter(|line| line.contains("WARN"))
            .map(str::to_owned)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_capture_holds_this_threads_lines_from_info_up_and_warnings_picks_the_warnings() {
        let logged = Logged::default();
        {
            let _capture = logged.capture();
            tracing::debug!("a debug line");
            tracing::info!("an info line");
            tracing::warn!("a warning line");
        }
        tracing::warn!("a line after the capture dropped");
        std::thread::spawn(|| tracing::warn!("a line from another thread"))
            .join()
            .unwrap();

        let text = logged.text();
        assert!(!text.contains("a debug line"), "{text}");
        assert!(text.contains("an info line"), "{text}");
        assert!(!text.contains("after the capture dropped"), "{text}");
        assert!(!text.contains("another thread"), "{text}");
        let warnings = logged.warnings();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("a warning line"), "{warnings:?}");
        assert!(!warnings[0].contains('\u{1b}'), "no colours: {warnings:?}");
        assert!(
            warnings[0].trim_start().starts_with("WARN"),
            "no time before the level: {warnings:?}"
        );
    }

    #[test]
    fn everywhere_holds_every_threads_lines_at_every_level_and_is_one_capture() {
        let logged = Logged::everywhere();
        std::thread::spawn(|| tracing::trace!("a trace line from test_log's own test"))
            .join()
            .unwrap();

        let text = Logged::everywhere().text();
        assert!(
            text.contains("a trace line from test_log's own test"),
            "{text}"
        );
        assert!(Arc::ptr_eq(&logged.0, &Logged::everywhere().0));
    }
}
