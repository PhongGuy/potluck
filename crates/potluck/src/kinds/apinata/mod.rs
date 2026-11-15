//! The Apinata kind: Bruno-shaped collections whose `environments/*.yml` must not carry a
//! value for a `secret: true` variable.

use std::sync::LazyLock;

use globset::{GlobBuilder, GlobMatcher};

use super::SeedFile;

/// `**/environments/*.yml` with literal separators: the directory at any depth, the file
/// directly inside it, extension `.yml` only.
pub fn is_environment_file(path: &str) -> bool {
    ENVIRONMENT_FILE.is_match(path)
}

/// The seed: `.gitignore` then `opencollection.yml`, both at the workspace root, bytes
/// vendored from Apinata's `Collection::create`; they are a contract, copied, never edited.
pub fn seed() -> Vec<SeedFile> {
    vec![
        SeedFile::new(".gitignore", include_bytes!("seed/.gitignore")),
        SeedFile::new(
            "opencollection.yml",
            include_bytes!("seed/opencollection.yml"),
        ),
    ]
}

// Literal separators keep `*` inside one path segment, so a nested directory never matches.
#[allow(
    clippy::expect_used,
    reason = "the glob is a constant; a bad pattern is a build defect the tests catch on first use"
)]
static ENVIRONMENT_FILE: LazyLock<GlobMatcher> = LazyLock::new(|| {
    GlobBuilder::new("**/environments/*.yml")
        .literal_separator(true)
        .build()
        .expect("the environment file glob compiles")
        .compile_matcher()
});
