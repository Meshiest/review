//! Spinning CI icons that do not re-render the view they sit in.
//!
//! An animation in a view asks for the next frame on that view, and gpui
//! marks a view dirty together with every view above it, so a spinner in
//! the sidebar would re-render the whole sidebar each frame, even as its own
//! entity. Instead the sidebar draws an empty [`SpinnerSlot`] that records
//! where it landed, and a [`SpinnerLayer`] in the window's root view paints
//! the turning icons there. The root re-renders each frame while one spins;
//! the sidebar stays a cached view.
//!
//! Dock panels register their slots in [`DockSpinners`]. A panel in a tab
//! that is not displayed keeps the slots of its last render, so each set is
//! painted only while its panel says it is shown.
//!
//! A cached view keeps its slots while the window replays it, so a slot
//! alone cannot tell whether its row is still on screen. Each slot holds a
//! token in its element's state: gpui keeps an element's state only while
//! the element is drawn or replayed, so a slot whose view was not drawn
//! loses its token at the end of that frame and is never painted again.
//! A slot is also clipped to the content mask it was drawn under, and one
//! entirely outside that mask (a folded or scrolled-away row) is skipped.

use std::cell::{Cell, RefCell};
use std::f32::consts::TAU;
use std::rc::{Rc, Weak};
use std::sync::OnceLock;
use std::time::Instant;

use gpui::{
    AnyElement, App, AvailableSpace, Bounds, ContentMask, Element, ElementId, Global,
    GlobalElementId, Hsla, InspectorElementId, IntoElement as _, LayoutId, Pixels, Position,
    SharedString, Style, Styled as _, Window, px, radians, size,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::Icon;

#[derive(Clone, Debug)]
struct Slot {
    bounds: Bounds<Pixels>,
    mask: ContentMask<Pixels>,
    color: Hsla,
    /// Alive while the element that recorded the slot is in the frame.
    token: Weak<()>,
}

impl Slot {
    /// Whether the slot is still drawn and some of it shows.
    fn visible(&self) -> bool {
        self.token.strong_count() > 0 && self.bounds.intersects(&self.mask.bounds)
    }
}

#[derive(Debug)]
struct Inner {
    slots: RefCell<Vec<Slot>>,
    /// Slots handed out since the last clear, to number their elements.
    next: Cell<usize>,
    shown: Cell<bool>,
}

/// The slots one view drew in its last render. Clear it at the start of
/// that view's render; the slots fill in again as its elements prepaint.
#[derive(Clone, Debug)]
pub struct SpinnerSlots(Rc<Inner>);

impl Default for SpinnerSlots {
    fn default() -> Self {
        Self(Rc::new(Inner {
            slots: RefCell::new(Vec::new()),
            next: Cell::new(0),
            shown: Cell::new(true),
        }))
    }
}

impl SpinnerSlots {
    /// Slots of a dock panel, registered with [`DockSpinners`] and hidden
    /// until the panel is shown.
    pub fn for_dock(cx: &mut App) -> Self {
        let slots = Self::default();
        slots.set_shown(false);
        cx.default_global::<DockSpinners>()
            .0
            .push(Rc::downgrade(&slots.0));
        slots
    }

    pub fn clear(&self) {
        self.0.slots.borrow_mut().clear();
        self.0.next.set(0);
    }

    /// Whether the view is on screen; set by a dock panel as its tab is
    /// displayed and hidden.
    pub fn set_shown(&self, shown: bool) {
        self.0.shown.set(shown);
    }

    pub fn is_shown(&self) -> bool {
        self.0.shown.get()
    }

    pub fn slot(&self, size: f32, color: Hsla) -> SpinnerSlot {
        let ix = self.0.next.get();
        self.0.next.set(ix + 1);
        // Unique per set and render, so each slot has its own state.
        let name = SharedString::from(format!("spinner-slot-{:p}", Rc::as_ptr(&self.0)));
        SpinnerSlot {
            slots: self.clone(),
            id: ElementId::NamedInteger(name, ix as u64),
            size: px(size),
            color,
        }
    }

    /// Where the spinners that would be painted now are, as (slot bounds,
    /// the mask they are clipped to): only slots still drawn and not
    /// entirely clipped away.
    pub fn visible(&self) -> Vec<(Bounds<Pixels>, Bounds<Pixels>)> {
        self.0
            .slots
            .borrow()
            .iter()
            .filter(|slot| slot.visible())
            .map(|slot| (slot.bounds, slot.mask.bounds))
            .collect()
    }

    /// Paints every recorded slot; put it last in the root view, which asks
    /// for the next frame while [`Self::is_empty`] is false.
    pub fn layer(&self) -> SpinnerLayer {
        SpinnerLayer {
            slots: self.clone(),
        }
    }

    /// Slots that are still drawn; dropped ones are forgotten.
    pub fn len(&self) -> usize {
        let mut slots = self.0.slots.borrow_mut();
        slots.retain(|slot| slot.token.strong_count() > 0);
        slots.len()
    }

    /// Whether no spinner would be painted: none drawn, or all clipped.
    pub fn is_empty(&self) -> bool {
        self.len() == 0 || self.visible().is_empty()
    }
}

/// The spinner slots of every dock panel.
#[derive(Default)]
pub struct DockSpinners(Vec<Weak<Inner>>);

impl Global for DockSpinners {}

impl DockSpinners {
    /// The slots of the panels that are shown now, forgetting dropped ones.
    pub fn shown(cx: &mut App) -> Vec<SpinnerSlots> {
        let registry = cx.default_global::<Self>();
        registry.0.retain(|slots| slots.strong_count() > 0);
        registry
            .0
            .iter()
            .filter_map(Weak::upgrade)
            .map(SpinnerSlots)
            .filter(SpinnerSlots::is_shown)
            .collect()
    }
}

/// Reserves the icon's space and records where it is.
pub struct SpinnerSlot {
    slots: SpinnerSlots,
    id: ElementId,
    size: Pixels,
    color: Hsla,
}

impl gpui::IntoElement for SpinnerSlot {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for SpinnerSlot {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = self.size.into();
        style.size.height = self.size.into();
        style.flex_shrink = 0.0;
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        _: &mut App,
    ) {
        let Some(id) = id else {
            return;
        };
        let mask = window.content_mask();
        let token = window.with_element_state::<Rc<()>, _>(id, |token, _| {
            let token = token.unwrap_or_default();
            (Rc::downgrade(&token), token)
        });
        let mut slots = self.slots.0.slots.borrow_mut();
        // A view laid out twice in one render records each slot once, at
        // its last place.
        slots.retain(|slot| !Weak::ptr_eq(&slot.token, &token));
        slots.push(Slot {
            bounds,
            mask,
            color: self.color,
            token,
        });
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        _: &mut Window,
        _: &mut App,
    ) {
    }
}

/// Paints the icons of a [`SpinnerSlots`] at their recorded places.
pub struct SpinnerLayer {
    slots: SpinnerSlots,
}

impl gpui::IntoElement for SpinnerLayer {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

/// One turn per second, in step for every spinner on screen.
fn angle() -> f32 {
    static START: OnceLock<Instant> = OnceLock::new();
    let elapsed = START.get_or_init(Instant::now).elapsed().as_secs_f32();
    elapsed.fract() * TAU
}

impl Element for SpinnerLayer {
    type RequestLayoutState = ();
    type PrepaintState = Vec<(AnyElement, ContentMask<Pixels>)>;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let style = Style {
            position: Position::Absolute,
            ..Style::default()
        };
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let angle = angle();
        let slots = self.slots.0.slots.borrow().clone();
        slots
            .into_iter()
            .filter(Slot::visible)
            .map(|slot| {
                let mut icon = Icon::new(IconName::LoaderCircle)
                    .size(slot.bounds.size.width)
                    .text_color(slot.color)
                    .rotate(radians(angle))
                    .into_any_element();
                window.with_content_mask(Some(slot.mask), |window| {
                    let space = size(
                        AvailableSpace::Definite(slot.bounds.size.width),
                        AvailableSpace::Definite(slot.bounds.size.height),
                    );
                    icon.prepaint_as_root(slot.bounds.origin, space, window, cx);
                });
                (icon, slot.mask)
            })
            .collect()
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        icons: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        for (icon, mask) in icons.iter_mut() {
            window.with_content_mask(Some(*mask), |window| icon.paint(window, cx));
        }
    }
}
