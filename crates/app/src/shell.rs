//! The window: title bar, the dock and the status bar. The dock's left
//! region is the sidebar (the inbox and the focused pull request's
//! sections), its center the pull requests' tabs; a tab moves between them
//! like between any two groups.

use std::path::PathBuf;
use std::time::Duration;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, FocusHandle, Focusable as _, FontWeight,
    Global, InteractiveElement as _, IntoElement, KeyContext, MouseButton, ParentElement, Render,
    StatefulInteractiveElement as _, Styled, Subscription, Task, WeakEntity, Window, actions, div,
    px,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::{
    Icon, Placement,
    dock::{
        DockArea, DockAreaState, DockEvent, DockPlacement, InsertTarget, NodeId, PaneRef, PaneTree,
        PanelId, ToggleZoom,
    },
    h_flex, v_flex,
};

use diff_view::FrameClock;

use crate::appearance;
use crate::commands::{self, Group};
use crate::layout;
use crate::model::PrKey;
use crate::palette::{Closed, Item, Mode, Palette};
use crate::panels::{ChecksPanel, ConversationPanel, DiffPanel, LogPanel, PanelKind};
use crate::session::{ForgeState, OpenMode, Session, SessionEvent, ShowPanel};
use crate::settings_panel::SettingsPanel;
use crate::sidebar::{Sidebar, SidebarEvent};
use crate::sidebar_panels;
use crate::subscriptions::Subscriptions;
use crate::tab_menu::{self, TabAction, tab_handle};
use crate::terminal_panel::TerminalPanel;
use crate::terminals::{NewTerminal, Purpose, TerminalSpec, Terminals, TerminalsEvent, Where};
use crate::ui::spinner::{DockSpinners, SpinnerSlots};
use crate::ui::{self, TextExt as _};
use crate::watching::NoticeKind;

actions!(
    shell,
    [
        ToggleSidebar,
        ToggleDiffLayout,
        ToggleKeyHelp,
        CloseKeyHelp,
        OpenSettings,
        /// `Ctrl+W`: closes the tab that has focus, or the one shown last.
        CloseTab,
        /// `Ctrl+Tab`: the tab to the right in the focused group, wrapping.
        NextTab,
        /// `Ctrl+Shift+Tab`: the tab to the left, wrapping.
        PrevTab,
        /// `Esc` while a tab group fills the dock: puts the dock back.
        ZoomOut,
        /// `Ctrl+Shift+P`: every command, by fuzzy name.
        OpenCommandPalette,
        /// `Ctrl+P`: go to a pull request or a file by fuzzy name.
        QuickOpen
    ]
);

pub const KEY_CONTEXT: &str = "Shell";
/// The `?` overlay, which holds focus while it is open.
pub const KEY_HELP_CONTEXT: &str = "KeyHelp";
/// On the shell beside [`KEY_CONTEXT`] while a tab group fills the dock.
pub const ZOOMED_CONTEXT: &str = "Zoomed";
const SAVE_DEBOUNCE: Duration = Duration::from_millis(400);
/// Width of the Log's group when it first splits off to the right.
const LOG_WIDTH: f32 = 340.0;

/// The window's shell, for the dock tabs' menus.
struct ShellHandle(WeakEntity<Shell>);

impl Global for ShellHandle {}

/// Drives a benchmark from inside the window's own frames: the shell starts
/// the clock first thing in its render, lets the probe act on the window, and
/// ends the clock in a deferred element after everything else has painted.
pub trait Probe {
    /// The clock timing the frame being drawn now.
    fn clock(&self) -> &FrameClock;
    fn frame(&mut self, window: &mut Window, cx: &mut Context<Shell>);
}

pub struct Shell {
    probe: Option<Box<dyn Probe>>,
    session: Entity<Session>,
    sidebar: Entity<Sidebar>,
    /// Everything docked: the sidebar's sections on the left, the tabs in
    /// the center.
    dock: Entity<DockArea>,
    /// The sidebar came with the layout the dock was loaded with, so an
    /// older build's separate sidebar file is not read over it.
    sidebar_restored: bool,
    /// The sidebar's arrangement when the sections last heard of it.
    sidebar_tree: Option<PaneTree>,
    persist: bool,
    /// Where a persisted layout is saved; `None` is the user's data directory.
    layout_file: Option<PathBuf>,
    save_task: Option<Task<()>>,
    key_help: Option<KeyHelp>,
    /// The activity list over the status bar is open.
    activity: bool,
    /// The tab brought forward last, which `Ctrl+W` closes when no tab
    /// has focus.
    last_shown: Option<PanelId>,
    /// The pull request tab brought forward last: new pull requests open in
    /// its group.
    last_pr_panel: Option<PanelId>,
    /// The open command palette or quick open.
    palette: Option<Entity<Palette>>,
    _palette: Option<Subscription>,
}

/// The open `?` overlay: its focus, and where focus goes back on close.
struct KeyHelp {
    focus: FocusHandle,
    previous: Option<FocusHandle>,
}

impl Shell {
    pub fn new(
        dock: Entity<DockArea>,
        persist: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        appearance::follow_os(window, cx);
        let session = Session::global(cx);
        cx.observe(&session, |_, _, cx| cx.notify()).detach();
        // Opens bring their panel forward, building it when missing; a
        // thread or a file picked in the sidebar shows in the Diff panel.
        cx.subscribe_in(
            &session,
            window,
            |this: &mut Self, _, event: &SessionEvent, window, cx| {
                let show = match event {
                    SessionEvent::ClosePanel(id) => {
                        this.close_panel(PanelId::from(*id), window, cx);
                        return;
                    }
                    SessionEvent::Show(show) => show.clone(),
                    SessionEvent::JumpToThread { pr, .. }
                    | SessionEvent::FileSelected { pr, .. } => ShowPanel {
                        pr: Some(pr.clone()),
                        kind: PanelKind::Diff,
                        replace: None,
                        beside: false,
                    },
                };
                this.show_panel(&show, window, cx);
                this.pass_on(event, window, cx);
            },
        )
        .detach();
        if let Some(subscriptions) = Subscriptions::global(cx) {
            cx.observe(&subscriptions, |_, _, cx| cx.notify()).detach();
        }
        if let Some(terminals) = Terminals::global(cx) {
            cx.subscribe_in(
                &terminals,
                window,
                |this: &mut Self, _, event: &TerminalsEvent, window, cx| match event {
                    TerminalsEvent::Open { spec, at } => {
                        this.add_terminal(spec.clone(), *at, window, cx);
                    }
                },
            )
            .detach();
        }
        let me = cx.weak_entity();
        cx.set_global(ShellHandle(me));
        let sidebar_restored =
            sidebar_panels::section_panel(dock.read(cx), sidebar_panels::Section::Inbox, cx)
                .is_some();
        sidebar_panels::install(&dock, window, cx);
        sidebar_panels::sync_placements(&dock, cx);
        let sidebar = Sidebar::global(cx);
        window.focus(&sidebar.focus_handle(cx), cx);
        cx.subscribe_in(
            &dock,
            window,
            |this, dock, event: &DockEvent, window, cx| {
                // A sidebar boundary being dragged changes only two slots'
                // sizes; its end reports the change again.
                if matches!(event, DockEvent::LayoutChanged)
                    && !sidebar_panels::dragging_boundary(cx)
                {
                    sidebar_panels::sync_placements(dock, cx);
                    this.close_empty_sidebar(window, cx);
                    // Which section pins the title follows the sidebar's
                    // arrangement; a change in the main area alone (a split
                    // dragged every frame) leaves the sections be.
                    let tree = dock.read(cx).layout(DockPlacement::Left).cloned();
                    if tree != this.sidebar_tree {
                        this.sidebar_tree = tree;
                        this.sidebar.update(cx, |_, cx| cx.notify());
                    }
                    this.schedule_save(cx);
                }
            },
        )
        .detach();
        cx.subscribe(&sidebar, |this, _, event: &SidebarEvent, cx| match event {
            SidebarEvent::LayoutChanged => this.schedule_save(cx),
        })
        .detach();
        let sidebar_tree = dock.read(cx).layout(DockPlacement::Left).cloned();
        Self {
            probe: None,
            session,
            sidebar,
            dock,
            sidebar_restored,
            sidebar_tree,
            persist,
            layout_file: None,
            save_task: None,
            key_help: None,
            activity: false,
            last_shown: None,
            last_pr_panel: None,
            palette: None,
            _palette: None,
        }
    }

    /// Opens a command line target in a permanent tab: its diff.
    pub fn open_target(&mut self, target: &crate::OpenTarget, cx: &mut Context<Self>) {
        let session = self.session.clone();
        session.update(cx, |session, cx| {
            if let Some(key) = session.resolve_target(target) {
                session.show(&key, PanelKind::Diff, OpenMode::Permanent, cx);
            }
        });
    }

    /// Opens `target`'s conversation in the preview tab, as a click on its
    /// inbox row does.
    pub fn preview_target(&mut self, target: &crate::OpenTarget, cx: &mut Context<Self>) {
        let session = self.session.clone();
        session.update(cx, |session, cx| {
            if let Some(key) = session.resolve_target(target) {
                session.show(&key, PanelKind::Conversation, OpenMode::Preview, cx);
            }
        });
    }

    /// Opens `target` permanently in a new tab group beside the others.
    pub fn open_beside(&mut self, target: &crate::OpenTarget, cx: &mut Context<Self>) {
        let session = self.session.clone();
        session.update(cx, |session, cx| {
            if let Some(key) = session.resolve_target(target) {
                session.show(&key, PanelKind::Diff, OpenMode::Beside, cx);
            }
        });
    }

    /// Brings a panel forward, building it where it belongs when its pull
    /// request has none: beside the preview it replaces, with the pull
    /// request's other tabs, or in the group of the last pull request shown.
    /// Hands a jump to the Diff panel that shows it, which may have been
    /// built for it just now.
    fn pass_on(&mut self, event: &SessionEvent, window: &mut Window, cx: &mut Context<Self>) {
        let pr = match event {
            SessionEvent::JumpToThread { pr, .. } | SessionEvent::FileSelected { pr, .. } => pr,
            SessionEvent::Show(_) | SessionEvent::ClosePanel(_) => return,
        };
        let Some(id) = self
            .session
            .read(cx)
            .panel_of(Some(pr), PanelKind::Diff)
            .map(PanelId::from)
        else {
            return;
        };
        let Some(panel) = self
            .dock
            .read(cx)
            .panel(id)
            .and_then(|panel| panel.view().downcast::<crate::diff_panel::DiffPanel>().ok())
        else {
            return;
        };
        panel.update(cx, |panel, cx| match event {
            SessionEvent::JumpToThread {
                thread, focus_diff, ..
            } => panel.reveal_thread(thread.clone(), *focus_diff, window, cx),
            SessionEvent::FileSelected { path, .. } => panel.reveal_file(path.clone(), cx),
            SessionEvent::Show(_) | SessionEvent::ClosePanel(_) => {}
        });
    }

    fn show_panel(&mut self, show: &ShowPanel, window: &mut Window, cx: &mut Context<Self>) {
        let existing = self
            .session
            .read(cx)
            .panel_of(show.pr.as_ref(), show.kind)
            .map(PanelId::from)
            .filter(|id| self.dock.read(cx).panel(*id).is_some());
        let id = match existing {
            Some(id) => {
                if show.beside {
                    self.place_beside(id, window, cx);
                }
                id
            }
            None => {
                let Some(id) = self.add_panel(show, window, cx) else {
                    return;
                };
                id
            }
        };
        if let Some(old) = &show.replace {
            self.close_pr(old, window, cx);
        }
        self.dock
            .update(cx, |dock, cx| dock.select_panel(id, window, cx));
        self.last_shown = Some(id);
        if show.kind != PanelKind::Log {
            self.last_pr_panel = Some(id);
        }
        cx.notify();
    }

    /// The main area's tab group holding `panel`, and its index there.
    fn place_of(&self, panel: PanelId, cx: &App) -> Option<(NodeId, usize)> {
        let place = tab_menu::place_of(self.dock.read(cx), panel)?;
        let ix = place.panels.iter().position(|held| *held == panel)?;
        (place.region == DockPlacement::Center).then_some((place.node, ix))
    }

    fn add_panel(
        &mut self,
        show: &ShowPanel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<PanelId> {
        let pr = show.pr.clone();
        let handle = match show.kind {
            PanelKind::Diff => tab_handle(DiffPanel::for_pr(pr.clone(), window, cx)),
            PanelKind::Conversation => {
                tab_handle(ConversationPanel::for_pr(pr.clone(), window, cx))
            }
            PanelKind::Checks => tab_handle(ChecksPanel::for_pr(pr.clone(), window, cx)),
            PanelKind::Log => tab_handle(LogPanel::new(window, cx)),
        };
        let id = handle.panel_id(cx);
        // Where it goes: the preview's place, its pull request's group, or
        // the group of the last pull request shown.
        let session = self.session.read(cx);
        let replaced = show.replace.as_ref().and_then(|old| {
            session
                .panels()
                .iter()
                .filter(|entry| entry.pr.as_ref() == Some(old))
                .find_map(|entry| self.place_of(PanelId::from(entry.id), cx))
        });
        let sibling = pr.as_ref().and_then(|key| {
            session
                .panels()
                .iter()
                .filter(|entry| entry.pr.as_ref() == Some(key) && PanelId::from(entry.id) != id)
                .find_map(|entry| self.place_of(PanelId::from(entry.id), cx))
        });
        let main = self
            .last_pr_panel
            .and_then(|panel| self.place_of(panel, cx))
            .or_else(|| {
                session
                    .panels()
                    .iter()
                    .filter(|entry| entry.kind != PanelKind::Log && PanelId::from(entry.id) != id)
                    .find_map(|entry| self.place_of(PanelId::from(entry.id), cx))
            });
        self.dock.update(cx, |dock, cx| {
            dock.add_panel_view(handle, DockPlacement::Center, None, window, cx);
        });
        let target = match show.kind {
            _ if show.beside => main.map(|(node, _)| InsertTarget::Split {
                node,
                placement: Placement::Right,
                size: None,
            }),
            PanelKind::Log => main.map(|(node, _)| InsertTarget::Split {
                node,
                placement: Placement::Right,
                size: Some(px(LOG_WIDTH)),
            }),
            _ => replaced
                .map(|(node, ix)| (node, Some(ix)))
                .or(sibling.map(|(node, _)| (node, None)))
                .or(main.map(|(node, _)| (node, None)))
                .map(|(node, ix)| InsertTarget::Tabs {
                    node,
                    ix,
                    activate: true,
                }),
        };
        if let Some(target) = target {
            self.dock
                .update(cx, |dock, cx| dock.move_panel(id, target, window, cx));
        }
        Some(id)
    }

    /// Moves `panel` into a new group right of the group it is in.
    fn place_beside(&mut self, panel: PanelId, window: &mut Window, cx: &mut Context<Self>) {
        let Some((node, _)) = self.place_of(panel, cx) else {
            return;
        };
        let target = InsertTarget::Split {
            node,
            placement: Placement::Right,
            size: None,
        };
        self.dock
            .update(cx, |dock, cx| dock.move_panel(panel, target, window, cx));
    }

    /// Closes every tab of `key`, which releases what was loaded for it.
    pub(crate) fn close_pr(&mut self, key: &PrKey, window: &mut Window, cx: &mut Context<Self>) {
        let ids: Vec<PanelId> = self
            .session
            .read(cx)
            .panels()
            .iter()
            .filter(|entry| entry.pr.as_ref() == Some(key))
            .map(|entry| PanelId::from(entry.id))
            .collect();
        for id in ids {
            self.close_panel(id, window, cx);
        }
        self.session
            .update(cx, |session, cx| session.release(key, cx));
    }

    /// Takes one tab out of the dock. The dock removes panels by their
    /// type, so the handle is matched against each of the app's.
    pub fn close_panel(&mut self, id: PanelId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(panel) = self.dock.read(cx).panel(id).cloned() else {
            return;
        };
        let focus = panel.focus_handle(cx);
        let view = panel.view();
        self.dock.update(cx, |dock, cx| {
            if let Ok(panel) = view.clone().downcast::<DiffPanel>() {
                dock.remove_panel(panel, window, cx);
            } else if let Ok(panel) = view.clone().downcast::<ConversationPanel>() {
                dock.remove_panel(panel, window, cx);
            } else if let Ok(panel) = view.clone().downcast::<ChecksPanel>() {
                dock.remove_panel(panel, window, cx);
            } else if let Ok(panel) = view.clone().downcast::<LogPanel>() {
                dock.remove_panel(panel, window, cx);
            } else if let Ok(panel) = view.clone().downcast::<SettingsPanel>() {
                dock.remove_panel(panel, window, cx);
            } else if let Ok(panel) = view.downcast::<TerminalPanel>() {
                dock.remove_panel(panel, window, cx);
            }
        });
        // The dock hands focus to a neighboring tab; with none left, the
        // sidebar takes it so the keys keep working.
        if window.focused(cx).is_none() || focus.contains_focused(window, cx) {
            window.focus(&self.sidebar.focus_handle(cx), cx);
        }
        cx.notify();
    }

    /// `Ctrl+`: the machine picker, for a terminal.
    fn new_terminal(&mut self, _: &NewTerminal, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(terminals) = Terminals::global(cx) {
            terminals.update(cx, |terminals, cx| {
                terminals.open_picker(Purpose::NewTerminal, window, cx);
            });
        }
    }

    /// Opens a terminal panel running `spec`: a tab in the main area's
    /// current group, a split to its right, or beside the CI log (a split
    /// at the log's width when there is no log yet). Focus goes to it.
    pub fn add_terminal(
        &mut self,
        spec: TerminalSpec,
        at: Where,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let agent = spec.agent.clone();
        let panel = TerminalPanel::spawn(spec, cx);
        if let (Some(agent), Some(review)) = (agent, crate::claude_review::AgentReview::global(cx))
        {
            review.update(cx, |review, cx| review.attach(&agent, &panel, cx));
        }
        let handle = tab_handle(panel.clone());
        let id = handle.panel_id(cx);
        let main = self.main_group(cx);
        let log = self
            .session
            .read(cx)
            .panel_of(None, PanelKind::Log)
            .map(PanelId::from)
            .and_then(|log| self.place_of(log, cx));
        self.dock.update(cx, |dock, cx| {
            dock.add_panel_view(handle, DockPlacement::Center, None, window, cx);
        });
        let target = match at {
            Where::Tab => main.map(|node| InsertTarget::Tabs {
                node,
                ix: None,
                activate: true,
            }),
            Where::Split => main.map(|node| InsertTarget::Split {
                node,
                placement: Placement::Right,
                size: None,
            }),
            Where::RightPane => match log {
                Some((node, _)) => Some(InsertTarget::Tabs {
                    node,
                    ix: None,
                    activate: true,
                }),
                None => main.map(|node| InsertTarget::Split {
                    node,
                    placement: Placement::Right,
                    size: Some(px(LOG_WIDTH * 1.5)),
                }),
            },
        };
        if let Some(target) = target {
            self.dock
                .update(cx, |dock, cx| dock.move_panel(id, target, window, cx));
        }
        self.dock
            .update(cx, |dock, cx| dock.select_panel(id, window, cx));
        self.last_shown = Some(id);
        window.focus(&panel.focus_handle(cx), cx);
        cx.notify();
    }

    /// Brings `panel`'s tab forward and gives it the keys.
    pub fn focus_terminal(
        &mut self,
        panel: &Entity<TerminalPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = PanelId::from(panel.entity_id());
        if self.dock.read(cx).panel(id).is_none() {
            return;
        }
        self.dock
            .update(cx, |dock, cx| dock.select_panel(id, window, cx));
        self.last_shown = Some(id);
        window.focus(&panel.focus_handle(cx), cx);
        cx.notify();
    }

    /// The main area's group: the last pull request's, else any group that
    /// is not the log's.
    pub(crate) fn main_group(&self, cx: &App) -> Option<NodeId> {
        if let Some((node, _)) = self.last_pr_panel.and_then(|id| self.place_of(id, cx)) {
            return Some(node);
        }
        let dock = self.dock.read(cx);
        let log = self
            .session
            .read(cx)
            .panel_of(None, PanelKind::Log)
            .map(PanelId::from);
        dock.layout(DockPlacement::Center)?
            .panels()
            .filter(|id| Some(*id) != log)
            .find_map(|id| self.place_of(id, cx))
            .map(|(node, _)| node)
    }

    /// The tab holding keyboard focus, in any region.
    fn focused_panel(&self, window: &Window, cx: &App) -> Option<PanelId> {
        let dock = self.dock.read(cx);
        [DockPlacement::Center, DockPlacement::Left]
            .into_iter()
            .filter_map(|region| dock.layout(region))
            .flat_map(|tree| tree.panels().collect::<Vec<_>>())
            .find(|id| {
                dock.panel(*id)
                    .is_some_and(|panel| panel.focus_handle(cx).contains_focused(window, cx))
            })
    }

    /// `Ctrl+W`: the tab holding keyboard focus, else the one shown last.
    /// The sidebar's sections do not close.
    fn close_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        let dock = self.dock.read(cx);
        let closable = |id: &PanelId| dock.panel(*id).is_some_and(|panel| panel.closable(cx));
        let target = self
            .focused_panel(window, cx)
            .filter(closable)
            .or(self.last_shown.filter(closable));
        if let Some(id) = target {
            self.close_panel(id, window, cx);
        }
    }

    /// `Ctrl+Tab` and `Ctrl+Shift+Tab`: shows the tab `step` places along in
    /// the group of the focused tab, else of the one shown last, wrapping,
    /// and focuses it the way a click on its tab does.
    fn step_tab(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) {
        let dock = self.dock.read(cx);
        let Some(panel) = self.focused_panel(window, cx).or(self.last_shown) else {
            return;
        };
        let Some(place) = tab_menu::place_of(dock, panel) else {
            return;
        };
        let Some(PaneRef::Tabs { panels, active_ix }) = dock
            .layout(place.region)
            .and_then(|tree| tree.find_node(place.node))
            .map(|node| node.kind())
        else {
            return;
        };
        let target = panels[wrapped(active_ix, step, panels.len())];
        if target == panels[active_ix] {
            return;
        }
        self.dock
            .update(cx, |dock, cx| dock.select_panel(target, window, cx));
        if let Some(view) = self.dock.read(cx).panel(target) {
            window.focus(&view.focus_handle(cx), cx);
        }
        if place.region == DockPlacement::Center {
            self.last_shown = Some(target);
        }
    }

    /// The tab menu's Close others, Close to the right and Close all: the
    /// closable tabs of `panel`'s group, as their close buttons close them.
    pub(crate) fn close_in_group(
        &mut self,
        panel: PanelId,
        which: TabAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dock = self.dock.read(cx);
        let Some(place) = tab_menu::place_of(dock, panel) else {
            return;
        };
        let at = place.panels.iter().position(|id| *id == panel).unwrap_or(0);
        let doomed: Vec<PanelId> = place
            .panels
            .iter()
            .enumerate()
            .filter(|&(ix, id)| match which {
                TabAction::CloseOthers => *id != panel,
                TabAction::CloseToRight => ix > at,
                _ => true,
            })
            .map(|(_, id)| *id)
            .filter(|id| dock.panel(*id).is_some_and(|view| view.closable(cx)))
            .collect();
        for id in doomed {
            self.close_panel(id, window, cx);
        }
    }

    /// The tab menu's Split right and Split down: `panel` in a new group
    /// beside the one it is in, which must hold more than it.
    pub(crate) fn split(
        &mut self,
        panel: PanelId,
        placement: Placement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(place) = tab_menu::place_of(self.dock.read(cx), panel) else {
            return;
        };
        if place.panels.len() < 2 {
            return;
        }
        let target = InsertTarget::Split {
            node: place.node,
            placement,
            size: None,
        };
        self.dock.update(cx, |dock, cx| {
            dock.move_panel(panel, target, window, cx);
            dock.select_panel(panel, window, cx);
        });
    }

    /// The tab menu's Move to the main area or Move to the sidebar. The
    /// sidebar opens if it was hidden.
    pub(crate) fn move_to_other_area(
        &mut self,
        panel: PanelId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let main_group = self.main_group(cx);
        let dock = self.dock.read(cx);
        let Some(place) = tab_menu::place_of(dock, panel) else {
            return;
        };
        let Some((to, target)) = tab_menu::other_area_target(dock, place.region, main_group) else {
            return;
        };
        self.dock.update(cx, |dock, cx| {
            if to == DockPlacement::Left && !dock.is_dock_open(DockPlacement::Left) {
                dock.toggle_dock(DockPlacement::Left, window, cx);
            }
            dock.move_panel(panel, target, window, cx);
            dock.select_panel(panel, window, cx);
        });
        if to == DockPlacement::Center {
            self.last_shown = Some(panel);
        }
    }

    /// A sidebar every section left is hidden rather than drawn empty.
    fn close_empty_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dock = self.dock.read(cx);
        let empty = dock
            .layout(DockPlacement::Left)
            .is_some_and(|tree| tree.panels().next().is_none());
        if empty && dock.is_dock_open(DockPlacement::Left) {
            self.dock.update(cx, |dock, cx| {
                dock.toggle_dock(DockPlacement::Left, window, cx);
            });
        }
    }

    /// The tab group to zoom: the one holding focus, else the one of the
    /// tab shown last.
    fn zoom_target(&self, window: &Window, cx: &App) -> Option<NodeId> {
        let dock = self.dock.read(cx);
        self.focused_panel(window, cx)
            .or(self.last_shown)
            .and_then(|panel| tab_menu::place_of(dock, panel))
            .map(|place| place.node)
    }

    /// `Shift+Esc` where no tab group took it: the group with focus fills
    /// the dock, or the dock comes back.
    fn toggle_zoom(&mut self, _: &ToggleZoom, window: &mut Window, cx: &mut Context<Self>) {
        if self.dock.read(cx).is_zoomed() {
            self.zoom_out(&ZoomOut, window, cx);
            return;
        }
        let Some(node) = self.zoom_target(window, cx) else {
            return;
        };
        self.dock
            .update(cx, |dock, cx| dock.set_zoomed_in(node, window, cx));
        cx.notify();
    }

    /// `Esc` while zoomed: the dock comes back.
    fn zoom_out(&mut self, _: &ZoomOut, window: &mut Window, cx: &mut Context<Self>) {
        self.dock
            .update(cx, |dock, cx| dock.set_zoomed_out(window, cx));
        cx.notify();
    }

    /// Whether a tab group fills the dock.
    pub fn is_zoomed(&self, cx: &App) -> bool {
        self.dock.read(cx).is_zoomed()
    }

    /// The window's shell, once it is built.
    pub fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<ShellHandle>()?.0.upgrade()
    }

    fn toggle_key_help(&mut self, _: &ToggleKeyHelp, window: &mut Window, cx: &mut Context<Self>) {
        if self.key_help.is_some() {
            self.close_key_help(&CloseKeyHelp, window, cx);
            return;
        }
        let focus = cx.focus_handle();
        let previous = window.focused(cx);
        window.focus(&focus, cx);
        self.key_help = Some(KeyHelp { focus, previous });
        cx.notify();
    }

    fn close_key_help(&mut self, _: &CloseKeyHelp, window: &mut Window, cx: &mut Context<Self>) {
        let Some(help) = self.key_help.take() else {
            cx.propagate();
            return;
        };
        if let Some(previous) = help.previous {
            window.focus(&previous, cx);
        }
        cx.notify();
    }

    /// The open command palette or quick open.
    pub fn palette(&self) -> Option<&Entity<Palette>> {
        self.palette.as_ref()
    }

    /// Opens the palette in `mode`, or closes it when that mode is the one
    /// open; the other mode open is replaced.
    fn toggle_palette(&mut self, mode: Mode, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(palette) = self.palette.take() {
            self._palette = None;
            let same = palette.read(cx).mode() == mode;
            palette.update(cx, |palette, cx| palette.dismiss(window, cx));
            cx.notify();
            if same {
                return;
            }
        }
        if self.key_help.is_some() {
            self.close_key_help(&CloseKeyHelp, window, cx);
        }
        let previous = window
            .focused(cx)
            .unwrap_or_else(|| self.sidebar.focus_handle(cx));
        let items = match mode {
            Mode::Commands => self.command_items(&previous, window, cx),
            Mode::QuickOpen => self.quick_open_items(cx),
        };
        let palette = cx.new(|cx| Palette::new(mode, items, Some(previous), window, cx));
        self._palette = Some(cx.subscribe(&palette, |this, palette, _: &Closed, cx| {
            if this.palette.as_ref() == Some(&palette) {
                this.palette = None;
                cx.notify();
            }
        }));
        self.palette = Some(palette);
        cx.notify();
    }

    /// Every command, disabled with the reason where `focus` cannot take
    /// it, then the open pull request's and the focused tab's.
    fn command_items(
        &self,
        focus: &FocusHandle,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Vec<Item> {
        let mut items: Vec<Item> = commands::COMMANDS
            .iter()
            .filter_map(|command| {
                let action = command.action()?;
                let available = window.is_action_available_in(action.as_ref(), focus);
                let item = Item::new(command.id(), command.title, move |window, cx| {
                    window.dispatch_action(action.boxed_clone(), cx);
                });
                Some(
                    item.detail(command.group.title())
                        .keys(command.keys)
                        .disabled((!available).then(|| command.needs())),
                )
            })
            .collect();
        if let Some(key) = self.session.read(cx).open_key().cloned() {
            items.extend(crate::pr_header::palette_items(&self.session, &key, cx));
        }
        items.extend(self.panel_items(window, cx));
        items
    }

    /// The focused tab's commands: split it off, close the rest of its group.
    fn panel_items(&self, window: &Window, cx: &mut Context<Self>) -> Vec<Item> {
        type Act = fn(&mut Shell, PanelId, &mut Window, &mut Context<Shell>);
        let Some(panel) = self.focused_panel(window, cx).or(self.last_shown) else {
            return Vec::new();
        };
        let dock = self.dock.read(cx);
        let (Some(view), Some(place)) = (dock.panel(panel), tab_menu::place_of(dock, panel)) else {
            return Vec::new();
        };
        if place.region != DockPlacement::Center {
            return Vec::new();
        }
        let name = view.panel_name(cx);
        let alone = (place.panels.len() < 2).then_some("It is the only tab in its group");
        let shell = cx.weak_entity();
        let item = |id: &'static str, title: &'static str, act: Act| {
            let shell = shell.clone();
            Item::new(id, title, move |window, cx| {
                shell
                    .update(cx, |shell, cx| act(shell, panel, window, cx))
                    .ok();
            })
            .detail(name)
            .disabled(alone)
        };
        vec![
            item(
                "panel:split-right",
                "Split the tab right",
                |shell, panel, window, cx| {
                    shell.split(panel, Placement::Right, window, cx);
                },
            ),
            item(
                "panel:split-down",
                "Split the tab down",
                |shell, panel, window, cx| {
                    shell.split(panel, Placement::Bottom, window, cx);
                },
            ),
            item(
                "panel:close-others",
                "Close the other tabs of its group",
                |shell, panel, window, cx| {
                    shell.close_in_group(panel, TabAction::CloseOthers, window, cx);
                },
            ),
        ]
    }

    /// The open pull requests, then the inbox's, then the files of the
    /// focused one.
    fn quick_open_items(&self, cx: &App) -> Vec<Item> {
        let session = self.session.read(cx);
        let open = session.open_prs().map(|open| (&open.pr, "Open"));
        let inbox = session.inbox.prs.iter().map(|pr| (pr, "In the inbox"));
        let mut seen = std::collections::HashSet::new();
        let mut items = Vec::new();
        for (pr, place) in open.chain(inbox) {
            if !seen.insert(&pr.key) {
                continue;
            }
            let (target, key) = (self.session.clone(), pr.key.clone());
            let label = pr.ref_label();
            let title = format!("{label} {}", pr.title);
            let item = Item::new(format!("pr:{label}"), title, move |_, cx| {
                target.update(cx, |session, cx| {
                    session.show(&key, PanelKind::Diff, OpenMode::Permanent, cx);
                });
            });
            items.push(item.detail(place));
        }
        if let Some(open) = session.open_pr() {
            let label = open.pr.ref_label();
            for file in &open.files {
                let (target, path) = (self.session.clone(), file.path.clone());
                let item = Item::new(
                    format!("file:{label}:{path}"),
                    path.clone(),
                    move |_, cx| {
                        target.update(cx, |session, cx| session.select_file(path.clone(), cx));
                    },
                );
                items.push(item.detail(label.clone()));
            }
        }
        items
    }

    /// Every key binding, grouped, over a dimmed window. Any click or Escape
    /// closes it.
    fn key_help_overlay(&self, help: &KeyHelp, cx: &mut Context<Self>) -> AnyElement {
        let t = ui::theme(cx);
        let c = &t.colors;
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
                .border_color(c.border_strong)
                .bg(c.field)
                .text_color(c.text)
                .child(key.to_owned())
        };
        let group = |(group, rows): (Group, Vec<&'static commands::Command>)| {
            v_flex()
                .gap(px(4.0))
                .child(
                    div()
                        .tiny(&t)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(c.text_muted)
                        .child(group.title().to_uppercase()),
                )
                .children(rows.into_iter().map(|row| {
                    h_flex()
                        .gap(px(12.0))
                        .items_center()
                        .child(
                            h_flex()
                                .w(px(150.0))
                                .flex_none()
                                .gap(px(4.0))
                                .children(row.keys.split("  ").map(keycap)),
                        )
                        .child(div().small(&t).text_color(c.text_secondary).child(row.help))
                }))
        };
        // The Diff's long list in a column of its own, the rest beside it.
        let (diff, rest): (Vec<_>, Vec<_>) =
            commands::key_help().partition(|(group, _)| *group == Group::Diff);
        let column = |groups: Vec<(Group, Vec<&'static commands::Command>)>| {
            v_flex()
                .w(px(380.0))
                .gap(px(14.0))
                .children(groups.into_iter().map(group))
        };
        div()
            .id("key-help")
            .key_context(KEY_HELP_CONTEXT)
            .track_focus(&help.focus)
            .on_action(cx.listener(Self::close_key_help))
            .on_action(cx.listener(Self::toggle_key_help))
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(c.background.opacity(0.7))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.close_key_help(&CloseKeyHelp, window, cx)),
            )
            .child(
                v_flex()
                    .id("key-help-box")
                    .max_h_full()
                    .overflow_y_scroll()
                    .gap(px(14.0))
                    .p(px(18.0))
                    .rounded(t.radius.lg)
                    .border_1()
                    .border_color(c.border_strong)
                    .bg(c.surface_menu)
                    .child(
                        h_flex()
                            .gap(px(8.0))
                            .items_center()
                            .child(
                                Icon::new(IconName::Keyboard)
                                    .size(px(16.0))
                                    .text_color(c.text_muted),
                            )
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(c.text)
                                    .child("Keyboard"),
                            )
                            .child(div().flex_1())
                            .child(div().small(&t).text_color(c.text_muted).child("Esc closes")),
                    )
                    .child(
                        h_flex()
                            .gap(px(28.0))
                            .items_start()
                            .child(column(rest))
                            .child(column(diff)),
                    ),
            )
            .into_any_element()
    }

    /// With `persist`, saves the layout to `path` instead of the user's data
    /// directory, so a test or a scripted run never touches the user's file.
    #[must_use]
    pub fn with_layout_file(mut self, path: PathBuf) -> Self {
        self.layout_file = Some(path);
        self
    }

    /// The dock: the sidebar's sections in its left region, the tabs in its
    /// center.
    pub fn dock(&self) -> &Entity<DockArea> {
        &self.dock
    }

    /// Puts an older build's separate sidebar in place of the default one,
    /// unless the layout the dock was loaded with had its sidebar already.
    pub fn load_sidebar(
        &mut self,
        state: DockAreaState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.sidebar_restored {
            return;
        }
        let mut merged = self.dock.read(cx).dump(cx);
        layout::merge_sidebar(&mut merged, state);
        let loaded = self
            .dock
            .update(cx, |dock, cx| dock.load(merged, window, cx));
        if let Err(error) = loaded {
            eprintln!("could not restore the sidebar: {error}");
        }
        sidebar_panels::install(&self.dock, window, cx);
        sidebar_panels::sync_placements(&self.dock, cx);
        self.sidebar_restored = true;
    }

    /// `--layout columns`: the Checks section in a second column of the
    /// sidebar, which widens to hold both.
    pub fn columns_preset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        sidebar_panels::checks_beside_inbox(&self.dock, window, cx);
        self.dock.update(cx, |dock, cx| {
            dock.set_dock_size(DockPlacement::Left, layout::SIDEBAR_WIDTH * 2.0, window, cx);
        });
        cx.notify();
    }

    /// Whether the `?` overlay is open.
    pub fn is_key_help_open(&self) -> bool {
        self.key_help.is_some()
    }

    #[must_use]
    pub fn with_probe(mut self, probe: Box<dyn Probe>) -> Self {
        self.probe = Some(probe);
        self
    }

    /// Debounced: a drag fires a change per frame, and the disk write runs off
    /// the main thread.
    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        if !self.persist {
            return;
        }
        let file = self.layout_file.clone();
        self.save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_DEBOUNCE).await;
            let Ok(state) = this.update(cx, |this, cx| this.dock.read(cx).dump(cx)) else {
                return;
            };
            let result = cx
                .background_executor()
                .spawn(async move {
                    match file {
                        Some(path) => layout::write_to(&path, &state),
                        None => layout::write(&state),
                    }
                })
                .await;
            if let Err(error) = result {
                eprintln!("could not save the dock layout: {error}");
            }
        }));
    }

    /// The settings panel is in the dock, wherever the user moved it.
    fn settings_shown(&self, cx: &App) -> bool {
        SettingsPanel::existing(cx).is_some_and(|panel| {
            self.dock
                .read(cx)
                .panel(PanelId::from(panel.entity_id()))
                .is_some()
        })
    }

    pub fn show_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_settings(&OpenSettings, window, cx);
    }

    /// Opens the settings as a tab of the dock, or brings its tab forward.
    fn open_settings(&mut self, _: &OpenSettings, window: &mut Window, cx: &mut Context<Self>) {
        let panel = SettingsPanel::existing(cx).filter(|_| self.settings_shown(cx));
        let panel = panel.unwrap_or_else(|| {
            let panel =
                SettingsPanel::existing(cx).unwrap_or_else(|| SettingsPanel::new(window, cx));
            self.dock.update(cx, |dock, cx| {
                dock.add_panel_view(
                    tab_handle(panel.clone()),
                    DockPlacement::Center,
                    None,
                    window,
                    cx,
                );
            });
            panel
        });
        let id = PanelId::from(panel.entity_id());
        self.dock
            .update(cx, |dock, cx| dock.select_panel(id, window, cx));
        self.last_shown = Some(id);
        window.focus(&panel.focus_handle(cx), cx);
        cx.notify();
    }

    /// `Ctrl+B`: shows or hides the sidebar, the dock's left region. A
    /// sidebar every section left stays hidden.
    fn toggle_sidebar(&mut self, _: &ToggleSidebar, window: &mut Window, cx: &mut Context<Self>) {
        let has_sections = self
            .dock
            .read(cx)
            .layout(DockPlacement::Left)
            .is_some_and(|tree| tree.panels().next().is_some());
        if !has_sections {
            return;
        }
        let had_focus = self
            .focused_panel(window, cx)
            .and_then(|panel| tab_menu::place_of(self.dock.read(cx), panel))
            .is_some_and(|place| place.region == DockPlacement::Left);
        let open = self.dock.update(cx, |dock, cx| {
            dock.toggle_dock(DockPlacement::Left, window, cx);
            dock.is_dock_open(DockPlacement::Left)
        });
        if open {
            window.focus(&self.sidebar.focus_handle(cx), cx);
        } else if had_focus {
            // The keys go on working: focus moves to the tab shown last, or
            // the dock.
            let dock = self.dock.read(cx);
            let focus = self
                .last_shown
                .and_then(|panel| dock.panel(panel))
                .map_or_else(|| dock.focus_handle(cx), |panel| panel.focus_handle(cx));
            window.focus(&focus, cx);
        }
        cx.notify();
    }

    /// Whether the sidebar, the dock's left region, is shown.
    pub fn is_sidebar_open(&self, cx: &App) -> bool {
        self.dock.read(cx).is_dock_open(DockPlacement::Left)
    }

    fn toggle_diff_layout(&mut self, _: &ToggleDiffLayout, _: &mut Window, cx: &mut Context<Self>) {
        self.session
            .update(cx, |session, cx| session.toggle_diff_layout(cx));
    }

    fn app_title(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = ui::theme(cx);
        let settings = ui::icon_button(
            cx,
            "open-settings",
            IconName::Settings,
            cx.listener(|this, _, window, cx| this.open_settings(&OpenSettings, window, cx)),
        );
        h_flex()
            .size_full()
            .items_center()
            .justify_between()
            .pr(px(12.0))
            .child(ui::wordmark(cx, "review"))
            .child(
                h_flex()
                    .gap(px(12.0))
                    .items_center()
                    .child(
                        div()
                            .small(&t)
                            .text_color(t.colors.text_muted)
                            .child("Ctrl+B sidebar  Ctrl+, settings"),
                    )
                    .child(settings),
            )
    }

    fn empty_main(&self, cx: &App) -> impl IntoElement {
        ui::empty_state(
            cx,
            "No pull request open",
            "Pick one from the inbox. The diff, conversation and checks open here.",
            &[("j / k", "move"), ("Enter", "open"), ("?", "all keys")],
        )
    }

    fn toggle_activity(&mut self, cx: &mut Context<Self>) {
        self.activity = !self.activity;
        if self.activity
            && let Some(subscriptions) = Subscriptions::global(cx)
        {
            subscriptions.update(cx, |s, cx| s.mark_read(cx));
        }
        cx.notify();
    }

    /// The bell in the status bar: how many changes came in since the list
    /// was last opened.
    fn activity_button(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let subscriptions = Subscriptions::global(cx)?;
        let t = ui::theme(cx);
        let c = &t.colors;
        let unread = subscriptions.read(cx).unread();
        let hover = c.hover;
        let label = if unread > 0 {
            format!("{unread} new")
        } else {
            "activity".to_owned()
        };
        Some(
            h_flex()
                .id("activity-button")
                .gap(px(4.0))
                .px(px(6.0))
                .h(px(20.0))
                .items_center()
                .rounded(t.radius.sm)
                .cursor_pointer()
                .hover(move |s| s.bg(hover))
                .on_click(cx.listener(|this, _, _, cx| this.toggle_activity(cx)))
                .child(
                    Icon::new(IconName::Bell)
                        .size(px(12.0))
                        .text_color(if unread > 0 {
                            c.accent_fg
                        } else {
                            c.text_muted
                        }),
                )
                .child(label)
                .into_any_element(),
        )
    }

    /// Recent changes on watched pull requests, newest first; a click opens
    /// the pull request where the change is.
    fn activity_overlay(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.activity {
            return None;
        }
        let subscriptions = Subscriptions::global(cx)?;
        let t = ui::theme(cx);
        let c = &t.colors;
        let hover = c.hover;
        let entries: Vec<_> = subscriptions.read(cx).activity().cloned().collect();
        let rows: Vec<AnyElement> = entries
            .into_iter()
            .enumerate()
            .map(|(ix, entry)| {
                let color = match entry.kind {
                    NoticeKind::CiFailed => c.danger,
                    NoticeKind::CiRecovered | NoticeKind::CiFinished => c.ok,
                    NoticeKind::Review | NoticeKind::Comment => c.info,
                    NoticeKind::MergedClosed | NoticeKind::Reopened => c.text_muted,
                };
                let minutes = (chrono::Utc::now() - entry.at).num_minutes().max(0);
                let ago = crate::model::relative_time(u32::try_from(minutes).unwrap_or(u32::MAX));
                let subscriptions = subscriptions.clone();
                let (key, place) = (entry.key.clone(), entry.place.clone());
                h_flex()
                    .id(("activity", ix))
                    .h(px(28.0))
                    .px(px(10.0))
                    .gap(px(8.0))
                    .items_center()
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.activity = false;
                        subscriptions.update(cx, |s, cx| s.open_place(&key, &place, cx));
                        cx.notify();
                    }))
                    .child(div().size(px(7.0)).flex_none().rounded_full().bg(color))
                    .child(
                        div()
                            .flex_none()
                            .mono(&t)
                            .tiny(&t)
                            .text_color(c.text_secondary)
                            .child(entry.label),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(c.text)
                            .child(entry.text),
                    )
                    .child(
                        div()
                            .flex_none()
                            .tiny(&t)
                            .text_color(c.text_muted)
                            .child(ago),
                    )
                    .into_any_element()
            })
            .collect();
        let empty = rows.is_empty();
        Some(
            v_flex()
                .id("activity-list")
                .absolute()
                .bottom(px(28.0))
                .right(px(12.0))
                .w(px(420.0))
                .max_h(px(360.0))
                .overflow_y_scroll()
                .py(px(6.0))
                .rounded(t.radius.lg)
                .border_1()
                .border_color(c.border_strong)
                .bg(c.surface_menu)
                .small(&t)
                .child(
                    div()
                        .px(px(10.0))
                        .pb(px(4.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(c.text)
                        .child("Activity on watched pull requests"),
                )
                .when(empty, |el| {
                    el.child(
                        div()
                            .px(px(10.0))
                            .py(px(6.0))
                            .text_color(c.text_muted)
                            .child("Nothing yet. Changes show here while the app runs."),
                    )
                })
                .children(rows)
                .into_any_element(),
        )
    }

    fn status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let bell = self.activity_button(cx);
        let t = ui::theme(cx);
        let c = &t.colors;
        let session = self.session.read(cx);
        let forges = session.forges.iter().map(|forge| {
            let dot = match forge.state {
                ForgeState::Ok => c.ok,
                ForgeState::Connecting => c.text_faint,
                ForgeState::RateLimited => c.warn,
                ForgeState::AuthError(_) | ForgeState::Error(_) => c.danger,
            };
            let label = match forge.state {
                ForgeState::Ok => format!("{} ({})", forge.host, forge.kind.label()),
                ref state => format!("{} ({}, {})", forge.host, forge.kind.label(), state.label()),
            };
            let account = forge
                .account
                .as_ref()
                .map(|user| ui::avatar(cx, &forge.host, user, ui::avatar::ROW));
            h_flex()
                .gap(px(5.0))
                .items_center()
                .child(div().size(px(6.0)).rounded_full().bg(dot))
                .children(account)
                .child(div().mono(&t).tiny(&t).child(label))
        });
        let activity = session.activity;
        let queue = if activity.forge + activity.git_active + activity.git_queued == 0 {
            "fetch queue: idle".to_owned()
        } else {
            format!(
                "fetch queue: {} active, {} queued",
                activity.forge + activity.git_active,
                activity.git_queued
            )
        };
        let notice = session.notice.clone();
        ui::status_bar(cx)
            .h(px(24.0))
            .flex_none()
            .flex()
            .px(px(12.0))
            .gap(px(16.0))
            .items_center()
            .justify_between()
            .small(&t)
            .text_color(c.text_muted)
            .child(
                h_flex().gap(px(16.0)).min_w_0().children(forges).children(
                    notice.map(|notice| div().truncate().text_color(c.danger).child(notice)),
                ),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap(px(16.0))
                    .items_center()
                    .children(bell)
                    .child(div().child(queue))
                    .child(div().child("j/k move  Enter open  ? keys")),
            )
    }
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(probe) = self.probe.as_mut() {
            probe.clock().begin(window, cx);
            probe.frame(window, cx);
        }
        let end_marker = self.probe.as_ref().map(|probe| probe.clock().end_marker());
        let t = ui::theme(cx);
        let dock = self.dock.read(cx);
        let zoomed = dock.is_zoomed();
        // Where the main area starts, right of the sidebar, for the note
        // an empty one shows.
        let empty_center = (!zoomed && dock.is_empty(DockPlacement::Center, cx)).then(|| {
            if dock.is_dock_open(DockPlacement::Left) {
                dock.dock_size(DockPlacement::Left).unwrap_or_default()
            } else {
                px(0.0)
            }
        });
        // The shown panels' slots.
        let spinners = DockSpinners::shown(cx);
        let key_help = self
            .key_help
            .as_ref()
            .map(|help| self.key_help_overlay(help, cx));
        // The spinners of the last frame turn on in this one. Asked here, in
        // render: a frame asked for from paint arrives late on Windows.
        if spinners.iter().any(|slots| !slots.is_empty()) {
            window.request_animation_frame();
        }

        let empty = empty_center.map(|left| {
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .right_0()
                .left(left)
                .child(self.empty_main(cx))
        });
        let main = div()
            .relative()
            .size_full()
            .child(self.dock.clone())
            .children(empty);

        let mut context = KeyContext::default();
        context.add(KEY_CONTEXT);
        if zoomed {
            context.add(ZOOMED_CONTEXT);
        }

        v_flex()
            .id("shell")
            .key_context(context)
            .on_action(cx.listener(Self::toggle_sidebar))
            .on_action(cx.listener(Self::toggle_diff_layout))
            .on_action(cx.listener(Self::toggle_key_help))
            .on_action(cx.listener(Self::close_key_help))
            .on_action(cx.listener(Self::open_settings))
            .on_action(cx.listener(Self::close_tab))
            .on_action(cx.listener(|this, _: &NextTab, window, cx| this.step_tab(1, window, cx)))
            .on_action(cx.listener(|this, _: &PrevTab, window, cx| this.step_tab(-1, window, cx)))
            .on_action(cx.listener(Self::new_terminal))
            .on_action(cx.listener(Self::toggle_zoom))
            .on_action(cx.listener(Self::zoom_out))
            .on_action(cx.listener(|this, _: &OpenCommandPalette, window, cx| {
                this.toggle_palette(Mode::Commands, window, cx);
            }))
            .on_action(cx.listener(|this, _: &QuickOpen, window, cx| {
                this.toggle_palette(Mode::QuickOpen, window, cx);
            }))
            .size_full()
            .bg(t.colors.background)
            .text_color(t.colors.text)
            .font_family(t.fonts.interface.family())
            .text_size(t.fonts.interface.size_px())
            .line_height(t.fonts.interface.line_height_px())
            .child(ui::title_bar(cx).child(self.app_title(cx)))
            .child(div().flex_1().min_h_0().w_full().child(main))
            .child(self.status_bar(cx))
            .children(spinners.iter().map(SpinnerSlots::layer))
            .children(self.activity_overlay(cx))
            .children(key_help)
            .children(self.palette.clone())
            .children(Terminals::global(cx).map(gpui::AnyView::from))
            .children(end_marker)
    }
}

/// `ix` moved by `step` places in a ring of `len` (at least one) tabs.
fn wrapped(ix: usize, step: isize, len: usize) -> usize {
    let len = len as isize;
    (ix as isize + step).rem_euclid(len) as usize
}

#[cfg(test)]
mod tests {
    use super::wrapped;

    #[test]
    fn stepping_wraps_at_both_ends() {
        assert_eq!(wrapped(0, 1, 3), 1);
        assert_eq!(wrapped(2, 1, 3), 0);
        assert_eq!(wrapped(0, -1, 3), 2);
        assert_eq!(wrapped(1, -1, 3), 0);
        assert_eq!(wrapped(0, 1, 1), 0);
    }
}
