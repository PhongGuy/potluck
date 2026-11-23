//! Audit retention: each class of row is pruned at start and then daily against its own days.

use std::time::Duration;

use tokio::sync::watch;
use tokio_util::task::TaskTracker;

use crate::clock;
use crate::config::Config;
use crate::db::audit::AuditClass;
use crate::db::{Db, DbError};
use crate::update::schedule;

/// The most rows one delete takes, so a large backlog never holds the writer for long.
pub(crate) const BATCH: u32 = 5000;

/// Prunes both classes against `now`, each in BATCH-sized calls until one comes back short;
/// a class with 0 days is skipped. Returns the rows removed.
pub(crate) async fn prune_pass(
    db: &Db,
    now: &str,
    change_days: u32,
    failed_days: u32,
) -> Result<usize, DbError> {
    let mut removed = 0;
    for (class, days) in [
        (AuditClass::Changes, change_days),
        (AuditClass::SignInFailures, failed_days),
    ] {
        if days == 0 {
            continue;
        }
        let Some(cutoff) = clock::shift_secs(now, -(i64::from(days) * 86_400)) else {
            tracing::error!(
                now,
                "audit pruning skipped: the current time does not parse"
            );
            continue;
        };
        loop {
            let gone = db.prune_audit(class, &cutoff, BATCH).await?;
            removed += gone;
            if gone < BATCH as usize {
                break;
            }
        }
    }
    Ok(removed)
}

/// Runs `prune_pass` at once and then every 24 h on `tracker` until `stop` turns true; a failed
/// pass is logged at warn.
pub(crate) fn spawn_pruner(
    db: Db,
    config: &Config,
    tracker: &TaskTracker,
    stop: watch::Receiver<bool>,
) {
    let (change_days, failed_days) = (
        config.audit_retention_days(),
        config.audit_failed_retention_days(),
    );
    tracker.spawn(schedule::run_daily(
        Duration::ZERO,
        || schedule::PERIOD,
        stop,
        move || {
            let db = db.clone();
            async move {
                match prune_pass(&db, &clock::now_rfc3339(), change_days, failed_days).await {
                    Ok(0) => {}
                    Ok(removed) => tracing::info!(removed, "pruned the audit log"),
                    Err(err) => tracing::warn!("audit pruning failed: {err}"),
                }
            }
        },
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::audit::AuditFilter;

    async fn db_with(rows: &[(&'static str, &'static str)]) -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("server.db")).unwrap();
        db.migrate().await.unwrap();
        let rows = rows.to_vec();
        db.call(move |c| {
            for (at, action) in rows {
                c.execute(
                    "INSERT INTO audit (at, action, actor_kind) VALUES (?1, ?2, 'system')",
                    [at, action],
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();
        (dir, db)
    }

    const NOW: &str = "2026-06-01T00:00:00Z";

    #[tokio::test]
    async fn each_class_uses_its_own_days() {
        let (_dir, db) = db_with(&[
            ("2026-05-20T00:00:00Z", "session.failed"), // 12 days old: past the 7 passed below
            ("2026-05-20T00:00:00Z", "user.create"),
            ("2025-05-01T00:00:00Z", "user.create"), // past 365
        ])
        .await;
        assert_eq!(prune_pass(&db, NOW, 365, 7).await.unwrap(), 2);
        let left = db.audit(AuditFilter::new()).await.unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(
            (left[0].at(), left[0].action()),
            ("2026-05-20T00:00:00Z", "user.create")
        );
    }

    #[tokio::test]
    async fn zero_days_keeps_a_class_forever() {
        let (_dir, db) = db_with(&[
            ("2000-01-01T00:00:00Z", "session.failed"),
            ("2000-01-01T00:00:00Z", "user.create"),
        ])
        .await;
        assert_eq!(prune_pass(&db, NOW, 0, 0).await.unwrap(), 0);
        assert_eq!(db.audit(AuditFilter::new()).await.unwrap().len(), 2);
    }
}
