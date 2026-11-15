//! The generic rule set: codes, the `.env` path rule, and the pattern table.

use potluck::kinds::generic::{MAX_CONTENT_BYTES, content_rule, path_rule};
use potluck::kinds::{Code, Finding};

#[test]
fn codes_carry_their_contract_names() {
    assert_eq!(Code::EnvFile.as_str(), "secret_env_file");
    assert_eq!(Code::SecretValue.as_str(), "secret_value");
    assert_eq!(Code::SecretPattern.as_str(), "secret_pattern");
    assert_eq!(MAX_CONTENT_BYTES, 1024 * 1024);
    let f = Finding::new(Code::SecretPattern, "aws");
    assert_eq!((f.code(), f.rule()), (Code::SecretPattern, "aws"));
    assert_eq!(f.clone(), f);
}

#[test]
fn only_a_file_named_dot_env_hits_the_path_rule() {
    let hit = Some(Finding::new(Code::EnvFile, "env_file"));
    for p in [".env", "api/.env", "deep/er/.env"] {
        assert_eq!(path_rule(p), hit, "{p}");
    }
    for p in [
        ".env.example",
        ".env.local",
        "env",
        "dotenv",
        ".envrc",
        "x.env",
        "environments/dev.yml",
        "-",
        "",
    ] {
        assert_eq!(path_rule(p), None, "{p}");
    }
}

macro_rules! fixture {
    ($dir:literal, $name:literal) => {
        include_bytes!(concat!("fixtures/secret-check/", $dir, "/", $name)) as &[u8]
    };
}

#[test]
fn each_bad_fixture_hits_its_rule() {
    let cases: [(&str, &[u8], &str); 8] = [
        (
            "aws-access-key.txt",
            fixture!("bad", "aws-access-key.txt"),
            "aws",
        ),
        (
            "github-classic.txt",
            fixture!("bad", "github-classic.txt"),
            "github",
        ),
        (
            "github-fine-grained.txt",
            fixture!("bad", "github-fine-grained.txt"),
            "github",
        ),
        ("slack-bot.txt", fixture!("bad", "slack-bot.txt"), "slack"),
        ("slack-user.txt", fixture!("bad", "slack-user.txt"), "slack"),
        (
            "potluck-token.txt",
            fixture!("bad", "potluck-token.txt"),
            "potluck",
        ),
        (
            "rsa-private-key.pem",
            fixture!("bad", "rsa-private-key.pem"),
            "private_key",
        ),
        (
            "openssh-private-key",
            fixture!("bad", "openssh-private-key"),
            "private_key",
        ),
    ];
    for (name, bytes, rule) in cases {
        assert_eq!(
            content_rule(bytes),
            Some(Finding::new(Code::SecretPattern, rule)),
            "{name}"
        );
    }
}

#[test]
fn good_fixtures_pass() {
    let cases: [(&str, &[u8]); 6] = [
        ("aws-prose.txt", fixture!("good", "aws-prose.txt")),
        ("github-prose.txt", fixture!("good", "github-prose.txt")),
        ("slack-prose.txt", fixture!("good", "slack-prose.txt")),
        ("potluck-prose.txt", fixture!("good", "potluck-prose.txt")),
        ("public-key.pem", fixture!("good", "public-key.pem")),
        ("certificate.pem", fixture!("good", "certificate.pem")),
    ];
    for (name, bytes) in cases {
        assert_eq!(content_rule(bytes), None, "{name}");
    }
    assert_eq!(content_rule(b""), None);
}

#[test]
fn the_rule_runs_on_arbitrary_bytes() {
    let mut bytes = vec![0xff, 0xfe, 0x00];
    bytes.extend_from_slice(b"AKIAIOSFODNN7EXAMPLE");
    bytes.push(0x80);
    assert_eq!(
        content_rule(&bytes),
        Some(Finding::new(Code::SecretPattern, "aws"))
    );
    let mut noise = vec![0xff; 4096];
    noise[100] = b'A';
    assert_eq!(content_rule(&noise), None);
}

#[test]
fn a_blob_with_two_secrets_reports_the_first_rule_in_table_order() {
    let mut both = fixture!("bad", "slack-bot.txt").to_vec();
    both.extend_from_slice(fixture!("bad", "rsa-private-key.pem"));
    assert_eq!(
        content_rule(&both),
        Some(Finding::new(Code::SecretPattern, "private_key"))
    );
}
