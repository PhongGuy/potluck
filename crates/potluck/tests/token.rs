//! Token minting, shape, digest and the redacting Debug.

use std::collections::HashSet;

use potluck::auth::token::{MintedToken, PREFIX_LEN, TOKEN_LEN, digest, digests_match, prefix_of};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn a_minted_token_has_the_contract_shape() {
    let t = MintedToken::mint().unwrap();
    let secret = t.secret();
    assert_eq!(secret.len(), TOKEN_LEN, "{secret}");
    assert!(secret.starts_with("plk_"), "{secret}");
    assert!(
        secret[4..]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "base64url alphabet, no padding: {secret}"
    );
    assert_eq!(t.prefix(), &secret[..PREFIX_LEN]);
    assert_eq!(t.digest(), &digest(secret));
}

#[test]
fn tokens_are_distinct() {
    let set: HashSet<String> = (0..64)
        .map(|_| MintedToken::mint().unwrap().secret().to_owned())
        .collect();
    assert_eq!(set.len(), 64);
}

#[test]
fn digest_is_sha256_of_the_text() {
    // printf 'plk_test' | sha256sum
    assert_eq!(
        hex(&digest("plk_test")),
        "d3c4083a282aa947c0aa54b54bfd7e47ec02823074c935c3da37099f7d8cfadb"
    );
}

#[test]
fn prefix_of_accepts_only_the_token_shape() {
    let t = MintedToken::mint().unwrap();
    let body = &t.secret()[4..];
    assert_eq!(prefix_of(t.secret()), Some(t.prefix()));
    for bad in [
        String::new(),
        "plk_".to_owned(),
        "plk_short".to_owned(),
        format!("PLK_{body}"),
        format!("ghp_{body}"),
        format!("{}=", &t.secret()[..46]),
        format!("{} ", &t.secret()[..46]),
        format!("{}x", t.secret()),
    ] {
        assert_eq!(prefix_of(&bad), None, "{bad:?}");
    }
}

#[test]
fn digests_match_compares_whole_digests() {
    let a = digest("plk_a");
    let b = digest("plk_b");
    assert!(digests_match(&a, &a));
    assert!(!digests_match(&a, &b));
}

#[test]
fn debug_shows_the_prefix_and_never_the_secret() {
    let t = MintedToken::mint().unwrap();
    let shown = format!("{t:?}");
    assert!(shown.contains(t.prefix()), "{shown}");
    assert!(!shown.contains(&t.secret()[PREFIX_LEN..]), "{shown}");
}
