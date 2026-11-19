//! The sidebar's panels: Inbox, Stack, Checks, Threads, Files and Repos, one
//! [`SectionPanel`] each, in the left region of the app's one dock. They
//! start in one column, the inbox on top, the focused pull request's
//! sections under it and Repos folded at the bottom; the user can drag any
//! of them into a second column or into the main area beside the pull
//! request's tabs and back, fold a tab group to its header while it is in the
//! sidebar, and resize them. The layout saves all of it. Folding belongs to
//! the tab group: every section of a group folds and unfolds together.
//!
//! Each panel draws its content as a view of its own, from its cache, so a
//! frame of the diff beside it does not build it again.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};

use gpui::{
    AnyElement, AnyView, App, AppContext as _, Axis, Bounds, Context, Entity, EntityId,
    EventEmitter, FocusHandle, Focusable, FontWeight, Global, InteractiveElement as _, IntoElement,
    MouseButton, ParentElement, Pixels, Point, Render, ScrollHandle, SharedString,
    StatefulInteractiveElement as _, StyleRefinement, Styled, Subscription, WeakEntity, Window,
    canvas, div, prelude::FluentBuilder as _, px,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::{
    Icon, Placement, Sizable as _,
    button::{Button, ButtonVariants as _},
    dock::{
        BasePanel, DockArea, DockEvent, DockLayout, DockPlacement, InsertTarget, NodeId, PaneNode,
        PaneRef, Panel, PanelEvent, PanelId, PanelInfo, PanelState, TabGroupContext,
        register_panel,
    },
    h_flex,
    scroll::ScrollableElement as _,
    v_flex,
};
use serde::{Deserialize, Serialize};

use crate::inbox::refresh::{INBOX_KEYS, Refresh};
use crate::inbox::{FOLD_CONTEXT, SavedInbox, ToggleFold};
use crate::layout;
use crate::panels::tab_label;
use crate::sidebar::{Sidebar, SidebarEvent};
use crate::tab_menu::tab_handle;
use crate::ui::spinner::SpinnerSlots;
use crate::ui::{self, TextExt as _};

/// Starting heights of the sections in the default column; Files takes the
/// rest.
const INBOX_HEIGHT: f32 = 420.0;
const STACK_HEIGHT: f32 = 100.0;
/// Room for the failing and running jobs' rows and the summary line.
const CHECKS_HEIGHT: f32 = 64.0;
const THREADS_HEIGHT: f32 = 100.0;
/// The least a folded section's slot keeps when its tab bar was not
/// measured yet.
const FOLDED_MIN: f32 = 24.0;
/// The least the dock keeps of any slot (gpui-base's `PANEL_MIN_SIZE`).
const PANEL_MIN_SIZE: Pixels = px(100.0);
/// The regions a section can be in, the sidebar first.
const REGIONS: [DockPlacement; 4] = [
    DockPlacement::Left,
    DockPlacement::Center,
    DockPlacement::Right,
    DockPlacement::Bottom,
];

/// A section of the sidebar.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Section {
    Inbox,
    Stack,
    Checks,
    Threads,
    Files,
    Repos,
}

impl Section {
    pub const ALL: [Self; 6] = [
        Self::Inbox,
        Self::Stack,
        Self::Checks,
        Self::Threads,
        Self::Files,
        Self::Repos,
    ];

    /// The name the layout saves it under. The checks section is not
    /// "Checks": that is the dock tab with the runs.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Inbox => "Inbox",
            Self::Stack => "Stack",
            Self::Checks => "Rollup",
            Self::Threads => "Threads",
            Self::Files => "Files",
            Self::Repos => "Repos",
        }
    }

    /// What its tab reads.
    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            Self::Checks => "Checks",
            other => other.name(),
        }
    }

    /// Whether it shows the focused pull request.
    #[must_use]
    pub fn of_pr(self) -> bool {
        !matches!(self, Self::Inbox | Self::Repos)
    }

    /// Where it starts in the default column: its height (`None` takes what
    /// is left), and whether it starts folded.
    fn default_slot(self) -> (Option<f32>, bool) {
        match self {
            Self::Inbox => (Some(INBOX_HEIGHT), false),
            Self::Stack => (Some(STACK_HEIGHT), false),
            Self::Checks => (Some(CHECKS_HEIGHT), false),
            Self::Threads => (Some(THREADS_HEIGHT), false),
            Self::Files => (None, false),
            Self::Repos => (Some(FOLDED_MIN), true),
        }
    }
}

/// What the layout saves of a section panel.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct Saved {
    collapsed: bool,
    /// The slot height to go back to when unfolded.
    restore: Option<f32>,
    /// The inbox panel's sections and grouping.
    #[serde(skip_serializing_if = "Option::is_none")]
    inbox: Option<SavedInbox>,
}

pub struct SectionPanel {
    id: EntityId,
    section: Section,
    sidebar: Entity<Sidebar>,
    focus: FocusHandle,
    slots: SpinnerSlots,
    body: Entity<SectionBody>,
    collapsed: bool,
    restore: Option<f32>,
    /// The dock region it is in, as [`sync_placements`] last found it. Read
    /// while the dock draws, which cannot be asked then.
    placement: DockPlacement,
    /// Where the panel was drawn in the last frame, without its tab bar.
    bounds: Rc<Cell<Bounds<Pixels>>>,
    _sidebar: Subscription,
}

impl EventEmitter<PanelEvent> for SectionPanel {}

impl Focusable for SectionPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

pub fn register(cx: &mut App) {
    crate::inbox::refresh::init(cx);
    for section in Section::ALL {
        register_panel(cx, section.name(), move |build, window, cx| {
            let saved = match build.info() {
                PanelInfo::Panel(value) => {
                    serde_json::from_value(value.clone()).unwrap_or_default()
                }
                _ => Saved::default(),
            };
            let sidebar = Sidebar::global(cx);
            let dock = build.dock_area();
            sidebar.update(cx, |sidebar, _| {
                if sidebar.dock().is_none() {
                    sidebar.set_dock(dock);
                }
            });
            tab_handle(SectionPanel::with_saved(section, saved, window, cx))
        });
    }
}

impl SectionPanel {
    pub fn new(section: Section, window: &mut Window, cx: &mut App) -> Entity<Self> {
        let saved = Saved {
            collapsed: section.default_slot().1,
            ..Saved::default()
        };
        Self::with_saved(section, saved, window, cx)
    }

    fn with_saved(section: Section, saved: Saved, _: &mut Window, cx: &mut App) -> Entity<Self> {
        let sidebar = Sidebar::global(cx);
        if let Some(inbox) = saved.inbox {
            sidebar.update(cx, |sidebar, cx| sidebar.restore_inbox(inbox, cx));
        }
        // The inbox panel's focus is the sidebar's, which the shell hands
        // the keys to.
        let focus = (section == Section::Inbox).then(|| sidebar.read(cx).focus.clone());
        let slots = SpinnerSlots::for_dock(cx);
        let body = cx.new(|cx| SectionBody {
            section,
            _sidebar: cx.observe(&sidebar, |_, _, cx| cx.notify()),
            sidebar: sidebar.clone(),
            slots: slots.clone(),
            scroll: ScrollHandle::new(),
            look: Look::default(),
        });
        cx.new(|cx| Self {
            id: cx.entity_id(),
            section,
            focus: focus.unwrap_or_else(|| cx.focus_handle()),
            _sidebar: cx.observe(&sidebar, |_, _, cx| cx.notify()),
            sidebar,
            slots,
            body,
            collapsed: saved.collapsed,
            restore: saved.restore,
            placement: DockPlacement::Left,
            bounds: Rc::new(Cell::new(Bounds::default())),
        })
    }

    #[must_use]
    pub fn section(&self) -> Section {
        self.section
    }

    /// Whether it is folded to its header under the sidebar. A section
    /// outside the sidebar is never folded.
    #[must_use]
    pub fn is_collapsed(&self) -> bool {
        self.collapsed && self.in_sidebar()
    }

    /// Whether it is in the sidebar, the dock's left region.
    #[must_use]
    pub fn in_sidebar(&self) -> bool {
        self.placement == DockPlacement::Left
    }

    /// The spinner slots the section's rows draw into.
    #[must_use]
    pub fn spinners(&self) -> &SpinnerSlots {
        &self.slots
    }

    /// Whether the section has something to show, so it takes space.
    fn shows_something(&self, cx: &App) -> bool {
        let sidebar = self.sidebar.read(cx);
        match self.section {
            Section::Inbox | Section::Repos => true,
            Section::Stack => sidebar.has_stack(cx),
            Section::Checks | Section::Files => sidebar.has_pr(cx),
            Section::Threads => sidebar.has_threads(cx),
        }
    }

    /// Folds the section's tab group to its header under the sidebar, or
    /// unfolds it: see [`set_group_folded`]. Only in the sidebar: elsewhere
    /// it is a tab.
    pub fn toggle_collapsed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.in_sidebar() {
            return;
        }
        let collapsed = !self.collapsed;
        let panel = cx.entity();
        // The dock asks its panels about themselves while it changes, so the
        // change waits until none of them is being updated.
        window.defer(cx, move |window, cx| {
            set_group_folded(&panel, collapsed, None, window, cx);
        });
    }

    /// Whether this panel draws the pinned title of the focused pull
    /// request: it is the first shown pull request section in its column of
    /// the sidebar.
    fn owns_title(&self, cx: &App) -> bool {
        if !self.section.of_pr() || !self.in_sidebar() {
            return false;
        }
        let me = PanelId::from(self.id);
        let Some(dock) = self.sidebar.read(cx).dock() else {
            return false;
        };
        let dock = dock.read(cx);
        let Some(tree) = dock.layout(DockPlacement::Left) else {
            return false;
        };
        let root = tree.root();
        let columns: Vec<&PaneNode> = match root.kind() {
            PaneRef::Split {
                axis: Axis::Horizontal,
                children,
                ..
            } => children.iter().collect(),
            _ => vec![root],
        };
        columns.into_iter().any(|column| {
            let mut first = None;
            column.walk(&mut |node| {
                if first.is_some() {
                    return;
                }
                let PaneRef::Tabs { panels, active_ix } = node.kind() else {
                    return;
                };
                let Some(&id) = panels.get(active_ix) else {
                    return;
                };
                let shown_pr_section = id == me
                    || dock
                        .panel(id)
                        .and_then(|panel| panel.view().downcast::<Self>().ok())
                        .is_some_and(|panel| {
                            let panel = panel.read(cx);
                            panel.section.of_pr()
                                && panel.shows_something(cx)
                                && !panel.is_collapsed()
                        });
                if shown_pr_section {
                    first = Some(id);
                }
            });
            first == Some(me)
        })
    }
}

/// The button that folds a sidebar section's tab group to its header row
/// under the sidebar, drawn before the group's tabs. `None` when the group's
/// displayed panel is not a section in the sidebar.
pub fn fold_button(group: &TabGroupContext, cx: &App) -> Option<AnyElement> {
    let panel = group
        .active_panel()?
        .view()
        .downcast::<SectionPanel>()
        .ok()?;
    let this = panel.read(cx);
    if !this.in_sidebar() {
        return None;
    }
    let name = this.section.name();
    let (icon, tip) = if this.is_collapsed() {
        (IconName::ChevronsUpDown, "Unfold")
    } else {
        (IconName::ChevronsDownUp, "Fold")
    };
    let fold = Button::new("section-fold")
        .icon(icon)
        .xsmall()
        .ghost()
        .tab_stop(false)
        .tooltip_with_action(tip, &ToggleFold, Some(FOLD_CONTEXT))
        // As the section's name in the folded row, which unfolds it.
        .debug_selector(move || format!("section-toggle-{name}"))
        .on_click(move |_, window, cx| {
            panel.update(cx, |this, cx| this.toggle_collapsed(window, cx));
        });
    Some(fold.into_any_element())
}

/// The tab group of the sidebar holding `panel`: its panels in tab order and
/// the stored active tab.
fn tab_group(dock: &DockArea, panel: PanelId) -> Option<(Vec<PanelId>, usize)> {
    let tree = dock.layout(DockPlacement::Left)?;
    let node = tree.find_node(tree.find_panel_node(panel)?)?;
    let PaneRef::Tabs { panels, active_ix } = node.kind() else {
        return None;
    };
    Some((panels.to_vec(), active_ix))
}

/// The section panels of `panel`'s sidebar tab group, in tab order.
fn group_sections(dock: &DockArea, panel: PanelId) -> Vec<Entity<SectionPanel>> {
    let Some((panels, _)) = tab_group(dock, panel) else {
        return Vec::new();
    };
    panels
        .into_iter()
        .filter_map(|id| dock.panel(id)?.view().downcast::<SectionPanel>().ok())
        .collect()
}

/// Folds the tab group of `panel` to its header under the sidebar, or unfolds
/// it, showing `show`'s tab when given. The dock keeps a slot at least
/// `PANEL_MIN_SIZE` tall, so a folded group gives its slot up: the other
/// slots take its height and the group gets it back when unfolded. Call it
/// while neither the dock nor a section panel is being updated.
fn set_group_folded(
    panel: &Entity<SectionPanel>,
    collapsed: bool,
    show: Option<PanelId>,
    window: &mut Window,
    cx: &mut App,
) {
    let (me, sidebar) = {
        let panel = panel.read(cx);
        (PanelId::from(panel.id), panel.sidebar.clone())
    };
    let Some(dock) = sidebar.read(cx).dock() else {
        return;
    };
    let members = group_sections(dock.read(cx), me);
    if members.is_empty() {
        return;
    }
    let restore = members.iter().find_map(|member| member.read(cx).restore);
    for member in &members {
        member.update(cx, |member, cx| {
            member.collapsed = collapsed;
            cx.notify();
        });
    }
    let old = dock.update(cx, |dock, cx| {
        let (node, ix, mut sizes, open) = slot_sizes(dock, me, cx)?;
        let old = sizes[ix];
        // A folded slot is hidden, and everything it held goes to the open
        // ones; unfolded, it takes its old size back.
        sizes[ix] = if collapsed {
            px(FOLDED_MIN).min(old)
        } else {
            restore.map_or(PANEL_MIN_SIZE * 2.0, px)
        };
        let grown = sizes[ix] - old;
        give_or_take(&mut sizes, ix, grown, &open);
        dock.set_split_sizes(node, sizes, window, cx);
        Some(old)
    });
    let restore = if collapsed { old.map(f32::from) } else { None };
    for member in &members {
        member.update(cx, |member, _| member.restore = restore);
    }
    if let Some(show) = show.filter(|_| !collapsed) {
        dock.update(cx, |dock, cx| dock.select_panel(show, window, cx));
    }
    let stack = sidebar.update(cx, |sidebar, cx| sidebar.folded_stack(cx));
    stack.update(cx, |_, cx| cx.notify());
    sidebar.update(cx, |_, cx| cx.emit(SidebarEvent::LayoutChanged));
}

/// The split holding `panel`'s tab group in the sidebar, the group's place in
/// it, and the split's sizes as drawn.
fn slot_sizes(
    dock: &DockArea,
    panel: PanelId,
    cx: &App,
) -> Option<(NodeId, usize, Vec<Pixels>, Vec<bool>)> {
    let tree = dock.layout(DockPlacement::Left)?;
    let group = tree.find_panel_node(panel)?;
    let mut path = Vec::new();
    if !path_to(tree.root(), group, &mut path) {
        return None;
    }
    let (&ix, parent_path) = path.split_last()?;
    let node = node_at(tree.root(), parent_path)?;
    let sizes = drawn_sizes(dock, parent_path, cx)?;
    let PaneRef::Split { children, .. } = node.kind() else {
        return None;
    };
    let open = children
        .iter()
        .map(|slot| slot_is_open(dock, slot, cx))
        .collect();
    (ix < sizes.len()).then_some((node.id(), ix, sizes, open))
}

/// The node `path` leads to from `node`.
fn node_at<'a>(mut node: &'a PaneNode, path: &[usize]) -> Option<&'a PaneNode> {
    for &step in path {
        let PaneRef::Split { children, .. } = node.kind() else {
            return None;
        };
        node = children.get(step)?;
    }
    Some(node)
}

/// The slot sizes of the sidebar's split at `path`, as drawn: the tree keeps
/// what the user arranged, the dump what the split measured.
fn drawn_sizes(dock: &DockArea, path: &[usize], cx: &App) -> Option<Vec<Pixels>> {
    let state = dock.dump(cx);
    let mut saved = state.left_dock.as_ref()?.panel();
    for &step in path {
        saved = saved.children.get(step)?;
    }
    saved.info.sizes().cloned()
}

/// How far from a boundary a press still grabs it: gpui-base's handle band.
const HANDLE_REACH: Pixels = px(5.0);

/// A drag on the boundary between two open slots of a sidebar column.
///
/// gpui-base draws the handle above a slot on that slot, and its drag sizes
/// the slot right before it in the split, also when that slot is hidden (a
/// section with nothing to show, or a folded group): the drag then sizes a
/// slot that is not drawn, and nothing on screen moves. The sidebar takes
/// these drags over and moves the boundary between the open slots on either
/// side, the hidden ones keeping their sizes.
#[derive(Clone, Debug)]
pub struct SlotDrag {
    node: NodeId,
    /// The split's sizes when the drag began.
    sizes: Vec<Pixels>,
    /// The split's open slots in order: their place in it and drawn height.
    open: Vec<(usize, Pixels)>,
    /// The boundary is under `open[at]`.
    at: usize,
    /// Where the pointer was pressed.
    from: Pixels,
}

impl SlotDrag {
    /// The drag a press at `position` starts: one on the boundary between
    /// two open slots of a column of the sidebar.
    #[must_use]
    pub fn start(dock: &DockArea, position: Point<Pixels>, cx: &App) -> Option<Self> {
        if !dock.is_dock_open(DockPlacement::Left) {
            return None;
        }
        let tree = dock.layout(DockPlacement::Left)?;
        let mut found = None;
        visit_splits(tree.root(), &mut Vec::new(), &mut |node, path| {
            if found.is_none() {
                found = Self::on_boundary(dock, node, path, position, cx);
            }
        });
        found
    }

    fn on_boundary(
        dock: &DockArea,
        node: &PaneNode,
        path: &[usize],
        position: Point<Pixels>,
        cx: &App,
    ) -> Option<Self> {
        let PaneRef::Split {
            axis: Axis::Vertical,
            children,
            ..
        } = node.kind()
        else {
            return None;
        };
        let mut open = Vec::new();
        for (ix, child) in children.iter().enumerate() {
            match slot_bounds(dock, child, cx) {
                SlotBounds::Hidden => {}
                SlotBounds::Open(bounds) => open.push((ix, bounds)),
                // Without a section to measure it by, the dock's own handle
                // keeps the split.
                SlotBounds::Unknown => return None,
            }
        }
        let at = open.windows(2).position(|pair| {
            let above = pair[0].1;
            (position.y - above.bottom()).abs() <= HANDLE_REACH
                && (above.left()..above.right()).contains(&position.x)
        })?;
        // A slot is its tab bar and its content; the bars are alike.
        let tab_bar = open[at + 1].1.top() - open[at].1.bottom();
        let open: Vec<(usize, Pixels)> = open
            .iter()
            .map(|&(ix, bounds)| (ix, bounds.size.height + tab_bar))
            .collect();
        let mut sizes = drawn_sizes(dock, path, cx)?;
        if sizes.len() != children.len() {
            return None;
        }
        // The dock scales the sizes it is given to fill the split, so they
        // must add up to what the open slots draw or every slot jumps when the
        // drag begins. The last open slot also draws the room the hidden ones
        // hold.
        let (&(last, _), rest) = open.split_last()?;
        for &(ix, height) in rest {
            sizes[ix] = height;
        }
        let filled: Pixels = open.iter().map(|&(_, height)| height).sum();
        let others: Pixels = (0..sizes.len())
            .filter(|&ix| ix != last)
            .map(|ix| sizes[ix])
            .sum();
        sizes[last] = (filled - others).max(Pixels::ZERO);
        Some(Self {
            node: node.id(),
            sizes,
            open,
            at,
            from: position.y,
        })
    }

    /// The split's sizes with the boundary moved to `y`. The slot that grows
    /// takes what the open slots on the other side can give, the nearest
    /// first, none below the dock's least; hidden slots keep their sizes.
    fn sizes_at(&self, y: Pixels) -> Vec<Pixels> {
        let moved = y - self.from;
        let (above, below) = self.open.split_at(self.at + 1);
        let (grows, givers): (usize, Vec<(usize, Pixels)>) = if moved > Pixels::ZERO {
            (above[above.len() - 1].0, below.to_vec())
        } else {
            (below[0].0, above.iter().rev().copied().collect())
        };
        let mut sizes = self.sizes.clone();
        let mut owed = moved.abs();
        for (ix, height) in givers {
            // The last open slot also draws what the hidden ones hold, which
            // is more than its own size.
            let give = (height - PANEL_MIN_SIZE)
                .min(sizes[ix])
                .max(Pixels::ZERO)
                .min(owed);
            sizes[ix] -= give;
            owed -= give;
        }
        sizes[grows] += moved.abs() - owed;
        sizes
    }

    /// Marks the drag as begun, see [`dragging_boundary`].
    pub fn begin(&self, cx: &mut App) {
        cx.set_global(BoundaryDrag(true));
    }

    /// Ends a drag, let go or lost: the dock reports its layout changed once
    /// more, now for the shell to save.
    pub fn finish(dock: &Entity<DockArea>, cx: &mut App) {
        cx.set_global(BoundaryDrag(false));
        dock.update(cx, |_, cx| cx.emit(DockEvent::LayoutChanged));
    }

    /// Moves the boundary to the pointer at `y`.
    pub fn drag_to(&self, y: Pixels, dock: &Entity<DockArea>, window: &mut Window, cx: &mut App) {
        let sizes = self.sizes_at(y);
        dock.update(cx, |dock, cx| {
            dock.set_split_sizes(self.node, sizes, window, cx);
        });
    }
}

/// Whether a sidebar boundary is being dragged.
#[derive(Default)]
struct BoundaryDrag(bool);

impl Global for BoundaryDrag {}

/// Whether a boundary of the sidebar is being dragged. The dock reports each
/// step of the drag as a layout change, which only resizes two slots: the
/// shell saves the layout and redraws the sections once the drag ends.
#[must_use]
pub fn dragging_boundary(cx: &App) -> bool {
    cx.try_global::<BoundaryDrag>().is_some_and(|drag| drag.0)
}

/// Calls `f` with every split under `node` and the child indices leading to
/// it, `node` first.
fn visit_splits(node: &PaneNode, path: &mut Vec<usize>, f: &mut impl FnMut(&PaneNode, &[usize])) {
    let PaneRef::Split { children, .. } = node.kind() else {
        return;
    };
    f(node, path);
    for (ix, child) in children.iter().enumerate() {
        path.push(ix);
        visit_splits(child, path, f);
        path.pop();
    }
}

/// Where a slot of a sidebar split is drawn.
enum SlotBounds {
    /// The dock hides it: nothing in it is visible.
    Hidden,
    /// The content of its shown sections, under their tab bars.
    Open(Bounds<Pixels>),
    /// It shows a panel that is not a section, which does not measure itself.
    Unknown,
}

fn slot_bounds(dock: &DockArea, slot: &PaneNode, cx: &App) -> SlotBounds {
    match slot.kind() {
        PaneRef::Tabs { panels, active_ix } => {
            let visible = |id: &&PanelId| dock.panel(**id).is_some_and(|panel| panel.visible(cx));
            // The dock draws the active tab, or the first visible one when the
            // active one is not.
            let shown = panels
                .get(active_ix)
                .filter(visible)
                .or_else(|| panels.iter().find(visible));
            let Some(&shown) = shown else {
                return SlotBounds::Hidden;
            };
            dock.panel(shown)
                .and_then(|panel| panel.view().downcast::<SectionPanel>().ok())
                .map_or(SlotBounds::Unknown, |panel| {
                    SlotBounds::Open(panel.read(cx).bounds.get())
                })
        }
        PaneRef::Split { children, .. } => {
            children.iter().fold(SlotBounds::Hidden, |all, child| {
                match (all, slot_bounds(dock, child, cx)) {
                    (SlotBounds::Unknown, _) | (_, SlotBounds::Unknown) => SlotBounds::Unknown,
                    (SlotBounds::Hidden, one) | (one, SlotBounds::Hidden) => one,
                    (SlotBounds::Open(a), SlotBounds::Open(b)) => SlotBounds::Open(a.union(&b)),
                }
            })
        }
    }
}

/// Whether a slot of the sidebar's split shows its body: its shown tab is a
/// section with something to show that is not folded. A slot holding anything
/// else counts as open.
fn slot_is_open(dock: &DockArea, slot: &PaneNode, cx: &App) -> bool {
    let mut open = true;
    let mut found = false;
    slot.walk(&mut |node| {
        if found {
            return;
        }
        let PaneRef::Tabs { panels, active_ix } = node.kind() else {
            return;
        };
        found = true;
        open = panels
            .get(active_ix)
            .and_then(|&id| dock.panel(id))
            .and_then(|panel| panel.view().downcast::<SectionPanel>().ok())
            .is_none_or(|panel| {
                let panel = panel.read(cx);
                !panel.is_collapsed() && panel.shows_something(cx)
            });
    });
    open
}

/// Takes `grown` from the open slots other than `ix`, the last first and
/// none below the dock's least, or gives a shrink to the first open one, so
/// the split keeps its height and a folded section never keeps room.
fn give_or_take(sizes: &mut [Pixels], ix: usize, grown: Pixels, open: &[bool]) {
    let others = || (0..sizes.len()).filter(|&other| other != ix);
    let is_open = |other: usize| open.get(other).copied().unwrap_or(true);
    if grown <= Pixels::ZERO {
        let Some(to) = others()
            .find(|&other| is_open(other))
            .or_else(|| others().next_back())
        else {
            return;
        };
        sizes[to] -= grown;
        return;
    }
    let mut owed = grown;
    for other in others()
        .rev()
        .filter(|&other| is_open(other))
        .collect::<Vec<_>>()
    {
        let spare = (sizes[other] - PANEL_MIN_SIZE).max(Pixels::ZERO).min(owed);
        sizes[other] -= spare;
        owed -= spare;
    }
}

/// The child indices from `node` down to `target`.
fn path_to(node: &PaneNode, target: NodeId, path: &mut Vec<usize>) -> bool {
    if node.id() == target {
        return true;
    }
    let PaneRef::Split { children, .. } = node.kind() else {
        return false;
    };
    for (ix, child) in children.iter().enumerate() {
        path.push(ix);
        if path_to(child, target, path) {
            return true;
        }
        path.pop();
    }
    false
}

/// One column of `sections`: the inbox on top, the focused pull request's
/// sections under it, Files taking what is left, Repos folded at the bottom.
#[must_use]
pub fn default_layout(sections: &[Section], window: &mut Window, cx: &mut App) -> DockLayout {
    sections
        .iter()
        .fold(DockLayout::v_split(), |split, &section| {
            let panel = tab_handle(SectionPanel::new(section, window, cx));
            let size = section.default_slot().0.map(px);
            split.child(DockLayout::tabs().panel_view(panel, cx), size)
        })
}

/// Points the sidebar at `dock`, and puts every section the dock does not
/// hold yet in its left region: the default column on a first start, or
/// under what a saved sidebar holds (a section newer than the layout).
pub fn install(dock: &Entity<DockArea>, window: &mut Window, cx: &mut App) {
    let sidebar = Sidebar::global(cx);
    sidebar.update(cx, |sidebar, _| sidebar.set_dock(dock.downgrade()));
    let missing: Vec<Section> = Section::ALL
        .into_iter()
        .filter(|section| section_panel(dock.read(cx), *section, cx).is_none())
        .collect();
    if missing.is_empty() {
        return;
    }
    let left_root = dock
        .read(cx)
        .layout(DockPlacement::Left)
        .filter(|tree| tree.panels().next().is_some())
        .map(|tree| tree.root().id());
    let Some(root) = left_root else {
        let fresh = !dock.read(cx).has_dock(DockPlacement::Left);
        let layout = default_layout(&missing, window, cx);
        dock.update(cx, |dock, cx| {
            dock.set_dock(DockPlacement::Left, layout, window, cx);
            if fresh {
                dock.set_dock_size(DockPlacement::Left, layout::SIDEBAR_WIDTH, window, cx);
            }
        });
        return;
    };
    for section in missing {
        let handle = tab_handle(SectionPanel::new(section, window, cx));
        let id = handle.panel_id(cx);
        let size = section.default_slot().0.map(px);
        dock.update(cx, |dock, cx| {
            dock.add_panel_view(handle, DockPlacement::Left, None, window, cx);
            let target = InsertTarget::Split {
                node: root,
                placement: Placement::Bottom,
                size,
            };
            dock.move_panel(id, target, window, cx);
        });
    }
}

/// Tells each section which region of `dock` it is in now. A section that
/// leaves the sidebar unfolds: folding is the sidebar's. A sidebar tab group
/// whose sections disagree about being folded (a saved layout from before
/// groups folded as one, or a tab dragged between groups) unfolds. The folded
/// strip learns the groups.
pub fn sync_placements(dock: &Entity<DockArea>, cx: &mut App) {
    let found: Vec<(Entity<SectionPanel>, DockPlacement)> = {
        let dock = dock.read(cx);
        REGIONS
            .into_iter()
            .filter_map(|region| Some((region, dock.layout(region)?)))
            .flat_map(|(region, tree)| tree.panels().map(move |id| (region, id)))
            .filter_map(|(region, id)| {
                let panel = dock.panel(id)?.view().downcast::<SectionPanel>().ok()?;
                Some((panel, region))
            })
            .collect()
    };
    for (panel, region) in found {
        panel.update(cx, |panel, cx| {
            if panel.placement == region {
                return;
            }
            panel.placement = region;
            if region != DockPlacement::Left && panel.collapsed {
                panel.collapsed = false;
                panel.restore = None;
            }
            cx.notify();
        });
    }
    let groups = sidebar_groups(dock.read(cx));
    for group in &groups {
        let folded = group.iter().filter(|p| p.read(cx).collapsed).count();
        if folded == 0 || folded == group.len() {
            continue;
        }
        for panel in group {
            panel.update(cx, |panel, cx| {
                panel.collapsed = false;
                panel.restore = None;
                cx.notify();
            });
        }
    }
    let stack = Sidebar::global(cx).update(cx, |sidebar, cx| sidebar.folded_stack(cx));
    stack.update(cx, |stack, cx| {
        let groups = groups
            .iter()
            .map(|group| group.iter().map(Entity::downgrade).collect())
            .collect();
        stack.set_groups(groups, cx);
    });
}

/// The sidebar's tab groups in layout order, each as its section panels in
/// tab order.
fn sidebar_groups(dock: &DockArea) -> Vec<Vec<Entity<SectionPanel>>> {
    let Some(tree) = dock.layout(DockPlacement::Left) else {
        return Vec::new();
    };
    let mut groups = Vec::new();
    tree.root().walk(&mut |node| {
        let PaneRef::Tabs { panels, .. } = node.kind() else {
            return;
        };
        let group: Vec<_> = panels
            .iter()
            .filter_map(|&id| dock.panel(id)?.view().downcast::<SectionPanel>().ok())
            .collect();
        if !group.is_empty() {
            groups.push(group);
        }
    });
    groups
}

/// `--demo-inbox`: unfolds `section` in the sidebar, in the first window.
pub fn unfold(section: Section, cx: &mut App) {
    let Some(dock) = Sidebar::global(cx).read(cx).dock() else {
        return;
    };
    let Some(panel) = section_panel(dock.read(cx), section, cx)
        .and_then(|id| dock.read(cx).panel(id).cloned())
        .and_then(|panel| panel.view().downcast::<SectionPanel>().ok())
    else {
        return;
    };
    let Some(window) = cx.windows().first().copied() else {
        return;
    };
    let _ = window.update(cx, |_, window, cx| {
        if panel.read(cx).is_collapsed() {
            let id = PanelId::from(panel.entity_id());
            set_group_folded(&panel, false, Some(id), window, cx);
        }
    });
}

impl BasePanel for SectionPanel {
    fn panel_name(&self) -> &'static str {
        self.section.name()
    }

    /// A folded section gives its slot up: its header is in the
    /// [`FoldedStack`] under the sidebar, which the dock cannot make smaller
    /// than its least slot.
    fn visible(&self, cx: &App) -> bool {
        self.shows_something(cx) && !self.is_collapsed()
    }

    fn closable(&self, _: &App) -> bool {
        false
    }

    fn set_active(&mut self, active: bool, _: &mut Window, cx: &mut Context<Self>) {
        self.slots.set_shown(active);
        if self.section == Section::Inbox {
            self.sidebar.read(cx).inbox.refresh_slots.set_shown(active);
        }
        // Reaches the shell, which then asks for frames while icons spin.
        cx.notify();
    }

    fn on_removed(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.slots.set_shown(false);
        if self.section == Section::Inbox {
            self.sidebar.read(cx).inbox.refresh_slots.set_shown(false);
        }
    }

    fn dump(&self, cx: &App) -> PanelState {
        let saved = Saved {
            collapsed: self.collapsed,
            restore: self.restore,
            inbox: (self.section == Section::Inbox).then(|| self.sidebar.read(cx).saved_inbox()),
        };
        PanelState {
            panel_name: self.section.name().to_owned(),
            children: Vec::new(),
            info: PanelInfo::panel(serde_json::to_value(saved).unwrap_or_default()),
        }
    }
}

impl Panel for SectionPanel {
    /// The section's name and how much is in it. A click on it only selects
    /// the tab; in the sidebar the tab also takes the fold key.
    fn title(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = ui::theme(cx);
        let c = &t.colors;
        let note = self.sidebar.read(cx).note(self.section, cx);
        let (group, refresh) = if self.section == Section::Inbox {
            self.sidebar.update(cx, |sidebar, cx| {
                (
                    Some(sidebar.group_toggle(&t, cx)),
                    sidebar.refresh_button(&t, cx),
                )
            })
        } else {
            (None, None)
        };
        let name = self.section.name();
        let hover = c.hover;
        h_flex()
            .id(SharedString::from(format!("section-title-{name}")))
            .debug_selector(move || format!("section-title-{name}"))
            .w_full()
            .h_full()
            .gap(px(6.0))
            .items_center()
            .cursor_pointer()
            .when(self.in_sidebar(), |el| {
                el.focusable()
                    .focus_visible(move |s| s.bg(hover))
                    .key_context(FOLD_CONTEXT)
                    .on_action(cx.listener(|this, _: &ToggleFold, window, cx| {
                        this.toggle_collapsed(window, cx);
                    }))
            })
            .child(tab_label(div()).child(self.section.title()))
            .when(!note.is_empty(), |el| {
                el.child(
                    div()
                        .flex_none()
                        .mono(&t)
                        .tiny(&t)
                        .text_color(c.text_muted)
                        .child(note),
                )
            })
            .children(group)
            .children(refresh)
    }

    fn inner_padding(&self, _: &App) -> bool {
        false
    }
}

/// How often a sidebar section panel was rendered, for the benchmarks.
pub static PANEL_RENDERS: AtomicUsize = AtomicUsize::new(0);

impl Render for SectionPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        PANEL_RENDERS.fetch_add(1, Ordering::Relaxed);
        let t = ui::theme(cx);
        let c = &t.colors;
        let collapsed = self.is_collapsed();
        let look = Look {
            collapsed,
            owns_title: !collapsed && self.owns_title(cx),
        };
        self.body.update(cx, |body, cx| {
            if body.look != look {
                body.look = look;
                cx.notify();
            }
        });
        let drawn = self.bounds.clone();
        let measure = canvas(move |bounds, _, _| drawn.set(bounds), |_, (), _, _| {})
            .absolute()
            .size_full();
        let name = self.section.name();
        // The search boxes take typing, so they are drawn here and not in
        // the cached body.
        let action_line = match (collapsed, self.section) {
            (false, Section::Inbox) => self
                .sidebar
                .update(cx, |sidebar, cx| sidebar.row_action_line(cx)),
            _ => None,
        };
        let search = match (collapsed, self.section) {
            (false, Section::Inbox) => Some(
                self.sidebar
                    .update(cx, |sidebar, cx| sidebar.filter_row(window, cx)),
            ),
            (false, Section::Repos) => Some(
                self.sidebar
                    .update(cx, |sidebar, cx| sidebar.repos_search_row(window, cx)),
            ),
            _ => None,
        };
        let root = v_flex()
            .id(name)
            .debug_selector(move || format!("section-{name}"))
            .relative()
            .size_full()
            .bg(c.surface)
            .font_family(t.fonts.interface.family())
            .text_size(t.fonts.interface.size_px())
            .track_focus(&self.focus)
            .on_mouse_down(
                MouseButton::Left,
                // A click inside keeps focus where it landed (a search box).
                cx.listener(|this, _, window, cx| {
                    if !this.focus.contains_focused(window, cx) {
                        window.focus(&this.focus, cx);
                    }
                }),
            )
            .child(measure)
            .children(search)
            .children(action_line)
            // Drawn again only when the sidebar changes, not on every frame
            // the window draws (a diff scrolling beside it).
            .child(
                AnyView::from(self.body.clone())
                    .cached(StyleRefinement::default().flex_1().min_h_0().w_full()),
            );
        let inbox = self.section == Section::Inbox;
        Sidebar::key_handlers(root, &self.sidebar).when(inbox, |el| {
            // The sidebar's keys, and the inbox's own.
            el.key_context(format!("{} {INBOX_KEYS}", crate::sidebar::KEY_CONTEXT).as_str())
                .on_action(|_: &Refresh, _, cx| crate::inbox::refresh::refresh(cx))
        })
    }
}

/// How a section's body is drawn: folded, and whether it pins the title.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Look {
    collapsed: bool,
    owns_title: bool,
}

/// A section's content, a view of its own so the dock can draw it from
/// its cache.
struct SectionBody {
    section: Section,
    sidebar: Entity<Sidebar>,
    slots: SpinnerSlots,
    scroll: ScrollHandle,
    look: Look,
    _sidebar: Subscription,
}

impl Render for SectionBody {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Only here: a frame drawn from the cache keeps the last slots.
        self.slots.clear();
        let t = ui::theme(cx);
        let c = &t.colors;
        let (section, slots, look) = (self.section, self.slots.clone(), self.look);
        let (title, content, note) = self.sidebar.update(cx, |sidebar, cx| {
            sidebar.spinners = slots;
            let title = look.owns_title.then(|| sidebar.title_row(&t, cx)).flatten();
            let content: Option<AnyElement> = match (look.collapsed, section) {
                (true, _) => None,
                (false, Section::Inbox) => Some(sidebar.inbox_body(window, cx)),
                (false, Section::Stack) => sidebar.stack_section(&t, cx),
                (false, Section::Checks) => sidebar.checks_section(&t, cx),
                (false, Section::Threads) => sidebar.threads_section(&t, cx),
                (false, Section::Files) => sidebar.files_section(&t, cx),
                (false, Section::Repos) => Some(sidebar.repos_body(&t, cx)),
            };
            (title, content, sidebar.note(section, cx))
        });
        let body = match (look.collapsed, section) {
            // The dock keeps a slot at least its minimum tall, so a folded
            // section says in one line what it holds.
            (true, _) => div()
                .px(px(10.0))
                .py(px(6.0))
                .small(&t)
                .text_color(c.text_muted)
                .child(note)
                .into_any_element(),
            (false, Section::Inbox) => content.unwrap_or_else(|| div().into_any_element()),
            (false, _) => div()
                .id("section-scroll")
                .flex_1()
                .min_h_0()
                .track_scroll(&self.scroll)
                .overflow_y_scroll()
                .children(content)
                .vertical_scrollbar(&self.scroll)
                .into_any_element(),
        };
        v_flex().size_full().children(title).child(body)
    }
}

/// The panel showing `section`, wherever in `dock` it is.
#[must_use]
pub fn section_panel(dock: &DockArea, section: Section, cx: &App) -> Option<PanelId> {
    REGIONS
        .into_iter()
        .filter_map(|region| dock.layout(region))
        .flat_map(|tree| tree.panels())
        .find(|id| {
            dock.panel(*id)
                .and_then(|panel| panel.view().downcast::<SectionPanel>().ok())
                .is_some_and(|panel| panel.read(cx).section == section)
        })
}

/// Moves the Checks section into a second column of the sidebar, right of
/// the one the inbox is in: what dropping its tab on the sidebar's right
/// edge does.
pub fn checks_beside_inbox(dock: &Entity<DockArea>, window: &mut Window, cx: &mut App) {
    dock.update(cx, |dock, cx| {
        let Some(checks) = section_panel(dock, Section::Checks, cx) else {
            return;
        };
        let Some(node) = dock
            .layout(DockPlacement::Left)
            .map(|tree| tree.root().id())
        else {
            return;
        };
        let target = InsertTarget::Split {
            node,
            placement: Placement::Right,
            size: None,
        };
        dock.move_panel(checks, target, window, cx);
    });
}

/// The headers of the folded tab groups, stacked under the sidebar: a row per
/// group, with each section's name and note in tab order. A folded group
/// leaves the dock's resizable slots altogether, so it never holds space the
/// open ones could use.
pub struct FoldedStack {
    sidebar: Entity<Sidebar>,
    /// The sidebar's tab groups in layout order, as [`sync_placements`] last
    /// found them: the dock cannot be read while it draws this.
    groups: Vec<Vec<WeakEntity<SectionPanel>>>,
    _sidebar: Subscription,
}

impl FoldedStack {
    pub(crate) fn new(sidebar: Entity<Sidebar>, cx: &mut Context<Self>) -> Self {
        Self {
            _sidebar: cx.observe(&sidebar, |_, _, cx| cx.notify()),
            sidebar,
            groups: Vec::new(),
        }
    }

    fn set_groups(&mut self, groups: Vec<Vec<WeakEntity<SectionPanel>>>, cx: &mut Context<Self>) {
        let ids = |groups: &[Vec<WeakEntity<SectionPanel>>]| -> Vec<Vec<EntityId>> {
            groups
                .iter()
                .map(|group| {
                    group
                        .iter()
                        .filter_map(|weak| weak.upgrade().map(|panel| panel.entity_id()))
                        .collect()
                })
                .collect()
        };
        if ids(&self.groups) != ids(&groups) {
            self.groups = groups;
            cx.notify();
        }
    }

    /// The folded groups it lists, in the sidebar's order, each as the
    /// sections that show something in tab order.
    fn folded(&self, cx: &App) -> Vec<Vec<Entity<SectionPanel>>> {
        self.groups
            .iter()
            .map(|group| {
                group
                    .iter()
                    .filter_map(WeakEntity::upgrade)
                    .filter(|panel| panel.read(cx).shows_something(cx))
                    .collect::<Vec<_>>()
            })
            .filter(|group| !group.is_empty() && group.iter().all(|p| p.read(cx).is_collapsed()))
            .collect()
    }

    /// How tall it draws: a row per folded group and the border over them.
    /// The dock draws it from its cache, which takes the size given.
    #[must_use]
    pub fn height(&self, cx: &App) -> Pixels {
        match self.folded(cx).len() {
            0 => Pixels::ZERO,
            rows => px(crate::sidebar::ROW * rows as f32 + 1.0),
        }
    }
}

impl Render for FoldedStack {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = ui::theme(cx);
        let c = &t.colors;
        let hover = c.hover;
        let rows: Vec<AnyElement> = self
            .folded(cx)
            .into_iter()
            .map(|group| {
                let first = group[0].clone();
                let first_name = first.read(cx).section.name();
                let (click, key) = (first.clone(), first);
                let sections: Vec<AnyElement> = group
                    .iter()
                    .map(|panel| {
                        let section = panel.read(cx).section;
                        let name = section.name();
                        let note = self.sidebar.read(cx).note(section, cx);
                        let (panel, id) = (panel.clone(), PanelId::from(panel.entity_id()));
                        h_flex()
                            .id(SharedString::from(format!("section-folded-{name}")))
                            .debug_selector(move || format!("section-toggle-{name}"))
                            .min_w_0()
                            .h_full()
                            .gap(px(6.0))
                            .items_center()
                            .cursor_pointer()
                            .hover(move |s| s.bg(hover))
                            // Unfolds the group with this tab shown.
                            .on_click(move |_, window, cx| {
                                cx.stop_propagation();
                                let panel = panel.clone();
                                window.defer(cx, move |window, cx| {
                                    set_group_folded(&panel, false, Some(id), window, cx);
                                });
                            })
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(c.text_secondary)
                                    .child(section.title()),
                            )
                            .when(!note.is_empty(), |el| {
                                el.child(
                                    div()
                                        .flex_none()
                                        .mono(&t)
                                        .tiny(&t)
                                        .text_color(c.text_muted)
                                        .child(note),
                                )
                            })
                            .into_any_element()
                    })
                    .collect();
                h_flex()
                    .id(SharedString::from(format!(
                        "section-folded-group-{first_name}"
                    )))
                    .debug_selector(move || format!("section-folded-group-{first_name}"))
                    .h(px(crate::sidebar::ROW))
                    .w_full()
                    .flex_none()
                    .overflow_hidden()
                    .px(px(10.0))
                    .gap(px(6.0))
                    .items_center()
                    .cursor_pointer()
                    .on_click(move |_, window, cx| {
                        let click = click.clone();
                        window.defer(cx, move |window, cx| {
                            set_group_folded(&click, false, None, window, cx);
                        });
                    })
                    .focusable()
                    .focus_visible(move |s| s.bg(hover))
                    .key_context(FOLD_CONTEXT)
                    .on_action(move |_: &ToggleFold, window, cx| {
                        set_group_folded(&key, false, None, window, cx);
                    })
                    .child(
                        Icon::new(IconName::ChevronRight)
                            .size(px(12.0))
                            .text_color(c.text_muted),
                    )
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .gap(px(14.0))
                            .items_center()
                            .overflow_hidden()
                            .children(sections),
                    )
                    .into_any_element()
            })
            .collect();
        v_flex()
            .w_full()
            .flex_none()
            .bg(c.surface)
            .when(!rows.is_empty(), |el| {
                el.border_t_1().border_color(c.border)
            })
            .children(rows)
    }
}
