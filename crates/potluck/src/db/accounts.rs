//! The user rows of `server.db`: create, look up, list and patch, each one job on the database
//! thread. Password hashes never reach `Debug` output.

use rusqlite::types::Value;
use rusqlite::{OptionalExtension, Row, params, params_from_iter};

use super::{Db, DbError};
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
#[derive(Clone, PartialEq, Eq)]
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
#[derive(Clone, PartialEq, Eq)]
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
#[derive(Clone, Default, PartialEq, Eq)]
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

const USER_COLUMNS: &str =
    "id, username, display_name, email, password_hash, server_admin, disabled, created_at";

fn user_from_row(row: &Row<'_>) -> rusqlite::Result<UserRecord> {
    Ok(UserRecord {
        id: row.get(0)?,
        username: row.get(1)?,
        display_name: row.get(2)?,
        email: row.get(3)?,
        password_hash: row.get(4)?,
        server_admin: row.get(5)?,
        disabled: row.get(6)?,
        created_at: row.get(7)?,
    })
}

fn select_user(conn: &rusqlite::Connection, username: &str) -> Result<Option<UserRecord>, DbError> {
    let sql = format!("SELECT {USER_COLUMNS} FROM users WHERE username = ?1");
    Ok(conn.query_row(&sql, [username], user_from_row).optional()?)
}

impl Db {
    /// Inserts with `created_at = clock::now_rfc3339()`; `Conflict` when the username exists.
    pub async fn create_user(&self, user: NewUser) -> Result<UserRecord, DbError> {
        // The check and the insert share one job, and the database thread runs one job at a
        // time, so no other insert can slip between them.
        self.call(move |conn| {
            let taken: bool = conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM users WHERE username = ?1)",
                [&user.username],
                |row| row.get(0),
            )?;
            if taken {
                return Err(DbError::Conflict {
                    what: format!("username {} is taken", user.username),
                });
            }
            conn.execute(
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
            )?;
            let sql = format!("SELECT {USER_COLUMNS} FROM users WHERE id = ?1");
            Ok(conn.query_row(&sql, [conn.last_insert_rowid()], user_from_row)?)
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

    /// Applies the set fields; `Ok(None)` for an unknown username.
    pub async fn update_user(
        &self,
        username: &str,
        patch: UserPatch,
    ) -> Result<Option<UserRecord>, DbError> {
        let username = username.to_owned();
        self.call(move |conn| {
            // Column names come from the fixed list in `assignments`, never from the caller;
            // every value is a bound parameter, with the username last.
            let set = patch.assignments();
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
                conn.execute(&sql, params_from_iter(values))?;
            }
            select_user(conn, &username)
        })
        .await
    }
}
