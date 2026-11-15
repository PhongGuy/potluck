//! Schema versioning: `PRAGMA user_version` counts the applied steps of [`MIGRATIONS`].

use rusqlite::Connection;

use super::DbError;

/// The schema steps in order; step `n` (1-based) brings the database to version `n`.
pub(crate) const MIGRATIONS: &[&str] = &[include_str!("migrations/0001_init.sql")];

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
                    supported: 1
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
            1,
            "schema version 1 applies cleanly"
        );
    }
}
