//! The binary's own flags, answered before any window opens (so they run
//! without a display).

use std::process::{Command, Output};

fn run(flag: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_streamforge-desktop"))
        .arg(flag)
        .output()
        .expect("run streamforge-desktop")
}

#[test]
fn version_and_help_answer_without_a_window() {
    let version = run("--version");
    assert!(version.status.success(), "{version:?}");
    assert_eq!(
        String::from_utf8_lossy(&version.stdout).trim(),
        format!("streamforge-desktop {}", env!("CARGO_PKG_VERSION"))
    );

    let help = run("--help");
    assert!(help.status.success(), "{help:?}");
    assert!(String::from_utf8_lossy(&help.stdout).starts_with("usage: streamforge-desktop"));

    let unknown = run("--no-such-flag");
    assert!(!unknown.status.success());
    assert!(String::from_utf8_lossy(&unknown.stderr).starts_with("usage: streamforge-desktop"));
}
