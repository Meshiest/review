//! Scripted scroll over a real-source diff; prints `PERF` lines and exits.
//!
//! cargo run -p diff_view --release --example scroll_bench -- --rows 100000 [--split] [--frames 1200]
//!     [--autoscroll-frames 600] [--source DIR]... [--plain] [--synthetic]
//!     [--theme FILE] [--font FAMILY] [--font-size PX] [--switch-style-every N]
//!     [--blocks N]
//!
//! The diff is built from `.rs` files (this repository's `crates` and
//! `.scratch/oracle/ripgrep` unless `--source` names others), repeated to
//! `--rows`, so lines highlight like real code. `--synthetic` uses generated
//! text instead and `--plain` turns highlighting off.
//!
//! `--theme` builds the style from a theme file and `--font` / `--font-size`
//! override its buffer font; metric names then end in `@<theme>@<font>`, such
//! as `diff_scroll_split@vercel-dark@Geist Mono`, so `checks.py` can require every
//! combination. `--switch-style-every N` calls `set_style` with the other
//! mode of the theme every N recorded frames (those frames are left out of
//! the scroll samples) and reports `diff_style_switch`: the time from
//! `set_style` to the end of the frame that shows it, p99 against 100 ms.
//!
//! `--blocks N` puts a thread card (real gpui elements: avatars, wrapped
//! comment text, code lines, a rail into the gutter) under every N-th hunk
//! and names the scroll metrics `diff_scroll_*_blocks`.
//!
//! Two measurements, one process. `diff_scroll_*` scrolls at a steady speed
//! from top to bottom, then makes a few jumps, each followed by small steps.
//! `diff_autoscroll` clicks the middle button once through the window's event
//! path (a toggle), holds the pointer a fixed distance from the anchor and
//! lets autoscroll move the view. Frame time is measured by
//! [`diff_view::FrameClock`]; see its module docs.

mod common;

use std::time::{Duration, Instant};

use diff::Layout;
use diff_view::{DiffStyle, DiffView, FrameClock};
use gpui::{
    App, AppContext, Context, Entity, IntoElement, Modifiers, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ParentElement, PlatformInput, Render, Styled, Window, div, point,
    px,
};

const WARMUP_FRAMES: usize = 60;
/// Longest wait for the background highlighter before measuring anyway.
const HIGHLIGHT_WAIT: Duration = Duration::from_secs(300);
const BUDGET_MS: f64 = 8.0;
/// The budget for `set_style` to a repainted frame.
const SWITCH_BUDGET_MS: f64 = 100.0;
/// Where each jump lands, as a fraction of the scroll range.
const JUMPS: [f64; 8] = [0.5, 0.1, 0.9, 0.33, 0.75, 0.0, 1.0, 0.6];
/// Frames after each jump that scroll by [`SMALL_STEP`].
const AFTER_JUMP: usize = 9;
const SMALL_STEP: f64 = 4.0;
/// How far below the anchor the pointer rests during the autoscroll run, in
/// pixels: about 4,700 px/s, several rows per frame.
const AUTOSCROLL_DISTANCE: f32 = 200.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Scroll,
    Autoscroll,
}

struct Bench {
    view: Entity<DiffView>,
    clock: FrameClock,
    layout: Layout,
    content: &'static str,
    rows: usize,
    frames: usize,
    autoscroll_frames: usize,
    stage: Stage,
    /// Frames drawn so far in this stage, warm-up included.
    frame: usize,
    shaped: usize,
    rows_painted: usize,
    blocks_painted: usize,
    /// Blocks whose measured height corrected the height index.
    blocks_resized: usize,
    blocks_every: Option<usize>,
    /// Most lines shaped by one frame that scrolled [`SMALL_STEP`].
    small_step_max_shaped: usize,
    /// Middle clicks scripted after the first because autoscroll had stopped.
    restarts: usize,
    /// The offset when the autoscroll run started recording.
    autoscroll_from: f64,
    /// When highlighting every blob started; `None` once it is done, so the
    /// measured frames paint real highlight runs.
    highlighting_since: Option<Instant>,
    /// Appended to metric names.
    suffix: String,
    font: String,
    font_size: f32,
    switch: Option<Switch>,
}

/// Alternates between two styles while scrolling.
struct Switch {
    every: usize,
    styles: [DiffStyle; 2],
    /// The style the next switch applies.
    next: usize,
    /// When the last `set_style` was called, until the frame that shows it
    /// has ended.
    started: Option<Instant>,
    samples: perf::Samples,
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

    /// Adds the previous frame's work, now that it is painted.
    fn count_previous_frame(&mut self, cx: &App) -> usize {
        let stats = self.view.read(cx).last_frame();
        self.shaped += stats.shaped;
        self.rows_painted += stats.rows;
        self.blocks_painted += stats.blocks;
        self.blocks_resized += stats.resized;
        stats.shaped
    }

    fn report(&self, metric: &str, cx: &App) -> perf::Report {
        let metric = format!("{metric}{}", self.suffix);
        let report = self.clock.samples().report(&metric, 99.0, BUDGET_MS);
        let recorded = report.count.max(1) as f64;
        report
            .with("content", self.content)
            .with("font", self.font.as_str())
            .with("font_size", self.font_size)
            .with("dropped_frames", self.clock.dropped_frames())
            .with("rows", self.rows)
            .with(
                "refresh_ms",
                self.clock.refresh_interval().as_secs_f64() * 1000.0,
            )
            .with("shaped_per_frame", self.shaped as f64 / recorded)
            .with("rows_per_frame", self.rows_painted as f64 / recorded)
            .with("files", self.view.read(cx).list().diff().files.len())
            .with("blocks", self.view.read(cx).list().diff().blocks.len())
            .with("block_every_hunks", self.blocks_every.unwrap_or(0))
            .with("blocks_per_frame", self.blocks_painted as f64 / recorded)
            .with("blocks_resized", self.blocks_resized)
    }

    fn finish_scroll(&mut self, window: &mut Window, cx: &mut App) {
        let metric = match self.layout {
            Layout::Unified => "diff_scroll_unified",
            Layout::Split => "diff_scroll_split",
        };
        let metric = if self.blocks_every.is_some() {
            format!("{metric}_blocks")
        } else {
            metric.to_owned()
        };
        self.report(&metric, cx)
            .with("small_step_max_shaped", self.small_step_max_shaped)
            .print();
        if let Some(switch) = self.switch.take() {
            let metric = format!("diff_style_switch{}", self.suffix);
            switch
                .samples
                .report(&metric, 99.0, SWITCH_BUDGET_MS)
                .with("content", self.content)
                .with("font", self.font.as_str())
                .with("font_size", self.font_size)
                .with("rows", self.rows)
                .with("every_frames", switch.every)
                .print();
        }
        if self.autoscroll_frames == 0 {
            cx.quit();
            return;
        }
        self.stage = Stage::Autoscroll;
        self.clock = FrameClock::new();
        self.frame = 0;
        self.shaped = 0;
        self.rows_painted = 0;
        self.blocks_painted = 0;
        self.blocks_resized = 0;
        self.view.update(cx, |view, _| view.set_scroll_y(0.0));
        window.request_animation_frame();
    }

    fn finish_autoscroll(&self, cx: &mut App) {
        let scrolled = self.view.read(cx).scroll_y() - self.autoscroll_from;
        self.report("diff_autoscroll", cx)
            .with("scrolled_px", scrolled)
            .with("restarts", self.restarts)
            .print();
        cx.quit();
    }

    /// Closes the timing of the last style switch: the frame that showed it
    /// has ended by the time the next one starts.
    fn finish_switch(&mut self) {
        let Some(switch) = &mut self.switch else {
            return;
        };
        if let Some(started) = switch.started.take()
            && let Some(end) = self.clock.last_frame_end()
        {
            switch.samples.push(end.duration_since(started));
            self.clock.set_recording(true);
        }
    }

    /// Applies the other style on every `every`-th recorded frame. That frame
    /// is not recorded as a scroll frame: it re-shapes the whole screen and
    /// is measured as a switch instead.
    fn maybe_switch(&mut self, n: usize, cx: &mut App) {
        let Some(switch) = &mut self.switch else {
            return;
        };
        if n == 0 || !n.is_multiple_of(switch.every) {
            return;
        }
        let style = switch.styles[switch.next].clone();
        switch.next = 1 - switch.next;
        self.clock.set_recording(false);
        switch.started = Some(Instant::now());
        self.view.update(cx, |view, cx| view.set_style(style, cx));
    }

    fn scroll_frame(&mut self, window: &mut Window, cx: &mut App) {
        self.finish_switch();
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
            self.finish_scroll(window, cx);
            return;
        }
        let n = self.frame.saturating_sub(WARMUP_FRAMES);
        let warm = self.frame < WARMUP_FRAMES;
        if !warm {
            self.maybe_switch(n, cx);
        }
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

    fn autoscroll_frame(&mut self, window: &mut Window, cx: &mut App) {
        if self.frame > WARMUP_FRAMES {
            self.count_previous_frame(cx);
        }
        if self.frame == WARMUP_FRAMES {
            self.clock.set_recording(true);
            self.autoscroll_from = self.view.read(cx).scroll_y();
        }
        if self.frame >= WARMUP_FRAMES + self.autoscroll_frames {
            self.clock.set_recording(false);
            self.finish_autoscroll(cx);
            return;
        }
        // Toggled on shortly before recording starts, and again if anything
        // (focus loss, say) stops it.
        if self.frame + 2 >= WARMUP_FRAMES && !self.view.read(cx).is_autoscrolling() {
            if self.frame >= WARMUP_FRAMES {
                self.restarts += 1;
            }
            let size = window.viewport_size();
            let anchor = point(size.width / 2.0, size.height / 4.0);
            window.on_next_frame(move |window, cx| middle_click(anchor, window, cx));
        }
        self.frame += 1;
        window.request_animation_frame();
    }
}

/// A quick middle click at `anchor`, then the pointer moved
/// [`AUTOSCROLL_DISTANCE`] below it, through the window's own event path.
fn middle_click(anchor: gpui::Point<gpui::Pixels>, window: &mut Window, cx: &mut App) {
    let modifiers = Modifiers::default();
    let moved = |position| {
        PlatformInput::MouseMove(MouseMoveEvent {
            position,
            pressed_button: None,
            modifiers,
        })
    };
    window.dispatch_event(moved(anchor), cx);
    window.dispatch_event(
        PlatformInput::MouseDown(MouseDownEvent {
            button: MouseButton::Middle,
            position: anchor,
            modifiers,
            click_count: 1,
            first_mouse: false,
        }),
        cx,
    );
    window.dispatch_event(
        PlatformInput::MouseUp(MouseUpEvent {
            button: MouseButton::Middle,
            position: anchor,
            modifiers,
            click_count: 1,
        }),
        cx,
    );
    window.dispatch_event(moved(anchor + point(px(0.0), px(AUTOSCROLL_DISTANCE))), cx);
}

impl Render for Bench {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(since) = self.highlighting_since {
            let settled = self.view.read(cx).highlights_settled();
            if settled || since.elapsed() > HIGHLIGHT_WAIT {
                eprintln!(
                    "highlighted {} files in {:.1}s{}",
                    self.view.read(cx).list().diff().files.len(),
                    since.elapsed().as_secs_f64(),
                    if settled { "" } else { " (timed out)" }
                );
                self.highlighting_since = None;
            } else {
                window.request_animation_frame();
                return div().size_full().child(self.view.clone());
            }
        }
        self.clock.begin(window, cx);
        match self.stage {
            Stage::Scroll => self.scroll_frame(window, cx),
            Stage::Autoscroll => self.autoscroll_frame(window, cx),
        }
        div()
            .size_full()
            .child(self.view.clone())
            .child(self.clock.end_marker())
    }
}

fn main() {
    let args = common::Args::parse();
    gpui_platform::application().run(move |cx: &mut App| {
        let styles = common::styles(&args, cx);
        let view = common::view(&args, styles.first.clone());
        let highlighting_since = Instant::now();
        view.prefetch_highlights();
        let rows = view.list().len();
        let options = common::window_options(cx, "diff scroll bench");
        let split_suffix = args
            .split_ratio
            .map_or_else(String::new, |r| format!("@split{:.0}", r * 100.0));
        cx.open_window(options, |_, cx| {
            let view = cx.new(|cx| {
                let mut view = view;
                if let Some(ratio) = args.split_ratio {
                    view.set_split_ratio(ratio, cx);
                }
                view
            });
            cx.new(|_| Bench {
                view,
                clock: FrameClock::new(),
                layout: args.layout,
                content: args.content(),
                rows,
                frames: args.frames,
                autoscroll_frames: args.autoscroll_frames,
                stage: Stage::Scroll,
                frame: 0,
                shaped: 0,
                rows_painted: 0,
                blocks_painted: 0,
                blocks_resized: 0,
                blocks_every: args.blocks,
                small_step_max_shaped: 0,
                restarts: 0,
                autoscroll_from: 0.0,
                highlighting_since: Some(highlighting_since),
                suffix: styles.suffix + &split_suffix,
                font: styles.font_family,
                font_size: styles.font_size,
                switch: args
                    .switch_style_every
                    .zip(styles.alternate)
                    .map(|(every, alternate)| Switch {
                        every,
                        styles: [styles.first, alternate],
                        next: 1,
                        started: None,
                        samples: perf::Samples::default(),
                    }),
            })
        })
        .expect("open window");
        cx.activate(true);
    });
}
