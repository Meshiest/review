//! The cursor blink cycle.

use std::time::{Duration, Instant};

/// How long the cursor stays on, and then off.
const HALF_PERIOD: Duration = Duration::from_millis(530);

/// Where in its cycle the cursor is. Restarting it (typing, output) shows the
/// cursor at once. Every method takes the current time, so a test can step a
/// clock by hand.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Blink {
    epoch: Instant,
}

impl Blink {
    pub(crate) fn new(now: Instant) -> Self {
        Self { epoch: now }
    }

    pub(crate) fn restart(&mut self, now: Instant) {
        self.epoch = now;
    }

    pub(crate) fn visible(&self, now: Instant) -> bool {
        let phase = self.elapsed(now).as_millis() / HALF_PERIOD.as_millis();
        phase.is_multiple_of(2)
    }

    /// Time until the cursor next flips.
    pub(crate) fn until_toggle(&self, now: Instant) -> Duration {
        let into_half = self.elapsed(now).as_millis() % HALF_PERIOD.as_millis();
        HALF_PERIOD - Duration::from_millis(into_half as u64)
    }

    fn elapsed(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.epoch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(millis: u64) -> Duration {
        Duration::from_millis(millis)
    }

    #[test]
    fn the_cursor_is_on_then_off_each_half_period() {
        let start = Instant::now();
        let blink = Blink::new(start);
        assert!(blink.visible(start));
        assert!(blink.visible(start + ms(529)));
        assert!(!blink.visible(start + ms(530)));
        assert!(!blink.visible(start + ms(1059)));
        assert!(blink.visible(start + ms(1060)));
    }

    #[test]
    fn the_next_flip_is_the_rest_of_the_half_period() {
        let start = Instant::now();
        let blink = Blink::new(start);
        assert_eq!(blink.until_toggle(start), HALF_PERIOD);
        assert_eq!(blink.until_toggle(start + ms(500)), ms(30));
        assert_eq!(blink.until_toggle(start + ms(530)), HALF_PERIOD);
    }

    #[test]
    fn restarting_shows_the_cursor_and_begins_a_new_cycle() {
        let start = Instant::now();
        let mut blink = Blink::new(start);
        let off = start + ms(600);
        assert!(!blink.visible(off));
        blink.restart(off);
        assert!(blink.visible(off));
        assert!(blink.visible(off + ms(529)));
        assert!(!blink.visible(off + ms(530)));
        assert_eq!(blink.until_toggle(off + ms(100)), ms(430));
    }

    #[test]
    fn a_clock_that_reads_earlier_than_the_epoch_counts_as_the_start() {
        let start = Instant::now();
        let blink = Blink::new(start + ms(1000));
        assert!(blink.visible(start));
        assert_eq!(blink.until_toggle(start), HALF_PERIOD);
    }
}
