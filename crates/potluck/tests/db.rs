//! `Db` on a temp dir: open, ping, migrate.

use potluck::db::{Db, DbError};

#[tokio::test]
async fn open_creates_the_file_and_pings() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("server.db");
    let db = Db::open(&path).unwrap();
    db.ping().await.unwrap();
    assert!(path.is_file(), "server.db is created on open");
    db.close();
}

#[tokio::test]
async fn open_fails_when_the_directory_is_missing() {
    let dir = tempfile::tempdir().unwrap();
    // `Db` has no `Debug`, so `unwrap_err` is unavailable; take the error side explicitly.
    let err = Db::open(&dir.path().join("missing").join("server.db"))
        .err()
        .expect("a missing parent fails");
    assert!(matches!(err, DbError::Sqlite(_)), "got {err:?}");
}

fn table_names(path: &std::path::Path) -> Vec<String> {
    let c = rusqlite::Connection::open(path).unwrap();
    let mut stmt = c
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
        .unwrap();
    stmt.query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

#[tokio::test]
async fn a_fresh_database_migrates_to_version_1_and_stays_there() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("server.db")).unwrap();
    assert_eq!(db.schema_version().await.unwrap(), 0, "fresh file");
    assert_eq!(db.migrate().await.unwrap(), 1);
    assert_eq!(db.migrate().await.unwrap(), 1, "a second run is a no-op");
    assert_eq!(db.schema_version().await.unwrap(), 1);
    db.close();
}

#[tokio::test]
async fn version_1_has_the_seven_core_tables() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("server.db");
    let db = Db::open(&path).unwrap();
    db.migrate().await.unwrap();
    db.close();
    assert_eq!(
        table_names(&path),
        [
            "grants",
            "overrides",
            "team_members",
            "teams",
            "tokens",
            "users",
            "workspaces"
        ]
    );
}

#[tokio::test]
async fn a_database_from_a_newer_potluck_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("server.db");
    rusqlite::Connection::open(&path)
        .unwrap()
        .pragma_update(None, "user_version", 99)
        .unwrap();
    let db = Db::open(&path).unwrap();
    let err = db.migrate().await.unwrap_err();
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
    assert!(
        err.to_string().contains("upgrade potluck"),
        "the message tells the operator what to do: {err}"
    );
    db.close();
}

#[tokio::test]
async fn the_schema_enforces_roles_kinds_and_digest_uniqueness() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("server.db");
    let db = Db::open(&path).unwrap();
    db.migrate().await.unwrap();
    db.close();
    let c = rusqlite::Connection::open(&path).unwrap();
    c.execute_batch(
        "INSERT INTO users (id, username, display_name, created_at) VALUES (1, 'ann', 'Ann', '2026-11-15T00:00:00Z');
         INSERT INTO teams (id, slug, name) VALUES (1, 'core', 'Core');
         INSERT INTO workspaces (id, team_id, slug, name, kind, default_branch, created_at)
             VALUES (1, 1, 'api', 'API', 'apinata', 'main', '2026-11-15T00:00:00Z');
         INSERT INTO tokens (user_id, name, prefix, digest, created_at) VALUES (1, 'cli', 'plk_abcd', x'00', '2026-11-15T00:00:00Z');",
    )
    .unwrap();
    let dup = c.execute(
        "INSERT INTO tokens (user_id, name, prefix, digest, created_at) VALUES (1, 'other', 'plk_abcd', x'00', '2026-11-15T00:00:00Z')",
        [],
    );
    assert!(dup.is_err(), "a duplicate digest is refused");
    let bad_role = c.execute("INSERT INTO grants VALUES (1, 'user', 1, 'owner')", []);
    assert!(bad_role.is_err(), "roles are read, write or admin");
    let bad_kind = c.execute(
        "INSERT INTO workspaces (team_id, slug, name, kind, default_branch, created_at) VALUES (1, 'x', 'X', 'other', 'main', 'now')",
        [],
    );
    assert!(bad_kind.is_err(), "kinds are apinata or dbento");
    c.execute("INSERT INTO grants VALUES (1, 'user', 1, 'write')", [])
        .unwrap();
}
