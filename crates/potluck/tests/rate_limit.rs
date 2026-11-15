//! The fixed-window limiter, its key and memory bounds, and the session defaults.

use std::time::{Duration, Instant};

use potluck::auth::rate_limit::{MAX_KEY_LEN, MAX_KEYS, RateLimiter, SessionLimits};

#[test]
fn the_limit_is_per_key_within_the_window() {
    let l = RateLimiter::new(3, Duration::from_secs(60));
    let t0 = Instant::now();
    for i in 0..3 {
        assert_eq!(
            l.check_at("a", t0 + Duration::from_secs(i)),
            Ok(()),
            "attempt {i}"
        );
    }
    assert_eq!(
        l.check_at("a", t0 + Duration::from_secs(10)),
        Err(50),
        "the rest of the window"
    );
    assert_eq!(l.check_at("a", t0 + Duration::from_secs(59)), Err(1));
    assert_eq!(
        l.check_at("b", t0 + Duration::from_secs(10)),
        Ok(()),
        "another key is independent"
    );
}

#[test]
fn the_window_resets_and_refusals_do_not_extend_it() {
    let l = RateLimiter::new(1, Duration::from_secs(60));
    let t0 = Instant::now();
    l.check_at("a", t0).unwrap();
    assert!(l.check_at("a", t0 + Duration::from_secs(59)).is_err());
    assert_eq!(
        l.check_at("a", t0 + Duration::from_secs(60)),
        Ok(()),
        "a new window"
    );
    assert!(l.check_at("a", t0 + Duration::from_secs(61)).is_err());
}

#[test]
fn retry_after_is_at_least_one_second() {
    let l = RateLimiter::new(1, Duration::from_millis(500));
    let t0 = Instant::now();
    l.check_at("a", t0).unwrap();
    assert_eq!(l.check_at("a", t0 + Duration::from_millis(100)), Err(1));
}

#[test]
fn check_uses_the_real_clock() {
    let l = RateLimiter::new(2, Duration::from_secs(60));
    assert!(l.check("k").is_ok());
    assert!(l.check("k").is_ok());
    assert!(l.check("k").is_err());
}

#[test]
fn long_keys_are_truncated() {
    let l = RateLimiter::new(1, Duration::from_secs(60));
    let t0 = Instant::now();
    let head = "k".repeat(MAX_KEY_LEN);
    l.check_at(&format!("{head}-one"), t0).unwrap();
    assert!(
        l.check_at(&format!("{head}-two"), t0).is_err(),
        "the same first 64 bytes are one key"
    );
    assert_eq!(l.len(), 1);
    let multibyte = format!("{}é", "x".repeat(MAX_KEY_LEN - 1));
    l.check_at(&multibyte, t0).unwrap();
    assert_eq!(l.len(), 2, "a key cut inside a character still counts");
}

#[test]
fn at_the_cap_the_earliest_window_is_evicted() {
    let l = RateLimiter::new(1, Duration::from_secs(60));
    let t0 = Instant::now();
    for i in 0..MAX_KEYS {
        l.check_at(&format!("k{i}"), t0 + Duration::from_millis(i as u64))
            .unwrap();
    }
    assert_eq!(l.len(), MAX_KEYS, "at the cap");
    let now = t0 + Duration::from_secs(30);
    assert_eq!(
        l.check_at("one-more", now),
        Ok(()),
        "a new key is always taken"
    );
    assert_eq!(l.len(), MAX_KEYS, "k0, the earliest window, was evicted");
    assert_eq!(
        l.check_at("k0", now),
        Ok(()),
        "k0 starts a fresh window instead of being limited"
    );
    assert_eq!(l.len(), MAX_KEYS, "which evicted k1");
    assert!(
        l.check_at("k9999", now).is_err(),
        "a kept window is still limited"
    );
    assert!(!l.is_empty());
}

#[test]
fn an_ended_window_is_replaced_without_eviction_of_others() {
    let l = RateLimiter::new(1, Duration::from_secs(60));
    let t0 = Instant::now();
    for i in 0..MAX_KEYS {
        l.check_at(&format!("k{i}"), t0).unwrap();
    }
    let later = t0 + Duration::from_secs(61);
    assert_eq!(
        l.check_at("k5000", later),
        Ok(()),
        "its own window ended: reset in place"
    );
    assert_eq!(l.len(), MAX_KEYS, "no eviction was needed");
    assert_eq!(
        l.check_at("k4999", later),
        Ok(()),
        "k4999 kept its (ended) window until now"
    );
    for i in 0..(2 * MAX_KEYS) {
        l.check_at(
            &format!("k{}", i % MAX_KEYS),
            later + Duration::from_secs(61 + (i / MAX_KEYS) as u64 * 61),
        )
        .unwrap();
    }
    assert_eq!(
        l.len(),
        MAX_KEYS,
        "resets never grow the map; the order log is compacted along the way"
    );
}

#[test]
fn session_defaults_are_ten_per_address_and_five_per_username() {
    let s = SessionLimits::default();
    for _ in 0..10 {
        s.per_ip().check("ip:x").unwrap();
    }
    assert!(s.per_ip().check("ip:x").is_err());
    for _ in 0..5 {
        s.per_username().check("bob").unwrap();
    }
    assert!(s.per_username().check("bob").is_err());
}
