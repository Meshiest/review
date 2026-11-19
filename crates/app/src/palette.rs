//! The command palette (`Ctrl+Shift+P`) and quick open (`Ctrl+P`): a box at
//! the top of the window over a list the query filters by fuzzy match.
//! Up and Down move, Enter runs, Esc or a click outside closes.
//!
//! The shell builds the items when it opens one: every command of
//! [`crate::commands`] plus the open pull request's and the focused panel's,
//! or the pull requests and files to go to. Items run last come first while
//! the query is empty, and win ties after it. A disabled item stays in the
//! list with the reason it cannot run, and Enter on it does nothing.

pub mod fuzzy;

use std::ops::Range;
use std::rc::Rc;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, FontWeight, Global,
    HighlightStyle, InteractiveElement as _, IntoElement, MouseButton, ParentElement, Render,
    ScrollHandle, SharedString, StatefulInteractiveElement as _, Styled, StyledText, Subscription,
    Window, div, px,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Enter, Escape, Input, InputEvent, InputState, MoveDown, MoveUp};
use gpui_kit::component::{Icon, h_flex, v_flex};

use crate::ui::{self, TextExt as _};

/// The key context of the palette's box.
pub const KEY_CONTEXT: &str = "Palette";
/// How many items run last are remembered.
const RECENT_KEPT: usize = 32;
/// The most rows drawn; a query narrows the rest.
const MAX_ROWS: usize = 120;
/// Gap between the window's top and the box.
const TOP: f32 = 56.0;
const WIDTH: f32 = 600.0;
const LIST_HEIGHT: f32 = 420.0;

/// What an item does when run, with focus back where it was.
pub type Run = Rc<dyn Fn(&mut Window, &mut App)>;

/// One row: a command, a pull request or a file.
#[derive(Clone)]
pub struct Item {
    /// Stable across openings, for the recent order.
    pub id: SharedString,
    pub title: SharedString,
    /// Where it belongs, on the right: a group, a pull request.
    pub detail: Option<SharedString>,
    /// Its shortcut, keys separated by two spaces.
    pub keys: Option<SharedString>,
    /// Why it cannot run now; `None` when it can.
    pub disabled: Option<SharedString>,
    run: Run,
}

impl Item {
    pub fn new(
        id: impl Into<SharedString>,
        title: impl Into<SharedString>,
        run: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            detail: None,
            keys: None,
            disabled: None,
            run: Rc::new(run),
        }
    }

    #[must_use]
    pub fn detail(mut self, detail: impl Into<SharedString>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    #[must_use]
    pub fn keys(mut self, keys: impl Into<SharedString>) -> Self {
        let keys = keys.into();
        self.keys = (!keys.is_empty()).then_some(keys);
        self
    }

    /// Disabled with `reason`, when there is one.
    #[must_use]
    pub fn disabled<S: Into<SharedString>>(mut self, reason: Option<S>) -> Self {
        self.disabled = reason.map(Into::into);
        self
    }
}

/// Which list the palette shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Every command, `Ctrl+Shift+P`.
    Commands,
    /// Pull requests and files, `Ctrl+P`.
    QuickOpen,
}

/// The ids of the items run last, newest first, while the app runs.
#[derive(Default)]
struct Recent(Vec<SharedString>);

impl Global for Recent {}

fn remember(id: &SharedString, cx: &mut App) {
    let recent = &mut cx.default_global::<Recent>().0;
    recent.retain(|held| held != id);
    recent.insert(0, id.clone());
    recent.truncate(RECENT_KEPT);
}

/// The order to show `items` in for `query`: matches only, best first,
/// with the matched byte offsets of each title. `recent` breaks ties, and
/// orders everything while the query is empty.
fn rank(items: &[Item], query: &str, recent: &[SharedString]) -> Vec<(usize, Vec<usize>)> {
    let age = |item: &Item| {
        recent
            .iter()
            .position(|id| *id == item.id)
            .unwrap_or(usize::MAX)
    };
    let mut ranked: Vec<(i32, usize, usize, Vec<usize>)> = items
        .iter()
        .enumerate()
        .filter_map(|(ix, item)| {
            let found = fuzzy::fuzzy(query, &item.title)?;
            Some((found.score, age(item), ix, found.positions))
        })
        .collect();
    ranked.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    ranked
        .into_iter()
        .map(|(_, _, ix, positions)| (ix, positions))
        .collect()
}

/// The palette closed: run or dismissed.
pub struct Closed;

pub struct Palette {
    mode: Mode,
    input: Entity<InputState>,
    items: Vec<Item>,
    /// The matching items, best first, with the matched offsets.
    shown: Vec<(usize, Vec<usize>)>,
    selected: usize,
    /// Where focus goes back when the palette closes.
    previous: Option<FocusHandle>,
    scroll: ScrollHandle,
    _input: Subscription,
}

impl EventEmitter<Closed> for Palette {}

impl Palette {
    pub fn new(
        mode: Mode,
        items: Vec<Item>,
        previous: Option<FocusHandle>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let placeholder = match mode {
            Mode::Commands => "Run a command",
            Mode::QuickOpen => "Go to a pull request or a file",
        };
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        let typed = cx.subscribe(&input, |this: &mut Self, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.refilter(cx);
            }
        });
        input.update(cx, |input, cx| input.focus(window, cx));
        let mut palette = Self {
            mode,
            input,
            items,
            shown: Vec::new(),
            selected: 0,
            previous,
            scroll: ScrollHandle::new(),
            _input: typed,
        };
        palette.refilter(cx);
        palette
    }

    #[must_use]
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The rows in the order shown: each title and, for a disabled one, why.
    #[must_use]
    pub fn rows(&self) -> Vec<(SharedString, Option<SharedString>)> {
        self.shown
            .iter()
            .map(|(ix, _)| {
                let item = &self.items[*ix];
                (item.title.clone(), item.disabled.clone())
            })
            .collect()
    }

    /// The selected row's index in [`Self::rows`].
    #[must_use]
    pub fn selected(&self) -> usize {
        self.selected
    }

    fn refilter(&mut self, cx: &mut Context<Self>) {
        let query = self.input.read(cx).value();
        let recent = cx.try_global::<Recent>().map_or(&[][..], |r| &r.0);
        self.shown = rank(&self.items, &query, recent);
        self.selected = 0;
        self.scroll.scroll_to_item(0);
        cx.notify();
    }

    fn step(&mut self, by: isize, cx: &mut Context<Self>) {
        let count = self.shown.len().min(MAX_ROWS);
        if count == 0 {
            return;
        }
        self.selected = self.selected.saturating_add_signed(by).min(count - 1);
        self.scroll.scroll_to_item(self.selected);
        cx.notify();
    }

    /// Closes the palette; focus goes back where it was.
    pub fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(previous) = self.previous.take() {
            window.focus(&previous, cx);
        }
        cx.emit(Closed);
    }

    /// Runs the row at `row` unless it is disabled: the palette closes,
    /// focus goes back, then the item runs there.
    fn run(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.shown.get(row).map(|(ix, _)| self.items[*ix].clone()) else {
            return;
        };
        if item.disabled.is_some() {
            self.selected = row;
            cx.notify();
            return;
        }
        remember(&item.id, cx);
        self.dismiss(window, cx);
        (item.run)(window, cx);
    }

    fn row(&self, row: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let t = ui::theme(cx);
        let c = &t.colors;
        let (ix, positions) = &self.shown[row];
        let item = &self.items[*ix];
        let disabled = item.disabled.is_some();
        let selected = row == self.selected;
        let highlight = HighlightStyle {
            color: Some(if disabled { c.text_muted } else { c.accent_fg }),
            font_weight: Some(FontWeight::SEMIBOLD),
            ..HighlightStyle::default()
        };
        let title = StyledText::new(item.title.clone()).with_highlights(
            spans(&item.title, positions)
                .into_iter()
                .map(|range| (range, highlight)),
        );
        let keycaps = item.keys.iter().flat_map(|keys| {
            keys.split("  ")
                .map(|key| keycap(&t, key))
                .collect::<Vec<_>>()
        });
        let hover = c.hover;
        h_flex()
            .id(("palette-row", row))
            .debug_selector(move || format!("palette-row-{row}"))
            .min_h(px(30.0))
            .px(px(10.0))
            .py(px(4.0))
            .gap(px(10.0))
            .items_center()
            .rounded(t.radius.sm)
            .when(selected, |el| el.bg(c.selected))
            .when(!disabled, |el| {
                el.cursor_pointer()
                    .when(!selected, |el| el.hover(move |s| s.bg(hover)))
            })
            .on_click(cx.listener(move |this, _, window, cx| this.run(row, window, cx)))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .truncate()
                            .text_color(if disabled { c.text_faint } else { c.text })
                            .child(title),
                    )
                    .children(item.disabled.clone().map(|why| {
                        div()
                            .tiny(&t)
                            .truncate()
                            .text_color(c.text_muted)
                            .child(why)
                    })),
            )
            .children(item.detail.clone().map(|detail| {
                div()
                    .flex_none()
                    .max_w(px(200.0))
                    .truncate()
                    .small(&t)
                    .text_color(c.text_muted)
                    .child(detail)
            }))
            .child(h_flex().flex_none().gap(px(4.0)).children(keycaps))
            .into_any_element()
    }
}

/// The byte ranges of `text` the matched characters at `positions` cover,
/// runs merged.
fn spans(text: &str, positions: &[usize]) -> Vec<Range<usize>> {
    let mut spans: Vec<Range<usize>> = Vec::new();
    for &at in positions {
        let end = at + text[at..].chars().next().map_or(0, char::len_utf8);
        match spans.last_mut() {
            Some(last) if last.end == at => last.end = end,
            _ => spans.push(at..end),
        }
    }
    spans
}

fn keycap(t: &theme::Theme, key: &str) -> gpui::Div {
    let c = &t.colors;
    div()
        .mono(t)
        .tiny(t)
        .px(px(5.0))
        .h(px(18.0))
        .flex()
        .items_center()
        .rounded(t.radius.sm)
        .border_1()
        .border_color(c.border_strong)
        .bg(c.field)
        .text_color(c.text_secondary)
        .child(key.to_owned())
}

impl Render for Palette {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = ui::theme(cx);
        let c = &t.colors;
        let rows: Vec<_> = (0..self.shown.len().min(MAX_ROWS))
            .map(|row| self.row(row, cx))
            .collect();
        let empty = rows.is_empty().then(|| {
            div()
                .px(px(10.0))
                .py(px(8.0))
                .text_color(c.text_muted)
                .child(match self.mode {
                    Mode::Commands => "No command matches",
                    Mode::QuickOpen => "No pull request or file matches",
                })
        });
        let hint = match self.mode {
            Mode::Commands => "Up and Down move, Enter runs, Esc closes",
            Mode::QuickOpen => "Up and Down move, Enter opens, Esc closes",
        };
        div()
            .id("palette-backdrop")
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .flex_col()
            .items_center()
            .bg(c.background.opacity(0.35))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.dismiss(window, cx)),
            )
            .child(
                v_flex()
                    .id("palette")
                    .debug_selector(|| "palette".into())
                    .key_context(KEY_CONTEXT)
                    .capture_action(cx.listener(|this, _: &MoveUp, _, cx| {
                        this.step(-1, cx);
                        cx.stop_propagation();
                    }))
                    .capture_action(cx.listener(|this, _: &MoveDown, _, cx| {
                        this.step(1, cx);
                        cx.stop_propagation();
                    }))
                    .capture_action(cx.listener(|this, _: &Enter, window, cx| {
                        cx.stop_propagation();
                        this.run(this.selected, window, cx);
                    }))
                    .capture_action(cx.listener(|this, _: &Escape, window, cx| {
                        cx.stop_propagation();
                        this.dismiss(window, cx);
                    }))
                    .occlude()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .mt(px(TOP))
                    .w(px(WIDTH))
                    .max_w_full()
                    .p(px(6.0))
                    .gap(px(6.0))
                    .rounded(t.radius.lg)
                    .border_1()
                    .border_color(c.border_strong)
                    .bg(c.surface_menu)
                    .shadow_lg()
                    .child(
                        Input::new(&self.input).prefix(Icon::new(IconName::Search).size(px(14.0))),
                    )
                    .child(
                        v_flex()
                            .id("palette-list")
                            .max_h(px(LIST_HEIGHT))
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll)
                            .children(rows)
                            .children(empty),
                    )
                    .child(
                        div()
                            .px(px(10.0))
                            .tiny(&t)
                            .text_color(c.text_muted)
                            .child(hint),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(titles: &[&str]) -> Vec<Item> {
        titles
            .iter()
            .map(|title| Item::new(*title, *title, |_, _| {}))
            .collect()
    }

    fn order(items: &[Item], query: &str, recent: &[&str]) -> Vec<String> {
        let recent: Vec<SharedString> = recent.iter().map(|id| (*id).into()).collect();
        rank(items, query, &recent)
            .into_iter()
            .map(|(ix, _)| items[ix].title.to_string())
            .collect()
    }

    #[test]
    fn an_empty_query_lists_recent_items_first_then_the_rest_in_order() {
        let items = items(&[
            "Next file",
            "Close the tab",
            "Open settings",
            "New terminal",
        ]);
        assert_eq!(
            order(&items, "", &["New terminal", "Close the tab"]),
            [
                "New terminal",
                "Close the tab",
                "Next file",
                "Open settings"
            ]
        );
    }

    #[test]
    fn a_query_keeps_matches_only_best_first_with_recent_breaking_ties() {
        let items = items(&["Unified", "Next file", "Next hunk", "Close the tab"]);
        assert_eq!(order(&items, "nf", &[]), ["Next file", "Unified"]);
        assert_eq!(order(&items, "next", &[]), ["Next file", "Next hunk"]);
        assert_eq!(
            order(&items, "next", &["Next hunk"]),
            ["Next hunk", "Next file"]
        );
    }

    #[test]
    fn matched_characters_merge_into_spans() {
        assert_eq!(spans("Next file", &[0, 1, 5]), vec![0..2, 5..6]);
        assert_eq!(spans("aé b", &[1, 3]), vec![1..4]);
    }
}
