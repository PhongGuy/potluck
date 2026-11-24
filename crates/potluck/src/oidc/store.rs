//! In-memory stores for sign-ins in flight: pending sign-ins keyed by `state`, and the
//! single-use handoff codes an app redeems. Both are capped at [`MAX_ENTRIES`].

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

use crate::auth::token::digest;

/// The hard cap on entries each store holds at once.
pub const MAX_ENTRIES: usize = 10_000;

/// How long entries live: a pending sign-in is usable for `pending_use` and remembered as
/// expired until `pending_keep`; a handoff code lives for `handoff`.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct Timings {
    pending_use: Duration,
    pending_keep: Duration,
    handoff: Duration,
}

impl Timings {
    /// Any lifetimes; tests use milliseconds to reach every expiry path.
    pub fn new(pending_use: Duration, pending_keep: Duration, handoff: Duration) -> Timings {
        Timings {
            pending_use,
            pending_keep,
            handoff,
        }
    }
}

impl Default for Timings {
    /// Fifteen minutes to finish a sign-in, kept as expired for thirty, and sixty seconds to
    /// redeem a handoff code.
    fn default() -> Self {
        Timings::new(
            Duration::from_secs(15 * 60),
            Duration::from_secs(30 * 60),
            Duration::from_secs(60),
        )
    }
}

/// Which loopback address the app listens on for the redirect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Loopback {
    /// `127.0.0.1`.
    V4,
    /// `[::1]`.
    V6,
}

/// A sign-in between `/oidc/start` and the callback. No `Debug`: it holds the server's PKCE
/// verifier and nonce.
#[non_exhaustive]
pub struct PendingSignIn {
    /// The app's own `state`, echoed back on its loopback redirect.
    pub app_state: String,
    /// The app's PKCE S256 challenge, checked at redeem.
    pub challenge: String,
    /// The app's loopback port.
    pub port: u16,
    /// The app's loopback address family.
    pub loopback: Loopback,
    /// The server's PKCE verifier for the IdP.
    pub verifier: String,
    /// The nonce the IdP's ID token must carry.
    pub nonce: String,
}

impl PendingSignIn {
    /// Bundles one sign-in's fields.
    pub fn new(
        app_state: String,
        challenge: String,
        port: u16,
        loopback: Loopback,
        verifier: String,
        nonce: String,
    ) -> PendingSignIn {
        PendingSignIn {
            app_state,
            challenge,
            port,
            loopback,
            verifier,
            nonce,
        }
    }
}

/// A pending sign-in taken out of the store.
#[non_exhaustive]
pub enum Taken {
    /// Taken within `pending_use`.
    Fresh(PendingSignIn),
    /// Taken after `pending_use` but within `pending_keep`.
    Expired(PendingSignIn),
}

/// Pending sign-ins keyed by the `state` sent to the IdP.
#[non_exhaustive]
pub struct PendingStore {
    timings: Timings,
    entries: Capped<String, PendingSignIn>,
}

impl PendingStore {
    /// An empty store.
    pub fn new(timings: Timings) -> PendingStore {
        PendingStore {
            timings,
            entries: Capped::new(timings.pending_keep),
        }
    }

    /// Stores the sign-in under a fresh 32-byte state (43 base64url chars) and returns it.
    pub fn insert_at(&self, now: Instant, entry: PendingSignIn) -> String {
        let state = random_secret();
        self.entries.insert(now, state.clone(), entry);
        state
    }

    /// Removes and returns the entry: `Fresh` within `pending_use`, `Expired` within
    /// `pending_keep`, `None` when absent or older.
    pub fn take_at(&self, now: Instant, state: &str) -> Option<Taken> {
        let (age, entry) = self.entries.take(now, state)?;
        Some(if age <= self.timings.pending_use {
            Taken::Fresh(entry)
        } else {
            Taken::Expired(entry)
        })
    }

    /// Entries held; never above [`MAX_ENTRIES`].
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no entry is held.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl std::fmt::Debug for PendingStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingStore")
            .field("len", &self.len())
            .finish_non_exhaustive()
    }
}

/// What a redeemed handoff code yields.
#[derive(Debug)]
#[non_exhaustive]
pub struct Handoff {
    /// The signed-in user.
    pub user_id: i64,
    /// The app's PKCE S256 challenge the redeem's verifier must match.
    pub challenge: String,
}

/// Single-use handoff codes, held only as their SHA-256 digest.
#[non_exhaustive]
pub struct HandoffStore {
    entries: Capped<[u8; 32], Handoff>,
}

impl HandoffStore {
    /// An empty store.
    pub fn new(timings: Timings) -> HandoffStore {
        HandoffStore {
            entries: Capped::new(timings.handoff),
        }
    }

    /// A fresh 32-byte code (43 base64url chars); only its SHA-256 is kept.
    pub fn insert_at(&self, now: Instant, user_id: i64, challenge: String) -> String {
        let code = random_secret();
        self.entries
            .insert(now, digest(&code), Handoff { user_id, challenge });
        code
    }

    /// Removes the entry whatever happens next; `None` when absent or past `handoff`.
    pub fn take_at(&self, now: Instant, code: &str) -> Option<Handoff> {
        self.entries
            .take(now, &digest(code))
            .map(|(_, handoff)| handoff)
    }

    /// Entries held; never above [`MAX_ENTRIES`].
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no entry is held.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl std::fmt::Debug for HandoffStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HandoffStore")
            .field("len", &self.len())
            .finish_non_exhaustive()
    }
}

// 32 bytes from the OS RNG, base64url without padding. The stores' signatures promise a value,
// so an RNG failure, which leaves the server unable to mint anything safely, panics.
#[allow(
    clippy::expect_used,
    reason = "the OS RNG failing is unrecoverable and the store contract returns a plain value"
)]
fn random_secret() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("the OS random number generator works");
    URL_SAFE_NO_PAD.encode(bytes)
}

// A map with a retention and a cap. Keys are random, so insertion order in `order` is age
// order; an `order` entry whose time no longer matches the map was taken and is skipped.
// Each insert first drops expired entries from the front, then evicts the oldest at the cap.
struct Capped<K, V> {
    keep: Duration,
    slots: Mutex<Slots<K, V>>,
}

struct Slots<K, V> {
    map: HashMap<K, (Instant, V)>,
    order: VecDeque<(K, Instant)>,
}

impl<K: Eq + Hash + Clone, V> Capped<K, V> {
    fn new(keep: Duration) -> Self {
        Capped {
            keep,
            slots: Mutex::new(Slots {
                map: HashMap::new(),
                order: VecDeque::new(),
            }),
        }
    }

    fn insert(&self, now: Instant, key: K, value: V) {
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        let Slots { map, order } = &mut *slots;
        while let Some((front, at)) = order.front() {
            let live = map.get(front).is_some_and(|(t, _)| t == at);
            if live && now.saturating_duration_since(*at) <= self.keep {
                break;
            }
            if live {
                map.remove(front);
            }
            order.pop_front();
        }
        if map.len() >= MAX_ENTRIES {
            while let Some((old, at)) = order.pop_front() {
                if map.get(&old).is_some_and(|(t, _)| *t == at) {
                    map.remove(&old);
                    break;
                }
            }
        }
        map.insert(key.clone(), (now, value));
        order.push_back((key, now));
        // Takes leave stale log entries behind; compact once they double the cap.
        if order.len() > 2 * MAX_ENTRIES {
            order.retain(|(k, s)| map.get(k).is_some_and(|(t, _)| t == s));
        }
    }

    // Removes the entry whatever its age; returns it with its age while within `keep`.
    fn take<Q>(&self, now: Instant, key: &Q) -> Option<(Duration, V)>
    where
        K: std::borrow::Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        let (at, value) = slots.map.remove(key)?;
        let age = now.saturating_duration_since(at);
        (age <= self.keep).then_some((age, value))
    }

    fn len(&self) -> usize {
        self.slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .map
            .len()
    }
}
