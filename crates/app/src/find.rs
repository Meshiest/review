//! The find bar: a small box at the top right of the diff or the
//! conversation, opened with Ctrl+F. It holds the query and says how many
//! matches the panel found; the panel searches its own text, highlights the
//! matches and scrolls to the current one.

use diff_view::{Needle, Step};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, InteractiveElement as _, IntoElement,
    MouseButton, ParentElement as _, Render, SharedString, Styled as _, Subscription, Window,
    actions, div, px,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::h_flex;
use gpui_kit::component::input::{Enter, Escape, Input, InputEvent, InputState, Search};

use crate::ui::{self, TextExt as _};

actions!(
    find,
    [
        /// `Ctrl+F`: opens the find bar of the focused diff or conversation,
        /// or selects its query again.
        OpenFind,
        /// `Enter` or `F3`: the next match, wrapping.
        FindNext,
        /// `Shift+Enter` or `Shift+F3`: the previous match, wrapping.
        FindPrev,
        /// `Alt+C`: matches case, or not.
        ToggleFindCase,
    ]
);

/// The bar's key context.
pub const KEY_CONTEXT: &str = "FindBar";

/// The query's box width.
const INPUT_WIDTH: f32 = 200.0;

/// What the panel found for the query.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Count {
    /// The current match, from 0.
    pub current: Option<usize>,
    pub total: usize,
    /// The search stopped at its limit: there are more than `total`.
    pub capped: bool,
}

/// What the bar asks of its panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FindBarEvent {
    /// The query or its case changed: search again.
    Changed,
    Step(Step),
    /// `Esc` or the close button.
    Close,
}

pub struct FindBar {
    input: Entity<InputState>,
    match_case: bool,
    count: Count,
    _input: Subscription,
}

impl EventEmitter<FindBarEvent> for FindBar {}

impl FindBar {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Find"));
        let subscription = cx.subscribe(&input, |_, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                cx.emit(FindBarEvent::Changed);
            }
        });
        Self {
            input,
            match_case: false,
            count: Count::default(),
            _input: subscription,
        }
    }

    /// The query as typed.
    pub fn text(&self, cx: &App) -> SharedString {
        self.input.read(cx).value()
    }

    pub fn match_case(&self) -> bool {
        self.match_case
    }

    /// What to search for: the query, matching case or not.
    pub fn needle(&self, cx: &App) -> Needle {
        Needle::new(&self.text(cx), self.match_case)
    }

    /// Puts the cursor in the box with its text selected, after replacing
    /// the text with `prefill` when there is one.
    pub fn focus(&mut self, prefill: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| {
            if let Some(text) = prefill {
                input.set_value(text, window, cx);
            }
            input.focus(window, cx);
            input.select_all(window, cx);
        });
    }

    pub fn count(&self) -> Count {
        self.count
    }

    pub fn set_count(&mut self, count: Count, cx: &mut Context<Self>) {
        if count != self.count {
            self.count = count;
            cx.notify();
        }
    }

    pub fn toggle_case(&mut self, cx: &mut Context<Self>) {
        self.match_case = !self.match_case;
        cx.emit(FindBarEvent::Changed);
        cx.notify();
    }

    /// `3 of 12`, `No results`, or nothing before anything is typed.
    #[must_use]
    pub fn label(&self, cx: &App) -> String {
        if self.text(cx).is_empty() {
            return String::new();
        }
        let Count {
            current,
            total,
            capped,
        } = self.count;
        if total == 0 {
            return "No results".to_owned();
        }
        let more = if capped { "+" } else { "" };
        match current {
            Some(current) => format!("{} of {total}{more}", current + 1),
            None => format!("{total}{more} found"),
        }
    }
}

/// The selected text, for the bar to start from: one line, not blank.
#[must_use]
pub fn prefill(selected: Option<String>) -> Option<String> {
    let text = selected?;
    let line = text.lines().next()?.trim_end_matches('\r');
    (!line.trim().is_empty()).then(|| line.to_owned())
}

impl Render for FindBar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = ui::theme(cx);
        let c = &t.colors;
        let case_on = self.match_case;
        let accent = c.accent_fg;
        let case = ui::icon_button(
            cx,
            "find-case",
            IconName::CaseSensitive,
            cx.listener(|this, _, _, cx| this.toggle_case(cx)),
        )
        .debug_selector(|| "find-case".into())
        .when(case_on, |el| el.bg(c.selected).border_color(accent));
        h_flex()
            .id("find-bar")
            .debug_selector(|| "find-bar".into())
            .key_context(KEY_CONTEXT)
            // Ahead of the box's own Enter and Esc, which a one-line box has
            // no use for, and its Ctrl+F.
            .capture_action(cx.listener(|_, enter: &Enter, _, cx| {
                cx.stop_propagation();
                let step = if enter.shift { Step::Prev } else { Step::Next };
                cx.emit(FindBarEvent::Step(step));
            }))
            .capture_action(cx.listener(|_, _: &Escape, _, cx| {
                cx.stop_propagation();
                cx.emit(FindBarEvent::Close);
            }))
            .capture_action(cx.listener(|this, _: &Search, window, cx| {
                cx.stop_propagation();
                this.focus(None, window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenFind, window, cx| this.focus(None, window, cx)))
            .on_action(cx.listener(|_, _: &FindNext, _, cx| {
                cx.emit(FindBarEvent::Step(Step::Next));
            }))
            .on_action(cx.listener(|_, _: &FindPrev, _, cx| {
                cx.emit(FindBarEvent::Step(Step::Prev));
            }))
            .on_action(cx.listener(|this, _: &ToggleFindCase, _, cx| this.toggle_case(cx)))
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .gap(px(4.0))
            .p(px(4.0))
            .items_center()
            .rounded(t.radius.md)
            .border_1()
            .border_color(c.border_strong)
            .bg(c.surface_menu)
            .child(
                div().w(px(INPUT_WIDTH)).child(
                    Input::new(&self.input)
                        .h(px(ui::CONTROL_HEIGHT))
                        .rounded(t.radius.sm)
                        .bg(c.field)
                        .text_size(t.fonts.interface.size_px()),
                ),
            )
            .child(
                div()
                    .debug_selector(|| "find-count".into())
                    .min_w(px(72.0))
                    .px(px(4.0))
                    .small(&t)
                    .text_color(c.text_muted)
                    .child(self.label(cx)),
            )
            .child(case)
            .child(
                ui::icon_button(
                    cx,
                    "find-prev",
                    IconName::ArrowUp,
                    cx.listener(|_, _, _, cx| cx.emit(FindBarEvent::Step(Step::Prev))),
                )
                .debug_selector(|| "find-prev".into()),
            )
            .child(
                ui::icon_button(
                    cx,
                    "find-next",
                    IconName::ArrowDown,
                    cx.listener(|_, _, _, cx| cx.emit(FindBarEvent::Step(Step::Next))),
                )
                .debug_selector(|| "find-next".into()),
            )
            .child(
                ui::icon_button(
                    cx,
                    "find-close",
                    IconName::Close,
                    cx.listener(|_, _, _, cx| cx.emit(FindBarEvent::Close)),
                )
                .debug_selector(|| "find-close".into()),
            )
    }
}

/// The index `step` moves to from `current` among `total` matches, wrapping;
/// the first (or last) when none is current.
#[must_use]
pub fn stepped(current: Option<usize>, total: usize, step: Step) -> Option<usize> {
    if total == 0 {
        return None;
    }
    Some(match (current, step) {
        (None, Step::Next) => 0,
        (None, Step::Prev) => total - 1,
        (Some(ix), Step::Next) => (ix + 1) % total,
        (Some(ix), Step::Prev) => (ix + total - 1) % total,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_wrap_both_ways_and_start_at_an_end() {
        assert_eq!(stepped(None, 0, Step::Next), None);
        assert_eq!(stepped(None, 3, Step::Next), Some(0));
        assert_eq!(stepped(None, 3, Step::Prev), Some(2));
        assert_eq!(stepped(Some(2), 3, Step::Next), Some(0));
        assert_eq!(stepped(Some(0), 3, Step::Prev), Some(2));
        assert_eq!(stepped(Some(1), 3, Step::Next), Some(2));
    }

    #[test]
    fn a_selection_prefills_its_first_line_unless_blank() {
        assert_eq!(prefill(Some("foo bar".into())).as_deref(), Some("foo bar"));
        assert_eq!(prefill(Some("one\r\ntwo".into())).as_deref(), Some("one"));
        assert_eq!(prefill(Some("  \nx".into())), None);
        assert_eq!(prefill(None), None);
    }
}
