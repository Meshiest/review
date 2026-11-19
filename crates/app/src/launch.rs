//! `review --report-launch`: how long the app took to paint the inbox from
//! the cache, for `examples/launch_bench.rs`.
//!
//! The probe watches the shell's frames. The first frame that draws rows of
//! a section still showing cached data is the one measured: on the frame
//! after it, the probe prints one [`LAUNCH_PREFIX`] line on stdout and
//! quits. A cold cache never shows such a frame; the probe then waits for
//! every section to answer, gives the cache writes a moment, says `cold` and
//! quits, so the run fills the cache for the next one.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use diff_view::FrameClock;
use gpui::{Context, Entity, Window};
use serde::Serialize;

use crate::model::{Inbox, SectionState};
use crate::session::Session;
use crate::shell::{Probe, Shell};

/// Starts the line the app prints; the rest is a [`LaunchReport`] as JSON.
pub const LAUNCH_PREFIX: &str = "LAUNCH ";
/// How long a cold run stays up after the forges answered, so the cache
/// writes they started land before the process ends.
const COLD_GRACE: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Cache {
    /// Rows were painted from the cache.
    Warm,
    /// The cache had nothing; the rows came from the forges.
    Cold,
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct LaunchReport {
    pub cache: Cache,
    /// Pull request rows in the painted sections.
    pub rows: usize,
    /// From the start of `main` to the end of the frame's paint.
    pub main_to_paint_ms: f64,
    /// Wall-clock end of the frame's paint, in microseconds since the Unix
    /// epoch, so a parent can time from its own spawn call.
    pub painted_unix_us: u128,
}

impl LaunchReport {
    /// Parses a line the app printed, if it is the report.
    pub fn parse(line: &str) -> Option<Self> {
        serde_json::from_str(line.strip_prefix(LAUNCH_PREFIX)?).ok()
    }

    fn print(&self) {
        println!(
            "{LAUNCH_PREFIX}{}",
            serde_json::to_string(self).expect("report serializes")
        );
    }
}

pub struct LaunchProbe {
    clock: FrameClock,
    session: Entity<Session>,
    started: Instant,
    /// Set on the frame that painted cached rows: how many.
    painted: Option<usize>,
    cold: bool,
}

impl LaunchProbe {
    pub fn new(session: Entity<Session>, started: Instant) -> Self {
        Self {
            clock: FrameClock::new(),
            session,
            started,
            painted: None,
            cold: false,
        }
    }

    fn report(&self, cache: Cache, rows: usize, end: Instant) {
        let now = (Instant::now(), SystemTime::now());
        let painted = now.1 - now.0.duration_since(end);
        LaunchReport {
            cache,
            rows,
            main_to_paint_ms: end.duration_since(self.started).as_secs_f64() * 1000.0,
            painted_unix_us: painted
                .duration_since(UNIX_EPOCH)
                .map_or(0, |since| since.as_micros()),
        }
        .print();
    }
}

/// Rows of expanded sections that still show what the cache had.
fn cached_rows(inbox: &Inbox) -> usize {
    inbox
        .sections
        .iter()
        .filter(|s| s.expanded_by_default && s.state == SectionState::Cached)
        .map(|s| s.items.len())
        .sum()
}

/// Every section has its forge's answer, or its failure.
fn settled(inbox: &Inbox) -> bool {
    !inbox.sections.is_empty()
        && inbox
            .sections
            .iter()
            .all(|s| matches!(s.state, SectionState::Fresh | SectionState::Failed(_)))
}

impl Probe for LaunchProbe {
    fn clock(&self) -> &FrameClock {
        &self.clock
    }

    fn frame(&mut self, window: &mut Window, cx: &mut Context<Shell>) {
        if let Some(rows) = self.painted {
            if let Some(end) = self.clock.last_frame_end() {
                self.report(Cache::Warm, rows, end);
                cx.quit();
            }
            return;
        }
        if self.cold {
            return;
        }
        let inbox = self.session.read(cx).inbox.clone();
        let rows = cached_rows(&inbox);
        if rows > 0 {
            // This frame paints them; its end is known on the next one.
            self.painted = Some(rows);
            window.request_animation_frame();
        } else if settled(&inbox) {
            self.cold = true;
            let rows = inbox.sections.iter().map(|s| s.items.len()).sum();
            self.report(Cache::Cold, rows, Instant::now());
            cx.spawn(async move |_, cx| {
                cx.background_executor().timer(COLD_GRACE).await;
                cx.update(|cx| cx.quit());
            })
            .detach();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_report_line_round_trips() {
        let report = LaunchReport {
            cache: Cache::Warm,
            rows: 12,
            main_to_paint_ms: 180.5,
            painted_unix_us: 1_800_000_000_000_000,
        };
        let line = format!(
            "{LAUNCH_PREFIX}{}",
            serde_json::to_string(&report).expect("serializes")
        );
        let parsed = LaunchReport::parse(&line).expect("parses");
        assert_eq!(parsed.cache, Cache::Warm);
        assert_eq!(parsed.rows, 12);
        assert_eq!(parsed.painted_unix_us, report.painted_unix_us);
        assert!(LaunchReport::parse("review: inbox from cache").is_none());
    }
}
