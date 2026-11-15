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
