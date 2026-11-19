//! What the sidebar's dock panels draw: the inbox (see [`crate::inbox`]), and
//! the focused pull request's title, stack, checks, threads and files.
//!
//! One [`Sidebar`] entity holds the state they share (the inbox cursor, the
//! thread cursor, the inbox layout) and builds each section's content; the
//! dock panels in [`crate::sidebar_panels`] place it. Running-CI icons are
//! painted by the shell's spinner layer (see [`crate::ui::spinner`]), into
//! the slots of the panel being drawn.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use forge::model::{JobId, Thread, ThreadId, Timestamp};

use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    FontWeight, Global, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement as _, Styled, Task, WeakEntity, actions, div,
    prelude::FluentBuilder as _, px, relative,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::{
    Icon,
    dock::{DockArea, DockPlacement, PaneRef, PanelId},
    h_flex, v_flex,
};
use theme::{ActiveTheme, Theme};

use crate::checks_panel::job_menu::JobMenu;
use crate::diff_panel::{NextInStack, PrevInStack};
use crate::inbox::InboxState;
use crate::inbox::marks::Marks;
use crate::model::{ChangedFile, CiState, Coverage, PrKey, Viewed};
use crate::opener::OpenStatus;
use crate::panels::PanelKind;
use crate::session::{OpenMode, OpenPr, Session, StackStep};
use crate::sidebar_panels::FoldedStack;
use crate::subscriptions::Subscriptions;
use crate::threads;
use crate::threads::thread_menu::{self, Origin, ThreadMenu};
use crate::ui::spinner::SpinnerSlots;
use crate::ui::{self, CI_ROW, TextExt as _, ci_color, status_color, status_letter};

actions!(
    sidebar,
    [
        SelectNext,
        SelectPrev,
        OpenSelected,
        /// `Ctrl+Enter`: the selected pull request's diff, in a tab group
        /// beside the others.
        OpenBeside,
        ToggleViewed
    ]
);

pub const KEY_CONTEXT: &str = "Sidebar";
pub(crate) const ROW: f32 = 28.0;
/// How long a row must stay selected or hovered before it is prefetched.
const INTENT_DELAY: Duration = Duration::from_millis(150);
/// The file tree's indent per level, its left padding, and where a
/// directory's guide line sits: under the center of the directory row's
/// leading checkbox (14px wide), left of every child's own content.
const TREE_INDENT: f32 = 12.0;
const TREE_PAD: f32 = 10.0;
const GUIDE_X: f32 = TREE_PAD + 7.0;
/// The share of a thread row its path may take before the message.
const PATH_SHARE: f32 = 0.4;

/// The selected inbox row: a pull request in one section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Cursor {
    pub section: usize,
    pub key: PrKey,
}

/// Something the layout saves changed: a section folded or resized, the
/// grouping switched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SidebarEvent {
    LayoutChanged,
}

pub struct Sidebar {
    pub(crate) session: Entity<Session>,
    pub(crate) focus: FocusHandle,
    pub(crate) cursor: Option<Cursor>,
    pub(crate) inbox: InboxState,
    /// The slots of the panel being drawn now: each panel puts its own in
    /// before its section is built.
    pub(crate) spinners: SpinnerSlots,
    /// A pending prefetch: the row and the timer that fires it.
    intent: Option<(PrKey, Task<()>)>,
    /// The selected row of the open pull request's thread list.
    thread_cursor: Option<ThreadId>,
    /// Whether j, k and Enter work on the thread list rather than the inbox:
    /// set by clicking a thread row, cleared by clicking a pull request.
    on_threads: bool,
    /// The dock the sections live in, to find where each one is.
    dock: Option<WeakEntity<DockArea>>,
    /// The folded sections' headers under the dock, made on first use.
    folded: Option<Entity<FoldedStack>>,
    /// Redraws the Checks section each second while a job it lists runs, so
    /// the elapsed time ticks.
    tick: Option<Task<()>>,
    /// What a click on an inbox row last brought forward, so a further click
    /// toggles from it.
    last_view: Option<(PrKey, PanelKind)>,
}

impl EventEmitter<SidebarEvent> for Sidebar {}

impl Focusable for Sidebar {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

struct GlobalSidebar(Entity<Sidebar>);

impl Global for GlobalSidebar {}

impl Sidebar {
    fn has_running_job(&self, cx: &App) -> bool {
        self.session.read(cx).open_pr().is_some_and(|open| {
            check_rows(open)
                .iter()
                .any(|job| job.state == CiState::Running && job.started.is_some())
        })
    }

    fn sync_tick(&mut self, cx: &mut Context<Self>) {
        if self.tick.is_some() || !self.has_running_job(cx) {
            return;
        }
        self.tick = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let again = this
                    .update(cx, |this, cx| {
                        let running = this.has_running_job(cx);
                        if running {
                            cx.notify();
                        } else {
                            this.tick = None;
                        }
                        running
                    })
                    .unwrap_or(false);
                if !again {
                    break;
                }
            }
        }));
    }

    /// The folded sections' headers, drawn under the sidebar's dock.
    pub fn folded_stack(&mut self, cx: &mut Context<Self>) -> Entity<FoldedStack> {
        if let Some(stack) = &self.folded {
            return stack.clone();
        }
        let me = cx.entity();
        let stack = cx.new(|cx| FoldedStack::new(me, cx));
        self.folded = Some(stack.clone());
        stack
    }

    /// The one sidebar, built on first use: dock panels restored from a
    /// saved layout ask for it before the shell exists.
    pub fn global(cx: &mut App) -> Entity<Self> {
        if let Some(sidebar) = cx.try_global::<GlobalSidebar>() {
            return sidebar.0.clone();
        }
        let sidebar = cx.new(Self::new);
        cx.set_global(GlobalSidebar(sidebar.clone()));
        sidebar
    }

    fn new(cx: &mut Context<Self>) -> Self {
        let session = Session::global(cx);
        cx.observe(&session, |this, _, cx| {
            this.sync_sections(cx);
            this.sync_tick(cx);
            this.advance_row_action(cx);
            this.release_idle(cx);
            cx.notify();
        })
        .detach();
        let marks = Marks::global(cx);
        cx.observe(&marks, |_, _, cx| cx.notify()).detach();
        if let Some(subscriptions) = Subscriptions::global(cx) {
            cx.observe(&subscriptions, |_, _, cx| cx.notify()).detach();
        }
        cx.observe_global::<ActiveTheme>(|this, cx| {
            this.restyle(cx);
            cx.notify();
        })
        .detach();
        let mut sidebar = Self {
            session,
            focus: cx.focus_handle(),
            cursor: None,
            inbox: InboxState::default(),
            spinners: SpinnerSlots::default(),
            intent: None,
            thread_cursor: None,
            on_threads: false,
            dock: None,
            folded: None,
            tick: None,
            last_view: None,
        };
        sidebar.inbox.refresh_slots = SpinnerSlots::for_dock(cx);
        sidebar.sync_sections(cx);
        sidebar
    }

    /// The dock the sections are in; set by the shell.
    pub fn set_dock(&mut self, dock: WeakEntity<DockArea>) {
        self.dock = Some(dock);
    }

    pub(crate) fn dock(&self) -> Option<Entity<DockArea>> {
        self.dock.as_ref()?.upgrade()
    }

    pub fn session(&self) -> &Entity<Session> {
        &self.session
    }

    /// Prefetches `key` unless the intent moves on within [`INTENT_DELAY`].
    pub(crate) fn intend(&mut self, key: PrKey, cx: &mut Context<Self>) {
        if self.intent.as_ref().is_some_and(|(k, _)| *k == key) {
            return;
        }
        let target = key.clone();
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(INTENT_DELAY).await;
            let _ = this.update(cx, |this, cx| {
                this.session.read(cx).prefetch(&target);
            });
        });
        self.intent = Some((key, task));
    }

    /// Drops the pending prefetch of `key`, if it is the pending one.
    pub(crate) fn forget_intent(&mut self, key: &PrKey) {
        if self.intent.as_ref().is_some_and(|(k, _)| k == key) {
            self.intent = None;
        }
    }

    /// Opens `key`'s `kind` panel and brings it forward.
    pub(crate) fn show(
        &mut self,
        key: &PrKey,
        kind: PanelKind,
        mode: OpenMode,
        cx: &mut Context<Self>,
    ) {
        self.intent = None;
        self.session
            .update(cx, |session, cx| session.show(key, kind, mode, cx));
    }

    /// A click on an inbox row: the first previews its conversation; each
    /// further one on the same row toggles between its diff and its
    /// conversation, and keeps the tab.
    pub(crate) fn click_row(&mut self, cursor: Cursor, beside: bool, cx: &mut Context<Self>) {
        let again = self.cursor.as_ref() == Some(&cursor) && self.has_view(&cursor.key, cx);
        let key = cursor.key.clone();
        self.cursor = Some(cursor);
        self.on_threads = false;
        match (beside, again) {
            (true, _) => self.show_row(&key, PanelKind::Diff, OpenMode::Beside, cx),
            (false, true) => self.toggle_row_view(&key, cx),
            (false, false) => self.show_row(&key, PanelKind::Conversation, OpenMode::Preview, cx),
        }
    }

    /// Shows `key`'s `kind` panel from its inbox row; opening it reads it
    /// when the settings say so, also when it already had the focus.
    pub(crate) fn show_row(
        &mut self,
        key: &PrKey,
        kind: PanelKind,
        mode: OpenMode,
        cx: &mut Context<Self>,
    ) {
        self.last_view = Some((key.clone(), kind));
        self.show(key, kind, mode, cx);
        let mark_seen = cx
            .try_global::<crate::appearance::Appearance>()
            .is_none_or(|state| state.settings.inbox.mark_seen_on_open);
        if let Some(row) = self.session.read(cx).inbox.get(key).cloned()
            && mark_seen
        {
            Marks::global(cx).update(cx, |marks, cx| marks.mark_seen(&row, cx));
        }
    }

    /// Brings the other of `key`'s diff and conversation forward; when
    /// neither is in front (Settings covers them), the last one shown.
    fn toggle_row_view(&mut self, key: &PrKey, cx: &mut Context<Self>) {
        let last = self
            .last_view
            .as_ref()
            .filter(|(at, _)| at == key)
            .map(|(_, kind)| *kind);
        let kind = match (
            self.is_shown(key, PanelKind::Conversation, cx),
            self.is_shown(key, PanelKind::Diff, cx),
        ) {
            (true, false) => PanelKind::Diff,
            (false, true) => PanelKind::Conversation,
            (true, true) if last == Some(PanelKind::Diff) => PanelKind::Conversation,
            (true, true) => PanelKind::Diff,
            (false, false) => last.unwrap_or(PanelKind::Conversation),
        };
        self.show_row(key, kind, OpenMode::Permanent, cx);
    }

    /// Whether `key` has a panel in the dock.
    fn has_view(&self, key: &PrKey, cx: &App) -> bool {
        self.session
            .read(cx)
            .panels()
            .iter()
            .any(|entry| entry.pr.as_ref() == Some(key))
    }

    /// Whether `key`'s `kind` panel is the shown tab of its group.
    fn is_shown(&self, key: &PrKey, kind: PanelKind, cx: &App) -> bool {
        let Some(id) = self
            .session
            .read(cx)
            .panels()
            .iter()
            .find(|entry| entry.kind == kind && entry.pr.as_ref() == Some(key))
            .map(|entry| PanelId::from(entry.id))
        else {
            return false;
        };
        let Some(dock) = self.dock() else {
            return false;
        };
        let dock = dock.read(cx);
        [
            DockPlacement::Center,
            DockPlacement::Left,
            DockPlacement::Right,
            DockPlacement::Bottom,
        ]
        .into_iter()
        .filter_map(|region| dock.layout(region))
        .any(|tree| {
            let node = tree
                .find_panel_node(id)
                .and_then(|node| tree.find_node(node));
            matches!(
                node.map(|node| node.kind()),
                Some(PaneRef::Tabs { panels, active_ix }) if panels.get(active_ix) == Some(&id)
            )
        })
    }

    fn on_next(&mut self, cx: &mut Context<Self>) {
        if self.on_threads {
            self.move_thread_cursor(1, cx);
        } else {
            self.move_cursor(1, cx);
        }
    }

    fn on_prev(&mut self, cx: &mut Context<Self>) {
        if self.on_threads {
            self.move_thread_cursor(-1, cx);
        } else {
            self.move_cursor(-1, cx);
        }
    }

    fn on_open(&mut self, cx: &mut Context<Self>) {
        if self.on_threads {
            if let Some(id) = self.thread_cursor.clone()
                && let Some(key) = self.session.read(cx).open_key().cloned()
            {
                thread_menu::jump_to_line(&key, &id, false, cx);
            }
        } else if let Some(cursor) = self.cursor.clone() {
            if self.has_view(&cursor.key, cx) {
                self.toggle_row_view(&cursor.key, cx);
            } else {
                self.show_row(&cursor.key, PanelKind::Conversation, OpenMode::Preview, cx);
            }
        }
    }

    fn on_open_beside(&mut self, cx: &mut Context<Self>) {
        if let Some(cursor) = self.cursor.clone() {
            self.show_row(&cursor.key, PanelKind::Diff, OpenMode::Beside, cx);
        }
    }

    /// The sidebar's keys on `el`, a panel's root, acting on `sidebar`.
    pub(crate) fn key_handlers<E: InteractiveElement>(el: E, sidebar: &Entity<Self>) -> E {
        let on = |sidebar: &Entity<Self>, act: fn(&mut Self, &mut Context<Self>)| {
            let sidebar = sidebar.clone();
            move |cx: &mut App| sidebar.update(cx, act)
        };
        let (next, prev, open, beside) = (
            on(sidebar, Self::on_next),
            on(sidebar, Self::on_prev),
            on(sidebar, Self::on_open),
            on(sidebar, Self::on_open_beside),
        );
        let (viewed, down, up) = (sidebar.clone(), sidebar.clone(), sidebar.clone());
        el.key_context(KEY_CONTEXT)
            .on_action(move |_: &SelectNext, _, cx| next(cx))
            .on_action(move |_: &SelectPrev, _, cx| prev(cx))
            .on_action(move |_: &OpenSelected, _, cx| open(cx))
            .on_action(move |_: &OpenBeside, _, cx| beside(cx))
            .on_action(move |_: &ToggleViewed, _, cx| {
                let session = viewed.read(cx).session.clone();
                session.update(cx, |session, cx| session.toggle_selected_viewed(cx));
            })
            .on_action(move |_: &NextInStack, _, cx| {
                let session = down.read(cx).session.clone();
                session.update(cx, |session, cx| session.step_stack(StackStep::Down, cx));
            })
            .on_action(move |_: &PrevInStack, _, cx| {
                let session = up.read(cx).session.clone();
                session.update(cx, |session, cx| session.step_stack(StackStep::Up, cx));
            })
    }

    /// Moves along the thread list of the open pull request.
    fn move_thread_cursor(&mut self, step: isize, cx: &mut Context<Self>) {
        let Some(open) = self.session.read(cx).open_pr() else {
            return;
        };
        let threads = open.threads.clone();
        let order = thread_order(&threads);
        if order.is_empty() {
            return;
        }
        let current = self
            .thread_cursor
            .as_ref()
            .and_then(|id| order.iter().position(|&ix| threads[ix].id == *id));
        let next = match current {
            None if step > 0 => 0,
            None => order.len() - 1,
            Some(at) => at.saturating_add_signed(step).min(order.len() - 1),
        };
        self.thread_cursor = Some(threads[order[next]].id.clone());
        cx.notify();
    }

    pub(crate) fn ci(&self, t: &Theme, state: CiState, size: f32) -> AnyElement {
        ui::ci_icon(t, state, size, &self.spinners)
    }

    /// The focused pull request, copied out so rows can hold listeners.
    fn detail(&self, cx: &App) -> Option<Detail> {
        self.session.read(cx).open_pr().map(Detail::of)
    }

    /// Whether the focused pull request has anything for `section`, so its
    /// panel takes space.
    pub(crate) fn has_stack(&self, cx: &App) -> bool {
        self.session
            .read(cx)
            .open_pr()
            .is_some_and(|open| open.stack.members.len() >= 2)
    }

    pub(crate) fn has_threads(&self, cx: &App) -> bool {
        self.session
            .read(cx)
            .open_pr()
            .is_some_and(|open| open.threads.iter().any(|t| t.anchor.is_some()))
    }

    pub(crate) fn has_pr(&self, cx: &App) -> bool {
        self.session.read(cx).open_pr().is_some()
    }

    /// The focused pull request's row pinned above its sections: CI, its
    /// reference, title and the watch bell. It never scrolls, so scrolled
    /// content never meets the edge of the panel above.
    pub(crate) fn title_row(&self, t: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let open = self.detail(cx)?;
        let c = &t.colors;
        let pr = &open.pr;
        let title_text = if pr.title.is_empty() {
            SharedString::from("loading")
        } else {
            pr.title.clone()
        };
        let number = pr.number();
        Some(
            ui::bar(cx)
                .debug_selector(move || format!("pinned-title-{number}"))
                .h(px(ROW + 2.0))
                .flex_none()
                .flex()
                .px(px(10.0))
                .gap(px(8.0))
                .items_center()
                .border_b_1()
                .border_color(c.border)
                .child(self.ci(t, pr.ci, CI_ROW))
                .child(
                    div()
                        .mono(t)
                        .tiny(t)
                        .flex_none()
                        .text_color(c.text_secondary)
                        .child(pr.ref_label()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(c.text)
                        .child(title_text),
                )
                .children(self.watch_toggle(&open.key, cx))
                .into_any_element(),
        )
    }

    /// The stack top first, as Graphite and GitHub draw it: the pull request
    /// furthest from the trunk at the top, numbered N down to 1, and the
    /// branch the bottom one merges into last.
    pub(crate) fn stack_section(&self, t: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let open = self.detail(cx)?;
        let stack = &open.stack;
        if stack.members.len() < 2 {
            return None;
        }
        let c = &t.colors;
        let hover = c.hover;
        let rows = stack.members.iter().enumerate().rev().map(|(pos, member)| {
            let current = member.key == open.key;
            let key = member.key.clone();
            h_flex()
                .id(("stack", pos))
                .w_full()
                .h(px(ROW))
                .px(px(10.0))
                .gap(px(8.0))
                .items_center()
                .cursor_pointer()
                .when(current, |el| el.bg(c.selected))
                .when(!current, |el| el.hover(move |s| s.bg(hover)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.show(&key, PanelKind::Diff, OpenMode::Preview, cx);
                }))
                .child(
                    div()
                        .mono(t)
                        .tiny(t)
                        .w(px(14.0))
                        .text_color(c.text_muted)
                        .child((pos + 1).to_string()),
                )
                .child(self.ci(t, member.ci, CI_ROW))
                .child(ui::avatar(
                    cx,
                    &open.key.repo.host,
                    ui::Who::new(&member.author, member.author_avatar.as_deref()),
                    ui::avatar::ROW,
                ))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(if current { c.text } else { c.text_secondary })
                        .when(current, |el| el.font_weight(FontWeight::SEMIBOLD))
                        .child(member.title.clone()),
                )
                .child(
                    div()
                        .mono(t)
                        .tiny(t)
                        .text_color(c.text_muted)
                        .child(format!("{}{}", open.pr.forge.sigil(), member.key.number)),
                )
                .into_any_element()
        });
        let base = h_flex()
            .h(px(ROW))
            .px(px(10.0))
            .gap(px(8.0))
            .items_center()
            .text_color(c.text_muted)
            .child(div().w(px(14.0)))
            .child(
                Icon::new(IconName::GitBranch)
                    .size(px(CI_ROW))
                    .text_color(c.text_faint),
            )
            .child(div().mono(t).tiny(t).truncate().child(stack.base.clone()));
        Some(
            v_flex()
                .w_full()
                .children(rows)
                .child(base)
                .into_any_element(),
        )
    }

    /// The checks: failing and canceled jobs first, then running and queued
    /// ones, each a row that opens the job's log; one line for the rest. A
    /// click on the line or the overflow row shows the Checks tab.
    pub(crate) fn checks_section(&self, t: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let open = self.detail(cx)?;
        let c = &t.colors;
        let hover = c.hover;
        if open.jobs.is_empty() {
            return Some(self.checks_note(&open, t, cx));
        }
        let (mut shown, rest): (Vec<&CheckRow>, Vec<&CheckRow>) =
            open.jobs.iter().partition(|job| job.is_listed());
        shown.sort_by_key(|job| job.rank());
        let overflow = shown.len().saturating_sub(CHECK_ROWS);
        shown.truncate(CHECK_ROWS);
        let rows = shown.into_iter().enumerate().map(|(ix, job)| {
            let job_id = job.job.clone();
            let menu = JobMenu::new(open.key.clone(), job.job.clone());
            let selector = format!("sidebar-check-{ix}");
            h_flex()
                .id(("sidebar-check", ix))
                .debug_selector(move || selector)
                .w_full()
                .h(px(ROW))
                .flex_none()
                .px(px(10.0))
                .gap(px(8.0))
                .items_center()
                .cursor_pointer()
                .hover(move |s| s.bg(hover))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.session.update(cx, |session, cx| {
                        session.show_panel(PanelKind::Checks, cx);
                        if let Some(job) = &job_id {
                            session.open_log(job, None, cx);
                        }
                    });
                }))
                .child(self.ci(t, job.state, CI_ROW))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(c.text)
                        .child(job.name.clone()),
                )
                .children(job.context.clone().map(|context| {
                    div()
                        .flex_shrink(1.0)
                        .min_w_0()
                        .max_w(relative(0.4))
                        .truncate()
                        .tiny(t)
                        .text_color(c.text_faint)
                        .child(context)
                }))
                .child(
                    div()
                        .flex_none()
                        .mono(t)
                        .tiny(t)
                        .text_color(ci_color(t, job.state))
                        .child(job.duration_label()),
                )
                .context_menu(move |popup, _, cx| menu.popup(popup, cx))
                .into_any_element()
        });
        let link = |id: &'static str, label: String, icon: Option<CiState>| {
            h_flex()
                .id(id)
                .debug_selector(move || id.to_owned())
                .w_full()
                .h(px(ROW))
                .flex_none()
                .px(px(10.0))
                .gap(px(8.0))
                .items_center()
                .cursor_pointer()
                .text_color(c.text_muted)
                .hover(move |s| s.bg(hover))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.session
                        .update(cx, |session, cx| session.show_panel(PanelKind::Checks, cx));
                }))
                .children(icon.map(|state| self.ci(t, state, CI_ROW)))
                .child(div().min_w_0().truncate().small(t).child(label))
                .into_any_element()
        };
        let more =
            (overflow > 0).then(|| link("sidebar-checks-more", format!("+{overflow} more"), None));
        let summary = rest_summary(&rest)
            .map(|(label, state)| link("sidebar-checks-summary", label, Some(state)));
        Some(
            v_flex()
                .w_full()
                .children(rows)
                .children(more)
                .children(summary)
                .into_any_element(),
        )
    }

    /// What the section says when no job is known: the forge has not
    /// answered, or reports no checks. A click shows the Checks tab.
    fn checks_note(&self, open: &Detail, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let hover = t.colors.hover;
        let note = match open.status {
            OpenStatus::Loading if open.pr.source.is_some() => "Loading checks",
            _ => "No checks reported",
        };
        div()
            .id("sidebar-checks")
            .w_full()
            .h(px(ROW))
            .px(px(10.0))
            .flex()
            .items_center()
            .cursor_pointer()
            .text_color(t.colors.text_muted)
            .hover(move |s| s.bg(hover))
            .on_click(cx.listener(|this, _, _, cx| {
                this.session
                    .update(cx, |session, cx| session.show_panel(PanelKind::Checks, cx));
            }))
            .child(note)
            .into_any_element()
    }

    /// The open pull request's inline threads, by file and line. A click
    /// shows one in the Conversation; a double click or Enter at its line in
    /// the diff. The path gives way in its middle and the message takes the
    /// rest of the row.
    pub(crate) fn threads_section(&self, t: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let open = self.detail(cx)?;
        let order = thread_order(&open.threads);
        if order.is_empty() {
            return None;
        }
        let c = &t.colors;
        let hover = c.hover;
        let rows = order.into_iter().enumerate().map(|(pos, ix)| {
            let thread = &open.threads[ix];
            let id = thread.id.clone();
            let key = open.key.clone();
            let menu = ThreadMenu::new(key.clone(), id.clone(), Origin::Sidebar);
            let selected = self.on_threads && self.thread_cursor.as_ref() == Some(&thread.id);
            let resolved = thread.is_resolved();
            let location = threads::location(thread);
            let avatar = thread
                .comments
                .first()
                .map(|c| ui::avatar(cx, &open.key.repo.host, &c.author, ui::avatar::ROW));
            h_flex()
                .id(("thread-row", ix))
                .debug_selector(move || format!("thread-row-{pos}"))
                .w_full()
                .h(px(ROW))
                .flex_none()
                .px(px(10.0))
                .gap(px(8.0))
                .items_center()
                .cursor_pointer()
                .when(selected, |el| el.bg(c.selected))
                .when(!selected, |el| el.hover(move |s| s.bg(hover)))
                .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _, cx| {
                    this.thread_cursor = Some(id.clone());
                    this.on_threads = true;
                    if event.click_count() >= 2 {
                        thread_menu::jump_to_line(&key, &id, false, cx);
                    } else {
                        thread_menu::show_in_conversation(&key, &id, cx);
                    }
                    cx.notify();
                }))
                .child(
                    div()
                        .size(px(7.0))
                        .flex_none()
                        .rounded_full()
                        .bg(threads::rail_color(t, thread)),
                )
                .child(middle_path(t, location).max_w(relative(PATH_SHARE)))
                .children(avatar)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(if resolved { c.text_muted } else { c.text })
                        .child(threads::preview(thread)),
                )
                .when(thread.outdated, |el| {
                    el.child(
                        div()
                            .flex_none()
                            .tiny(t)
                            .text_color(c.warn)
                            .child("outdated"),
                    )
                })
                .child(
                    div()
                        .flex_none()
                        .mono(t)
                        .tiny(t)
                        .text_color(c.text_muted)
                        .child(thread.comments.len().to_string()),
                )
                .context_menu(move |popup, _, cx| menu.popup(popup, cx))
                .into_any_element()
        });
        Some(v_flex().w_full().children(rows).into_any_element())
    }

    /// Bell for a watched pull request, crossed out for one that is not.
    fn watch_toggle(&self, key: &PrKey, cx: &mut Context<Self>) -> Option<AnyElement> {
        let subscriptions = Subscriptions::global(cx)?;
        let watched = subscriptions.read(cx).is_watched(key, cx);
        let key = key.clone();
        let icon = if watched {
            IconName::Bell
        } else {
            IconName::BellOff
        };
        Some(
            ui::icon_button(cx, "watch-open", icon, move |_, _, cx| {
                subscriptions.update(cx, |s, cx| s.toggle(&key, cx));
            })
            .into_any_element(),
        )
    }

    /// What a section's tab says after its name: how much is in it.
    pub(crate) fn note(&self, section: crate::sidebar_panels::Section, cx: &App) -> String {
        use crate::sidebar_panels::Section;

        let session = self.session.read(cx);
        match section {
            Section::Inbox => return format!("{} open", session.inbox.prs.len()),
            Section::Repos => return self.repos_note(cx),
            _ => {}
        }
        let Some(open) = session.open_pr() else {
            return String::new();
        };
        match section {
            Section::Inbox | Section::Repos => String::new(),
            Section::Stack => format!("{} pull requests", open.stack.members.len()),
            Section::Checks => {
                let failing = open
                    .checks
                    .iter()
                    .filter(|c| c.state == CiState::Fail)
                    .count();
                match (open.checks.len(), failing) {
                    (0, _) => String::new(),
                    (n, 0) => n.to_string(),
                    (n, f) => format!("{f} of {n} failing"),
                }
            }
            Section::Threads => {
                let anchored = open.threads.iter().filter(|t| t.anchor.is_some());
                let total = anchored.clone().count();
                let unresolved = anchored.filter(|t| !t.is_resolved()).count();
                format!("{unresolved} open, {total} total")
            }
            Section::Files => {
                let total = open.files.len();
                format!("{} / {total} viewed", open.viewed.count(&open.files))
            }
        }
    }
}

/// What the sections draw of the open pull request, copied out of the
/// session so the rows can register listeners on the sidebar.
struct Detail {
    key: PrKey,
    pr: crate::model::PullRequest,
    stack: crate::model::Stack,
    jobs: Vec<CheckRow>,
    files: Vec<ChangedFile>,
    viewed: Viewed,
    status: OpenStatus,
    threads: Arc<[Thread]>,
}

impl Detail {
    fn of(open: &OpenPr) -> Self {
        Self {
            key: open.key.clone(),
            pr: open.pr.clone(),
            stack: open.stack.clone(),
            jobs: check_rows(open),
            files: open.files.clone(),
            viewed: open.viewed.clone(),
            status: open.status.clone(),
            threads: open.threads.clone(),
        }
    }
}

/// A path that ellipsizes in its middle, keeping the directory it starts
/// with and the file name; it shrinks before anything else in its row.
pub(crate) fn middle_path(t: &Theme, path: String) -> gpui::Div {
    div()
        .flex_shrink(1.0)
        .min_w(px(40.0))
        .overflow_hidden()
        .whitespace_nowrap()
        .text_ellipsis_middle()
        .mono(t)
        .tiny(t)
        .text_color(t.colors.text_secondary)
        .child(path)
}

/// The anchored threads, by file and then line, as indices.
fn thread_order(threads: &[Thread]) -> Vec<usize> {
    let mut order: Vec<usize> = threads
        .iter()
        .enumerate()
        .filter(|(_, thread)| thread.anchor.is_some())
        .map(|(ix, _)| ix)
        .collect();
    order.sort_by_key(|&ix| {
        let anchor = threads[ix].anchor.as_ref();
        (
            anchor.map(|a| a.path.clone()),
            anchor
                .and_then(|a| a.best_position())
                .map_or(0, |p| p.line.line),
        )
    });
    order
}

/// The viewed progress under the Files tab: a thin bar.
fn files_progress(t: &Theme, open: &Detail) -> AnyElement {
    let c = &t.colors;
    let total = open.files.len();
    let done = open.viewed.count(&open.files);
    let fraction = if total == 0 {
        0.0
    } else {
        done as f32 / total as f32
    };
    div()
        .h(px(2.0))
        .w_full()
        .flex_none()
        .bg(c.border)
        .child(div().h_full().w(relative(fraction)).bg(c.accent))
        .into_any_element()
}

#[derive(Default)]
struct Node {
    dirs: BTreeMap<String, Node>,
    files: Vec<ChangedFile>,
}

impl Node {
    /// Every file at or under this directory.
    fn descendants(&self) -> Vec<ChangedFile> {
        let mut out = self.files.clone();
        for dir in self.dirs.values() {
            out.extend(dir.descendants());
        }
        out
    }
}

enum RowKind {
    /// A directory with all the files under it.
    Dir(Vec<ChangedFile>),
    File(ChangedFile),
}

struct TreeRow {
    depth: usize,
    name: String,
    kind: RowKind,
    /// The directory row at each level above this one, outermost first.
    ancestors: Vec<usize>,
}

fn flatten(node: &Node, ancestors: &mut Vec<usize>, out: &mut Vec<TreeRow>) {
    let depth = ancestors.len();
    for (name, child) in &node.dirs {
        // A directory holding only one directory is shown as one row.
        let (mut label, mut cursor) = (name.clone(), child);
        while cursor.files.is_empty() && cursor.dirs.len() == 1 {
            let (next_name, next) = cursor.dirs.iter().next().expect("one entry");
            label = format!("{label}/{next_name}");
            cursor = next;
        }
        out.push(TreeRow {
            depth,
            name: label,
            kind: RowKind::Dir(cursor.descendants()),
            ancestors: ancestors.clone(),
        });
        ancestors.push(out.len() - 1);
        flatten(cursor, ancestors, out);
        ancestors.pop();
    }
    for file in &node.files {
        let name = file
            .path
            .rsplit('/')
            .next()
            .unwrap_or(&file.path)
            .to_owned();
        out.push(TreeRow {
            depth,
            name,
            kind: RowKind::File(file.clone()),
            ancestors: ancestors.clone(),
        });
    }
}

fn tree_rows(files: &[ChangedFile]) -> Vec<TreeRow> {
    let mut root = Node::default();
    for file in files {
        let mut node = &mut root;
        let mut parts = file.path.split('/').peekable();
        while let Some(part) = parts.next() {
            if parts.peek().is_some() {
                node = node.dirs.entry(part.to_owned()).or_default();
            }
        }
        node.files.push(file.clone());
    }
    let mut rows = Vec::new();
    flatten(&root, &mut Vec::new(), &mut rows);
    rows
}

/// The directory row whose guide is drawn strong: the selected file's parent.
fn selected_parent(rows: &[TreeRow], selected: Option<&SharedString>) -> Option<usize> {
    let selected = selected?;
    rows.iter()
        .find(|row| matches!(&row.kind, RowKind::File(file) if &file.path == selected))
        .and_then(|row| row.ancestors.last().copied())
}

impl Sidebar {
    /// The viewed progress and the file tree of the focused pull request.
    pub(crate) fn files_section(&self, t: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let open = self.detail(cx)?;
        let c = &t.colors;
        let hover = c.hover;
        let viewed: &Viewed = &open.viewed;
        let selected = self.session.read(cx).selected_file().cloned();
        let all_changes = all_changes_row(t, open.files.len(), selected.is_none(), cx);
        let rows = tree_rows(&open.files);
        let strong = selected_parent(&rows, selected.as_ref());
        let rows = rows.into_iter().enumerate().map(|(ix, row)| {
            let indent = px(TREE_PAD + TREE_INDENT * row.depth as f32);
            // One guide per enclosing directory, each running through every
            // row under it; no elbows.
            let guides = row.ancestors.iter().enumerate().map(|(level, &dir)| {
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(px(GUIDE_X + TREE_INDENT * level as f32))
                    .w(px(1.0))
                    .bg(if Some(dir) == strong {
                        c.border_strong
                    } else {
                        c.border
                    })
            });
            let base = h_flex()
                .id(("file", ix))
                .relative()
                .w_full()
                .h(px(ROW))
                .flex_none()
                .pr(px(10.0))
                .pl(indent)
                .gap(px(6.0))
                .items_center()
                .children(guides);
            match row.kind {
                RowKind::Dir(files) => {
                    let coverage = viewed.coverage(&files);
                    base.hover(move |s| s.bg(hover))
                        .child(
                            ui::checkbox(("dir-viewed", ix))
                                .checked(coverage == Coverage::Full)
                                .indeterminate(coverage == Coverage::Partial)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.session.update(cx, |session, cx| {
                                        session.toggle_viewed_all(&files, cx);
                                    });
                                })),
                        )
                        .child(
                            Icon::new(IconName::Folder)
                                .size(px(14.0))
                                .text_color(c.text_muted),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_color(c.text_secondary)
                                .child(row.name),
                        )
                }
                RowKind::File(file) => {
                    let is_viewed = viewed.contains(&file);
                    let is_selected = selected.as_ref() == Some(&file.path);
                    let (text, plus, minus) = if is_viewed {
                        (c.text_muted, c.text_muted, c.text_muted)
                    } else {
                        (c.text, c.ok, c.danger)
                    };
                    let toggled = file.clone();
                    let path = file.path.clone();
                    base.cursor_pointer()
                        .when(is_selected, |el| el.bg(c.selected))
                        .when(!is_selected, |el| el.hover(move |s| s.bg(hover)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let path = path.clone();
                            this.session
                                .update(cx, |session, cx| session.select_file(path, cx));
                        }))
                        .child(
                            ui::checkbox(("file-viewed", ix))
                                .checked(is_viewed)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.session.update(cx, |session, cx| {
                                        session.toggle_viewed(&toggled, cx);
                                    });
                                })),
                        )
                        .child(
                            div()
                                .mono(t)
                                .w(px(14.0))
                                .tiny(t)
                                .text_color(status_color(t, file.status))
                                .child(status_letter(file.status)),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_color(text)
                                .child(row.name),
                        )
                        .when(file.added > 0, |el| {
                            el.child(
                                div()
                                    .mono(t)
                                    .tiny(t)
                                    .text_color(plus)
                                    .child(format!("+{}", file.added)),
                            )
                        })
                        .when(file.removed > 0, |el| {
                            el.child(
                                div()
                                    .mono(t)
                                    .tiny(t)
                                    .text_color(minus)
                                    .child(format!("-{}", file.removed)),
                            )
                        })
                }
            }
        });
        Some(
            v_flex()
                .w_full()
                .child(files_progress(t, &open))
                .child(all_changes)
                .children(rows)
                .into_any_element(),
        )
    }
}

/// The tree's first row: the whole diff, selected until a file is picked;
/// a click goes back to it from a file shown alone.
fn all_changes_row(
    t: &Theme,
    files: usize,
    selected: bool,
    cx: &mut Context<Sidebar>,
) -> AnyElement {
    let c = &t.colors;
    let hover = c.hover;
    let label = if files == 1 {
        "All changes (1 file)".to_owned()
    } else {
        format!("All changes ({files} files)")
    };
    h_flex()
        .id("tree-all-changes")
        .debug_selector(|| "tree-all-changes".into())
        .w_full()
        .h(px(ROW))
        .flex_none()
        .px(px(TREE_PAD))
        .gap(px(6.0))
        .items_center()
        .cursor_pointer()
        .when(selected, |el| el.bg(c.selected))
        .when(!selected, |el| el.hover(move |s| s.bg(hover)))
        .on_click(cx.listener(|this, _, _, cx| {
            this.session.update(cx, crate::diff_panel::show_all_changes);
        }))
        .child(
            Icon::new(IconName::FileDiff)
                .size(px(14.0))
                .text_color(c.text_muted),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(c.text)
                .child(label),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::FileStatus;

    fn file(path: &str) -> ChangedFile {
        ChangedFile {
            path: path.to_owned().into(),
            blob: "b".into(),
            status: FileStatus::Modified,
            added: 1,
            removed: 0,
        }
    }

    #[test]
    fn directory_rows_roll_up_their_descendants() {
        let files = [
            file("crates/x/src/a.rs"),
            file("crates/x/src/b.rs"),
            file("crates/x/tests/t.rs"),
            file("docs/d.md"),
        ];
        let mut viewed = Viewed::default();
        viewed.set(&files[0], true);
        viewed.set(&files[2], true);

        let coverage: Vec<(String, Coverage)> = tree_rows(&files)
            .into_iter()
            .filter_map(|row| match row.kind {
                RowKind::Dir(under) => Some((row.name, viewed.coverage(&under))),
                RowKind::File(_) => None,
            })
            .collect();
        assert_eq!(
            coverage,
            [
                ("crates/x".to_owned(), Coverage::Partial),
                ("src".to_owned(), Coverage::Partial),
                ("tests".to_owned(), Coverage::Full),
                ("docs".to_owned(), Coverage::Empty),
            ]
        );
    }

    #[test]
    fn toggling_a_directory_marks_everything_under_it() {
        let files = [file("d/a.rs"), file("d/b.rs")];
        let mut viewed = Viewed::default();
        let rows = tree_rows(&files);
        let RowKind::Dir(under) = &rows[0].kind else {
            panic!("first row is the directory");
        };
        viewed.toggle_all(under);
        assert_eq!(viewed.count(&files), 2);
        viewed.toggle_all(under);
        assert_eq!(viewed.count(&files), 0);
    }

    #[test]
    fn guides_run_through_a_directory_and_stop_after_it() {
        let files = [
            file("crates/x/src/a.rs"),
            file("crates/x/src/b.rs"),
            file("crates/x/tests/t.rs"),
            file("docs/d.md"),
            file("top.rs"),
        ];
        let rows = tree_rows(&files);
        let shape: Vec<(&str, Vec<usize>)> = rows
            .iter()
            .map(|row| (row.name.as_str(), row.ancestors.clone()))
            .collect();
        assert_eq!(
            shape,
            [
                ("crates/x", vec![]),
                ("src", vec![0]),
                ("a.rs", vec![0, 1]),
                ("b.rs", vec![0, 1]),
                ("tests", vec![0]),
                ("t.rs", vec![0, 4]),
                ("docs", vec![]),
                ("d.md", vec![6]),
                ("top.rs", vec![]),
            ]
        );
        let selected: SharedString = "crates/x/tests/t.rs".into();
        assert_eq!(selected_parent(&rows, Some(&selected)), Some(4));
        assert_eq!(selected_parent(&rows, None), None);
    }
}

/// Most failing and running jobs the Checks section lists before "+N more".
pub const CHECK_ROWS: usize = 8;

/// One job in the Checks section.
#[derive(Clone, Debug, PartialEq)]
pub struct CheckRow {
    pub name: String,
    /// The workflow or stage, when it tells jobs apart.
    pub context: Option<String>,
    pub state: CiState,
    pub started: Option<Timestamp>,
    pub completed: Option<Timestamp>,
    /// What to show when there are no times (a rollup entry's own text).
    pub detail: Option<String>,
    /// What the log panel opens; `None` for a rollup entry.
    pub job: Option<JobId>,
}

impl CheckRow {
    /// Failing, canceled, running and queued jobs get a row of their own.
    #[must_use]
    pub fn is_listed(&self) -> bool {
        matches!(
            self.state,
            CiState::Fail | CiState::Canceled | CiState::Running | CiState::Queued
        )
    }

    /// Failing and canceled before running and queued.
    fn rank(&self) -> u8 {
        match self.state {
            CiState::Fail => 0,
            CiState::Canceled => 1,
            CiState::Running => 2,
            _ => 3,
        }
    }

    /// The right-hand text: how long it took, how long it has been running,
    /// or its state.
    #[must_use]
    pub fn duration_label(&self) -> String {
        match self.state {
            CiState::Queued => ui::ci_label(self.state).to_owned(),
            _ => crate::ci::duration(self.started, self.completed)
                .or_else(|| self.detail.clone())
                .unwrap_or_else(|| ui::ci_label(self.state).to_owned()),
        }
    }
}

/// The open pull request's jobs as the Checks section lists them: the runs'
/// jobs when the forge gave them, else the rollup's checks.
#[must_use]
pub fn check_rows(open: &OpenPr) -> Vec<CheckRow> {
    let several = open.ci.runs.len() > 1;
    let from_runs: Vec<CheckRow> = open
        .ci
        .runs
        .iter()
        .flat_map(|run| {
            run.jobs.iter().map(move |job| CheckRow {
                name: job.name.clone(),
                context: job
                    .stage
                    .clone()
                    .or_else(|| several.then(|| run.name.clone())),
                state: job.state,
                started: job.started_at,
                completed: job.completed_at,
                detail: None,
                job: Some(job.id.clone()),
            })
        })
        .collect();
    if !from_runs.is_empty() {
        return from_runs;
    }
    open.checks
        .iter()
        .map(|check| CheckRow {
            name: check.name.to_string(),
            context: None,
            state: check.state,
            started: None,
            completed: None,
            detail: Some(check.detail.to_string()),
            job: None,
        })
        .collect()
}

/// "4 passed · 2 skipped" for the jobs that get no row, with the state
/// whose icon leads the line.
fn rest_summary(rest: &[&CheckRow]) -> Option<(String, CiState)> {
    let count = |state: CiState| rest.iter().filter(|job| job.state == state).count();
    let (passed, skipped) = (count(CiState::Pass), count(CiState::Skipped));
    let parts: Vec<String> = [(passed, "passed"), (skipped, "skipped")]
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, word)| format!("{n} {word}"))
        .collect();
    (!parts.is_empty()).then(|| {
        let lead = if passed > 0 {
            CiState::Pass
        } else {
            CiState::Skipped
        };
        (parts.join(" · "), lead)
    })
}
