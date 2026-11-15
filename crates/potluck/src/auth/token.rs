//! Personal access tokens: minting `plk_` tokens, their stored prefix and their SHA-256 digest.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// `plk_` plus 43 base64url characters.
pub const TOKEN_LEN: usize = 47;
/// The stored and displayed prefix: the first 8 characters of the token.
pub const PREFIX_LEN: usize = 8;

const SCHEME: &str = "plk_";

/// A fresh token: the secret, shown to the client once, and what the database keeps.
#[derive(Clone)]
#[non_exhaustive]
pub struct MintedToken {
    secret: String,
    prefix: String,
    digest: [u8; 32],
}

impl MintedToken {
    /// 32 bytes from the OS RNG, base64url without padding, behind `plk_`.
    pub fn mint() -> Result<Self, TokenError> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).map_err(|e| TokenError::Rng {
            reason: e.to_string(),
        })?;
        let secret = format!("{SCHEME}{}", URL_SAFE_NO_PAD.encode(bytes));
        Ok(MintedToken {
            prefix: secret[..PREFIX_LEN].to_owned(),
            digest: digest(&secret),
            secret,
        })
    }

    /// The full token text; shown to the client once and never stored.
    pub fn secret(&self) -> &str {
        &self.secret
    }

    /// The first [`PREFIX_LEN`] characters, safe to store and display.
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// The SHA-256 digest the database keeps.
    pub fn digest(&self) -> &[u8; 32] {
        &self.digest
    }
}

impl std::fmt::Debug for MintedToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MintedToken")
            .field("prefix", &self.prefix)
            .finish_non_exhaustive()
    }
}

/// SHA-256 of the token text as presented.
pub fn digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

/// `Some(&token[..8])` when `token` has the shape (47 chars, `plk_`, base64url body); `None` otherwise.
pub fn prefix_of(token: &str) -> Option<&str> {
    // A shape check only: anything that passes is still looked up by digest.
    let body = token.strip_prefix(SCHEME)?;
    let shaped = token.len() == TOKEN_LEN
        && body
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    shaped.then(|| &token[..PREFIX_LEN])
}

/// Constant-time equality.
pub fn digests_match(a: &[u8; 32], b: &[u8; 32]) -> bool {
    bool::from(a.ct_eq(b))
}

/// Why a token could not be minted.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TokenError {
    /// The operating system's random number generator returned an error.
    #[error("the OS random number generator failed: {reason}")]
    Rng {
        /// The generator's own description of the failure.
        reason: String,
    },
}
