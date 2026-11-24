//! The two in-memory stores: lifetimes, single use, the cap and what they keep.

use std::time::{Duration, Instant};

use potluck::oidc::{
    HandoffStore, Loopback, MAX_ENTRIES, PendingSignIn, PendingStore, Taken, Timings,
};

fn pending(n: u32) -> PendingSignIn {
    PendingSignIn::new(
        format!("app-state-{n:08}"),
        "c".repeat(43),
        49152,
        Loopback::V4,
        format!("verifier-{n}"),
        format!("nonce-{n}"),
    )
}

const MIN: Duration = Duration::from_secs(60);

#[test]
fn a_state_is_43_base64url_characters_and_unique() {
    let store = PendingStore::new(Timings::default());
    let now = Instant::now();
    let a = store.insert_at(now, pending(1));
    let b = store.insert_at(now, pending(2));
    assert_ne!(a, b);
    for s in [&a, &b] {
        assert_eq!(s.len(), 43);
        assert!(
            s.bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'),
            "{s}"
        );
    }
}

#[test]
fn a_pending_sign_in_is_fresh_then_expired_then_gone_and_single_use() {
    let store = PendingStore::new(Timings::default());
    let t0 = Instant::now();
    let s = store.insert_at(t0, pending(1));
    match store.take_at(t0 + 14 * MIN, &s) {
        Some(Taken::Fresh(entry)) => assert_eq!(entry.app_state, "app-state-00000001"),
        _ => panic!("fresh within 15 minutes"),
    }
    assert!(store.take_at(t0 + 14 * MIN, &s).is_none(), "single use");
    let s = store.insert_at(t0, pending(2));
    assert!(matches!(
        store.take_at(t0 + 16 * MIN, &s),
        Some(Taken::Expired(_))
    ));
    let s = store.insert_at(t0, pending(3));
    assert!(
        store.take_at(t0 + 31 * MIN, &s).is_none(),
        "gone after 30 minutes"
    );
    assert!(store.take_at(t0, "unknown").is_none());
}

#[test]
fn a_handoff_code_lives_sixty_seconds_and_burns_on_the_first_take() {
    let store = HandoffStore::new(Timings::default());
    let t0 = Instant::now();
    let code = store.insert_at(t0, 7, "c".repeat(43));
    assert_eq!(code.len(), 43);
    let got = store.take_at(t0 + Duration::from_secs(59), &code).unwrap();
    assert_eq!(
        (got.user_id, got.challenge.as_str()),
        (7, "c".repeat(43).as_str())
    );
    assert!(store.take_at(t0 + Duration::from_secs(59), &code).is_none());
    let late = store.insert_at(t0, 7, "c".repeat(43));
    assert!(store.take_at(t0 + Duration::from_secs(61), &late).is_none());
    assert_eq!(store.len(), 0, "an expired take still removes");
}

#[test]
fn both_stores_cap_at_max_entries_evicting_the_oldest() {
    let pending_store = PendingStore::new(Timings::default());
    let handoffs = HandoffStore::new(Timings::default());
    let t0 = Instant::now();
    let first = pending_store.insert_at(t0, pending(0));
    let first_code = handoffs.insert_at(t0, 0, "c".repeat(43));
    for i in 1..=MAX_ENTRIES as u32 {
        let at = t0 + Duration::from_millis(u64::from(i));
        pending_store.insert_at(at, pending(i));
        handoffs.insert_at(at, i64::from(i), "c".repeat(43));
    }
    assert_eq!(pending_store.len(), MAX_ENTRIES);
    assert_eq!(handoffs.len(), MAX_ENTRIES);
    assert!(
        pending_store
            .take_at(t0 + Duration::from_secs(20), &first)
            .is_none(),
        "oldest evicted"
    );
    assert!(
        handoffs
            .take_at(t0 + Duration::from_secs(20), &first_code)
            .is_none()
    );
}

#[test]
fn expired_entries_go_before_live_ones_at_the_cap() {
    let store = HandoffStore::new(Timings::default());
    let t0 = Instant::now();
    for i in 0..MAX_ENTRIES as i64 {
        store.insert_at(t0, i, "c".repeat(43));
    }
    let later = t0 + Duration::from_secs(120);
    let live = store.insert_at(later, 1, "c".repeat(43));
    assert_eq!(store.len(), 1, "every expired entry was dropped");
    assert!(store.take_at(later, &live).is_some());
}

#[test]
fn custom_timings_shorten_every_lifetime() {
    let t = Timings::new(
        Duration::from_millis(100),
        Duration::from_millis(200),
        Duration::from_millis(50),
    );
    let store = PendingStore::new(t);
    let t0 = Instant::now();
    let s = store.insert_at(t0, pending(1));
    assert!(matches!(
        store.take_at(t0 + Duration::from_millis(150), &s),
        Some(Taken::Expired(_))
    ));
}
