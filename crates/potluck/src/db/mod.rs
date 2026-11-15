//! The one connection to `server.db`, owned by a dedicated thread and reached through an
//! async request/reply `Db::call`.

use std::path::Path;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use rusqlite::Connection;

pub mod accounts;
pub mod grants;
mod migrations;
pub mod overrides;
pub mod teams;
pub mod workspaces;

/// A handle to the one connection to `server.db`, owned by a dedicated thread.
/// Clones share the thread; `close` stops it.
#[derive(Clone)]
#[non_exhaustive]
pub struct Db {
    inner: Arc<Inner>,
}

/// Why a database call failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DbError {
    /// The owning thread has stopped (after `close`), so no call can run.
    #[error("the database thread has stopped")]
    Closed,
    /// SQLite refused something; the text is for the log, never for a client.
    #[error("database error: {0}")]
    Sqlite(String),
    /// The file was written by a newer potluck.
    #[error(
        "database schema version {found} is newer than this build supports ({supported}); upgrade potluck"
    )]
    SchemaTooNew {
        /// The schema version recorded in the file.
        found: u32,
        /// The newest schema version this build knows.
        supported: u32,
    },
    /// A row that must be unique already exists; `what` is client-safe text such as
    /// `username bob is taken`.
    #[error("conflict: {what}")]
    Conflict {
        /// What clashed, fit to show the client.
        what: String,
    },
}

impl From<rusqlite::Error> for DbError {
    fn from(err: rusqlite::Error) -> Self {
        DbError::Sqlite(err.to_string())
    }
}

/// Maps a failed write: a UNIQUE or PRIMARY KEY violation becomes `Conflict` with `what()`,
/// any other failure `Sqlite`, as `From<rusqlite::Error>` gives.
pub(crate) fn conflict_on_unique(err: rusqlite::Error, what: impl FnOnce() -> String) -> DbError {
    // Match the extended codes, not ConstraintViolation: CHECK, NOT NULL and foreign-key
    // failures share that primary code and are server faults, not duplicates.
    match err.sqlite_extended_error_code() {
        Some(
            rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE | rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY,
        ) => DbError::Conflict { what: what() },
        _ => DbError::from(err),
    }
}

type Job = Box<dyn FnOnce(&mut Connection) + Send + 'static>;

struct Inner {
    // Taking the sender closes the channel; the thread's `for job in rx` then ends.
    tx: Mutex<Option<Sender<Job>>>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Inner {
    // Close the channel first so the thread drains its queue and exits, then join it so the
    // file is released before we return. A poisoned lock still yields its value, so the
    // sender is always dropped and the join cannot hang.
    fn stop(&self) {
        self.tx
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let handle = self
            .thread
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Db {
    /// Opens (creating if absent) the database file and starts the owning thread. The parent
    /// directory must exist. Sets `journal_mode=WAL`, `synchronous=NORMAL`, `foreign_keys=ON`
    /// and a 5 s busy timeout.
    pub fn open(path: &Path) -> Result<Db, DbError> {
        // Open and configure on the caller so every failure surfaces synchronously, then move
        // the connection into the thread that owns it from here on.
        let mut conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.busy_timeout(Duration::from_secs(5))?;

        let (tx, rx) = std::sync::mpsc::channel::<Job>();
        let thread = std::thread::Builder::new()
            .name("potluck-db".to_owned())
            .spawn(move || {
                for job in rx {
                    job(&mut conn);
                }
            })
            .map_err(|err| {
                DbError::Sqlite(format!("could not start the database thread: {err}"))
            })?;
        Ok(Db {
            inner: Arc::new(Inner {
                tx: Mutex::new(Some(tx)),
                thread: Mutex::new(Some(thread)),
            }),
        })
    }

    /// Runs `f` on the database thread and returns its result. The closure returns `DbError`
    /// so it can carry domain failures as well as SQLite ones; `?` on a `rusqlite::Result`
    /// inside it converts through `From`.
    pub(crate) async fn call<T, F>(&self, f: F) -> Result<T, DbError>
    where
        F: FnOnce(&mut Connection) -> Result<T, DbError> + Send + 'static,
        T: Send + 'static,
    {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let job: Job = Box::new(move |conn| {
            // A dropped receiver means the caller went away; nothing to report.
            let _ = reply_tx.send(f(conn));
        });
        // std mpsc is unbounded, so `send` never blocks the async task; the lock is held only
        // for the send.
        self.inner
            .tx
            .lock()
            .map_err(|_| DbError::Closed)?
            .as_ref()
            .ok_or(DbError::Closed)?
            .send(job)
            .map_err(|_| DbError::Closed)?;
        reply_rx.await.map_err(|_| DbError::Closed)?
    }

    /// `SELECT 1`: proves the thread and the connection answer.
    pub async fn ping(&self) -> Result<(), DbError> {
        self.call(|conn| {
            conn.query_row("SELECT 1", [], |_| Ok(()))?;
            Ok(())
        })
        .await
    }

    /// Runs the shipped migrations; returns the schema version afterwards.
    pub async fn migrate(&self) -> Result<u32, DbError> {
        self.call(|c| migrations::migrate_with(c, migrations::MIGRATIONS))
            .await
    }

    /// `PRAGMA user_version`.
    pub async fn schema_version(&self) -> Result<u32, DbError> {
        self.call(|c| migrations::user_version(c)).await
    }

    /// Stops the thread; every later call fails with `Closed`. Idempotent.
    pub fn close(&self) {
        self.inner.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("server.db")).unwrap();
        (dir, db)
    }

    #[tokio::test]
    async fn call_returns_the_closure_value() {
        let (_dir, db) = temp_db();
        let two: i64 = db
            .call(|c| Ok(c.query_row("SELECT 1 + 1", [], |r| r.get(0))?))
            .await
            .unwrap();
        assert_eq!(
            two, 2,
            "the closure's value comes back through the reply channel"
        );
    }

    #[tokio::test]
    async fn calls_run_in_order_on_one_connection() {
        let (_dir, db) = temp_db();
        db.call(|c| Ok(c.execute_batch("CREATE TABLE t (n INTEGER); INSERT INTO t VALUES (7)")?))
            .await
            .unwrap();
        let n: i64 = db
            .call(|c| Ok(c.query_row("SELECT n FROM t", [], |r| r.get(0))?))
            .await
            .unwrap();
        assert_eq!(n, 7, "the second call sees the first call's write");
    }

    #[tokio::test]
    async fn sqlite_errors_become_db_error_sqlite() {
        let (_dir, db) = temp_db();
        let err = db
            .call(|c| Ok(c.execute_batch("SELECT * FROM missing")?))
            .await
            .unwrap_err();
        assert!(matches!(err, DbError::Sqlite(_)), "got {err:?}");
        assert!(
            err.to_string().contains("missing"),
            "the message names the table: {err}"
        );
    }

    #[tokio::test]
    async fn domain_errors_pass_through_unchanged() {
        let (_dir, db) = temp_db();
        let err = db
            .call(|_| {
                Err::<(), _>(DbError::SchemaTooNew {
                    found: 2,
                    supported: 1,
                })
            })
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                DbError::SchemaTooNew {
                    found: 2,
                    supported: 1
                }
            ),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn open_enables_wal_and_foreign_keys() {
        let (_dir, db) = temp_db();
        let mode: String = db
            .call(|c| Ok(c.pragma_query_value(None, "journal_mode", |r| r.get(0))?))
            .await
            .unwrap();
        assert_eq!(mode, "wal", "journal mode");
        let fk: i64 = db
            .call(|c| Ok(c.pragma_query_value(None, "foreign_keys", |r| r.get(0))?))
            .await
            .unwrap();
        assert_eq!(fk, 1, "foreign keys are enforced");
    }

    #[tokio::test]
    async fn close_stops_the_thread_and_later_calls_fail_closed() {
        let (_dir, db) = temp_db();
        let other = db.clone();
        db.close();
        let err = other.ping().await.unwrap_err();
        assert!(matches!(err, DbError::Closed), "got {err:?}");
        db.close();
        other.close();
    }

    #[tokio::test]
    async fn clones_share_the_thread() {
        let (_dir, db) = temp_db();
        let clone = db.clone();
        drop(db);
        clone.ping().await.unwrap();
    }

    #[tokio::test]
    async fn unique_and_primary_key_violations_become_the_given_conflict() {
        let (_dir, db) = temp_db();
        db.call(|c| {
            Ok(c.execute_batch(
                "CREATE TABLE u (k TEXT UNIQUE); INSERT INTO u VALUES ('a');
                 CREATE TABLE p (a INTEGER, b INTEGER, PRIMARY KEY (a, b)); INSERT INTO p VALUES (1, 1);
                 CREATE TABLE r (id INTEGER PRIMARY KEY); INSERT INTO r VALUES (1);",
            )?)
        })
        .await
        .unwrap();
        for sql in [
            "INSERT INTO u VALUES ('a')",
            "INSERT INTO p VALUES (1, 1)",
            "INSERT INTO r VALUES (1)",
        ] {
            let err = db
                .call(move |c| {
                    c.execute(sql, [])
                        .map_err(|e| conflict_on_unique(e, || format!("{sql} clashed")))?;
                    Ok(())
                })
                .await
                .unwrap_err();
            match err {
                DbError::Conflict { what } => assert_eq!(what, format!("{sql} clashed")),
                other => panic!("{sql}: expected Conflict, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn other_failures_stay_sqlite() {
        let (_dir, db) = temp_db();
        db.call(|c| Ok(c.execute_batch("CREATE TABLE c (n INTEGER NOT NULL CHECK (n > 0))")?))
            .await
            .unwrap();
        for sql in [
            "INSERT INTO c VALUES (0)",
            "INSERT INTO c VALUES (NULL)",
            "INSERT INTO missing VALUES (1)",
        ] {
            let err = db
                .call(move |c| {
                    c.execute(sql, [])
                        .map_err(|e| conflict_on_unique(e, || "not a duplicate".to_owned()))?;
                    Ok(())
                })
                .await
                .unwrap_err();
            assert!(matches!(err, DbError::Sqlite(_)), "{sql}: got {err:?}");
        }
    }
}
