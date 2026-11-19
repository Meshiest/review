//! A window running a shell in the current directory.
//!
//! `cargo run -p term_view --example shell` runs `pwsh`; arguments after
//! `--` replace the program and its arguments. The environment variables
//! `SHELL_NO_BLINK` and `SHELL_COPY_ON_SELECT` switch those settings.
//!
//! Two flags drive the window from inside, by dispatching gpui events into
//! it (never through the operating system), print what they did as `DEMO`
//! lines, and close the window after `--demo-hold SECONDS` (default 30):
//!
//! - `--demo-selection`: prints 300 lines, scrolls back with the mouse wheel,
//!   drags a selection over wide characters and rows of history, and
//!   double-clicks a word.
//! - `--demo-style`: switches to a larger font and prints the grid size
//!   before and after, to show the session was resized to the new cells.

use std::time::{Duration, Instant};

use gpui::{
    AnyWindowHandle, App, AsyncApp, Bounds, Focusable, Modifiers, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, PlatformInput, Point, ScrollDelta, ScrollEvent, WindowBounds,
    WindowHandle, WindowOptions, point, px, size,
};
use gpui_platform::application;
use term::{SessionConfig, visible_text};
use term_view::{TermStyle, TerminalEvent, TerminalView};
use theme::{ModeSetting, ThemeRegistry};

/// The last line the demo program prints, to know it has finished.
const LAST_LINE: &str = "line 300";
/// Time for the window to paint between demo steps.
const STEP: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Demo {
    Selection,
    Style,
}

fn main() {
    let mut args = std::env::args().skip(1).peekable();
    let mut demo = None;
    let mut hold = Duration::from_secs(30);
    while let Some(flag) = args.next_if(|arg| arg.starts_with("--demo")) {
        match flag.as_str() {
            "--demo-selection" => demo = Some(Demo::Selection),
            "--demo-style" => demo = Some(Demo::Style),
            "--demo-hold" => {
                let seconds = args.next().and_then(|value| value.parse().ok());
                hold = Duration::from_secs(seconds.expect("--demo-hold needs seconds"));
            }
            other => panic!("unknown flag {other}"),
        }
    }
    let mut config = SessionConfig::default();
    if demo.is_some() {
        config.program = "pwsh".to_owned();
        config.args = [
            "-NoLogo",
            "-NoProfile",
            "-NoExit",
            "-Command",
            "1..300 | ForEach-Object { \"line $_ the quick brown fox 日本語\" }",
        ]
        .map(String::from)
        .to_vec();
    } else if let Some(program) = args.next() {
        config.program = program;
        config.args = args.collect();
    }
    config.cwd = std::env::current_dir().ok();

    application().run(move |cx: &mut App| {
        theme::fonts::register(cx);
        term_view::init(cx);
        let theme = ThemeRegistry::builtin().select(
            theme::DEFAULT_FAMILY,
            ModeSetting::System,
            cx.window_appearance(),
        );
        let style = TermStyle::from_theme(&theme);
        let bounds = Bounds::centered(None, size(px(960.), px(560.)), cx);
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..WindowOptions::default()
            },
            |window, cx| {
                let view = TerminalView::spawn(config, style, cx).expect("start the shell");
                view.update(cx, |view, cx| {
                    view.set_cursor_blink(std::env::var_os("SHELL_NO_BLINK").is_none(), cx);
                    view.set_copy_on_select(std::env::var_os("SHELL_COPY_ON_SELECT").is_some());
                });
                window.focus(&view.focus_handle(cx), cx);
                if demo.is_none() {
                    cx.subscribe(&view, |_, _: &TerminalEvent, cx| cx.quit())
                        .detach();
                }
                view
            },
        );
        match opened {
            Ok(handle) => {
                if let Some(demo) = demo {
                    cx.spawn(async move |cx| {
                        if let Err(error) = run_demo(demo, handle, hold, cx).await {
                            println!("DEMO failed: {error}");
                        }
                        cx.update(|cx| cx.quit());
                    })
                    .detach();
                }
            }
            Err(error) => {
                eprintln!("could not open a window: {error:#}");
                cx.quit();
            }
        }
        cx.activate(true);
    });
}

async fn run_demo(
    demo: Demo,
    handle: WindowHandle<TerminalView>,
    hold: Duration,
    cx: &AsyncApp,
) -> Result<(), String> {
    let started = Instant::now();
    wait_for(cx, "the program's output", || {
        cx.update(|cx| {
            let view = handle.read(cx).ok()?;
            let rows = visible_text(&view.session().lock());
            rows.iter()
                .any(|row| row.starts_with(LAST_LINE))
                .then_some(())
        })
        .is_some()
    })
    .await?;
    pause(cx).await;
    match demo {
        Demo::Selection => selection(handle, cx).await?,
        Demo::Style => restyle(handle, cx).await?,
    }
    cx.background_executor()
        .timer(hold.saturating_sub(started.elapsed()))
        .await;
    Ok(())
}

/// Scrolls back with the wheel, drags a selection and double-clicks a word.
async fn selection(handle: WindowHandle<TerminalView>, cx: &AsyncApp) -> Result<(), String> {
    let cell = |row, col| cell_center(handle, row, col, cx);
    let inside = cell(8, 12)?;

    dispatch(
        handle,
        cx,
        PlatformInput::Scroll(ScrollEvent {
            position: inside,
            delta: ScrollDelta::Lines(point(0.0, 40.0)),
            ..ScrollEvent::default()
        }),
    )
    .await?;
    let (offset, history) = handle
        .read_with(cx, |view, _| view.scroll_position())
        .map_err(|e| e.to_string())?;
    println!("DEMO scrolled back {offset} of {history} history lines");

    // A word, by double click.
    let word = cell(3, 2)?;
    for clicks in [1, 2] {
        click(handle, cx, word, clicks).await?;
    }
    println!("DEMO double click selected {:?}", selected(handle, cx)?);

    // A drag from the middle of one row to the wide characters of another.
    let (from, to) = (cell(10, 20)?, cell(15, 32)?);
    dispatch(handle, cx, mouse_move(from, None)).await?;
    dispatch(handle, cx, mouse_down(from, 1)).await?;
    dispatch(
        handle,
        cx,
        mouse_move(cell(12, 25)?, Some(MouseButton::Left)),
    )
    .await?;
    dispatch(handle, cx, mouse_move(to, Some(MouseButton::Left))).await?;
    dispatch(handle, cx, mouse_up(to, 1)).await?;
    println!("DEMO drag selected {:?}", selected(handle, cx)?);
    Ok(())
}

/// Applies a larger font and reports the grid before and after.
async fn restyle(handle: WindowHandle<TerminalView>, cx: &AsyncApp) -> Result<(), String> {
    let grid = |cx: &AsyncApp| {
        handle
            .read_with(cx, |view, _| {
                let size = view.session().size();
                (size.cols, size.rows, size.cell_width, size.cell_height)
            })
            .map_err(|e| e.to_string())
    };
    let before = grid(cx)?;
    cx.update(|cx| {
        handle.update(cx, |view, _, cx| {
            let mut style = view.style().clone();
            style.font_size *= 1.5;
            view.set_style(style, cx);
        })
    })
    .map_err(|e| e.to_string())?;
    pause(cx).await;
    pause(cx).await;
    let after = grid(cx)?;
    println!(
        "DEMO grid {}x{} cells of {}x{} px -> {}x{} cells of {}x{} px",
        before.0, before.1, before.2, before.3, after.0, after.1, after.2, after.3
    );
    Ok(())
}

fn selected(handle: WindowHandle<TerminalView>, cx: &AsyncApp) -> Result<Option<String>, String> {
    handle
        .read_with(cx, |view, _| view.selected_text())
        .map_err(|e| e.to_string())
}

fn cell_center(
    handle: WindowHandle<TerminalView>,
    row: usize,
    col: usize,
    cx: &AsyncApp,
) -> Result<Point<gpui::Pixels>, String> {
    handle
        .read_with(cx, |view, _| view.cell_bounds(row, col))
        .map_err(|e| e.to_string())?
        .map(|bounds| bounds.center())
        .ok_or_else(|| format!("no cell at row {row}, column {col}"))
}

fn mouse_move(position: Point<gpui::Pixels>, pressed_button: Option<MouseButton>) -> PlatformInput {
    PlatformInput::MouseMove(MouseMoveEvent {
        position,
        pressed_button,
        modifiers: Modifiers::default(),
    })
}

fn mouse_down(position: Point<gpui::Pixels>, click_count: usize) -> PlatformInput {
    PlatformInput::MouseDown(MouseDownEvent {
        button: MouseButton::Left,
        position,
        modifiers: Modifiers::default(),
        click_count,
        first_mouse: false,
    })
}

fn mouse_up(position: Point<gpui::Pixels>, click_count: usize) -> PlatformInput {
    PlatformInput::MouseUp(MouseUpEvent {
        button: MouseButton::Left,
        position,
        modifiers: Modifiers::default(),
        click_count,
    })
}

async fn click(
    handle: WindowHandle<TerminalView>,
    cx: &AsyncApp,
    position: Point<gpui::Pixels>,
    click_count: usize,
) -> Result<(), String> {
    dispatch(handle, cx, mouse_move(position, None)).await?;
    dispatch(handle, cx, mouse_down(position, click_count)).await?;
    dispatch(handle, cx, mouse_up(position, click_count)).await
}

/// Sends `event` into the window's own event path, then lets it paint.
async fn dispatch(
    handle: WindowHandle<TerminalView>,
    cx: &AsyncApp,
    event: PlatformInput,
) -> Result<(), String> {
    // Through the untyped handle: it does not borrow the root view, which the
    // event's listeners update.
    let window = AnyWindowHandle::from(handle);
    cx.update(|cx| window.update(cx, |_, window, cx| window.dispatch_event(event, cx)))
        .map_err(|e| e.to_string())?;
    pause(cx).await;
    Ok(())
}

async fn pause(cx: &AsyncApp) {
    cx.background_executor().timer(STEP).await;
}

async fn wait_for(cx: &AsyncApp, what: &str, mut done: impl FnMut() -> bool) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done() {
        if Instant::now() > deadline {
            return Err(format!("timed out waiting for {what}"));
        }
        cx.background_executor()
            .timer(Duration::from_millis(100))
            .await;
    }
    Ok(())
}
