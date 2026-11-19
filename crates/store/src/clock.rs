use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use forge::model::Timestamp;

/// Wall-clock time for write stamps, pruning and file names, so tests run on
/// a fake one.
pub trait Clock: Send + Sync + 'static {
    /// Milliseconds since the Unix epoch.
    fn now_ms(&self) -> i64;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        let since_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        i64::try_from(since_epoch.as_millis()).unwrap_or(i64::MAX)
    }
}

/// A clock that only moves when told to.
#[derive(Debug)]
pub struct ManualClock {
    ms: AtomicI64,
}

impl ManualClock {
    #[must_use]
    pub fn new(start: Timestamp) -> Self {
        Self {
            ms: AtomicI64::new(start.timestamp_millis()),
        }
    }

    pub fn advance(&self, by: Duration) {
        let by = i64::try_from(by.as_millis()).unwrap_or(i64::MAX);
        self.ms.fetch_add(by, Ordering::SeqCst);
    }

    pub fn set(&self, to: Timestamp) {
        self.ms.store(to.timestamp_millis(), Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> i64 {
        self.ms.load(Ordering::SeqCst)
    }
}

pub(crate) fn to_ms(ts: Timestamp) -> i64 {
    ts.timestamp_millis()
}

pub(crate) fn from_ms(ms: i64) -> Timestamp {
    Timestamp::from_timestamp_millis(ms).unwrap_or_default()
}
