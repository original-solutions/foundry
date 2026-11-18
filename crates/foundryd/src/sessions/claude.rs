#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    use serde_json::{Value, json};

    use super::*;

    const URL: &str = "http://127.0.0.1:4100/hooks/67e55044-10b1-426f-9247-bb680e5fe0c8";
    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn read(path: &Path) -> Value {
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    fn command(settings: &Value, event: &str) -> String {
        settings["hooks"][event][0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn settings_register_the_seven_events_each_with_one_command() {
        let settings = settings_json(URL, TOKEN);
        let hooks = settings["hooks"].as_object().unwrap();
        let mut names: Vec<&str> = hooks.keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "Notification",
                "PostToolUse",
                "PreToolUse",
                "SessionEnd",
                "SessionStart",
                "Stop",
                "UserPromptSubmit"
            ]
        );
        for (event, entries) in hooks {
            let entries = entries.as_array().unwrap();
            assert_eq!(entries.len(), 1, "{event}");
            let commands = entries[0]["hooks"].as_array().unwrap();
            assert_eq!(commands.len(), 1, "{event}");
            assert_eq!(commands[0]["type"], "command");
            match event.as_str() {
                "PreToolUse" | "PostToolUse" => assert_eq!(entries[0]["matcher"], "*"),
                _ => assert!(entries[0].get("matcher").is_none(), "{event}"),
            }
        }
    }

    #[test]
    fn the_hook_command_posts_stdin_quietly_and_never_blocks_the_agent() {
        let settings = settings_json(URL, TOKEN);
        let want = format!(
            "curl -sS -m 5 -X POST -H 'Content-Type: application/json' \
             -H 'Authorization: Bearer {TOKEN}' --data-binary @- '{URL}' >/dev/null 2>&1 || true"
        );
        for event in ["SessionStart", "PreToolUse", "SessionEnd"] {
            assert_eq!(command(&settings, event), want);
        }
    }

    #[test]
    fn settings_are_written_new_with_mode_0600_in_a_0700_folder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions").join("s.json");
        let value = settings_json(URL, TOKEN);

        write_settings(&path, &value).unwrap();

        assert_eq!(read(&path), value);
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert!(write_settings(&path, &value).is_err(), "a second write");
    }

    #[test]
    fn a_missing_claude_json_is_created_with_mode_0600_and_just_the_trust() {
        let dir = tempfile::tempdir().unwrap();
        let worktree = tempfile::tempdir().unwrap();
        let file = dir.path().join(".claude.json");

        mark_trusted(&file, worktree.path()).unwrap();

        let key = std::fs::canonicalize(worktree.path()).unwrap();
        assert_eq!(
            read(&file),
            json!({"projects": {key.to_str().unwrap(): {"hasTrustDialogAccepted": true}}})
        );
        assert_eq!(mode(&file), 0o600);
    }

    #[test]
    fn other_keys_and_the_entrys_other_fields_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let worktree = tempfile::tempdir().unwrap();
        let key = std::fs::canonicalize(worktree.path()).unwrap();
        let key = key.to_str().unwrap();
        let file = dir.path().join(".claude.json");
        let before = json!({
            "oauthAccount": {"emailAddress": "a@b.c"},
            "projects": {
                key: {"allowedTools": ["Bash"], "hasTrustDialogAccepted": false},
                "/other": {"hasTrustDialogAccepted": false}
            }
        });
        std::fs::write(&file, before.to_string()).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o640)).unwrap();

        mark_trusted(&file, worktree.path()).unwrap();

        let mut want = before.clone();
        want["projects"][key]["hasTrustDialogAccepted"] = json!(true);
        assert_eq!(read(&file), want);
        assert_eq!(mode(&file), 0o640, "the file's mode is kept");
        let left: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(left.len(), 1, "no temporary file is left behind");
    }

    #[test]
    fn a_new_project_entry_is_added_beside_the_others() {
        let dir = tempfile::tempdir().unwrap();
        let worktree = tempfile::tempdir().unwrap();
        let file = dir.path().join(".claude.json");
        std::fs::write(&file, json!({"projects": {"/other": {"x": 1}}}).to_string()).unwrap();

        mark_trusted(&file, worktree.path()).unwrap();

        let key = std::fs::canonicalize(worktree.path()).unwrap();
        let got = read(&file);
        assert_eq!(got["projects"]["/other"], json!({"x": 1}));
        assert_eq!(
            got["projects"][key.to_str().unwrap()]["hasTrustDialogAccepted"],
            true
        );
    }

    #[test]
    fn an_already_trusted_file_is_not_written_at_all() {
        let dir = tempfile::tempdir().unwrap();
        let worktree = tempfile::tempdir().unwrap();
        let key = std::fs::canonicalize(worktree.path()).unwrap();
        let file = dir.path().join(".claude.json");
        // Odd spacing and key order a rewrite would change.
        let text = format!(
            "{{ \"z\": 1,\n \"projects\": {{ {:?}: {{\"hasTrustDialogAccepted\":true}} }} }}",
            key.to_str().unwrap()
        );
        std::fs::write(&file, &text).unwrap();
        let before = std::fs::metadata(&file).unwrap().modified().unwrap();

        mark_trusted(&file, worktree.path()).unwrap();

        assert_eq!(std::fs::read_to_string(&file).unwrap(), text);
        assert_eq!(std::fs::metadata(&file).unwrap().modified().unwrap(), before);
    }

    #[test]
    fn a_file_that_is_not_json_is_an_error_that_leaves_it_alone_and_does_not_quote_it() {
        let dir = tempfile::tempdir().unwrap();
        let worktree = tempfile::tempdir().unwrap();
        let file = dir.path().join(".claude.json");
        let text = r#"{"oauthToken": "SECRET-VALUE" nope"#;
        std::fs::write(&file, text).unwrap();

        let error = mark_trusted(&file, worktree.path()).unwrap_err();

        assert_eq!(std::fs::read_to_string(&file).unwrap(), text);
        let shown = format!("{error} {error:?}");
        assert!(!shown.contains("SECRET-VALUE"), "{shown}");
        assert!(shown.contains(".claude.json"), "{shown}");
    }

    #[test]
    fn projects_or_the_entry_that_is_not_an_object_is_an_error_that_leaves_the_file_alone() {
        let worktree = tempfile::tempdir().unwrap();
        let key = std::fs::canonicalize(worktree.path()).unwrap();
        for text in [
            json!([1]).to_string(),
            json!({"projects": []}).to_string(),
            json!({"projects": {key.to_str().unwrap(): "yes"}}).to_string(),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join(".claude.json");
            std::fs::write(&file, &text).unwrap();

            assert!(mark_trusted(&file, worktree.path()).is_err(), "{text}");
            assert_eq!(std::fs::read_to_string(&file).unwrap(), text);
        }
    }

    #[test]
    fn a_worktree_given_through_a_symlink_is_trusted_under_its_canonical_path() {
        let dir = tempfile::tempdir().unwrap();
        let worktree = tempfile::tempdir().unwrap();
        let link = dir.path().join("link");
        symlink(worktree.path(), &link).unwrap();
        let file = dir.path().join(".claude.json");

        mark_trusted(&file, &link).unwrap();

        let key = std::fs::canonicalize(worktree.path()).unwrap();
        let got = read(&file);
        let projects = got["projects"].as_object().unwrap();
        assert_eq!(projects.keys().collect::<Vec<_>>(), [key.to_str().unwrap()]);
    }

    #[test]
    fn a_symlinked_claude_json_has_its_target_updated_and_stays_a_link() {
        let dir = tempfile::tempdir().unwrap();
        let worktree = tempfile::tempdir().unwrap();
        let target = dir.path().join("real.json");
        std::fs::write(&target, json!({"keep": true}).to_string()).unwrap();
        let link = dir.path().join(".claude.json");
        symlink(&target, &link).unwrap();

        mark_trusted(&link, worktree.path()).unwrap();

        assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
        let key = std::fs::canonicalize(worktree.path()).unwrap();
        let got = read(&target);
        assert_eq!(got["keep"], true);
        assert_eq!(
            got["projects"][key.to_str().unwrap()]["hasTrustDialogAccepted"],
            true
        );
    }

    #[test]
    fn claude_json_is_in_the_config_dir_when_set_else_in_home_else_nowhere() {
        let os = |s: &str| Some(OsString::from(s));
        assert_eq!(
            claude_json(os("/cfg"), os("/home/me")),
            Some(PathBuf::from("/cfg/.claude.json"))
        );
        assert_eq!(
            claude_json(os(""), os("/home/me")),
            Some(PathBuf::from("/home/me/.claude.json"))
        );
        assert_eq!(
            claude_json(None, os("/home/me")),
            Some(PathBuf::from("/home/me/.claude.json"))
        );
        assert_eq!(claude_json(None, None), None);
        assert_eq!(claude_json(os(""), os("")), None);
    }
}
