//! The binary exists, answers `--version` and rejects unknown flags.

use crate::common::potluck;

#[test]
fn version_prints_the_crate_name_and_version() {
    let out = potluck().arg("--version").output().expect("run potluck");
    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("potluck {}\n", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn an_unknown_flag_is_a_usage_error_with_exit_2() {
    let out = potluck()
        .arg("--no-such-flag")
        .output()
        .expect("run potluck");
    assert_eq!(out.status.code(), Some(2), "clap usage errors exit 2");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--no-such-flag"),
        "stderr names the flag: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
