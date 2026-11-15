//! The generic rule set: codes, the `.env` path rule, and the pattern table.

use potluck::kinds::generic::{MAX_CONTENT_BYTES, path_rule};
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
