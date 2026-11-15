//! Fixed-window rate limiting in memory, bounded in keys and bytes.

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Bytes of a key that count; longer keys are cut at a character boundary.
pub const MAX_KEY_LEN: usize = 64;
/// The hard cap on keys held at once.
pub const MAX_KEYS: usize = 10_000;

/// A fixed-window counter per key, in memory: `limit` attempts per `window`, then refusals until
/// the window ends. Refused attempts are not counted. At most `MAX_KEYS` keys of at most
/// `MAX_KEY_LEN` bytes are held; at the cap the window with the earliest end is evicted.
#[derive(Debug)]
#[non_exhaustive]
pub struct RateLimiter {
    limit: u32,
    window: Duration,
    state: Mutex<State>,
}

// Every window has the same length, so insertion order in `order` is also end order. An
// `order` entry whose start no longer matches the map is stale and is skipped.
#[derive(Debug, Default)]
struct State {
    windows: HashMap<String, Window>,
    order: VecDeque<(String, Instant)>,
}

#[derive(Debug, Clone, Copy)]
struct Window {
    start: Instant,
    count: u32,
}

impl State {
    fn is_live(&self, key: &str, start: Instant) -> bool {
        self.windows.get(key).is_some_and(|w| w.start == start)
    }
}

impl RateLimiter {
    /// `limit` attempts per `window`.
    pub fn new(limit: u32, window: Duration) -> Self {
        Self {
            limit,
            window,
            state: Mutex::new(State::default()),
        }
    }

    /// Counts an attempt now; `Err(retry_after_secs)`, at least 1, once the key's window is full.
    pub fn check(&self, key: &str) -> Result<(), u64> {
        self.check_at(key, Instant::now())
    }

    /// `check` at `now`, for tests.
    pub fn check_at(&self, key: &str, now: Instant) -> Result<(), u64> {
        let key = truncate(key);
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);

        // A live window counts or refuses; an ended one is dropped and the key starts afresh.
        if let Some(w) = state.windows.get_mut(key) {
            let end = w.start + self.window;
            if end > now {
                if w.count < self.limit {
                    w.count += 1;
                    return Ok(());
                }
                let rest = end.duration_since(now);
                let secs = rest.as_secs() + u64::from(rest.subsec_nanos() > 0);
                return Err(secs.max(1));
            }
            state.windows.remove(key);
        }

        // At the cap, evict the earliest live window; stale log entries are consumed on the way.
        if state.windows.len() >= MAX_KEYS {
            while let Some((old, start)) = state.order.pop_front() {
                if state.is_live(&old, start) {
                    state.windows.remove(&old);
                    break;
                }
            }
        }

        state.windows.insert(
            key.to_owned(),
            Window {
                start: now,
                count: 1,
            },
        );
        state.order.push_back((key.to_owned(), now));
        // Resets leave stale log entries behind; compact once they double the cap, so the log
        // stays bounded at O(1) amortised cost.
        if state.order.len() > 2 * MAX_KEYS {
            let State { windows, order } = &mut *state;
            order.retain(|(k, s)| windows.get(k).is_some_and(|w| w.start == *s));
        }
        Ok(())
    }

    /// Keys currently held; never above `MAX_KEYS`.
    pub fn len(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .windows
            .len()
    }

    /// Whether no key is held.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// Cut to at most `MAX_KEY_LEN` bytes, backing off to the last character boundary.
fn truncate(key: &str) -> &str {
    if key.len() <= MAX_KEY_LEN {
        return key;
    }
    let mut end = MAX_KEY_LEN;
    while !key.is_char_boundary(end) {
        end -= 1;
    }
    &key[..end]
}

/// The `POST /session` limiters, also guarding `current_password` checks.
#[derive(Debug)]
#[non_exhaustive]
pub struct SessionLimits {
    per_ip: RateLimiter,
    per_username: RateLimiter,
}

impl SessionLimits {
    /// `per_ip` and `per_username` attempts per `window`.
    pub fn new(per_ip: u32, per_username: u32, window: Duration) -> Self {
        Self {
            per_ip: RateLimiter::new(per_ip, window),
            per_username: RateLimiter::new(per_username, window),
        }
    }

    /// The limiter keyed by `ClientIp::key`.
    pub fn per_ip(&self) -> &RateLimiter {
        &self.per_ip
    }

    /// The limiter keyed by username.
    pub fn per_username(&self) -> &RateLimiter {
        &self.per_username
    }
}

impl Default for SessionLimits {
    /// Ten attempts per client address and five per username, per minute.
    fn default() -> Self {
        Self::new(10, 5, Duration::from_secs(60))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every reset of a held key leaves a stale log entry; compaction must keep the log bounded
    // even when the map never grows.
    #[test]
    fn the_order_log_stays_bounded_under_resets() {
        let l = RateLimiter::new(1, Duration::from_secs(1));
        let t0 = Instant::now();
        for i in 0..(3 * MAX_KEYS as u64) {
            l.check_at("a", t0 + Duration::from_secs(i)).unwrap();
            let log = l.state.lock().unwrap().order.len();
            assert!(
                log <= 2 * MAX_KEYS + 1,
                "log grew to {log} after {i} resets"
            );
        }
    }
}
