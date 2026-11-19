//! The real shell with a 100,000-row diff in the dock; prints `PERF` lines and exits.
//!
//! cargo run -p app --release --example dock_bench -- [--rows 100000] [--frames 1200] [--resize-frames 300] [--files-in-main] [--unthrottled]
//!
//! The dock is Diff on the left and Log on the right; with `--files-in-main`
//! the sidebar's Files section is docked in the main area too, left of the
//! Diff, as a user who dragged it there has it. Phase 1 (`dock_scroll`)
//! scrolls the diff with the script of `diff_view`'s `scroll_bench`: a steady
//! pass through the whole list, then jumps each followed by small steps.
//! Phase 2 (`dock_resize`) parks the diff mid-list and moves the split between
//! Diff and Log back and forth every frame. Frame time is measured by
//! `diff_view::FrameClock`, started in the shell's render and ended in a
//! deferred element, so it covers the sidebar, the dock chrome and the diff.
//! The window is never throttled; `--unthrottled` is accepted for symmetry
//! with the other benchmarks.

use std::f32::consts::TAU;

use app::panels::{DiffPanel, LogPanel};
use app::shell::{Probe, Shell};
use app::sidebar_panels::{Section, SectionPanel};
use app::tab_menu::tab_handle;
use app::{OpenTarget, Setup, assets, new_dock, window_options};
use diff_view::{DiffView, FrameClock};
use gpui::{App, AppContext as _, Context, Entity, Pixels, Window, px};
use gpui_kit::component::dock::{DockArea, DockLayout, DockPlacement, panel_handle};

const WARMUP_FRAMES: usize = 60;
const BUDGET_MS: f64 = 8.0;
/// Where each jump lands, as a fraction of the scroll range.
const JUMPS: [f64; 8] = [0.5, 0.1, 0.9, 0.33, 0.75, 0.0, 1.0, 0.6];
/// Frames after each jump that scroll by [`SMALL_STEP`].
const AFTER_JUMP: usize = 9;
const SMALL_STEP: f64 = 4.0;
/// The split's divider sweeps this fraction of the dock width...
const SWEEP: f32 = 0.4;
/// ...around this fraction, so it runs from 40% to 80% of the width.
const SWEEP_CENTER: f32 = 0.6;
/// Frames per back-and-forth of the divider.
const SWEEP_PERIOD: f32 = 150.0;
/// Width of the Files section's group with `--files-in-main`.
const FILES_WIDTH: f32 = 240.0;

struct Args {
    rows: usize,
    frames: usize,
    resize_frames: usize,
    files_in_main: bool,
}

impl Args {
    /// `--rows N`, `--frames N`, `--resize-frames N`, `--files-in-main`,
    /// `--unthrottled`.
    fn parse() -> Self {
        let mut args = Self {
            rows: 100_000,
            frames: 1200,
            resize_frames: 300,
            files_in_main: false,
        };
        let mut it = std::env::args().skip(1);
        while let Some(arg) = it.next() {
            let mut number = |name: &str| -> usize {
                it.next()
                    .and_then(|v| v.replace('_', "").parse().ok())
                    .unwrap_or_else(|| panic!("{name} takes a number"))
            };
            match arg.as_str() {
                "--rows" => args.rows = number("--rows"),
                "--frames" => args.frames = number("--frames"),
                "--resize-frames" => args.resize_frames = number("--resize-frames"),
                "--files-in-main" => args.files_in_main = true,
                "--unthrottled" => {}
                other => panic!(
                    "unknown argument {other}; expected --rows N, --frames N, --resize-frames N, \
                     --files-in-main, --unthrottled"
                ),
            }
        }
        args
    }
}

/// The scroll offset of each recorded frame; the same script as `scroll_bench`.
struct ScrollScript {
    frames: usize,
}

impl ScrollScript {
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
}

/// What the wheel handler does: the dock's panels are cached views, so a scroll
/// that does not notify the diff view would never reach the screen.
fn set_scroll(view: &mut DiffView, y: f64, cx: &mut Context<DiffView>) {
    if view.set_scroll_y(y) {
        cx.notify();
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Frames before `dock_scroll` that let the window, fonts and caches settle.
    Warmup,
    Scroll,
    /// The same for `dock_resize`, with the diff already parked mid-list.
    ResizeWarmup,
    Resize,
    Done,
}

struct DockBench {
    dock: Entity<DockArea>,
    diff: Entity<DiffPanel>,
    scroll_clock: FrameClock,
    resize_clock: FrameClock,
    script: ScrollScript,
    resize_frames: usize,
    phase: Phase,
    /// Frames drawn in the current phase.
    frame: usize,
    rows: usize,
    shaped: usize,
    rows_painted: usize,
    small_step_max_shaped: usize,
    /// Smallest and largest left-hand width the split was driven to.
    split_range: (f32, f32),
    /// The Files section is the split's first group, left of the Diff.
    files_in_main: bool,
}

impl DockBench {
    fn next(&mut self, phase: Phase) {
        self.phase = phase;
        self.frame = 0;
    }

    fn scroll_frame(&mut self, cx: &mut Context<Shell>) {
        let Some(view) = self.diff.read(cx).view().cloned() else {
            return;
        };
        if self.frame == 0 {
            self.scroll_clock.set_recording(true);
        } else {
            // The previous frame's work, now that it is painted.
            let stats = view.read(cx).last_frame();
            self.shaped += stats.shaped;
            self.rows_painted += stats.rows;
            if self.script.is_small_step(self.frame - 1) {
                self.small_step_max_shaped = self.small_step_max_shaped.max(stats.shaped);
            }
        }
        if self.frame >= self.script.frames {
            self.scroll_clock.set_recording(false);
            self.report_scroll();
            self.next(Phase::ResizeWarmup);
            return;
        }
        view.update(cx, |view, cx| {
            let y = self
                .script
                .target(self.frame, view.scroll_y(), view.max_scroll_y());
            set_scroll(view, y, cx);
        });
        self.frame += 1;
    }

    fn report_scroll(&self) {
        let recorded = self.scroll_clock.samples().len().max(1) as f64;
        self.scroll_clock
            .samples()
            .report("dock_scroll", 99.0, BUDGET_MS)
            .with("dropped_frames", self.scroll_clock.dropped_frames())
            .with("rows", self.rows)
            .with(
                "refresh_ms",
                self.scroll_clock.refresh_interval().as_secs_f64() * 1000.0,
            )
            .with("shaped_per_frame", self.shaped as f64 / recorded)
            .with("rows_per_frame", self.rows_painted as f64 / recorded)
            .with("small_step_max_shaped", self.small_step_max_shaped)
            .with("files_in_main", self.files_in_main)
            .print();
    }

    fn report_resize(&self) {
        self.resize_clock
            .samples()
            .report("dock_resize", 99.0, BUDGET_MS)
            .with("dropped_frames", self.resize_clock.dropped_frames())
            .with("rows", self.rows)
            .with(
                "refresh_ms",
                self.resize_clock.refresh_interval().as_secs_f64() * 1000.0,
            )
            .with("split_min_px", self.split_range.0)
            .with("split_max_px", self.split_range.1)
            .with("files_in_main", self.files_in_main)
            .print();
    }

    /// Moves the divider to `at` of the main area's width with
    /// `DockArea::set_split_sizes`.
    fn set_split(&mut self, at: f32, window: &mut Window, cx: &mut Context<Shell>) {
        let (node, width) = {
            let dock = self.dock.read(cx);
            let node = dock
                .layout(DockPlacement::Center)
                .map(|tree| tree.root().id());
            // The main area: what the sidebar, the dock's left region,
            // leaves of the dock.
            let sidebar = dock
                .dock_size(DockPlacement::Left)
                .filter(|_| dock.is_dock_open(DockPlacement::Left))
                .unwrap_or_default();
            (node, dock.bounds().size.width - sidebar)
        };
        let Some(node) = node else { return };
        if width <= px(0.0) {
            return;
        }
        let left = (width * at).round();
        let sizes: Vec<Pixels> = if self.files_in_main {
            let files = px(FILES_WIDTH);
            vec![files, left - files, width - left]
        } else {
            vec![left, width - left]
        };
        let left_px = left.as_f32();
        self.split_range = (
            self.split_range.0.min(left_px),
            self.split_range.1.max(left_px),
        );
        self.dock
            .update(cx, |dock, cx| dock.set_split_sizes(node, sizes, window, cx));
    }

    fn park_mid_list(&self, cx: &mut Context<Shell>) {
        let Some(view) = self.diff.read(cx).view().cloned() else {
            return;
        };
        view.update(cx, |view, cx| {
            let y = view.max_scroll_y() * 0.5;
            set_scroll(view, y, cx);
        });
    }

    fn resize_frame(&mut self, window: &mut Window, cx: &mut Context<Shell>) {
        match self.phase {
            Phase::ResizeWarmup => {
                self.park_mid_list(cx);
                self.set_split(SWEEP_CENTER, window, cx);
                self.frame += 1;
                if self.frame >= WARMUP_FRAMES {
                    self.next(Phase::Resize);
                }
            }
            _ => {
                if self.frame == 0 {
                    self.resize_clock.set_recording(true);
                }
                if self.frame >= self.resize_frames {
                    self.resize_clock.set_recording(false);
                    self.report_resize();
                    self.next(Phase::Done);
                    return;
                }
                let wave = (self.frame as f32 / SWEEP_PERIOD * TAU).sin();
                self.set_split(SWEEP_CENTER + SWEEP / 2.0 * wave, window, cx);
                self.frame += 1;
            }
        }
    }
}

impl Probe for DockBench {
    fn clock(&self) -> &FrameClock {
        match self.phase {
            Phase::Warmup | Phase::Scroll => &self.scroll_clock,
            Phase::ResizeWarmup | Phase::Resize | Phase::Done => &self.resize_clock,
        }
    }

    fn frame(&mut self, window: &mut Window, cx: &mut Context<Shell>) {
        match self.phase {
            Phase::Warmup => {
                if let Some(view) = self.diff.read(cx).view().cloned() {
                    view.update(cx, |view, cx| set_scroll(view, 0.0, cx));
                    self.rows = view.read(cx).list().len();
                }
                self.frame += 1;
                if self.frame >= WARMUP_FRAMES {
                    self.next(Phase::Scroll);
                }
            }
            Phase::Scroll => self.scroll_frame(cx),
            Phase::ResizeWarmup | Phase::Resize => self.resize_frame(window, cx),
            Phase::Done => {
                cx.quit();
                return;
            }
        }
        window.request_animation_frame();
    }
}

fn main() {
    let args = Args::parse();
    gpui_kit::application()
        .with_assets(assets::AppAssets)
        .run(move |cx: &mut App| {
            app::init(
                cx,
                &Setup {
                    open_pr: Some(OpenTarget::Number(41)),
                    diff_rows: Some(args.rows),
                    fixtures: true,
                    ..Setup::default()
                },
            );
            gpui_kit::open_window(window_options(cx, false), cx, move |window, cx| {
                let dock = new_dock(window, cx);
                let diff = DiffPanel::new(window, cx);
                let log = LogPanel::new(window, cx);
                let mut layout = DockLayout::h_split();
                if args.files_in_main {
                    let files = tab_handle(SectionPanel::new(Section::Files, window, cx));
                    layout = layout.child(
                        DockLayout::tabs().panel_view(files, cx),
                        Some(px(FILES_WIDTH)),
                    );
                }
                let layout = layout
                    .child(
                        DockLayout::tabs().panel_view(panel_handle(diff.clone()), cx),
                        Some(px(820.0)),
                    )
                    .child(
                        DockLayout::tabs().panel_view(panel_handle(log), cx),
                        Some(px(320.0)),
                    );
                dock.update(cx, |dock, cx| dock.set_center(layout, window, cx));
                let bench = DockBench {
                    dock: dock.clone(),
                    diff,
                    scroll_clock: FrameClock::new(),
                    resize_clock: FrameClock::new(),
                    script: ScrollScript {
                        frames: args.frames,
                    },
                    resize_frames: args.resize_frames,
                    phase: Phase::Warmup,
                    frame: 0,
                    rows: 0,
                    shaped: 0,
                    rows_painted: 0,
                    small_step_max_shaped: 0,
                    split_range: (f32::MAX, 0.0),
                    files_in_main: args.files_in_main,
                };
                cx.new(|cx| Shell::new(dock, false, window, cx).with_probe(Box::new(bench)))
            })
            .expect("open the main window");
            cx.activate(true);
        });
}
