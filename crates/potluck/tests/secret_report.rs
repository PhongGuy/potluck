//! The hook's environment and the report lines.

use potluck::kinds::{Code, Finding};
use potluck::secret_check::scan::{HookEnv, MAX_LINES, Rejection, percent_encode, report};

fn env_file(path: &[u8]) -> Rejection {
    Rejection::new(
        "refs/heads/main",
        path,
        Finding::new(Code::EnvFile, "env_file"),
    )
}

fn text(rejections: &[Rejection]) -> String {
    let mut out = Vec::new();
    report(rejections, &mut out).unwrap();
    String::from_utf8(out).unwrap()
}

#[test]
fn hook_env_reads_the_three_variables() {
    let e = HookEnv::new(Some("apinata"), true, None);
    assert_eq!(
        (e.kind(), e.is_import(), e.override_id()),
        (Some("apinata"), true, None)
    );
    assert_eq!(
        HookEnv::new(None, false, Some("17")).override_id(),
        Some("17")
    );
    assert_eq!(
        HookEnv::new(None, false, Some("")).override_id(),
        None,
        "an empty override is no override"
    );
    assert_eq!(
        HookEnv::new(Some(""), false, None).kind(),
        None,
        "an empty kind is no kind"
    );
}

#[test]
fn rejection_accessors_round_trip() {
    let r = Rejection::new(
        "refs/heads/x",
        b"a/b",
        Finding::new(Code::SecretPattern, "aws"),
    );
    assert_eq!(
        (r.ref_name(), r.path(), r.finding().rule()),
        ("refs/heads/x", b"a/b" as &[u8], "aws")
    );
}

#[test]
fn report_prints_the_contract_lines_capped_at_twenty() {
    let one = Rejection::new(
        "refs/heads/main",
        b"creds.txt",
        Finding::new(Code::SecretPattern, "aws"),
    );
    assert_eq!(
        text(&[one]),
        "potluck-reject secret_pattern refs/heads/main creds.txt aws\n"
    );
    let many: Vec<Rejection> = (0..23)
        .map(|i| env_file(format!("f{i}").as_bytes()))
        .collect();
    let all = text(&many);
    let lines: Vec<&str> = all.lines().collect();
    assert_eq!(lines.len(), MAX_LINES + 1, "{all}");
    assert_eq!(
        lines[0],
        "potluck-reject secret_env_file refs/heads/main f0 env_file"
    );
    assert_eq!(
        lines[19],
        "potluck-reject secret_env_file refs/heads/main f19 env_file"
    );
    assert_eq!(lines[20], "potluck-reject-more 3");
    assert_eq!(
        text(&many[..MAX_LINES]).lines().count(),
        MAX_LINES,
        "exactly twenty: no more line"
    );
    assert_eq!(text(&[]), "");
}

#[test]
fn report_percent_encodes_paths_and_ref_names() {
    assert_eq!(
        text(&[env_file(b"notes/with space.txt")]),
        "potluck-reject secret_env_file refs/heads/main notes/with%20space.txt env_file\n"
    );
    assert_eq!(
        text(&[env_file(b"a\nb\tc\x7fd%e")]),
        "potluck-reject secret_env_file refs/heads/main a%0Ab%09c%7Fd%25e env_file\n"
    );
    assert_eq!(
        text(&[env_file(b"caf\xe9/.env")]),
        "potluck-reject secret_env_file refs/heads/main caf%E9/.env env_file\n"
    );
    assert_eq!(
        text(&[env_file("caf\u{e9}/.env".as_bytes())]),
        "potluck-reject secret_env_file refs/heads/main caf\u{e9}/.env env_file\n",
        "valid UTF-8 stays readable"
    );
    let odd_ref = Rejection::new(
        "refs/heads/a\nb",
        b".env",
        Finding::new(Code::EnvFile, "env_file"),
    );
    assert_eq!(
        text(&[odd_ref]),
        "potluck-reject secret_env_file refs/heads/a%0Ab .env env_file\n"
    );
    assert_eq!(percent_encode(b"-"), "-");
    for line in text(&[env_file(b"a b"), env_file(b"x\xffy")]).lines() {
        assert_eq!(line.split(' ').count(), 5, "{line}");
    }
}
