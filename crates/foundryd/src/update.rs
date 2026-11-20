
#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;

    use super::*;

    /// A data directory with versions 0.1.0 and 0.2.0 on disk, `current` on 0.2.0 and a
    /// trial of 0.2.0 that has not started yet.
    fn trial_home() -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        for version in ["0.1.0", "0.2.0"] {
            fs::create_dir_all(home.path().join("versions").join(version)).unwrap();
        }
        point_current(home.path(), "0.2.0").unwrap();
        save(
            home.path(),
            &State {
                trial: Some(Trial {
                    version: "0.2.0".into(),
                    previous: "0.1.0".into(),
                    starts: 0,
                }),
                refused: None,
            },
        )
        .unwrap();
        home
    }

    fn current(home: &Path) -> PathBuf {
        fs::read_link(home.join("current")).unwrap()
    }

    #[test]
    fn three_starts_that_never_report_healthy_roll_back_on_the_next() {
        let home = trial_home();

        for _ in 0..3 {
            assert_eq!(start_with(home.path(), "0.2.0").unwrap(), Start::Run);
        }
        let fourth = start_with(home.path(), "0.2.0").unwrap();

        assert_eq!(fourth, Start::RolledBack { to: "0.1.0".into() });
        assert_eq!(current(home.path()), PathBuf::from("versions/0.1.0"));
        let state = state(home.path()).unwrap();
        assert_eq!(state.refused.as_deref(), Some("v0.2.0"));
        assert_eq!(state.trial, None);
    }

    #[test]
    fn a_version_that_turns_healthy_before_the_third_start_is_not_rolled_back() {
        let home = trial_home();
        start_with(home.path(), "0.2.0").unwrap();
        start_with(home.path(), "0.2.0").unwrap();

        mark_healthy(home.path(), "0.2.0");

        assert_eq!(state(home.path()).unwrap().trial, None);
        assert_eq!(start_with(home.path(), "0.2.0").unwrap(), Start::Run);
        assert_eq!(current(home.path()), PathBuf::from("versions/0.2.0"));
        assert_eq!(state(home.path()).unwrap().refused, None);
    }

    #[test]
    fn healthy_for_another_version_keeps_the_trial() {
        let home = trial_home();

        mark_healthy(home.path(), "0.1.0");

        assert!(state(home.path()).unwrap().trial.is_some());
    }

    #[test]
    fn a_start_of_a_version_that_is_not_the_trial_is_not_counted_and_writes_nothing() {
        let home = trial_home();
        let before = fs::read(home.path().join("update.json")).unwrap();

        assert_eq!(start_with(home.path(), "0.1.0").unwrap(), Start::Run);
        assert_eq!(start_with(home.path(), "9.9.9").unwrap(), Start::Run);

        assert_eq!(fs::read(home.path().join("update.json")).unwrap(), before);
    }

    #[test]
    fn a_start_with_no_state_file_runs_and_writes_nothing() {
        let home = tempfile::tempdir().unwrap();

        assert_eq!(start_with(home.path(), "0.1.0").unwrap(), Start::Run);

        assert!(!home.path().join("update.json").exists());
    }

    #[test]
    fn a_rollback_to_a_version_that_is_gone_is_an_error_and_changes_nothing() {
        let home = trial_home();
        fs::remove_dir_all(home.path().join("versions/0.1.0")).unwrap();
        for _ in 0..3 {
            start_with(home.path(), "0.2.0").unwrap();
        }

        let error = start_with(home.path(), "0.2.0").unwrap_err();

        assert!(error.to_string().contains("0.1.0"), "{error}");
        assert_eq!(current(home.path()), PathBuf::from("versions/0.2.0"));
        assert_eq!(state(home.path()).unwrap().refused, None);
    }

    #[test]
    fn the_state_file_is_private() {
        let home = trial_home();

        let mode = fs::metadata(home.path().join("update.json"))
            .unwrap()
            .permissions()
            .mode();

        assert_eq!(mode & 0o777, 0o600);
    }

    fn tags(list: &[&str]) -> Vec<String> {
        list.iter().map(|tag| tag.to_string()).collect()
    }

    #[test]
    fn pick_takes_the_highest_tag_above_the_running_version() {
        let picked = pick(&tags(&["v0.2.0", "v0.3.0", "v0.1.5"]), "0.2.0", None);

        assert_eq!(picked.as_deref(), Some("v0.3.0"));
    }

    #[test]
    fn pick_compares_numbers_not_text() {
        let picked = pick(&tags(&["v0.9.0", "v0.10.0"]), "0.9.0", None);

        assert_eq!(picked.as_deref(), Some("v0.10.0"));
    }

    #[test]
    fn pick_takes_nothing_when_the_highest_is_not_above_running() {
        assert_eq!(pick(&tags(&["v0.1.0", "v0.2.0"]), "0.2.0", None), None);
        assert_eq!(pick(&tags(&["v0.1.0"]), "0.2.0", None), None);
        assert_eq!(pick(&[], "0.2.0", None), None);
    }

    #[test]
    fn pick_skips_a_refused_tag_while_it_is_the_highest() {
        let list = tags(&["v0.2.0", "v0.3.0"]);

        assert_eq!(pick(&list, "0.2.0", Some("v0.3.0")), None);
    }

    #[test]
    fn pick_takes_a_tag_above_the_refused_one() {
        let list = tags(&["v0.3.0", "v0.4.0"]);

        assert_eq!(
            pick(&list, "0.2.0", Some("v0.3.0")).as_deref(),
            Some("v0.4.0")
        );
    }

    #[test]
    fn pick_ignores_tags_that_are_not_three_numbers() {
        let list = tags(&["v1.0", "v1.0.0-rc1", "1.0.0", "vx.y.z", "v1.0.0.1", "latest", ""]);

        assert_eq!(pick(&list, "0.2.0", None), None);
    }

    /// Writes a shell script that answers `--version` with `printed`.
    fn script(path: &Path, printed: &str) {
        fs::write(path, format!("#!/bin/sh\necho \"{printed}\"\n")).unwrap();
    }

    fn installed_home() -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.path().join("versions/0.1.0")).unwrap();
        point_current(home.path(), "0.1.0").unwrap();
        home
    }

    #[test]
    fn install_tag_installs_points_current_and_starts_a_trial() {
        let home = installed_home();
        save(
            home.path(),
            &State {
                trial: None,
                refused: Some("v0.2.0".into()),
            },
        )
        .unwrap();

        install_tag(home.path(), "v0.2.0", "0.1.0", |tag, stage| {
            assert_eq!(tag, "v0.2.0");
            script(&stage.join("foundryd"), "foundryd 0.2.0");
            script(&stage.join("foundry-holder"), "holder");
            Ok(())
        })
        .unwrap();

        assert_eq!(current(home.path()), PathBuf::from("versions/0.2.0"));
        for name in ["foundryd", "foundry-holder"] {
            let mode = fs::metadata(home.path().join("versions/0.2.0").join(name))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o755, 0o755, "{name}");
        }
        let state = state(home.path()).unwrap();
        assert_eq!(
            state.trial,
            Some(Trial {
                version: "0.2.0".into(),
                previous: "0.1.0".into(),
                starts: 0
            })
        );
        assert_eq!(state.refused, None);
        assert!(!home.path().join("versions/.staging-0.2.0").exists());
    }

    #[test]
    fn install_tag_refuses_a_binary_that_prints_another_version() {
        let home = installed_home();

        let error = install_tag(home.path(), "v0.2.0", "0.1.0", |_, stage| {
            script(&stage.join("foundryd"), "foundryd 0.9.9");
            script(&stage.join("foundry-holder"), "holder");
            Ok(())
        })
        .unwrap_err();

        assert!(error.to_string().contains("foundryd 0.9.9"), "{error}");
        assert_eq!(current(home.path()), PathBuf::from("versions/0.1.0"));
        assert!(!home.path().join("versions/0.2.0").exists());
        assert_eq!(state(home.path()).unwrap(), State::default());
    }

    #[test]
    fn install_tag_refuses_an_asset_without_the_holder() {
        let home = installed_home();

        let error = install_tag(home.path(), "v0.2.0", "0.1.0", |_, stage| {
            script(&stage.join("foundryd"), "foundryd 0.2.0");
            Ok(())
        })
        .unwrap_err();

        assert!(error.to_string().contains("foundry-holder"), "{error}");
        assert_eq!(current(home.path()), PathBuf::from("versions/0.1.0"));
    }

    #[test]
    fn install_tag_passes_a_failed_fetch_on_and_changes_nothing() {
        let home = installed_home();

        let error = install_tag(home.path(), "v0.2.0", "0.1.0", |_, _| {
            Err(UpdateError::Fetch("gh is not logged in".into()))
        })
        .unwrap_err();

        assert!(error.to_string().contains("gh is not logged in"), "{error}");
        assert_eq!(current(home.path()), PathBuf::from("versions/0.1.0"));
    }

    #[test]
    fn installed_is_true_only_when_current_points_at_the_running_version() {
        let home = installed_home();

        assert!(installed(home.path(), "0.1.0"));
        assert!(!installed(home.path(), "0.2.0"));
        assert!(!installed(tempfile::tempdir().unwrap().path(), "0.1.0"));
        let other = tempfile::tempdir().unwrap();
        symlink("elsewhere", other.path().join("current")).unwrap();
        assert!(!installed(other.path(), "0.1.0"));
    }
}
