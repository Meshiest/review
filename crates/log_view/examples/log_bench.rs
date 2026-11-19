//! Scripted scroll and tail over a big generated log; prints `PERF` lines
//! and exits.
//!
//! cargo run -p log_view --release --example log_bench -- --lines 200000 [--frames 1200]
//!     [--tail-frames 600] [--tail-lines 50] [--collapse-every 3] [--time]
//!
//! The log is generated (groups, ANSI colors, long lines, timestamps), fed in
//! 16 KiB chunks that split lines and escapes, and parsed on the worker
//! thread; the bench waits for that to settle. Then two measurements, one
//! process. `log_scroll` scrolls at a steady speed from the top to the
//! bottom, then makes a few jumps, each followed by small steps. `log_tail`
//! appends `--tail-lines` lines every frame while the view follows the end.
//! `--collapse-every N` collapses every N-th group first. Frame time is
//! measured by [`diff_view::FrameClock`]; see its module docs.

mod common;

use std::time::{Duration, Instant};

use diff_view::FrameClock;
use gpui::{
    App, AppContext, Context, Entity, IntoElement, ParentElement, Render, Styled, Window, div,
};
use log_view::{LogFeed, LogView};

const CHUNK_BYTES: usize = 16 * 1024;
const WARMUP_FRAMES: usize = 60;
const SETTLE_WAIT: Duration = Duration::from_secs(300);
const BUDGET_MS: f64 = 8.0;
/// Where each jump lands, as a fraction of the scroll range.
const JUMPS: [f64; 8] = [0.5, 0.1, 0.9, 0.33, 0.75, 0.0, 1.0, 0.6];
/// Frames after each jump that scroll by [`SMALL_STEP`].
const AFTER_JUMP: usize = 9;
const SMALL_STEP: f64 = 4.0;

struct Args {
    lines: usize,
    frames: usize,
    tail_frames: usize,
    tail_lines: usize,
    collapse_every: usize,
    time: bool,
}

impl Args {
    fn parse() -> Self {
        let mut args = Self {
            lines: 200_000,
            frames: 1200,
            tail_frames: 600,
            tail_lines: 50,
            collapse_every: 3,
            time: false,
        };
        let mut it = std::env::args().skip(1);
        while let Some(arg) = it.next() {
            let mut number = |name: &str| -> usize {
                it.next()
                    .and_then(|v| v.replace('_', "").parse().ok())
                    .unwrap_or_else(|| panic!("{name} takes a number"))
            };
            match arg.as_str() {
                "--lines" => args.lines = number("--lines"),
                "--frames" => args.frames = number("--frames"),
                "--tail-frames" => args.tail_frames = number("--tail-frames"),
                "--tail-lines" => args.tail_lines = number("--tail-lines"),
                "--collapse-every" => args.collapse_every = number("--collapse-every"),
                "--time" => args.time = true,
                other => panic!(
                    "unknown argument {other}; expected --lines N, --frames N, --tail-frames N, \
                     --tail-lines N, --collapse-every N, --time"
                ),
            }
        }
        args
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Load,
    Scroll,
    Tail,
}

struct Bench {
    view: Entity<LogView>,
    feed: LogFeed,
    clock: FrameClock,
    stage: Stage,
    frames: usize,
    tail_frames: usize,
    collapse_every: usize,
    /// Pre-generated chunks, one per tail frame.
    tail: Vec<Vec<u8>>,
    loading_since: Instant,
    /// Frames drawn so far in this stage, warm-up included.
    frame: usize,
    shaped: usize,
    rows_painted: usize,
    /// Most lines shaped by one frame that scrolled [`SMALL_STEP`].
    small_step_max_shaped: usize,
    appended: usize,
}

impl Bench {
    fn steady_frames(&self) -> usize {
        self.frames
            .saturating_sub(JUMPS.len() * (AFTER_JUMP + 1))
            .max(1)
    }

    fn is_small_step(&self, n: usize) -> bool {
        let Some(k) = n.checked_sub(self.steady_frames()) else {
            return false;
        };
        k / (AFTER_JUMP + 1) < JUMPS.len() && k % (AFTER_JUMP + 1) != 0
    }

    /// The scroll offset for recorded frame `n`.
    fn target(&self, n: usize, current: f64, max: f64) -> f64 {
        let steady = self.steady_frames();
        if n < steady {
            return max * n as f64 / (steady - 1).max(1) as f64;
        }
        let k = n - steady;
        let (jump, step) = (k / (AFTER_JUMP + 1), k % (AFTER_JUMP + 1));
        match JUMPS.get(jump) {
            Some(fraction) if step == 0 => max * fraction,
            Some(_) => {
                current
                    + if current + SMALL_STEP > max {
                        -SMALL_STEP
                    } else {
                        SMALL_STEP
                    }
            }
            None => current,
        }
    }

    fn count_previous_frame(&mut self, cx: &App) -> usize {
        let stats = self.view.read(cx).last_frame();
        self.shaped += stats.shaped;
        self.rows_painted += stats.rows;
        stats.shaped
    }

    fn report(&self, metric: &str) -> perf::Report {
        let report = self.clock.samples().report(metric, 99.0, BUDGET_MS);
        let recorded = report.count.max(1) as f64;
        report
            .with("lines", self.feed.read(|b| b.len()))
            .with("dropped_frames", self.clock.dropped_frames())
            .with(
                "refresh_ms",
                self.clock.refresh_interval().as_secs_f64() * 1000.0,
            )
            .with("shaped_per_frame", self.shaped as f64 / recorded)
            .with("rows_per_frame", self.rows_painted as f64 / recorded)
    }

    fn load_frame(&mut self, window: &mut Window, cx: &mut App) {
        let settled = self.feed.is_settled();
        if !settled && self.loading_since.elapsed() < SETTLE_WAIT {
            window.request_animation_frame();
            return;
        }
        let (lines, groups, columns) = self
            .feed
            .read(|b| (b.len(), b.groups().len(), b.max_columns()));
        eprintln!(
            "parsed {lines} lines, {groups} groups, widest {columns} columns in {:.2}s{}",
            self.loading_since.elapsed().as_secs_f64(),
            if settled { "" } else { " (timed out)" }
        );
        if self.collapse_every > 0 {
            let headers: Vec<u64> = self
                .feed
                .read(|b| b.groups().iter().map(|g| g.start).collect());
            self.view.update(cx, |view, cx| {
                for header in headers.into_iter().step_by(self.collapse_every) {
                    view.set_group_collapsed(header, true, cx);
                }
            });
        }
        self.stage = Stage::Scroll;
        self.frame = 0;
        window.request_animation_frame();
    }

    fn scroll_frame(&mut self, window: &mut Window, cx: &mut App) {
        if self.frame > WARMUP_FRAMES {
            let shaped = self.count_previous_frame(cx);
            if self.is_small_step(self.frame - 1 - WARMUP_FRAMES) {
                self.small_step_max_shaped = self.small_step_max_shaped.max(shaped);
            }
        }
        if self.frame == WARMUP_FRAMES {
            self.clock.set_recording(true);
        }
        if self.frame >= WARMUP_FRAMES + self.frames {
            self.clock.set_recording(false);
            self.report("log_scroll")
                .with("small_step_max_shaped", self.small_step_max_shaped)
                .print();
            self.start_tail(window, cx);
            return;
        }
        let n = self.frame.saturating_sub(WARMUP_FRAMES);
        let warm = self.frame < WARMUP_FRAMES;
        self.view.update(cx, |view, _| {
            let y = if warm {
                0.0
            } else {
                self.target(n, view.scroll_y(), view.max_scroll_y())
            };
            view.set_scroll_y(y);
        });
        self.frame += 1;
        window.request_animation_frame();
    }

    fn start_tail(&mut self, window: &mut Window, cx: &mut App) {
        self.stage = Stage::Tail;
        self.clock = FrameClock::new();
        self.frame = 0;
        self.shaped = 0;
        self.rows_painted = 0;
        self.view.update(cx, |view, cx| view.scroll_to_latest(cx));
        window.request_animation_frame();
    }

    fn tail_frame(&mut self, window: &mut Window, cx: &mut App) {
        if self.frame > WARMUP_FRAMES {
            self.count_previous_frame(cx);
        }
        if self.frame == WARMUP_FRAMES {
            self.clock.set_recording(true);
        }
        if self.frame >= WARMUP_FRAMES + self.tail_frames {
            self.clock.set_recording(false);
            self.feed.wait_settled(Duration::from_secs(30));
            let view = self.view.read(cx);
            let followed = view.is_following() && view.scroll_y() >= view.max_scroll_y() - 1.0;
            self.report("log_tail")
                .with("appended", self.appended)
                .with("followed", followed)
                .print();
            cx.quit();
            return;
        }
        if let Some(chunk) = self.tail.get(self.frame) {
            self.feed.push(chunk.clone());
            self.appended += chunk.iter().filter(|&&b| b == b'\n').count();
        }
        self.frame += 1;
        window.request_animation_frame();
    }
}

impl Render for Bench {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.stage == Stage::Load {
            self.load_frame(window, cx);
            return div().size_full().child(self.view.clone());
        }
        self.clock.begin(window, cx);
        match self.stage {
            Stage::Load => {}
            Stage::Scroll => self.scroll_frame(window, cx),
            Stage::Tail => self.tail_frame(window, cx),
        }
        div()
            .size_full()
            .child(self.view.clone())
            .child(self.clock.end_marker())
    }
}

fn main() {
    let args = Args::parse();
    gpui_platform::application().run(move |cx: &mut App| {
        let style = common::style(cx);
        let mut generator = common::Generator::new(0x5eed_1096);
        let log = generator.lines(args.lines);
        let feed = LogFeed::new();
        let loading_since = Instant::now();
        for chunk in common::chunks(&log, CHUNK_BYTES) {
            feed.push(chunk);
        }
        let tail = (0..args.tail_frames)
            .map(|_| generator.lines(args.tail_lines))
            .collect();
        let options = common::window_options(cx, "log bench");
        cx.open_window(options, |_, cx| {
            let view = cx.new(|cx| {
                let mut view = LogView::new(&feed, style, cx);
                view.set_show_time(args.time, cx);
                view
            });
            cx.new(|_| Bench {
                view,
                feed,
                clock: FrameClock::new(),
                stage: Stage::Load,
                frames: args.frames,
                tail_frames: args.tail_frames,
                collapse_every: args.collapse_every,
                tail,
                loading_since,
                frame: 0,
                shaped: 0,
                rows_painted: 0,
                small_step_max_shaped: 0,
                appended: 0,
            })
        })
        .expect("open window");
        cx.activate(true);
    });
}
