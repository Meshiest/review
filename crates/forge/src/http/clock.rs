use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Time for the rate limiter and retries, so tests run on a fake one.
pub trait Clock: Send + Sync + 'static {
    /// Monotonic time since the clock's own start.
    fn now(&self) -> Duration;
    /// Wall-clock time since the Unix epoch, for `x-ratelimit-reset`.
    fn unix_time(&self) -> Duration;
    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send>>;
}

/// The real clock; sleeps on the tokio timer.
pub struct TokioClock {
    start: tokio::time::Instant,
}

impl TokioClock {
    #[must_use]
    pub fn new() -> Self {
        Self {
            start: tokio::time::Instant::now(),
        }
    }
}

impl Default for TokioClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for TokioClock {
    fn now(&self) -> Duration {
        self.start.elapsed()
    }

    fn unix_time(&self) -> Duration {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
    }

    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(tokio::time::sleep(duration))
    }
}

/// A fake clock: `sleep` returns at once and moves time forward, and every
/// sleep is recorded, so retry and rate-limit tests need no wall clock.
pub struct ManualClock {
    state: Mutex<ManualState>,
}

struct ManualState {
    now: Duration,
    unix_offset: Duration,
    sleeps: Vec<Duration>,
}

impl ManualClock {
    /// Starts at monotonic zero and the given Unix time.
    #[must_use]
    pub fn new(unix_time: Duration) -> Self {
        Self {
            state: Mutex::new(ManualState {
                now: Duration::ZERO,
                unix_offset: unix_time,
                sleeps: Vec::new(),
            }),
        }
    }

    pub fn advance(&self, by: Duration) {
        self.lock().now += by;
    }

    /// Every sleep so far, in order.
    #[must_use]
    pub fn sleeps(&self) -> Vec<Duration> {
        self.lock().sleeps.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ManualState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Duration {
        self.lock().now
    }

    fn unix_time(&self) -> Duration {
        let state = self.lock();
        state.unix_offset + state.now
    }

    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        let mut state = self.lock();
        state.now += duration;
        state.sleeps.push(duration);
        Box::pin(std::future::ready(()))
    }
}
