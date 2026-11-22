//! What Tailscale says about this machine.

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::ffi::OsString;
    use std::net::IpAddr;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use super::*;

    fn os(text: &str) -> Option<OsString> {
        Some(text.into())
    }

    fn ips<const N: usize>(texts: [&str; N]) -> BTreeSet<IpAddr> {
        texts
            .into_iter()
            .map(|text| text.parse().unwrap())
            .collect()
    }

    /// `tailscale status --self --json` from a real Mac, cut to the fields read and a few
    /// beside them.
    const STATUS: &str = r#"{
        "Version": "1.90.6",
        "BackendState": "Running",
        "Self": {
            "ID": "n1",
            "HostName": "Personal Mac",
            "DNSName": "personal-mac.tail459612.ts.net.",
            "OS": "macOS",
            "TailscaleIPs": ["100.91.54.18", "fd7a:115c:a1e0::e832:3612"],
            "Online": true
        },
        "MagicDNSSuffix": "tail459612.ts.net"
    }"#;

    #[test]
    fn the_name_is_the_first_label_of_the_dns_name_and_the_address_the_first_ipv4() {
        assert_eq!(
            entry(STATUS.as_bytes(), Some("personal-mac-host")),
            Ok(Entry {
                name: "personal-mac".into(),
                address: "100.91.54.18".into(),
            })
        );
    }

    #[test]
    fn the_ipv4_is_taken_wherever_it_sits_among_the_addresses() {
        let status = STATUS.replace(
            r#"["100.91.54.18", "fd7a:115c:a1e0::e832:3612"]"#,
            r#"["fd7a:115c:a1e0::e832:3612", "100.91.54.18"]"#,
        );
        assert_eq!(
            entry(status.as_bytes(), None).map(|e| e.address),
            Ok("100.91.54.18".to_owned())
        );
    }

    #[test]
    fn with_no_dns_name_the_host_name_is_the_name() {
        for blank in [r#""DNSName": "","#, ""] {
            let status = STATUS.replace(r#""DNSName": "personal-mac.tail459612.ts.net.","#, blank);
            assert_eq!(
                entry(status.as_bytes(), Some("personal-mac-host")),
                Ok(Entry {
                    name: "personal-mac-host".into(),
                    address: "100.91.54.18".into(),
                }),
                "{blank:?}"
            );
        }
    }

    #[test]
    fn with_no_dns_name_and_no_host_name_there_is_no_entry() {
        let status = STATUS.replace(r#""DNSName": "personal-mac.tail459612.ts.net.","#, "");
        assert!(entry(status.as_bytes(), None).is_err());
    }

    #[test]
    fn the_name_is_the_whole_first_label_however_long() {
        let label = "m".repeat(63);
        let status = STATUS.replace("personal-mac.tail", &format!("{label}.tail"));
        assert_eq!(entry(status.as_bytes(), None).map(|e| e.name), Ok(label));
    }

    #[test]
    fn with_no_ipv4_there_is_no_entry() {
        let status = STATUS.replace(r#""100.91.54.18", "#, "");
        let why = entry(status.as_bytes(), Some("host")).unwrap_err();
        assert!(why.contains("IPv4"), "{why}");
    }

    #[test]
    fn an_answer_that_is_not_status_json_is_no_entry() {
        for answer in ["", "Tailscale is stopped.", r#"{"Version": "1.90.6"}"#] {
            assert!(
                entry(answer.as_bytes(), Some("host")).is_err(),
                "{answer:?}"
            );
        }
    }

    #[test]
    fn foundry_tailscale_names_the_cli_when_set() {
        let dir = tempfile::tempdir().unwrap();
        let known = dir.path().join("known");
        std::fs::write(&known, "").unwrap();
        let known = known.to_str().unwrap();

        assert_eq!(
            choose_cli(os("/opt/fake/tailscale"), true, &[known]),
            PathBuf::from("/opt/fake/tailscale")
        );
    }

    /// A release build ignores `FOUNDRY_TAILSCALE`: whatever the variable names, the daemon asks
    /// the CLI where Tailscale puts it.
    #[test]
    fn foundry_tailscale_is_ignored_when_the_build_does_not_take_it() {
        let dir = tempfile::tempdir().unwrap();
        let known = dir.path().join("known");
        std::fs::write(&known, "").unwrap();

        assert_eq!(
            choose_cli(os("/opt/fake/tailscale"), false, &[known.to_str().unwrap()]),
            known
        );
    }

    /// The debug build the tests run takes `FOUNDRY_TAILSCALE`.
    #[cfg(debug_assertions)]
    #[test]
    fn this_debug_build_takes_foundry_tailscale() {
        assert_eq!(
            cli_for(os("/opt/fake/tailscale")),
            PathBuf::from("/opt/fake/tailscale")
        );
    }

    /// A release build does not. CI runs this one in a release build.
    #[cfg(not(debug_assertions))]
    #[test]
    fn this_release_build_ignores_foundry_tailscale() {
        assert_eq!(cli_for(os("/opt/fake/tailscale")), cli_for(None));
    }

    #[test]
    fn without_foundry_tailscale_the_first_known_cli_that_is_there_is_used() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        let there = dir.path().join("there");
        let also = dir.path().join("also");
        std::fs::write(&there, "").unwrap();
        std::fs::write(&also, "").unwrap();
        let known = [
            missing.to_str().unwrap(),
            there.to_str().unwrap(),
            also.to_str().unwrap(),
        ];

        assert_eq!(choose_cli(None, true, &known), there);
        assert_eq!(choose_cli(os(""), true, &known), there);
    }

    #[test]
    fn with_no_known_cli_there_tailscale_on_path_is_used() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");

        assert_eq!(
            choose_cli(None, true, &[missing.to_str().unwrap()]),
            PathBuf::from("tailscale")
        );
    }

    /// A service's `PATH` under launchd holds neither the app's CLI nor Homebrew's.
    #[test]
    fn on_macos_the_app_cli_comes_first_then_homebrew_s() {
        let expected: &[&str] = if cfg!(target_os = "macos") {
            &[
                "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
                "/opt/homebrew/bin/tailscale",
                "/usr/local/bin/tailscale",
            ]
        } else {
            &[]
        };
        assert_eq!(KNOWN_CLIS, expected);
    }

    /// Writes an executable stand-in for the Tailscale CLI that runs `body`.
    fn fake_cli(dir: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let cli = dir.join("tailscale");
        std::fs::write(&cli, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        cli
    }

    #[tokio::test]
    async fn the_cli_is_asked_for_ip_and_its_addresses_are_read() {
        let dir = tempfile::tempdir().unwrap();
        let cli = fake_cli(
            dir.path(),
            r#"[ "$1" = ip ] || exit 9; printf '100.101.102.103\nfd7a:115c:a1e0::1\n'"#,
        );

        let reported = ask_cli(&cli, Duration::from_secs(10)).await;

        assert_eq!(reported, Ok(ips(["100.101.102.103", "fd7a:115c:a1e0::1"])));
    }

    /// The CLI inside Tailscale.app starts the app's GUI instead of answering when `TERM` is not
    /// set, as under launchd. The test's own `TERM` is whatever the shell gave it, so the stand-in
    /// answers only to the value the daemon sets.
    #[tokio::test]
    async fn the_cli_is_given_a_term_so_the_app_cli_answers_under_launchd() {
        let dir = tempfile::tempdir().unwrap();
        let cli = fake_cli(
            dir.path(),
            r#"[ "$TERM" = dumb ] || { echo 'The Tailscale GUI failed to start' >&2; exit 1; }; echo 100.101.102.103"#,
        );

        let reported = ask_cli(&cli, Duration::from_secs(10)).await;

        assert_eq!(reported, Ok(ips(["100.101.102.103"])));
    }

    #[tokio::test]
    async fn a_cli_that_fails_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let cli = fake_cli(dir.path(), "echo 'Tailscale is stopped.' >&2; exit 1");

        let why = ask_cli(&cli, Duration::from_secs(10)).await.unwrap_err();

        assert!(why.contains("Tailscale is stopped."), "{why}");
    }

    #[tokio::test]
    async fn a_missing_cli_says_which_one_could_not_be_run() {
        let dir = tempfile::tempdir().unwrap();
        let cli = dir.path().join("tailscale");

        let why = ask_cli(&cli, Duration::from_secs(10)).await.unwrap_err();

        assert!(why.contains(&cli.display().to_string()), "{why}");
    }

    #[tokio::test]
    async fn output_that_is_not_addresses_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let cli = fake_cli(dir.path(), "echo 'not an address'");

        let why = ask_cli(&cli, Duration::from_secs(10)).await.unwrap_err();

        assert!(why.contains("not an address"), "{why}");
    }

    #[tokio::test]
    async fn a_cli_that_hangs_is_stopped_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        // A distinctive duration, so a stray is easy to find.
        let cli = fake_cli(dir.path(), "exec sleep 7.317");

        let why = ask_cli(&cli, Duration::from_millis(200)).await.unwrap_err();

        assert!(why.contains("did not answer"), "{why}");
    }
}
