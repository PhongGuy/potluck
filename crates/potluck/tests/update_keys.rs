//! The trusted update keys and the update source: the embedded list, the keys file format, the
//! base URLs a source accepts, and the failure messages.

use potluck::update::{GITHUB_BASE, TrustedKeys, UpdateError, UpdateSource};

use crate::common::release::TestKey;

#[test]
fn the_embedded_keys_parse_and_hold_the_release_key() {
    let source = UpdateSource::github().expect("the embedded keys parse");
    assert_eq!(source.keys().key_ids(), ["E5C2C7E76E58D79D"]);
    assert_eq!(
        source.base_url(),
        "https://github.com/PhongGuy/potluck/releases/latest/download/"
    );
    assert_eq!(GITHUB_BASE, source.base_url());
}

#[test]
fn parse_skips_comments_and_blank_lines_and_keeps_every_key_in_order() {
    let (a, b) = (TestKey::new(), TestKey::new());
    let text = format!(
        "# trusted\n\n  {}  \n# next\n{}\n",
        a.keys_line(),
        b.keys_line()
    );
    let keys = TrustedKeys::parse(&text).unwrap();
    assert_eq!(keys.key_ids(), [a.key_id(), b.key_id()]);
}

#[test]
fn parse_names_the_line_that_is_not_a_key() {
    let a = TestKey::new();
    let text = format!("{}\n# c\nnot base64!\n", a.keys_line());
    assert!(
        matches!(
            TrustedKeys::parse(&text),
            Err(UpdateError::Keys { line: 3, .. })
        ),
        "line 3 is not base64"
    );
    // base64 of "hello\n": decodes, but is not a minisign public key
    assert!(matches!(
        TrustedKeys::parse("aGVsbG8K\n"),
        Err(UpdateError::Keys { line: 1, .. })
    ));
}

#[test]
fn parse_refuses_a_list_with_no_key() {
    assert_eq!(
        TrustedKeys::parse("# none yet\n\n").unwrap_err(),
        UpdateError::NoKeys
    );
}

#[test]
fn a_source_takes_https_or_loopback_ip_http_only() {
    let keys = TestKey::new().trusted();
    for ok in [
        "https://example.com/dl/",
        "http://127.0.0.1:8080/dl/",
        "http://[::1]:9/",
    ] {
        assert!(
            UpdateSource::new(ok, keys.clone()).is_ok(),
            "{ok} is accepted"
        );
    }
    for bad in [
        "http://example.com/",
        "http://localhost:8080/",
        "ftp://127.0.0.1/",
        "https://example.com/dl/?a=1",
        "https://example.com/dl/#x",
        "https://user:pw@example.com/",
        "not a url",
    ] {
        assert!(
            matches!(
                UpdateSource::new(bad, keys.clone()),
                Err(UpdateError::BaseUrl { .. })
            ),
            "{bad} is refused"
        );
    }
}

#[test]
fn a_base_without_a_trailing_slash_gains_one() {
    let source = UpdateSource::new("http://127.0.0.1:9/dl", TestKey::new().trusted()).unwrap();
    assert_eq!(source.base_url(), "http://127.0.0.1:9/dl/");
}

#[test]
fn failure_messages_name_the_class_only() {
    assert_eq!(
        UpdateError::UnknownKey.to_string(),
        "signature by an unknown key"
    );
    assert_eq!(
        UpdateError::Status { status: 502 }.to_string(),
        "the update source answered HTTP 502"
    );
    assert_eq!(
        UpdateError::Manifest { field: "digest" }.to_string(),
        "the signed manifest has a malformed digest"
    );
}
