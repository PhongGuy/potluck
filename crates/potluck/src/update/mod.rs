//! The update notice: the minisign keys a signed release must verify against, and the source
//! the signed release manifest is fetched from.

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use url::{Host, Url};

mod checker;
mod verify;

pub use checker::{UpdateChecker, UpdateStatus};
pub use verify::{Release, verify_release};

/// The container image a release is published as.
pub const IMAGE: &str = "ghcr.io/phongguy/potluck";
/// Where the latest release's files are downloaded from.
pub const GITHUB_BASE: &str = "https://github.com/PhongGuy/potluck/releases/latest/download/";
/// The signed release manifest, relative to the base URL.
pub const MANIFEST_FILE: &str = "potluck-release.json";
/// The manifest's minisign signature, relative to the base URL.
pub const SIGNATURE_FILE: &str = "potluck-release.json.sig";
/// The largest release file the checker reads.
pub const MAX_FILE_BYTES: usize = 64 * 1024;

const EMBEDDED_KEYS: &str = include_str!("../../assets/update-keys.txt");
const DEFAULT_CONNECT: Duration = Duration::from_secs(10);
const DEFAULT_TOTAL: Duration = Duration::from_secs(30);

/// Why an update check, or the setup of one, failed. The message names the class only and
/// never carries fetched bytes.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum UpdateError {
    /// A line of the keys file is not a minisign public key.
    #[error("update-keys.txt line {line}: {reason}")]
    Keys {
        /// The 1-based line number.
        line: usize,
        /// What is wrong with it.
        reason: String,
    },
    /// The keys file holds no key.
    #[error("update-keys.txt lists no key")]
    NoKeys,
    /// The base URL is not https or loopback-IP http, or carries more than a path.
    #[error("invalid update base URL: {reason}")]
    BaseUrl {
        /// What is wrong with it.
        reason: String,
    },
    /// The running version cannot be compared.
    #[error("the running version {version} is not semver")]
    CurrentVersion {
        /// The version as built.
        version: String,
    },
    /// The source could not be reached.
    #[error("cannot reach the update source (DNS, connect, TLS or timeout)")]
    Network,
    /// The source has no signed release.
    #[error("no signed release is published (HTTP 404)")]
    NotFound,
    /// The source answered with an unexpected HTTP status.
    #[error("the update source answered HTTP {status}")]
    Status {
        /// The status code.
        status: u16,
    },
    /// The source redirected too often or off https.
    #[error("the update source redirected too often or to a non-https URL")]
    Redirect,
    /// A release file exceeds [`MAX_FILE_BYTES`].
    #[error("a release file is larger than 64 KiB")]
    TooLarge,
    /// The signature file does not parse.
    #[error("the signature file is not a minisign signature")]
    SignatureFormat,
    /// The signature names a key that is not trusted.
    #[error("signature by an unknown key")]
    UnknownKey,
    /// The signature does not match the manifest.
    #[error("the signature does not verify")]
    BadSignature,
    /// The signed manifest has a missing or malformed field.
    #[error("the signed manifest has a malformed {field}")]
    Manifest {
        /// The field's name.
        field: &'static str,
    },
    /// The server stopped during the check.
    #[error("the update check stopped before it finished")]
    Interrupted,
}

/// The minisign public keys a release signature may be made with, in file order.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct TrustedKeys {
    keys: Vec<(String, minisign_verify::PublicKey)>,
}

impl TrustedKeys {
    /// Parses a keys file: one base64-encoded minisign `.pub` text per line, with blank lines
    /// and `#` comments skipped. Fails on the first bad line, or when no key is listed.
    pub fn parse(text: &str) -> Result<TrustedKeys, UpdateError> {
        let mut keys = Vec::new();
        for (index, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let bad = |reason: &str| UpdateError::Keys {
                line: index + 1,
                reason: reason.into(),
            };
            let bytes = STANDARD.decode(line).map_err(|_| bad("not base64"))?;
            let pub_text = String::from_utf8(bytes).map_err(|_| bad("not UTF-8"))?;
            let key = minisign_verify::PublicKey::decode(&pub_text)
                .map_err(|_| bad("not a minisign public key"))?;
            let id = key_id(&pub_text).ok_or_else(|| bad("not a minisign public key"))?;
            keys.push((id, key));
        }
        if keys.is_empty() {
            return Err(UpdateError::NoKeys);
        }
        Ok(TrustedKeys { keys })
    }

    /// The key ids as minisign prints them (uppercase hex), in file order.
    pub fn key_ids(&self) -> Vec<String> {
        self.keys.iter().map(|(id, _)| id.clone()).collect()
    }
}

// minisign-verify exposes no key id, so it is read from the key line itself: bytes 2..10 after
// the algorithm tag, stored little-endian and printed most significant byte first.
fn key_id(pub_text: &str) -> Option<String> {
    let bytes = STANDARD.decode(pub_text.lines().nth(1)?.trim()).ok()?;
    Some(
        bytes
            .get(2..10)?
            .iter()
            .rev()
            .map(|b| format!("{b:02X}"))
            .collect(),
    )
}

/// Where signed releases are fetched from, the keys they must verify against, and the
/// per-request timeouts.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct UpdateSource {
    base: Url,
    keys: TrustedKeys,
    connect: Duration,
    total: Duration,
}

impl UpdateSource {
    /// A source at `base_url`: https, or plain http to a loopback IP literal, with no userinfo,
    /// query or fragment. A path without a trailing `/` gains one.
    pub fn new(base_url: &str, keys: TrustedKeys) -> Result<UpdateSource, UpdateError> {
        let bad = |reason: &str| UpdateError::BaseUrl {
            reason: reason.into(),
        };
        let mut base = Url::parse(base_url).map_err(|_| bad("not a URL"))?;
        if !base.username().is_empty() || base.password().is_some() {
            return Err(bad("it carries credentials"));
        }
        if base.query().is_some() || base.fragment().is_some() {
            return Err(bad("it carries a query or fragment"));
        }
        let loopback_ip = match base.host() {
            Some(Host::Ipv4(ip)) => ip.is_loopback(),
            Some(Host::Ipv6(ip)) => ip.is_loopback(),
            _ => false,
        };
        match base.scheme() {
            "https" => {}
            "http" if loopback_ip => {}
            _ => return Err(bad("it must be https, or http to a loopback IP")),
        }
        if !base.path().ends_with('/') {
            let path = format!("{}/", base.path());
            base.set_path(&path);
        }
        Ok(UpdateSource {
            base,
            keys,
            connect: DEFAULT_CONNECT,
            total: DEFAULT_TOTAL,
        })
    }

    /// The published GitHub releases, checked against the keys compiled into this binary.
    pub fn github() -> Result<UpdateSource, UpdateError> {
        UpdateSource::new(GITHUB_BASE, TrustedKeys::parse(EMBEDDED_KEYS)?)
    }

    /// Replaces the connect timeout and the whole-request timeout (defaults 10 s and 30 s).
    #[must_use]
    pub fn with_timeouts(self, connect: Duration, total: Duration) -> UpdateSource {
        UpdateSource {
            connect,
            total,
            ..self
        }
    }

    /// The base URL, always ending in `/`.
    pub fn base_url(&self) -> &str {
        self.base.as_str()
    }

    /// The keys a release signature must verify against.
    pub fn keys(&self) -> &TrustedKeys {
        &self.keys
    }
}
