//! The app's dock: gpui-component's skin, plus the headers of the folded
//! sidebar sections under the left region and a fold button before a
//! sidebar section's tabs.
//!
//! One dock area holds everything: the sidebar's sections in its left
//! region and the pull request tabs in its center, so a tab drags between
//! the two like between any two groups.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    AnyElement, AnyView, App, AppContext as _, Axis, CursorStyle, DispatchPhase, Div, Entity,
    Hitbox, HitboxBehavior, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    ParentElement, Pixels, Stateful, StyleRefinement, Styled, Window, canvas, div, px,
};
use gpui_kit::base::ResizeHandleContext;
use gpui_kit::component::{
    ActiveTheme as _,
    dock::{
        BasePanelView, DockArea, DockAreaRenderer, DockContext, DockPlacement, DockSkin,
        DropIndicator, NodeId, PanelState, PanelStyle, TabGroupContext, TabGroupRenderer,
    },
    h_flex, v_flex,
};

use crate::layout;
use crate::sidebar::Sidebar;
use crate::sidebar_panels::{self, SlotDrag};

/// gpui-component's skin with the folded sections stacked under the left
/// region, which also moves the boundaries between the sidebar's open slots
/// (see [`SlotDrag`]); everything else is the skin's own.
struct AppSkin {
    inner: Rc<DockSkin>,
    /// The sidebar boundary being dragged.
    drag: Rc<RefCell<Option<SlotDrag>>>,
}

impl DockAreaRenderer for AppSkin {
    fn frame(&self, window: &mut Window, cx: &mut App) -> Stateful<Div> {
        self.inner.frame(window, cx)
    }

    fn split_frame(
        &self,
        node: NodeId,
        axis: Axis,
        window: &mut Window,
        cx: &mut App,
    ) -> Stateful<Div> {
        self.inner.split_frame(node, axis, window, cx)
    }

    fn center_frame(&self, window: &mut Window, cx: &mut App) -> Stateful<Div> {
        self.inner.center_frame(window, cx)
    }

    fn render_split_handle(
        &self,
        handle: &ResizeHandleContext,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<AnyElement> {
        self.inner.render_split_handle(handle, window, cx)
    }

    fn closed_dock_extent(&self, placement: DockPlacement) -> Pixels {
        self.inner.closed_dock_extent(placement)
    }

    fn render_dock(
        &self,
        dock: &DockContext,
        content: AnyElement,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let content = if dock.placement() == DockPlacement::Left {
            // Drawn from its cache: a diff frame does not build it again.
            let stack = Sidebar::global(cx).update(cx, |sidebar, cx| sidebar.folded_stack(cx));
            let height = stack.read(cx).height(cx);
            let style = StyleRefinement::default().w_full().flex_none().h(height);
            let drag = self.drag.clone();
            // Over the sections, so it is hovered wherever they are and not
            // where a popover covers them; it takes no hover from them.
            let boundaries = canvas(
                |region, window, _| window.insert_hitbox(region, HitboxBehavior::Normal),
                move |_, hitbox, window, _| watch_boundaries(hitbox, &drag, window),
            )
            .absolute()
            .size_full();
            v_flex()
                .size_full()
                .child(
                    v_flex()
                        .relative()
                        .flex_1()
                        .min_h_0()
                        .w_full()
                        .child(content)
                        .child(boundaries),
                )
                .child(AnyView::from(stack).cached(style))
                .into_any_element()
        } else {
            content
        };
        self.inner.render_dock(dock, content, window, cx)
    }

    fn build_placeholder(
        &self,
        state: &PanelState,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Arc<dyn BasePanelView>> {
        self.inner.build_placeholder(state, window, cx)
    }

    fn tab_group_renderer(&self) -> Rc<dyn TabGroupRenderer> {
        Rc::new(AppTabs {
            inner: self.inner.tab_group_renderer(),
        })
    }
}

/// The skin's tab groups, with a sidebar section's fold button before its
/// tabs.
struct AppTabs {
    inner: Rc<dyn TabGroupRenderer>,
}

impl TabGroupRenderer for AppTabs {
    fn frame(&self, group: &TabGroupContext, window: &mut Window, cx: &mut App) -> Stateful<Div> {
        self.inner.frame(group, window, cx)
    }

    fn content_frame(
        &self,
        group: &TabGroupContext,
        window: &mut Window,
        cx: &mut App,
    ) -> Stateful<Div> {
        self.inner.content_frame(group, window, cx)
    }

    fn render_tab_bar(
        &self,
        group: &TabGroupContext,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let bar = self.inner.render_tab_bar(group, window, cx);
        let Some(fold) = sidebar_panels::fold_button(group, cx) else {
            return bar;
        };
        let theme = cx.theme();
        // As the skin's own leading buttons: the tab bar's ground and bottom
        // border, and the first tab's 1px border overlapped.
        let lead = h_flex()
            .flex_none()
            .items_center()
            .h_full()
            .px_1()
            .mr(px(-1.0))
            .border_r_1()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.tokens.tab_bar)
            .child(fold);
        h_flex()
            .w_full()
            .items_stretch()
            .child(lead)
            .child(div().flex_1().min_w_0().child(bar))
            .into_any_element()
    }

    fn render_active_panel(
        &self,
        panel: AnyView,
        group: &TabGroupContext,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        self.inner.render_active_panel(panel, group, window, cx)
    }

    fn render_drop_indicator(
        &self,
        indicator: DropIndicator,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<AnyElement> {
        self.inner.render_drop_indicator(indicator, window, cx)
    }

    fn render_empty(
        &self,
        group: &TabGroupContext,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<AnyElement> {
        self.inner.render_empty(group, window, cx)
    }
}

/// An empty dock with the app's skin; the shell puts the sidebar's sections
/// in its left region. Every tab shows a tab bar, also the only tab of a
/// group, and no dock toggle buttons: `Ctrl+B` toggles the sidebar.
pub fn new_dock(window: &mut Window, cx: &mut App) -> Entity<DockArea> {
    let mut skin = None;
    let dock = cx.new(|cx| {
        let inner = DockSkin::new(cx);
        skin = Some(inner.clone());
        DockArea::new("main", Some(layout::VERSION), window, cx).with_renderer(Rc::new(AppSkin {
            inner,
            drag: Rc::default(),
        }))
    });
    let skin = skin.expect("the skin is built with the dock");
    skin.set_toggle_button_visible(false, cx);
    // The panels draw their own close button: the skin's is not drawn on a
    // dock's last tab, which must still close.
    skin.set_panel_style(PanelStyle::TabBar, cx);
    dock
}

/// Takes over a press on a boundary between two open slots of the sidebar,
/// over `region`, before gpui-base's handle sees it, and moves the boundary
/// with the pointer until it is let go.
fn watch_boundaries(region: Hitbox, drag: &Rc<RefCell<Option<SlotDrag>>>, window: &mut Window) {
    if drag.borrow().is_some() {
        window.set_window_cursor_style(CursorStyle::ResizeUpDown);
    }
    window.on_mouse_event({
        let drag = drag.clone();
        move |event: &MouseDownEvent, phase, window, cx| {
            if phase != DispatchPhase::Capture
                || event.button != MouseButton::Left
                || !region.is_hovered(window)
            {
                return;
            }
            let Some(dock) = Sidebar::global(cx).read(cx).dock() else {
                return;
            };
            let Some(started) = SlotDrag::start(dock.read(cx), event.position, cx) else {
                return;
            };
            started.begin(cx);
            *drag.borrow_mut() = Some(started);
            cx.stop_propagation();
            window.refresh();
        }
    });
    window.on_mouse_event({
        let drag = drag.clone();
        move |event: &MouseMoveEvent, phase, window, cx| {
            if phase != DispatchPhase::Capture {
                return;
            }
            let Some(current) = drag.borrow().clone() else {
                return;
            };
            cx.stop_propagation();
            let Some(dock) = Sidebar::global(cx).read(cx).dock() else {
                return;
            };
            // Let go outside the window: the drag ended there.
            if event.pressed_button != Some(MouseButton::Left) {
                drag.borrow_mut().take();
                SlotDrag::finish(&dock, cx);
                window.refresh();
                return;
            }
            current.drag_to(event.position.y, &dock, window, cx);
        }
    });
    window.on_mouse_event({
        let drag = drag.clone();
        move |_: &MouseUpEvent, phase, window, cx| {
            if phase != DispatchPhase::Capture || drag.borrow_mut().take().is_none() {
                return;
            }
            cx.stop_propagation();
            if let Some(dock) = Sidebar::global(cx).read(cx).dock() {
                SlotDrag::finish(&dock, cx);
            }
            window.refresh();
        }
    });
}
