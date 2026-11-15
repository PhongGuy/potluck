//! The `overrides` rows and the push record: consuming an override and clearing the import
//! state happen in one transaction.

use rusqlite::{OptionalExtension, Row, params};

use super::{Db, DbError};
use crate::clock;
use crate::git::RefMove;

/// An `overrides` row with its workspace's `{team}/{ws}` name and the user's username.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct OverrideRecord {
    id: i64,
    workspace: String,
    for_user: String,
    reason: String,
    armed_by: String,
    armed_at: String,
    consumed_at: Option<String>,
    consumed_refs: Option<Vec<RefMove>>,
}

impl OverrideRecord {
    /// The row id.
    pub fn id(&self) -> i64 {
        self.id
    }

    /// The workspace as `{team}/{ws}`.
    pub fn workspace(&self) -> &str {
        &self.workspace
    }

    /// The username the override is for.
    pub fn for_user(&self) -> &str {
        &self.for_user
    }

    /// Why it was armed.
    pub fn reason(&self) -> &str {
        &self.reason
    }

    /// The arming user's username, or `system`.
    pub fn armed_by(&self) -> &str {
        &self.armed_by
    }

    /// When it was armed, RFC 3339.
    pub fn armed_at(&self) -> &str {
        &self.armed_at
    }

    /// When a push consumed it, RFC 3339; `None` while armed.
    pub fn consumed_at(&self) -> Option<&str> {
        self.consumed_at.as_deref()
    }

    /// The refs the consuming push moved; `None` while armed.
    pub fn consumed_refs(&self) -> Option<&[RefMove]> {
        self.consumed_refs.as_deref()
    }
}

/// What `disarm_override` found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Disarmed {
    /// The armed row is gone.
    Removed,
    /// It had been consumed; history keeps it.
    Consumed,
    /// No such override on this workspace.
    Missing,
}

/// What `record_push` changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Recorded {
    override_consumed: bool,
    import_cleared: bool,
}

impl Recorded {
    /// Whether the given override was armed and is now consumed.
    pub fn override_consumed(&self) -> bool {
        self.override_consumed
    }

    /// Whether the workspace was in import state and is no longer.
    pub fn import_cleared(&self) -> bool {
        self.import_cleared
    }
}

const OVERRIDE_SELECT: &str = "SELECT o.id, t.slug || '/' || w.slug, u.username, o.reason, \
     o.armed_by, o.armed_at, o.consumed_at, o.consumed_refs FROM overrides o \
     JOIN workspaces w ON w.id = o.workspace_id JOIN teams t ON t.id = w.team_id \
     JOIN users u ON u.id = o.for_user_id";

// `consumed_refs` was written by `record_push`, so JSON that does not parse is a storage fault
// and surfaces as a SQLite conversion error.
fn override_from_row(row: &Row<'_>) -> rusqlite::Result<OverrideRecord> {
    let consumed_refs = row
        .get::<_, Option<String>>(7)?
        .map(|json| serde_json::from_str::<Vec<RefMove>>(&json))
        .transpose()
        .map_err(|err| {
            rusqlite::Error::FromSqlConversionFailure(7, rusqlite::types::Type::Text, err.into())
        })?;
    Ok(OverrideRecord {
        id: row.get(0)?,
        workspace: row.get(1)?,
        for_user: row.get(2)?,
        reason: row.get(3)?,
        armed_by: row.get(4)?,
        armed_at: row.get(5)?,
        consumed_at: row.get(6)?,
        consumed_refs,
    })
}

impl Db {
    /// The workspace's overrides, oldest first.
    pub async fn list_overrides(&self, workspace_id: i64) -> Result<Vec<OverrideRecord>, DbError> {
        self.call(move |conn| {
            let sql = format!("{OVERRIDE_SELECT} WHERE o.workspace_id = ?1 ORDER BY o.id");
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt
                .query_map([workspace_id], override_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
    }

    /// One override, if it belongs to the workspace.
    pub async fn override_by_id(
        &self,
        workspace_id: i64,
        id: i64,
    ) -> Result<Option<OverrideRecord>, DbError> {
        self.call(move |conn| {
            let sql = format!("{OVERRIDE_SELECT} WHERE o.id = ?1 AND o.workspace_id = ?2");
            Ok(conn
                .query_row(&sql, [id, workspace_id], override_from_row)
                .optional()?)
        })
        .await
    }

    /// Removes an armed override; see `Disarmed`.
    pub async fn disarm_override(&self, workspace_id: i64, id: i64) -> Result<Disarmed, DbError> {
        // One job on the database thread, so nothing can consume the row between the look and
        // the delete.
        self.call(move |conn| {
            let found: Option<Option<String>> = conn
                .query_row(
                    "SELECT consumed_at FROM overrides WHERE id = ?1 AND workspace_id = ?2",
                    [id, workspace_id],
                    |row| row.get(0),
                )
                .optional()?;
            match found {
                None => Ok(Disarmed::Missing),
                Some(Some(_)) => Ok(Disarmed::Consumed),
                Some(None) => {
                    conn.execute(
                        "DELETE FROM overrides WHERE id = ?1 AND workspace_id = ?2",
                        [id, workspace_id],
                    )?;
                    Ok(Disarmed::Removed)
                }
            }
        })
        .await
    }

    /// Records an accepted push in one transaction: `override_id` (when given and still armed)
    /// gets `consumed_at = now` and `consumed_refs = moves` as JSON, and the workspace's
    /// `import_pending` becomes 0. Both persist or neither does.
    pub async fn record_push(
        &self,
        workspace_id: i64,
        override_id: Option<i64>,
        moves: &[RefMove],
    ) -> Result<Recorded, DbError> {
        let refs = serde_json::to_string(moves)
            .map_err(|err| DbError::Sqlite(format!("could not encode the moved refs: {err}")))?;
        self.call(move |conn| {
            // An error at any step drops `tx` uncommitted, which rolls the consume back.
            let tx = conn.transaction()?;
            let override_consumed = match override_id {
                Some(id) => {
                    tx.execute(
                        "UPDATE overrides SET consumed_at = ?1, consumed_refs = ?2 \
                         WHERE id = ?3 AND workspace_id = ?4 AND consumed_at IS NULL",
                        params![clock::now_rfc3339(), refs, id, workspace_id],
                    )? > 0
                }
                None => false,
            };
            let import_cleared = tx.execute(
                "UPDATE workspaces SET import_pending = 0 WHERE id = ?1 AND import_pending = 1",
                [workspace_id],
            )? > 0;
            tx.commit()?;
            Ok(Recorded {
                override_consumed,
                import_cleared,
            })
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::push::ZERO_SHA;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    /// A migrated database with user `bob`, team `core`, workspace `core/api` in import state,
    /// and one armed override for bob; returns `(dir, db, workspace_id, override_id)`.
    async fn seeded() -> (tempfile::TempDir, Db, i64, i64) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("server.db")).unwrap();
        db.migrate().await.unwrap();
        let (ws, ov) = db
            .call(|c| {
                c.execute_batch(
                    "INSERT INTO users (id, username, display_name, created_at) VALUES (1, 'bob', 'Bob', '2026-01-01T00:00:00Z');
                     INSERT INTO teams (id, slug, name) VALUES (1, 'core', 'Core');
                     INSERT INTO workspaces (id, team_id, slug, name, kind, default_branch, import_pending, created_at)
                         VALUES (1, 1, 'api', 'API', 'apinata', 'main', 1, '2026-01-01T00:00:00Z');
                     INSERT INTO overrides (id, workspace_id, for_user_id, reason, armed_by, armed_at)
                         VALUES (5, 1, 1, 'import', 'system', '2026-01-01T00:00:00Z');",
                )?;
                Ok((1, 5))
            })
            .await
            .unwrap();
        (dir, db, ws, ov)
    }

    async fn import_pending(db: &Db) -> bool {
        db.call(|c| {
            Ok(c.query_row(
                "SELECT import_pending FROM workspaces WHERE id = 1",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn record_push_consumes_the_override_and_clears_the_import_state_together() {
        let (_dir, db, ws, ov) = seeded().await;
        let moves = vec![
            RefMove::new("refs/heads/main", ZERO_SHA, A),
            RefMove::new("refs/tags/v1", ZERO_SHA, A),
        ];
        let done = db.record_push(ws, Some(ov), &moves).await.unwrap();
        assert!(
            done.override_consumed() && done.import_cleared(),
            "{done:?}"
        );
        let row = db.override_by_id(ws, ov).await.unwrap().unwrap();
        assert_eq!(row.workspace(), "core/api");
        assert_eq!(row.for_user(), "bob");
        assert_eq!(row.armed_by(), "system");
        assert!(row.consumed_at().is_some());
        assert_eq!(row.consumed_refs(), Some(moves.as_slice()));
        assert!(!import_pending(&db).await);
        let again = db.record_push(ws, Some(ov), &moves).await.unwrap();
        assert!(
            !again.override_consumed() && !again.import_cleared(),
            "nothing left to change: {again:?}"
        );
        let none = db.record_push(ws, None, &moves).await.unwrap();
        assert!(!none.override_consumed(), "no override given");
        assert_eq!(
            db.override_by_id(2, ov).await.unwrap(),
            None,
            "another workspace does not see it"
        );
    }

    #[tokio::test]
    async fn a_failure_after_the_consume_rolls_the_consume_back() {
        let (_dir, db, ws, ov) = seeded().await;
        db.call(|c| {
            Ok(c.execute_batch(
                "CREATE TRIGGER boom BEFORE UPDATE OF import_pending ON workspaces BEGIN SELECT RAISE(ABORT, 'boom'); END",
            )?)
        })
        .await
        .unwrap();
        let err = db
            .record_push(
                ws,
                Some(ov),
                &[RefMove::new("refs/heads/main", ZERO_SHA, A)],
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, DbError::Sqlite(ref m) if m.contains("boom")),
            "got {err:?}"
        );
        let row = db.override_by_id(ws, ov).await.unwrap().unwrap();
        assert_eq!(row.consumed_at(), None, "the consume did not persist");
        assert_eq!(row.consumed_refs(), None);
        assert!(
            import_pending(&db).await,
            "the import state did not change either"
        );
    }

    #[tokio::test]
    async fn list_and_disarm() {
        let (_dir, db, ws, ov) = seeded().await;
        let list = db.list_overrides(ws).await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id(), ov);
        assert_eq!(list[0].reason(), "import");
        assert_eq!(
            db.disarm_override(2, ov).await.unwrap(),
            Disarmed::Missing,
            "wrong workspace"
        );
        assert_eq!(db.disarm_override(ws, 99).await.unwrap(), Disarmed::Missing);
        assert_eq!(db.disarm_override(ws, ov).await.unwrap(), Disarmed::Removed);
        assert_eq!(db.disarm_override(ws, ov).await.unwrap(), Disarmed::Missing);
        assert!(db.list_overrides(ws).await.unwrap().is_empty());
        db.call(|c| {
            Ok(c.execute_batch(
                "INSERT INTO overrides (id, workspace_id, for_user_id, reason, armed_by, armed_at) VALUES (6, 1, 1, 'again', 'root', '2026-01-02T00:00:00Z')",
            )?)
        })
        .await
        .unwrap();
        db.record_push(ws, Some(6), &[RefMove::new("refs/heads/main", ZERO_SHA, A)])
            .await
            .unwrap();
        assert_eq!(db.disarm_override(ws, 6).await.unwrap(), Disarmed::Consumed);
        assert_eq!(
            db.list_overrides(ws).await.unwrap().len(),
            1,
            "a consumed override stays in history"
        );
    }
}
