//! argon2id hashing and verification, and the RFC 3339 clock.

use potluck::auth::password::{PasswordError, Passwords};
use potluck::clock::{normalize_rfc3339, now_rfc3339, shift_secs};

fn fast() -> Passwords {
    Passwords::with_cost(1024, 1, 1).unwrap()
}

#[test]
fn default_cost_is_the_spec_s_argon2id_parameters() {
    let phc = Passwords::new().hash("correct horse").unwrap();
    assert!(phc.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"), "{phc}");
    assert!(Passwords::new().verify("correct horse", &phc).unwrap());
    assert_eq!(Passwords::default(), Passwords::new());
}

#[test]
fn a_hash_verifies_and_a_wrong_password_does_not() {
    let p = fast();
    let phc = p.hash("s3cret-pw").unwrap();
    assert!(phc.starts_with("$argon2id$v=19$m=1024,t=1,p=1$"), "{phc}");
    assert!(p.verify("s3cret-pw", &phc).unwrap());
    assert!(!p.verify("s3cret-pW", &phc).unwrap());
    assert!(!p.verify("", &phc).unwrap());
}

#[test]
fn hashing_the_same_password_twice_gives_different_strings() {
    let p = fast();
    assert_ne!(
        p.hash("pw").unwrap(),
        p.hash("pw").unwrap(),
        "a fresh salt each time"
    );
}

#[test]
fn verification_takes_the_cost_from_the_stored_hash() {
    let phc = fast().hash("pw").unwrap();
    assert!(
        Passwords::new().verify("pw", &phc).unwrap(),
        "a default verifier accepts a test-cost hash"
    );
}

#[test]
fn a_malformed_stored_hash_is_an_error_not_a_mismatch() {
    for bad in [
        "not-a-phc-string",
        "",
        "$nope$v=19$m=1024,t=1,p=1$c2FsdHNhbHQ$aGFzaGhhc2g",
    ] {
        let err = fast().verify("pw", bad).unwrap_err();
        assert!(
            matches!(err, PasswordError::MalformedHash),
            "{bad:?}: got {err:?}"
        );
    }
}

#[test]
fn a_cost_below_argon2_s_minimum_is_refused() {
    for (m, t, p) in [(1, 1, 1), (1024, 0, 1), (1024, 1, 0)] {
        let err = Passwords::with_cost(m, t, p).unwrap_err();
        assert!(
            matches!(err, PasswordError::InvalidCost { .. }),
            "({m},{t},{p}): got {err:?}"
        );
    }
}

#[test]
fn now_is_rfc3339_utc_to_the_second() {
    let now = now_rfc3339();
    assert_eq!(now.len(), 20, "{now}");
    assert!(now.ends_with('Z') && now.as_bytes()[10] == b'T', "{now}");
    assert_eq!(
        normalize_rfc3339(&now).as_deref(),
        Some(now.as_str()),
        "already normal"
    );
    assert!(now.as_str() > "2026-01-01T00:00:00Z", "{now}");
}

#[test]
fn normalize_converts_offsets_and_drops_fractions() {
    assert_eq!(
        normalize_rfc3339("2026-11-15T12:30:00+02:00").as_deref(),
        Some("2026-11-15T10:30:00Z")
    );
    assert_eq!(
        normalize_rfc3339("2026-11-15T10:30:00.999Z").as_deref(),
        Some("2026-11-15T10:30:00Z")
    );
    assert_eq!(
        normalize_rfc3339("2026-11-15T10:30:00-00:00").as_deref(),
        Some("2026-11-15T10:30:00Z")
    );
    for bad in [
        "2026-11-15",
        "yesterday",
        "",
        "2026-13-01T00:00:00Z",
        "2026-11-15T10:30:00",
    ] {
        assert_eq!(normalize_rfc3339(bad), None, "{bad:?}");
    }
}

#[test]
fn normalized_strings_order_chronologically() {
    let earlier = normalize_rfc3339("2026-11-15T23:59:59+01:00").unwrap();
    let later = normalize_rfc3339("2026-11-15T23:00:00Z").unwrap();
    assert!(earlier < later, "{earlier} < {later}");
}

#[test]
fn shift_moves_whole_seconds_across_boundaries() {
    assert_eq!(
        shift_secs("2026-11-15T10:00:00Z", -60).as_deref(),
        Some("2026-11-15T09:59:00Z")
    );
    assert_eq!(
        shift_secs("2026-12-31T23:59:30Z", 45).as_deref(),
        Some("2027-01-01T00:00:15Z")
    );
    assert_eq!(shift_secs("nope", 1), None);
}
