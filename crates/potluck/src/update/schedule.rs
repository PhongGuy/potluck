//! The daily update check: a first check shortly after start, then one a day with jitter, until
//! the server stops.

use std::time::Duration;

use tokio::sync::watch;
use tokio_util::task::TaskTracker;

use super::UpdateChecker;

/// The wait before the first check after start.
pub(crate) const FIRST_CHECK: Duration = Duration::from_secs(5 * 60);
/// The base wait between checks.
pub(crate) const PERIOD: Duration = Duration::from_secs(24 * 60 * 60);
/// The most jitter added to each period, so a fleet does not check in lockstep.
pub(crate) const JITTER_MAX: Duration = Duration::from_secs(60 * 60);

/// `period` plus `random` reduced to whole seconds in 0..=jitter_max.
pub(crate) fn jittered(period: Duration, jitter_max: Duration, random: u64) -> Duration {
    let extra = random % jitter_max.as_secs().saturating_add(1);
    period + Duration::from_secs(extra)
}

/// Waits `first`, runs `tick`, then waits `next()` before each further tick, until `stop` turns
/// true or its sender is gone; a stop ends a wait or a running tick at once.
pub(crate) async fn run_daily<N, F, Fut>(
    first: Duration,
    mut next: N,
    mut stop: watch::Receiver<bool>,
    mut tick: F,
) where
    N: FnMut() -> Duration,
    F: FnMut() -> Fut,
    Fut: Future<Output = ()>,
{
    let mut delay = first;
    loop {
        tokio::select! {
            // wait_for sees a value that is already true, and errs once the sender is gone.
            _ = stop.wait_for(|stopped| *stopped) => return,
            () = async { tokio::time::sleep(delay).await; tick().await } => {}
        }
        delay = next();
    }
}

/// Spawns the daily check loop for `checker` on `tracker`, ending when `stop` turns true.
pub(crate) fn spawn_daily(
    checker: UpdateChecker,
    tracker: &TaskTracker,
    stop: watch::Receiver<bool>,
) {
    // An RNG failure falls back to no jitter rather than skipping the check.
    let next = || jittered(PERIOD, JITTER_MAX, getrandom::u64().unwrap_or(0));
    tracker.spawn(run_daily(FIRST_CHECK, next, stop, move || {
        let checker = checker.clone();
        async move {
            // `check` logs its own outcome.
            let _ = checker.check().await;
        }
    }));
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use tokio::sync::watch;

    use super::{jittered, run_daily};

    #[test]
    fn the_jitter_adds_zero_to_sixty_minutes() {
        let day = Duration::from_secs(24 * 3600);
        let hour = Duration::from_secs(3600);
        assert_eq!(jittered(day, hour, 0), day);
        assert_eq!(jittered(day, hour, 3600), day + hour);
        assert_eq!(jittered(day, hour, 3601), day);
        assert!(jittered(day, hour, u64::MAX) <= day + hour);
    }

    #[tokio::test]
    async fn the_loop_waits_the_first_delay_then_ticks_each_period() {
        let ticks = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&ticks);
        let (stop, rx) = watch::channel(false);
        let task = tokio::spawn(run_daily(
            Duration::from_millis(100),
            || Duration::from_millis(20),
            rx,
            move || {
                let counter = Arc::clone(&counter);
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                }
            },
        ));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            ticks.load(Ordering::SeqCst),
            0,
            "nothing before the first delay"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        let seen = ticks.load(Ordering::SeqCst);
        assert!(seen >= 3, "ticks after the first delay: {seen}");
        stop.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("the loop stops")
            .unwrap();
    }

    #[tokio::test]
    async fn a_stop_ends_the_loop_during_a_wait_or_a_tick_and_a_dropped_sender_too() {
        let hour = Duration::from_secs(3600);
        let (stop, rx) = watch::channel(false);
        let waiting = tokio::spawn(run_daily(hour, move || hour, rx, || async {}));
        stop.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("stops during the wait")
            .unwrap();

        let (stop, rx) = watch::channel(false);
        let ticking = tokio::spawn(run_daily(
            Duration::ZERO,
            move || hour,
            rx,
            move || tokio::time::sleep(hour),
        ));
        tokio::time::sleep(Duration::from_millis(50)).await;
        stop.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(1), ticking)
            .await
            .expect("stops during a tick")
            .unwrap();

        let (stop, rx) = watch::channel(false);
        let orphaned = tokio::spawn(run_daily(hour, move || hour, rx, || async {}));
        drop(stop);
        tokio::time::timeout(Duration::from_secs(1), orphaned)
            .await
            .expect("stops when the sender is gone")
            .unwrap();
    }
}
