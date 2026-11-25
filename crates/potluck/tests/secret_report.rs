//! The hook's environment and the report lines.

use potluck::kinds::{Code, Finding};
use potluck::secret_check::scan::{HookEnv, MAX_LINES, Rejection, percent_encode, report};

fn env_file(path: &[u8]) -> Rejection {
    Rejection::new(
        b"refs/heads/main",
        path,
        Finding::new(Code::EnvFile, "env_file", &String::from_utf8_lossy(path)),
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
fn hook_env_role_defaults_to_write() {
    use potluck::role::Role;
    assert_eq!(HookEnv::new(None, false, None).role(), Role::Write);
    assert_eq!(
        HookEnv::new(None, false, None)
            .with_role(Role::Admin)
            .role(),
        Role::Admin
    );
}

#[test]
fn rejection_accessors_round_trip() {
    let r = Rejection::new(
        b"refs/heads/x",
        b"a/b",
        Finding::new(Code::SecretPattern, "aws", "a/b"),
    );
    assert_eq!(
        (r.ref_name(), r.path(), r.finding().rule()),
        (b"refs/heads/x" as &[u8], b"a/b" as &[u8], "aws")
    );
}

#[test]
fn report_prints_the_contract_lines_capped_at_twenty() {
    let one = Rejection::new(
        b"refs/heads/main",
        b"creds.txt",
        Finding::new(Code::SecretPattern, "aws", "creds.txt"),
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
        b"refs/heads/a\nb",
        b".env",
        Finding::new(Code::EnvFile, "env_file", ".env"),
    );
    assert_eq!(
        text(&[odd_ref]),
        "potluck-reject secret_env_file refs/heads/a%0Ab .env env_file\n"
    );
    assert_eq!(
        percent_encode(b"/"),
        "/",
        "the no-path marker is written as is"
    );
    for line in text(&[env_file(b"a b"), env_file(b"x\xffy")]).lines() {
        assert_eq!(line.split(' ').count(), 5, "{line}");
    }
}

#[test]
fn percent_encode_escapes_each_byte_of_a_truncated_sequence() {
    assert_eq!(percent_encode(b"caf\xc3"), "caf%C3", "cut at the end");
    assert_eq!(
        percent_encode(b"\xe2\x82"),
        "%E2%82",
        "nothing but the cut sequence"
    );
    assert_eq!(
        percent_encode(b"\xf0\x9f\x98 x"),
        "%F0%9F%98%20x",
        "cut before a space"
    );
    assert_eq!(
        percent_encode(b"\xe2\x82\xc3\xa9"),
        "%E2%82\u{e9}",
        "a valid character after the cut stays readable"
    );
}

fn detailed(path: &[u8], detail: &str) -> Rejection {
    Rejection::new(
        b"refs/heads/main",
        path,
        Finding::new(Code::SecretValue, "password_key", "x").with_detail(detail),
    )
}

#[test]
fn a_detail_line_follows_its_reject_line_encoded() {
    assert_eq!(
        text(&[
            detailed(b"dbento.toml", "connections.prod password"),
            env_file(b".env")
        ]),
        concat!(
            "potluck-reject secret_value refs/heads/main dbento.toml password_key\n",
            "potluck-reject-detail connections.prod%20password\n",
            "potluck-reject secret_env_file refs/heads/main .env env_file\n",
        )
    );
}

#[test]
fn an_empty_detail_prints_no_detail_line() {
    assert_eq!(
        text(&[detailed(b"dbento.toml", ""), env_file(b".env")]),
        concat!(
            "potluck-reject secret_value refs/heads/main dbento.toml password_key\n",
            "potluck-reject secret_env_file refs/heads/main .env env_file\n",
        )
    );
}

#[test]
fn a_detail_is_capped_at_200_bytes_on_a_character_boundary() {
    let long = format!("{}\u{e9}tail", "a".repeat(199));
    let out = text(&[detailed(b"f", &long)]);
    let detail = out.lines().nth(1).unwrap();
    assert_eq!(
        detail,
        format!("potluck-reject-detail {}", "a".repeat(199)),
        "the 2-byte é would cross 200"
    );
    let exact = "b".repeat(250);
    let out = text(&[detailed(b"f", &exact)]);
    assert_eq!(
        out.lines().nth(1).unwrap(),
        format!("potluck-reject-detail {}", "b".repeat(200))
    );
}

#[test]
fn detail_lines_do_not_count_toward_twenty() {
    let many: Vec<Rejection> = (0..22)
        .map(|i| detailed(format!("f{i}").as_bytes(), "k"))
        .collect();
    let out = text(&many);
    assert_eq!(
        out.lines()
            .filter(|l| l.starts_with("potluck-reject "))
            .count(),
        MAX_LINES
    );
    assert_eq!(
        out.lines()
            .filter(|l| l.starts_with("potluck-reject-detail "))
            .count(),
        MAX_LINES
    );
    assert_eq!(out.lines().last(), Some("potluck-reject-more 2"));
}

#[test]
fn percent_encode_escapes_format_characters() {
    assert_eq!(percent_encode("a\u{202E}b".as_bytes()), "a%E2%80%AEb");
    assert_eq!(percent_encode("a\u{200B}b".as_bytes()), "a%E2%80%8Bb");
    assert_eq!(percent_encode("\u{FEFF}x".as_bytes()), "%EF%BB%BFx");
}

#[test]
fn percent_encode_escapes_c1_controls() {
    assert_eq!(percent_encode("a\u{0085}b".as_bytes()), "a%C2%85b");
    assert_eq!(percent_encode("\u{0080}".as_bytes()), "%C2%80");
    assert_eq!(percent_encode("\u{009F}".as_bytes()), "%C2%9F");
}

#[test]
fn percent_encode_escapes_line_and_paragraph_separators() {
    assert_eq!(percent_encode("a\u{2028}b".as_bytes()), "a%E2%80%A8b");
    assert_eq!(percent_encode("a\u{2029}b".as_bytes()), "a%E2%80%A9b");
}

#[test]
fn percent_encode_leaves_ordinary_non_ascii_raw() {
    for raw in [
        "\u{e6}\u{f8}\u{e5}",
        "\u{6f22}\u{5b57}",
        "\u{1F600}",
        "a\u{00A0}b",
    ] {
        assert_eq!(percent_encode(raw.as_bytes()), raw);
    }
}

/// The client's side: each `%XX` back to its byte, every other byte as is.
fn percent_decode(encoded: &str) -> Vec<u8> {
    let bytes = encoded.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap();
            out.push(u8::from_str_radix(hex, 16).unwrap());
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

#[test]
fn percent_decode_of_percent_encode_restores_the_bytes() {
    let mut raw = Vec::new();
    raw.extend_from_slice("a %\t\x7f\u{202E}\u{200B}\u{FEFF}".as_bytes());
    raw.extend_from_slice("\u{0080}\u{0085}\u{009F}\u{2028}\u{2029}".as_bytes());
    raw.extend_from_slice("\u{e6}\u{f8}\u{e5}\u{6f22}\u{1F600}\u{00A0}".as_bytes());
    raw.extend_from_slice(b"\xff\xe2\x80 \xc3");
    let encoded = percent_encode(&raw);
    assert!(
        !encoded.contains(['\u{202E}', '\u{0085}', '\u{2028}']),
        "{encoded}"
    );
    assert_eq!(percent_decode(&encoded), raw);
}
