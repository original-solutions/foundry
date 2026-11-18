//! The command line: `--version` works and every other command says it is not built yet.

use std::process::{Command, Output};

fn foundryd(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_foundryd"))
        .args(args)
        .output()
        .expect("foundryd runs")
}

#[test]
fn version_prints_the_crate_version() {
    let out = foundryd(&["--version"]);

    assert!(out.status.success(), "exit: {:?}", out.status);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("foundryd {}\n", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn every_other_command_exits_2_and_says_not_built_yet() {
    let commands: [(&str, &[&str]); 4] = [
        ("run", &["run"]),
        ("install", &["install"]),
        ("uninstall", &["uninstall"]),
        ("status", &["status"]),
    ];

    for (name, args) in commands {
        let out = foundryd(args);
        let stderr = String::from_utf8_lossy(&out.stderr);

        assert_eq!(
            out.status.code(),
            Some(2),
            "{args:?} exit: {:?}",
            out.status
        );
        assert!(
            stderr.contains(&format!("foundryd {name}: not built yet")),
            "{args:?} stderr: {stderr}"
        );
    }
}

#[test]
fn join_with_a_port_that_is_not_a_number_exits_1_and_names_it() {
    let out = Command::new(env!("CARGO_BIN_EXE_foundryd"))
        .args(["join", "100.64.0.1", "ABCDEFGHJK"])
        .env("FOUNDRY_PORT", "not-a-port")
        .output()
        .expect("foundryd runs");
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert_eq!(out.status.code(), Some(1), "exit: {:?}", out.status);
    assert!(stderr.contains("FOUNDRY_PORT"), "stderr: {stderr}");
}
