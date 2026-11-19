//! Times a terminal panel in the real shell, on fixtures, and prints two
//! `PERF` lines, then exits.
//!
//! cargo run -p app --release --example term_bench -- --unthrottled [--keys 200] [--lines 1000000] [--shell cmd|pwsh]
//!
//! - `term_keypress`: a local shell runs in a terminal panel that fills the
//!   main area. Each key goes through `Window::dispatch_keystroke` into the
//!   focused terminal, which writes it to the shell. When the terminal
//!   wakes the window with new output, the frame is drawn at once (as
//!   `composer_bench` draws its frame), and the sample runs from the key's
//!   dispatch to the end of the first frame whose painted screen shows the
//!   echo on the cursor's row: the PTY round trip, the shell's echo and the
//!   app's frame. p99 against 16 ms.
//! - `term_stream`: the panel then runs this example again as a program
//!   that prints `--lines` lines (1,000,000) as fast as the PTY takes them;
//!   every frame drawn while they stream is timed, begin to end of paint.
//!   p99 against 8 ms.
//!
//! `--print-lines N` is that printing mode. `--unthrottled` keeps gpui from
//! slowing frames while another window has focus; `just bench` runs with it.

use std::cell::RefCell;
use std::io::Write as _;
use std::rc::Rc;
use std::time::{Duration, Instant};

use app::appearance::ThemeFlags;
use app::scenario::keystroke_for;
use app::shell::{Probe, Shell};
use app::terminal_panel::{RunState, TerminalPanel};
use app::terminals::TerminalSpec;
use app::{Setup, assets, new_dock, window_options};
use diff_view::FrameClock;
use gpui::{App, AppContext as _, Context, Entity, Focusable as _, Keystroke, Window};
use gpui_kit::component::dock::{DockArea, DockLayout, panel_handle};
use term::SessionConfig;
use term::alacritty_terminal::grid::Dimensions as _;
use term::alacritty_terminal::index::{Column, Line, Point};
use term_view::TerminalView;
use theme::ModeSetting;

const KEY_BUDGET_MS: f64 = 16.0;
const STREAM_BUDGET_MS: f64 = 8.0;
const WARMUP_FRAMES: usize = 30;
const WARMUP_KEYS: usize = 10;
/// Frames after the stream's panel is put in place before they are timed.
const STREAM_WARMUP_FRAMES: usize = 3;
/// Frames between two keys, so each key is a sample of its own.
const SETTLE_FRAMES: usize = 2;
/// Keys typed on one line before it is cleared.
const LINE_KEYS: usize = 24;
const TEXT: &str = "thequickbrownfoxjumpsoverthelazydog0123456789";
const WAIT: Duration = Duration::from_secs(60);
const STREAM_WAIT: Duration = Duration::from_secs(900);
/// A key whose echo has not shown by then counts as lost.
const ECHO_WAIT: Duration = Duration::from_secs(5);

struct Args {
    keys: usize,
    lines: usize,
    shell: String,
    unthrottled: bool,
    print_lines: Option<usize>,
}

impl Args {
    fn parse() -> Self {
        let mut args = Self {
            keys: 200,
            lines: 1_000_000,
            shell: if cfg!(windows) { "cmd" } else { "sh" }.to_owned(),
            unthrottled: false,
            print_lines: None,
        };
        let mut it = std::env::args().skip(1);
        let number = |value: Option<String>, flag: &str| {
            value
                .and_then(|v| v.parse().ok())
                .unwrap_or_else(|| panic!("{flag} takes a number"))
        };
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--keys" => args.keys = number(it.next(), "--keys"),
                "--lines" => args.lines = number(it.next(), "--lines"),
                "--print-lines" => args.print_lines = Some(number(it.next(), "--print-lines")),
                "--shell" => args.shell = it.next().expect("--shell takes cmd, pwsh or sh"),
                "--unthrottled" => args.unthrottled = true,
                other => panic!(
                    "unknown argument {other}; expected --keys N, --lines N, --shell S, --unthrottled"
                ),
            }
        }
        args
    }
}

/// The printing mode: `count` numbered lines on stdout, buffered.
fn print_lines(count: usize) {
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::with_capacity(1 << 16, stdout.lock());
    for n in 1..=count {
        let _ = writeln!(
            out,
            "line {n:07} the quick brown fox jumps over the lazy dog"
        );
    }
    let _ = out.flush();
}

fn shell_config(shell: &str) -> SessionConfig {
    let mut config = SessionConfig::default();
    match shell {
        "pwsh" => {
            config.program = "pwsh".to_owned();
            config.args = ["-NoLogo", "-NoProfile"].map(str::to_owned).to_vec();
        }
        // A prompt like cmd's, which the bench waits for.
        "sh" => {
            config.program = "/bin/sh".to_owned();
            config.env.insert("PS1".to_owned(), ">".to_owned());
        }
        _ => {
            config.program = "cmd.exe".to_owned();
            config.args = vec!["/Q".to_owned()];
        }
    }
    let scratch = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.scratch");
    config.cwd = scratch.canonicalize().ok();
    config
}

fn printer_config(lines: usize) -> SessionConfig {
    SessionConfig {
        program: std::env::current_exe()
            .expect("the bench's own path")
            .to_string_lossy()
            .into_owned(),
        args: vec!["--print-lines".to_owned(), lines.to_string()],
        ..SessionConfig::default()
    }
}

/// The text of the cursor's row, trailing blanks trimmed.
fn cursor_row(view: &TerminalView) -> String {
    let term = view.session().lock();
    let line = term.grid().cursor.point.line;
    let mut text: String = (0..term.columns())
        .map(|col| term.grid()[Point::new(line, Column(col))].c)
        .collect();
    text.truncate(text.trim_end().len());
    text
}

/// Whether any row on screen starts with `prefix`.
fn screen_has(view: &TerminalView, prefix: &str) -> bool {
    let term = view.session().lock();
    (0..term.screen_lines()).any(|row| {
        let line = Line(row as i32);
        let text: String = (0..term.columns().min(prefix.len()))
            .map(|col| term.grid()[Point::new(line, Column(col))].c)
            .collect();
        text == prefix
    })
}

/// One key on its way to the screen.
struct Pending {
    sent: Instant,
    /// What the cursor's row must contain once it is echoed.
    expect: String,
    timed: bool,
}

/// The key phase's state, shared with the observer that draws on output.
#[derive(Default)]
struct Keys {
    pending: Option<Pending>,
    typed: String,
    count: usize,
    samples: perf::Samples,
    /// Draws whose screen moved on while they painted, so were not judged.
    moved: usize,
    /// Keys whose echo never showed.
    lost: usize,
}

enum Phase {
    Warmup,
    Prompt,
    Typing,
    Clearing,
    Streaming,
    Done,
}

struct TermBench {
    clock: FrameClock,
    dock: Entity<DockArea>,
    panel: Entity<TerminalPanel>,
    keys: Rc<RefCell<Keys>>,
    total_keys: usize,
    lines: usize,
    /// The key that empties the shell's line.
    clear_key: &'static str,
    phase: Phase,
    frame: usize,
    settle: usize,
    started: Instant,
    observing: bool,
    stream_started: Option<Instant>,
    stream_warmup: usize,
    stream_report: Option<perf::Report>,
}

impl TermBench {
    fn view(&self, cx: &App) -> Entity<TerminalView> {
        self.panel
            .read(cx)
            .view()
            .cloned()
            .expect("the terminal is running")
    }

    /// Draws as soon as the terminal has new output while a key waits, and
    /// takes the sample from the first frame that shows its echo.
    fn observe(&mut self, window: &mut Window, cx: &mut Context<Shell>) {
        if self.observing {
            return;
        }
        self.observing = true;
        let (keys, clock) = (self.keys.clone(), self.clock.clone());
        let view = self.view(cx);
        cx.observe_in(&view, window, move |_, view, window, cx| {
            if keys.borrow().pending.is_none() {
                return;
            }
            let (keys, clock, view) = (keys.clone(), clock.clone(), view.clone());
            window.defer(cx, move |window, cx| {
                let before = view.read(cx).session().generation();
                window.draw(cx).clear(cx);
                let after = view.read(cx).session().generation();
                let mut keys = keys.borrow_mut();
                let Some(pending) = keys.pending.as_ref() else {
                    return;
                };
                if pending.sent.elapsed() > ECHO_WAIT {
                    keys.lost += 1;
                    keys.pending = None;
                    window.refresh();
                    return;
                }
                if before != after {
                    keys.moved += 1;
                    return;
                }
                if !cursor_row(view.read(cx)).contains(&pending.expect) {
                    return;
                }
                let end = clock.last_frame_end().expect("a frame was drawn");
                let elapsed = end.saturating_duration_since(pending.sent);
                if pending.timed {
                    keys.samples.push(elapsed);
                }
                keys.pending = None;
                window.refresh();
            });
        })
        .detach();
    }

    fn type_key(&mut self, window: &mut Window, cx: &mut Context<Shell>) {
        let (count, typed) = {
            let keys = self.keys.borrow();
            (keys.count, keys.typed.clone())
        };
        if count >= self.total_keys {
            self.finish_keys(window, cx);
            return;
        }
        if typed.len() >= LINE_KEYS {
            self.clear_line(window, cx);
            return;
        }
        let ch = TEXT
            .chars()
            .cycle()
            .nth(count)
            .expect("the text is not empty");
        let expect = format!("{typed}{ch}");
        let keys = self.keys.clone();
        window.defer(cx, move |window, cx| {
            {
                let mut keys = keys.borrow_mut();
                keys.typed.push(ch);
                keys.count += 1;
                keys.pending = Some(Pending {
                    sent: Instant::now(),
                    expect,
                    timed: count >= WARMUP_KEYS,
                });
            }
            window.dispatch_keystroke(keystroke_for(ch), cx);
        });
    }

    /// Empties the shell's line (not timed).
    fn clear_line(&mut self, window: &mut Window, cx: &mut Context<Shell>) {
        self.keys.borrow_mut().typed.clear();
        let key = self.clear_key;
        window.defer(cx, move |window, cx| {
            window.dispatch_keystroke(Keystroke::parse(key).expect("a key"), cx);
        });
        self.phase = Phase::Clearing;
        self.started = Instant::now();
    }

    fn finish_keys(&mut self, window: &mut Window, cx: &mut Context<Shell>) {
        let keys = self.keys.borrow();
        let mut report = keys
            .samples
            .report("term_keypress", 99.0, KEY_BUDGET_MS)
            .with("keys", keys.samples.len())
            .with("moved_frames", keys.moved)
            .with("lost_keys", keys.lost);
        report.pass &= keys.lost == 0;
        report.print();
        drop(keys);
        // The stream takes the panel's place; the shell's program ends as
        // its panel leaves the dock.
        let spec = self.panel.read(cx).spec().clone();
        let panel = TerminalPanel::spawn_config(spec, printer_config(self.lines), cx);
        self.panel = panel.clone();
        let dock = self.dock.clone();
        window.defer(cx, move |window, cx| {
            let layout = DockLayout::tabs().panel_view(panel_handle(panel), cx);
            dock.update(cx, |dock, cx| dock.set_center(layout, window, cx));
        });
        self.stream_warmup = STREAM_WARMUP_FRAMES;
        self.stream_started = Some(Instant::now());
        self.phase = Phase::Streaming;
    }

    fn streaming(&mut self, cx: &mut Context<Shell>) {
        // The frames that build the new panel and lay out its first screen
        // are not part of the stream.
        if self.stream_warmup > 0 {
            self.stream_warmup -= 1;
            if self.stream_warmup == 0 {
                self.clock.set_recording(true);
            }
        }
        let started = self.stream_started.expect("the stream started");
        assert!(
            started.elapsed() < STREAM_WAIT,
            "{} lines did not stream within {STREAM_WAIT:?}",
            self.lines
        );
        let exited = matches!(self.panel.read(cx).state(), RunState::Exited(_));
        let Some(view) = self.panel.read(cx).view().cloned() else {
            return;
        };
        let last = format!("line {:07}", self.lines);
        if !(exited && screen_has(view.read(cx), &last)) {
            return;
        }
        self.clock.set_recording(false);
        let seconds = started.elapsed().as_secs_f64();
        let frames = self.clock.samples();
        let report = frames
            .report("term_stream", 99.0, STREAM_BUDGET_MS)
            .with("lines", self.lines)
            .with("seconds", (seconds * 10.0).round() / 10.0)
            .with("lines_per_second", (self.lines as f64 / seconds).round())
            .with("dropped_frames", self.clock.dropped_frames());
        self.stream_report = Some(report);
        self.phase = Phase::Done;
    }
}

impl Probe for TermBench {
    fn clock(&self) -> &FrameClock {
        &self.clock
    }

    fn frame(&mut self, window: &mut Window, cx: &mut Context<Shell>) {
        match self.phase {
            Phase::Warmup => {
                self.frame += 1;
                if self.frame >= WARMUP_FRAMES {
                    let view = self.view(cx);
                    window.defer(cx, move |window, cx| {
                        window.focus(&view.focus_handle(cx), cx);
                    });
                    self.started = Instant::now();
                    self.phase = Phase::Prompt;
                }
            }
            Phase::Prompt => {
                assert!(self.started.elapsed() < WAIT, "no prompt within {WAIT:?}");
                let view = self.view(cx);
                let row = cursor_row(view.read(cx));
                if row.ends_with('>') {
                    self.observe(window, cx);
                    self.settle = SETTLE_FRAMES * 10;
                    self.phase = Phase::Typing;
                }
            }
            Phase::Typing => {
                if self.keys.borrow().pending.is_some() {
                    // The observer draws the frames until the echo shows.
                    return;
                }
                if self.settle > 0 {
                    self.settle -= 1;
                } else {
                    self.settle = SETTLE_FRAMES;
                    self.type_key(window, cx);
                }
            }
            Phase::Clearing => {
                assert!(
                    self.started.elapsed() < WAIT,
                    "the clear key did not clear the line"
                );
                let view = self.view(cx);
                if cursor_row(view.read(cx)).ends_with('>') {
                    self.settle = SETTLE_FRAMES;
                    self.phase = Phase::Typing;
                }
            }
            Phase::Streaming => self.streaming(cx),
            Phase::Done => {
                if let Some(report) = self.stream_report.take() {
                    report.print();
                }
                if let Some(view) = self.panel.read(cx).view().cloned() {
                    view.update(cx, |view, cx| view.close(cx));
                }
                cx.quit();
                return;
            }
        }
        window.request_animation_frame();
    }
}

fn main() {
    let args = Args::parse();
    if let Some(count) = args.print_lines {
        print_lines(count);
        return;
    }
    gpui_kit::application()
        .with_assets(assets::AppAssets)
        .run(move |cx: &mut App| {
            app::init(
                cx,
                &Setup {
                    fixtures: true,
                    theme: ThemeFlags {
                        family: Some("Vercel".to_owned()),
                        mode: Some(ModeSetting::Dark),
                    },
                    ..Setup::default()
                },
            );
            let options = window_options(cx, !args.unthrottled);
            let (shell, keys, lines) = (args.shell.clone(), args.keys, args.lines);
            gpui_kit::open_window(options, cx, move |window, cx| {
                let dock = new_dock(window, cx);
                let spec = TerminalSpec {
                    machine: app::machines::THIS_PC.to_owned(),
                    shell: Some(shell.clone()),
                    ..TerminalSpec::default()
                };
                let panel = TerminalPanel::spawn_config(spec, shell_config(&shell), cx);
                let layout = DockLayout::tabs().panel_view(panel_handle(panel.clone()), cx);
                dock.update(cx, |dock, cx| dock.set_center(layout, window, cx));
                let bench = TermBench {
                    clock: FrameClock::new(),
                    dock: dock.clone(),
                    panel,
                    keys: Rc::default(),
                    total_keys: WARMUP_KEYS + keys,
                    lines,
                    // `sh` reads its line through the tty, where Ctrl+U is the
                    // kill character; Esc would be typed into the line.
                    clear_key: if shell == "sh" { "ctrl-u" } else { "escape" },
                    phase: Phase::Warmup,
                    frame: 0,
                    settle: 0,
                    started: Instant::now(),
                    observing: false,
                    stream_started: None,
                    stream_warmup: 0,
                    stream_report: None,
                };
                cx.new(|cx| Shell::new(dock, false, window, cx).with_probe(Box::new(bench)))
            })
            .expect("open the main window");
            cx.activate(true);
        });
}
