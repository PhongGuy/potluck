//! [`Slug`]: a validated team, workspace, user or token name.

use std::fmt;
use std::str::FromStr;

/// A team, workspace, user or token name: `^[a-z0-9][a-z0-9-]{0,62}$`, so it is safe as one
/// path segment under `repos/`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Slug(String);

impl Slug {
    /// Validates `name` against the pattern.
    pub fn parse(name: &str) -> Result<Slug, SlugError> {
        if name.is_empty() || name.len() > 63 {
            return Err(SlugError::Length);
        }
        let allowed = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-';
        if name.starts_with('-') || !name.bytes().all(allowed) {
            return Err(SlugError::Charset);
        }
        Ok(Slug(name.to_owned()))
    }

    /// The validated text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for Slug {
    type Err = SlugError;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Slug::parse(name)
    }
}

impl AsRef<str> for Slug {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Slug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why a name was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SlugError {
    /// Empty, or longer than 63 bytes.
    #[error("a name is 1 to 63 characters")]
    Length,
    /// A byte outside `[a-z0-9-]`, or a leading hyphen.
    #[error("a name is lower-case letters, digits and hyphens and does not start with a hyphen")]
    Charset,
}
