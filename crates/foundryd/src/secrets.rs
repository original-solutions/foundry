//! Secrets in the macOS keychain or a 0600 file on Linux. Filled by plan task 13.

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn file() -> (tempfile::TempDir, FileSecrets) {
        let dir = tempfile::tempdir().unwrap();
        let secrets = FileSecrets::new(dir.path().join("secrets.json"));
        (dir, secrets)
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn a_secret_set_is_got_back() {
        let (_dir, secrets) = file();
        assert_eq!(secrets.get("gateway").unwrap(), None);
        secrets.set("gateway", "sk-one").unwrap();
        secrets.set("management", "sk-two").unwrap();
        secrets.set("gateway", "sk-three").unwrap();
        assert_eq!(secrets.get("gateway").unwrap().as_deref(), Some("sk-three"));
        assert_eq!(secrets.get("management").unwrap().as_deref(), Some("sk-two"));
    }

    #[test]
    fn the_file_is_created_0600() {
        let (dir, secrets) = file();
        secrets.set("gateway", "sk-one").unwrap();
        assert_eq!(mode(&dir.path().join("secrets.json")), 0o600);
    }

    #[test]
    fn the_file_is_created_0600_whatever_mode_a_leftover_temporary_file_had() {
        let (dir, secrets) = file();
        let leftover = dir.path().join("secrets.json.tmp");
        fs::write(&leftover, "{}").unwrap();
        fs::set_permissions(&leftover, fs::Permissions::from_mode(0o644)).unwrap();
        secrets.set("gateway", "sk-one").unwrap();
        assert_eq!(mode(&dir.path().join("secrets.json")), 0o600);
    }

    #[test]
    fn a_file_others_can_read_or_write_is_refused_at_read() {
        for wider in [0o640, 0o604, 0o620, 0o602, 0o644] {
            let (dir, secrets) = file();
            secrets.set("gateway", "sk-one").unwrap();
            let path = dir.path().join("secrets.json");
            fs::set_permissions(&path, fs::Permissions::from_mode(wider)).unwrap();
            let error = secrets.get("gateway").unwrap_err();
            assert!(
                matches!(error, SecretsError::TooOpen { .. }),
                "{wider:o}: {error}"
            );
            assert!(error.to_string().contains("0600"), "{error}");
            assert!(secrets.set("other", "x").is_err(), "{wider:o}: set");
        }
    }

    #[test]
    fn an_unreadable_file_names_no_secret_in_its_error() {
        let (dir, secrets) = file();
        secrets.set("gateway", "sk-one").unwrap();
        let path = dir.path().join("secrets.json");
        fs::write(&path, r#"{"gateway": ["sk-leaked"]}"#).unwrap();
        let error = secrets.get("gateway").unwrap_err();
        assert!(!error.to_string().contains("sk-leaked"), "{error}");
        assert!(!format!("{error:?}").contains("sk-leaked"), "{error:?}");
    }

    #[test]
    fn the_in_memory_fake_keeps_what_is_set() {
        let secrets = MemorySecrets::default();
        assert_eq!(secrets.get("gateway").unwrap(), None);
        secrets.set("gateway", "sk-one").unwrap();
        assert_eq!(secrets.get("gateway").unwrap().as_deref(), Some("sk-one"));
    }

    /// Touches the real keychain, which may ask a person to allow it: a background service
    /// cannot answer that, so this runs only by hand.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore]
    fn the_keychain_keeps_what_is_set() {
        let secrets = Keychain::new("foundry-test");
        let name = format!("probe-{}", uuid::Uuid::new_v4());
        assert_eq!(secrets.get(&name).unwrap(), None);
        secrets.set(&name, "sk-one").unwrap();
        secrets.set(&name, "sk-two").unwrap();
        assert_eq!(secrets.get(&name).unwrap().as_deref(), Some("sk-two"));
        security_framework::passwords::delete_generic_password("foundry-test", &name).unwrap();
    }
}
