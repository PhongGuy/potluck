//! The server's scoped quiet panic hook: rule-set panics print nothing, others still do.

use potluck::secret_check::git::{Git, RefUpdate};
use potluck::secret_check::scan::{ScanError, accepted_relaxations};

use crate::common::scratch::Scratch;
use crate::test_rules::{Act, rules};

/// Re-runs this test binary on `quiet_hook_child`, since a panic hook is process-wide, and
/// reads what reached the child's stderr.
#[test]
fn a_rule_panic_is_silent_and_an_unrelated_panic_still_prints_once() {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "panic_hook::quiet_hook_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("POTLUCK_TEST_QUIET_HOOK_CHILD", "1")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}{stderr}");
    assert!(stdout.contains("1 passed"), "the child ran: {stdout}");
    assert!(!stderr.contains("PANIC-SECRET-TEXT"), "{stderr}");
    assert_eq!(
        stderr.matches("UNRELATED-PANIC").count(),
        1,
        "the previous hook runs once, however often the quiet hook is installed: {stderr}"
    );
}

#[test]
fn quiet_hook_child() {
    if std::env::var_os("POTLUCK_TEST_QUIET_HOOK_CHILD").is_none() {
        return;
    }
    potluck::secret_check::quiet_rule_panics();
    potluck::secret_check::quiet_rule_panics();
    let s = Scratch::new();
    s.write("a.t", b"one\n");
    let old = s.commit("old");
    s.publish("main");
    s.write("a.t", b"two\n");
    let new = s.stage_commit("new");
    let update = RefUpdate::parse_line(format!("{old} {new} refs/heads/main").as_bytes()).unwrap();
    let git = Git::new(s.isolated_git(), Some(s.bare.clone()));
    assert!(matches!(
        accepted_relaxations(&rules(Act::Panic), &[update], &git),
        Err(ScanError::Rules)
    ));
    let unrelated = std::thread::spawn(|| panic!("UNRELATED-PANIC")).join();
    assert!(unrelated.is_err());
}
