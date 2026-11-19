//! The log view: state, scrolling, groups, search and selection. The element
//! that lays out and paints its rows is in [`paint`].

mod keys;
mod paint;

use std::collections::BTreeSet;
use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    App, Bounds, ClipboardItem, Context, EventEmitter, FocusHandle, Focusable, IntoElement, Pixels,
    Point, Render, Task, Window, px,
};
use scroll::Scroller;

use crate::buffer::{LineKind, LogBuffer};
use crate::feed::{LogFeed, Shared};
use crate::projection::Projection;
use crate::search::{Match, Search};
use crate::shape::{Fonts, LineCache, LineKey};
use crate::style::LogStyle;
use crate::timestamp;

/// Longest the view asks for frames when lines arrive: at most one wake-up
/// per display frame.
const WAKE_INTERVAL: Duration = Duration::from_millis(16);

/// How far from the end still counts as at the end, in px.
const AT_BOTTOM: f64 = 1.0;

/// What a view tells its owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogEvent {
    /// Lines with these ids were appended to the log.
    Appended(Range<u64>),
}

/// What the last frame did, for benchmarks.
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameStats {
    /// Rows laid out and painted.
    pub rows: usize,
    /// Lines shaped because they were not in the cache.
    pub shaped: usize,
}

/// Selected lines, from where the press landed to where the pointer is now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Selection {
    anchor: u64,
    head: u64,
}

impl Selection {
    fn lines(self) -> Range<u64> {
        self.anchor.min(self.head)..self.anchor.max(self.head) + 1
    }
}

/// The line at the top of the view and how far its top is above the view's.
#[derive(Clone, Copy)]
struct Anchor {
    line: u64,
    above: f64,
}

#[derive(Clone, Copy, Default)]
struct Seen {
    version: u64,
    total: u64,
    dropped: u64,
}

#[derive(Default)]
struct SearchUi {
    open: bool,
    input: String,
    active: Option<Search>,
    current: Option<Match>,
    /// Go to the first match as soon as one is found.
    jump: bool,
}

/// Column widths for the current font and the lines held.
#[derive(Clone, Copy, Default)]
struct Metrics {
    ch: Pixels,
    number: Pixels,
    time: Pixels,
    marker: Pixels,
    /// Where text starts, left of which is the gutter.
    text_left: Pixels,
    /// The text area's width, less the scrollbar.
    text_width: Pixels,
}

/// A row in view.
enum Row {
    Dropped { top: Pixels, count: u64 },
    Line(LineRow),
}

struct LineRow {
    top: Pixels,
    id: u64,
    /// For a group header, whether the group is collapsed.
    header: Option<bool>,
    has_time: bool,
    text: LineKey,
    selected: bool,
}

/// A search match to paint, in columns of its row.
struct Highlight {
    top: Pixels,
    from: usize,
    to: usize,
    current: bool,
}

/// A virtualized, tailing view of a [`LogFeed`].
///
/// Render it as a child; it fills its parent. It paints only the rows in
/// view from a cache of shaped lines, scrolls with the wheel, a scrollbar and
/// middle-click autoscroll, and follows the end of the log until scrolled up.
pub struct LogView {
    shared: Arc<Shared>,
    style: LogStyle,
    fonts: Fonts,
    scroll: Scroller,
    focus: FocusHandle,
    /// Keeps the wake-up listener alive.
    listener: Option<Task<()>>,
    cache: LineCache,
    ch: Option<Pixels>,
    metrics: Metrics,
    bounds: Bounds<Pixels>,
    follow: bool,
    /// The log's length when follow mode was left, for "N new".
    unfollowed_at: u64,
    seen: Seen,
    collapsed: BTreeSet<u64>,
    projection: Projection,
    /// Collapsed groups or the lines changed since the projection was built.
    dirty: bool,
    anchor: Option<Anchor>,
    show_time: bool,
    selection: Option<Selection>,
    dragging: bool,
    search: SearchUi,
    rows: Vec<Row>,
    highlights: Vec<Highlight>,
    stats: FrameStats,
}

impl EventEmitter<LogEvent> for LogView {}

impl Focusable for LogView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl LogView {
    #[must_use]
    pub fn new(feed: &LogFeed, style: LogStyle, cx: &mut Context<Self>) -> Self {
        let mut view = Self {
            shared: feed.shared(),
            fonts: Fonts::new(&style),
            scroll: Scroller::new(style.scroll_style()),
            style,
            focus: cx.focus_handle(),
            listener: None,
            cache: LineCache::default(),
            ch: None,
            metrics: Metrics::default(),
            bounds: Bounds::default(),
            follow: true,
            unfollowed_at: 0,
            seen: Seen::default(),
            collapsed: BTreeSet::new(),
            projection: Projection::default(),
            dirty: true,
            anchor: None,
            show_time: false,
            selection: None,
            dragging: false,
            search: SearchUi::default(),
            rows: Vec::new(),
            highlights: Vec::new(),
            stats: FrameStats::default(),
        };
        view.listen(cx);
        view
    }

    /// Wakes the view when lines or search results arrive, at most once per
    /// frame; everything else is read at paint time.
    fn listen(&mut self, cx: &mut Context<Self>) {
        let wake = self.shared.wake_receiver();
        self.listener = Some(cx.spawn(async move |this, cx| {
            while wake.recv().await.is_ok() {
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
                cx.background_executor().timer(WAKE_INTERVAL).await;
            }
        }));
    }

    /// Replaces colors and font. Shaped lines are shaped again on the next
    /// frame; the scroll position, groups, search and selection are kept.
    pub fn set_style(&mut self, style: LogStyle, cx: &mut Context<Self>) {
        if style == self.style {
            return;
        }
        if style.line_height != self.style.line_height {
            self.dirty = true;
        }
        self.fonts = Fonts::new(&style);
        self.scroll.set_style(style.scroll_style());
        self.style = style;
        self.ch = None;
        self.cache.clear();
        cx.notify();
    }

    #[must_use]
    pub fn style(&self) -> &LogStyle {
        &self.style
    }

    /// What the last frame did.
    #[must_use]
    pub fn last_frame(&self) -> FrameStats {
        self.stats
    }

    // Scrolling and following.

    #[must_use]
    pub fn scroll_y(&self) -> f64 {
        self.scroll.state.y.offset
    }

    /// The largest scroll offset for the viewport and rows of the last frame.
    #[must_use]
    pub fn max_scroll_y(&self) -> f64 {
        self.scroll.state.y.max()
    }

    /// Sets the vertical offset, clamped; leaves follow mode unless it lands
    /// at the end. Returns whether it changed.
    pub fn set_scroll_y(&mut self, y: f64) -> bool {
        let changed = self.scroll.state.y.set(y);
        self.after_user_scroll();
        changed
    }

    pub fn scroll_by(&mut self, dy: f64) -> bool {
        self.set_scroll_y(self.scroll_y() + dy)
    }

    /// True while middle-click autoscroll is running.
    #[must_use]
    pub fn is_autoscrolling(&self) -> bool {
        self.scroll.is_autoscrolling()
    }

    /// True while the view keeps the end of the log in view.
    #[must_use]
    pub fn is_following(&self) -> bool {
        self.follow
    }

    /// Lines appended since follow mode was left; 0 while following.
    #[must_use]
    pub fn new_lines(&self) -> u64 {
        if self.follow {
            return 0;
        }
        self.seen.total.saturating_sub(self.unfollowed_at)
    }

    /// Goes to the end of the log and follows it.
    pub fn scroll_to_latest(&mut self, cx: &mut Context<Self>) {
        self.follow = true;
        cx.notify();
    }

    fn at_bottom(&self) -> bool {
        let y = &self.scroll.state.y;
        y.offset >= y.max() - AT_BOTTOM
    }

    /// Follow mode is exactly "at the end", whichever way it got there.
    fn after_user_scroll(&mut self) {
        let at_bottom = self.at_bottom();
        if self.follow && !at_bottom {
            self.unfollowed_at = self.seen.total;
        }
        self.follow = at_bottom;
    }

    fn line_height(&self) -> f64 {
        f64::from(self.style.line_height)
    }

    // Time column.

    #[must_use]
    pub fn shows_time(&self) -> bool {
        self.show_time
    }

    pub fn set_show_time(&mut self, show: bool, cx: &mut Context<Self>) {
        if show != self.show_time {
            self.show_time = show;
            cx.notify();
        }
    }

    // Groups.

    /// Collapses or expands the group whose header is line `header`.
    pub fn set_group_collapsed(&mut self, header: u64, collapsed: bool, cx: &mut Context<Self>) {
        let changed = if collapsed {
            self.collapsed.insert(header)
        } else {
            self.collapsed.remove(&header)
        };
        if changed {
            self.dirty = true;
            cx.notify();
        }
    }

    pub fn toggle_group(&mut self, header: u64, cx: &mut Context<Self>) {
        let collapsed = !self.collapsed.contains(&header);
        self.set_group_collapsed(header, collapsed, cx);
    }

    #[must_use]
    pub fn is_group_collapsed(&self, header: u64) -> bool {
        self.collapsed.contains(&header)
    }

    pub fn collapse_all(&mut self, cx: &mut Context<Self>) {
        let headers: Vec<u64> = self
            .shared
            .read()
            .groups()
            .iter()
            .map(|g| g.start)
            .collect();
        self.collapsed.extend(headers);
        self.dirty = true;
        cx.notify();
    }

    pub fn expand_all(&mut self, cx: &mut Context<Self>) {
        self.collapsed.clear();
        self.dirty = true;
        cx.notify();
    }

    fn rebuild_projection(&mut self, buffer: &LogBuffer) {
        self.projection = project(buffer, &self.collapsed);
        self.dirty = false;
    }

    /// Rows before the first line: 1 while a "dropped" notice is shown.
    fn lead(buffer: &LogBuffer) -> u64 {
        u64::from(buffer.dropped() > 0)
    }

    // Search.

    /// Opens the search bar and takes the keyboard.
    pub fn open_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search.open = true;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    pub fn close_search(&mut self, cx: &mut Context<Self>) {
        self.search = SearchUi::default();
        cx.notify();
    }

    #[must_use]
    pub fn is_search_open(&self) -> bool {
        self.search.open
    }

    /// Searches for `text` (smart case) and goes to the first match at or
    /// after the top of the view once one is found. Empty text ends the
    /// search.
    pub fn set_search(&mut self, text: &str, cx: &mut Context<Self>) {
        self.search.open = true;
        text.clone_into(&mut self.search.input);
        self.restart_search();
        cx.notify();
    }

    fn restart_search(&mut self) {
        self.search.current = None;
        self.search.jump = true;
        self.search.active = Search::new(&self.search.input);
        if let Some(search) = &self.search.active {
            let total = self.shared.read().total();
            search.extend(&self.shared, total);
        }
    }

    /// Steps to the next or previous match, wrapping around.
    pub fn step_match(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(search) = &self.search.active else {
            return;
        };
        let top = self.top_line();
        let current = self.search.current;
        let Some(found) = search.with(|set| pick(&set.matches, current, top, forward)) else {
            return;
        };
        self.search.current = Some(found);
        self.reveal(found.line, cx);
    }

    /// Matches found so far.
    #[must_use]
    pub fn match_count(&self) -> usize {
        self.search
            .active
            .as_ref()
            .map_or(0, |s| s.with(|set| set.total))
    }

    /// True while a search thread is still reading the log.
    #[must_use]
    pub fn is_searching(&self) -> bool {
        self.search
            .active
            .as_ref()
            .is_some_and(|s| s.with(|set| set.running))
    }

    /// The line at the top of the view.
    fn top_line(&self) -> u64 {
        let row = (self.scroll_y() / self.line_height()) as u64;
        let lead = Self::lead(&self.shared.read());
        self.projection.line_at(row.saturating_sub(lead))
    }

    /// Scrolls `line` into view, opening the group that hides it.
    fn reveal(&mut self, line: u64, cx: &mut Context<Self>) {
        let shared = Arc::clone(&self.shared);
        let buffer = shared.read();
        if let Some(group) = buffer.group_containing(line)
            && group.start != line
            && self.collapsed.remove(&group.start)
        {
            self.rebuild_projection(&buffer);
        }
        let row = self.projection.row_of(line) + Self::lead(&buffer);
        let lh = self.line_height();
        let (top, viewport) = (row as f64 * lh, self.scroll.state.y.viewport);
        let offset = self.scroll_y();
        if top < offset || top + lh > offset + viewport {
            self.set_scroll_y((top - (viewport - lh) / 2.0).max(0.0));
        }
        cx.notify();
    }

    // Selection and copy.

    /// The text of the selected lines that are shown, one per line, with the
    /// time column when it is on.
    #[must_use]
    pub fn selected_text(&self) -> Option<String> {
        let selection = self.selection?;
        let buffer = self.shared.read();
        let mut out = String::new();
        for line in buffer.lines(selection.lines()) {
            if self.projection.is_hidden(line.id) {
                continue;
            }
            if !out.is_empty() {
                out.push('\n');
            }
            if self.show_time
                && let Some(time) = line.time_ms
            {
                out.push_str(&timestamp::clock(time));
                out.push(' ');
            }
            out.push_str(line.text);
        }
        Some(out)
    }

    pub fn copy_selection(&self, cx: &mut Context<Self>) {
        if let Some(text) = self.selected_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    pub fn select_all(&mut self, cx: &mut Context<Self>) {
        let buffer = self.shared.read();
        self.selection = (!buffer.is_empty()).then(|| Selection {
            anchor: buffer.dropped(),
            head: buffer.total() - 1,
        });
        drop(buffer);
        cx.notify();
    }

    pub fn clear_selection(&mut self, cx: &mut Context<Self>) {
        if self.selection.take().is_some() {
            cx.notify();
        }
    }

    /// The selected line ids, if any.
    #[must_use]
    pub fn selection(&self) -> Option<Range<u64>> {
        self.selection.map(Selection::lines)
    }

    // Per-frame work.

    /// Reads what changed in the log: events, the projection, search.
    fn sync(&mut self, buffer: &LogBuffer, cx: &mut Context<Self>) {
        let total = buffer.total();
        if buffer.version() != self.seen.version {
            let appended = self.seen.total..total;
            if buffer.dropped() != self.seen.dropped
                && let Some(search) = &self.search.active
            {
                let oldest = buffer.dropped();
                search.with(|set| set.matches.retain(|m| m.line >= oldest));
            }
            self.seen = Seen {
                version: buffer.version(),
                total,
                dropped: buffer.dropped(),
            };
            self.dirty = true;
            if !appended.is_empty() {
                cx.emit(LogEvent::Appended(appended));
            }
        }
        let reanchor = self.dirty;
        if self.dirty {
            self.rebuild_projection(buffer);
        }
        if let Some(search) = &self.search.active {
            search.extend(&self.shared, total);
        }
        if reanchor && !self.follow {
            self.restore_anchor(buffer);
        }
    }

    /// Puts the line that was at the top back there after the rows moved.
    fn restore_anchor(&mut self, buffer: &LogBuffer) {
        let Some(anchor) = self.anchor else { return };
        let rows = Self::lead(buffer) + self.projection.rows();
        self.scroll.state.y.content = rows as f64 * self.line_height();
        let row = self.projection.row_of(anchor.line.max(buffer.dropped())) + Self::lead(buffer);
        self.scroll
            .state
            .y
            .set(row as f64 * self.line_height() - anchor.above);
    }

    fn remember_anchor(&mut self, buffer: &LogBuffer) {
        let lh = self.line_height();
        let row = (self.scroll_y() / lh) as u64;
        let lead = Self::lead(buffer);
        self.anchor = row.checked_sub(lead).map(|row| Anchor {
            line: self.projection.line_at(row),
            above: row as f64 * lh + lead as f64 * lh - self.scroll_y(),
        });
    }

    fn char_width(&mut self, window: &Window) -> Pixels {
        *self.ch.get_or_insert_with(|| {
            let text_system = window.text_system();
            let font_id = text_system.resolve_font(self.fonts.regular());
            text_system
                .ch_advance(font_id, self.style.font_size)
                .unwrap_or(self.style.font_size * 0.6)
        })
    }

    /// The row at `y` (in the view) and the line on it, if it is a line.
    fn line_at_y(&self, y: Pixels) -> Option<u64> {
        let lh = self.line_height();
        let row = ((f64::from(y) + self.scroll_y()) / lh).floor();
        if row < 0.0 {
            return None;
        }
        let buffer = self.shared.read();
        let row = (row as u64).checked_sub(Self::lead(&buffer))?;
        (row < self.projection.rows()).then(|| self.projection.line_at(row))
    }

    /// Handles a left press at `position`: a group header toggles, anything
    /// else starts or extends a selection.
    fn press(
        &mut self,
        position: Point<Pixels>,
        extend: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus, cx);
        let Some(id) = self.line_at_y(position.y - self.bounds.origin.y) else {
            self.clear_selection(cx);
            return;
        };
        let header = {
            let buffer = self.shared.read();
            buffer
                .line(id)
                .is_some_and(|line| line.kind == LineKind::GroupStart)
        };
        if header {
            self.toggle_group(id, cx);
            return;
        }
        self.selection = Some(match self.selection {
            Some(selection) if extend => Selection {
                head: id,
                ..selection
            },
            _ => Selection {
                anchor: id,
                head: id,
            },
        });
        self.dragging = true;
        cx.notify();
    }

    fn drag_to(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        if !self.dragging {
            return;
        }
        let y = (position.y - self.bounds.origin.y).clamp(px(0.0), self.bounds.size.height);
        if let Some(id) = self.line_at_y(y)
            && let Some(selection) = &mut self.selection
            && selection.head != id
        {
            selection.head = id;
            cx.notify();
        }
    }
}

impl Render for LogView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.chrome(window, cx)
    }
}

/// The lines of `buffer` less the bodies of the groups headed by `collapsed`.
fn project(buffer: &LogBuffer, collapsed: &BTreeSet<u64>) -> Projection {
    let hidden = collapsed
        .iter()
        .filter_map(|&header| buffer.group_at(header))
        .map(|group| group.body(buffer.total()));
    Projection::new(buffer.dropped(), buffer.total(), hidden)
}

/// The match to go to from `current` (or from the top line when there is
/// none), among `matches` in order; wraps around.
fn pick(matches: &[Match], current: Option<Match>, top: u64, forward: bool) -> Option<Match> {
    let last = matches.len().checked_sub(1)?;
    let key = |m: &Match| (m.line, m.start);
    let index = match (current, forward) {
        (Some(cur), true) => matches.partition_point(|m| key(m) <= key(&cur)) % (last + 1),
        (Some(cur), false) => matches
            .partition_point(|m| key(m) < key(&cur))
            .checked_sub(1)
            .unwrap_or(last),
        (None, true) => matches.partition_point(|m| m.line < top) % (last + 1),
        (None, false) => matches
            .partition_point(|m| m.line < top)
            .checked_sub(1)
            .unwrap_or(last),
    };
    matches.get(index).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(line: u64, start: u32) -> Match {
        Match {
            line,
            start,
            end: start + 1,
        }
    }

    fn sample() -> Vec<Match> {
        vec![at(2, 0), at(2, 5), at(7, 1), at(9, 0)]
    }

    #[test]
    fn next_goes_to_the_match_after_the_current_and_wraps() {
        let m = sample();
        assert_eq!(pick(&m, Some(at(2, 0)), 0, true), Some(at(2, 5)));
        assert_eq!(pick(&m, Some(at(2, 5)), 0, true), Some(at(7, 1)));
        assert_eq!(pick(&m, Some(at(9, 0)), 0, true), Some(at(2, 0)));
    }

    #[test]
    fn previous_goes_back_and_wraps() {
        let m = sample();
        assert_eq!(pick(&m, Some(at(7, 1)), 0, false), Some(at(2, 5)));
        assert_eq!(pick(&m, Some(at(2, 0)), 0, false), Some(at(9, 0)));
    }

    #[test]
    fn without_a_current_match_the_top_of_the_view_decides() {
        let m = sample();
        assert_eq!(pick(&m, None, 3, true), Some(at(7, 1)));
        assert_eq!(pick(&m, None, 7, true), Some(at(7, 1)));
        assert_eq!(pick(&m, None, 10, true), Some(at(2, 0)));
        assert_eq!(pick(&m, None, 3, false), Some(at(2, 5)));
        assert_eq!(pick(&m, None, 0, false), Some(at(9, 0)));
    }

    #[test]
    fn no_matches_means_no_step() {
        assert_eq!(pick(&[], None, 0, true), None);
        assert_eq!(pick(&[], Some(at(1, 0)), 0, false), None);
    }

    fn log_with_groups() -> LogBuffer {
        let mut buffer = LogBuffer::new();
        let text = [
            "a",
            "##[group]one",
            "b",
            "c",
            "##[endgroup]",
            "##[group]two",
            "d",
            "##[endgroup]",
            "e",
            "##[group]open",
            "f",
            "",
        ]
        .join("\n");
        buffer.append(&mut crate::LogParser::new(), text.as_bytes());
        buffer
    }

    fn shown(buffer: &LogBuffer, projection: &Projection) -> Vec<String> {
        (0..projection.rows())
            .filter_map(|row| buffer.line(projection.line_at(row)))
            .map(|line| line.text.to_owned())
            .collect()
    }

    #[test]
    fn a_collapsed_group_shows_its_header_and_hides_its_body() {
        let buffer = log_with_groups();
        let all = project(&buffer, &BTreeSet::new());
        assert_eq!(
            shown(&buffer, &all),
            ["a", "one", "b", "c", "two", "d", "e", "open", "f"]
        );
        let header = |title: &str| {
            buffer
                .groups()
                .iter()
                .find(|g| buffer.line(g.start).is_some_and(|l| l.text == title))
                .map(|g| g.start)
                .expect("group exists")
        };
        let folded = project(&buffer, &BTreeSet::from([header("one"), header("open")]));
        assert_eq!(
            shown(&buffer, &folded),
            ["a", "one", "two", "d", "e", "open"]
        );
    }

    #[test]
    fn a_collapsed_open_group_hides_lines_that_arrive_later() {
        let mut buffer = log_with_groups();
        let open = buffer.groups().last().map(|g| g.start).expect("a group");
        let collapsed = BTreeSet::from([open]);
        let before = project(&buffer, &collapsed).rows();
        buffer.append(&mut crate::LogParser::new(), b"g\nh\n");
        assert_eq!(project(&buffer, &collapsed).rows(), before);
    }

    #[test]
    fn a_selection_covers_both_ends_in_either_direction() {
        assert_eq!(Selection { anchor: 3, head: 7 }.lines(), 3..8);
        assert_eq!(Selection { anchor: 7, head: 3 }.lines(), 3..8);
        assert_eq!(Selection { anchor: 4, head: 4 }.lines(), 4..5);
    }
}
