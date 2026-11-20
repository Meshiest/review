//! Controls that open a gpui-kit dropdown menu: any element as the trigger,
//! and a split button (a main action with a chevron for the others) in the
//! look of the pull request header's.

use std::rc::Rc;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    Anchor, App, ClickEvent, Context, Div, ElementId, Hsla, InteractiveElement, Interactivity,
    IntoElement, MouseButton, ParentElement as _, RenderOnce, SharedString, Stateful,
    StatefulInteractiveElement as _, StyleRefinement, Styled, Window, div, px,
};
use gpui_kit::TestSupportExt as _;
use gpui_kit::assets::IconName;
use gpui_kit::base::Selectable;
use gpui_kit::component::menu::{DropdownMenu, PopupMenu};
use gpui_kit::component::{Icon, h_flex};

use super::{CONTROL_HEIGHT, TextExt as _, secondary_style, theme};

/// An element of the caller's as the trigger of a gpui-kit dropdown menu
/// (`.dropdown_menu(..)`): the menu opens on a press, or on Enter or Space
/// while it has the focus, and the trigger takes `open_bg` while it shows.
#[derive(IntoElement)]
pub struct MenuTrigger {
    base: Stateful<Div>,
    open: bool,
    open_bg: Option<Hsla>,
}

impl MenuTrigger {
    pub fn new(base: Stateful<Div>) -> Self {
        Self {
            base,
            open: false,
            open_bg: None,
        }
    }

    /// The fill while the menu is open.
    #[must_use]
    pub fn open_bg(mut self, color: Hsla) -> Self {
        self.open_bg = Some(color);
        self
    }
}

impl Selectable for MenuTrigger {
    fn selected(mut self, selected: bool) -> Self {
        self.open = selected;
        self
    }

    fn is_selected(&self) -> bool {
        self.open
    }
}

impl Styled for MenuTrigger {
    fn style(&mut self) -> &mut StyleRefinement {
        self.base.style()
    }
}

impl InteractiveElement for MenuTrigger {
    fn interactivity(&mut self) -> &mut Interactivity {
        self.base.interactivity()
    }
}

impl DropdownMenu for MenuTrigger {}

impl RenderOnce for MenuTrigger {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let fill = self.open_bg.filter(|_| self.open);
        self.base.when_some(fill, |el, fill| el.bg(fill))
    }
}

type ClickHandler = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;
type MenuBuilder = Rc<dyn Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu>;

/// A main action and a chevron that opens a menu of the others, as one
/// toolbar control: [`CONTROL_HEIGHT`] tall, the secondary look, a hairline
/// between the two parts. The main part's id is `id`, the chevron's
/// `{id}-menu`; both are `debug_selector`s too.
#[derive(IntoElement)]
pub struct SplitButton {
    id: &'static str,
    label: SharedString,
    disabled: bool,
    on_click: Option<ClickHandler>,
    menu: Option<MenuBuilder>,
}

impl SplitButton {
    pub fn new(id: &'static str, label: impl Into<SharedString>) -> Self {
        Self {
            id,
            label: label.into(),
            disabled: false,
            on_click: None,
            menu: None,
        }
    }

    /// The main part is drawn faint, and a click on it does nothing.
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
        self.on_click = Some(Rc::new(handler));
        self
    }

    /// The chevron's menu, built each time it opens.
    #[must_use]
    pub fn menu(
        mut self,
        build: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
    ) -> Self {
        self.menu = Some(Rc::new(build));
        self
    }
}

impl RenderOnce for SplitButton {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let t = theme(cx);
        let c = &t.colors;
        let hover = c.hover;
        let id = self.id;
        let on_click = self.on_click.filter(|_| !self.disabled);
        let main = div()
            .id(id)
            .debug_selector(move || id.into())
            .h_full()
            .px(px(8.0))
            .flex()
            .items_center()
            .whitespace_nowrap()
            .when(self.disabled, |el| el.text_color(c.text_faint))
            .when_some(on_click, |el, handler| {
                el.cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .on_click(move |event, window, cx| handler(event, window, cx))
            })
            // A press never moves the focus, so the box being written in keeps it.
            .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
            .child(self.label)
            .test_support();
        let unit = secondary_style(h_flex().id(ElementId::from(format!("{id}-unit"))), cx)
            .debug_selector(move || format!("{id}-unit"))
            .overflow_hidden()
            .small(&t)
            .child(main);
        let Some(menu) = self.menu else {
            return unit.into_any_element();
        };
        let menu_id = format!("{id}-menu");
        let selector = menu_id.clone();
        // Inside the unit's border: the chevron is as wide as it is tall.
        let chevron = div()
            .id(SharedString::from(menu_id))
            .debug_selector(move || selector)
            .size(px(CONTROL_HEIGHT - 2.0))
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .hover(move |s| s.bg(hover))
            .child(
                Icon::new(IconName::ChevronDown)
                    .size(px(12.0))
                    .text_color(c.text_secondary),
            );
        unit.child(div().w(px(1.0)).h_full().bg(c.border_strong))
            .child(
                MenuTrigger::new(chevron)
                    .open_bg(c.selected)
                    .dropdown_menu_with_anchor(Anchor::TopRight, move |popup, window, cx| {
                        menu(popup, window, cx)
                    }),
            )
            .into_any_element()
    }
}
