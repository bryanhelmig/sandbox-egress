//! Process-level checks for the thin executable wrapper.

use std::process::{Command, Stdio};

fn executable() -> Command {
    Command::new(env!("CARGO_BIN_EXE_sandbox-egress"))
}

#[test]
fn missing_policy_prints_usage_and_exits_with_two() {
    let output = executable().output().expect("run executable");

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).expect("UTF-8 stderr"),
        "usage: sandbox-egress HOST [HOST ...]\n"
    );
}

#[test]
fn stdin_eof_revokes_the_embedded_lease_cleanly() {
    let output = executable()
        .arg("example.com")
        .stdin(Stdio::null())
        .output()
        .expect("run executable");

    assert!(
        output.status.success(),
        "wrapper failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 stdout");
    assert!(
        stdout.starts_with("HTTP_PROXY=http://127.0.0.1:"),
        "{stdout}"
    );
    assert!(
        stdout.contains("Press Enter to revoke the lease.\n"),
        "{stdout}"
    );
    assert!(stdout.contains("final usage: Usage"), "{stdout}");
}
