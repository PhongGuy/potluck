//! Turning a fetched manifest and its `.sig` file into a [`Release`]: the signature is checked
//! against the trusted keys first, and only verified bytes are parsed.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;

use super::{IMAGE, TrustedKeys, UpdateError};
use crate::clock;

/// A signed, well-formed release as the manifest describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Release {
    version: String,
    // The public image digest, not a secret; named apart from the secret-digest fields.
    image_sha: String,
    published: String,
    notes_url: String,
}

impl Release {
    /// The release version, bare semver as signed.
    pub fn version(&self) -> &str {
        &self.version
    }

    /// The image digest, `sha256:` and 64 lowercase hex digits.
    pub fn digest(&self) -> &str {
        &self.image_sha
    }

    /// When the release was published, normalised to UTC RFC 3339.
    pub fn published(&self) -> &str {
        &self.published
    }

    /// The https URL of the release notes.
    pub fn notes_url(&self) -> &str {
        &self.notes_url
    }
}

#[derive(Deserialize)]
struct Manifest {
    version: String,
    image: String,
    digest: String,
    published: String,
    notes_url: String,
}

/// Verifies `signature` (the `.sig` file) over `manifest` with each key in turn, then parses and
/// checks every field. Only verified bytes are parsed.
pub fn verify_release(
    keys: &TrustedKeys,
    manifest: &[u8],
    signature: &[u8],
) -> Result<Release, UpdateError> {
    let sig = decode_signature(signature).ok_or(UpdateError::SignatureFormat)?;
    // A key whose id differs from the signature's is skipped; the first key that owns the id
    // decides, and a legacy (non-prehashed) signature is refused like a bad one.
    let mut verified = false;
    for (_, key) in &keys.keys {
        match key.verify(manifest, &sig, false) {
            Ok(()) => {
                verified = true;
                break;
            }
            Err(minisign_verify::Error::UnexpectedKeyId) => {}
            Err(_) => return Err(UpdateError::BadSignature),
        }
    }
    if !verified {
        return Err(UpdateError::UnknownKey);
    }
    parse_manifest(manifest)
}

// The `.sig` file is the minisign signature box, base64-encoded on one line.
fn decode_signature(signature: &[u8]) -> Option<minisign_verify::Signature> {
    let bytes = STANDARD.decode(signature.trim_ascii()).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    minisign_verify::Signature::decode(&text).ok()
}

fn parse_manifest(bytes: &[u8]) -> Result<Release, UpdateError> {
    let bad = |field| UpdateError::Manifest { field };
    let doc: Manifest = serde_json::from_slice(bytes).map_err(|_| bad("json"))?;
    semver::Version::parse(&doc.version).map_err(|_| bad("version"))?;
    if !is_sha256_digest(&doc.digest) {
        return Err(bad("digest"));
    }
    if doc.image != IMAGE {
        return Err(bad("image"));
    }
    let published = clock::normalize_rfc3339(&doc.published).ok_or(bad("published"))?;
    let notes = url::Url::parse(&doc.notes_url).map_err(|_| bad("notes_url"))?;
    if notes.scheme() != "https" {
        return Err(bad("notes_url"));
    }
    Ok(Release {
        version: doc.version,
        image_sha: doc.digest,
        published,
        notes_url: doc.notes_url,
    })
}

fn is_sha256_digest(digest: &str) -> bool {
    digest.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    })
}
