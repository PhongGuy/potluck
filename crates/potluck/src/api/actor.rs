//! The identity a request acts as.

use crate::db::accounts::UserRecord;

/// Who a request acts as. Requests over the admin socket are `System`; TCP requests are
/// `Anonymous` unless they carry a valid token, which makes them `User`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Actor {
    /// An unauthenticated request over TCP.
    Anonymous,
    /// A request over the admin socket, trusted as the server operator.
    System,
    /// A request carrying a valid token.
    User(AuthUser),
}

impl Actor {
    /// The signed-in user, if any.
    pub fn user(&self) -> Option<&AuthUser> {
        match self {
            Actor::User(user) => Some(user),
            Actor::Anonymous | Actor::System => None,
        }
    }

    /// `System`, or a user with `server_admin`.
    pub fn is_server_admin(&self) -> bool {
        match self {
            Actor::System => true,
            Actor::User(user) => user.user.server_admin(),
            Actor::Anonymous => false,
        }
    }
}

/// The user a token resolved to, and which token it was. `Debug` goes through
/// `UserRecord`'s, which redacts the password hash.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AuthUser {
    user: UserRecord,
    token_id: i64,
}

impl AuthUser {
    /// Pairs `user` with the id of the token that authenticated it.
    pub fn new(user: UserRecord, token_id: i64) -> Self {
        AuthUser { user, token_id }
    }

    /// The account.
    pub fn user(&self) -> &UserRecord {
        &self.user
    }

    /// The token's row id.
    pub fn token_id(&self) -> i64 {
        self.token_id
    }
}
