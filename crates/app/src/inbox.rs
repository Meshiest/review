//! The inbox panel's content: its sections (Review requested, Created by me,
//! one per repository) as a vertical group the user resizes, each a
//! virtualized list that scrolls on its own; optionally grouped by
//! repository. Also the Repos panel's list, which hides repositories from
//! the inbox's view.
//!
//! Hiding is display only: hidden repositories are still read, counted,
//! watched and notified about.
//!
//! Rows carry the local review marks ([`marks`]), a right-click menu of
//! actions on the pull request ([`row_menu`]), and the inbox's tab a refresh
//! button ([`refresh`]).

pub mod marks;
pub mod refresh;
pub mod row_menu;

use std::cell::Cell;
use std::collections::{BTreeSet, HashSet};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};

use forge::model::PrState;
use gpui::{
    Anchor, AnyElement, App, AppContext as _, Axis, ClickEvent, Context, DragMoveEvent, Empty,
    Entity, FontWeight, InteractiveElement as _, IntoElement, MouseButton, ParentElement, Pixels,
    Render, SharedString, StatefulInteractiveElement as _, Styled, WeakEntity, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::{
    Icon, Sizable as _,
    button::{Button as FlatButton, ButtonVariants as _},
    h_flex,
    input::InputState,
    menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem},
    resizable::{HandleEdge, resize_handle},
    v_flex,
};
use scroll::Scroller;
use serde::{Deserialize, Serialize};
use theme::Theme;

use crate::appearance::{self, Appearance};
use crate::config::SectionKind;
use crate::model::{
    Inbox, PullRequest, RepoRef, ReviewStatus, RowReview, SectionState, relative_time,
};
use crate::references;
use crate::settings::InboxSettings;
use crate::sidebar::{Cursor, ROW, Sidebar, SidebarEvent};
use crate::subscriptions::Subscriptions;
use crate::ui::list::{RowTops, virtual_list};
use crate::ui::spinner::SpinnerSlots;
use crate::ui::{self, Button, ButtonKind, CI_ROW, TextExt as _, review_color};
use marks::{Progress, ReviewedMark};

/// How often the inbox body and its rows were built, for the benchmarks to
/// see what a scrolled frame re-renders.
pub static BODY_RENDERS: AtomicUsize = AtomicUsize::new(0);
pub static ROWS_BUILT: AtomicUsize = AtomicUsize::new(0);

/// A pull request row has three lines: the title; the reference, author and
/// age with the discussion counts; and the review state with the viewed
/// progress and lines changed.
const TITLE_LINE: f32 = 20.0;
const META_LINE: f32 = 16.0;
const LINE_GAP: f32 = 1.0;
const ROW_PAD: f32 = 5.0;
pub const PR_ROW: f32 = 2.0 * ROW_PAD + TITLE_LINE + 2.0 * META_LINE + 2.0 * LINE_GAP;
const GROUP_ROW: f32 = 26.0;
/// How small a drag may make an open section: its header and a row.
pub const SECTION_MIN: f32 = ROW + PR_ROW;
/// How small dragging a handle makes a section: its header and two rows.
pub const DRAG_MIN: f32 = ROW + 2.0 * PR_ROW;
/// The indent of a row under a repository group or a stack.
const GROUP_INDENT: f32 = 14.0;

/// One row of a section's list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InboxRow {
    /// A repository's group: its first pull request (whose repository it
    /// is) and how many it holds.
    Group {
        section: usize,
        first: usize,
        count: usize,
        folded: bool,
    },
    /// A stack within a repository's group: its bottom pull request (in
    /// the whole stack, which names it) and how many of it the group holds.
    Stack {
        section: usize,
        bottom: usize,
        count: usize,
        folded: bool,
    },
    Pr {
        section: usize,
        pr: usize,
        /// 0 in a flat list, 1 under a repository, 2 under a stack too.
        depth: u8,
    },
}

impl InboxRow {
    fn height(self) -> f32 {
        match self {
            Self::Group { .. } | Self::Stack { .. } => GROUP_ROW,
            Self::Pr { .. } => PR_ROW,
        }
    }
}

/// One section's rows and scroll position, an entity so its virtualized
/// list can hold it.
pub(crate) struct SectionList {
    sidebar: WeakEntity<Sidebar>,
    scroll: Scroller,
    rows: Rc<[InboxRow]>,
    tops: RowTops,
    /// The inbox panel's spinner slots, which the rows draw into.
    slots: SpinnerSlots,
}

/// A view, so a wheel step that notifies it redraws this list (and the views
/// above it), not a cached view that never read it.
impl Render for SectionList {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        virtual_list(
            cx.entity(),
            |list: &mut SectionList| &mut list.scroll,
            self.tops.clone(),
            SectionList::row,
        )
    }
}

impl SectionList {
    fn row(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let (Some(row), Some(sidebar)) = (self.rows.get(ix).copied(), self.sidebar.upgrade())
        else {
            return Empty.into_any_element();
        };
        let slots = self.slots.clone();
        sidebar.update(cx, |sidebar, cx| {
            sidebar.spinners = slots;
            sidebar.inbox_row(row, window, cx)
        })
    }
}

/// Whether a section titled `title` is the built-in review requested one,
/// the only one that can leave drafts out.
fn can_hide_drafts(title: &str) -> bool {
    title == SectionKind::ReviewRequested.title()
}

/// An entry of a section header's right-click menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SectionMenuItem {
    Fold,
    Unfold,
    /// Only on the review requested section.
    HideDrafts,
    /// Only on a repository's section.
    RemoveFromInbox,
}

impl SectionMenuItem {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Fold => "Fold",
            Self::Unfold => "Unfold",
            Self::HideDrafts => "Hide drafts",
            Self::RemoveFromInbox => "Remove from inbox",
        }
    }
}

/// What "Remove from inbox" does: the repository's sections leave the sidebar
/// and the session's inbox with their saved fold and hide state, and a live
/// session stops fetching them and takes the `[[repos]]` table out of the
/// config file. A fixtures session has no file and no sources, so only the
/// first part applies.
pub fn remove_repo_from_inbox(repo: &RepoRef, cx: &mut App) {
    Sidebar::global(cx).update(cx, |sidebar, cx| sidebar.forget_repo_sections(repo, cx));
    let session = crate::session::Session::global(cx);
    session.update(cx, |session, cx| session.drop_repo_sections(repo, cx));
    // After the update: the live remover publishes the inbox again.
    if let Some(remover) = session.read(cx).repo_remover() {
        remover(repo, cx);
    }
}

/// How one section is drawn.
#[derive(Clone, Debug, PartialEq)]
struct SectionLayout {
    title: SharedString,
    collapsed: bool,
    /// Its height as a fraction of the space the open sections have, once
    /// the user dragged a handle. The shares of the open sections add up to
    /// at most one; what is under one is empty space below the last
    /// section. `None` sizes it by its rows. Kept while folded, so
    /// unfolding takes its share back.
    share: Option<f32>,
    /// Whether draft pull requests are left out of it (only the review
    /// requested section offers this).
    hide_drafts: bool,
}

/// What the layout saves of the inbox.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedInbox {
    pub sections: Vec<SavedSection>,
    pub group_by_repo: bool,
}

/// A section's saved state, by its title.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedSection {
    pub title: String,
    pub collapsed: bool,
    /// Its share of the space, as [`SectionLayout`] keeps it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub share: Option<f32>,
    /// An older layout's relative weight, which filled the panel; read as a
    /// share by [`legacy_shares`].
    #[serde(skip_serializing)]
    pub weight: Option<f32>,
    /// An even older layout's height in pixels; read the same way.
    #[serde(skip_serializing)]
    pub size: Option<f32>,
    /// Draft pull requests are left out of the section.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub hide_drafts: bool,
}

impl SavedSection {
    fn legacy(&self) -> Option<f32> {
        self.weight
            .or(self.size)
            .filter(|w| w.is_finite() && *w > 0.0)
    }

    fn share(&self) -> Option<f32> {
        self.share.filter(|s| s.is_finite() && *s > 0.0)
    }
}

/// Turns an older layout's weights and pixel heights into shares, relative
/// to the open sections, which they filled.
fn legacy_shares(sections: &mut [SavedSection]) {
    let sum = |only_open: bool| -> f32 {
        sections
            .iter()
            .filter(|s| s.share().is_none() && !(only_open && s.collapsed))
            .filter_map(SavedSection::legacy)
            .sum()
    };
    let open = sum(true);
    let total = if open > 0.0 { open } else { sum(false) };
    for section in sections {
        if section.share().is_none() {
            section.share = section
                .legacy()
                .filter(|_| total > 0.0)
                .map(|w| (w / total).min(1.0));
        }
        section.weight = None;
        section.size = None;
    }
}

/// The inbox's own state in the sidebar.
#[derive(Default)]
pub(crate) struct InboxState {
    sections: Vec<SectionLayout>,
    lists: Vec<Entity<SectionList>>,
    group_by_repo: bool,
    /// Folded repository groups, by section title and repository.
    folded: HashSet<(SharedString, String)>,
    /// The filter box at the top of the inbox.
    search: Option<Entity<InputState>>,
    /// What it holds, trimmed and lowercased: rows show only repositories
    /// whose name contains it.
    filter: String,
    /// The Repos panel's search box, and what it holds the same way: the
    /// panel lists only repositories whose name contains it.
    repos_search: Option<Entity<InputState>>,
    repos_filter: String,
    /// Saved sections, applied when a section of that title appears.
    saved: Vec<SavedSection>,
    /// The sections' height in the last frame, which they are allotted.
    height: Rc<Cell<Pixels>>,
    /// Each section's allotted height in the last frame.
    heights: Vec<f32>,
    /// The action a row's menu started, in the line under the filter.
    acting: Option<row_menu::Acting>,
    /// Where the refresh button's spinner is drawn while it runs.
    pub(crate) refresh_slots: SpinnerSlots,
}

/// What a section asks for: folded, its dragged share, or its rows'.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Want {
    pub collapsed: bool,
    pub share: Option<f32>,
    /// Header and rows.
    pub content: f32,
}

impl Want {
    fn min(self) -> f32 {
        if self.collapsed {
            ROW
        } else {
            SECTION_MIN.min(self.content)
        }
    }
}

/// The last open section, which fills what the others leave.
fn last_open(wants: &[Want]) -> Option<usize> {
    wants.iter().rposition(|w| !w.collapsed)
}

/// The height of each section in `avail`. A folded one is its header, and
/// the last open one takes whatever the others leave, so the open sections
/// fill the panel.
///
/// Once a handle was dragged each open section above the last is its share
/// of the space the folded ones leave (a section without one asks for its
/// rows), so a taller or shorter panel scales every section alike; when the
/// shares together exceed the space they are scaled down to fit, and none
/// goes under its least. Before that, open ones top down get their rows'
/// height, leaving room for the least each section under them needs.
pub(crate) fn allot(avail: f32, wants: &[Want]) -> Vec<f32> {
    let mut heights = if wants.iter().any(|w| !w.collapsed && w.share.is_some()) {
        allot_by_share(avail, wants)
    } else {
        allot_top_down(avail, wants)
    };
    if let Some(last) = last_open(wants) {
        let used: f32 = heights.iter().sum();
        heights[last] += (avail - used).max(0.0);
    }
    heights
}

fn allot_top_down(avail: f32, wants: &[Want]) -> Vec<f32> {
    let mut left = avail;
    wants
        .iter()
        .enumerate()
        .map(|(ix, want)| {
            let after: f32 = wants[ix + 1..].iter().map(|w| w.min()).sum();
            let height = if want.collapsed {
                ROW
            } else {
                want.content.min(left - after).max(want.min())
            };
            left -= height;
            height
        })
        .collect()
}

fn allot_by_share(avail: f32, wants: &[Want]) -> Vec<f32> {
    let folded = wants.iter().filter(|w| w.collapsed).count() as f32 * ROW;
    let space = (avail - folded).max(0.0);
    let last = last_open(wants);
    let asked = |w: &Want| w.share.unwrap_or_else(|| w.content / space.max(1.0));
    // The last open one's share is what the others leave.
    let rest = 1.0
        - (0..wants.len())
            .filter(|&ix| !wants[ix].collapsed && Some(ix) != last)
            .map(|ix| asked(&wants[ix]))
            .sum::<f32>();
    let shares: Vec<f32> = (0..wants.len())
        .map(|ix| {
            if Some(ix) == last {
                rest
            } else {
                asked(&wants[ix])
            }
            .max(f32::EPSILON)
        })
        .collect();
    let all: f32 = (0..wants.len())
        .filter(|&ix| !wants[ix].collapsed)
        .map(|ix| shares[ix])
        .sum();
    let widest = space / all.max(1.0);
    let mut heights: Vec<Option<f32>> = wants.iter().map(|w| w.collapsed.then_some(ROW)).collect();
    // A section whose share is under its least takes the least; the others
    // share what is left, until no share is too small.
    loop {
        let free: Vec<usize> = (0..wants.len())
            .filter(|&ix| heights[ix].is_none())
            .collect();
        let fixed: f32 = (0..wants.len())
            .filter(|&ix| !wants[ix].collapsed)
            .filter_map(|ix| heights[ix])
            .sum();
        let left = (space - fixed).max(0.0);
        let total: f32 = free.iter().map(|&ix| shares[ix]).sum();
        let unit = if total > 0.0 {
            widest.min(left / total)
        } else {
            widest
        };
        let short: Vec<usize> = free
            .iter()
            .copied()
            .filter(|&ix| shares[ix] * unit < wants[ix].min())
            .collect();
        if short.is_empty() {
            for ix in free {
                heights[ix] = Some(shares[ix] * unit);
            }
            break;
        }
        for ix in short {
            heights[ix] = Some(wants[ix].min());
        }
    }
    heights.into_iter().map(|h| h.unwrap_or(ROW)).collect()
}

/// The payload of a section's resize drag: the section above the handle.
#[derive(Clone, Copy)]
struct ResizeSection(usize);

impl Render for ResizeSection {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// The filter box's key context, where `Esc` empties it.
pub const FILTER_CONTEXT: &str = "RepoFilter";

/// The key context of a fold header, where `Enter` and `Space` fold it.
pub const FOLD_CONTEXT: &str = "FoldHeader";

gpui::actions!(
    inbox,
    [
        /// `Esc` in the filter box: empties it.
        ClearFilter,
        /// `Enter` or `Space` on a focused fold header: folds or unfolds it.
        ToggleFold,
    ]
);

/// The group name of an inbox row, which its "..." button shows on hover.
const ROW_GROUP: &str = "inbox-row";

/// `host/owner/name`, as the settings name a repository.
#[must_use]
pub fn repo_id(repo: &RepoRef) -> String {
    format!("{}/{}", repo.host, repo.path())
}

/// The counts an inbox row shows, each only when the forge said: comments
/// and unresolved threads at the end of the author line, lines changed at
/// the end of the status line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowCount {
    Comments(u32),
    Unresolved(u32),
    Lines { added: u32, removed: u32 },
}

#[must_use]
pub fn row_counts(pr: &PullRequest) -> Vec<RowCount> {
    let mut counts = Vec::new();
    if let Some(n) = pr.comments.filter(|n| *n > 0) {
        counts.push(RowCount::Comments(n));
    }
    if let Some(n) = pr.unresolved.filter(|n| *n > 0) {
        counts.push(RowCount::Unresolved(n));
    }
    if let Some((added, removed)) = pr.stat {
        counts.push(RowCount::Lines { added, removed });
    }
    counts
}

/// The unread rows, by index into the inbox's pull requests, when the
/// settings sort them first.
fn unread_rows(inbox: &Inbox, settings: &InboxSettings, cx: &App) -> Option<HashSet<usize>> {
    if !settings.unread_first {
        return None;
    }
    let marks = marks::Marks::try_global(cx)?;
    let marks = marks.read(cx);
    Some(
        inbox
            .prs
            .iter()
            .enumerate()
            .filter(|(_, pr)| marks.row(pr).unread)
            .map(|(ix, _)| ix)
            .collect(),
    )
}

/// Whether `repo` (as [`repo_id`] names it) passes the inbox filter: its
/// name contains `filter`, ignoring case. `filter` is already lowercased.
fn matches_filter(repo: &str, filter: &str) -> bool {
    filter.is_empty() || repo.to_lowercase().contains(filter)
}

/// What leaves pull requests out of a section's rows.
#[derive(Clone, Copy)]
struct Hiding<'a> {
    /// Hidden repositories.
    repos: &'a InboxSettings,
    /// The filter box's text, lowercased: only repositories containing it.
    filter: &'a str,
    /// Draft pull requests.
    drafts: bool,
}

/// What a stack is folded by: its repository and its bottom pull request.
fn stack_id(bottom: &PullRequest) -> String {
    format!("{}#{}", repo_id(&bottom.key.repo), bottom.key.number)
}

/// The rows of one section: its pull requests that `hiding` leaves in, under
/// a header per repository when grouped. There, two or more of a stack
/// (from `stacks`, each bottom first) go under a header of their own, top
/// first as the Stack section draws them.
fn section_rows(
    inbox: &Inbox,
    section: usize,
    grouped: bool,
    hiding: Hiding,
    folded: &HashSet<(SharedString, String)>,
    unread: Option<&HashSet<usize>>,
    stacks: &[Vec<usize>],
) -> Vec<InboxRow> {
    let Some(spec) = inbox.sections.get(section) else {
        return Vec::new();
    };
    let mut shown = spec
        .items
        .iter()
        .copied()
        .filter(|&ix| {
            inbox.prs.get(ix).is_some_and(|pr| {
                let repo = repo_id(&pr.key.repo);
                !hiding.repos.is_hidden(&repo)
                    && matches_filter(&repo, hiding.filter)
                    && !(hiding.drafts && pr.state == PrState::Draft)
            })
        })
        .collect::<Vec<_>>();
    if let Some(unread) = unread {
        // Stable: within each half the newest stay first.
        shown.sort_by_key(|ix| !unread.contains(ix));
    }
    if !grouped {
        return shown
            .into_iter()
            .map(|pr| InboxRow::Pr {
                section,
                pr,
                depth: 0,
            })
            .collect();
    }
    // Groups in the order their first pull request appears.
    let mut order: Vec<(String, Vec<usize>)> = Vec::new();
    for ix in shown {
        let id = repo_id(&inbox.prs[ix].key.repo);
        match order.iter_mut().find(|(repo, _)| *repo == id) {
            Some((_, items)) => items.push(ix),
            None => order.push((id, vec![ix])),
        }
    }
    let mut rows = Vec::new();
    for (id, items) in order {
        let is_folded = |id: String| folded.contains(&(spec.title.clone(), id));
        let repo_folded = is_folded(id);
        rows.push(InboxRow::Group {
            section,
            first: items[0],
            count: items.len(),
            folded: repo_folded,
        });
        if repo_folded {
            continue;
        }
        let mut placed = HashSet::new();
        for &ix in &items {
            if placed.contains(&ix) {
                continue;
            }
            let stack = stacks
                .iter()
                .find(|stack| stack.contains(&ix))
                .map(|stack| {
                    let members: Vec<usize> = stack
                        .iter()
                        .rev()
                        .copied()
                        .filter(|m| items.contains(m))
                        .collect();
                    (stack[0], members)
                })
                .filter(|(_, members)| members.len() >= 2);
            let Some((bottom, members)) = stack else {
                rows.push(InboxRow::Pr {
                    section,
                    pr: ix,
                    depth: 1,
                });
                continue;
            };
            let folded = is_folded(stack_id(&inbox.prs[bottom]));
            rows.push(InboxRow::Stack {
                section,
                bottom,
                count: members.len(),
                folded,
            });
            placed.extend(members.iter().copied());
            if !folded {
                rows.extend(members.into_iter().map(|pr| InboxRow::Pr {
                    section,
                    pr,
                    depth: 2,
                }));
            }
        }
    }
    rows
}

impl Sidebar {
    /// Sections can appear once the config is read; each starts as its
    /// config or the saved layout says, and ones already shown keep their
    /// state.
    pub(crate) fn sync_sections(&mut self, cx: &mut Context<Self>) {
        let inbox = self.session.read(cx).inbox.clone();
        let state = &mut self.inbox;
        // A section that moved (one before it left) keeps its layout.
        let old = std::mem::take(&mut state.sections);
        state.sections = inbox
            .sections
            .iter()
            .enumerate()
            .map(|(ix, spec)| {
                let same = |layout: &&SectionLayout| layout.title == spec.title;
                if let Some(layout) = old.get(ix).filter(same).or_else(|| old.iter().find(same)) {
                    return layout.clone();
                }
                let saved = state
                    .saved
                    .iter()
                    .find(|saved| saved.title == spec.title.as_ref());
                SectionLayout {
                    title: spec.title.clone(),
                    collapsed: saved.map_or(!spec.expanded_by_default, |s| s.collapsed),
                    share: saved.and_then(SavedSection::share),
                    hide_drafts: saved
                        .is_some_and(|s| s.hide_drafts && can_hide_drafts(spec.title.as_ref())),
                }
            })
            .collect();
        let weak = cx.weak_entity();
        let style = ui::scroll_style(&ui::theme(cx));
        state.lists.truncate(inbox.sections.len());
        while state.lists.len() < inbox.sections.len() {
            let sidebar = weak.clone();
            state.lists.push(cx.new(|_| SectionList {
                sidebar,
                scroll: Scroller::new(style),
                rows: Rc::new([]),
                tops: RowTops::default(),
                slots: SpinnerSlots::default(),
            }));
        }
    }

    pub(crate) fn restyle(&mut self, cx: &mut Context<Self>) {
        let style = ui::scroll_style(&ui::theme(cx));
        for list in &self.inbox.lists {
            list.update(cx, |list, _| list.scroll.set_style(style));
        }
    }

    /// The inbox's part of the saved layout.
    #[must_use]
    pub fn saved_inbox(&self) -> SavedInbox {
        let state = &self.inbox;
        let mut sections: Vec<SavedSection> = state
            .sections
            .iter()
            .map(|layout| SavedSection {
                title: layout.title.to_string(),
                collapsed: layout.collapsed,
                share: layout.share,
                weight: None,
                size: None,
                hide_drafts: layout.hide_drafts,
            })
            .collect();
        // Sections not listed now (a forge still loading) keep their state.
        for saved in &state.saved {
            if !sections.iter().any(|s| s.title == saved.title) {
                sections.push(saved.clone());
            }
        }
        SavedInbox {
            sections,
            group_by_repo: state.group_by_repo,
        }
    }

    /// Takes the inbox's part of a restored layout.
    pub fn restore_inbox(&mut self, mut saved: SavedInbox, cx: &mut Context<Self>) {
        legacy_shares(&mut saved.sections);
        self.inbox.group_by_repo = saved.group_by_repo;
        for layout in &mut self.inbox.sections {
            if let Some(entry) = saved
                .sections
                .iter()
                .find(|s| s.title == layout.title.as_ref())
            {
                layout.collapsed = entry.collapsed;
                layout.share = entry.share();
                layout.hide_drafts = entry.hide_drafts && can_hide_drafts(&layout.title);
            }
        }
        self.inbox.saved = saved.sections;
        cx.notify();
    }

    fn layout_changed(&mut self, cx: &mut Context<Self>) {
        cx.emit(SidebarEvent::LayoutChanged);
        cx.notify();
    }

    /// Whether the inbox groups each section by repository.
    #[must_use]
    pub fn grouped_by_repo(&self) -> bool {
        self.inbox.group_by_repo
    }

    pub fn set_grouped_by_repo(&mut self, grouped: bool, cx: &mut Context<Self>) {
        if self.inbox.group_by_repo != grouped {
            self.inbox.group_by_repo = grouped;
            self.layout_changed(cx);
        }
    }

    /// Whether section `ix` is folded to its header.
    #[must_use]
    pub fn section_collapsed(&self, ix: usize) -> Option<bool> {
        self.inbox.sections.get(ix).map(|layout| layout.collapsed)
    }

    pub fn toggle_section(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(layout) = self.inbox.sections.get_mut(ix) {
            layout.collapsed = !layout.collapsed;
            self.layout_changed(cx);
        }
    }

    /// Whether section `ix` leaves draft pull requests out.
    #[must_use]
    pub fn section_hides_drafts(&self, ix: usize) -> bool {
        self.inbox
            .sections
            .get(ix)
            .is_some_and(|layout| layout.hide_drafts)
    }

    /// Leaves draft pull requests out of section `ix`, or lists them again.
    /// Display only, like hiding a repository; only the review requested
    /// section can.
    pub fn set_section_hides_drafts(&mut self, ix: usize, hide: bool, cx: &mut Context<Self>) {
        let Some(layout) = self.inbox.sections.get_mut(ix) else {
            return;
        };
        if layout.hide_drafts == hide || !can_hide_drafts(&layout.title) {
            return;
        }
        layout.hide_drafts = hide;
        self.layout_changed(cx);
    }

    /// What section `ix`'s header menu offers.
    #[must_use]
    pub fn section_menu(&self, ix: usize, cx: &App) -> Vec<SectionMenuItem> {
        let inbox = &self.session.read(cx).inbox;
        let Some(spec) = inbox.sections.get(ix) else {
            return Vec::new();
        };
        let mut items = vec![if self.section_collapsed(ix).unwrap_or(false) {
            SectionMenuItem::Unfold
        } else {
            SectionMenuItem::Fold
        }];
        if can_hide_drafts(&spec.title) {
            items.push(SectionMenuItem::HideDrafts);
        }
        if spec.repo.is_some() {
            items.push(SectionMenuItem::RemoveFromInbox);
        }
        items
    }

    /// Forgets the saved fold, share and hide state of the sections for
    /// `repo`, so the layout saved next no longer lists them. Called before
    /// the session drops the sections.
    fn forget_repo_sections(&mut self, repo: &RepoRef, cx: &mut Context<Self>) {
        let inbox = self.session.read(cx).inbox.clone();
        let titles: Vec<&SharedString> = inbox
            .sections
            .iter()
            .filter(|section| section.repo.as_ref() == Some(repo))
            .map(|section| &section.title)
            .collect();
        if titles.is_empty() {
            return;
        }
        let state = &mut self.inbox;
        state
            .saved
            .retain(|saved| !titles.iter().any(|title| title.as_ref() == saved.title));
        state.folded.retain(|(title, _)| !titles.contains(&title));
        state
            .sections
            .retain(|layout| !titles.contains(&&layout.title));
        self.layout_changed(cx);
    }

    /// Section `ix`'s share of the space, once a handle was dragged.
    #[must_use]
    pub fn section_size(&self, ix: usize) -> Option<f32> {
        self.inbox.sections.get(ix).and_then(|layout| layout.share)
    }

    /// Turns the wheel over section `ix` by `dy` pixels (down is positive),
    /// as the list's own wheel handler does. Returns the offset reached and
    /// the furthest it can go, in pixels.
    pub fn scroll_section(&mut self, ix: usize, dy: f64, cx: &mut Context<Self>) -> (f64, f64) {
        let Some(list) = self.inbox.lists.get(ix).cloned() else {
            return (0.0, 0.0);
        };
        list.update(cx, |list, cx| {
            if list.scroll.state.y.by(dy) {
                cx.notify();
            }
            let y = &list.scroll.state.y;
            (y.offset, (y.content - y.viewport).max(0.0))
        })
    }

    /// Each section's height in the last frame.
    #[must_use]
    pub fn section_heights(&self) -> &[f32] {
        &self.inbox.heights
    }

    /// The handle under section `ix` was dragged so that it would be `size`
    /// tall, not under about two rows. Shrinking gives the space to the next
    /// open section; growing takes from the open sections after it, down to
    /// their least; folded ones are skipped. The last open section has no
    /// handle: it fills what the others leave. Every open section's share is
    /// then what it shows now, so later resizes of the panel keep them.
    pub fn resize_section(&mut self, ix: usize, size: f32, cx: &mut Context<Self>) {
        let heights = self.inbox.heights.clone();
        let sections = &self.inbox.sections;
        let count = heights.len().min(sections.len());
        if ix >= count || sections[ix].collapsed {
            return;
        }
        let Some(next) = (ix + 1..count).find(|&n| !sections[n].collapsed) else {
            return;
        };
        let folded = sections[..count].iter().filter(|s| s.collapsed).count() as f32;
        let space = (f32::from(self.inbox.height.get()) - folded * ROW).max(1.0);
        let least = |n: usize| {
            let rows = self
                .inbox
                .lists
                .get(n)
                .map_or(0.0, |list| list.read(cx).tops.total());
            DRAG_MIN.min(ROW + rows).min(heights[n])
        };
        let used: f32 = (0..count)
            .filter(|&n| !sections[n].collapsed)
            .map(|n| heights[n])
            .sum();
        let mut new_heights = heights.clone();
        let new = size.max(least(ix));
        new_heights[ix] = new;
        let grow = new - heights[ix];
        if grow > 0.0 {
            let mut need = (grow - (space - used).max(0.0)).max(0.0);
            for n in (ix + 1..count).filter(|&n| !sections[n].collapsed) {
                let give = (heights[n] - least(n)).clamp(0.0, need);
                new_heights[n] -= give;
                need -= give;
            }
            new_heights[ix] -= need;
        } else {
            new_heights[next] -= grow;
        }
        let unchanged = (0..count).all(|n| {
            sections[n].collapsed
                || sections[n]
                    .share
                    .is_some_and(|share| (share * space - new_heights[n]).abs() < 0.5)
        });
        if unchanged {
            return;
        }
        for (n, layout) in self.inbox.sections.iter_mut().enumerate().take(count) {
            if !layout.collapsed {
                layout.share = Some(new_heights[n] / space);
            }
        }
        self.layout_changed(cx);
    }

    /// Folds or unfolds a repository group (by [`repo_id`]) or a stack (by
    /// [`stack_id`]) in a section.
    fn toggle_fold(&mut self, section: usize, id: String, cx: &mut Context<Self>) {
        let Some(layout) = self.inbox.sections.get(section) else {
            return;
        };
        let key = (layout.title.clone(), id);
        if !self.inbox.folded.remove(&key) {
            self.inbox.folded.insert(key);
        }
        cx.notify();
    }

    /// The filter box at the top of the inbox, made on first use. Each
    /// change filters the rows at once.
    pub fn repo_filter(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputState> {
        if let Some(search) = &self.inbox.search {
            return search.clone();
        }
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Filter repositories"));
        cx.observe(&search, |this: &mut Self, search, cx| {
            let filter = search.read(cx).value().trim().to_lowercase();
            if filter != this.inbox.filter {
                this.inbox.filter = filter;
                cx.notify();
            }
        })
        .detach();
        self.inbox.search = Some(search.clone());
        search
    }

    /// The filter box's row, always at the top of the inbox panel. `Esc`
    /// in it empties it; the inbox's own keys stay out of it.
    pub(crate) fn filter_row(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let search = self.repo_filter(window, cx);
        let sidebar = cx.entity();
        h_flex()
            .id("repo-filter")
            .debug_selector(|| "repo-filter".into())
            .key_context(FILTER_CONTEXT)
            .on_action(move |_: &ClearFilter, window, cx| {
                sidebar.update(cx, |sidebar, cx| sidebar.clear_repo_filter(window, cx));
            })
            .flex_none()
            .px(px(8.0))
            .py(px(3.0))
            .child(ui::search_input(&search, cx))
            .into_any_element()
    }

    /// Empties the filter box (`Esc` in it).
    pub fn clear_repo_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(search) = self.inbox.search.clone() {
            search.update(cx, |state, cx| state.set_value("", window, cx));
        }
        self.inbox.filter.clear();
        cx.notify();
    }

    /// The pull request the inbox cursor is on.
    #[must_use]
    pub fn cursor_key(&self) -> Option<&crate::model::PrKey> {
        self.cursor.as_ref().map(|cursor| &cursor.key)
    }

    /// What the filter box holds, lowercased.
    #[must_use]
    pub fn filter_text(&self) -> &str {
        &self.inbox.filter
    }

    /// The hidden repositories, as the settings hold them.
    fn hidden(cx: &App) -> InboxSettings {
        cx.try_global::<Appearance>()
            .map(|state| state.settings.inbox.clone())
            .unwrap_or_default()
    }

    /// Hides or shows `repo` in the inbox, and saves that in the settings.
    pub fn set_repo_hidden(&mut self, repo: &str, hidden: bool, cx: &mut Context<Self>) {
        let repo = repo.to_owned();
        appearance::update_quiet(cx, move |settings| {
            settings.inbox.set_hidden(&repo, hidden);
        });
        cx.notify();
    }

    /// Shows or hides every repository the Repos list holds.
    pub fn set_all_hidden(&mut self, hidden: bool, cx: &mut Context<Self>) {
        let repos = self.repos(cx);
        appearance::update_quiet(cx, move |settings| {
            if hidden {
                for repo in &repos {
                    settings.inbox.set_hidden(repo, true);
                }
            } else {
                settings.inbox.hidden_repos.clear();
            }
        });
        cx.notify();
    }

    /// Every repository seen: in the inbox now, or hidden earlier.
    #[must_use]
    pub fn repos(&self, cx: &App) -> Vec<String> {
        let inbox = &self.session.read(cx).inbox;
        let mut repos: BTreeSet<String> =
            inbox.prs.iter().map(|pr| repo_id(&pr.key.repo)).collect();
        repos.extend(Self::hidden(cx).hidden_repos);
        repos.into_iter().collect()
    }

    /// How many of the repositories seen are shown, and how many there are.
    #[must_use]
    pub fn repos_shown(&self, cx: &App) -> (usize, usize) {
        let hidden = Self::hidden(cx);
        let repos = self.repos(cx);
        let shown = repos.iter().filter(|r| !hidden.is_hidden(r)).count();
        (shown, repos.len())
    }

    /// Every pull request row the keys walk, in order, with its section
    /// and its row in that section's list.
    fn key_rows(&self, cx: &App) -> Vec<(usize, usize, usize)> {
        let session = self.session.read(cx);
        let (inbox, stacks) = (&session.inbox, session.inbox_stacks());
        let hidden = Self::hidden(cx);
        let unread = unread_rows(inbox, &hidden, cx);
        let mut out = Vec::new();
        for (section, layout) in self.inbox.sections.iter().enumerate() {
            if layout.collapsed {
                continue;
            }
            let rows = section_rows(
                inbox,
                section,
                self.inbox.group_by_repo,
                Hiding {
                    repos: &hidden,
                    filter: &self.inbox.filter,
                    drafts: layout.hide_drafts,
                },
                &self.inbox.folded,
                unread.as_ref(),
                &stacks,
            );
            for (row_ix, row) in rows.into_iter().enumerate() {
                if let InboxRow::Pr { pr, .. } = row {
                    out.push((section, row_ix, pr));
                }
            }
        }
        out
    }

    /// `j` and `k`: the next or previous pull request row, scrolled into its
    /// section's view.
    pub(crate) fn move_cursor(&mut self, step: isize, cx: &mut Context<Self>) {
        let rows = self.key_rows(cx);
        if rows.is_empty() {
            return;
        }
        let prs = &self.session.read(cx).inbox.prs;
        let current = self.cursor.as_ref().and_then(|cursor| {
            rows.iter().position(|&(section, _, pr)| {
                section == cursor.section && prs.get(pr).map(|p| &p.key) == Some(&cursor.key)
            })
        });
        let next = match current {
            None if step > 0 => 0,
            None => rows.len() - 1,
            Some(ix) => ix.saturating_add_signed(step).min(rows.len() - 1),
        };
        let (section, row_ix, pr) = rows[next];
        let Some(key) = prs.get(pr).map(|p| p.key.clone()) else {
            return;
        };
        self.cursor = Some(Cursor {
            section,
            key: key.clone(),
        });
        if let Some(list) = self.inbox.lists.get(section) {
            list.update(cx, |list, _| {
                let axis = &list.scroll.state.y;
                let y = list.tops.reveal(row_ix, axis.offset, axis.viewport);
                list.scroll.state.y.set(y);
            });
        }
        self.intend(key, cx);
        cx.notify();
    }

    pub(crate) fn inbox_row(
        &mut self,
        row: InboxRow,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = ui::theme(cx);
        ROWS_BUILT.fetch_add(1, Ordering::Relaxed);
        match row {
            InboxRow::Group {
                section,
                first,
                count,
                folded,
            } => self.group_row(section, first, count, folded, &t, cx),
            InboxRow::Stack {
                section,
                bottom,
                count,
                folded,
            } => self.stack_row(section, bottom, count, folded, &t, cx),
            InboxRow::Pr { section, pr, depth } => self.pr_row(section, pr, depth, &t, cx),
        }
    }

    /// A repository's group header: fold chevron, name, count, and a menu
    /// to hide it or open it on its forge.
    fn group_row(
        &self,
        section: usize,
        first: usize,
        count: usize,
        folded: bool,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = &t.colors;
        let inbox = self.session.read(cx).inbox.clone();
        let Some(pr) = inbox.prs.get(first) else {
            return Empty.into_any_element();
        };
        let repo = pr.key.repo.clone();
        let name = repo.name.clone();
        let hover = c.hover;
        let (fold, more_repo, menu_repo) = (repo.clone(), repo.clone(), repo.clone());
        let menu_selector = format!("repo-menu-{name}");
        let row_selector = format!("repo-group-{section}-{name}");
        h_flex()
            .id(SharedString::from(format!(
                "repo-group-{section}-{}",
                repo_id(&repo)
            )))
            .debug_selector(move || row_selector)
            .size_full()
            .px(px(10.0))
            .gap(px(6.0))
            .items_center()
            .cursor_pointer()
            .hover(move |s| s.bg(hover))
            .on_click(cx.listener({
                let fold = fold.clone();
                move |this, _, _, cx| this.toggle_fold(section, repo_id(&fold), cx)
            }))
            .focusable()
            .focus_visible(move |s| s.bg(hover))
            .key_context(FOLD_CONTEXT)
            .on_action(cx.listener(move |this, _: &ToggleFold, _, cx| {
                this.toggle_fold(section, repo_id(&fold), cx);
            }))
            .child(
                Icon::new(if folded {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                })
                .size(px(12.0))
                .text_color(c.text_muted),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .mono(t)
                    .tiny(t)
                    .text_color(c.text_secondary)
                    .child(repo.path()),
            )
            .child(
                div()
                    .flex_none()
                    .mono(t)
                    .tiny(t)
                    .text_color(c.text_muted)
                    .child(count.to_string()),
            )
            .child(
                div()
                    .id(SharedString::from(format!("repo-more-{}", repo_id(&repo))))
                    .debug_selector(move || menu_selector)
                    .flex_none()
                    // The header under it must not also take the click.
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .child(
                        FlatButton::new(SharedString::from(format!(
                            "repo-menu-{}",
                            repo_id(&repo)
                        )))
                        .icon(Icon::new(IconName::Ellipsis))
                        .ghost()
                        .xsmall()
                        .cursor_pointer()
                        .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
                            repo_menu(menu, &more_repo)
                        }),
                    ),
            )
            .context_menu(move |menu, _, _| repo_menu(menu, &menu_repo))
            .into_any_element()
    }

    /// A stack's header in a repository group: fold chevron, the branch the
    /// stack merges into, and how many of it the group holds.
    fn stack_row(
        &self,
        section: usize,
        bottom: usize,
        count: usize,
        folded: bool,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = &t.colors;
        let inbox = self.session.read(cx).inbox.clone();
        let Some(pr) = inbox.prs.get(bottom) else {
            return Empty.into_any_element();
        };
        let id = stack_id(pr);
        let fold = id.clone();
        let hover = c.hover;
        let selector = format!("stack-group-{section}-{}", pr.key.number);
        h_flex()
            .id(SharedString::from(format!("stack-group-{section}-{id}")))
            .debug_selector(move || selector)
            .size_full()
            .pl(px(10.0 + GROUP_INDENT))
            .pr(px(10.0))
            .gap(px(6.0))
            .items_center()
            .cursor_pointer()
            .hover(move |s| s.bg(hover))
            .on_click(cx.listener({
                let fold = fold.clone();
                move |this, _, _, cx| this.toggle_fold(section, fold.clone(), cx)
            }))
            .focusable()
            .focus_visible(move |s| s.bg(hover))
            .key_context(FOLD_CONTEXT)
            .on_action(cx.listener(move |this, _: &ToggleFold, _, cx| {
                this.toggle_fold(section, fold.clone(), cx);
            }))
            .child(
                Icon::new(if folded {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                })
                .size(px(12.0))
                .text_color(c.text_muted),
            )
            .child(
                div()
                    .flex_none()
                    .tiny(t)
                    .text_color(c.text_secondary)
                    .child("Stack on"),
            )
            .child(
                Icon::new(IconName::GitBranch)
                    .size(px(12.0))
                    .text_color(c.text_faint),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .mono(t)
                    .tiny(t)
                    .text_color(c.text_secondary)
                    .child(pr.base.clone()),
            )
            .child(
                div()
                    .flex_none()
                    .mono(t)
                    .tiny(t)
                    .text_color(c.text_muted)
                    .child(count.to_string()),
            )
            .into_any_element()
    }

    fn pr_row(
        &self,
        section: usize,
        ix: usize,
        depth: u8,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = &t.colors;
        let grouped = depth > 0;
        let session = self.session.read(cx);
        let inbox = session.inbox.clone();
        let open = session.open_key().cloned();
        let Some(pr) = inbox.prs.get(ix) else {
            return Empty.into_any_element();
        };
        let review = pr.row_review(session.account(&pr.key.repo.host));
        let key = pr.key.clone();
        let is_open = open.as_ref() == Some(&key);
        let state = match pr.state {
            PrState::Open => None,
            PrState::Draft => Some(("draft", c.text_muted)),
            PrState::Merged => Some(("merged", c.accent_fg)),
            PrState::Closed => Some(("closed", c.danger)),
        };
        let badge = Subscriptions::global(cx).is_some_and(|s| s.read(cx).has_badge(&key));
        let marks = marks::Marks::try_global(cx)
            .map(|marks| marks.read(cx).row(pr))
            .unwrap_or_default();
        let dim = marks.settled() && Self::hidden(cx).dim_reviewed;
        let at_cursor = self
            .cursor
            .as_ref()
            .is_some_and(|cursor| cursor.section == section && cursor.key == key);
        let hover = c.hover;
        let bg = if is_open {
            Some(c.selected)
        } else if at_cursor {
            Some(hover)
        } else {
            None
        };
        let author = ui::avatar(
            cx,
            &pr.key.repo.host,
            ui::Who::new(&pr.author, pr.author_avatar.as_deref()),
            ui::avatar::ROW,
        );
        let number = pr.number();
        let (lines, talk): (Vec<RowCount>, Vec<RowCount>) = row_counts(pr)
            .into_iter()
            .partition(|count| matches!(count, RowCount::Lines { .. }));
        let reference_line = h_flex()
            .gap(px(6.0))
            .h(px(META_LINE))
            .items_center()
            .small(t)
            .text_color(c.text_muted)
            .child(
                h_flex()
                    .mono(t)
                    .tiny(t)
                    .min_w_0()
                    .overflow_hidden()
                    .when(!grouped, |el| {
                        el.child(
                            ui::web_link(
                                SharedString::from(format!("inbox-repo-{section}-{ix}")),
                                pr.key.repo.path(),
                                references::repo_url(&pr.key.repo),
                                cx,
                            )
                            .truncate(),
                        )
                    })
                    .child(format!("{}{}", pr.forge.sigil(), pr.key.number)),
            )
            .child(
                h_flex()
                    .gap(px(4.0))
                    .items_center()
                    .min_w_0()
                    .overflow_hidden()
                    .child(div().flex_none().child(author))
                    // Plain text: a click on the name opens the pull request
                    // like the rest of the row.
                    .child(
                        div()
                            .debug_selector(move || format!("inbox-author-{number}"))
                            .min_w_0()
                            .truncate()
                            .child(pr.author.clone()),
                    )
                    .child(div().flex_none().text_color(c.text_faint).child("\u{b7}"))
                    .child(
                        div()
                            .debug_selector(move || format!("inbox-age-{number}"))
                            .flex_none()
                            .tiny(t)
                            .child(relative_time(pr.minutes_ago())),
                    ),
            )
            .children(
                counts(t, format!("inbox-talk-{number}"), talk)
                    .map(|talk| div().flex_none().ml_auto().child(talk)),
            );
        let status_line = h_flex()
            .gap(px(6.0))
            .h(px(META_LINE))
            .items_center()
            .small(t)
            .text_color(c.text_muted)
            .children(state.map(|(label, color)| ui::chip(cx, label, color)))
            .child(
                div()
                    .debug_selector(move || format!("inbox-review-{number}"))
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(row_review_color(t, review))
                    .child(review.label()),
            )
            .children(
                marks
                    .reviewed
                    .map(|reviewed| reviewed_icon(t, number, reviewed)),
            )
            .children(
                marks
                    .progress
                    .map(|progress| progress_bar(t, number, progress)),
            )
            .children(counts(t, format!("inbox-lines-{number}"), lines));
        let row_id = SharedString::from(format!("pr-{section}-{}", pr.ref_label()));
        let (open_key, hover_key, checks_key, menu_key) =
            (key.clone(), key.clone(), key.clone(), key);
        let ci_selector = format!("inbox-ci-{number}");
        let more_selector = format!("inbox-more-{number}");
        let more_visible = is_open || at_cursor;
        let more_key = menu_key.clone();
        let row_selector = format!("inbox-row-{number}");
        let title_selector = format!("inbox-title-{number}");
        h_flex()
            .id(row_id)
            .size_full()
            .pl(px(10.0 + f32::from(depth) * GROUP_INDENT))
            .pr(px(10.0))
            .gap(px(8.0))
            .items_start()
            .cursor_pointer()
            .border_l_2()
            .border_color(if is_open {
                c.accent
            } else {
                gpui::transparent_black()
            })
            .when_some(bg, |el, bg| el.bg(bg))
            .when(bg.is_none(), |el| el.hover(move |s| s.bg(hover)))
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                if *hovered {
                    this.intend(hover_key.clone(), cx);
                } else {
                    this.forget_intent(&hover_key);
                }
            }))
            .debug_selector(move || row_selector)
            .group(ROW_GROUP)
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                let cursor = Cursor {
                    section,
                    key: open_key.clone(),
                };
                this.click_row(cursor, event.modifiers().control, cx);
                window.focus(&this.focus, cx);
            }))
            .child(
                div()
                    .id(("inbox-ci", ix))
                    .debug_selector(move || ci_selector)
                    .pt(px(8.0))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        // The row under it must not also take the click.
                        cx.stop_propagation();
                        this.cursor = Some(Cursor {
                            section,
                            key: checks_key.clone(),
                        });
                        this.show_pipelines(&checks_key, cx);
                        window.focus(&this.focus, cx);
                    }))
                    .child(self.ci(t, pr.ci, CI_ROW)),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .py(px(ROW_PAD))
                    .gap(px(LINE_GAP))
                    .when(dim, |el| el.opacity(DIM_OPACITY))
                    .child(
                        h_flex()
                            .h(px(TITLE_LINE))
                            .gap(px(8.0))
                            .items_center()
                            .when(marks.unread, |el| {
                                el.child(
                                    div()
                                        .debug_selector(move || format!("inbox-unread-{number}"))
                                        .size(px(7.0))
                                        .flex_none()
                                        .rounded_full()
                                        .bg(c.accent),
                                )
                            })
                            .child(
                                div()
                                    .debug_selector(move || title_selector)
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(c.text)
                                    .when(marks.unread, |el| el.font_weight(FontWeight::SEMIBOLD))
                                    .child(pr.title.clone()),
                            )
                            .when(badge, |el| {
                                el.child(
                                    div().size(px(6.0)).flex_none().rounded_full().bg(c.accent),
                                )
                            })
                            .child(
                                div()
                                    .id(("inbox-more", ix))
                                    .debug_selector(move || more_selector)
                                    .flex_none()
                                    .when(!more_visible, |el| {
                                        el.opacity(0.0)
                                            .group_hover(ROW_GROUP, |style| style.opacity(1.0))
                                    })
                                    // The row under it must not also take the click.
                                    .on_click(|_, _, cx| cx.stop_propagation())
                                    .child(
                                        FlatButton::new(("inbox-more-button", ix))
                                            .icon(Icon::new(IconName::Ellipsis))
                                            .ghost()
                                            .xsmall()
                                            .cursor_pointer()
                                            .dropdown_menu_with_anchor(
                                                Anchor::TopRight,
                                                move |menu, window, cx| {
                                                    row_menu::popup(menu, &more_key, window, cx)
                                                },
                                            ),
                                    ),
                            ),
                    )
                    .child(reference_line)
                    .child(status_line),
            )
            .context_menu(move |menu, window, cx| row_menu::popup(menu, &menu_key, window, cx))
            .into_any_element()
    }

    /// What a section's header says after its name: how many it holds, and
    /// what display-only hiding leaves out.
    #[must_use]
    pub fn section_note(&self, ix: usize, cx: &App) -> String {
        let inbox = &self.session.read(cx).inbox;
        let Some(spec) = inbox.sections.get(ix) else {
            return String::new();
        };
        let hidden = Self::hidden(cx);
        let total = spec.items.len();
        let hide_drafts = self.section_hides_drafts(ix);
        let prs = || spec.items.iter().filter_map(|&pr| inbox.prs.get(pr));
        let in_hidden_repo = |pr: &PullRequest| hidden.is_hidden(&repo_id(&pr.key.repo));
        match &spec.state {
            SectionState::Loading => "loading".to_owned(),
            SectionState::Failed(_) => format!("{total} offline"),
            SectionState::Cached | SectionState::Fresh if !self.inbox.filter.is_empty() => {
                let matching = prs()
                    .filter(|pr| {
                        let repo = repo_id(&pr.key.repo);
                        !hidden.is_hidden(&repo)
                            && matches_filter(&repo, &self.inbox.filter)
                            && !(hide_drafts && pr.state == PrState::Draft)
                    })
                    .count();
                format!("{matching} of {total}")
            }
            SectionState::Cached | SectionState::Fresh => {
                let mut note = total.to_string();
                let hidden_here = prs().filter(|pr| in_hidden_repo(pr)).count();
                if hidden_here > 0 {
                    note.push_str(&format!(", {hidden_here} hidden"));
                }
                let drafts = prs()
                    .filter(|pr| hide_drafts && pr.state == PrState::Draft && !in_hidden_repo(pr))
                    .count();
                match drafts {
                    0 => {}
                    1 => note.push_str(", 1 draft hidden"),
                    n => note.push_str(&format!(", {n} drafts hidden")),
                }
                note
            }
        }
    }

    /// A section's header: fold chevron, title and count. A click folds it.
    fn section_header(&self, ix: usize, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let c = &t.colors;
        let inbox = self.session.read(cx).inbox.clone();
        let Some(spec) = inbox.sections.get(ix) else {
            return Empty.into_any_element();
        };
        let collapsed = self.section_collapsed(ix).unwrap_or(false);
        let hover = c.hover;
        let note = self.section_note(ix, cx);
        let note_color = match &spec.state {
            SectionState::Loading => c.text_faint,
            SectionState::Failed(_) => c.warn,
            SectionState::Cached | SectionState::Fresh => c.text_muted,
        };
        let hide_drafts = self.section_hides_drafts(ix);
        let items = self.section_menu(ix, cx);
        let repo = spec.repo.clone();
        let selector = format!("inbox-section-{ix}");
        h_flex()
            .id(("section", ix))
            .debug_selector(move || selector)
            .h(px(ROW))
            .flex_none()
            .px(px(10.0))
            .gap(px(6.0))
            .items_center()
            .cursor_pointer()
            .hover(move |s| s.bg(hover))
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_section(ix, cx)))
            .focusable()
            .focus_visible(move |s| s.bg(hover))
            .key_context(FOLD_CONTEXT)
            .on_action(cx.listener(move |this, _: &ToggleFold, _, cx| {
                this.toggle_section(ix, cx);
            }))
            .context_menu(move |mut menu, _, _| {
                for item in &items {
                    if !matches!(item, SectionMenuItem::Fold | SectionMenuItem::Unfold) {
                        menu = menu.separator();
                    }
                    let entry = PopupMenuItem::new(item.label());
                    let repo = repo.clone();
                    menu = menu.item(match item {
                        SectionMenuItem::Fold | SectionMenuItem::Unfold => {
                            entry.on_click(move |_, _, cx| {
                                Sidebar::global(cx)
                                    .update(cx, |this, cx| this.toggle_section(ix, cx));
                            })
                        }
                        SectionMenuItem::HideDrafts => {
                            entry.checked(hide_drafts).on_click(move |_, _, cx| {
                                Sidebar::global(cx).update(cx, |this, cx| {
                                    this.set_section_hides_drafts(ix, !hide_drafts, cx);
                                });
                            })
                        }
                        SectionMenuItem::RemoveFromInbox => entry.on_click(move |_, _, cx| {
                            if let Some(repo) = &repo {
                                remove_repo_from_inbox(repo, cx);
                            }
                        }),
                    });
                }
                menu
            })
            .child(
                Icon::new(if collapsed {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                })
                .size(px(14.0))
                .text_color(c.text_muted),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(c.text_secondary)
                    .child(spec.title.clone()),
            )
            .child(div().mono(t).tiny(t).text_color(note_color).child(note))
            .into_any_element()
    }

    /// The inbox panel's content: the grouping toggle, the sections and the
    /// Repos list.
    pub(crate) fn inbox_body(&mut self, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        BODY_RENDERS.fetch_add(1, Ordering::Relaxed);
        let t = ui::theme(cx);
        let session = self.session.read(cx);
        let (inbox, stacks) = (session.inbox.clone(), session.inbox_stacks());
        let hidden = Self::hidden(cx);
        let unread = unread_rows(&inbox, &hidden, cx);
        let slots = self.spinners.clone();
        for (ix, list) in self.inbox.lists.clone().into_iter().enumerate() {
            let rows = section_rows(
                &inbox,
                ix,
                self.inbox.group_by_repo,
                Hiding {
                    repos: &hidden,
                    filter: &self.inbox.filter,
                    drafts: self.section_hides_drafts(ix),
                },
                &self.inbox.folded,
                unread.as_ref(),
                &stacks,
            );
            let slots = slots.clone();
            list.update(cx, |list, _| {
                list.tops = RowTops::new(rows.iter().map(|row| row.height()));
                list.rows = rows.into();
                list.slots = slots;
            });
        }
        let count = self.inbox.sections.len().min(inbox.sections.len());
        let wanted: Vec<Want> = (0..count)
            .map(|ix| {
                let layout = &self.inbox.sections[ix];
                let rows = self.inbox.lists[ix].read(cx).tops.total();
                Want {
                    collapsed: layout.collapsed,
                    share: layout.share,
                    content: ROW + rows,
                }
            })
            .collect();
        let avail = f32::from(self.inbox.height.get());
        let heights = (avail > 0.0).then(|| allot(avail, &wanted));
        if let Some(heights) = &heights {
            self.inbox.heights.clone_from(heights);
        }
        let last = last_open(&wanted);
        let sections: Vec<AnyElement> = (0..count)
            .map(|ix| {
                let height = heights.as_ref().map(|h| h[ix]);
                self.section_block(ix, height, Some(ix) == last, &t, cx)
            })
            .collect();
        let measured = self.inbox.height.clone();
        let sidebar = cx.weak_entity();
        let measure = gpui::canvas(
            move |bounds, _, cx| {
                // A new height (the window or the dock resized) is allotted
                // again in the next frame.
                if measured.replace(bounds.size.height) != bounds.size.height {
                    sidebar.update(cx, |_, cx| cx.notify()).ok();
                }
            },
            |_, (), _, _| {},
        )
        .absolute()
        .size_full();
        v_flex()
            .id("inbox-sections")
            .relative()
            .size_full()
            .overflow_hidden()
            .child(measure)
            .children(sections)
            .into_any_element()
    }

    /// The "Group by repo" switch, in the inbox's tab.
    pub(crate) fn group_toggle(&self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let c = &t.colors;
        let grouped = self.inbox.group_by_repo;
        let hover = c.hover;
        div()
            .id("group-by-repo")
            .debug_selector(|| "group-by-repo".into())
            .tooltip(|window, cx| {
                gpui_kit::component::tooltip::Tooltip::new("Group by repo").build(window, cx)
            })
            .size(px(18.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(t.radius.sm)
            .cursor_pointer()
            .when(grouped, |el| el.bg(c.selected))
            .hover(move |s| s.bg(hover))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.set_grouped_by_repo(!grouped, cx);
            }))
            .child(
                Icon::new(IconName::ListTree)
                    .size(px(13.0))
                    .text_color(if grouped { c.accent_fg } else { c.text_muted }),
            )
            .into_any_element()
    }

    /// Section `ix`: its header, and while open its list, sized as dragged
    /// (or by its rows), with a drag handle under it unless it is the `last`
    /// open one, which fills the rest.
    fn section_block(
        &self,
        ix: usize,
        height: Option<f32>,
        last: bool,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let header = self.section_header(ix, t, cx);
        let Some(layout) = self.inbox.sections.get(ix) else {
            return Empty.into_any_element();
        };
        let block = v_flex()
            .id(("inbox-section-block", ix))
            .debug_selector(move || format!("inbox-section-block-{ix}"))
            .relative()
            .w_full()
            .min_w_0()
            .child(header);
        if layout.collapsed {
            return block.flex_none().h(px(ROW)).into_any_element();
        }
        let Some(list) = self.inbox.lists.get(ix).cloned() else {
            return block.into_any_element();
        };
        let tops = list.read(cx).tops.clone();
        let content = ROW + tops.total();
        let body = div().flex_1().min_h_0().child(list);
        let block = match height {
            Some(height) => block.flex_none().h(px(height)),
            // Before the first measure: by its rows.
            None => block
                .flex_basis(px(content))
                .flex_grow(if last { 1.0 } else { 0.0 })
                .flex_shrink(1.0)
                .min_h(px(SECTION_MIN.min(content))),
        };
        let block = block.child(body);
        if last {
            return block.into_any_element();
        }
        block
            .on_drag_move(
                cx.listener(move |this, event: &DragMoveEvent<ResizeSection>, _, cx| {
                    if event.drag(cx).0 == ix {
                        let size = event.event.position.y - event.bounds.top();
                        this.resize_section(ix, f32::from(size), cx);
                    }
                }),
            )
            .child(
                resize_handle::<ResizeSection, ResizeSection>(("inbox-handle", ix), Axis::Vertical)
                    .inside(HandleEdge::Trailing)
                    .on_drag(ResizeSection(ix), |payload, _, _, cx| cx.new(|_| *payload)),
            )
            .into_any_element()
    }

    /// The Repos panel's search box, made on first use. Each change filters
    /// the panel's list at once.
    pub fn repos_search(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputState> {
        if let Some(search) = &self.inbox.repos_search {
            return search.clone();
        }
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search repositories"));
        cx.observe(&search, |this: &mut Self, search, cx| {
            let filter = search.read(cx).value().trim().to_lowercase();
            if filter != this.inbox.repos_filter {
                this.inbox.repos_filter = filter;
                cx.notify();
            }
        })
        .detach();
        self.inbox.repos_search = Some(search.clone());
        search
    }

    /// The Repos panel's search row, over its list. `Esc` in it empties it.
    pub(crate) fn repos_search_row(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let search = self.repos_search(window, cx);
        let sidebar = cx.entity();
        h_flex()
            .id("repos-search")
            .debug_selector(|| "repos-search".into())
            .key_context(FILTER_CONTEXT)
            .on_action(move |_: &ClearFilter, window, cx| {
                sidebar.update(cx, |sidebar, cx| sidebar.clear_repos_search(window, cx));
            })
            .flex_none()
            .px(px(8.0))
            .py(px(3.0))
            .child(ui::search_input(&search, cx))
            .into_any_element()
    }

    /// Empties the Repos panel's search box (`Esc` in it).
    pub fn clear_repos_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(search) = self.inbox.repos_search.clone() {
            search.update(cx, |state, cx| state.set_value("", window, cx));
        }
        self.inbox.repos_filter.clear();
        cx.notify();
    }

    /// What the Repos panel's search box holds, lowercased.
    #[must_use]
    pub fn repos_search_text(&self) -> &str {
        &self.inbox.repos_filter
    }

    /// The repositories the Repos panel lists: every one seen whose name
    /// passes its search.
    #[must_use]
    pub fn listed_repos(&self, cx: &App) -> Vec<String> {
        let filter = &self.inbox.repos_filter;
        self.repos(cx)
            .into_iter()
            .filter(|repo| matches_filter(repo, filter))
            .collect()
    }

    /// The Repos panel's tab note: how many of the repositories seen the
    /// inbox shows.
    #[must_use]
    pub fn repos_note(&self, cx: &App) -> String {
        let (shown, total) = self.repos_shown(cx);
        format!("{shown} of {total} shown")
    }

    /// The Repos panel's content: show all, hide all, and a checkbox per
    /// repository that passes its search.
    pub(crate) fn repos_body(&mut self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let c = &t.colors;
        let hidden = Self::hidden(cx);
        let listed = self.listed_repos(cx);
        let empty = listed.is_empty();
        let rows = listed.into_iter().enumerate().map(|(ix, repo)| {
            let is_hidden = hidden.is_hidden(&repo);
            let toggled = repo.clone();
            let selector = format!("repo-row-{repo}");
            let label = format!("repo-label-{repo}");
            let hover = c.hover;
            // The whole row toggles, its name included; the box only shows.
            h_flex()
                .id(("repo-row", ix))
                .debug_selector(move || selector)
                .h(px(24.0))
                .px(px(10.0))
                .gap(px(6.0))
                .items_center()
                .cursor_pointer()
                .hover(move |s| s.bg(hover))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.set_repo_hidden(&toggled, !is_hidden, cx);
                }))
                .child(ui::checkbox(("repo-shown", ix)).checked(!is_hidden))
                .child(
                    div()
                        .debug_selector(move || label)
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .mono(t)
                        .tiny(t)
                        .text_color(if is_hidden { c.text_muted } else { c.text })
                        .child(repo),
                )
        });
        v_flex()
            .w_full()
            .pb(px(6.0))
            .child(
                h_flex()
                    .px(px(10.0))
                    .py(px(4.0))
                    .gap(px(6.0))
                    .items_center()
                    .child(div().flex_1())
                    .child(
                        Button::new("repos-show-all", "Show all", ButtonKind::Ghost)
                            .on_click(cx.listener(|this, _, _, cx| this.set_all_hidden(false, cx))),
                    )
                    .child(
                        Button::new("repos-hide-all", "Hide all", ButtonKind::Ghost)
                            .on_click(cx.listener(|this, _, _, cx| this.set_all_hidden(true, cx))),
                    ),
            )
            .when(empty, |el| {
                el.child(
                    div()
                        .debug_selector(|| "repos-no-match".into())
                        .px(px(10.0))
                        .py(px(4.0))
                        .small(t)
                        .text_color(c.text_muted)
                        .child("No repository matches"),
                )
            })
            .children(rows)
            .into_any_element()
    }
}

/// Some of an inbox row's counts, muted and fixed in width so the title
/// keeps the room; `None` when there are none.
fn counts(t: &Theme, selector: String, counts: Vec<RowCount>) -> Option<AnyElement> {
    if counts.is_empty() {
        return None;
    }
    let c = &t.colors;
    let items = counts.into_iter().map(|count| match count {
        RowCount::Comments(n) => h_flex()
            .gap(px(2.0))
            .items_center()
            .child(
                Icon::new(IconName::MessageSquare)
                    .size(px(11.0))
                    .text_color(c.text_muted),
            )
            .child(n.to_string())
            .into_any_element(),
        RowCount::Unresolved(n) => h_flex()
            .gap(px(2.0))
            .items_center()
            .text_color(c.warn)
            .child(
                Icon::new(IconName::MessageSquareDot)
                    .size(px(11.0))
                    .text_color(c.warn),
            )
            .child(n.to_string())
            .into_any_element(),
        RowCount::Lines { added, removed } => h_flex()
            .gap(px(3.0))
            .child(div().text_color(c.ok).child(format!("+{added}")))
            .child(
                div()
                    .text_color(c.danger)
                    .child(format!("\u{2212}{removed}")),
            )
            .into_any_element(),
    });
    let counts = h_flex()
        .debug_selector(move || selector)
        .flex_none()
        .gap(px(8.0))
        .items_center()
        .mono(t)
        .tiny(t)
        .text_color(c.text_muted)
        .children(items)
        .into_any_element();
    Some(counts)
}

/// How faded a reviewed row with nothing new is drawn.
const DIM_OPACITY: f32 = 0.55;

/// The width of a row's viewed-progress bar.
const PROGRESS_WIDTH: f32 = 24.0;

fn row_review_color(t: &Theme, review: RowReview) -> gpui::Hsla {
    let c = &t.colors;
    match review {
        RowReview::Overall(state) => review_color(t, state),
        RowReview::Mine(status) => match status {
            ReviewStatus::Approved => c.ok,
            ReviewStatus::ChangesRequested => c.danger,
            ReviewStatus::ReReviewRequested | ReviewStatus::StaleApproval => c.warn,
            ReviewStatus::ReviewRequested => c.accent_fg,
            ReviewStatus::Pending | ReviewStatus::Commented | ReviewStatus::Dismissed => {
                c.text_muted
            }
        },
    }
}

/// The reviewed mark: a check, faint once new commits came after it.
fn reviewed_icon(t: &Theme, number: u64, reviewed: ReviewedMark) -> AnyElement {
    let color = match reviewed {
        ReviewedMark::Current => t.colors.ok,
        ReviewedMark::Changed => t.colors.text_faint,
    };
    div()
        .debug_selector(move || format!("inbox-reviewed-{number}"))
        .flex_none()
        .child(
            Icon::new(IconName::CircleCheck)
                .size(px(11.0))
                .text_color(color),
        )
        .into_any_element()
}

/// `12/23 viewed` with a small bar: the files viewed when it was last open.
/// Counted before the last push, it is drawn faint.
fn progress_bar(t: &Theme, number: u64, progress: Progress) -> AnyElement {
    let c = &t.colors;
    let (fill, text) = if progress.current {
        (c.accent, c.text_muted)
    } else {
        (c.text_faint, c.text_faint)
    };
    h_flex()
        .debug_selector(move || format!("inbox-progress-{number}"))
        .flex_none()
        .gap(px(4.0))
        .items_center()
        .mono(t)
        .tiny(t)
        .text_color(text)
        .child(
            div()
                .w(px(PROGRESS_WIDTH))
                .h(px(3.0))
                .rounded_full()
                .bg(c.border)
                .child(
                    div()
                        .h_full()
                        .w(px(PROGRESS_WIDTH * progress.fraction()))
                        .rounded_full()
                        .bg(fill),
                ),
        )
        .child(format!("{}/{} viewed", progress.viewed, progress.total))
        .into_any_element()
}

/// `--demo-inbox`: groups by repository, unfolds the Repos panel and hides
/// the last repository.
pub fn demo_tree(cx: &mut App) {
    let sidebar = Sidebar::global(cx);
    sidebar.update(cx, |sidebar, cx| {
        sidebar.set_grouped_by_repo(true, cx);
        if let Some(last) = sidebar.repos(cx).pop() {
            sidebar.set_repo_hidden(&last, true, cx);
        }
    });
    crate::sidebar_panels::unfold(crate::sidebar_panels::Section::Repos, cx);
}

/// `--demo-sections`: the second section folded, the third unfolded and then
/// dragged down to two rows, which leaves empty space under the sections.
pub fn demo_sections(cx: &mut App) {
    let sidebar = Sidebar::global(cx);
    sidebar.update(cx, |sidebar, cx| {
        if sidebar.section_collapsed(1) == Some(false) {
            sidebar.toggle_section(1, cx);
        }
        if sidebar.section_collapsed(2) == Some(true) {
            sidebar.toggle_section(2, cx);
        }
    });
    // The drag needs the heights the first frame gave the sections.
    cx.spawn(async move |cx| {
        cx.background_executor()
            .timer(std::time::Duration::from_millis(400))
            .await;
        cx.update(|cx| {
            sidebar.update(cx, |sidebar, cx| {
                sidebar.resize_section(2, DRAG_MIN, cx);
            });
        });
    })
    .detach();
}

/// A repository group's menu, for a right-click on its header or its "..."
/// button: hide it, or open it on its forge.
pub(crate) fn repo_menu(menu: PopupMenu, repo: &RepoRef) -> PopupMenu {
    let id = repo_id(repo);
    let url = format!("https://{}/{}", repo.host, repo.path());
    menu.item(
        PopupMenuItem::new("Hide this repository").on_click(move |_, _, cx| {
            Sidebar::global(cx).update(cx, |this, cx| this.set_repo_hidden(&id, true, cx));
        }),
    )
    .item(
        PopupMenuItem::new("Open on the forge").on_click(move |_, _, cx| {
            if crate::untrusted::is_web_url(&url) {
                cx.open_url(&url);
            }
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::Fixtures;

    #[test]
    fn counts_show_only_what_the_forge_said() {
        let fixtures = Fixtures::load();
        let mut pr = fixtures.inbox.prs[0].clone();
        pr.comments = Some(4);
        pr.unresolved = Some(0);
        pr.stat = Some((12, 3));
        assert_eq!(
            row_counts(&pr),
            [
                RowCount::Comments(4),
                RowCount::Lines {
                    added: 12,
                    removed: 3
                }
            ]
        );
        pr.comments = None;
        pr.unresolved = Some(2);
        pr.stat = None;
        assert_eq!(row_counts(&pr), [RowCount::Unresolved(2)]);
    }

    #[test]
    fn sections_are_allotted_top_down() {
        let open = |share, content| Want {
            collapsed: false,
            share,
            content,
        };
        let folded = Want {
            collapsed: true,
            share: None,
            content: 500.0,
        };
        // Room for everything: rows' heights, and the last open one takes
        // the rest.
        assert_eq!(
            allot(600.0, &[open(None, 100.0), folded, open(None, 50.0)]),
            [100.0, ROW, 600.0 - 100.0 - ROW]
        );
        assert_eq!(
            allot(600.0, &[open(None, 50.0), folded]),
            [600.0 - ROW, ROW]
        );
        // Too little: the first keeps what it can and leaves the others
        // their least.
        let heights = allot(300.0, &[open(None, 400.0), open(None, 400.0), folded]);
        assert_eq!(heights, [300.0 - SECTION_MIN - ROW, SECTION_MIN, ROW]);
    }

    #[test]
    fn an_older_layouts_weights_and_pixel_heights_become_shares() {
        let parse = |json: &str| -> SavedSection { serde_json::from_str(json).expect("parses") };
        let mut old = vec![
            parse(r#"{"title":"A","size":150.0}"#),
            parse(r#"{"title":"B","collapsed":true,"weight":900.0}"#),
            parse(r#"{"title":"C","weight":450.0}"#),
            parse(r#"{"title":"D","share":0.25}"#),
        ];
        legacy_shares(&mut old);
        // The open ones filled the panel, so they add up to one.
        assert_eq!(old[0].share, Some(0.25));
        assert_eq!(old[2].share, Some(0.75));
        assert_eq!(old[1].share, Some(1.0), "a folded one keeps a share");
        assert_eq!(old[3].share, Some(0.25), "a share stays");
        let saved = serde_json::to_value(&old[0]).expect("serializes");
        assert_eq!(saved["share"], 0.25);
        assert!(saved.get("weight").is_none() && saved.get("size").is_none());
    }

    fn close(a: &[f32], b: &[f32]) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(a, b)| (a - b).abs() < 0.01)
    }

    #[test]
    fn dragged_sections_keep_their_shares() {
        let open = |share: f32| Want {
            collapsed: false,
            share: Some(share),
            content: 1000.0,
        };
        let folded = |share: f32| Want {
            collapsed: true,
            share: Some(share),
            content: 1000.0,
        };
        // Shares of the space; the last open one takes what the others
        // leave, whatever its own share.
        assert_eq!(allot(600.0, &[open(0.25), open(0.5)]), [150.0, 450.0]);
        assert_eq!(allot(900.0, &[open(0.25), open(0.5)]), [225.0, 675.0]);
        // The others asking for more than the space leave the last its least.
        let over = allot(600.0, &[open(1.0), open(2.0)]);
        assert!(
            close(&over, &[600.0 - SECTION_MIN, SECTION_MIN]),
            "{over:?}"
        );
        // Shrunk below a section's least, it keeps the least; grown again,
        // the shares come back.
        let small = allot(200.0, &[open(0.05), open(0.95)]);
        assert!(
            close(&small, &[SECTION_MIN, 200.0 - SECTION_MIN]),
            "{small:?}"
        );
        assert_eq!(allot(1000.0, &[open(0.1), open(0.5)]), [100.0, 900.0]);
        // A folded section is its header and gives its share to the rest;
        // unfolded, it takes the share back.
        assert_eq!(
            allot(600.0 + ROW, &[open(0.25), folded(0.5), open(0.5)]),
            [150.0, ROW, 450.0]
        );
        assert_eq!(
            allot(600.0 + ROW, &[open(0.25), open(0.5), folded(0.25)]),
            [150.0, 450.0, ROW]
        );
        assert_eq!(
            allot(600.0, &[open(0.25), open(0.5), open(0.25)]),
            [150.0, 300.0, 150.0]
        );
    }

    #[test]
    fn grouping_and_hiding_shape_a_section() {
        let fixtures = Fixtures::load();
        let inbox = &fixtures.inbox;
        let section = inbox
            .sections
            .iter()
            .position(|s| {
                let repos: BTreeSet<String> = s
                    .items
                    .iter()
                    .map(|&ix| repo_id(&inbox.prs[ix].key.repo))
                    .collect();
                repos.len() >= 2
            })
            .expect("a fixture section with two repositories");
        let none = InboxSettings::default();
        let hiding = |repos| Hiding {
            repos,
            filter: "",
            drafts: false,
        };
        let folded = HashSet::new();
        let flat = section_rows(inbox, section, false, hiding(&none), &folded, None, &[]);
        let grouped = section_rows(inbox, section, true, hiding(&none), &folded, None, &[]);
        let groups = grouped
            .iter()
            .filter(|row| matches!(row, InboxRow::Group { .. }))
            .count();
        assert!(groups >= 2);
        assert_eq!(grouped.len(), flat.len() + groups, "a header per group");

        let InboxRow::Group { first, count, .. } = grouped[0] else {
            panic!("a group comes first");
        };
        let repo = repo_id(&inbox.prs[first].key.repo);
        let mut hidden = InboxSettings::default();
        hidden.set_hidden(&repo, true);
        let without = section_rows(inbox, section, false, hiding(&hidden), &folded, None, &[]);
        assert_eq!(without.len(), flat.len() - count, "its rows are hidden");

        let title = inbox.sections[section].title.clone();
        let folded = HashSet::from([(title, repo)]);
        let folded_rows = section_rows(inbox, section, true, hiding(&none), &folded, None, &[]);
        assert!(matches!(
            folded_rows[0],
            InboxRow::Group { folded: true, .. }
        ));
        assert_eq!(folded_rows.len(), grouped.len() - count);
    }

    #[test]
    fn a_stack_nests_under_its_repository_top_first() {
        let fixtures = Fixtures::load();
        let inbox = &fixtures.inbox;
        let stack: Vec<usize> = [38, 39, 40, 41]
            .into_iter()
            .map(|n| {
                inbox
                    .prs
                    .iter()
                    .position(|pr| pr.key.number.0 == n)
                    .expect("the fixture stack")
            })
            .collect();
        let section = inbox
            .sections
            .iter()
            .position(|s| stack.iter().all(|ix| s.items.contains(ix)))
            .expect("a section with the whole stack");
        let none = InboxSettings::default();
        let hiding = Hiding {
            repos: &none,
            filter: "",
            drafts: false,
        };
        let stacks = [stack.clone()];
        let rows = |folded: &HashSet<(SharedString, String)>| {
            section_rows(inbox, section, true, hiding, folded, None, &stacks)
        };
        let open = rows(&HashSet::new());
        let at = open
            .iter()
            .position(|row| matches!(row, InboxRow::Stack { .. }))
            .expect("a stack header");
        assert!(matches!(
            open[at],
            InboxRow::Stack { bottom, count: 4, folded: false, .. } if bottom == stack[0]
        ));
        let under: Vec<(usize, u8)> = open[at + 1..at + 5]
            .iter()
            .map(|row| match *row {
                InboxRow::Pr { pr, depth, .. } => (pr, depth),
                _ => panic!("a pull request row"),
            })
            .collect();
        let top_first: Vec<(usize, u8)> = stack.iter().rev().map(|&ix| (ix, 2)).collect();
        assert_eq!(under, top_first);
        assert_eq!(
            open.len(),
            section_rows(inbox, section, true, hiding, &HashSet::new(), None, &[]).len() + 1,
            "one header more, every row kept"
        );

        let title = inbox.sections[section].title.clone();
        let folded = HashSet::from([(title, stack_id(&inbox.prs[stack[0]]))]);
        let folded_rows = rows(&folded);
        assert!(matches!(
            folded_rows[at],
            InboxRow::Stack { folded: true, .. }
        ));
        assert_eq!(folded_rows.len(), open.len() - 4, "its rows fold away");

        let mut drafts = hiding;
        drafts.drafts = true;
        let without_draft =
            section_rows(inbox, section, true, drafts, &HashSet::new(), None, &stacks);
        assert!(
            matches!(without_draft[at], InboxRow::Stack { count: 3, .. }),
            "a hidden member leaves the stack, the rest stay under it"
        );
    }

    #[test]
    fn drafts_leave_a_section_only_when_asked() {
        let fixtures = Fixtures::load();
        let mut inbox = fixtures.inbox.clone();
        let draft = inbox
            .prs
            .iter()
            .position(|pr| pr.state == PrState::Draft)
            .expect("a fixture draft");
        inbox.sections[0].items.insert(0, draft);
        let (none, folded) = (InboxSettings::default(), HashSet::new());
        let rows = |drafts| {
            let hiding = Hiding {
                repos: &none,
                filter: "",
                drafts,
            };
            section_rows(&inbox, 0, false, hiding, &folded, None, &[])
        };
        let listed = |rows: &[InboxRow]| {
            rows.iter()
                .any(|row| matches!(row, InboxRow::Pr { pr, .. } if *pr == draft))
        };
        assert!(listed(&rows(false)));
        assert!(!listed(&rows(true)));
        assert_eq!(rows(true).len() + 1, rows(false).len());
    }
}
