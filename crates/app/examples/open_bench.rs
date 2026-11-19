//! Opens a cached 50,000-line pull request in the real shell, then scrolls
//! it in unified and split; prints `PERF` lines and exits.
//!
//! cargo run -p app --release --example open_bench -- --unthrottled [--rows 50000] [--opens 20] [--frames 1200]
//!     [--no-threads]
//!
//! The shell runs on fixtures with the default dock (Diff, Conversation and
//! Checks tabbed, Log beside them). The big pull request is #41, whose
//! synthetic diff has `--rows` rows and a review thread every few hunks, so
//! its cards are real block elements in the list.
//!
//! - `open_cached`: #41 is opened once to build and cache its diff, as a
//!   pull request whose objects and snapshot are local; then the bench
//!   switches to a small pull request and back, `--opens` times. Each sample
//!   runs from the `open` call to the end of the first frame that painted
//!   #41's rows. p95 against 100 ms.
//! - `scroll_50k_unified`, `scroll_50k_split`: the scroll script of
//!   `diff_view`'s `scroll_bench` (a steady pass through the whole list, then
//!   jumps each followed by small steps), with every blob highlighted first.
//!   Frame time is `diff_view::FrameClock`, started in the shell's render and
//!   ended after the last paint, so it covers the sidebar, the dock and the
//!   diff with its thread cards. p99 against 8 ms.
//!
//! `--unthrottled` keeps gpui from slowing frames while another window has
//! focus; `just bench` runs with it. `--no-threads` leaves the review threads
//! out, to compare against the same diff without cards.

use std::time::{Duration, Instant};

use app::model::PrKey;
use app::panels::{ChecksPanel, ConversationPanel, DiffPanel, LogPanel};
use app::session::Session;
use app::shell::{Probe, Shell};
use app::{Setup, assets, new_dock, window_options};
use diff::Layout;
use diff_view::{DiffView, FrameClock};
use gpui::{App, AppContext as _, Context, Entity, EntityId, Window, px};
use gpui_kit::component::dock::{DockLayout, panel_handle};

const WARMUP_FRAMES: usize = 60;
const SCROLL_BUDGET_MS: f64 = 8.0;
const OPEN_BUDGET_MS: f64 = 100.0;
/// Longest wait for one open, or for the highlighter, before giving up.
const WAIT: Duration = Duration::from_secs(120);
const BIG: u64 = 41;
const SMALL: u64 = 42;
/// Where each jump lands, as a fraction of the scroll range.
const JUMPS: [f64; 8] = [0.5, 0.1, 0.9, 0.33, 0.75, 0.0, 1.0, 0.6];
/// Frames after each jump that scroll by [`SMALL_STEP`].
const AFTER_JUMP: usize = 9;
const SMALL_STEP: f64 = 4.0;

struct Args {
    rows: usize,
    opens: usize,
    frames: usize,
    unthrottled: bool,
    threads: bool,
}

impl Args {
    /// `--rows N`, `--opens N`, `--frames N`, `--unthrottled`, `--no-threads`.
    fn parse() -> Self {
        let mut args = Self {
            rows: 50_000,
            opens: 20,
            frames: 1200,
            unthrottled: false,
            threads: true,
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
                "--opens" => args.opens = number("--opens").max(1),
                "--frames" => args.frames = number("--frames"),
                "--unthrottled" => args.unthrottled = true,
                "--no-threads" => args.threads = false,
                other => panic!(
                    "unknown argument {other}; expected --rows N, --opens N, --frames N, \
                     --unthrottled, --no-threads"
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Warmup,
    /// Waiting for an open of `number` to paint; `counted` opens are samples.
    Opening {
        number: u64,
        counted: bool,
    },
    Highlight,
    ScrollWarmup(Layout),
    Scroll(Layout),
    Done,
}

struct OpenBench {
    session: Entity<Session>,
    diff: Entity<DiffPanel>,
    clock: FrameClock,
    script: ScrollScript,
    opens_left: usize,
    phase: Phase,
    frame: usize,
    /// The view the open in flight replaces.
    replacing: Option<Option<EntityId>>,
    open_samples: perf::Samples,
    phase_started: Instant,
    rows: usize,
    blocks: usize,
    rows_painted: usize,
    blocks_painted: usize,
    blocks_resized: usize,
}

impl OpenBench {
    fn key(&self, number: u64, cx: &App) -> PrKey {
        let session = self.session.read(cx);
        session
            .inbox
            .prs
            .iter()
            .find(|pr| pr.number() == number)
            .map(|pr| pr.key.clone())
            .unwrap_or_else(|| panic!("fixture #{number} exists"))
    }

    fn view(&self, cx: &App) -> Option<Entity<DiffView>> {
        self.diff.read(cx).view().cloned()
    }

    /// Opens `number` once this frame is drawn, as a click would: a notify
    /// from inside a draw reaches no observers in gpui-pre, so the panels
    /// would miss it. The sample starts at the session's own `opened_at`.
    fn start_open(&mut self, number: u64, counted: bool, cx: &mut Context<Shell>) {
        let key = self.key(number, cx);
        self.replacing = Some(self.view(cx).map(|view| view.entity_id()));
        let session = self.session.clone();
        // Each open is a fresh one, as when the last tab of the other
        // closed: an open pull request would only take the focus.
        cx.defer(move |cx| {
            session.update(cx, |session, cx| {
                if let Some(previous) = session.open_key().cloned() {
                    session.release(&previous, cx);
                }
                session.open(&key, cx);
            });
        });
        self.phase = Phase::Opening { number, counted };
        self.phase_started = Instant::now();
    }

    /// Whether the open in flight has painted: a new view whose rows the
    /// last frame laid out.
    fn open_painted(&self, cx: &App) -> bool {
        let Some(previous) = self.replacing else {
            return false;
        };
        self.view(cx).is_some_and(|view| {
            Some(view.entity_id()) != previous && view.read(cx).last_frame().rows > 0
        })
    }

    fn opening(&mut self, number: u64, counted: bool, cx: &mut Context<Shell>) {
        if !self.open_painted(cx) {
            assert!(
                self.phase_started.elapsed() < WAIT,
                "#{number} did not paint within {WAIT:?}"
            );
            return;
        }
        self.replacing = None;
        let started = self.session.read(cx).open_pr().map(|open| open.opened_at);
        if counted
            && let Some(started) = started
            && let Some(end) = self.clock.last_frame_end()
        {
            self.open_samples.push(end.duration_since(started));
        }
        match number {
            SMALL => self.start_open(BIG, true, cx),
            _ if self.opens_left > 0 => {
                self.opens_left -= 1;
                self.start_open(SMALL, false, cx);
            }
            _ => {
                self.report_open();
                if let Some(view) = self.view(cx) {
                    view.read(cx).prefetch_highlights();
                }
                self.phase = Phase::Highlight;
                self.phase_started = Instant::now();
            }
        }
    }

    fn report_open(&self) {
        self.open_samples
            .report("open_cached", 95.0, OPEN_BUDGET_MS)
            .with("rows", self.rows)
            .with("content", "fixture_synthetic")
            .print();
    }

    fn scroll_frame(&mut self, layout: Layout, cx: &mut Context<Shell>) {
        let Some(view) = self.view(cx) else {
            return;
        };
        if self.frame == 0 {
            self.clock.set_recording(true);
            self.rows_painted = 0;
            self.blocks_painted = 0;
            self.blocks_resized = 0;
        } else {
            let stats = view.read(cx).last_frame();
            self.rows_painted += stats.rows;
            self.blocks_painted += stats.blocks;
            self.blocks_resized += stats.resized;
        }
        if self.frame >= self.script.frames {
            self.clock.set_recording(false);
            self.report_scroll(layout);
            self.next_layout(layout, cx);
            return;
        }
        let frame = self.frame;
        view.update(cx, |view, cx| {
            let y = self
                .script
                .target(frame, view.scroll_y(), view.max_scroll_y());
            if view.set_scroll_y(y) {
                cx.notify();
            }
        });
        self.frame += 1;
    }

    fn report_scroll(&self, layout: Layout) {
        let metric = match layout {
            Layout::Unified => "scroll_50k_unified",
            Layout::Split => "scroll_50k_split",
        };
        let recorded = self.clock.samples().len().max(1) as f64;
        self.clock
            .samples()
            .report(metric, 99.0, SCROLL_BUDGET_MS)
            .with("rows", self.rows)
            .with("blocks", self.blocks)
            .with("content", "fixture_synthetic")
            .with("dropped_frames", self.clock.dropped_frames())
            .with(
                "refresh_ms",
                self.clock.refresh_interval().as_secs_f64() * 1000.0,
            )
            .with("rows_per_frame", self.rows_painted as f64 / recorded)
            .with("blocks_per_frame", self.blocks_painted as f64 / recorded)
            .with("blocks_resized", self.blocks_resized)
            .print();
    }

    fn next_layout(&mut self, done: Layout, cx: &mut Context<Shell>) {
        self.clock = FrameClock::new();
        self.frame = 0;
        match done {
            Layout::Unified => {
                self.session
                    .update(cx, |session, cx| session.set_diff_layout(Layout::Split, cx));
                self.phase = Phase::ScrollWarmup(Layout::Split);
            }
            Layout::Split => self.phase = Phase::Done,
        }
    }
}

impl Probe for OpenBench {
    fn clock(&self) -> &FrameClock {
        &self.clock
    }

    fn frame(&mut self, window: &mut Window, cx: &mut Context<Shell>) {
        match self.phase {
            Phase::Warmup => {
                self.frame += 1;
                if self.frame >= WARMUP_FRAMES {
                    self.frame = 0;
                    // The first open builds and caches the diff; not a sample.
                    self.start_open(BIG, false, cx);
                }
            }
            Phase::Opening { number, counted } => {
                if let Some(view) = self.view(cx).filter(|_| number == BIG) {
                    let list = view.read(cx).list();
                    self.rows = list.len();
                    self.blocks = list.diff().blocks.len();
                }
                self.opening(number, counted, cx);
            }
            Phase::Highlight => {
                let settled = self
                    .view(cx)
                    .is_none_or(|view| view.read(cx).highlights_settled());
                if settled || self.phase_started.elapsed() > WAIT {
                    eprintln!(
                        "highlighted in {:.1}s{}",
                        self.phase_started.elapsed().as_secs_f64(),
                        if settled { "" } else { " (timed out)" }
                    );
                    self.phase = Phase::ScrollWarmup(Layout::Unified);
                    self.frame = 0;
                }
            }
            Phase::ScrollWarmup(layout) => {
                if let Some(view) = self.view(cx) {
                    view.update(cx, |view, cx| {
                        if view.set_scroll_y(0.0) {
                            cx.notify();
                        }
                    });
                }
                self.frame += 1;
                if self.frame >= WARMUP_FRAMES {
                    self.frame = 0;
                    self.phase = Phase::Scroll(layout);
                }
            }
            Phase::Scroll(layout) => self.scroll_frame(layout, cx),
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
                    diff_rows: Some(args.rows),
                    fixture_threads: args.threads,
                    fixtures: true,
                    ..Setup::default()
                },
            );
            let options = window_options(cx, !args.unthrottled);
            gpui_kit::open_window(options, cx, move |window, cx| {
                let dock = new_dock(window, cx);
                let diff = DiffPanel::new(window, cx);
                let conversation = ConversationPanel::new(window, cx);
                let checks = ChecksPanel::new(window, cx);
                let log = LogPanel::new(window, cx);
                let layout = DockLayout::h_split()
                    .child(
                        DockLayout::tabs()
                            .panel_view(panel_handle(diff.clone()), cx)
                            .panel_view(panel_handle(conversation), cx)
                            .panel_view(panel_handle(checks), cx),
                        Some(px(820.0)),
                    )
                    .child(
                        DockLayout::tabs().panel_view(panel_handle(log), cx),
                        Some(px(340.0)),
                    );
                dock.update(cx, |dock, cx| dock.set_center(layout, window, cx));
                let bench = OpenBench {
                    session: Session::global(cx),
                    diff,
                    clock: FrameClock::new(),
                    script: ScrollScript {
                        frames: args.frames,
                    },
                    opens_left: args.opens,
                    phase: Phase::Warmup,
                    frame: 0,
                    replacing: None,
                    open_samples: perf::Samples::default(),
                    phase_started: Instant::now(),
                    rows: 0,
                    blocks: 0,
                    rows_painted: 0,
                    blocks_painted: 0,
                    blocks_resized: 0,
                };
                cx.new(|cx| Shell::new(dock, false, window, cx).with_probe(Box::new(bench)))
            })
            .expect("open the main window");
            cx.activate(true);
        });
}
