//! The audit pruner: rows past their class's retention go at server start.

use std::time::Duration;

use potluck::db::Db;
use potluck::db::audit::AuditFilter;
use potluck::server::start;

use crate::common::config_for;

#[tokio::test]
async fn start_prunes_old_rows_of_each_class_and_keeps_recent_ones() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("server.db");
    {
        let db = Db::open(&path).unwrap();
        db.migrate().await.unwrap();
        db.close();
    }
    let recent = potluck::clock::now_rfc3339();
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        for (at, action) in [
            ("2000-01-01T00:00:00Z", "user.create"),
            ("2000-01-01T00:00:00Z", "session.failed"),
            (recent.as_str(), "user.create"),
            (recent.as_str(), "session.failed"),
        ] {
            c.execute(
                "INSERT INTO audit (at, action, actor_kind) VALUES (?1, ?2, 'system')",
                [at, action],
            )
            .unwrap();
        }
    }
    let running = start(&config_for(dir.path())).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let rows = running.db().audit(AuditFilter::new()).await.unwrap();
        if rows.len() == 2 {
            assert!(
                rows.iter().all(|r| r.at() == recent),
                "only the recent rows stay"
            );
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "pruned within 10 s: {rows:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    running.shutdown().await;
}
