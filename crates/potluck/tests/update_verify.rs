//! Verifying a fetched release: the minisign signature against the trusted keys, then every
//! manifest field.

use potluck::update::{Release, TrustedKeys, UpdateError, verify_release};
use serde_json::{Value, json};

use crate::common::release::{DIGEST, TestKey, manifest};

fn signed(key: &TestKey, body: &[u8]) -> Result<Release, UpdateError> {
    verify_release(&key.trusted(), body, &key.sign(body))
}

#[test]
fn a_signed_manifest_becomes_a_release() {
    let key = TestKey::new();
    let release = signed(&key, &manifest("1.2.0")).unwrap();
    assert_eq!(release.version(), "1.2.0");
    assert_eq!(release.digest(), DIGEST);
    assert_eq!(release.published(), "2026-11-20T10:00:00Z");
    assert_eq!(
        release.notes_url(),
        "https://github.com/PhongGuy/potluck/releases/tag/v1.2.0"
    );
}

#[test]
fn unknown_fields_are_ignored_and_published_is_normalised_to_utc() {
    let key = TestKey::new();
    let mut doc: Value = serde_json::from_slice(&manifest("1.2.0")).unwrap();
    doc["published"] = json!("2026-11-20T12:30:00+02:00");
    doc["channel"] = json!("stable");
    let release = signed(&key, doc.to_string().as_bytes()).unwrap();
    assert_eq!(release.published(), "2026-11-20T10:30:00Z");
}

#[test]
fn any_listed_key_may_sign() {
    let (a, b) = (TestKey::new(), TestKey::new());
    let keys = TrustedKeys::parse(&format!("{}\n{}\n", a.keys_line(), b.keys_line())).unwrap();
    let body = manifest("1.2.0");
    assert!(verify_release(&keys, &body, &b.sign(&body)).is_ok());
}

#[test]
fn a_key_not_in_the_list_is_an_unknown_key() {
    let (trusted, other) = (TestKey::new(), TestKey::new());
    let body = manifest("1.2.0");
    assert_eq!(
        verify_release(&trusted.trusted(), &body, &other.sign(&body)),
        Err(UpdateError::UnknownKey)
    );
}

#[test]
fn changed_bytes_do_not_verify() {
    let key = TestKey::new();
    let sig = key.sign(&manifest("1.2.0"));
    assert_eq!(
        verify_release(&key.trusted(), &manifest("9.9.9"), &sig),
        Err(UpdateError::BadSignature)
    );
}

#[test]
fn a_sig_file_that_is_not_a_signature_box_is_a_format_error() {
    let key = TestKey::new();
    let body = manifest("1.2.0");
    for sig in [&b"%%% not base64"[..], b"aGVsbG8K\n", b""] {
        assert_eq!(
            verify_release(&key.trusted(), &body, sig),
            Err(UpdateError::SignatureFormat),
            "{sig:?}"
        );
    }
}

#[test]
fn each_malformed_field_is_named_even_when_signed() {
    let key = TestKey::new();
    let upper = format!("sha256:{}", DIGEST["sha256:".len()..].to_uppercase());
    let sha512 = format!("sha512:{}", &DIGEST["sha256:".len()..]);
    let cases: [(&str, Value, &'static str); 9] = [
        ("version", json!("1.2"), "version"),
        ("version", json!(12), "json"),
        ("digest", json!(upper), "digest"),
        ("digest", json!("sha256:abc"), "digest"),
        ("digest", json!(sha512), "digest"),
        ("image", json!("ghcr.io/someone/potluck"), "image"),
        ("published", json!("yesterday"), "published"),
        (
            "notes_url",
            json!("http://github.com/PhongGuy/potluck/releases"),
            "notes_url",
        ),
        ("notes_url", json!("not a url"), "notes_url"),
    ];
    for (name, value, field) in cases {
        let mut doc: Value = serde_json::from_slice(&manifest("1.2.0")).unwrap();
        doc[name] = value.clone();
        assert_eq!(
            signed(&key, doc.to_string().as_bytes()),
            Err(UpdateError::Manifest { field }),
            "{name} = {value}"
        );
    }
    assert_eq!(
        signed(&key, b"not json"),
        Err(UpdateError::Manifest { field: "json" })
    );
    let mut missing: Value = serde_json::from_slice(&manifest("1.2.0")).unwrap();
    missing.as_object_mut().unwrap().remove("digest");
    assert_eq!(
        signed(&key, missing.to_string().as_bytes()),
        Err(UpdateError::Manifest { field: "json" })
    );
}

#[test]
fn verification_does_not_judge_the_version_order() {
    let key = TestKey::new();
    for v in ["0.0.1", "2.0.0-rc.1", "1.2.0+build.5"] {
        assert_eq!(signed(&key, &manifest(v)).unwrap().version(), v);
    }
}
