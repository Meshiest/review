//! Small controls built from theme tokens: a row of choices, a number stepper,
//! an icon button and a search box.

use std::rc::Rc;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, ClickEvent, Div, ElementId, Entity, InteractiveElement as _, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{Icon, h_flex};

use super::{CONTROL_HEIGHT, TextExt as _, theme};

/// A one-line search box as tall as the toolbar buttons, in the interface
/// font, on the field color, with a search icon and a clear button.
pub fn search_input(state: &Entity<InputState>, cx: &App) -> Input {
    let t = theme(cx);
    Input::new(state)
        .prefix(
            Icon::new(IconName::Search)
                .size(px(13.0))
                .text_color(t.colors.text_muted),
        )
        .cleanable(true)
        .h(px(CONTROL_HEIGHT))
        .rounded(t.radius.sm)
        .bg(t.colors.field)
        .text_size(t.fonts.interface.size_px())
}

type Pick<T> = Rc<dyn Fn(&T, &mut Window, &mut App)>;

/// A row of choices, one selected. Wraps when there are many.
pub fn segmented<T: Clone + 'static>(
    cx: &App,
    id: &'static str,
    options: Vec<(T, SharedString)>,
    selected: impl Fn(&T) -> bool,
    on_pick: impl Fn(&T, &mut Window, &mut App) + 'static,
) -> Div {
    let t = theme(cx);
    let c = &t.colors;
    let on_pick: Pick<T> = Rc::new(on_pick);
    let (hover, active, border) = (c.hover, c.selected, c.control_border);
    h_flex()
        .flex_wrap()
        .gap(px(6.0))
        .children(options.into_iter().enumerate().map(|(ix, (value, label))| {
            let on = selected(&value);
            let on_pick = on_pick.clone();
            div()
                .id(ElementId::from((id, ix)))
                .debug_selector(move || format!("{id}-{ix}"))
                .h(px(26.0))
                .px(px(10.0))
                .flex()
                .items_center()
                .rounded(t.radius.md)
                .border_1()
                .border_color(if on { border } else { c.border })
                .bg(if on { active } else { c.field })
                .text_color(if on { c.text } else { c.text_secondary })
                .small(&t)
                .cursor_pointer()
                .when(!on, move |s| s.hover(move |s| s.bg(hover)))
                .on_click(move |_, window, cx| on_pick(&value, window, cx))
                .child(label)
        }))
}

/// `label  [-] value [+]`.
pub fn stepper(
    cx: &App,
    id: &'static str,
    label: &'static str,
    value: String,
    on_minus: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_plus: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Div {
    let t = theme(cx);
    let c = &t.colors;
    h_flex()
        .gap(px(6.0))
        .items_center()
        .child(
            div()
                .w(px(90.0))
                .small(&t)
                .text_color(c.text_secondary)
                .child(label),
        )
        .child(icon_button(cx, (id, 0usize), IconName::Minus, on_minus))
        .child(
            div()
                .w(px(36.0))
                .flex()
                .justify_center()
                .mono(&t)
                .text_color(c.text)
                .child(value),
        )
        .child(icon_button(cx, (id, 1usize), IconName::Plus, on_plus))
}

/// A square button with an icon.
pub fn icon_button(
    cx: &App,
    id: impl Into<ElementId>,
    icon: IconName,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<Div> {
    let t = theme(cx);
    super::secondary_style(div().id(id), cx)
        .w(px(super::CONTROL_HEIGHT))
        .cursor_pointer()
        .occlude()
        .on_click(on_click)
        .child(
            Icon::new(icon)
                .size(px(super::CONTROL_ICON))
                .text_color(t.colors.text_secondary),
        )
}
