//! What the OIDC routes share: the provider, the two in-memory stores and their rate limiters.

use std::sync::Arc;
use std::time::Duration;

use crate::auth::rate_limit::RateLimiter;

use super::{HandoffStore, PendingStore, Provider, Timings};

/// Sign-in starts and code redemptions allowed per address per minute by default.
const DEFAULT_PER_MINUTE: u32 = 10;
const MINUTE: Duration = Duration::from_secs(60);

/// The configured provider with its pending sign-ins, handoff codes and rate limiters.
/// Clones share every part.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct OidcState {
    provider: Arc<Provider>,
    pending: Arc<PendingStore>,
    handoffs: Arc<HandoffStore>,
    start_limiter: Arc<RateLimiter>,
    redeem_limiter: Arc<RateLimiter>,
}

impl OidcState {
    /// Both limiters at 10 per minute per address.
    pub fn new(provider: Provider, timings: Timings) -> OidcState {
        OidcState {
            provider: Arc::new(provider),
            pending: Arc::new(PendingStore::new(timings)),
            handoffs: Arc::new(HandoffStore::new(timings)),
            start_limiter: Arc::new(RateLimiter::new(DEFAULT_PER_MINUTE, MINUTE)),
            redeem_limiter: Arc::new(RateLimiter::new(DEFAULT_PER_MINUTE, MINUTE)),
        }
    }

    /// Other per-minute budgets for start and redeem; tests that are not about limits.
    #[must_use]
    pub fn with_rate_limits(mut self, start_per_min: u32, redeem_per_min: u32) -> OidcState {
        self.start_limiter = Arc::new(RateLimiter::new(start_per_min, MINUTE));
        self.redeem_limiter = Arc::new(RateLimiter::new(redeem_per_min, MINUTE));
        self
    }

    /// The provider.
    pub(crate) fn provider(&self) -> &Provider {
        &self.provider
    }

    /// Sign-ins sent to the provider and not yet called back.
    pub(crate) fn pending(&self) -> &PendingStore {
        &self.pending
    }

    /// Handoff codes waiting for the app to redeem them.
    pub(crate) fn handoffs(&self) -> &HandoffStore {
        &self.handoffs
    }

    /// The per-address limiter of `/oidc/start`.
    pub(crate) fn start_limiter(&self) -> &RateLimiter {
        &self.start_limiter
    }

    /// The per-address limiter of `/oidc/redeem`.
    #[allow(dead_code, reason = "read by the OIDC routes")]
    pub(crate) fn redeem_limiter(&self) -> &RateLimiter {
        &self.redeem_limiter
    }
}
