//! The `oidc_links` rows of `server.db`: which IdP subject signs in as which user, at most one
//! subject per user per issuer. Issuers compare as exact text.

use rusqlite::{OptionalExtension, Row, params};

use super::accounts::{USER_COLUMNS, UserRecord, owner_name, prefixed, user_at};
use super::audit::{self, AuditAction, AuditCtx, Entry, OidcSubject};
use super::{Db, DbError, conflict_on_unique};
use crate::clock;

/// An `oidc_links` row.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct OidcLink {
    issuer: String,
    subject: String,
    user_id: i64,
    linked_at: String,
}

impl OidcLink {
    /// The issuer URL as configured when the link was made.
    pub fn issuer(&self) -> &str {
        &self.issuer
    }
    /// The IdP's `sub` claim.
    pub fn subject(&self) -> &str {
        &self.subject
    }
    /// The linked user's id.
    pub fn user_id(&self) -> i64 {
        self.user_id
    }
    /// When the link was made, RFC 3339 UTC.
    pub fn linked_at(&self) -> &str {
        &self.linked_at
    }
}

const LINK_COLUMNS: &str = "issuer, subject, user_id, linked_at";

fn link_from_row(row: &Row<'_>) -> rusqlite::Result<OidcLink> {
    Ok(OidcLink {
        issuer: row.get(0)?,
        subject: row.get(1)?,
        user_id: row.get(2)?,
        linked_at: row.get(3)?,
    })
}

fn select_link(
    conn: &rusqlite::Connection,
    issuer: &str,
    user_id: i64,
) -> Result<Option<OidcLink>, DbError> {
    let sql = format!("SELECT {LINK_COLUMNS} FROM oidc_links WHERE issuer = ?1 AND user_id = ?2");
    Ok(conn
        .query_row(&sql, params![issuer, user_id], link_from_row)
        .optional()?)
}

impl Db {
    /// The user linked to `subject` under `issuer`, if any.
    pub async fn user_by_oidc(
        &self,
        issuer: &str,
        subject: &str,
    ) -> Result<Option<UserRecord>, DbError> {
        let (issuer, subject) = (issuer.to_owned(), subject.to_owned());
        self.call(move |conn| {
            let sql = format!(
                "SELECT {} FROM oidc_links l JOIN users u ON u.id = l.user_id \
                 WHERE l.issuer = ?1 AND l.subject = ?2",
                prefixed("u", USER_COLUMNS)
            );
            Ok(conn
                .query_row(&sql, params![issuer, subject], |r| user_at(r, 0))
                .optional()?)
        })
        .await
    }

    /// The user's link under `issuer`, if any.
    pub async fn oidc_link_of(
        &self,
        issuer: &str,
        user_id: i64,
    ) -> Result<Option<OidcLink>, DbError> {
        let issuer = issuer.to_owned();
        self.call(move |conn| select_link(conn, &issuer, user_id))
            .await
    }

    /// Replaces the user's link for `issuer`; `Conflict` when `subject` is linked to another
    /// user. Writes `user.oidc_link` with `{subject}` in the same transaction.
    pub async fn put_oidc_link(
        &self,
        issuer: &str,
        user_id: i64,
        subject: &str,
        ctx: &AuditCtx,
    ) -> Result<OidcLink, DbError> {
        let (issuer, subject, ctx) = (issuer.to_owned(), subject.to_owned(), ctx.clone());
        // Drop the user's old link, then insert: the primary key catches a subject held by
        // another user, and the uncommitted transaction then keeps the old link in place.
        self.call(move |conn| {
            let tx = conn.transaction()?;
            tx.execute(
                "DELETE FROM oidc_links WHERE issuer = ?1 AND user_id = ?2",
                params![issuer, user_id],
            )?;
            tx.execute(
                "INSERT INTO oidc_links (issuer, subject, user_id, linked_at) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![issuer, subject, user_id, clock::now_rfc3339()],
            )
            .map_err(|e| {
                conflict_on_unique(e, || "that identity is linked to another user".to_owned())
            })?;
            let link = select_link(&tx, &issuer, user_id)?
                .ok_or_else(|| DbError::Sqlite("the new link is missing".to_owned()))?;
            let name = owner_name(&tx, user_id)?;
            let entry = Entry::new(AuditAction::UserOidcLink)
                .target("user", Some(user_id), Some(&name))
                .detail(&OidcSubject { subject: &subject })?;
            audit::insert(&tx, &ctx, &entry)?;
            tx.commit()?;
            Ok(link)
        })
        .await
    }

    /// Removes the user's link for `issuer`; `true` when a link went, and only then writes
    /// `user.oidc_unlink` with the removed `{subject}`.
    pub async fn delete_oidc_link(
        &self,
        issuer: &str,
        user_id: i64,
        ctx: &AuditCtx,
    ) -> Result<bool, DbError> {
        let (issuer, ctx) = (issuer.to_owned(), ctx.clone());
        self.call(move |conn| {
            // The subject is read before the delete, which leaves nothing to read.
            let tx = conn.transaction()?;
            let Some(link) = select_link(&tx, &issuer, user_id)? else {
                return Ok(false);
            };
            tx.execute(
                "DELETE FROM oidc_links WHERE issuer = ?1 AND user_id = ?2",
                params![issuer, user_id],
            )?;
            let name = owner_name(&tx, user_id)?;
            let entry = Entry::new(AuditAction::UserOidcUnlink)
                .target("user", Some(user_id), Some(&name))
                .detail(&OidcSubject {
                    subject: &link.subject,
                })?;
            audit::insert(&tx, &ctx, &entry)?;
            tx.commit()?;
            Ok(true)
        })
        .await
    }
}
