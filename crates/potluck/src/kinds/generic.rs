//! The rule set every kind gets.

use std::sync::LazyLock;

use regex::bytes::RegexSet;

use super::{Code, Finding};

/// Blobs above this size skip content rules; path rules still apply.
pub const MAX_CONTENT_BYTES: u64 = 1024 * 1024;

/// `env_file`: a basename of exactly `.env`, at any depth.
pub fn path_rule(path: &str) -> Option<Finding> {
    // Only the exact name: `.env.example` and friends are committed on purpose.
    (path.rsplit('/').next() == Some(".env")).then(|| Finding::new(Code::EnvFile, "env_file"))
}

/// The first pattern that matches, in table order: `private_key`, `aws`, `github`, `slack`,
/// `potluck`; all under `Code::SecretPattern`.
pub fn content_rule(blob: &[u8]) -> Option<Finding> {
    // A set reports every pattern that matched; the lowest index is the earliest table row.
    let first = PATTERN_SET.matches(blob).iter().next()?;
    PATTERNS
        .get(first)
        .map(|(rule, _)| Finding::new(Code::SecretPattern, rule))
}

/// `(rule, pattern)` rows; order decides which rule a blob with several secrets reports.
const PATTERNS: [(&str, &str); 5] = [
    // RFC 7468 and OpenSSH key labels, RFC 4880 PGP block; gitleaks `private-key`.
    (
        "private_key",
        r"-----BEGIN (?:[A-Z]+ )*PRIVATE KEY(?: BLOCK)?-----",
    ),
    // AWS IAM unique ID prefixes (AKIA, ASIA); base32 tail from gitleaks `aws-access-token`.
    ("aws", r"(?-u:\b)(?:AKIA|ASIA)[A-Z2-7]{16}(?-u:\b)"),
    // GitHub's token formats; lengths from gitleaks `github-pat`, `github-fine-grained-pat`.
    (
        "github",
        r"(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{36}|github_pat_[A-Za-z0-9]{22}_[A-Za-z0-9]{59}",
    ),
    // Slack bot and user token prefixes; shapes from gitleaks `slack-bot-token`, `slack-user-token`.
    (
        "slack",
        r"xoxb-[0-9]{10,13}-[0-9]{10,13}-[A-Za-z0-9]{24}|xoxp-[0-9]{10,13}-[0-9]{10,13}-[0-9]{10,13}-[a-f0-9]{32}",
    ),
    // Potluck's own token: `plk_` plus 43 base64url characters.
    ("potluck", r"plk_[A-Za-z0-9_-]{43}"),
];

#[allow(
    clippy::expect_used,
    reason = "the table is a constant; a bad pattern is a build defect the tests catch on first use"
)]
static PATTERN_SET: LazyLock<RegexSet> = LazyLock::new(|| {
    RegexSet::new(PATTERNS.iter().map(|(_, pattern)| pattern))
        .expect("the secret pattern table compiles")
});
