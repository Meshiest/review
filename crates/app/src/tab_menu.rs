//! A dock tab's own controls: a right-click menu (copy the pull request's
//! reference, link, branch or title; close tabs; keep a preview; split the
//! tab off or move it between the sidebar and the main area; open the pull
//! request on its forge) and a middle click that closes the tab the way its
//! close button does.
//!
//! Every panel the app docks is wrapped in a [`Tab`] by [`tab_handle`],
//! which draws the panel's own title inside an element carrying both.

use std::any::Any;
use std::sync::Arc;

use gpui::{
    AnyElement, AnyView, App, ClipboardItem, Context, Entity, EntityId, FocusHandle,
    InteractiveElement as _, IntoElement, MouseButton, ParentElement, SharedString, Styled,
    WeakEntity, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_kit::component::{
    Placement,
    button::Button,
    dock::{
        BasePanelView, DockArea, DockPlacement, InsertTarget, NodeId, PaneNode, PaneRef, Panel,
        PanelControl, PanelHandle, PanelId, PanelState, PanelView, TabGroup, TitleStyle,
    },
    menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem},
};

use crate::model::{PrKey, PullRequest};
use crate::references;
use crate::session::Session;
use crate::shell::Shell;
use crate::untrusted::is_web_url;

/// The horizontal padding of the skin's tab around the title, which the
/// title's own element reaches into so a click anywhere on the tab hits it.
const TAB_PAD: f32 = 12.0;

/// Wraps `panel` for the dock, as `panel_handle` does, with the tab's menu
/// and middle click around its title.
pub fn tab_handle<P: Panel>(panel: Entity<P>) -> Arc<dyn BasePanelView> {
    let view: Arc<dyn PanelView> = Arc::new(panel);
    Arc::new(PanelHandle::from_view(Arc::new(Tab(view))))
}

/// A docked panel: everything is the panel's own except that its title is
/// drawn inside the tab's controls.
struct Tab(Arc<dyn PanelView>);

impl BasePanelView for Tab {
    fn panel_name(&self, cx: &App) -> &'static str {
        self.0.panel_name(cx)
    }

    fn panel_id(&self, cx: &App) -> PanelId {
        self.0.panel_id(cx)
    }

    fn closable(&self, cx: &App) -> bool {
        self.0.closable(cx)
    }

    fn zoomable(&self, cx: &App) -> bool {
        self.0.zoomable(cx)
    }

    fn visible(&self, cx: &App) -> bool {
        self.0.visible(cx)
    }

    fn set_active(&self, active: bool, window: &mut Window, cx: &mut App) {
        self.0.set_active(active, window, cx);
    }

    fn set_zoomed(&self, zoomed: bool, window: &mut Window, cx: &mut App) {
        self.0.set_zoomed(zoomed, window, cx);
    }

    fn on_added_to(&self, group: WeakEntity<TabGroup>, window: &mut Window, cx: &mut App) {
        self.0.on_added_to(group, window, cx);
    }

    fn on_removed(&self, window: &mut Window, cx: &mut App) {
        self.0.on_removed(window, cx);
    }

    fn view(&self) -> AnyView {
        self.0.view()
    }

    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.0.focus_handle(cx)
    }

    fn dump(&self, cx: &App) -> PanelState {
        self.0.dump(cx)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl PanelView for Tab {
    fn tab_name(&self, cx: &App) -> Option<SharedString> {
        self.0.tab_name(cx)
    }

    fn title(&self, window: &mut Window, cx: &mut App) -> AnyElement {
        let id = self.0.panel_id(cx);
        let closable = self.0.closable(cx);
        let title = self.0.title(window, cx);
        tab_title(id, closable, title)
    }

    fn title_style(&self, cx: &App) -> Option<TitleStyle> {
        self.0.title_style(cx)
    }

    fn title_suffix(&self, window: &mut Window, cx: &mut App) -> Option<AnyElement> {
        self.0.title_suffix(window, cx)
    }

    fn toolbar_buttons(&self, window: &mut Window, cx: &mut App) -> Option<Vec<Button>> {
        self.0.toolbar_buttons(window, cx)
    }

    fn dropdown_menu(&self, menu: PopupMenu, window: &mut Window, cx: &mut App) -> PopupMenu {
        self.0.dropdown_menu(menu, window, cx)
    }

    fn zoom_control(&self, cx: &App) -> Option<PanelControl> {
        self.0.zoom_control(cx)
    }

    fn inner_padding(&self, cx: &App) -> bool {
        self.0.inner_padding(cx)
    }

    fn title_bar(&self, cx: &App) -> bool {
        self.0.title_bar(cx)
    }
}

/// The `debug_selector` of a tab's title element.
#[must_use]
pub fn tab_selector(panel: PanelId) -> String {
    format!("dock-tab-{}", panel.as_u64())
}

/// `title` in an element as wide as the tab, which opens the tab's menu on
/// a right click and closes a closable tab on a middle click.
fn tab_title(panel: PanelId, closable: bool, title: AnyElement) -> AnyElement {
    div()
        .id(("dock-tab", panel.as_u64()))
        .debug_selector(move || tab_selector(panel))
        .flex()
        .items_center()
        .min_w_0()
        .mx(-px(TAB_PAD))
        .px(px(TAB_PAD))
        .when(closable, |el| {
            el.on_mouse_up(MouseButton::Middle, move |_, _, cx| {
                cx.stop_propagation();
                close(panel, cx);
            })
        })
        .child(title)
        .context_menu(move |menu, window, cx| popup(menu, panel, window, cx))
        .into_any_element()
}

/// What a tab's menu can do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabAction {
    /// `owner/repo#123`, or `group/project!123` on GitLab.
    CopyReference,
    CopyLink,
    CopyBranch,
    /// `title (owner/repo#123)`.
    CopyTitle,
    Close,
    /// The other closable tabs of its group.
    CloseOthers,
    CloseToRight,
    /// Every closable tab of its group.
    CloseAll,
    /// Every tab of its pull request, wherever they are.
    ClosePr,
    /// Makes the preview tab of its pull request permanent.
    KeepOpen,
    SplitRight,
    SplitDown,
    /// From the sidebar to the main area, or back.
    MoveToOtherArea,
    OpenOnForge,
}

/// One entry of a tab's menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuEntry {
    pub action: TabAction,
    pub label: &'static str,
    pub enabled: bool,
}

/// Where a tab is: its dock region, its tab group, and the group's tabs.
#[derive(Clone, Debug)]
pub struct Place {
    pub region: DockPlacement,
    pub node: NodeId,
    pub panels: Vec<PanelId>,
}

/// The regions of the dock, the main area first.
const REGIONS: [DockPlacement; 4] = [
    DockPlacement::Center,
    DockPlacement::Left,
    DockPlacement::Right,
    DockPlacement::Bottom,
];

/// Where `panel` is in `dock`.
pub fn place_of(dock: &DockArea, panel: PanelId) -> Option<Place> {
    REGIONS.into_iter().find_map(|region| {
        let tree = dock.layout(region)?;
        let node = tree.find_panel_node(panel)?;
        let PaneRef::Tabs { panels, .. } = tree.find_node(node)?.kind() else {
            return None;
        };
        Some(Place {
            region,
            node,
            panels: panels.to_vec(),
        })
    })
}

/// The first tab group under `node`, in layout order.
pub(crate) fn first_group(node: &PaneNode) -> Option<NodeId> {
    let mut found = None;
    node.walk(&mut |node| {
        if found.is_none() && matches!(node.kind(), PaneRef::Tabs { .. }) {
            found = Some(node.id());
        }
    });
    found
}

/// The pull request `panel` is bound to.
fn key_of(panel: PanelId, cx: &App) -> Option<PrKey> {
    Session::global(cx)
        .read(cx)
        .panels()
        .iter()
        .find(|entry| PanelId::from(entry.id) == panel)
        .and_then(|entry| entry.pr.clone())
}

/// The pull request `panel` is bound to, as the app knows it.
fn pr_of(panel: PanelId, cx: &App) -> Option<PullRequest> {
    let key = key_of(panel, cx)?;
    let session = Session::global(cx).read(cx);
    session
        .pr(&key)
        .map(|open| open.pr.clone())
        .or_else(|| session.inbox.get(&key).cloned())
}

/// The pull request's page on its forge.
fn link(pr: &PullRequest) -> String {
    pr.source
        .as_ref()
        .map(|source| source.web_url.clone())
        .filter(|url| is_web_url(url))
        .unwrap_or_else(|| references::pr_url(pr.forge, &pr.key.repo, pr.key.number))
}

/// The entries of `panel`'s menu, in groups, as the menu shows them: a pull
/// request's tab has every entry, any other tab the close entries (when it
/// closes) and the placement ones.
#[must_use]
pub fn entries(panel: PanelId, cx: &App) -> Vec<Vec<MenuEntry>> {
    let Some(shell) = Shell::global(cx) else {
        return Vec::new();
    };
    let dock = shell.read(cx).dock().read(cx);
    let Some(place) = place_of(dock, panel) else {
        return Vec::new();
    };
    let closable = |id: &PanelId| dock.panel(*id).is_some_and(|view| view.closable(cx));
    let at = place.panels.iter().position(|id| *id == panel).unwrap_or(0);
    let entry = |action, label, enabled| MenuEntry {
        action,
        label,
        enabled,
    };
    let pr = key_of(panel, cx);
    let mut groups = Vec::new();
    if pr.is_some() {
        groups.push(vec![
            entry(TabAction::CopyReference, "Copy reference", true),
            entry(TabAction::CopyLink, "Copy link", true),
            entry(TabAction::CopyBranch, "Copy branch name", true),
            entry(TabAction::CopyTitle, "Copy title and reference", true),
        ]);
    }
    if closable(&panel) {
        let others = place.panels.iter().any(|id| *id != panel && closable(id));
        let right = place.panels[at + 1..].iter().any(closable);
        let mut close = vec![
            entry(TabAction::Close, "Close", true),
            entry(TabAction::CloseOthers, "Close others", others),
            entry(TabAction::CloseToRight, "Close to the right", right),
            entry(TabAction::CloseAll, "Close all", true),
        ];
        if pr.is_some() {
            close.push(entry(
                TabAction::ClosePr,
                "Close this pull request's tabs",
                true,
            ));
        }
        groups.push(close);
    }
    let preview = pr
        .as_ref()
        .is_some_and(|key| Session::global(cx).read(cx).is_preview(key));
    if preview {
        groups.push(vec![entry(TabAction::KeepOpen, "Keep open", true)]);
    }
    let splits = place.panels.len() > 1;
    let other_area = if place.region == DockPlacement::Left {
        "Move to the main area"
    } else {
        "Move to the sidebar"
    };
    groups.push(vec![
        entry(TabAction::SplitRight, "Split right", splits),
        entry(TabAction::SplitDown, "Split down", splits),
        entry(TabAction::MoveToOtherArea, other_area, true),
    ]);
    if pr.is_some() {
        groups.push(vec![entry(TabAction::OpenOnForge, "Open on forge", true)]);
    }
    groups
}

/// `panel`'s menu.
fn popup(
    menu: PopupMenu,
    panel: PanelId,
    _: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    entries(panel, cx)
        .into_iter()
        .enumerate()
        .fold(menu, |menu, (ix, group)| {
            let menu = if ix > 0 { menu.separator() } else { menu };
            group.into_iter().fold(menu, |menu, entry| {
                let action = entry.action;
                menu.item(
                    PopupMenuItem::new(entry.label)
                        .disabled(!entry.enabled)
                        .on_click(move |_, window, cx| run(action, panel, window, cx)),
                )
            })
        })
}

/// Closes `panel` the way its close button does.
pub fn close(panel: PanelId, cx: &mut App) {
    let Some(id) = view_id(panel, cx) else {
        return;
    };
    Session::global(cx).update(cx, |session, cx| session.close_panel(id, cx));
}

/// The entity behind `panel`, if the dock holds it.
fn view_id(panel: PanelId, cx: &App) -> Option<EntityId> {
    let shell = Shell::global(cx)?;
    let view = shell.read(cx).dock().read(cx).panel(panel)?.view();
    Some(view.entity_id())
}

/// Does what `action` of `panel`'s menu says.
pub fn run(action: TabAction, panel: PanelId, window: &mut Window, cx: &mut App) {
    let Some(shell) = Shell::global(cx) else {
        return;
    };
    match action {
        TabAction::CopyReference => copy_from(panel, cx, |pr| pr.ref_label()),
        TabAction::CopyLink => copy_from(panel, cx, link),
        TabAction::CopyBranch => copy_from(panel, cx, |pr| pr.head.to_string()),
        TabAction::CopyTitle => {
            copy_from(panel, cx, |pr| format!("{} ({})", pr.title, pr.ref_label()))
        }
        TabAction::OpenOnForge => {
            if let Some(url) = pr_of(panel, cx).map(|pr| link(&pr)) {
                cx.open_url(&url);
            }
        }
        TabAction::KeepOpen => {
            if let Some(key) = key_of(panel, cx) {
                Session::global(cx).update(cx, |session, cx| session.keep(&key, cx));
            }
        }
        TabAction::Close => close(panel, cx),
        TabAction::CloseOthers | TabAction::CloseToRight | TabAction::CloseAll => {
            shell.update(cx, |shell, cx| {
                shell.close_in_group(panel, action, window, cx)
            });
        }
        TabAction::ClosePr => {
            if let Some(key) = key_of(panel, cx) {
                shell.update(cx, |shell, cx| shell.close_pr(&key, window, cx));
            }
        }
        TabAction::SplitRight => {
            shell.update(cx, |shell, cx| {
                shell.split(panel, Placement::Right, window, cx)
            });
        }
        TabAction::SplitDown => {
            shell.update(cx, |shell, cx| {
                shell.split(panel, Placement::Bottom, window, cx)
            });
        }
        TabAction::MoveToOtherArea => {
            shell.update(cx, |shell, cx| shell.move_to_other_area(panel, window, cx));
        }
    }
}

fn copy_from(panel: PanelId, cx: &mut App, text: impl FnOnce(&PullRequest) -> String) {
    if let Some(pr) = pr_of(panel, cx) {
        cx.write_to_clipboard(ClipboardItem::new_string(text(&pr)));
    }
}

/// Where a tab moved to the other area lands: the main area's group of the
/// last pull request shown, else its first group, else a new one; or the
/// sidebar's first group.
pub(crate) fn other_area_target(
    dock: &DockArea,
    from: DockPlacement,
    main_group: Option<NodeId>,
) -> Option<(DockPlacement, InsertTarget)> {
    let to = if from == DockPlacement::Left {
        DockPlacement::Center
    } else {
        DockPlacement::Left
    };
    let tree = dock.layout(to)?;
    let group = match to {
        DockPlacement::Center => main_group.or_else(|| first_group(tree.root())),
        _ => first_group(tree.root()),
    };
    let target = match group {
        Some(node) => InsertTarget::Tabs {
            node,
            ix: None,
            activate: true,
        },
        None => InsertTarget::Split {
            node: tree.root().id(),
            placement: Placement::Right,
            size: None,
        },
    };
    Some((to, target))
}
