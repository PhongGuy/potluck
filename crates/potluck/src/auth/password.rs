//! argon2id password hashing and verification over PHC strings.

use argon2::password_hash::Error as HashError;
use argon2::{Algorithm, Argon2, Params, PasswordHasher, PasswordVerifier, Version};

/// argon2id v19 hashing at a fixed cost; `verify` reads the cost from the stored string.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Passwords {
    m_cost_kib: u32,
    t_cost: u32,
    p_cost: u32,
}

impl Passwords {
    /// m=19456 KiB, t=2, p=1 (the spec's parameters).
    pub fn new() -> Self {
        Self {
            m_cost_kib: 19_456,
            t_cost: 2,
            p_cost: 1,
        }
    }

    /// Another cost, for tests and benchmarks; refused below argon2's minimum (m under 8 KiB,
    /// t of 0, p of 0).
    pub fn with_cost(m_cost_kib: u32, t_cost: u32, p_cost: u32) -> Result<Self, PasswordError> {
        let candidate = Self {
            m_cost_kib,
            t_cost,
            p_cost,
        };
        // Validate once here so a constructed value can always build its hasher.
        candidate.argon2()?;
        Ok(candidate)
    }

    /// A PHC string `$argon2id$v=19$m=..,t=..,p=..$<salt>$<hash>` over a fresh 16-byte OS salt.
    pub fn hash(&self, password: &str) -> Result<String, PasswordError> {
        let hash = self
            .argon2()?
            .hash_password(password.as_bytes())
            .map_err(|err| PasswordError::Hashing {
                reason: err.to_string(),
            })?;
        Ok(hash.to_string())
    }

    /// `Ok(true)` when `password` produced `phc`, `Ok(false)` on a mismatch, `Err(MalformedHash)`
    /// when `phc` is not an argon2 PHC string.
    pub fn verify(&self, password: &str, phc: &str) -> Result<bool, PasswordError> {
        // The verifier takes algorithm and cost from `phc`, so our own cost only builds it.
        match PasswordVerifier::<str>::verify_password(&self.argon2()?, password.as_bytes(), phc) {
            Ok(()) => Ok(true),
            Err(HashError::PasswordInvalid) => Ok(false),
            Err(_) => Err(PasswordError::MalformedHash),
        }
    }

    fn argon2(&self) -> Result<Argon2<'static>, PasswordError> {
        let params =
            Params::new(self.m_cost_kib, self.t_cost, self.p_cost, None).map_err(|err| {
                PasswordError::InvalidCost {
                    reason: err.to_string(),
                }
            })?;
        Ok(Argon2::new(Algorithm::Argon2id, Version::V0x13, params))
    }
}

impl Default for Passwords {
    fn default() -> Self {
        Self::new()
    }
}

/// Why a password could not be hashed or checked.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PasswordError {
    /// The requested argon2 cost is outside what argon2 accepts.
    #[error("invalid argon2 cost: {reason}")]
    InvalidCost {
        /// argon2's description of the rejected parameter.
        reason: String,
    },
    /// argon2 or the OS random source failed while hashing.
    #[error("password hashing failed: {reason}")]
    Hashing {
        /// The underlying failure, which never contains the password.
        reason: String,
    },
    /// The stored hash is not an argon2 PHC string.
    #[error("stored password hash is malformed")]
    MalformedHash,
}
