use std::time::Duration;

/// A per-forge request budget: a token bucket, plus a block set from the
/// server's own quota headers or a `Retry-After`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RateLimit {
    /// Requests that may go at once after an idle period.
    pub burst: u32,
    /// Sustained requests per second.
    pub per_second: f64,
}

impl RateLimit {
    /// Well under each forge's documented limits, so the app is never the
    /// reason a secondary limit trips (GitHub: 900 REST points a minute and
    /// no bursts of mutations; gitlab.com: 2,000 a minute; a self-hosted
    /// Forgejo: unknown, so gentle).
    pub const GITHUB: Self = Self {
        burst: 15,
        per_second: 5.0,
    };
    pub const GITLAB: Self = Self {
        burst: 20,
        per_second: 8.0,
    };
    pub const FORGEJO: Self = Self {
        burst: 10,
        per_second: 5.0,
    };
}

/// Pure state machine over a clock reading, so tests drive time by hand.
#[derive(Clone, Debug)]
pub struct RateLimiter {
    limit: RateLimit,
    tokens: f64,
    last: Duration,
    blocked_until: Duration,
}

impl RateLimiter {
    #[must_use]
    pub fn new(limit: RateLimit, now: Duration) -> Self {
        Self {
            limit,
            tokens: f64::from(limit.burst),
            last: now,
            blocked_until: Duration::ZERO,
        }
    }

    /// Takes one request's permit, or says how long to wait before asking
    /// again.
    ///
    /// # Errors
    /// The wait, when no permit is available at `now`.
    pub fn try_acquire(&mut self, now: Duration) -> Result<(), Duration> {
        if now < self.blocked_until {
            return Err(self.blocked_until - now);
        }
        self.refill(now);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            return Ok(());
        }
        let missing = 1.0 - self.tokens;
        Err(Duration::from_secs_f64(missing / self.limit.per_second))
    }

    /// Stops all requests until `until` (a `Retry-After`, a secondary limit,
    /// or an exhausted quota until its reset).
    pub fn block_until(&mut self, until: Duration) {
        self.blocked_until = self.blocked_until.max(until);
    }

    /// Applies the server's quota headers: with nothing left, block until
    /// the reset.
    pub fn observe_quota(&mut self, remaining: u64, reset_in: Duration, now: Duration) {
        if remaining == 0 {
            self.block_until(now.saturating_add(reset_in));
        }
    }

    fn refill(&mut self, now: Duration) {
        let elapsed = now.saturating_sub(self.last).as_secs_f64();
        self.last = self.last.max(now);
        self.tokens =
            (self.tokens + elapsed * self.limit.per_second).min(f64::from(self.limit.burst));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMIT: RateLimit = RateLimit {
        burst: 3,
        per_second: 2.0,
    };

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn burst_then_waits_for_refill() {
        let mut limiter = RateLimiter::new(LIMIT, Duration::ZERO);
        for _ in 0..3 {
            assert_eq!(limiter.try_acquire(Duration::ZERO), Ok(()));
        }
        assert_eq!(limiter.try_acquire(Duration::ZERO), Err(ms(500)));
        assert_eq!(limiter.try_acquire(ms(250)), Err(ms(250)));
        assert_eq!(limiter.try_acquire(ms(500)), Ok(()));
        assert_eq!(limiter.try_acquire(ms(500)), Err(ms(500)));
    }

    #[test]
    fn refill_is_capped_at_the_burst() {
        let mut limiter = RateLimiter::new(LIMIT, Duration::ZERO);
        let later = Duration::from_secs(3600);
        for _ in 0..3 {
            assert_eq!(limiter.try_acquire(later), Ok(()));
        }
        assert!(limiter.try_acquire(later).is_err());
    }

    #[test]
    fn block_overrides_tokens_and_never_shrinks() {
        let mut limiter = RateLimiter::new(LIMIT, Duration::ZERO);
        limiter.block_until(Duration::from_secs(10));
        limiter.block_until(Duration::from_secs(5));
        assert_eq!(
            limiter.try_acquire(Duration::from_secs(4)),
            Err(Duration::from_secs(6))
        );
        assert_eq!(limiter.try_acquire(Duration::from_secs(10)), Ok(()));
    }

    #[test]
    fn exhausted_quota_blocks_until_reset() {
        let mut limiter = RateLimiter::new(LIMIT, Duration::ZERO);
        limiter.observe_quota(12, Duration::from_secs(60), Duration::ZERO);
        assert_eq!(limiter.try_acquire(Duration::ZERO), Ok(()));
        limiter.observe_quota(0, Duration::from_secs(60), Duration::ZERO);
        assert_eq!(
            limiter.try_acquire(ms(1)),
            Err(Duration::from_secs(60) - ms(1))
        );
        assert_eq!(limiter.try_acquire(Duration::from_secs(60)), Ok(()));
    }
}
