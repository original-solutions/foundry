//! Capturing what the crate logs, for its unit tests.

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
        assert_eq!(logged.text(), Logged::everywhere().text());
    }
}
