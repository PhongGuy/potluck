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
async fn a_fresh_database_migrates_to_version_4_and_stays_there() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("server.db")).unwrap();
    assert_eq!(db.schema_version().await.unwrap(), 0, "fresh file");
    assert_eq!(db.migrate().await.unwrap(), 4);
    assert_eq!(db.migrate().await.unwrap(), 4, "a second run is a no-op");
    assert_eq!(db.schema_version().await.unwrap(), 4);
    db.close();
}

#[tokio::test]
async fn the_shipped_schema_has_its_tables() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("server.db");
    let db = Db::open(&path).unwrap();
    db.migrate().await.unwrap();
    db.close();
    assert_eq!(
        table_names(&path),
        [
            "audit",
            "grants",
            "oidc_links",
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
                supported: 4
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

/// Rows of `sql` against a migrated fresh database, each row's columns joined with `|`.
async fn migrated_rows(sql: &str) -> Vec<String> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("server.db");
    let db = Db::open(&path).unwrap();
    db.migrate().await.unwrap();
    db.close();
    let c = rusqlite::Connection::open(&path).unwrap();
    let mut stmt = c.prepare(sql).unwrap();
    let width = stmt.column_count();
    stmt.query_map([], |r| {
        (0..width)
            .map(|i| r.get::<_, String>(i))
            .collect::<Result<Vec<_>, _>>()
            .map(|cols| cols.join("|"))
    })
    .unwrap()
    .map(Result::unwrap)
    .collect()
}

#[tokio::test]
async fn every_table_is_strict() {
    let loose = migrated_rows(
        "SELECT name FROM pragma_table_list
         WHERE schema = 'main' AND type = 'table' AND name NOT LIKE 'sqlite_%' AND strict = 0",
    )
    .await;
    assert_eq!(loose, Vec::<String>::new(), "tables without STRICT");
    let strict = migrated_rows(
        "SELECT count(*) || '' FROM pragma_table_list
         WHERE schema = 'main' AND type = 'table' AND name NOT LIKE 'sqlite_%' AND strict = 1",
    )
    .await;
    assert_eq!(strict, ["9"], "all nine tables were checked");
}

#[tokio::test]
async fn the_unique_constraints_conflict_mapping_relies_on_are_pinned() {
    // Each row is table|origin|columns: `u` a UNIQUE constraint, `pk` a composite primary key.
    let unique = migrated_rows(
        "SELECT t.name, il.origin,
                (SELECT group_concat(name, ',') FROM
                    (SELECT ii.name FROM pragma_index_info(il.name) ii ORDER BY ii.seqno))
         FROM sqlite_schema t, pragma_index_list(t.name) il
         WHERE t.type = 'table' AND il.\"unique\" = 1
         ORDER BY t.name, il.origin, 3",
    )
    .await;
    assert_eq!(
        unique,
        [
            "grants|pk|workspace_id,subject_kind,subject_id",
            "oidc_links|pk|issuer,subject",
            "oidc_links|u|issuer,user_id",
            "team_members|pk|team_id,user_id",
            "teams|u|slug",
            "tokens|u|digest",
            "users|u|username",
            "workspaces|u|team_id,slug",
        ]
    );
}
