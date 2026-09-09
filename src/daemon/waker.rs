//! A wake signal for the daemon's main loop. Socket handlers pulse it right after
//! enqueuing work (a switch, a refresh, a fallback-config edit) so the loop applies
//! the op on the NEXT instant instead of sleeping out the remainder of its ~1s tick.
//!
//! Before this, an interactive command enqueued just after a tick waited nearly a
//! full second to land — the daemon's latency floor, not compute. With the wake the
//! op is drained in well under a tick; the timeout still fires the periodic tick, so
//! the 1s usage-refresh cadence is unchanged.

use std::sync::{Condvar, Mutex, PoisonError};
use std::time::Duration;

/// A coalescing, edge-triggered wake. Any number of `wake()` calls before the loop
/// next `wait()`s collapse into a single early tick — and that one tick drains every
/// queued op — so a burst of edits never spins the loop. The pre-wait flag check
/// under the lock means a `wake()` that races just ahead of a `wait()` is not lost.
#[derive(Default)]
pub(crate) struct TickWaker {
    woken: Mutex<bool>,
    cv: Condvar,
}

impl TickWaker {
    /// Signal the loop to tick now. Cheap and thread-safe. A poisoned lock is
    /// recovered rather than panicked — a dropped signal costs at most one tick of
    /// latency (the `wait` timeout still fires).
    pub(crate) fn wake(&self) {
        let mut woken = self.woken.lock().unwrap_or_else(PoisonError::into_inner);
        *woken = true;
        self.cv.notify_one();
    }

    /// Block until woken or `timeout` elapses, then consume the signal. Returns in
    /// both cases — the caller ticks regardless (timeout ⇒ the periodic tick,
    /// woken ⇒ an enqueued op landed). `wait_timeout_while` re-checks the flag, so
    /// spurious wakeups don't return early and a wake delivered before the wait is
    /// observed immediately.
    pub(crate) fn wait(&self, timeout: Duration) {
        let woken = self.woken.lock().unwrap_or_else(PoisonError::into_inner);
        let (mut woken, _) = self
            .cv
            .wait_timeout_while(woken, timeout, |w| !*w)
            .unwrap_or_else(PoisonError::into_inner);
        *woken = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Instant;

    /// A wake delivered before the wait returns (nearly) immediately — no lost signal.
    #[test]
    fn wake_before_wait_returns_immediately() {
        let w = TickWaker::default();
        w.wake();
        let t = Instant::now();
        w.wait(Duration::from_secs(10));
        assert!(
            t.elapsed() < Duration::from_millis(500),
            "should not have blocked"
        );
    }

    /// With no wake, `wait` returns after ~timeout (the periodic tick still fires).
    #[test]
    fn wait_without_wake_times_out() {
        let w = TickWaker::default();
        let t = Instant::now();
        w.wait(Duration::from_millis(60));
        let e = t.elapsed();
        assert!(
            e >= Duration::from_millis(55),
            "returned before timeout: {e:?}"
        );
        assert!(e < Duration::from_secs(2), "waited far past timeout: {e:?}");
    }

    /// A wake from another thread during the wait returns early — the interactive path.
    #[test]
    fn cross_thread_wake_returns_early() {
        let w = Arc::new(TickWaker::default());
        let w2 = Arc::clone(&w);
        let t = Instant::now();
        let h = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            w2.wake();
        });
        w.wait(Duration::from_secs(10));
        assert!(
            t.elapsed() < Duration::from_secs(1),
            "wake did not interrupt the wait"
        );
        h.join().unwrap();
    }

    /// The consume is one-shot: after a woken wait, the next wait blocks to timeout.
    #[test]
    fn signal_is_consumed_after_one_wait() {
        let w = TickWaker::default();
        w.wake();
        w.wait(Duration::from_secs(10)); // consumes the signal
        let t = Instant::now();
        w.wait(Duration::from_millis(60)); // no fresh signal → times out
        assert!(
            t.elapsed() >= Duration::from_millis(55),
            "signal was not consumed"
        );
    }
}
