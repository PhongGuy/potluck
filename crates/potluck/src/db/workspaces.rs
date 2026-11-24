//! The workspace rows of `server.db`. The import state is set here and cleared by push
//! bookkeeping.

use rusqlite::{Connection, OptionalExtension, Row, params};

use super::audit::{self, AuditAction, AuditCtx, Entry, WorkspaceCreated};
use super::{Db, DbError, conflict_on_unique};
use crate::clock;
use crate::slug::Slug;

/// What `create_workspace` inserts; `created_at` is set by the database thread.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct NewWorkspace {
    team_id: i64,
    slug: String,
    name: String,
    kind: String,
    default_branch: String,
    import_pending: bool,
}

impl NewWorkspace {
    /// A seeded workspace of `kind` (import state clear).
    pub fn new(team_id: i64, slug: &Slug, name: &str, kind: &str, default_branch: &str) -> Self {
        NewWorkspace {
            team_id,
            slug: slug.as_str().to_owned(),
            name: name.to_owned(),
            kind: kind.to_owned(),
            default_branch: default_branch.to_owned(),
            import_pending: false,
        }
    }

    /// Sets the one-time import state.
    #[must_use]
    pub fn import_pending(mut self, yes: bool) -> Self {
        self.import_pending = yes;
        self
    }
}

/// A `workspaces` row with its team's slug.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WorkspaceRecord {
    id: i64,
    team_id: i64,
    team_slug: String,
    slug: String,
    name: String,
    kind: String,
    default_branch: String,
    import_pending: bool,
    created_at: String,
}

impl WorkspaceRecord {
    /// The row id.
    pub fn id(&self) -> i64 {
        self.id
    }

    /// The owning team's row id.
    pub fn team_id(&self) -> i64 {
        self.team_id
    }

    /// The owning team's slug.
    pub fn team_slug(&self) -> &str {
        &self.team_slug
    }

    /// The slug, unique within the team.
    pub fn slug(&self) -> &str {
        &self.slug
    }

    /// The display name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The kind name, `apinata` or `dbento`.
    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// The branch the repository was initialised with.
    pub fn default_branch(&self) -> &str {
        &self.default_branch
    }

    /// Whether the first accepted push is still an import.
    pub fn import_pending(&self) -> bool {
        self.import_pending
    }

    /// When the row was created, RFC 3339 UTC to the second.
    pub fn created_at(&self) -> &str {
        &self.created_at
    }
}

// Every query selecting these must declare the aliases:
// `FROM workspaces w JOIN teams t ON t.id = w.team_id`.
pub(super) const WORKSPACE_COLUMNS: &str = "w.id, w.team_id, t.slug, w.slug, w.name, w.kind, \
     w.default_branch, w.import_pending, w.created_at";

/// Reads the nine `WORKSPACE_COLUMNS` from column 0.
pub(super) fn workspace_from_row(row: &Row<'_>) -> rusqlite::Result<WorkspaceRecord> {
    Ok(WorkspaceRecord {
        id: row.get(0)?,
        team_id: row.get(1)?,
        team_slug: row.get(2)?,
        slug: row.get(3)?,
        name: row.get(4)?,
        kind: row.get(5)?,
        default_branch: row.get(6)?,
        import_pending: row.get(7)?,
        created_at: row.get(8)?,
    })
}

/// The workspace's audit name, `{team}/{ws}` by slugs as they stand now; `None` when no
/// workspace has that id. Every audit row names its workspace through this, so
/// `potluck admin audit` filters match.
pub(super) fn team_ws_name(
    conn: &Connection,
    workspace_id: i64,
) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT t.slug || '/' || w.slug FROM workspaces w \
         JOIN teams t ON t.id = w.team_id WHERE w.id = ?1",
        [workspace_id],
        |row| row.get(0),
    )
    .optional()
}

impl Db {
    /// Inserts with `created_at = now` and writes its `workspace.create` row in one transaction;
    /// `Conflict` ("workspace {slug} exists in this team") on the `(team_id, slug)` key. The kind
    /// must pass the schema's CHECK.
    pub async fn create_workspace(
        &self,
        ws: NewWorkspace,
        ctx: &AuditCtx,
    ) -> Result<WorkspaceRecord, DbError> {
        let ctx = ctx.clone();
        // The UNIQUE constraint is the duplicate check, so a writer on another connection is
        // caught too; an unknown kind or team is left to the schema and surfaces as `Sqlite`.
        // An error at any step drops `tx` uncommitted.
        self.call(move |conn| {
            let tx = conn.transaction()?;
            tx.execute(
                "INSERT INTO workspaces \
                 (team_id, slug, name, kind, default_branch, import_pending, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    ws.team_id,
                    ws.slug,
                    ws.name,
                    ws.kind,
                    ws.default_branch,
                    ws.import_pending,
                    clock::now_rfc3339(),
                ],
            )
            .map_err(|e| {
                conflict_on_unique(e, || format!("workspace {} exists in this team", ws.slug))
            })?;
            let sql = format!(
                "SELECT {WORKSPACE_COLUMNS} FROM workspaces w JOIN teams t ON t.id = w.team_id \
                 WHERE w.id = ?1"
            );
            let record = tx.query_row(&sql, [tx.last_insert_rowid()], workspace_from_row)?;
            let name = team_ws_name(&tx, record.id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
            let entry = Entry::new(AuditAction::WorkspaceCreate)
                .target("workspace", Some(record.id), Some(&name))
                .workspace(&name)
                .detail(&WorkspaceCreated {
                    kind: &record.kind,
                    seed: !record.import_pending,
                })?;
            audit::insert(&tx, &ctx, &entry)?;
            tx.commit()?;
            Ok(record)
        })
        .await
    }

    /// `{team}/{ws}` by slugs.
    pub async fn workspace_by_slugs(
        &self,
        team: &str,
        ws: &str,
    ) -> Result<Option<WorkspaceRecord>, DbError> {
        let team = team.to_owned();
        let ws = ws.to_owned();
        self.call(move |conn| {
            let sql = format!(
                "SELECT {WORKSPACE_COLUMNS} FROM workspaces w JOIN teams t ON t.id = w.team_id \
                 WHERE t.slug = ?1 AND w.slug = ?2"
            );
            Ok(conn
                .query_row(&sql, [team, ws], workspace_from_row)
                .optional()?)
        })
        .await
    }

    /// Every workspace, by team slug then slug.
    pub async fn list_workspaces(&self) -> Result<Vec<WorkspaceRecord>, DbError> {
        self.call(|conn| {
            let sql = format!(
                "SELECT {WORKSPACE_COLUMNS} FROM workspaces w JOIN teams t ON t.id = w.team_id \
                 ORDER BY t.slug, w.slug"
            );
            let mut stmt = conn.prepare(&sql)?;
            let workspaces = stmt
                .query_map([], workspace_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(workspaces)
        })
        .await
    }

    /// Deletes the row (grants and overrides cascade) and writes its `workspace.delete` row in
    /// one transaction. `Ok(true)` when a row went; `Ok(false)`, and no audit row, otherwise.
    pub async fn delete_workspace(&self, id: i64, ctx: &AuditCtx) -> Result<bool, DbError> {
        let ctx = ctx.clone();
        // The name is read before the delete, since the row it comes from is about to go.
        self.call(move |conn| {
            let tx = conn.transaction()?;
            let Some(name) = team_ws_name(&tx, id)? else {
                return Ok(false);
            };
            tx.execute("DELETE FROM workspaces WHERE id = ?1", [id])?;
            let entry = Entry::new(AuditAction::WorkspaceDelete)
                .target("workspace", Some(id), Some(&name))
                .workspace(&name);
            audit::insert(&tx, &ctx, &entry)?;
            tx.commit()?;
            Ok(true)
        })
        .await
    }
}
