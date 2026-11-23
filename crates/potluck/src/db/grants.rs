//! Grants and the role they add up to, plus the lookup of an armed override.

use rusqlite::{Connection, OptionalExtension, Row, named_params, params};

use super::audit::{self, AuditAction, AuditCtx, Entry, GrantChange};
use super::workspaces::{WORKSPACE_COLUMNS, WorkspaceRecord, workspace_from_row};
use super::{Db, DbError};
use crate::clock;
use crate::role::Role;

/// Who a grant is for.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Subject {
    /// A user, by username.
    User(String),
    /// A team, by slug: every member holds the grant.
    Team(String),
}

impl Subject {
    /// `user` or `team`, as the schema and the JSON spell it.
    pub fn kind(&self) -> &'static str {
        match self {
            Subject::User(_) => "user",
            Subject::Team(_) => "team",
        }
    }

    /// The username or team slug.
    pub fn name(&self) -> &str {
        match self {
            Subject::User(name) | Subject::Team(name) => name,
        }
    }
}

/// A `grants` row with its subject's name.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct GrantRecord {
    subject: Subject,
    role: Role,
}

impl GrantRecord {
    /// A grant of `role` to `subject`.
    pub fn new(subject: Subject, role: Role) -> Self {
        GrantRecord { subject, role }
    }

    /// Who holds it.
    pub fn subject(&self) -> &Subject {
        &self.subject
    }

    /// What it grants.
    pub fn role(&self) -> Role {
        self.role
    }
}

/// A workspace and the role a user holds on it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct HeldWorkspace {
    workspace: WorkspaceRecord,
    role: Role,
}

impl HeldWorkspace {
    /// The workspace.
    pub fn workspace(&self) -> &WorkspaceRecord {
        &self.workspace
    }

    /// The user's role on it.
    pub fn role(&self) -> Role {
        self.role
    }
}

// Roles compare as ranks in SQL so `MAX` picks the highest grant; `role_from_rank` reverses it.
// An unknown role ranks above admin, so `MAX` surfaces it beside any valid grant and
// `role_from_rank` refuses it rather than reading it as `read`.
const RANK: &str =
    "MAX(CASE g.role WHEN 'admin' THEN 3 WHEN 'write' THEN 2 WHEN 'read' THEN 1 ELSE 4 END)";

// The grants `g` that reach user `:user`: their own, or one to a team they belong to.
const HELD_BY_USER: &str = "((g.subject_kind = 'user' AND g.subject_id = :user) \
     OR (g.subject_kind = 'team' AND g.subject_id IN \
     (SELECT team_id FROM team_members WHERE user_id = :user)))";

fn role_from_rank(rank: i64, idx: usize) -> rusqlite::Result<Role> {
    match rank {
        3 => Ok(Role::Admin),
        2 => Ok(Role::Write),
        1 => Ok(Role::Read),
        _ => Err(rusqlite::Error::InvalidColumnType(
            idx,
            "role".to_owned(),
            rusqlite::types::Type::Integer,
        )),
    }
}

fn role_from_text(row: &Row<'_>, idx: usize) -> rusqlite::Result<Role> {
    let text: String = row.get(idx)?;
    Role::parse(&text).ok_or_else(|| {
        rusqlite::Error::InvalidColumnType(idx, "role".to_owned(), rusqlite::types::Type::Text)
    })
}

/// The row id behind a subject's name, if it exists.
fn subject_id(conn: &Connection, subject: &Subject) -> rusqlite::Result<Option<i64>> {
    let sql = match subject {
        Subject::User(_) => "SELECT id FROM users WHERE username = ?1",
        Subject::Team(_) => "SELECT id FROM teams WHERE slug = ?1",
    };
    conn.query_row(sql, [subject.name()], |row| row.get(0))
        .optional()
}

/// The role text of the subject's grant on the workspace, if it holds one.
fn current_role(
    conn: &Connection,
    workspace_id: i64,
    subject: &Subject,
    id: i64,
) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT role FROM grants \
         WHERE workspace_id = ?1 AND subject_kind = ?2 AND subject_id = ?3",
        params![workspace_id, subject.kind(), id],
        |row| row.get(0),
    )
    .optional()
}

/// Writes a grant's audit row, naming the workspace as `{team}/{ws}` as it stands now.
fn log_grant(
    conn: &Connection,
    ctx: &AuditCtx,
    action: AuditAction,
    workspace_id: i64,
    subject: &Subject,
    id: i64,
    change: &GrantChange<'_>,
) -> Result<(), DbError> {
    let workspace: String = conn.query_row(
        "SELECT t.slug || '/' || w.slug FROM workspaces w \
         JOIN teams t ON t.id = w.team_id WHERE w.id = ?1",
        [workspace_id],
        |row| row.get(0),
    )?;
    let entry = Entry::new(action)
        .target(subject.kind(), Some(id), Some(subject.name()))
        .workspace(&workspace)
        .detail(change)?;
    Ok(audit::insert(conn, ctx, &entry)?)
}

impl Db {
    /// Inserts or replaces the subject's grant and writes its `grant.put` row in one
    /// transaction; `Ok(None)`, and no audit row, when no such user or team exists.
    pub async fn put_grant(
        &self,
        workspace_id: i64,
        subject: Subject,
        role: Role,
        ctx: &AuditCtx,
    ) -> Result<Option<GrantRecord>, DbError> {
        let ctx = ctx.clone();
        // An error at any step drops `tx` uncommitted.
        self.call(move |conn| {
            let tx = conn.transaction()?;
            let Some(id) = subject_id(&tx, &subject)? else {
                return Ok(None);
            };
            let old = current_role(&tx, workspace_id, &subject, id)?;
            tx.execute(
                "INSERT INTO grants (workspace_id, subject_kind, subject_id, role) \
                 VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT(workspace_id, subject_kind, subject_id) \
                 DO UPDATE SET role = excluded.role",
                params![workspace_id, subject.kind(), id, role.as_str()],
            )?;
            let change = GrantChange {
                old: old.as_deref(),
                new: Some(role.as_str()),
            };
            log_grant(
                &tx,
                &ctx,
                AuditAction::GrantPut,
                workspace_id,
                &subject,
                id,
                &change,
            )?;
            tx.commit()?;
            Ok(Some(GrantRecord::new(subject, role)))
        })
        .await
    }

    /// Deletes the subject's grant and writes its `grant.delete` row in one transaction.
    /// `Ok(true)` when a grant went; `Ok(false)`, and no audit row, for no such grant or subject.
    pub async fn delete_grant(
        &self,
        workspace_id: i64,
        subject: Subject,
        ctx: &AuditCtx,
    ) -> Result<bool, DbError> {
        let ctx = ctx.clone();
        self.call(move |conn| {
            let tx = conn.transaction()?;
            let Some(id) = subject_id(&tx, &subject)? else {
                return Ok(false);
            };
            let Some(old) = current_role(&tx, workspace_id, &subject, id)? else {
                return Ok(false);
            };
            tx.execute(
                "DELETE FROM grants \
                 WHERE workspace_id = ?1 AND subject_kind = ?2 AND subject_id = ?3",
                params![workspace_id, subject.kind(), id],
            )?;
            let change = GrantChange {
                old: Some(&old),
                new: None,
            };
            log_grant(
                &tx,
                &ctx,
                AuditAction::GrantDelete,
                workspace_id,
                &subject,
                id,
                &change,
            )?;
            tx.commit()?;
            Ok(true)
        })
        .await
    }

    /// The workspace's grants: teams first, then users, each by name. A grant whose subject no
    /// longer exists is left out.
    pub async fn list_grants(&self, workspace_id: i64) -> Result<Vec<GrantRecord>, DbError> {
        self.call(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT g.subject_kind, COALESCE(u.username, t.slug), g.role FROM grants g \
                 LEFT JOIN users u ON g.subject_kind = 'user' AND u.id = g.subject_id \
                 LEFT JOIN teams t ON g.subject_kind = 'team' AND t.id = g.subject_id \
                 WHERE g.workspace_id = ?1 AND COALESCE(u.username, t.slug) IS NOT NULL \
                 ORDER BY g.subject_kind, 2",
            )?;
            let grants = stmt
                .query_map([workspace_id], |row| {
                    let kind: String = row.get(0)?;
                    let name: String = row.get(1)?;
                    let subject = if kind == "team" {
                        Subject::Team(name)
                    } else {
                        Subject::User(name)
                    };
                    Ok(GrantRecord::new(subject, role_from_text(row, 2)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(grants)
        })
        .await
    }

    /// The highest of the user's own grant and the grants of the teams they belong to; `None`
    /// without any. Server admins are not special here.
    pub async fn role_for(&self, workspace_id: i64, user_id: i64) -> Result<Option<Role>, DbError> {
        self.call(move |conn| {
            // The aggregate always yields one row, NULL when no grant matched.
            let sql = format!(
                "SELECT {RANK} FROM grants g WHERE g.workspace_id = :ws AND {HELD_BY_USER}"
            );
            let role = conn.query_row(
                &sql,
                named_params! {":ws": workspace_id, ":user": user_id},
                |row| {
                    let rank: Option<i64> = row.get(0)?;
                    rank.map(|rank| role_from_rank(rank, 0)).transpose()
                },
            )?;
            Ok(role)
        })
        .await
    }

    /// Every workspace the user holds a role on, with that role, by team slug then slug.
    pub async fn workspaces_for_user(&self, user_id: i64) -> Result<Vec<HeldWorkspace>, DbError> {
        self.call(move |conn| {
            let sql = format!(
                "SELECT {WORKSPACE_COLUMNS}, {RANK} FROM grants g \
                 JOIN workspaces w ON w.id = g.workspace_id JOIN teams t ON t.id = w.team_id \
                 WHERE {HELD_BY_USER} GROUP BY w.id ORDER BY t.slug, w.slug"
            );
            let mut stmt = conn.prepare(&sql)?;
            let held = stmt
                .query_map(named_params! {":user": user_id}, |row| {
                    Ok(HeldWorkspace {
                        workspace: workspace_from_row(row)?,
                        role: role_from_rank(row.get(9)?, 9)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(held)
        })
        .await
    }

    /// The lowest id of an armed (unconsumed) override for `for_user_id` on the workspace.
    pub async fn armed_override(
        &self,
        workspace_id: i64,
        for_user_id: i64,
    ) -> Result<Option<i64>, DbError> {
        self.call(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT id FROM overrides \
                     WHERE workspace_id = ?1 AND for_user_id = ?2 AND consumed_at IS NULL \
                     ORDER BY id LIMIT 1",
                    [workspace_id, for_user_id],
                    |row| row.get(0),
                )
                .optional()?)
        })
        .await
    }

    /// Arms an override: inserts with `armed_at = now` and returns its id. Its endpoints and
    /// consumption live with push bookkeeping.
    pub async fn arm_override(
        &self,
        workspace_id: i64,
        for_user_id: i64,
        reason: &str,
        armed_by: &str,
    ) -> Result<i64, DbError> {
        let reason = reason.to_owned();
        let armed_by = armed_by.to_owned();
        self.call(move |conn| {
            conn.execute(
                "INSERT INTO overrides (workspace_id, for_user_id, reason, armed_by, armed_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    workspace_id,
                    for_user_id,
                    reason,
                    armed_by,
                    clock::now_rfc3339()
                ],
            )?;
            Ok(conn.last_insert_rowid())
        })
        .await
    }
}
