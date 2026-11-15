//! The rule set every kind gets.

use super::{Code, Finding};

/// Blobs above this size skip content rules; path rules still apply.
pub const MAX_CONTENT_BYTES: u64 = 1024 * 1024;

/// `env_file`: a basename of exactly `.env`, at any depth.
pub fn path_rule(path: &str) -> Option<Finding> {
    // Only the exact name: `.env.example` and friends are committed on purpose.
    (path.rsplit('/').next() == Some(".env")).then(|| Finding::new(Code::EnvFile, "env_file"))
}
