//! OpenID Connect sign-in: the pieces Potluck needs to talk to one identity provider.
//! No openidconnect or reqwest type appears in this module's public surface.

mod http;
mod provider;
mod settings;
mod state;
mod store;

pub use http::{IdpHttp, IdpHttpError};
pub use provider::{IdClaims, Provider, ProviderError};
pub use settings::{ClientSecret, OidcSettings};
pub use state::OidcState;
pub use store::{
    Handoff, HandoffStore, Loopback, MAX_ENTRIES, PendingSignIn, PendingStore, Taken, Timings,
};
