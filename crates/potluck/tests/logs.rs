//! A shared thread-local log capture, since a process may install only one global subscriber.

use std::cell::RefCell;
use std::sync::{Arc, Mutex, Once};

thread_local! {
    static CAPTURE: RefCell<Option<Arc<Mutex<Vec<u8>>>>> = const { RefCell::new(None) };
}

/// Writes into the current thread's capture buffer, if it has one, and drops the bytes otherwise.
struct ThreadSink;

impl std::io::Write for ThreadSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        CAPTURE.with(|c| {
            if let Some(captured) = &*c.borrow() {
                captured.lock().unwrap().extend_from_slice(buf);
            }
        });
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// One process-wide subscriber rather than a scoped one: with a single dispatcher, tracing caches
// each callsite's interest from whichever thread reaches it first, so a concurrent test with no
// subscriber could silence the events this test reads. The thread-local writer keeps capture
// per test.
fn install_capturing_subscriber() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(|| ThreadSink)
            .finish();
        tracing::subscriber::set_global_default(subscriber).expect("no other global subscriber");
    });
}

/// Runs `f` and returns, as UTF-8, everything logged on this thread meanwhile, at any level.
pub(crate) fn captured_logs(f: impl FnOnce()) -> String {
    install_capturing_subscriber();
    let captured = Arc::new(Mutex::new(Vec::new()));
    CAPTURE.with(|c| *c.borrow_mut() = Some(Arc::clone(&captured)));
    f();
    CAPTURE.with(|c| *c.borrow_mut() = None);
    String::from_utf8(captured.lock().unwrap().clone()).unwrap()
}
