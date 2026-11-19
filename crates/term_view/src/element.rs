//! The gpui element that paints a terminal grid and routes the mouse to its view.

use std::rc::Rc;

use gpui::{
    App, Bounds, ContentMask, CursorStyle, DispatchPhase, Element, ElementId, Entity,
    GlobalElementId, Hitbox, HitboxBehavior, Hsla, InspectorElementId, IntoElement, LayoutId,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, ScrollEvent, Size, Style, TextAlign,
    Window, fill, outline, relative,
};

use crate::frame::{CursorPaint, Drawn, PaintRow};
use crate::view::TerminalView;

pub(crate) struct TerminalElement {
    pub view: Entity<TerminalView>,
}

impl IntoElement for TerminalElement {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

/// What a frame paints; built by [`TerminalView::prepare`].
pub(crate) struct Frame {
    pub cell: Size<Pixels>,
    pub background: Hsla,
    pub selection: Hsla,
    pub cursor_color: Hsla,
    pub rows: Rc<Vec<PaintRow>>,
    pub cursor: Option<CursorPaint>,
}

pub(crate) struct Prepaint {
    hitbox: Hitbox,
    frame: Frame,
}

impl Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = Prepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, None, cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Prepaint {
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        let frame = self
            .view
            .update(cx, |view, cx| view.prepare(bounds, window, cx));
        Prepaint { hitbox, frame }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        prepaint: &mut Prepaint,
        window: &mut Window,
        cx: &mut App,
    ) {
        // Text the user can select; the scrollbar sets its own cursor after.
        window.set_cursor_style(CursorStyle::IBeam, &prepaint.hitbox);
        self.listen(&prepaint.hitbox, window);
        let frame = &prepaint.frame;
        let cell = frame.cell;
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            window.paint_quad(fill(bounds, frame.background));
            for row in frame.rows.iter() {
                for (span, color) in &row.backgrounds {
                    window.paint_quad(fill(*span, *color));
                }
                for span in &row.selection {
                    window.paint_quad(fill(*span, frame.selection));
                }
                for (origin, line) in &row.lines {
                    // A failed glyph lookup leaves the run blank; nothing to recover.
                    let _ = line.paint(*origin, cell.height, TextAlign::Left, None, window, cx);
                }
                paint_drawn(&row.drawn, window);
            }
            if let Some(cursor) = &frame.cursor {
                for (quad, color) in &cursor.fills {
                    window.paint_quad(fill(*quad, *color));
                }
                if let Some(rect) = cursor.outline {
                    window.paint_quad(outline(rect, frame.cursor_color, gpui::BorderStyle::Solid));
                }
                for (origin, line) in &cursor.lines {
                    let _ = line.paint(*origin, cell.height, TextAlign::Left, None, window, cx);
                }
                paint_drawn(&cursor.drawn, window);
            }
            self.view.read(cx).paint_scroll(window);
        });
    }
}

impl TerminalElement {
    /// Registers the mouse listeners for the frame: the scroller's first, so
    /// a press on the scrollbar never reaches the terminal.
    fn listen(&self, hitbox: &Hitbox, window: &mut Window) {
        let view = &self.view;
        scroll::install(view, hitbox, |view| view.scroller_mut(), window);
        let id = hitbox.id;
        window.on_mouse_event({
            let view = view.clone();
            move |event: &ScrollEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble || !id.should_handle_scroll(window) {
                    return;
                }
                view.update(cx, |view, cx| view.wheel(event, cx));
                cx.stop_propagation();
            }
        });
        window.on_mouse_event({
            let view = view.clone();
            move |event: &MouseDownEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble || !id.is_hovered(window) {
                    return;
                }
                view.update(cx, |view, cx| view.mouse_down(event, window, cx));
            }
        });
        window.on_mouse_event({
            let view = view.clone();
            move |event: &MouseMoveEvent, phase, window, cx| {
                if phase != DispatchPhase::Capture {
                    return;
                }
                let hovered = id.is_hovered(window);
                view.update(cx, |view, cx| view.mouse_move(event, hovered, cx));
            }
        });
        window.on_mouse_event({
            let view = view.clone();
            move |event: &MouseUpEvent, phase, _, cx| {
                if phase == DispatchPhase::Capture {
                    view.update(cx, |view, cx| view.mouse_up(event, cx));
                }
            }
        });
    }
}

fn paint_drawn(drawn: &[Drawn], window: &mut Window) {
    for drawn in drawn {
        match drawn {
            Drawn::Quads(quads) => {
                for (quad, color) in quads {
                    window.paint_quad(fill(*quad, *color));
                }
            }
            // The clip trims stroke ends that overrun the cell.
            Drawn::Path { clip, path, color } => {
                window.with_content_mask(Some(ContentMask { bounds: *clip }), |window| {
                    window.paint_path(path.clone(), *color);
                });
            }
        }
    }
}
