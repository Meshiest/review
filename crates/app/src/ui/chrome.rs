//! Chrome helpers: panes, bars, cards, chips, rules, the empty state and the
//! app's buttons. They draw 1px `border` hairlines, no shadows, the theme's
//! radii and gpui-component's widgets.

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, ClickEvent, Div, ElementId, FontWeight, Hsla, InteractiveElement as _,
    IntoElement, MouseButton, ParentElement as _, RenderOnce, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use gpui_kit::TestSupportExt as _;
use gpui_kit::assets::IconName;
use gpui_kit::component::Disableable as _;
use gpui_kit::component::button::{Button as FlatButton, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::{Icon, Sizable as _, Size, TitleBar, h_flex, v_flex};

use super::{TextExt as _, theme};

/// A pane of the window: the sidebar, a bordered `surface`.
pub fn panel(cx: &App) -> Div {
    let t = theme(cx);
    div()
        .bg(t.colors.surface)
        .border_1()
        .border_color(t.colors.border)
}

/// A panel header: the panel's own `surface` with a hairline under it.
pub fn bar(cx: &App) -> Div {
    let t = theme(cx);
    div()
        .bg(t.colors.surface)
        .border_b_1()
        .border_color(t.colors.border)
}

/// The window's status bar: the same bar, with its hairline above.
pub fn status_bar(cx: &App) -> Div {
    let t = theme(cx);
    div()
        .bg(t.colors.surface)
        .border_t_1()
        .border_color(t.colors.border)
}

/// The window's title bar. Geist keeps it on the page background.
pub fn title_bar(cx: &App) -> TitleBar {
    TitleBar::new().bg(theme(cx).colors.background)
}

/// The app's name in the title bar.
pub fn wordmark(cx: &App, text: impl Into<SharedString>) -> Div {
    div()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme(cx).colors.text)
        .child(text.into())
}

/// A bordered block on a panel: a comment, a note.
pub fn card(cx: &App) -> Div {
    let t = theme(cx);
    div()
        .border_1()
        .border_color(t.colors.border)
        .rounded(t.radius.lg)
        .bg(t.colors.surface)
}

/// A status chip: the state color as text, tinted fill and border from the
/// theme's recipe. `state` is usually one of `ok`, `warn`, `danger`, `info`.
pub fn chip(cx: &App, label: impl Into<SharedString>, state: Hsla) -> Div {
    let t = theme(cx);
    let colors = t.chip_colors(state);
    div()
        .flex_none()
        .flex()
        .items_center()
        .h(px(18.0))
        .px(px(6.0))
        .rounded(t.radius.sm)
        .border_1()
        .border_color(colors.border)
        .bg(colors.fill)
        .text_color(colors.text)
        .tiny(&t)
        .whitespace_nowrap()
        .child(label.into())
}

/// How far a checkbox's hit area reaches past its box on every side: with the
/// 14 px box, 22 px square, the height of a list row.
pub const CHECKBOX_HIT_PAD: f32 = 4.0;

/// A viewed-style checkbox, sized for list rows. Add `.checked`, `.on_click`.
/// The padding makes a click near the box toggle it; the equal negative
/// margin keeps the row's layout as if there were none.
pub fn checkbox(id: impl Into<ElementId>) -> Checkbox {
    Checkbox::new(id)
        .with_size(Size::Large)
        .cursor_pointer()
        .p(px(CHECKBOX_HIT_PAD))
        .m(px(-CHECKBOX_HIT_PAD))
}

/// A 1px horizontal rule.
pub fn divider(cx: &App) -> Div {
    div()
        .h(px(1.0))
        .w_full()
        .flex_none()
        .bg(theme(cx).colors.border)
}

/// A 1px vertical rule.
pub fn vdivider(cx: &App) -> Div {
    div()
        .w(px(1.0))
        .h_full()
        .flex_none()
        .bg(theme(cx).colors.border)
}

/// A line of muted text centered in its panel.
pub fn empty_note(cx: &App, text: &str) -> Div {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .text_color(theme(cx).colors.text_muted)
        .child(text.to_owned())
}

/// The main area when nothing is open: a title, a line of explanation and the
/// keys that work, as a quiet centered message.
pub fn empty_state(cx: &App, title: &str, body: &str, hints: &[(&str, &str)]) -> AnyElement {
    let t = theme(cx);
    let c = &t.colors;
    let hint_row = |keycap: &dyn Fn(&str) -> Div| {
        h_flex()
            .gap(px(12.0))
            .pt(px(6.0))
            .small(&t)
            .text_color(c.text_muted)
            .children(hints.iter().map(|(key, action)| {
                h_flex()
                    .gap(px(6.0))
                    .items_center()
                    .child(keycap(key))
                    .child(div().child((*action).to_owned()))
            }))
    };
    let keycap = |key: &str| {
        div()
            .mono(&t)
            .tiny(&t)
            .px(px(5.0))
            .h(px(18.0))
            .flex()
            .items_center()
            .rounded(t.radius.sm)
            .border_1()
            .border_color(c.border)
            .text_color(c.text_secondary)
            .child(key.to_owned())
    };
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(c.surface)
        .child(
            v_flex()
                .items_center()
                .gap(px(4.0))
                .child(
                    div()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(c.text)
                        .child(title.to_owned()),
                )
                .child(div().text_color(c.text_muted).child(body.to_owned()))
                .child(hint_row(&keycap)),
        )
        .into_any_element()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ButtonKind {
    /// The one confirming action of a dialog or the merge box ("Merge
    /// pull request", "Submit review", "Confirm"); nowhere else. Drawn
    /// like [`ButtonKind::Secondary`]: the app has no light buttons.
    Primary,
    /// Toolbars and headers: a dark fill, a hairline control border, the
    /// text color. What [`Button::toolbar`] and [`Button::icon`] make.
    Secondary,
    Outline,
    /// Text until hovered.
    Ghost,
    Danger,
}

type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

/// How dim a toolbar button that cannot be pressed is drawn.
const DISABLED_OPACITY: f32 = 0.45;

/// A compact button. Labels are sentence case.
///
/// Registers the button under its id for headless UI tests (a no-op outside
/// them): gpui-component's button does so itself, the toolbar variant through
/// `test_support`.
#[derive(IntoElement)]
pub struct Button {
    id: ElementId,
    label: SharedString,
    kind: ButtonKind,
    /// Drawn instead of the label, in a square as wide as it is tall.
    icon: Option<IconName>,
    on_click: Option<ClickHandler>,
    disabled: bool,
}

/// The height of a toolbar control; an icon-only button is this wide too.
pub const CONTROL_HEIGHT: f32 = 24.0;
/// The icon in an icon-only button.
pub const CONTROL_ICON: f32 = 14.0;

impl Button {
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>, kind: ButtonKind) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            kind,
            icon: None,
            on_click: None,
            disabled: false,
        }
    }

    /// A toolbar or header button: [`ButtonKind::Secondary`].
    pub fn toolbar(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Self {
        Self::new(id, label, ButtonKind::Secondary)
    }

    /// An icon-only toolbar button, square ([`CONTROL_HEIGHT`] each way)
    /// with the icon centered.
    pub fn icon(id: impl Into<ElementId>, icon: IconName) -> Self {
        Self {
            icon: Some(icon),
            ..Self::new(id, SharedString::default(), ButtonKind::Secondary)
        }
    }

    /// Drawn dimmed, and a click does nothing.
    #[must_use]
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    #[must_use]
    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Box::new(handler));
        self
    }
}

/// The toolbar look on any element: [`CONTROL_HEIGHT`] tall, a dark fill,
/// a hairline control border, the text color and the hover token, all from the
/// theme.
pub fn secondary_style<E: gpui::Styled + gpui::InteractiveElement>(el: E, cx: &App) -> E {
    let t = theme(cx);
    let c = &t.colors;
    let hover = c.hover;
    el.h(px(CONTROL_HEIGHT))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(t.radius.sm)
        .border_1()
        .border_color(c.border_strong)
        .bg(c.surface_raised)
        .text_color(c.text)
        .hover(move |s| s.bg(hover))
}

impl Button {
    fn secondary(self, cx: &App) -> impl IntoElement {
        let t = theme(cx);
        let c = &t.colors;
        let disabled = self.disabled;
        let el = secondary_style(div().id(self.id), cx).small(&t);
        let el = match self.icon {
            Some(icon) => el.w(px(CONTROL_HEIGHT)).child(
                Icon::new(icon)
                    .size(px(CONTROL_ICON))
                    .text_color(c.text_secondary),
            ),
            None => el.px(px(8.0)).whitespace_nowrap().child(self.label),
        };
        let el = match self.on_click.filter(|_| !disabled) {
            Some(handler) => el
                .cursor_pointer()
                .on_click(move |event, window, cx| handler(event, window, cx)),
            None => el,
        };
        // Like gpui-component's buttons: a press never moves the focus, so
        // the box being written in keeps it.
        el.on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
            .when(disabled, |el| el.opacity(DISABLED_OPACITY))
            .test_support()
    }
}

impl RenderOnce for Button {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let dark = matches!(
            self.kind,
            ButtonKind::Primary | ButtonKind::Secondary | ButtonKind::Outline
        );
        if dark || self.icon.is_some() {
            return self.secondary(cx).into_any_element();
        }
        let on_click = self.on_click.filter(|_| !self.disabled);
        let button = FlatButton::new(self.id)
            .label(self.label)
            .with_size(Size::Small)
            .disabled(self.disabled);
        let button = match self.kind {
            ButtonKind::Danger => button.danger().outline(),
            _ => button.ghost(),
        };
        // gpui-component draws its buttons with the arrow cursor.
        match on_click {
            Some(handler) => button
                .cursor_pointer()
                .on_click(move |event, window, cx| handler(event, window, cx))
                .into_any_element(),
            None => button.into_any_element(),
        }
    }
}
