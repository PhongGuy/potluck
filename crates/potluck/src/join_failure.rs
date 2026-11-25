//! Logging for a spawned task that did not finish.

#[cfg(test)]
use std::sync::{Arc, Mutex};

use tokio::task::JoinError;
use tracing::Level;

/// Logs a task that did not finish, with `message` and whether it panicked, at `Level::ERROR`
/// when `level` is that and at `Level::WARN` otherwise. The join error itself is never logged:
/// a panic payload is arbitrary text and can name paths.
pub(crate) fn log_join_failure(err: &JoinError, level: Level, message: &str) {
    if level == Level::ERROR {
        tracing::error!(panicked = err.is_panic(), "{message}");
    } else {
        tracing::warn!(panicked = err.is_panic(), "{message}");
    }
}

/// A log subscriber whose output tests can read back.
#[cfg(test)]
pub(crate) fn capturing() -> (Arc<Mutex<Vec<u8>>>, impl tracing::Subscriber + Send + Sync) {
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let logs = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&logs);
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(move || Sink(Arc::clone(&sink)))
        .finish();
    (logs, subscriber)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captures what `log_join_failure` writes for `err` at `level`.
    fn captured(err: &JoinError, level: Level, message: &str) -> String {
        let (logs, subscriber) = capturing();
        tracing::subscriber::with_default(subscriber, || log_join_failure(err, level, message));
        String::from_utf8(logs.lock().unwrap().clone()).unwrap()
    }

    async fn panicked(payload: &'static str) -> JoinError {
        tokio::task::spawn_blocking(move || std::panic::panic_any(payload))
            .await
            .unwrap_err()
    }

    #[tokio::test]
    async fn a_panicked_task_logs_only_that_it_panicked() {
        let err = panicked("/srv/secret/path in the payload").await;
        let line = captured(&err, Level::WARN, "startup sweep task failed");
        assert!(line.contains("WARN"), "{line}");
        assert!(line.contains("startup sweep task failed"), "{line}");
        assert!(line.contains("panicked=true"), "{line}");
        assert!(!line.contains("/srv/secret/path"), "payload leaked: {line}");
    }

    #[tokio::test]
    async fn an_error_level_failure_logs_at_error_without_the_payload() {
        let err = panicked("/srv/secret/path in the payload").await;
        let line = captured(&err, Level::ERROR, "workspace creation task failed");
        assert!(line.contains("ERROR"), "{line}");
        assert!(line.contains("workspace creation task failed"), "{line}");
        assert!(line.contains("panicked=true"), "{line}");
        assert!(!line.contains("/srv/secret/path"), "payload leaked: {line}");
    }

    #[tokio::test]
    async fn a_cancelled_task_logs_that_it_did_not_panic() {
        let task = tokio::spawn(std::future::pending::<()>());
        task.abort();
        let err = task.await.unwrap_err();
        let line = captured(&err, Level::WARN, "sweep task failed");
        assert!(line.contains("panicked=false"), "{line}");
    }
}
