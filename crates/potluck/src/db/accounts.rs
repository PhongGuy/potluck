//! The user and token rows of `server.db`, each query one job on the database thread. Password
//! hashes and token digests never reach `Debug` output.

use rusqlite::types::Value;
use rusqlite::{OptionalExtension, Row, params, params_from_iter};

use super::audit::{self, AuditAction, AuditCtx, Entry, TokenOwner, UserCreated, UserUpdated};
use super::{Db, DbError, conflict_on_unique};
use crate::clock;

/// Prints `<redacted>` in place of a secret, so a `Debug` derive-alike can show the field exists.
struct Redacted;

impl std::fmt::Debug for Redacted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

/// What `create_user` inserts; `created_at` is set by the database thread. `Debug` prints the
/// hash as `<redacted>`.
#[derive(Clone)]
#[non_exhaustive]
pub struct NewUser {
    username: String,
    display_name: String,
    email: Option<String>,
    password_hash: Option<String>,
    server_admin: bool,
}

impl NewUser {
    /// A user with no email, no password hash and no server admin flag.
    pub fn new(username: &str, display_name: &str) -> Self {
        NewUser {
            username: username.to_owned(),
            display_name: display_name.to_owned(),
            email: None,
            password_hash: None,
            server_admin: false,
        }
    }

    /// Sets the email.
    pub fn email(mut self, email: &str) -> Self {
        self.email = Some(email.to_owned());
        self
    }

    /// Sets the argon2id PHC string.
    pub fn password_hash(mut self, phc: &str) -> Self {
        self.password_hash = Some(phc.to_owned());
        self
    }

    /// Sets whether the user is a server admin.
    pub fn server_admin(mut self, yes: bool) -> Self {
        self.server_admin = yes;
        self
    }
}

impl std::fmt::Debug for NewUser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewUser")
            .field("username", &self.username)
            .field("display_name", &self.display_name)
            .field("email", &self.email)
            .field(
                "password_hash",
                &self.password_hash.as_ref().map(|_| Redacted),
            )
            .field("server_admin", &self.server_admin)
            .finish()
    }
}

/// A `users` row. `Debug` prints the hash as `<redacted>`.
#[derive(Clone)]
#[non_exhaustive]
pub struct UserRecord {
    id: i64,
    username: String,
    display_name: String,
    email: Option<String>,
    password_hash: Option<String>,
    server_admin: bool,
    disabled: bool,
    created_at: String,
}

impl UserRecord {
    /// The row id.
    pub fn id(&self) -> i64 {
        self.id
    }

    /// The unique login name.
    pub fn username(&self) -> &str {
        &self.username
    }

    /// The name shown to other users.
    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    /// The email, if one is set.
    pub fn email(&self) -> Option<&str> {
        self.email.as_deref()
    }

    /// The argon2id PHC string; `None` for an account without a password.
    pub fn password_hash(&self) -> Option<&str> {
        self.password_hash.as_deref()
    }

    /// Whether the user administers the whole server.
    pub fn server_admin(&self) -> bool {
        self.server_admin
    }

    /// Whether the account is disabled.
    pub fn disabled(&self) -> bool {
        self.disabled
    }

    /// When the row was created, RFC 3339 UTC to the second.
    pub fn created_at(&self) -> &str {
        &self.created_at
    }
}

impl std::fmt::Debug for UserRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserRecord")
            .field("id", &self.id)
            .field("username", &self.username)
            .field("display_name", &self.display_name)
            .field("email", &self.email)
            .field(
                "password_hash",
                &self.password_hash.as_ref().map(|_| Redacted),
            )
            .field("server_admin", &self.server_admin)
            .field("disabled", &self.disabled)
            .field("created_at", &self.created_at)
            .finish()
    }
}

/// Fields to change; an unset field is left alone. `email(None)` clears the email. `Debug` prints
/// a set hash as `Some(<redacted>)`.
#[derive(Clone, Default)]
#[non_exhaustive]
pub struct UserPatch {
    display_name: Option<String>,
    email: Option<Option<String>>,
    server_admin: Option<bool>,
    disabled: Option<bool>,
    password_hash: Option<String>,
}

impl UserPatch {
    /// Changes the display name.
    pub fn display_name(mut self, name: &str) -> Self {
        self.display_name = Some(name.to_owned());
        self
    }

    /// Sets the email, or clears it with `None`.
    pub fn email(mut self, email: Option<&str>) -> Self {
        self.email = Some(email.map(str::to_owned));
        self
    }

    /// Changes the server admin flag.
    pub fn server_admin(mut self, yes: bool) -> Self {
        self.server_admin = Some(yes);
        self
    }

    /// Changes the disabled flag.
    pub fn disabled(mut self, yes: bool) -> Self {
        self.disabled = Some(yes);
        self
    }

    /// Replaces the argon2id PHC string.
    pub fn password_hash(mut self, phc: &str) -> Self {
        self.password_hash = Some(phc.to_owned());
        self
    }

    // The set fields as `column = ?` pairs, in a fixed order, ready for one UPDATE.
    fn assignments(self) -> Vec<(&'static str, Value)> {
        let mut set = Vec::new();
        if let Some(name) = self.display_name {
            set.push(("display_name", Value::Text(name)));
        }
        if let Some(email) = self.email {
            set.push(("email", email.map_or(Value::Null, Value::Text)));
        }
        if let Some(yes) = self.server_admin {
            set.push(("server_admin", Value::Integer(i64::from(yes))));
        }
        if let Some(yes) = self.disabled {
            set.push(("disabled", Value::Integer(i64::from(yes))));
        }
        if let Some(phc) = self.password_hash {
            set.push(("password_hash", Value::Text(phc)));
        }
        set
    }
}

impl std::fmt::Debug for UserPatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserPatch")
            .field("display_name", &self.display_name)
            .field("email", &self.email)
            .field("server_admin", &self.server_admin)
            .field("disabled", &self.disabled)
            .field(
                "password_hash",
                &self.password_hash.as_ref().map(|_| Redacted),
            )
            .finish()
    }
}

pub(super) const USER_COLUMNS: &str =
    "id, username, display_name, email, password_hash, server_admin, disabled, created_at";

/// What `create_token` inserts; `created_at` is set by the database thread. `Debug` prints the
/// digest as `<redacted>`.
#[derive(Clone)]
#[non_exhaustive]
pub struct NewToken {
    user_id: i64,
    name: String,
    prefix: String,
    digest: [u8; 32],
    expires_at: Option<String>,
}

impl NewToken {
    /// A token for `user_id`; `expires_at` is an RFC 3339 UTC stamp or `None` for no expiry.
    pub fn new(
        user_id: i64,
        name: &str,
        prefix: &str,
        digest: [u8; 32],
        expires_at: Option<&str>,
    ) -> Self {
        NewToken {
            user_id,
            name: name.to_owned(),
            prefix: prefix.to_owned(),
            digest,
            expires_at: expires_at.map(str::to_owned),
        }
    }
}

impl std::fmt::Debug for NewToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewToken")
            .field("user_id", &self.user_id)
            .field("name", &self.name)
            .field("prefix", &self.prefix)
            .field("digest", &Redacted)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// Which door minted a token, and so which audit action records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TokenOrigin {
    /// `POST /session`.
    Session,
    /// `POST /tokens` or direct.
    Api,
}

/// A `tokens` row without its digest.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct TokenRecord {
    id: i64,
    user_id: i64,
    name: String,
    prefix: String,
    created_at: String,
    last_used_at: Option<String>,
    expires_at: Option<String>,
}

impl TokenRecord {
    /// The row id.
    pub fn id(&self) -> i64 {
        self.id
    }

    /// The owning user's row id.
    pub fn user_id(&self) -> i64 {
        self.user_id
    }

    /// The label the owner gave the token.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The token's first 8 characters, `plk_` included.
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// When the row was created, RFC 3339 UTC to the second.
    pub fn created_at(&self) -> &str {
        &self.created_at
    }

    /// When the token last authenticated a request, if ever.
    pub fn last_used_at(&self) -> Option<&str> {
        self.last_used_at.as_deref()
    }

    /// When the token stops working; `None` for never.
    pub fn expires_at(&self) -> Option<&str> {
        self.expires_at.as_deref()
    }
}

/// One token sharing a presented prefix, with its digest and owner, for the auth path. `Debug`
/// prints the digest as `<redacted>` and the user through its own redacting `Debug`.
#[derive(Clone)]
#[non_exhaustive]
pub struct TokenCandidate {
    token: TokenRecord,
    digest: [u8; 32],
    user: UserRecord,
}

impl TokenCandidate {
    /// The token row.
    pub fn token(&self) -> &TokenRecord {
        &self.token
    }

    /// The stored SHA-256 digest of the full token text.
    pub fn digest(&self) -> &[u8; 32] {
        &self.digest
    }

    /// The token's owner.
    pub fn user(&self) -> &UserRecord {
        &self.user
    }
}

impl std::fmt::Debug for TokenCandidate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenCandidate")
            .field("token", &self.token)
            .field("digest", &Redacted)
            .field("user", &self.user)
            .finish()
    }
}

const TOKEN_COLUMNS: &str = "id, user_id, name, prefix, created_at, last_used_at, expires_at";

fn token_from_row(row: &Row<'_>) -> rusqlite::Result<TokenRecord> {
    Ok(TokenRecord {
        id: row.get(0)?,
        user_id: row.get(1)?,
        name: row.get(2)?,
        prefix: row.get(3)?,
        created_at: row.get(4)?,
        last_used_at: row.get(5)?,
        expires_at: row.get(6)?,
    })
}

/// The username of user `id`; a token or link always has its user, so a missing one is a storage
/// fault.
pub(super) fn owner_name(conn: &rusqlite::Connection, id: i64) -> Result<String, DbError> {
    Ok(
        conn.query_row("SELECT username FROM users WHERE id = ?1", [id], |row| {
            row.get(0)
        })?,
    )
}

fn select_token(conn: &rusqlite::Connection, id: i64) -> Result<Option<TokenRecord>, DbError> {
    let sql = format!("SELECT {TOKEN_COLUMNS} FROM tokens WHERE id = ?1");
    Ok(conn.query_row(&sql, [id], token_from_row).optional()?)
}

// Qualifies each name in a comma-separated column list with a table alias, so a join can reuse
// the same constants the single-table queries select.
pub(super) fn prefixed(alias: &str, columns: &str) -> String {
    columns
        .split(',')
        .map(|name| format!("{alias}.{}", name.trim()))
        .collect::<Vec<_>>()
        .join(", ")
}

fn user_from_row(row: &Row<'_>) -> rusqlite::Result<UserRecord> {
    user_at(row, 0)
}

// Reads the `USER_COLUMNS` that start at column `base`, so a join can carry a user after other
// columns.
pub(super) fn user_at(row: &Row<'_>, base: usize) -> rusqlite::Result<UserRecord> {
    Ok(UserRecord {
        id: row.get(base)?,
        username: row.get(base + 1)?,
        display_name: row.get(base + 2)?,
        email: row.get(base + 3)?,
        password_hash: row.get(base + 4)?,
        server_admin: row.get(base + 5)?,
        disabled: row.get(base + 6)?,
        created_at: row.get(base + 7)?,
    })
}

fn select_user(conn: &rusqlite::Connection, username: &str) -> Result<Option<UserRecord>, DbError> {
    let sql = format!("SELECT {USER_COLUMNS} FROM users WHERE username = ?1");
    Ok(conn.query_row(&sql, [username], user_from_row).optional()?)
}

impl Db {
    /// Inserts with `created_at = clock::now_rfc3339()` and its `user.create` row in one
    /// transaction; `Conflict` when the username exists.
    pub async fn create_user(&self, user: NewUser, ctx: &AuditCtx) -> Result<UserRecord, DbError> {
        let ctx = ctx.clone();
        // The UNIQUE constraint is the duplicate check, so a writer on another connection is
        // caught too. An error at any step drops `tx` uncommitted.
        self.call(move |conn| {
            let tx = conn.transaction()?;
            tx.execute(
                "INSERT INTO users \
                 (username, display_name, email, password_hash, server_admin, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    user.username,
                    user.display_name,
                    user.email,
                    user.password_hash,
                    user.server_admin,
                    clock::now_rfc3339(),
                ],
            )
            .map_err(|e| {
                conflict_on_unique(e, || format!("username {} is taken", user.username))
            })?;
            let sql = format!("SELECT {USER_COLUMNS} FROM users WHERE id = ?1");
            let created = tx.query_row(&sql, [tx.last_insert_rowid()], user_from_row)?;
            let entry = Entry::new(AuditAction::UserCreate)
                .target("user", Some(created.id), Some(&created.username))
                .detail(&UserCreated {
                    server_admin: created.server_admin,
                })?;
            audit::insert(&tx, &ctx, &entry)?;
            tx.commit()?;
            Ok(created)
        })
        .await
    }

    /// Exact match on `username`.
    pub async fn user_by_username(&self, username: &str) -> Result<Option<UserRecord>, DbError> {
        let username = username.to_owned();
        self.call(move |conn| select_user(conn, &username)).await
    }

    /// Every user, by username.
    pub async fn list_users(&self) -> Result<Vec<UserRecord>, DbError> {
        self.call(|conn| {
            let sql = format!("SELECT {USER_COLUMNS} FROM users ORDER BY username");
            let mut stmt = conn.prepare(&sql)?;
            let users = stmt
                .query_map([], user_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(users)
        })
        .await
    }

    /// Applies the set fields and writes their `user.update` row in one transaction;
    /// `Ok(None)` for an unknown username. No row when nothing is set or the user is unknown.
    pub async fn update_user(
        &self,
        username: &str,
        patch: UserPatch,
        ctx: &AuditCtx,
    ) -> Result<Option<UserRecord>, DbError> {
        let username = username.to_owned();
        let ctx = ctx.clone();
        self.call(move |conn| {
            let (server_admin, disabled) = (patch.server_admin, patch.disabled);
            // Column names come from the fixed list in `assignments`, never from the caller;
            // every value is a bound parameter, with the username last.
            let set = patch.assignments();
            let fields: Vec<&'static str> = set
                .iter()
                .map(|(column, _)| match *column {
                    "password_hash" => "password",
                    column => column,
                })
                .collect();
            let tx = conn.transaction()?;
            let mut changed = false;
            if !set.is_empty() {
                let columns: Vec<String> = set
                    .iter()
                    .enumerate()
                    .map(|(i, (column, _))| format!("{column} = ?{}", i + 1))
                    .collect();
                let sql = format!(
                    "UPDATE users SET {} WHERE username = ?{}",
                    columns.join(", "),
                    set.len() + 1
                );
                let values = set
                    .into_iter()
                    .map(|(_, value)| value)
                    .chain(std::iter::once(Value::Text(username.clone())));
                changed = tx.execute(&sql, params_from_iter(values))? > 0;
            }
            let user = select_user(&tx, &username)?;
            if changed && let Some(user) = &user {
                let entry = Entry::new(AuditAction::UserUpdate)
                    .target("user", Some(user.id), Some(&user.username))
                    .detail(&UserUpdated {
                        fields,
                        server_admin,
                        disabled,
                    })?;
                audit::insert(&tx, &ctx, &entry)?;
            }
            tx.commit()?;
            Ok(user)
        })
        .await
    }

    /// Inserts with `created_at = clock::now_rfc3339()` and its audit row in one transaction:
    /// `session.create` without detail for `Session`, `token.create` naming the owner for `Api`.
    pub async fn create_token(
        &self,
        token: NewToken,
        origin: TokenOrigin,
        ctx: &AuditCtx,
    ) -> Result<TokenRecord, DbError> {
        let ctx = ctx.clone();
        self.call(move |conn| {
            let tx = conn.transaction()?;
            tx.execute(
                "INSERT INTO tokens (user_id, name, prefix, digest, created_at, expires_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    token.user_id,
                    token.name,
                    token.prefix,
                    token.digest,
                    clock::now_rfc3339(),
                    token.expires_at,
                ],
            )?;
            let sql = format!("SELECT {TOKEN_COLUMNS} FROM tokens WHERE id = ?1");
            let created = tx.query_row(&sql, [tx.last_insert_rowid()], token_from_row)?;
            let target = Entry::new(match origin {
                TokenOrigin::Session => AuditAction::SessionCreate,
                TokenOrigin::Api => AuditAction::TokenCreate,
            })
            .target("token", Some(created.id), Some(&created.name));
            let entry = match origin {
                TokenOrigin::Session => target,
                TokenOrigin::Api => {
                    let owner = owner_name(&tx, created.user_id)?;
                    target.detail(&TokenOwner { user: &owner })?
                }
            };
            audit::insert(&tx, &ctx, &entry)?;
            tx.commit()?;
            Ok(created)
        })
        .await
    }

    /// Tokens whose stored prefix is `prefix`, each with its digest and owner.
    pub async fn tokens_by_prefix(&self, prefix: &str) -> Result<Vec<TokenCandidate>, DbError> {
        let prefix = prefix.to_owned();
        self.call(move |conn| {
            // Token columns, then the digest, then the user, each read at a fixed offset; a digest
            // that is not 32 bytes fails the read.
            let sql = format!(
                "SELECT {}, t.digest, {} FROM tokens t JOIN users u ON u.id = t.user_id \
                 WHERE t.prefix = ?1 ORDER BY t.id",
                prefixed("t", TOKEN_COLUMNS),
                prefixed("u", USER_COLUMNS)
            );
            let mut stmt = conn.prepare(&sql)?;
            let candidates = stmt
                .query_map([&prefix], |row| {
                    Ok(TokenCandidate {
                        token: token_from_row(row)?,
                        digest: row.get(7)?,
                        user: user_at(row, 8)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(candidates)
        })
        .await
    }

    /// A user's tokens in creation order.
    pub async fn tokens_for_user(&self, user_id: i64) -> Result<Vec<TokenRecord>, DbError> {
        self.call(move |conn| {
            let sql = format!("SELECT {TOKEN_COLUMNS} FROM tokens WHERE user_id = ?1 ORDER BY id");
            let mut stmt = conn.prepare(&sql)?;
            let tokens = stmt
                .query_map([user_id], token_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(tokens)
        })
        .await
    }

    /// The token with row id `id`, if it exists.
    pub async fn token_by_id(&self, id: i64) -> Result<Option<TokenRecord>, DbError> {
        self.call(move |conn| select_token(conn, id)).await
    }

    /// Deletes the token and writes its `token.revoke` row in one transaction; `Ok(true)` when a
    /// row went, and no audit row otherwise.
    pub async fn delete_token(&self, id: i64, ctx: &AuditCtx) -> Result<bool, DbError> {
        let ctx = ctx.clone();
        self.call(move |conn| {
            // The name and owner are read before the delete, which leaves nothing to read.
            let tx = conn.transaction()?;
            let Some(token) = select_token(&tx, id)? else {
                return Ok(false);
            };
            let owner = owner_name(&tx, token.user_id)?;
            tx.execute("DELETE FROM tokens WHERE id = ?1", [id])?;
            let entry = Entry::new(AuditAction::TokenRevoke)
                .target("token", Some(token.id), Some(&token.name))
                .detail(&TokenOwner { user: &owner })?;
            audit::insert(&tx, &ctx, &entry)?;
            tx.commit()?;
            Ok(true)
        })
        .await
    }

    /// Sets `last_used_at = now` only when it is null or before `stale_before`; a missing row is
    /// not an error.
    pub async fn touch_token(&self, id: i64, now: &str, stale_before: &str) -> Result<(), DbError> {
        let (now, stale_before) = (now.to_owned(), stale_before.to_owned());
        self.call(move |conn| {
            conn.execute(
                "UPDATE tokens SET last_used_at = ?1 \
                 WHERE id = ?2 AND (last_used_at IS NULL OR last_used_at < ?3)",
                params![now, id, stale_before],
            )?;
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixed_qualifies_each_column_with_the_alias() {
        assert_eq!(
            prefixed("t", "id, user_id, name"),
            "t.id, t.user_id, t.name"
        );
    }

    #[test]
    fn the_token_join_selects_tokens_then_digest_then_users() {
        assert_eq!(
            format!(
                "{}, t.digest, {}",
                prefixed("t", TOKEN_COLUMNS),
                prefixed("u", USER_COLUMNS)
            ),
            "t.id, t.user_id, t.name, t.prefix, t.created_at, t.last_used_at, t.expires_at, \
             t.digest, u.id, u.username, u.display_name, u.email, u.password_hash, \
             u.server_admin, u.disabled, u.created_at"
        );
    }
}
