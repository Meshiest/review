use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use gpui::prelude::*;
use gpui::{
    App, Bounds, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels,
    Render, ScrollEvent, Size, Styled, Task, Window, div,
};
use scroll::Scroller;
use term::alacritty_terminal::Term;
use term::alacritty_terminal::event::EventListener;
use term::alacritty_terminal::grid::{Dimensions, Scroll};
use term::alacritty_terminal::selection::SelectionRange;
use term::alacritty_terminal::term::TermMode;
use term::{ExitState, GridSize, Session, SessionConfig, SpawnError};

use crate::blink::Blink;
use crate::element::{Frame, TerminalElement};
use crate::frame::{PaintRow, Painter, cell_size, fit, to_u16};
use crate::geometry::{Geometry, Hit, grid_bounds};
use crate::keys::{self, Local};
use crate::mouse::{self, Action, Button, Modifiers, Report};
use crate::select::{self, Held};
use crate::snapshot::{cursor_snapshot, snapshot};
use crate::style::TermStyle;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalEvent {
    /// The program ended; the screen stays readable.
    Exited(ExitState),
}

/// Settings of one terminal that are not part of its look.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Options {
    cursor_blink: bool,
    copy_on_select: bool,
}

/// What the rows of a cached frame were built from.
#[derive(Clone, Copy, Debug, PartialEq)]
struct FrameKey {
    generation: u64,
    display_offset: usize,
    cols: usize,
    rows: usize,
    cell: Size<Pixels>,
    bounds: Bounds<Pixels>,
    scale: f32,
    style_version: u64,
    selection: Option<SelectionRange>,
}

/// A terminal pane: the grid, scrollback, selection, keyboard, mouse and paste.
pub struct TerminalView {
    session: Arc<Session>,
    style: TermStyle,
    /// Counts style changes, so cached layout is rebuilt after one.
    style_version: u64,
    focus: FocusHandle,
    scroll: Scroller,
    options: Options,
    blink: Blink,
    blink_task: Option<Task<()>>,
    /// Whether the pane had focus when it was last painted.
    focused: bool,
    geometry: Option<Geometry>,
    cell_size: Memo<(u64, f32), Size<Pixels>>,
    cache: Option<(FrameKey, Rc<Vec<PaintRow>>)>,
    last_generation: u64,
    /// The scroll position and history length the scroller was last set to.
    synced: (usize, usize),
    /// Wheel movement not yet worth a whole line.
    wheel_lines: f32,
    selecting: bool,
    /// The button the program was told is down, and where.
    held: Option<(Button, (usize, usize))>,
    hover_cell: Option<(usize, usize)>,
}

impl EventEmitter<TerminalEvent> for TerminalView {}

impl TerminalView {
    /// Starts the program in `config` and shows it. The session's change
    /// notification becomes a redraw of this view; `config.on_change` is replaced.
    ///
    /// # Errors
    /// When the program cannot be started.
    pub fn spawn(
        mut config: SessionConfig,
        style: TermStyle,
        cx: &mut App,
    ) -> Result<Entity<Self>, SpawnError> {
        let (wake, woken) = async_channel::bounded::<()>(1);
        config.on_change = Some(Arc::new(move || {
            // A full channel already has a redraw pending.
            let _ = wake.try_send(());
        }));
        let session = Arc::new(Session::spawn(config)?);

        Ok(cx.new(|cx: &mut Context<Self>| {
            cx.spawn(async move |this, cx| {
                let mut exited = false;
                while woken.recv().await.is_ok() {
                    let result = this.update(cx, |view, cx| {
                        cx.notify();
                        let state = view.session.exit_state();
                        if !exited && state != ExitState::Running {
                            exited = true;
                            cx.emit(TerminalEvent::Exited(state));
                        }
                    });
                    if result.is_err() {
                        break;
                    }
                }
            })
            .detach();

            let mut scroll = Scroller::new(style.scroll);
            // A program that asks for the mouse gets middle clicks.
            scroll.set_start_filter({
                let session = Arc::clone(&session);
                move |_, _, _| !mouse::reporting(*session.lock().mode())
            });
            let mut view = Self {
                session,
                style,
                style_version: 0,
                focus: cx.focus_handle(),
                scroll,
                options: Options {
                    cursor_blink: true,
                    copy_on_select: false,
                },
                blink: Blink::new(Instant::now()),
                blink_task: None,
                focused: false,
                geometry: None,
                cell_size: Memo::default(),
                cache: None,
                last_generation: 0,
                synced: (0, 0),
                wheel_lines: 0.0,
                selecting: false,
                held: None,
                hover_cell: None,
            };
            view.start_blink(cx);
            view
        }))
    }

    pub fn session(&self) -> &Arc<Session> {
        &self.session
    }

    pub fn style(&self) -> &TermStyle {
        &self.style
    }

    /// Switches font and colors, for a theme or font change. The next frame
    /// recomputes the cell size, resizes the session to fit and lays the rows
    /// out again; the scroll position and selection stay.
    pub fn set_style(&mut self, style: TermStyle, cx: &mut Context<Self>) {
        self.scroll.set_style(style.scroll);
        self.style = style;
        self.style_version += 1;
        cx.notify();
    }

    /// Whether the cursor blinks while the pane has focus. On by default.
    #[must_use]
    pub fn cursor_blink(&self) -> bool {
        self.options.cursor_blink
    }

    pub fn set_cursor_blink(&mut self, on: bool, cx: &mut Context<Self>) {
        self.options.cursor_blink = on;
        if on {
            self.blink.restart(Instant::now());
            self.start_blink(cx);
        } else {
            self.blink_task = None;
        }
        cx.notify();
    }

    /// Whether finishing a selection copies it. Off by default.
    #[must_use]
    pub fn copy_on_select(&self) -> bool {
        self.options.copy_on_select
    }

    pub fn set_copy_on_select(&mut self, on: bool) {
        self.options.copy_on_select = on;
    }

    /// The selected text, if anything is selected.
    #[must_use]
    pub fn selected_text(&self) -> Option<String> {
        select::text(&self.session.lock())
    }

    /// Copies the selection to the clipboard; false when nothing is selected.
    pub fn copy_selection(&self, cx: &mut Context<Self>) -> bool {
        let Some(text) = self.selected_text() else {
            return false;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        true
    }

    pub fn clear_selection(&mut self, cx: &mut Context<Self>) {
        self.session.lock().selection = None;
        self.selecting = false;
        cx.notify();
    }

    /// Scrolls the history: by lines, a page, or to either end.
    pub fn scroll(&mut self, scroll: Scroll, cx: &mut Context<Self>) {
        self.session.lock().scroll_display(scroll);
        cx.notify();
    }

    /// How far back the view is scrolled, and how many lines of history exist.
    #[must_use]
    pub fn scroll_position(&self) -> (usize, usize) {
        let term = self.session.lock();
        (term.grid().display_offset(), term.grid().history_size())
    }

    /// Ends the program and everything it started, without waiting. The
    /// screen stays readable and [`TerminalEvent::Exited`] follows with the
    /// exit status.
    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.session.close();
        cx.notify();
    }

    pub(crate) fn scroller_mut(&mut self) -> &mut Scroller {
        &mut self.scroll
    }

    pub(crate) fn paint_scroll(&self, window: &mut Window) {
        self.scroll.paint(window);
    }

    /// Flips the cursor on a timer. The task repaints only; the rows come
    /// from the cache.
    fn start_blink(&mut self, cx: &mut Context<Self>) {
        if self.blink_task.is_some() || !self.options.cursor_blink {
            return;
        }
        self.blink_task = Some(cx.spawn(async move |this, cx| {
            loop {
                let Ok(wait) =
                    this.read_with(cx, |view, _| view.blink.until_toggle(Instant::now()))
                else {
                    break;
                };
                cx.background_executor().timer(wait).await;
                let result = this.update(cx, |view, cx| {
                    if view.focused {
                        cx.notify();
                    }
                });
                if result.is_err() {
                    break;
                }
            }
        }));
    }

    fn mode(&self) -> TermMode {
        *self.session.lock().mode()
    }

    // Painting.

    /// Brings the view up to date for a frame in `bounds`: sizes the grid to
    /// fit, follows the scroller, and lays out what changed since last frame.
    pub(crate) fn prepare(
        &mut self,
        pane: Bounds<Pixels>,
        window: &Window,
        _cx: &mut Context<Self>,
    ) -> Frame {
        // The grid sits inside the padding; the scroller and the drag edges
        // use the whole pane.
        let bounds = grid_bounds(pane, self.style.padding);
        let scale = window.scale_factor();
        let cell = self.cell_size(window, scale);
        let cols = fit(bounds.size.width, cell.width);
        let rows = fit(bounds.size.height, cell.height);
        self.session.resize(GridSize {
            cols,
            rows,
            cell_width: to_u16(cell.width),
            cell_height: to_u16(cell.height),
        });
        let focused = self.focus.is_focused(window);
        self.focused = focused;
        let generation = self.session.generation();
        if generation != self.last_generation {
            self.last_generation = generation;
            self.blink.restart(Instant::now());
        }

        let (max_cols, max_rows) = (usize::from(cols), usize::from(rows));
        let mut term = self.session.lock_for_paint();
        let history = term.grid().history_size();
        let mut display_offset = term.grid().display_offset();
        let screen_lines = term.screen_lines();

        // The scroller counts pixels from the top of the history.
        let line = f64::from(cell.height);
        let leftover = f64::from(pane.size.height) - screen_lines as f64 * line;
        let axis = &mut self.scroll.state.y;
        axis.content = (history + screen_lines) as f64 * line + leftover.max(0.0);
        if self.synced != (display_offset, history) {
            axis.offset = (history - display_offset.min(history)) as f64 * line;
        }
        if self.scroll.frame(pane, window) {
            let from_top = (self.scroll.state.y.offset / line).round() as usize;
            let wanted = history.saturating_sub(from_top);
            let delta = wanted as i32 - display_offset as i32;
            if delta != 0 {
                term.scroll_display(Scroll::Delta(delta));
                display_offset = term.grid().display_offset();
            }
        }
        self.synced = (display_offset, history);

        let selection = term
            .selection
            .as_ref()
            .and_then(|selection| selection.to_range(&term));
        let key = FrameKey {
            generation,
            display_offset,
            cols: max_cols,
            rows: max_rows,
            cell,
            bounds,
            scale,
            style_version: self.style_version,
            selection,
        };
        let cached = self
            .cache
            .as_ref()
            .filter(|(cached, _)| *cached == key)
            .map(|(_, rows)| Rc::clone(rows));
        let snapshot_rows = cached
            .is_none()
            .then(|| snapshot(&term, &self.style, selection, max_cols, max_rows));
        let cursor = cursor_snapshot(&term, focused, max_cols, max_rows);
        let geometry = Geometry {
            bounds,
            pane,
            cell,
            cols: term.columns().min(max_cols),
            rows: screen_lines.min(max_rows),
        };
        drop(term);
        self.geometry = Some(geometry);

        let painter = Painter {
            style: &self.style,
            cell,
            scale,
        };
        let rows = cached.unwrap_or_else(|| {
            let rows = Rc::new(painter.rows(
                snapshot_rows.as_deref().unwrap_or_default(),
                bounds.origin,
                window,
            ));
            self.cache = Some((key, Rc::clone(&rows)));
            rows
        });
        let blink_on = !self.options.cursor_blink || !focused || self.blink.visible(Instant::now());
        let cursor = cursor
            .filter(|_| blink_on)
            .map(|cursor| painter.cursor(&cursor, bounds.origin, window));

        Frame {
            cell,
            background: self.style.background.into(),
            selection: self.style.selection,
            cursor_color: self.style.cursor.into(),
            rows,
            cursor,
        }
    }

    fn cell_size(&mut self, window: &Window, scale: f32) -> Size<Pixels> {
        let style = &self.style;
        self.cell_size
            .get((self.style_version, scale), || cell_size(style, window))
    }

    // Keyboard.

    fn on_key_down(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let mode = self.mode();
        if let Some(local) = keys::local(&event.keystroke, mode) {
            match local {
                Local::Copy => {
                    self.copy_selection(cx);
                }
                Local::Paste => self.paste(cx),
                Local::Scroll(scroll) => self.scroll(scroll, cx),
            }
            cx.stop_propagation();
            return;
        }
        let Some(bytes) = keys::encode(&event.keystroke, event.prefer_character_input, mode) else {
            return;
        };
        self.send_input(bytes, cx);
        cx.stop_propagation();
    }

    /// Input from the user: it returns the view to the present, drops the
    /// selection (the text is about to change) and shows the cursor.
    fn send_input(&mut self, bytes: Vec<u8>, cx: &mut Context<Self>) {
        return_to_present(&mut self.session.lock());
        self.selecting = false;
        self.blink.restart(Instant::now());
        self.session.write(bytes);
        cx.notify();
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        let bracketed = self.mode().contains(TermMode::BRACKETED_PASTE);
        self.send_input(paste_bytes(&text, bracketed), cx);
    }

    // Mouse.

    pub(crate) fn mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus, cx);
        let Some(geometry) = self.geometry else {
            return;
        };
        let hit = geometry.locate(event.position);
        let mode = self.mode();
        if mouse::reporting(mode) && !event.modifiers.shift {
            if let Some(button) = report_button(event.button) {
                self.held = Some((button, (hit.col, hit.row)));
                self.send_report(
                    button,
                    Action::Press,
                    event.modifiers,
                    (hit.col, hit.row),
                    mode,
                );
            }
            return;
        }
        if event.button == MouseButton::Left {
            self.start_selection(event, hit);
            cx.notify();
        }
    }

    fn start_selection(&mut self, event: &MouseDownEvent, hit: Hit) {
        let held = Held {
            shift: event.modifiers.shift,
            alt: event.modifiers.alt,
        };
        select::begin(&mut self.session.lock(), hit, event.click_count, held);
        self.selecting = true;
    }

    pub(crate) fn mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        hovered: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(geometry) = self.geometry else {
            return;
        };
        if self.selecting {
            if event.pressed_button == Some(MouseButton::Left) {
                self.drag_selection(event, geometry, cx);
            } else {
                self.finish_selection(cx);
            }
            return;
        }
        let hit = geometry.locate(event.position);
        let cell = (hit.col, hit.row);
        let mode = self.mode();
        if let Some((button, last)) = self.held.as_mut() {
            if event.pressed_button.is_none() {
                self.held = None;
            } else if *last != cell {
                *last = cell;
                let button = *button;
                self.send_report(button, Action::Motion, event.modifiers, cell, mode);
            }
        } else if hovered
            && !event.modifiers.shift
            && self.hover_cell != Some(cell)
            && mode.contains(TermMode::MOUSE_MOTION)
        {
            self.hover_cell = Some(cell);
            self.send_report(Button::None, Action::Motion, event.modifiers, cell, mode);
        }
    }

    fn drag_selection(
        &mut self,
        event: &MouseMoveEvent,
        geometry: Geometry,
        cx: &mut Context<Self>,
    ) {
        let hit = geometry.locate(event.position);
        select::drag(&mut self.session.lock(), hit, geometry.edge(event.position));
        cx.notify();
    }

    fn finish_selection(&mut self, cx: &mut Context<Self>) {
        self.selecting = false;
        let selected = select::finish(&mut self.session.lock());
        if selected && self.options.copy_on_select {
            self.copy_selection(cx);
        }
        cx.notify();
    }

    pub(crate) fn mouse_up(&mut self, event: &MouseUpEvent, cx: &mut Context<Self>) {
        if self.selecting && event.button == MouseButton::Left {
            self.finish_selection(cx);
            return;
        }
        let Some((button, cell)) = self.held else {
            return;
        };
        if report_button(event.button) == Some(button) {
            self.held = None;
            let mode = self.mode();
            self.send_report(button, Action::Release, event.modifiers, cell, mode);
        }
    }

    fn send_report(
        &self,
        button: Button,
        action: Action,
        modifiers: gpui::Modifiers,
        (col, row): (usize, usize),
        mode: TermMode,
    ) {
        let report = Report {
            button,
            action,
            modifiers: Modifiers {
                shift: modifiers.shift,
                alt: modifiers.alt,
                control: modifiers.control,
            },
            col,
            row,
        };
        if let Some(bytes) = mouse::encode(report, mode) {
            self.session.write(bytes);
        }
    }

    pub(crate) fn wheel(&mut self, event: &ScrollEvent, cx: &mut Context<Self>) {
        let Some(geometry) = self.geometry else {
            return;
        };
        let delta = event.delta.pixel_delta(geometry.cell.height);
        self.wheel_lines += delta.y / geometry.cell.height;
        let lines = self.wheel_lines.trunc();
        if lines == 0.0 {
            return;
        }
        self.wheel_lines -= lines;
        let lines = lines as i32;
        let mode = self.mode();
        match wheel_action(lines, mode, event.modifiers.shift) {
            Wheel::Report(button) => {
                let hit = geometry.locate(event.position);
                for _ in 0..lines.unsigned_abs() {
                    self.send_report(
                        button,
                        Action::Press,
                        event.modifiers,
                        (hit.col, hit.row),
                        mode,
                    );
                }
            }
            Wheel::Arrows(arrow) => {
                let count = lines.unsigned_abs() as usize;
                self.session.write(arrow.repeat(count));
            }
            Wheel::Scroll => {
                self.session.lock().scroll_display(Scroll::Delta(lines));
                cx.notify();
            }
        }
    }

    /// The bounds of a cell of the visible grid, in window coordinates, as of
    /// the last frame. `None` before the first frame or outside the grid.
    #[must_use]
    pub fn cell_bounds(&self, row: usize, col: usize) -> Option<Bounds<Pixels>> {
        let geometry = self.geometry?;
        (row < geometry.rows && col < geometry.cols).then(|| geometry.cell_bounds(row, col))
    }
}

/// What a wheel step of `lines` (positive toward the history) does.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Wheel {
    /// Tell the program, which asked for mouse events.
    Report(Button),
    /// Send this arrow key once per line: a full-screen program without
    /// mouse support scrolls that way.
    Arrows(&'static [u8]),
    /// Scroll the history.
    Scroll,
}

fn wheel_action(lines: i32, mode: TermMode, shift: bool) -> Wheel {
    let up = lines > 0;
    if mouse::reporting(mode) && !shift {
        return Wheel::Report(if up {
            Button::WheelUp
        } else {
            Button::WheelDown
        });
    }
    if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) {
        return Wheel::Arrows(match (up, mode.contains(TermMode::APP_CURSOR)) {
            (true, true) => b"\x1bOA",
            (true, false) => b"\x1b[A",
            (false, true) => b"\x1bOB",
            (false, false) => b"\x1b[B",
        });
    }
    Wheel::Scroll
}

/// The last value computed for a key; it is computed again when the key changes.
struct Memo<K, V> {
    slot: Option<(K, V)>,
}

impl<K, V> Default for Memo<K, V> {
    fn default() -> Self {
        Self { slot: None }
    }
}

impl<K: PartialEq, V: Copy> Memo<K, V> {
    fn get(&mut self, key: K, compute: impl FnOnce() -> V) -> V {
        match self.slot {
            Some((ref last, value)) if *last == key => value,
            _ => {
                let value = compute();
                self.slot = Some((key, value));
                value
            }
        }
    }
}

/// What typing does to the display: back to the newest output, selection
/// dropped because the text under it is about to change.
fn return_to_present<L: EventListener>(term: &mut Term<L>) {
    term.scroll_display(Scroll::Bottom);
    term.selection = None;
}

fn report_button(button: MouseButton) -> Option<Button> {
    match button {
        MouseButton::Left => Some(Button::Left),
        MouseButton::Middle => Some(Button::Middle),
        MouseButton::Right => Some(Button::Right),
        MouseButton::Navigate(_) => None,
    }
}

/// Clipboard text as terminal input: Enter is CR, and in bracketed mode the
/// text is fenced and stripped of ESC so it cannot end the fence early.
fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    let text = text.replace("\r\n", "\r").replace('\n', "\r");
    if !bracketed {
        return text.into_bytes();
    }
    let mut out = b"\x1b[200~".to_vec();
    out.extend(text.replace('\x1b', "").into_bytes());
    out.extend_from_slice(b"\x1b[201~");
    out
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for TerminalView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .key_context("Terminal")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                this.on_key_down(event, cx);
            }))
            .child(TerminalElement { view: cx.entity() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paste_converts_newlines() {
        assert_eq!(paste_bytes("a\r\nb\nc", false), b"a\rb\rc");
    }

    #[test]
    fn bracketed_paste_is_fenced_and_stripped_of_escapes() {
        assert_eq!(
            paste_bytes("x\x1b[201~y", true),
            b"\x1b[200~x[201~y\x1b[201~"
        );
    }

    #[test]
    fn typing_returns_to_the_bottom_and_drops_the_selection() {
        use term::alacritty_terminal::event::VoidListener;
        use term::alacritty_terminal::index::Side;
        use term::alacritty_terminal::selection::{Selection, SelectionType};
        use term::alacritty_terminal::term::Config;
        use term::alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

        let size = GridSize {
            cols: 10,
            rows: 3,
            ..GridSize::default()
        };
        let mut term = Term::new(Config::default(), &size, VoidListener);
        Processor::<StdSyncHandler>::new().advance(
            &mut term,
            b"1
2
3
4
5
6",
        );
        term.scroll_display(Scroll::Delta(2));
        term.selection = Some(Selection::new(
            SelectionType::Simple,
            select::grid_point(0, 0, 2),
            Side::Left,
        ));
        return_to_present(&mut term);
        assert_eq!(term.grid().display_offset(), 0);
        assert!(term.selection.is_none());
    }

    #[test]
    fn the_wheel_scrolls_the_history_unless_the_program_wants_it() {
        let plain = TermMode::empty();
        assert_eq!(wheel_action(3, plain, false), Wheel::Scroll);
        assert_eq!(wheel_action(-3, plain, false), Wheel::Scroll);

        let mouse = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        assert_eq!(
            wheel_action(1, mouse, false),
            Wheel::Report(Button::WheelUp)
        );
        assert_eq!(
            wheel_action(-1, mouse, false),
            Wheel::Report(Button::WheelDown)
        );
        assert_eq!(
            wheel_action(1, mouse, true),
            Wheel::Scroll,
            "Shift keeps it"
        );
    }

    #[test]
    fn full_screen_programs_without_a_mouse_get_arrow_keys() {
        let alt = TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL;
        assert_eq!(wheel_action(1, alt, false), Wheel::Arrows(b"\x1b[A"));
        assert_eq!(wheel_action(-1, alt, false), Wheel::Arrows(b"\x1b[B"));
        assert_eq!(
            wheel_action(1, alt | TermMode::APP_CURSOR, false),
            Wheel::Arrows(b"\x1bOA")
        );
        assert_eq!(wheel_action(1, TermMode::ALT_SCREEN, false), Wheel::Scroll);
        assert_eq!(wheel_action(1, alt, true), Wheel::Arrows(b"[A"));
    }

    #[test]
    fn a_memo_computes_again_only_when_its_key_changes() {
        let mut calls = 0;
        let mut memo = Memo::default();
        let mut get = |key: (u64, u32)| {
            memo.get(key, || {
                calls += 1;
                calls * 10
            })
        };
        assert_eq!(get((0, 1)), 10);
        assert_eq!(get((0, 1)), 10);
        assert_eq!(get((1, 1)), 20, "a new style version");
        assert_eq!(get((1, 2)), 30, "a new scale");
        assert_eq!(get((1, 2)), 30);
    }
}
