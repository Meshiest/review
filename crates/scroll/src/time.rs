//! The clock the scroll logic runs on.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Time since an arbitrary fixed point. The logic takes it as an argument, so
/// tests drive it with plain durations instead of sleeping.
pub type Time = Duration;

/// The current time on the real clock.
#[must_use]
pub fn now() -> Time {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed()
}
