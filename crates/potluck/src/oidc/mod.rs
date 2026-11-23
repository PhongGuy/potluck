//! OpenID Connect sign-in: the pieces Potluck needs to talk to one identity provider.
//! No openidconnect or reqwest type appears in this module's public surface.

mod http;
mod settings;

pub use http::{IdpHttp, IdpHttpError};
pub use settings::{ClientSecret, OidcSettings};
