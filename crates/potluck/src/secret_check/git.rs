//! The git plumbing the hook runs inside the push's quarantine.

use std::fmt;

/// A hex object name: 40 (SHA-1) or 64 (SHA-256) lower-case hex digits.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Sha(String);

impl Sha {
    /// Parses an object name of either width, folding upper-case hex to lower; anything else
    /// is [`ParseError::NotHex`].
    pub fn parse(hex: &str) -> Result<Sha, ParseError> {
        let width_ok = hex.len() == 40 || hex.len() == 64;
        if !width_ok || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ParseError::NotHex {
                text: hex.to_owned(),
            });
        }
        Ok(Sha(hex.to_ascii_lowercase()))
    }

    /// The lower-case hex digits.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Every digit is `0`: git's "no object".
    pub fn is_zero(&self) -> bool {
        self.0.bytes().all(|b| b == b'0')
    }
}

impl fmt::Display for Sha {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One `<old> <new> <ref>` line of pre-receive stdin.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RefUpdate {
    old: Sha,
    new: Sha,
    name: String,
}

impl RefUpdate {
    /// Parses exactly three space-separated fields; a trailing newline is fine.
    pub fn parse_line(line: &str) -> Result<RefUpdate, ParseError> {
        let trimmed = line.strip_suffix('\n').unwrap_or(line);
        let fields: Vec<&str> = trimmed.split(' ').collect();
        let [old, new, name] = fields.as_slice() else {
            return Err(ParseError::Shape {
                line: line.to_owned(),
            });
        };
        if name.is_empty() {
            return Err(ParseError::Shape {
                line: line.to_owned(),
            });
        }
        Ok(RefUpdate {
            old: Sha::parse(old)?,
            new: Sha::parse(new)?,
            name: (*name).to_owned(),
        })
    }

    /// The ref's value before the push.
    pub fn old(&self) -> &Sha {
        &self.old
    }

    /// The ref's value the push proposes.
    // An accessor named after git's own `<old> <new> <ref>` field, not a constructor.
    #[allow(clippy::new_ret_no_self, clippy::wrong_self_convention)]
    pub fn new(&self) -> &Sha {
        &self.new
    }

    /// The full ref name, such as `refs/heads/main`.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// `new` is all zeros: the ref is being deleted and there is nothing to scan.
    pub fn is_delete(&self) -> bool {
        self.new.is_zero()
    }
}

/// Why a stdin line was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ParseError {
    /// Not 40 or 64 hex digits.
    #[error("not an object name: {text:?}")]
    NotHex {
        /// The offending field.
        text: String,
    },
    /// Not three fields.
    #[error("expected `<old> <new> <ref>`: {line:?}")]
    Shape {
        /// The whole line.
        line: String,
    },
}
