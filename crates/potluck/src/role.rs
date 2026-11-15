//! Workspace roles, ordered so that comparing two roles is the permission check.

/// A workspace role; `read < write < admin`, so `Ord` is the permission check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Role {
    /// Clone and fetch.
    Read,
    /// Also push.
    Write,
    /// Also manage grants and overrides.
    Admin,
}

impl Role {
    /// `read`, `write` or `admin`.
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Read => "read",
            Role::Write => "write",
            Role::Admin => "admin",
        }
    }

    /// `read`, `write` or `admin`; `None` for anything else.
    pub fn parse(s: &str) -> Option<Role> {
        [Role::Read, Role::Write, Role::Admin]
            .into_iter()
            .find(|role| role.as_str() == s)
    }
}
