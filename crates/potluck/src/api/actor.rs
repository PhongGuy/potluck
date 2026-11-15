//! The identity a request acts as.

/// Who a request acts as. Requests over the admin socket are `System`; TCP requests are
/// `Anonymous` until a later plan adds token auth.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Actor {
    /// An unauthenticated request over TCP.
    Anonymous,
    /// A request over the admin socket, trusted as the server operator.
    System,
}
