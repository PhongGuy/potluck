//! The Apinata kind: Bruno-shaped collections whose `environments/*.yml` must not carry a
//! value for a `secret: true` variable.

use std::sync::LazyLock;

use globset::{GlobBuilder, GlobMatcher};
use yaml_serde::Value;

use super::{Code, Finding, Kind, SeedFile};

/// The one kind of v1.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Apinata;

impl Kind for Apinata {
    fn name(&self) -> &'static str {
        "apinata"
    }

    /// No path rule of its own: `.env` belongs to the generic set.
    fn path_rule(&self, _path: &str) -> Option<Finding> {
        None
    }

    /// `secret_value` when `path` is an environment file whose single YAML document holds,
    /// at any depth, a mapping with `secret: true` (the boolean) and a non-null `value`.
    fn content_rule(&self, path: &str, blob: &[u8]) -> Option<Finding> {
        if !is_environment_file(path) {
            return None;
        }
        // A blob that is not one YAML document is not something Apinata would read; it passes.
        let doc = yaml_serde::from_slice::<Value>(blob).ok()?;
        has_valued_secret(&doc).then(|| Finding::new(Code::SecretValue, "secret_value"))
    }

    fn seed(&self) -> Vec<SeedFile> {
        seed()
    }
}

// Depth-first through mappings, sequences and tags. Only `null` means "no value", so an
// empty string still counts as one.
fn has_valued_secret(node: &Value) -> bool {
    match node {
        Value::Mapping(map) => {
            let secret = map.get("secret") == Some(&Value::Bool(true));
            let valued = map.get("value").is_some_and(|v| !v.is_null());
            (secret && valued) || map.values().any(has_valued_secret)
        }
        Value::Sequence(items) => items.iter().any(has_valued_secret),
        Value::Tagged(tagged) => has_valued_secret(&tagged.value),
        _ => false,
    }
}

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
