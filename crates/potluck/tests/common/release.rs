//! Release fixtures for the update checker: a minisign key made at test time and what it signs.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use potluck::update::TrustedKeys;

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
}
