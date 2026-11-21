//! A dropdown for a choice among more options than a [`super::toggle_group`]
//! fits: a closed control showing the chosen option and a chevron, and under
//! it a list with a search box at the top. The search keeps the options
//! whose label holds it, whatever the case; Up and Down move, Enter or a
//! click picks, Esc or a click outside closes.
//!
//! [`SelectState`] is the open list's state and draws both parts, so opening
//! it or typing in its search box draws nothing else again. [`Select`] hands
//! it the options each time its view is drawn.

use std::cell::Cell;
use std::rc::Rc;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    Anchor, AnyElement, App, AppContext as _, Bounds, Context, ElementId, Entity, FocusHandle,
    InteractiveElement as _, IntoElement, KeyDownEvent, MouseDownEvent, ParentElement as _, Pixels,
    Render, RenderOnce, ScrollHandle, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, Window, anchored, canvas, deferred, div, point, px,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Enter, Escape, InputEvent, InputState, MoveDown, MoveUp};
use gpui_kit::component::{Icon, h_flex, v_flex};
use theme::Theme;

use super::{CONTROL_HEIGHT, TextExt as _, search_input, theme};

/// The closed control's width unless [`Select::width`] sets one; it narrows
/// with a narrow window.
const DEFAULT_WIDTH: f32 = 260.0;
/// The list is as wide as the control, and never narrower than this.
const MIN_LIST_WIDTH: f32 = 220.0;
/// The rows scroll past this height.
const LIST_MAX_HEIGHT: f32 = 240.0;
const ROW_HEIGHT: f32 = 24.0;

type Pick<T> = Rc<dyn Fn(&T, &mut Window, &mut App)>;

/// The state of one dropdown, kept by the view that draws it. Its selectors
/// are the [`Select`]'s id for the closed control, `{id}-list` for the open
/// list, `{id}-search` for its search box and `{id}-{n}` for the row of
/// option `n`, numbered as given whatever the search hides.
pub struct SelectState<T> {
    id: SharedString,
    options: Vec<(T, SharedString)>,
    selected: Option<usize>,
    on_pick: Option<Pick<T>>,
    width: Pixels,
    open: bool,
    /// The row Up, Down and Enter act on, among the rows shown.
    cursor: usize,
    /// The search as last filtered, so clearing it on open does not move
    /// the cursor off the chosen row.
    query_text: SharedString,
    focus: FocusHandle,
    query: Entity<InputState>,
    scroll: ScrollHandle,
    /// Where the closed control was drawn last, which the list hangs from.
    control: Rc<Cell<Bounds<Pixels>>>,
    _query: Subscription,
}

impl<T: Clone + 'static> SelectState<T> {
    /// `placeholder` is the search box's.
    pub fn new(
        placeholder: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let placeholder = placeholder.into();
        let query = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        let typed = cx.subscribe(&query, |this: &mut Self, query, event: &InputEvent, cx| {
            if !matches!(event, InputEvent::Change) {
                return;
            }
            let text = query.read(cx).value();
            if text != this.query_text {
                this.query_text = text;
                this.cursor = 0;
                this.scroll.scroll_to_item(0);
                cx.notify();
            }
        });
        Self {
            id: SharedString::default(),
            options: Vec::new(),
            selected: None,
            on_pick: None,
            width: px(DEFAULT_WIDTH),
            open: false,
            cursor: 0,
            query_text: SharedString::default(),
            focus: cx.focus_handle(),
            query,
            scroll: ScrollHandle::new(),
            control: Rc::default(),
            _query: typed,
        }
    }

    #[must_use]
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// The options the search keeps, as indices into the options.
    fn shown(&self) -> Vec<usize> {
        let query = self.query_text.trim().to_lowercase();
        self.options
            .iter()
            .enumerate()
            .filter(|(_, (_, label))| query.is_empty() || label.to_lowercase().contains(&query))
            .map(|(ix, _)| ix)
            .collect()
    }

    /// Opens the list on the chosen row, with an empty search box that has
    /// the keyboard.
    fn open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.query_text.is_empty() {
            self.query_text = SharedString::default();
            self.query
                .update(cx, |query, cx| query.set_value(String::new(), window, cx));
        }
        self.cursor = self.selected.unwrap_or(0);
        self.scroll.scroll_to_item(self.cursor);
        self.open = true;
        self.query.update(cx, |query, cx| query.focus(window, cx));
        cx.notify();
    }

    /// Closes the list; `refocus` gives the keyboard back to the control.
    fn close(&mut self, refocus: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !self.open {
            return;
        }
        self.open = false;
        if refocus {
            window.focus(&self.focus, cx);
        }
        cx.notify();
    }

    fn step(&mut self, by: isize, cx: &mut Context<Self>) {
        let count = self.shown().len();
        if count == 0 {
            return;
        }
        self.cursor = self.cursor.saturating_add_signed(by).min(count - 1);
        self.scroll.scroll_to_item(self.cursor);
        cx.notify();
    }

    fn pick(&mut self, option: usize, window: &mut Window, cx: &mut Context<Self>) {
        let value = self.options.get(option).map(|(value, _)| value.clone());
        self.close(true, window, cx);
        if let (Some(value), Some(on_pick)) = (value, self.on_pick.clone()) {
            on_pick(&value, window, cx);
        }
    }

    fn pick_cursor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(&option) = self.shown().get(self.cursor) {
            self.pick(option, window, cx);
        }
    }

    fn control(&self, t: &Theme, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = &t.colors;
        let id = self.id.clone();
        let open = self.open;
        let label = self
            .selected
            .and_then(|ix| self.options.get(ix))
            .map(|(_, label)| label.clone())
            .unwrap_or_default();
        let edge = if open || self.focus.is_focused(window) {
            c.focus
        } else {
            c.control_border
        };
        let recorded = self.control.clone();
        h_flex()
            .id(ElementId::Name(id.clone()))
            .debug_selector(move || id.to_string())
            .track_focus(&self.focus)
            .relative()
            .w(self.width)
            .max_w_full()
            .h(px(CONTROL_HEIGHT))
            .px(px(10.0))
            .gap(px(8.0))
            .items_center()
            .justify_between()
            .rounded(t.radius.md)
            .border_1()
            .border_color(edge)
            .bg(c.field)
            .text_color(c.text)
            .cursor_pointer()
            .on_click(cx.listener(|this, _, window, cx| {
                if this.open {
                    this.close(true, window, cx);
                } else {
                    this.open(window, cx);
                }
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if !this.open && matches!(event.keystroke.key.as_str(), "enter" | "space" | "down")
                {
                    cx.stop_propagation();
                    this.open(window, cx);
                }
            }))
            .child(div().flex_1().min_w_0().truncate().child(label))
            .child(
                Icon::new(IconName::ChevronDown)
                    .size(px(14.0))
                    .flex_none()
                    .text_color(c.text_muted),
            )
            .child(
                canvas(
                    move |bounds, window, _| {
                        // The list hangs from where the control is now.
                        if recorded.replace(bounds) != bounds && open {
                            window.refresh();
                        }
                    },
                    |_, (), _, _| {},
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
    }

    /// The open list, hung under the control (over it when there is no
    /// room below) and drawn above everything, so opening it moves nothing.
    fn list(&self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let c = &t.colors;
        let id = &self.id;
        let control = self.control.get();
        let hover = c.hover;
        let rows: Vec<_> = self
            .shown()
            .into_iter()
            .enumerate()
            .map(|(row, option)| {
                let label = self.options[option].1.clone();
                let chosen = self.selected == Some(option);
                let under_cursor = row == self.cursor;
                let selector = format!("{id}-{option}");
                h_flex()
                    .id(ElementId::named_usize(id.clone(), option))
                    .debug_selector(move || selector)
                    .h(px(ROW_HEIGHT))
                    .flex_none()
                    .px(px(8.0))
                    .gap(px(6.0))
                    .items_center()
                    .rounded(t.radius.sm)
                    .cursor_pointer()
                    .text_color(if chosen { c.text } else { c.text_secondary })
                    .when(under_cursor, |el| el.bg(c.selected))
                    .when(!under_cursor, |el| el.hover(move |s| s.bg(hover)))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.pick(option, window, cx);
                    }))
                    .child(div().w(px(14.0)).flex_none().children(chosen.then(|| {
                        Icon::new(IconName::Check)
                            .size(px(14.0))
                            .text_color(c.accent_fg)
                    })))
                    .child(div().min_w_0().truncate().child(label))
            })
            .collect();
        let empty = rows.is_empty().then(|| {
            div()
                .px(px(8.0))
                .py(px(4.0))
                .text_color(c.text_muted)
                .child("Nothing matches")
        });
        let close_outside = cx.listener(move |this, event: &MouseDownEvent, window, cx| {
            // A press on the control toggles the list itself.
            if !control.contains(&event.position) {
                this.close(false, window, cx);
            }
        });
        let list = format!("{id}-list");
        let search = format!("{id}-search");
        deferred(
            anchored()
                .attach(control, Anchor::BottomLeft)
                .offset(point(px(0.0), px(4.0)))
                .child(
                    v_flex()
                        .id(ElementId::Name(list.clone().into()))
                        .debug_selector(move || list)
                        .occlude()
                        .capture_action(cx.listener(|this, _: &MoveUp, _, cx| {
                            cx.stop_propagation();
                            this.step(-1, cx);
                        }))
                        .capture_action(cx.listener(|this, _: &MoveDown, _, cx| {
                            cx.stop_propagation();
                            this.step(1, cx);
                        }))
                        .capture_action(cx.listener(|this, _: &Enter, window, cx| {
                            cx.stop_propagation();
                            this.pick_cursor(window, cx);
                        }))
                        .capture_action(cx.listener(|this, _: &Escape, window, cx| {
                            cx.stop_propagation();
                            this.close(true, window, cx);
                        }))
                        .on_mouse_down_out(close_outside)
                        .w(control.size.width.max(px(MIN_LIST_WIDTH)))
                        .gap(px(6.0))
                        .p(px(8.0))
                        .rounded(t.radius.lg)
                        .border_1()
                        .border_color(c.border_strong)
                        .bg(c.surface_menu)
                        .child(
                            div()
                                .debug_selector(move || search)
                                .child(search_input(&self.query, cx)),
                        )
                        .child(
                            v_flex()
                                .id(ElementId::Name(format!("{id}-rows").into()))
                                .max_h(px(LIST_MAX_HEIGHT))
                                .overflow_y_scroll()
                                .track_scroll(&self.scroll)
                                .small(t)
                                .children(rows)
                                .children(empty),
                        ),
                ),
        )
        .with_priority(1)
        .into_any_element()
    }
}

impl<T: Clone + 'static> Render for SelectState<T> {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        let list = self.open.then(|| self.list(&t, cx));
        div()
            .max_w_full()
            .child(self.control(&t, window, cx))
            .children(list)
    }
}

/// A dropdown drawn from `state`: its options, which one is chosen and what
/// a pick does, given afresh each time the view holding it is drawn.
#[derive(IntoElement)]
pub struct Select<T: Clone + 'static> {
    id: SharedString,
    state: Entity<SelectState<T>>,
    options: Vec<(T, SharedString)>,
    selected: Option<usize>,
    on_pick: Option<Pick<T>>,
    width: Pixels,
}

impl<T: Clone + 'static> Select<T> {
    /// `id` is the closed control's id and `debug_selector`, and the prefix
    /// of the open list's.
    pub fn new(id: impl Into<SharedString>, state: &Entity<SelectState<T>>) -> Self {
        Self {
            id: id.into(),
            state: state.clone(),
            options: Vec::new(),
            selected: None,
            on_pick: None,
            width: px(DEFAULT_WIDTH),
        }
    }

    /// Each option's value and label; the first that `selected` holds for
    /// is the chosen one.
    #[must_use]
    pub fn options<L: Into<SharedString>>(
        mut self,
        options: impl IntoIterator<Item = (T, L)>,
        selected: impl Fn(&T) -> bool,
    ) -> Self {
        self.options = options
            .into_iter()
            .map(|(value, label)| (value, label.into()))
            .collect();
        self.selected = self.options.iter().position(|(value, _)| selected(value));
        self
    }

    /// The closed control's width.
    #[must_use]
    pub fn width(mut self, width: Pixels) -> Self {
        self.width = width;
        self
    }

    /// Runs when an option is picked, after the list closes.
    #[must_use]
    pub fn on_pick(mut self, on_pick: impl Fn(&T, &mut Window, &mut App) + 'static) -> Self {
        self.on_pick = Some(Rc::new(on_pick));
        self
    }
}

impl<T: Clone + 'static> RenderOnce for Select<T> {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        self.state.update(cx, |state, _| {
            state.id = self.id;
            state.options = self.options;
            state.selected = self.selected;
            state.on_pick = self.on_pick;
            state.width = self.width;
        });
        self.state
    }
}
