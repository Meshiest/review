//! Laying out the rows in view, painting them, and the elements around them.

use std::sync::Arc;

use gpui::{
    App, Bounds, ContentMask, Context, CursorStyle, DispatchPhase, Div, Element, ElementId, Entity,
    GlobalElementId, Hitbox, HitboxBehavior, Hsla, InspectorElementId, InteractiveElement,
    IntoElement, KeyDownEvent, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    ParentElement, PathBuilder, Pixels, Point, ScrollEvent, SharedString, Style, Styled, TextAlign,
    Window, div, fill, point, px, relative, size,
};

use super::{Highlight, LineRow, LogView, Metrics, Row};
use crate::buffer::{LineKind, LogBuffer};
use crate::search::{MAX_MATCHES, Match};
use crate::shape::{COLUMN_STEP, LineKey, LineText, dropped_notice, plain_run, push_line};

/// Matches painted at most per frame; a view spanning more is mostly hidden
/// lines.
const MAX_VISIBLE_MATCHES: usize = 4096;

/// What a frame shows, derived from the scroll position.
struct ShapedWindow {
    /// First column of the shaped text and how many are shaped.
    first: usize,
    columns: usize,
}

impl LogView {
    pub(super) fn prepaint(
        &mut self,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.bounds = bounds;
        let was_following = self.follow;
        let shared = Arc::clone(&self.shared);
        let buffer = shared.read();
        self.sync(&buffer, cx);
        let ch = self.char_width(window);
        let lh = self.line_height();
        let rows = Self::lead(&buffer) + self.projection.rows();
        self.metrics = self.measure(bounds, ch, &buffer);
        self.scroll.state.y.content = rows as f64 * lh;
        self.scroll.state.x.content = f64::from(self.metrics.text_left)
            + buffer.max_columns() as f64 * f64::from(ch)
            + f64::from(self.style.scrollbar.width);
        let moved = self.scroll.frame(bounds, window);
        if moved {
            self.after_user_scroll();
        } else if self.follow {
            let y = &mut self.scroll.state.y;
            y.set(y.max());
        }
        self.scroll.state.y.clamp();
        self.scroll.state.x.clamp();
        if !self.follow && self.at_bottom() {
            self.follow = true;
        }
        self.layout_rows(&buffer, rows, window);
        if !self.follow {
            self.remember_anchor(&buffer);
        }
        let jump = self.search.jump
            && self
                .search
                .active
                .as_ref()
                .is_some_and(|s| s.with(|set| !set.matches.is_empty()));
        drop(buffer);
        if self.follow != was_following {
            // The jump pill follows the mode and was drawn before it changed.
            cx.notify();
        }
        if jump {
            self.search.jump = false;
            self.step_match(true, cx);
        }
    }

    fn measure(&self, bounds: Bounds<Pixels>, ch: Pixels, buffer: &LogBuffer) -> Metrics {
        let digits = buffer.total().max(1).ilog10() as f32 + 1.0;
        let number = ch * (digits + 2.0);
        let time = if self.show_time { ch * 9.0 } else { px(0.0) };
        let marker = ch * 2.0;
        let text_left = number + time + marker;
        Metrics {
            ch,
            number,
            time,
            marker,
            text_left,
            text_width: (bounds.size.width - text_left - self.style.scrollbar.width).max(px(0.0)),
        }
    }

    /// Which columns to shape, so that a few pixels of scrolling reuse the
    /// shaped lines.
    fn shaped_window(&self) -> ShapedWindow {
        let ch = f64::from(self.metrics.ch).max(1.0);
        let scroll_col = (self.scroll.state.x.offset / ch) as usize;
        let first = scroll_col / COLUMN_STEP * COLUMN_STEP;
        let visible = (f64::from(self.metrics.text_width) / ch).ceil() as usize;
        ShapedWindow {
            first,
            columns: (scroll_col - first + visible + 1).div_ceil(COLUMN_STEP) * COLUMN_STEP,
        }
    }

    /// Resolves and shapes the rows in view.
    fn layout_rows(&mut self, buffer: &LogBuffer, rows: u64, window: &Window) {
        self.rows.clear();
        self.highlights.clear();
        self.cache.begin_frame();
        let (lh, lead) = (self.line_height(), Self::lead(buffer));
        let top = self.scroll.state.y.offset;
        let first_row = (top / lh).floor() as u64;
        let last_row = (((top + self.scroll.state.y.viewport) / lh).ceil() as u64).min(rows);
        let shaped = self.shaped_window();
        let font_size = self.style.font_size;
        for digit in 0..10u8 {
            let color = self.style.gutter_text;
            let fonts = &self.fonts;
            self.cache
                .ensure(LineKey::Digit(digit), 1, font_size, window, |text, runs| {
                    text.push(char::from(b'0' + digit));
                    runs.push(plain_run(1, color, fonts));
                    false
                });
        }
        let matches = self.visible_matches(buffer, first_row, last_row, lead);
        let mut next_match = 0;
        for row in first_row..last_row {
            let y = px((row as f64 * lh - top) as f32);
            if row < lead {
                let count = buffer.dropped();
                let (color, fonts) = (self.style.notice_text, &self.fonts);
                self.cache.ensure(
                    LineKey::Dropped(count),
                    1,
                    font_size,
                    window,
                    |text, runs| {
                        text.push_str(&dropped_notice(count));
                        runs.push(plain_run(text.len(), color, fonts));
                        false
                    },
                );
                self.rows.push(Row::Dropped { top: y, count });
                continue;
            }
            let id = self.projection.line_at(row - lead);
            let Some(line) = buffer.line(id) else {
                continue;
            };
            let header = (line.kind == LineKind::GroupStart).then(|| self.collapsed.contains(&id));
            let hidden = match (header, buffer.group_at(id)) {
                (Some(true), Some(group)) => {
                    let body = group.body(buffer.total());
                    (body.end - body.start) as u32
                }
                _ => 0,
            };
            let hint = (hidden > 0).then(|| {
                let noun = if hidden == 1 { "line" } else { "lines" };
                format!(" ({hidden} {noun})")
            });
            let base = match line.kind {
                LineKind::Text => self.style.foreground,
                LineKind::GroupStart => self.style.group_text,
                LineKind::Error => self.style.error_text,
                LineKind::Warning => self.style.warning_text,
            };
            let key = LineKey::Text {
                line: id,
                first_col: shaped.first as u32,
                hint: hidden,
            };
            let source = LineText {
                line: &line,
                buffer,
                base,
                hint: hint.as_deref(),
            };
            let (style, fonts) = (&self.style, &self.fonts);
            self.cache
                .ensure(key, shaped.columns, font_size, window, |text, runs| {
                    push_line(
                        &source,
                        style,
                        fonts,
                        shaped.first,
                        shaped.columns,
                        text,
                        runs,
                    )
                });
            if self.show_time
                && let Some(time) = line.time_ms
            {
                let color = self.style.time_text;
                let fonts = &self.fonts;
                self.cache
                    .ensure(LineKey::Time(id), 8, font_size, window, |text, runs| {
                        text.push_str(&crate::timestamp::clock(time));
                        runs.push(plain_run(text.len(), color, fonts));
                        false
                    });
            }
            while matches.get(next_match).is_some_and(|m| m.line < id) {
                next_match += 1;
            }
            let tab = self.style.tab_width.max(1) as usize;
            for found in matches[next_match..].iter().take_while(|m| m.line == id) {
                self.highlights.push(Highlight {
                    top: y,
                    from: column_of(line.text, tab, found.start as usize),
                    to: column_of(line.text, tab, found.end as usize),
                    current: self.search.current == Some(*found),
                });
            }
            self.rows.push(Row::Line(LineRow {
                top: y,
                id,
                header,
                has_time: line.time_ms.is_some(),
                text: key,
                selected: self.selection.is_some_and(|s| s.lines().contains(&id)),
            }));
        }
        self.cache.end_frame();
        self.stats = super::FrameStats {
            rows: self.rows.len(),
            shaped: self.cache.shaped_this_frame(),
        };
    }

    /// The search matches on the lines between two rows.
    fn visible_matches(
        &self,
        buffer: &LogBuffer,
        first_row: u64,
        last_row: u64,
        lead: u64,
    ) -> Vec<Match> {
        let Some(search) = &self.search.active else {
            return Vec::new();
        };
        if last_row <= first_row || last_row <= lead {
            return Vec::new();
        }
        let first = self.projection.line_at(first_row.saturating_sub(lead));
        let last = self.projection.line_at(last_row - 1 - lead);
        let _ = buffer;
        search.with(|set| {
            let from = set.matches.partition_point(|m| m.line < first);
            let to = set.matches.partition_point(|m| m.line <= last);
            set.matches[from..to.min(from + MAX_VISIBLE_MATCHES).min(MAX_MATCHES)].to_vec()
        })
    }

    pub(super) fn paint(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        let (style, m) = (&self.style, self.metrics);
        let lh = style.line_height;
        window.paint_quad(fill(bounds, style.background));
        window.paint_quad(fill(
            Bounds::new(bounds.origin, size(m.text_left, bounds.size.height)),
            style.gutter_background,
        ));
        let row_bounds = |top: Pixels| {
            Bounds::new(
                point(bounds.left(), bounds.top() + top),
                size(bounds.size.width, lh),
            )
        };
        for row in &self.rows {
            let Row::Line(line) = row else { continue };
            if line.selected {
                window.paint_quad(fill(row_bounds(line.top), style.selection));
            } else if line.header.is_some() {
                window.paint_quad(fill(row_bounds(line.top), style.group_background));
            }
        }
        let text_bounds = Bounds::new(
            point(bounds.left() + m.text_left, bounds.top()),
            size(m.text_width, bounds.size.height),
        );
        let scroll_x = px(self.scroll.state.x.offset as f32);
        window.with_content_mask(
            Some(ContentMask {
                bounds: text_bounds,
            }),
            |window| {
                let left = bounds.left() + m.text_left - scroll_x;
                for h in &self.highlights {
                    let color = if h.current {
                        style.current_match_background
                    } else {
                        style.match_background
                    };
                    let x = left + m.ch * h.from as f32;
                    let width = m.ch * h.to.saturating_sub(h.from) as f32;
                    let at = Bounds::new(point(x, bounds.top() + h.top), size(width, lh));
                    window.paint_quad(fill(at, color));
                }
                for row in &self.rows {
                    let Row::Line(line) = row else { continue };
                    let first = match line.text {
                        LineKey::Text { first_col, .. } => first_col as usize,
                        _ => 0,
                    };
                    let origin = point(left + m.ch * first as f32, bounds.top() + line.top);
                    self.paint_shaped(line.text, origin, window, cx);
                }
            },
        );
        for row in &self.rows {
            match row {
                Row::Dropped { top, count } => {
                    let origin = point(bounds.left() + m.ch, bounds.top() + *top);
                    self.paint_shaped(LineKey::Dropped(*count), origin, window, cx);
                }
                Row::Line(line) => self.paint_gutter(line, bounds, window, cx),
            }
        }
        self.scroll.paint(window);
    }

    fn paint_shaped(&self, key: LineKey, origin: Point<Pixels>, window: &mut Window, cx: &mut App) {
        let Some((shaped, background)) = self.cache.get(key) else {
            return;
        };
        let lh = self.style.line_height;
        // A glyph that fails to paint only loses itself; keep painting.
        if background {
            let _ = shaped.paint_background(origin, lh, TextAlign::Left, None, window, cx);
        }
        let _ = shaped.paint(origin, lh, TextAlign::Left, None, window, cx);
    }

    /// The line number, time and fold marker of a row.
    fn paint_gutter(
        &self,
        row: &LineRow,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let m = self.metrics;
        let y = bounds.top() + row.top;
        let mut x = bounds.left() + m.number - m.ch;
        let mut rest = row.id + 1;
        loop {
            if let Some((shaped, _)) = self.cache.get(LineKey::Digit((rest % 10) as u8)) {
                x -= shaped.width();
                let _ = shaped.paint(
                    point(x, y),
                    self.style.line_height,
                    TextAlign::Left,
                    None,
                    window,
                    cx,
                );
            }
            rest /= 10;
            if rest == 0 {
                break;
            }
        }
        if row.has_time && self.show_time {
            let origin = point(bounds.left() + m.number, y);
            self.paint_shaped(LineKey::Time(row.id), origin, window, cx);
        }
        if let Some(collapsed) = row.header {
            let center = point(
                bounds.left() + m.number + m.time + m.marker / 2.0,
                y + self.style.line_height / 2.0,
            );
            paint_chevron(
                center,
                m.ch * 0.3,
                collapsed,
                self.style.gutter_text,
                window,
            );
        }
    }

    /// The view and the search bar and jump pill laid over it.
    pub(super) fn chrome(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> Div {
        let root = div()
            .size_full()
            .relative()
            .key_context("LogView")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.key_down(event, window, cx);
            }))
            .child(LogElement { view: cx.entity() });
        let root = if self.search.open {
            root.child(self.search_bar())
        } else {
            root
        };
        if self.follow {
            root
        } else {
            root.child(self.jump_pill(cx))
        }
    }

    fn search_bar(&self) -> impl IntoElement {
        let s = &self.style;
        let pad = s.line_height * 0.4;
        let input: SharedString = if self.search.input.is_empty() {
            "Find".into()
        } else {
            self.search.input.clone().into()
        };
        let input_color = if self.search.input.is_empty() {
            s.overlay_muted
        } else {
            s.overlay_text
        };
        div()
            .absolute()
            .top(s.line_height * 0.5)
            .right(s.scrollbar.hover_width + s.line_height * 0.5)
            .flex()
            .flex_row()
            .items_center()
            .gap(pad)
            .px(pad * 1.5)
            .py(pad)
            .rounded(s.radius)
            .border_1()
            .border_color(s.overlay_border)
            .bg(s.overlay_background)
            .text_color(s.overlay_text)
            .font_family(s.font_family.clone())
            .text_size(s.font_size)
            .line_height(s.line_height)
            .occlude()
            .child(
                div()
                    .flex()
                    .items_center()
                    .min_w(self.metrics.ch * 20.0)
                    .child(div().text_color(input_color).child(input))
                    .child(div().w(px(1.0)).h(s.line_height * 0.8).bg(s.overlay_text)),
            )
            .child(
                div()
                    .text_color(s.overlay_muted)
                    .child(SharedString::from(self.search_label())),
            )
    }

    /// "3 of 120", "120 found", "No results", with an ellipsis while the
    /// search is still reading.
    fn search_label(&self) -> String {
        let Some(search) = &self.search.active else {
            return String::new();
        };
        let current = self.search.current;
        let (total, running, position) = search.with(|set| {
            let position = current.map(|c| {
                set.matches
                    .partition_point(|m| (m.line, m.start) <= (c.line, c.start))
            });
            (set.total, set.running, position)
        });
        let more = if running { "\u{2026}" } else { "" };
        match (total, position) {
            (0, _) if !running => "No results".to_owned(),
            (_, Some(position)) => format!("{position} of {total}{more}"),
            _ => format!("{total} found{more}"),
        }
    }

    fn jump_pill(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let s = &self.style;
        let new = self.new_lines();
        let label: SharedString = if new > 0 {
            format!("Jump to latest ({new} new)").into()
        } else {
            "Jump to latest".into()
        };
        div()
            .absolute()
            .bottom(s.line_height * 1.5)
            .left_0()
            .right_0()
            .flex()
            .justify_center()
            .child(
                div()
                    .occlude()
                    .cursor_pointer()
                    .px(s.line_height * 0.8)
                    .py(s.line_height * 0.25)
                    .rounded(s.radius)
                    .bg(s.accent)
                    .text_color(s.accent_text)
                    .font_family(s.font_family.clone())
                    .text_size(s.font_size)
                    .line_height(s.line_height)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseDownEvent, _, cx| {
                            this.scroll_to_latest(cx);
                            cx.stop_propagation();
                        }),
                    )
                    .child(label),
            )
    }
}

/// The column a byte offset of `text` falls in, with tabs expanded.
fn column_of(text: &str, tab_width: usize, byte: usize) -> usize {
    text.char_indices()
        .take_while(|&(at, _)| at < byte)
        .fold(0, |column, (_, ch)| {
            if ch == '\t' {
                (column / tab_width + 1) * tab_width
            } else {
                column + 1
            }
        })
}

/// A small triangle: pointing right when collapsed, down when open.
fn paint_chevron(
    center: Point<Pixels>,
    half: Pixels,
    collapsed: bool,
    color: Hsla,
    window: &mut Window,
) {
    let at = |dx: f32, dy: f32| point(center.x + half * dx, center.y + half * dy);
    let corners = if collapsed {
        [at(-0.6, -1.0), at(0.8, 0.0), at(-0.6, 1.0)]
    } else {
        [at(-1.0, -0.6), at(1.0, -0.6), at(0.0, 0.8)]
    };
    let mut path = PathBuilder::fill();
    path.move_to(corners[0]);
    path.line_to(corners[1]);
    path.line_to(corners[2]);
    path.close();
    if let Ok(path) = path.build() {
        window.paint_path(path, color);
    }
}

/// Lays out and paints only the rows of a [`LogView`] that are in view.
pub struct LogElement {
    view: Entity<LogView>,
}

impl IntoElement for LogElement {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for LogElement {
    type RequestLayoutState = ();
    type PrepaintState = Hitbox;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.0).into();
        style.size.height = relative(1.0).into();
        style.flex_grow = 1.0;
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Hitbox {
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        self.view
            .update(cx, |view, cx| view.prepaint(bounds, window, cx));
        hitbox
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        hitbox: &mut Hitbox,
        window: &mut Window,
        cx: &mut App,
    ) {
        let view = self.view.clone();
        let hitbox_id = hitbox.id;
        // Text the user can select; the scrollbar sets its own cursor after.
        window.set_cursor_style(CursorStyle::IBeam, hitbox);
        scroll::install(&view, hitbox, |view| &mut view.scroll, window);
        window.on_mouse_event({
            let view = view.clone();
            move |event: &ScrollEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble || !hitbox_id.should_handle_scroll(window) {
                    return;
                }
                view.update(cx, |view, cx| {
                    let delta = event.delta.pixel_delta(view.style.line_height);
                    let (across, along) = if event.modifiers.shift {
                        (delta.y, delta.x)
                    } else {
                        (delta.x, delta.y)
                    };
                    view.scroll.state.x.by(-f64::from(across));
                    view.scroll_by(-f64::from(along));
                    cx.notify();
                });
                cx.stop_propagation();
            }
        });
        window.on_mouse_event({
            let view = view.clone();
            move |event: &MouseDownEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble
                    || event.button != MouseButton::Left
                    || !hitbox_id.is_hovered(window)
                {
                    return;
                }
                view.update(cx, |view, cx| {
                    view.press(event.position, event.modifiers.shift, window, cx);
                });
            }
        });
        window.on_mouse_event({
            let view = view.clone();
            move |event: &MouseMoveEvent, phase, _, cx| {
                if phase == DispatchPhase::Capture
                    && event.pressed_button == Some(MouseButton::Left)
                {
                    view.update(cx, |view, cx| view.drag_to(event.position, cx));
                }
            }
        });
        window.on_mouse_event({
            let view = view.clone();
            move |event: &MouseUpEvent, phase, _, cx| {
                if phase == DispatchPhase::Capture && event.button == MouseButton::Left {
                    view.update(cx, |view, _| view.dragging = false);
                }
            }
        });
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            self.view
                .update(cx, |view, cx| view.paint(bounds, window, cx));
        });
    }
}
