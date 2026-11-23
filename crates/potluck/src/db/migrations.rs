//! Schema versioning: `PRAGMA user_version` counts the applied steps of [`MIGRATIONS`].

use rusqlite::Connection;

use super::DbError;

/// The schema steps in order; step `n` (1-based) brings the database to version `n`.
pub(crate) const MIGRATIONS: &[&str] = &[
    include_str!("migrations/0001_init.sql"),
    include_str!("migrations/0002_grant_subjects.sql"),
    include_str!("migrations/0003_audit.sql"),
];

/// Applies the migrations past `PRAGMA user_version` in one transaction and sets
/// `user_version` to `migrations.len()`. A database already past that version is refused.
pub(crate) fn migrate_with(conn: &mut Connection, migrations: &[&str]) -> Result<u32, DbError> {
    let found = user_version(conn)?;
    let supported = u32::try_from(migrations.len()).map_err(|e| DbError::Sqlite(e.to_string()))?;
    if found > supported {
        return Err(DbError::SchemaTooNew { found, supported });
    }
    // One transaction: a failing step leaves both the tables and user_version as they were.
    let tx = conn.transaction()?;
    for sql in &migrations[found as usize..] {
        tx.execute_batch(sql)?;
    }
    tx.pragma_update(None, "user_version", supported)?;
    tx.commit()?;
    Ok(supported)
}

/// The schema version recorded in the file header.
pub(crate) fn user_version(conn: &Connection) -> Result<u32, DbError> {
    let v: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    u32::try_from(v).map_err(|_| DbError::Sqlite(format!("user_version {v} is negative")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> rusqlite::Connection {
        rusqlite::Connection::open_in_memory().unwrap()
    }

    #[test]
    fn applies_only_the_pending_migrations() {
        let mut c = conn();
        assert_eq!(
            migrate_with(&mut c, &["CREATE TABLE a (x INTEGER)"]).unwrap(),
            1,
            "first run"
        );
        // Re-running the first step would fail with "table a already exists"; it must be skipped.
        assert_eq!(
            migrate_with(
                &mut c,
                &["CREATE TABLE a (x INTEGER)", "CREATE TABLE b (y INTEGER)"]
            )
            .unwrap(),
            2,
            "only the pending step runs"
        );
        assert_eq!(user_version(&c).unwrap(), 2, "user_version follows");
    }

    #[test]
    fn a_failing_step_rolls_everything_back() {
        let mut c = conn();
        let err = migrate_with(
            &mut c,
            &["CREATE TABLE a (x INTEGER)", "CREATE TABLE a (x INTEGER)"],
        )
        .unwrap_err();
        assert!(matches!(err, DbError::Sqlite(_)), "got {err:?}");
        assert_eq!(user_version(&c).unwrap(), 0, "user_version is untouched");
        let tables: i64 = c
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tables, 0, "the first step's table was rolled back");
    }

    #[test]
    fn a_newer_schema_is_refused() {
        let mut c = conn();
        c.pragma_update(None, "user_version", 99).unwrap();
        let err = migrate_with(&mut c, MIGRATIONS).unwrap_err();
        assert!(
            matches!(
                err,
                DbError::SchemaTooNew {
                    found: 99,
                    supported: 3
                }
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn the_shipped_migrations_apply_to_an_empty_database() {
        let mut c = conn();
        assert_eq!(
            migrate_with(&mut c, MIGRATIONS).unwrap(),
            3,
            "schema version 3 applies cleanly"
        );
    }

    #[test]
    fn version_2_drops_grants_whose_subject_is_gone() {
        let mut c = conn();
        migrate_with(&mut c, &MIGRATIONS[..1]).unwrap();
        c.execute_batch(
            "INSERT INTO users (id, username, display_name, created_at) VALUES (1, 'bob', 'Bob', 'x');
             INSERT INTO teams (id, slug, name) VALUES (1, 'core', 'Core');
             INSERT INTO workspaces (id, team_id, slug, name, kind, default_branch, created_at)
                 VALUES (1, 1, 'api', 'API', 'apinata', 'main', 'x');
             INSERT INTO grants (workspace_id, subject_kind, subject_id, role) VALUES
                 (1, 'user', 1, 'read'), (1, 'team', 1, 'write'),
                 (1, 'user', 7, 'admin'), (1, 'team', 7, 'admin');",
        )
        .unwrap();
        assert_eq!(migrate_with(&mut c, MIGRATIONS).unwrap(), 3);
        let mut stmt = c
            .prepare("SELECT subject_kind, subject_id FROM grants ORDER BY 1, 2")
            .unwrap();
        let left: Vec<(String, i64)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            left,
            [("team".to_owned(), 1), ("user".to_owned(), 1)],
            "only the grants of subjects that exist survive"
        );
    }

    #[test]
    fn version_3_adds_the_audit_table_with_its_actor_check() {
        let mut c = conn();
        assert_eq!(migrate_with(&mut c, MIGRATIONS).unwrap(), 3);
        c.execute(
            "INSERT INTO audit (at, action, actor_kind) VALUES ('2026-01-01T00:00:00Z', 'x.y', 'system')",
            [],
        )
        .unwrap();
        let err = c
            .execute(
                "INSERT INTO audit (at, action, actor_kind) VALUES ('2026-01-01T00:00:00Z', 'x.y', 'robot')",
                [],
            )
            .unwrap_err();
        assert!(err.to_string().contains("CHECK"), "{err}");
        let indexes: i64 = c
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'index' AND tbl_name = 'audit' \
                 AND name IN ('audit_actor', 'audit_workspace', 'audit_action', 'audit_at')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(indexes, 4);
    }

    #[test]
    fn version_3_applies_over_a_version_2_database() {
        let mut c = conn();
        migrate_with(&mut c, &MIGRATIONS[..2]).unwrap();
        assert_eq!(migrate_with(&mut c, MIGRATIONS).unwrap(), 3);
        assert_eq!(user_version(&c).unwrap(), 3);
    }
}
