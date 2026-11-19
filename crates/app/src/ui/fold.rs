//! Rows that fold or unfold: the whole row is the target, not only its
//! chevron.

use std::rc::Rc;

use gpui::{
    App, ClickEvent, Div, FocusHandle, InteractiveElement as _, KeyboardClickEvent, Stateful,
    StatefulInteractiveElement as _, Styled as _, Window,
};
use theme::Theme;

use crate::inbox::{FOLD_CONTEXT, ToggleFold};

/// Makes `row` a toggle: a pointer cursor, the `hover` background, a place in
/// the tab order, and `toggle` on a click anywhere on it or on Enter or Space
/// while it has the focus (the app's `ToggleFold` binding). The click stops here, so rows above do not also
/// take it.
///
/// A control inside the row keeps its own action by stopping its click
/// (`cx.stop_propagation()`) or by occluding the row (`.occlude()`).
pub fn toggle_row(
    row: Stateful<Div>,
    t: &Theme,
    toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    toggle_row_with(row.focusable(), t, toggle)
}

/// [`toggle_row`] with a focus handle of the caller's, for a row that is
/// drawn as another element when it folds and must keep the focus.
pub fn toggle_row_focused(
    row: Stateful<Div>,
    t: &Theme,
    focus: &FocusHandle,
    toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    toggle_row_with(row.track_focus(focus), t, toggle)
}

fn toggle_row_with(
    row: Stateful<Div>,
    t: &Theme,
    toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let hover = t.colors.hover;
    let toggle = Rc::new(toggle);
    let by_key = toggle.clone();
    row.cursor_pointer()
        .tab_stop(true)
        .hover(move |s| s.bg(hover))
        .focus_visible(move |s| s.bg(hover))
        // The keys the other fold headers answer to (`FOLD_CONTEXT` is bound
        // to Enter and Space), so they act on key down and the built-in
        // keyboard click does not fire a second time.
        .key_context(FOLD_CONTEXT)
        .on_action(move |_: &ToggleFold, window, cx| {
            by_key(
                &ClickEvent::Keyboard(KeyboardClickEvent::default()),
                window,
                cx,
            );
        })
        .on_click(move |event, window, cx| {
            cx.stop_propagation();
            toggle(event, window, cx);
        })
}
