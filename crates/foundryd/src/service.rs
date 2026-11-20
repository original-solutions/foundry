
#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn program() -> PathBuf {
        PathBuf::from("/data/foundry/current/foundryd")
    }

    #[test]
    fn the_plist_spares_child_processes_and_keeps_the_daemon_alive() {
        let text = plist(&program(), &env(&[]), Path::new("/logs/foundryd.log"));

        assert!(
            text.contains("<key>AbandonProcessGroup</key>\n\t<true/>"),
            "{text}"
        );
        assert!(text.contains("<key>KeepAlive</key>\n\t<true/>"), "{text}");
        assert!(text.contains("<key>RunAtLoad</key>\n\t<true/>"), "{text}");
        assert!(text.contains("<string>/data/foundry/current/foundryd</string>"));
        assert!(text.contains("<string>run</string>"));
        assert!(text.contains(LABEL));
        assert!(text.contains("<string>/logs/foundryd.log</string>"));
    }

    #[test]
    fn the_plist_carries_the_gateway_url_only_when_it_is_given() {
        let with = plist(
            &program(),
            &env(&[("FOUNDRY_GATEWAY_URL", "https://gw.example")]),
            Path::new("/l"),
        );
        let without = plist(&program(), &env(&[("FOUNDRY_HOME", "/h")]), Path::new("/l"));

        assert!(with.contains("<key>FOUNDRY_GATEWAY_URL</key>"));
        assert!(with.contains("<string>https://gw.example</string>"));
        assert!(!without.contains("FOUNDRY_GATEWAY_URL"));
        assert!(without.contains("<key>FOUNDRY_HOME</key>"));
    }

    #[test]
    fn the_plist_escapes_xml() {
        let text = plist(
            &program(),
            &env(&[("PATH", "/a&b:<c>\"'")]),
            Path::new("/l"),
        );

        assert!(text.contains("/a&amp;b:&lt;c&gt;&quot;&apos;"), "{text}");
    }

    #[test]
    fn the_unit_spares_child_processes_and_runs_as_the_user() {
        let text = unit(&program(), &env(&[]), "foundry").unwrap();

        assert!(text.contains("\nKillMode=process\n"), "{text}");
        assert!(text.contains("\nUser=foundry\n"), "{text}");
        assert!(text.contains("\nRestart=always\n"), "{text}");
        assert!(text.contains("\nExecStart=\"/data/foundry/current/foundryd\" run\n"));
        assert!(text.contains("WantedBy=multi-user.target"));
    }

    #[test]
    fn the_unit_carries_the_gateway_url_only_when_it_is_given() {
        let with = unit(
            &program(),
            &env(&[("FOUNDRY_GATEWAY_URL", "https://gw.example")]),
            "foundry",
        )
        .unwrap();
        let without = unit(&program(), &env(&[("FOUNDRY_HOME", "/h")]), "foundry").unwrap();

        assert!(with.contains("\nEnvironment=\"FOUNDRY_GATEWAY_URL=https://gw.example\"\n"));
        assert!(!without.contains("FOUNDRY_GATEWAY_URL"));
        assert!(without.contains("Environment=\"FOUNDRY_HOME=/h\""));
    }

    #[test]
    fn the_unit_quotes_for_systemd() {
        let text = unit(
            &PathBuf::from("/d/100%/a\"b\\c/foundryd"),
            &env(&[("PATH", "/x%y\"z")]),
            "foundry",
        )
        .unwrap();

        assert!(
            text.contains("ExecStart=\"/d/100%%/a\\\"b\\\\c/foundryd\" run"),
            "{text}"
        );
        assert!(text.contains("Environment=\"PATH=/x%%y\\\"z\""), "{text}");
    }

    #[test]
    fn the_unit_refuses_root() {
        let error = unit(&program(), &env(&[]), "root").unwrap_err();

        assert!(error.to_string().contains("root"), "{error}");
    }

    #[test]
    fn the_environment_always_has_home_and_takes_the_listed_variables_when_set() {
        let lookup = |name: &str| match name {
            "PATH" => Some("/usr/bin".to_string()),
            "FOUNDRY_GATEWAY_URL" => Some("https://gw".to_string()),
            "FOUNDRY_PORT" => Some(String::new()),
            "HOME" => Some("/ignored".to_string()),
            _ => None,
        };

        let vars = environment(lookup, Path::new("/data"));

        assert_eq!(
            vars,
            env(&[
                ("FOUNDRY_HOME", "/data"),
                ("PATH", "/usr/bin"),
                ("FOUNDRY_GATEWAY_URL", "https://gw"),
            ])
        );
    }

    #[test]
    fn place_copies_both_binaries_executable_and_points_current() {
        let home = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        fs::write(src.path().join("foundryd"), "daemon").unwrap();
        fs::write(src.path().join("foundry-holder"), "holder").unwrap();

        place(home.path(), &src.path().join("foundryd"), "0.1.0").unwrap();

        let dir = home.path().join("versions/0.1.0");
        assert_eq!(fs::read_to_string(dir.join("foundryd")).unwrap(), "daemon");
        assert_eq!(
            fs::read_to_string(dir.join("foundry-holder")).unwrap(),
            "holder"
        );
        for name in ["foundryd", "foundry-holder"] {
            let mode = fs::metadata(dir.join(name)).unwrap().permissions().mode();
            assert_eq!(mode & 0o755, 0o755, "{name}");
        }
        assert_eq!(
            fs::read_link(home.path().join("current")).unwrap(),
            PathBuf::from("versions/0.1.0")
        );
    }

    #[test]
    fn place_names_a_missing_holder_and_changes_nothing() {
        let home = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        fs::write(src.path().join("foundryd"), "daemon").unwrap();

        let error = place(home.path(), &src.path().join("foundryd"), "0.1.0").unwrap_err();

        assert!(error.to_string().contains("foundry-holder"), "{error}");
        assert!(!home.path().join("current").exists());
    }

    #[test]
    fn place_from_inside_the_version_directory_copies_nothing() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("versions/0.1.0");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("foundryd"), "daemon").unwrap();
        fs::write(dir.join("foundry-holder"), "holder").unwrap();

        place(home.path(), &dir.join("foundryd"), "0.1.0").unwrap();

        assert_eq!(fs::read_to_string(dir.join("foundryd")).unwrap(), "daemon");
        assert_eq!(
            fs::read_link(home.path().join("current")).unwrap(),
            PathBuf::from("versions/0.1.0")
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_plist_passes_plutil() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.plist");
        let text = plist(
            &program(),
            &env(&[("PATH", "/a&b"), ("FOUNDRY_HOME", "/h")]),
            Path::new("/logs/f.log"),
        );
        fs::write(&file, text).unwrap();

        let out = std::process::Command::new("plutil")
            .arg("-lint")
            .arg(&file)
            .output()
            .unwrap();

        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
    }
}
