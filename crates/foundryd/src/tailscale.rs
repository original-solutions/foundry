//! What Tailscale says about this machine: the addresses it reports, from `tailscale ip`, and
//! the entry the machine lists itself under ([`Entry`]), from `tailscale status --self --json`.
//! [`own_entry`] is the one source of that entry, for the daemon's listing and for
//! `foundryd join`. The machine's own host name ([`host_names`]) is read from the system, not
//! from Tailscale: the entry falls back to it when Tailscale gives no DNS name, and the API
//! trusts it in a `Host` header.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

/// What Tailscale says this machine is called and where it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Its default name: the first label of its MagicDNS name.
    pub name: String,
    /// Its Tailscale IPv4 address.
    pub address: String,
}

/// Reads `tailscale status --self --json`: the first label of `Self.DNSName` as the name
/// (`personal-mac.tail459612.ts.net.` gives `personal-mac`), else `host`, and the first IPv4
/// in `Self.TailscaleIPs` as the address.
/// `Self.HostName` is a display name with spaces, so it is not used.
pub fn entry(status: &[u8], host: Option<&str>) -> Result<Entry, String> {
    #[derive(Deserialize)]
    struct Status {
        #[serde(rename = "Self")]
        me: Node,
    }
    #[derive(Deserialize)]
    struct Node {
        #[serde(rename = "DNSName", default)]
        dns_name: String,
        #[serde(rename = "TailscaleIPs", default)]
        ips: Vec<String>,
    }
    let status: Status = serde_json::from_slice(status).map_err(|e| {
        format!("`tailscale status --self --json` did not say what this machine is: {e}")
    })?;
    let name = status
        .me
        .dns_name
        .split('.')
        .next()
        .filter(|label| !label.is_empty())
        .or(host.filter(|host| !host.is_empty()))
        .ok_or("Tailscale gives this machine no DNS name and it has no host name")?;
    let address = status
        .me
        .ips
        .iter()
        .find(|ip| ip.parse::<Ipv4Addr>().is_ok())
        .ok_or("Tailscale reports no IPv4 address for this machine")?;
    Ok(Entry {
        name: name.to_owned(),
        address: address.clone(),
    })
}

/// This machine's short host name, lowercased: the API trusts it in a `Host` header as its
/// MagicDNS name.
pub(crate) fn host_names() -> Vec<String> {
    sysinfo::System::host_name()
        .ok()
        .and_then(|name| name.split('.').next().map(str::to_ascii_lowercase))
        .filter(|name| !name.is_empty())
        .into_iter()
        .collect()
}

/// The Tailscale CLI, by [`choose_cli`]. In a debug build `FOUNDRY_TAILSCALE` names it, which
/// is how tests keep the daemon away from the real one. A release build ignores it, so what
/// the daemon listens on always comes from Tailscale itself.
fn tailscale_cli() -> PathBuf {
    cli_for(std::env::var_os("FOUNDRY_TAILSCALE"))
}

/// The CLI this build asks for a `FOUNDRY_TAILSCALE` value.
fn cli_for(set: Option<OsString>) -> PathBuf {
    choose_cli(set, TAILSCALE_OVERRIDE, KNOWN_CLIS)
}

/// Whether this build takes `FOUNDRY_TAILSCALE`: only a debug build.
const TAILSCALE_OVERRIDE: bool = cfg!(debug_assertions);

/// Where the Tailscale CLI is when it is not on a service's `PATH`: on macOS inside the app,
/// which does not put it on `PATH`, then Homebrew's, which launchd's `PATH` leaves out. On
/// Linux it is on `PATH`.
const KNOWN_CLIS: &[&str] = if cfg!(target_os = "macos") {
    &[
        "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
        "/opt/homebrew/bin/tailscale",
        "/usr/local/bin/tailscale",
    ]
} else {
    &[]
};

/// The CLI to ask: `set` when `taken` (the build takes the override) and it is not empty,
/// else the first of `known` that is there, else `tailscale` on `PATH`.
fn choose_cli(set: Option<OsString>, taken: bool, known: &[&str]) -> PathBuf {
    if let Some(cli) = set.filter(|cli| taken && !cli.is_empty()) {
        return PathBuf::from(cli);
    }
    known
        .iter()
        .map(Path::new)
        .find(|cli| cli.exists())
        .map_or_else(|| PathBuf::from("tailscale"), Path::to_path_buf)
}

/// How long a `tailscale` ask may take before it is stopped. An ask that runs past it is
/// [`AskError::TimedOut`]. The daemon's watch keeps the served addresses through the first two
/// timeouts in a row and stops them at the third (`TIMEOUTS_BEFORE_AWAY` in daemon.rs); any
/// other failure it acts on at once.
const ASK_WITHIN: Duration = Duration::from_secs(3);

/// Why an ask of the Tailscale CLI got no answer. A timeout is told apart from every other
/// failure by its kind, not its message: a slow CLI says nothing about whether Tailscale is up,
/// where a CLI that fails, is missing or prints nonsense is a definite answer.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum AskError {
    /// The CLI did not answer within its time.
    #[error("{0}")]
    TimedOut(String),
    /// The CLI exited non-zero, could not be run, or printed what was not expected.
    #[error("{0}")]
    Failed(String),
}

/// The addresses Tailscale reports for this machine, or why it cannot say.
pub(crate) async fn tailscale_ips() -> Result<BTreeSet<IpAddr>, AskError> {
    ask_cli(&tailscale_cli(), ASK_WITHIN).await
}

/// Runs `<cli> ip`, which prints one of this machine's Tailscale addresses a line, and reads
/// them. A CLI that fails, is missing or prints anything else gives why as
/// [`AskError::Failed`]; one that runs past `within` as [`AskError::TimedOut`].
async fn ask_cli(cli: &Path, within: Duration) -> Result<BTreeSet<IpAddr>, AskError> {
    String::from_utf8_lossy(&run_cli(cli, &["ip"], within).await?)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            line.parse().map_err(|_| {
                AskError::Failed(format!(
                    "`{} ip` printed {line:?}, not an address",
                    cli.display()
                ))
            })
        })
        .collect()
}

/// Runs `<cli> status --self --json`, which says what Tailscale calls this machine and its
/// addresses, and gives what it printed, or why it cannot, as [`run_cli`] does.
async fn ask_status(cli: &Path, within: Duration) -> Result<Vec<u8>, AskError> {
    run_cli(cli, &["status", "--self", "--json"], within).await
}

/// Runs `<cli> <args>` and gives what it printed. A CLI that fails or is missing gives why as
/// [`AskError::Failed`]; one that runs past `within` is stopped and gives [`AskError::TimedOut`].
async fn run_cli(cli: &Path, args: &[&str], within: Duration) -> Result<Vec<u8>, AskError> {
    let asked = format!("{} {}", cli.display(), args.join(" "));
    let run = tokio::process::Command::new(cli)
        .args(args)
        // The CLI inside Tailscale.app answers only when `TERM` is set; without it, as under
        // launchd, it tries to start the app's GUI and fails.
        .env("TERM", "dumb")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let out = match tokio::time::timeout(within, run).await {
        Ok(Ok(out)) => out,
        Ok(Err(error)) => {
            return Err(AskError::Failed(format!(
                "{} could not be run: {error}",
                cli.display()
            )));
        }
        Err(_) => {
            return Err(AskError::TimedOut(format!(
                "`{asked}` did not answer within {} s",
                within.as_secs_f32()
            )));
        }
    };
    if !out.status.success() {
        return Err(AskError::Failed(format!(
            "`{asked}` failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(out.stdout)
}

/// What this machine lists itself under: what `tailscale status --self --json` says it is
/// called and its address, read by [`entry`]. The one source of the entry
/// the daemon lists and the one `foundryd join` sends, so the two never differ.
pub(crate) async fn own_entry() -> Result<Entry, String> {
    let status = ask_status(&tailscale_cli(), ASK_WITHIN)
        .await
        .map_err(|error| error.to_string())?;
    entry(&status, host_names().first().map(String::as_str))
}

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

    /// A release build does not. CI runs this one with debug assertions off, as in a release build.
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

        assert!(matches!(why, AskError::Failed(_)), "{why:?}");
        assert!(why.to_string().contains("Tailscale is stopped."), "{why}");
    }

    #[tokio::test]
    async fn a_missing_cli_says_which_one_could_not_be_run() {
        let dir = tempfile::tempdir().unwrap();
        let cli = dir.path().join("tailscale");

        let why = ask_cli(&cli, Duration::from_secs(10)).await.unwrap_err();

        assert!(matches!(why, AskError::Failed(_)), "{why:?}");
        assert!(
            why.to_string().contains(&cli.display().to_string()),
            "{why}"
        );
    }

    #[tokio::test]
    async fn output_that_is_not_addresses_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let cli = fake_cli(dir.path(), "echo 'not an address'");

        let why = ask_cli(&cli, Duration::from_secs(10)).await.unwrap_err();

        assert!(matches!(why, AskError::Failed(_)), "{why:?}");
        assert!(why.to_string().contains("not an address"), "{why}");
    }

    #[tokio::test]
    async fn a_cli_that_hangs_is_stopped_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        // A distinctive duration, so a stray is easy to find.
        let cli = fake_cli(dir.path(), "exec sleep 7.317");

        let why = ask_cli(&cli, Duration::from_millis(200)).await.unwrap_err();

        assert!(matches!(why, AskError::TimedOut(_)), "{why:?}");
        assert!(why.to_string().contains("did not answer"), "{why}");
    }
}
