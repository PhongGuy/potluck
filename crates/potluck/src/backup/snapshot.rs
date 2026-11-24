//! Database snapshot and reconcile: the live workspace listing with its audit mark, a
//! consistent copy of `server.db` by SQLite's backup API, and the repair of that staged copy
//! so it agrees with the staged repositories and carries no armed override.

use std::path::Path;
use std::time::{Duration, Instant};

use rusqlite::backup::{Backup, StepResult};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

/// How long a live-database open waits on a lock, and how long the snapshot retries one.
const BUSY: Duration = Duration::from_secs(5);

/// A database step that failed; the text is SQLite's message and never holds row contents.
#[derive(Debug)]
pub(crate) struct SnapshotError(pub(crate) String);

impl From<rusqlite::Error> for SnapshotError {
    fn from(err: rusqlite::Error) -> Self {
        SnapshotError(err.to_string())
    }
}

/// The audit log's high-water mark when the workspaces were listed.
#[derive(Debug, Clone)]
pub(crate) struct Mark {
    pub(crate) audit_id: i64,
    pub(crate) at: String,
}

/// Which of a workspace's repository the staging directory holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StagedRepo {
    Missing,
    Empty,
    WithRefs,
}

/// What `reconcile` kept and changed. `created` names the rows dropped for want of a staged
/// repository, sorted; rows dropped only by a later audit delete are counted, not named.
#[derive(Debug, Default)]
pub(crate) struct Reconciled {
    pub(crate) kept: Vec<(String, String)>,
    pub(crate) created: Vec<(String, String)>,
    pub(crate) dropped_rows: usize,
    pub(crate) disarmed: usize,
}

/// Opens the live database for reading without ever creating it.
fn open_live(db: &Path) -> Result<Connection, SnapshotError> {
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    conn.busy_timeout(BUSY)?;
    Ok(conn)
}

/// Every workspace as (team slug, ws slug), sorted, and the mark, in one read transaction.
pub(crate) fn live(db: &Path) -> Result<(Vec<(String, String)>, Mark), SnapshotError> {
    let mut conn = open_live(db)?;
    let tx = conn.transaction()?;
    let rows = {
        let mut stmt = tx.prepare(
            "SELECT t.slug, w.slug FROM workspaces w JOIN teams t ON t.id = w.team_id
             ORDER BY t.slug, w.slug",
        )?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?
    };
    let audit_id = tx.query_row("SELECT coalesce(max(id), 0) FROM audit", [], |r| r.get(0))?;
    let mark = Mark {
        audit_id,
        at: crate::clock::now_rfc3339(),
    };
    tx.commit()?;
    Ok((rows, mark))
}

/// Whether the live database still has this workspace row.
pub(crate) fn row_exists(db: &Path, team: &str, ws: &str) -> Result<bool, SnapshotError> {
    let conn = open_live(db)?;
    let found = conn
        .query_row(
            "SELECT 1 FROM workspaces w JOIN teams t ON t.id = w.team_id
             WHERE t.slug = ?1 AND w.slug = ?2",
            params![team, ws],
            |_| Ok(()),
        )
        .optional()?;
    Ok(found.is_some())
}

/// The live database's `user_version`.
pub(crate) fn schema_version(db: &Path) -> Result<u32, SnapshotError> {
    let conn = open_live(db)?;
    let v: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    u32::try_from(v).map_err(|_| SnapshotError(format!("user_version {v} is negative")))
}

/// One `Backup::step(-1)` from `src` into new file `dst` (Busy/Locked retried for 5 s),
/// then `journal_mode=DELETE` and `integrity_check` == "ok" on the copy.
pub(crate) fn snapshot(src: &Path, dst: &Path) -> Result<(), SnapshotError> {
    let source = open_live(src)?;
    let mut copy = Connection::open(dst)?;
    // Copy every page in one step so the copy is one read snapshot; a writer holding the
    // lock is waited out up to the deadline rather than failing the backup at once.
    {
        let backup = Backup::new(&source, &mut copy)?;
        let deadline = Instant::now() + BUSY;
        loop {
            match backup.step(-1)? {
                StepResult::Done => break,
                StepResult::Busy | StepResult::Locked if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                StepResult::Busy | StepResult::Locked => {
                    return Err(SnapshotError(
                        "the database stayed locked for 5 s".to_owned(),
                    ));
                }
                _ => {}
            }
        }
    }
    // A WAL-mode source copies as WAL; the archived file must stand alone.
    let mode: String = copy.query_row("PRAGMA journal_mode=DELETE", [], |r| r.get(0))?;
    if mode != "delete" {
        return Err(SnapshotError(format!("the copy kept journal mode {mode}")));
    }
    let check: String = copy.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    if check != "ok" {
        return Err(SnapshotError(
            "the copy failed its integrity check".to_owned(),
        ));
    }
    Ok(())
}

/// In one transaction on the staged copy (foreign_keys=ON): drop rows that are Missing or
/// have a `workspace.delete` audit row for "{team}/{ws}" with id > mark.audit_id or at >= mark.at;
/// clear import_pending on kept WithRefs rows; delete every override with consumed_at NULL.
pub(crate) fn reconcile(
    staged_db: &Path,
    mark: &Mark,
    staged: &dyn Fn(&str, &str) -> StagedRepo,
) -> Result<Reconciled, SnapshotError> {
    let mut conn = Connection::open_with_flags(staged_db, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    conn.pragma_update(None, "foreign_keys", true)?;
    let tx = conn.transaction()?;
    let rows: Vec<(i64, String, String)> = {
        let mut stmt = tx.prepare(
            "SELECT w.id, t.slug, w.slug FROM workspaces w JOIN teams t ON t.id = w.team_id
             ORDER BY t.slug, w.slug",
        )?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<Vec<_>, _>>()?
    };
    let mut out = Reconciled::default();
    // A row with no staged repository was created after the listing; one deleted after the
    // mark has a repository the copy may hold but the server has already let go of.
    for (id, team, ws) in rows {
        let repo = staged(&team, &ws);
        let deleted_later = repo != StagedRepo::Missing
            && tx
                .query_row(
                    "SELECT 1 FROM audit WHERE action = 'workspace.delete' AND workspace = ?1
                     AND (id > ?2 OR at >= ?3) LIMIT 1",
                    params![format!("{team}/{ws}"), mark.audit_id, mark.at],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
        if repo == StagedRepo::Missing || deleted_later {
            tx.execute("DELETE FROM workspaces WHERE id = ?1", params![id])?;
            out.dropped_rows += 1;
            if repo == StagedRepo::Missing {
                out.created.push((team, ws));
            }
            continue;
        }
        if repo == StagedRepo::WithRefs {
            tx.execute(
                "UPDATE workspaces SET import_pending = 0 WHERE id = ?1",
                params![id],
            )?;
        }
        out.kept.push((team, ws));
    }
    out.disarmed = tx.execute("DELETE FROM overrides WHERE consumed_at IS NULL", [])?;
    tx.commit()?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use rusqlite::Connection;

    use super::*;

    /// A migrated `server.db` in `dir` with user 1, team `core` and workspaces `api` (id 1)
    /// and `web` (id 2, import pending), a grant on each and, on `api`, one armed and one
    /// consumed override.
    async fn seeded(dir: &Path) -> PathBuf {
        let path = dir.join("server.db");
        let db = crate::db::Db::open(&path).unwrap();
        db.migrate().await.unwrap();
        db.close();
        let c = Connection::open(&path).unwrap();
        c.execute_batch(
            "INSERT INTO users (id, username, display_name, created_at) VALUES (1, 'bob', 'bob', '2026-01-01T00:00:00Z');
             INSERT INTO teams (id, slug, name) VALUES (1, 'core', 'Core');
             INSERT INTO workspaces (id, team_id, slug, name, kind, default_branch, import_pending, created_at)
               VALUES (1, 1, 'api', 'api', 'apinata', 'main', 0, '2026-01-01T00:00:00Z'),
                      (2, 1, 'web', 'web', 'apinata', 'main', 1, '2026-01-01T00:00:00Z');
             INSERT INTO grants (workspace_id, subject_kind, subject_id, role) VALUES (1, 'user', 1, 'write'), (2, 'user', 1, 'read');
             INSERT INTO overrides (workspace_id, for_user_id, reason, armed_by, armed_at) VALUES (1, 1, 'armed', 'system', '2026-01-01T00:00:00Z');
             INSERT INTO overrides (workspace_id, for_user_id, reason, armed_by, armed_at, consumed_at, consumed_refs)
               VALUES (1, 1, 'used', 'system', '2026-01-01T00:00:00Z', '2026-01-02T00:00:00Z', '[]');",
        )
        .unwrap();
        path
    }

    fn audit_delete(db: &Path, id: i64, at: &str, workspace: &str) {
        Connection::open(db).unwrap().execute(
            "INSERT INTO audit (id, at, action, actor_kind, workspace) VALUES (?1, ?2, 'workspace.delete', 'system', ?3)",
            rusqlite::params![id, at, workspace],
        ).unwrap();
    }

    fn one(db: &Path, sql: &str) -> i64 {
        Connection::open(db)
            .unwrap()
            .query_row(sql, [], |r| r.get(0))
            .unwrap()
    }

    fn mark(audit_id: i64) -> Mark {
        Mark {
            audit_id,
            at: "2026-06-01T00:00:00Z".to_owned(),
        }
    }

    #[tokio::test]
    async fn live_lists_workspaces_and_marks_the_audit_log() {
        let dir = tempfile::tempdir().unwrap();
        let db = seeded(dir.path()).await;
        let (rows, m) = live(&db).unwrap();
        assert_eq!(
            rows,
            vec![
                ("core".to_owned(), "api".to_owned()),
                ("core".to_owned(), "web".to_owned())
            ],
            "rows"
        );
        assert_eq!(m.audit_id, 0, "empty audit log marks 0");
        assert_eq!(m.at.len(), 20, "an RFC 3339 second stamp: {}", m.at);
        audit_delete(&db, 7, "2026-01-01T00:00:00Z", "core/old");
        assert_eq!(live(&db).unwrap().1.audit_id, 7, "max audit id");
        assert!(row_exists(&db, "core", "api").unwrap(), "api exists");
        assert!(!row_exists(&db, "core", "old").unwrap(), "old does not");
        assert!(schema_version(&db).unwrap() >= 3, "audit migration present");
    }

    #[test]
    fn a_missing_database_is_an_error_and_is_not_created() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.db");
        assert!(live(&path).is_err(), "no database");
        assert!(!path.exists(), "never created");
    }

    #[tokio::test]
    async fn a_snapshot_holds_committed_rows_only_and_leaves_no_wal() {
        let dir = tempfile::tempdir().unwrap();
        let db = seeded(dir.path()).await;
        let writer = Connection::open(&db).unwrap();
        writer.pragma_update(None, "journal_mode", "WAL").unwrap();
        writer
            .execute_batch(
                "BEGIN IMMEDIATE; INSERT INTO teams (slug, name) VALUES ('pending', 'P');",
            )
            .unwrap();
        let copy = dir.path().join("copy.db");
        snapshot(&db, &copy).unwrap();
        writer.execute_batch("COMMIT;").unwrap();
        assert_eq!(
            one(&copy, "SELECT count(*) FROM teams WHERE slug = 'core'"),
            1,
            "committed row"
        );
        assert_eq!(
            one(&copy, "SELECT count(*) FROM teams WHERE slug = 'pending'"),
            0,
            "uncommitted row"
        );
        let mode: String = Connection::open(&copy)
            .unwrap()
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "delete", "rollback journal on the copy");
        assert!(
            !dir.path().join("copy.db-wal").exists(),
            "no wal beside the copy"
        );
    }

    #[tokio::test]
    async fn reconcile_drops_a_row_without_a_staged_repository_with_its_grants_and_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let db = seeded(dir.path()).await;
        let r = reconcile(&db, &mark(0), &|_, ws| {
            if ws == "api" {
                StagedRepo::Missing
            } else {
                StagedRepo::Empty
            }
        })
        .unwrap();
        assert_eq!(r.kept, vec![("core".to_owned(), "web".to_owned())], "kept");
        assert_eq!(r.dropped_rows, 1, "dropped");
        assert_eq!(
            r.created,
            vec![("core".to_owned(), "api".to_owned())],
            "named as created"
        );
        assert_eq!(
            one(&db, "SELECT count(*) FROM workspaces WHERE slug = 'api'"),
            0,
            "row gone"
        );
        assert_eq!(
            one(&db, "SELECT count(*) FROM grants WHERE workspace_id = 1"),
            0,
            "grants cascade"
        );
        assert_eq!(
            one(&db, "SELECT count(*) FROM overrides WHERE workspace_id = 1"),
            0,
            "overrides cascade"
        );
    }

    #[tokio::test]
    async fn reconcile_drops_a_workspace_deleted_after_the_mark_and_keeps_older_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let db = seeded(dir.path()).await;
        audit_delete(&db, 5, "2026-05-01T00:00:00Z", "core/web");
        audit_delete(&db, 6, "2026-05-01T00:00:00Z", "core/api");
        let r = reconcile(&db, &mark(5), &|_, _| StagedRepo::WithRefs).unwrap();
        assert_eq!(
            r.kept,
            vec![("core".to_owned(), "web".to_owned())],
            "only the delete after the mark drops"
        );
        assert!(
            r.created.is_empty(),
            "an audit drop is not a creation: {:?}",
            r.created
        );
        let dir2 = tempfile::tempdir().unwrap();
        let db2 = seeded(dir2.path()).await;
        audit_delete(&db2, 1, "2026-06-01T00:00:00Z", "core/web");
        let r2 = reconcile(&db2, &mark(9), &|_, _| StagedRepo::WithRefs).unwrap();
        assert_eq!(
            r2.kept,
            vec![("core".to_owned(), "api".to_owned())],
            "a stamp at the mark's second drops too"
        );
    }

    #[tokio::test]
    async fn reconcile_disarms_overrides_and_clears_import_only_where_refs_exist() {
        let dir = tempfile::tempdir().unwrap();
        let db = seeded(dir.path()).await;
        Connection::open(&db)
            .unwrap()
            .execute(
                "UPDATE workspaces SET import_pending = 1 WHERE slug = 'api'",
                [],
            )
            .unwrap();
        let r = reconcile(&db, &mark(0), &|_, ws| {
            if ws == "api" {
                StagedRepo::WithRefs
            } else {
                StagedRepo::Empty
            }
        })
        .unwrap();
        assert_eq!(r.disarmed, 1, "one armed override");
        assert_eq!(r.dropped_rows, 0, "nothing dropped");
        assert_eq!(
            one(
                &db,
                "SELECT count(*) FROM overrides WHERE consumed_at IS NULL"
            ),
            0,
            "none armed"
        );
        assert_eq!(
            one(
                &db,
                "SELECT count(*) FROM overrides WHERE consumed_at IS NOT NULL"
            ),
            1,
            "consumed kept"
        );
        assert_eq!(
            one(
                &db,
                "SELECT import_pending FROM workspaces WHERE slug = 'api'"
            ),
            0,
            "pushed: cleared"
        );
        assert_eq!(
            one(
                &db,
                "SELECT import_pending FROM workspaces WHERE slug = 'web'"
            ),
            1,
            "empty: kept"
        );
    }
}
