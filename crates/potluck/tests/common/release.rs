//! Release fixtures for the update checker: a minisign key made at test time and what it signs.

use std::io::Cursor;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use potluck::update::TrustedKeys;

/// A well-formed digest for test manifests.
pub const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// A well-formed manifest for `version`, as `release.sh manifest` writes it.
pub fn manifest(version: &str) -> Vec<u8> {
    serde_json::json!({
        "version": version,
        "image": "ghcr.io/phongguy/potluck",
        "digest": DIGEST,
        "published": "2026-11-20T10:00:00Z",
        "notes_url": format!("https://github.com/PhongGuy/potluck/releases/tag/v{version}"),
    })
    .to_string()
    .into_bytes()
}

/// A fresh unencrypted minisign key pair.
pub struct TestKey {
    pair: minisign::KeyPair,
    line: String,
}

impl TestKey {
    pub fn new() -> TestKey {
        let pair = minisign::KeyPair::generate_unencrypted_keypair().expect("keypair");
        let text = pair.pk.to_box().expect("public key box").into_string();
        TestKey {
            line: STANDARD.encode(text),
            pair,
        }
    }

    /// The key as one line of `update-keys.txt`: the `.pub` file's text, base64.
    pub fn keys_line(&self) -> &str {
        &self.line
    }

    /// The key id as minisign prints it: the key number little-endian, uppercase hex.
    pub fn key_id(&self) -> String {
        self.pair
            .pk
            .keynum()
            .iter()
            .rev()
            .map(|b| format!("{b:02X}"))
            .collect()
    }

    /// A key list holding only this key.
    pub fn trusted(&self) -> TrustedKeys {
        TrustedKeys::parse(&self.line).expect("test key parses")
    }

    /// The `.sig` file for `bytes`: the prehashed minisign signature box, base64 on one line.
    pub fn sign(&self, bytes: &[u8]) -> Vec<u8> {
        let sig = minisign::sign(None, &self.pair.sk, Cursor::new(bytes.to_vec()), None, None)
            .expect("sign");
        format!("{}\n", STANDARD.encode(sig.into_string())).into_bytes()
    }
}
