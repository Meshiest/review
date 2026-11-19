//! The Conversation panel: the pull request's description, then its
//! timeline in time order: conversation comments, review verdicts, and a
//! compact row per inline thread that jumps to it in the diff.
//!
//! The verdict row (a tinted circle with the verdict's icon, "name approved
//! these changes", the summary in a card edged in the verdict's color) is
//! adapted from voximity/dispatch2 (crates/app/src/ui/forge_common.rs,
//! commit 39fd9fd), AGPL-3.0.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use forge::model::{Comment, Thread, ThreadId};
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, FocusHandle, FontWeight,
    InteractiveElement as _, IntoElement, ParentElement, Pixels, Render, SharedString,
    StatefulInteractiveElement as _, Styled, Subscription, Task, Window, canvas, div, point,
    prelude::FluentBuilder as _, px,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::{Icon, dock::Panel, h_flex, scroll::ScrollableElement as _, v_flex};
use scroll::HandleAutoscroll;
use theme::Theme;

pub mod ci_box;
mod footer;

use self::ci_box::CiBoxes;
use self::footer::{Footer, FooterState};
use crate::appearance::{self, Appearance};
use crate::model::{CiState, PrKey};
use crate::panels::{Binding, PanelKind, dock_panel, header};
use crate::pr_header::{self, Facts, PrActions};
use crate::session::{CommentKind, LocalComment, Session};
use crate::settings::ThreadStyle;
use crate::side_column::{self, SideColumn};
use crate::suggestions::{self, BodyPart};
use crate::thread_code::{self, Excerpt, Job, Original, StoreOriginal};
use crate::threads::comment_menu::{CommentMenu, comment_url};
use crate::threads::thread_menu::{self, Origin, ThreadMenu, ThreadNav};
use crate::threads::{self, Bodies, ago, open_host, verdict_style};
use crate::ui::spinner::SpinnerSlots;
use crate::ui::{self, CI_HEADER, TextExt as _, ci_color, ci_icon, ci_label};
use crate::writing::{Writing, anchor_label};
use store::DraftAnchor;

/// The share of an inline thread's row its path may take; the message has
/// the rest.
const INLINE_PATH_SHARE: f32 = 0.4;
/// An inline thread's header line, folded or not, and the room above and
/// under it in the folded row: the chevron is in the same place either way.
const INLINE_HEAD: f32 = 20.0;
const INLINE_HEAD_PAD: f32 = 5.0;
/// From this panel width the side column sits beside the conversation;
/// narrower, it is stacked under the header.
pub const WIDE: f32 = 760.0;
/// The space above a thread scrolled to from elsewhere.
const REVEAL_MARGIN: f32 = 12.0;

/// The code of each inline thread, for one diff and one set of threads.
#[derive(Default)]
struct CodeCache {
    /// The pull request, diff generation and thread list the excerpts are for.
    stamp: Option<(PrKey, u64, usize)>,
    excerpts: Arc<HashMap<ThreadId, Arc<Excerpt>>>,
    ready: bool,
    _task: Option<Task<()>>,
}

pub struct ConversationPanel {
    focus: FocusHandle,
    boxes: Entity<CiBoxes>,
    session: Entity<Session>,
    binding: Binding,
    /// Review, merge and the rest, under the title.
    actions: Entity<PrActions>,
    /// Reviewers, assignees, labels and the milestone.
    side: Entity<SideColumn>,
    /// What the user writes on the pull request shown; an empty one until
    /// a pull request is open.
    writing: Entity<Writing>,
    _writing: Option<Subscription>,
    scroll: gpui::ScrollHandle,
    autoscroll: HandleAutoscroll,
    /// Parsed markdown of the open pull request's description and comments.
    bodies: Bodies,
    bodies_for: Option<PrKey>,
    code: CodeCache,
    /// The panel's width at the last paint, and whether the last render
    /// laid it out wide.
    width: Rc<Cell<Pixels>>,
    wide: Rc<Cell<bool>>,
    spinners: SpinnerSlots,
    /// What the merge box shows open, and the action it asked for.
    footer: FooterState,
    /// Threads asked for from elsewhere, and the ones folded by hand.
    nav: Entity<ThreadNav>,
    /// No code excerpt is still being read, so the threads keep their
    /// heights: a thread scrolled to stays where it was put.
    laid_out: bool,
}

dock_panel!(ConversationPanel, "Conversation", bound, spinners);

/// One timeline entry.
#[derive(Clone, Copy)]
enum Entry<'a> {
    /// A comment on the pull request itself.
    Comment(&'a Comment),
    /// A review summary with its verdict.
    Review(&'a Comment),
    /// An inline thread, shown as one row.
    Inline(&'a Thread),
}

/// The timeline: every comment of an anchorless thread and one row per
/// anchored thread, by time.
fn timeline(threads: &[Thread]) -> Vec<Entry<'_>> {
    let mut entries: Vec<(chrono::DateTime<chrono::Utc>, Entry<'_>)> = Vec::new();
    for thread in threads {
        if thread.anchor.is_some() {
            if let Some(first) = thread.comments.first() {
                entries.push((first.created_at, Entry::Inline(thread)));
            }
            continue;
        }
        for comment in &thread.comments {
            let entry = if comment.verdict.is_some() {
                Entry::Review(comment)
            } else if comment.body.trim().is_empty() {
                continue;
            } else {
                Entry::Comment(comment)
            };
            entries.push((comment.created_at, entry));
        }
    }
    entries.sort_by_key(|(at, _)| *at);
    entries.into_iter().map(|(_, entry)| entry).collect()
}

impl ConversationPanel {
    /// A Conversation panel that follows the focused pull request.
    pub fn new(window: &mut Window, cx: &mut App) -> Entity<Self> {
        Self::for_pr(None, window, cx)
    }

    /// A Conversation panel bound to `pr`, or following the focus for `None`.
    pub fn for_pr(pr: Option<PrKey>, window: &mut Window, cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            let session = Session::global(cx);
            cx.observe(&session, |_: &mut Self, _, cx| cx.notify())
                .detach();
            cx.observe_global::<Appearance>(|_: &mut Self, cx| cx.notify())
                .detach();
            let binding = Binding::new(pr, PanelKind::Conversation, cx);
            let boxes = CiBoxes::global(cx);
            cx.observe(&boxes, |_: &mut Self, _, cx| cx.notify())
                .detach();
            let nav = ThreadNav::global(cx);
            let scroll = gpui::ScrollHandle::new();
            cx.observe(&nav, |_: &mut Self, _, cx| cx.notify()).detach();
            Self {
                focus: cx.focus_handle(),
                session,
                binding,
                boxes,
                actions: cx.new(|cx| PrActions::new(false, window, cx)),
                side: cx.new(|cx| SideColumn::new(window, cx)),
                writing: cx.new(|_| Writing::with_log_sink()),
                _writing: None,
                autoscroll: HandleAutoscroll::new(scroll.clone()),
                scroll,
                bodies: Bodies::default(),
                bodies_for: None,
                code: CodeCache::default(),
                width: Rc::new(Cell::new(px(0.0))),
                wide: Rc::new(Cell::new(false)),
                spinners: SpinnerSlots::for_dock(cx),
                footer: FooterState::default(),
                nav,
                laid_out: false,
            }
        })
    }

    /// Follows the writing state of the pull request shown.
    fn sync_writing(&mut self, key: &PrKey, cx: &mut Context<Self>) {
        let Some(writing) = self.session.read(cx).writing_for(key) else {
            return;
        };
        if writing != self.writing {
            self._writing = Some(cx.observe(&writing, |_: &mut Self, _, cx| cx.notify()));
            self.writing = writing;
        }
    }

    /// The pull request this panel draws now.
    pub fn pr(&self, cx: &App) -> Option<PrKey> {
        self.binding.key(cx)
    }

    /// The review, merge and "..." bar under the title.
    pub fn actions(&self) -> &Entity<PrActions> {
        &self.actions
    }

    /// The reviewers, assignees and labels column.
    pub fn side_column(&self) -> &Entity<SideColumn> {
        &self.side
    }

    /// The code each inline thread is on, once read; `None` while it is
    /// being read.
    pub fn excerpt(&self, thread: &ThreadId) -> Option<Option<Arc<Excerpt>>> {
        self.code
            .ready
            .then(|| self.code.excerpts.get(thread).cloned())
    }

    /// Whether a middle-click autoscroll is running.
    pub fn is_autoscrolling(&self) -> bool {
        self.autoscroll.is_active()
    }

    /// How far the conversation is scrolled down, in pixels.
    pub fn scrolled(&self) -> f32 {
        -f32::from(self.scroll.offset().y)
    }

    /// Scrolls the conversation `scrolled` pixels down, as far as it goes.
    pub fn scroll_to(&mut self, scrolled: f32, cx: &mut Context<Self>) {
        let offset = self.scroll.offset();
        self.scroll.set_offset(point(offset.x, px(-scrolled)));
        cx.notify();
    }

    /// Whether the side column sat beside the conversation at the last
    /// render.
    pub fn is_wide(&self) -> bool {
        self.wide.get()
    }

    /// How inline threads show: the user's setting.
    pub fn thread_style(cx: &App) -> ThreadStyle {
        cx.try_global::<Appearance>()
            .map_or_else(ThreadStyle::default, |a| a.settings.conversation.threads)
    }

    fn set_thread_style(style: ThreadStyle, cx: &mut App) {
        appearance::update_quiet(cx, |settings| settings.conversation.threads = style);
    }

    /// Reads the code of `threads` on `key`'s diff off the main thread, once
    /// per diff and thread list.
    fn ensure_code(
        &mut self,
        key: &PrKey,
        diff: Option<(Arc<diff::Diff>, u64)>,
        threads: &Arc<[Thread]>,
        cx: &mut Context<Self>,
    ) {
        let Some((diff, generation)) = diff else {
            return;
        };
        let stamp = (
            key.clone(),
            generation,
            Arc::as_ptr(threads).cast::<()>() as usize,
        );
        if self.code.stamp.as_ref() == Some(&stamp) {
            return;
        }
        if self.code.stamp.as_ref().is_none_or(|(old, ..)| old != key) {
            self.code.excerpts = Arc::default();
            self.code.ready = false;
        }
        self.code.stamp = Some(stamp);
        let job = Job {
            diff,
            threads: threads.clone(),
            highlighter: thread_code::highlighter(cx),
        };
        let services = self.session.read(cx).services().cloned();
        let read = match services {
            // Live: outdated threads may read their commit from git.
            Some(services) => {
                let (objects, repo, number) =
                    (services.objects.clone(), key.repo.clone(), key.number.0);
                let run = services.run_blocking(move || {
                    let original = objects
                        .resolve(&repo)
                        .ok()
                        .map(|store| StoreOriginal { store, number });
                    thread_code::compute(&job, original.as_ref().map(|o| o as &dyn Original))
                });
                cx.background_spawn(async move { run.await.unwrap_or_default() })
            }
            None => cx.background_spawn(async move { thread_code::compute(&job, None) }),
        };
        let wanted = self.code.stamp.clone();
        self.code._task = Some(cx.spawn(async move |this, cx| {
            let excerpts = read.await;
            this.update(cx, |this, cx| {
                if this.code.stamp == wanted {
                    this.code.excerpts = Arc::new(excerpts);
                    this.code.ready = true;
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    fn markdown(&mut self, key: &str, text: &str, t: &Theme, cx: &mut App) -> AnyElement {
        let state = self.bodies.get(key, text, cx);
        threads::body(&state, t).into_any_element()
    }

    fn entry(
        &mut self,
        entry: &Entry<'_>,
        style: ThreadStyle,
        pr: &PrKey,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match *entry {
            Entry::Comment(comment) => self.comment(comment, t, cx),
            Entry::Review(comment) => self.review(comment, t, cx),
            Entry::Inline(thread)
                if self
                    .nav
                    .read(cx)
                    .is_folded(pr, &thread.id, thread.is_resolved(), style) =>
            {
                self.inline_row(thread, pr, t, cx)
            }
            Entry::Inline(thread) => self.inline_thread(thread, pr, t, cx),
        }
    }

    /// Whether `thread` is highlighted, and the marker that scrolls to it
    /// when it is the one asked for.
    fn reveal(&self, thread: &Thread, pr: &PrKey, cx: &App) -> (bool, Option<AnyElement>) {
        let nav = self.nav.read(cx);
        let highlighted = nav.is_highlighted(pr, &thread.id);
        let wanted = nav.pending_reveal(pr) == Some(&thread.id);
        (highlighted, wanted.then(|| self.reveal_marker(pr)))
    }

    /// Scrolls the conversation, once, so the element this is drawn in
    /// starts near the top of the view.
    fn reveal_marker(&self, pr: &PrKey) -> AnyElement {
        let (scroll, nav, key) = (self.scroll.clone(), self.nav.clone(), pr.clone());
        let laid_out = self.laid_out;
        canvas(
            move |bounds, window, cx| {
                let view = scroll.bounds();
                // The scroll view is not laid out yet: the next frame scrolls.
                if view.size.height <= px(0.0) {
                    window.refresh();
                    return;
                }
                let offset = scroll.offset();
                let y = (offset.y - (bounds.top() - view.top()) + px(REVEAL_MARGIN)).min(px(0.0));
                if (y - offset.y).abs() > px(0.5) {
                    scroll.set_offset(point(offset.x, y));
                    window.refresh();
                }
                // Code still being read changes the heights above it: the
                // thread is scrolled to again when it lands.
                if laid_out {
                    nav.update(cx, |nav, _| nav.revealed(&key));
                }
            },
            |_, (), _, _| {},
        )
        .absolute()
        .size_full()
        .into_any_element()
    }

    /// An author's avatar, name and how long ago they wrote.
    fn byline(comment: &Comment, t: &Theme, cx: &App) -> gpui::Div {
        let c = &t.colors;
        h_flex()
            .gap(px(8.0))
            .items_center()
            .child(ui::avatar(
                cx,
                &open_host(cx),
                &comment.author,
                ui::avatar::CARD,
            ))
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(c.text)
                    .child(comment.author.login.clone()),
            )
            .child(div().small(t).text_color(c.text_muted).child(ago(comment)))
    }

    fn comment(&mut self, comment: &Comment, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let body = self.markdown(comment.id.as_str(), &comment.body, t, cx);
        ui::card(cx)
            .flex()
            .flex_col()
            .gap(px(6.0))
            .p(px(10.0))
            .child(Self::byline(comment, t, cx))
            .child(
                div()
                    .w_full()
                    .min_w_0()
                    .pl(px(ui::avatar::CARD + 8.0))
                    .child(body),
            )
            .into_any_element()
    }

    /// A review verdict: a tinted circle with the verdict's icon and a line
    /// saying it; a summary, when there is one, in a card edged in its color.
    fn review(&mut self, comment: &Comment, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let c = &t.colors;
        let Some(verdict) = comment.verdict else {
            return self.comment(comment, t, cx);
        };
        let (icon, color, words) = verdict_style(t, verdict);
        let tint = t.chip_colors(color);
        let body = (!comment.body.trim().is_empty())
            .then(|| self.markdown(comment.id.as_str(), &comment.body, t, cx));
        v_flex()
            .gap(px(6.0))
            .child(
                h_flex()
                    .gap(px(8.0))
                    .items_center()
                    .child(
                        div()
                            .size(px(22.0))
                            .flex_none()
                            .rounded_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(tint.fill)
                            .child(Icon::new(icon).size(px(13.0)).text_color(color)),
                    )
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(c.text)
                            .child(comment.author.login.clone()),
                    )
                    .child(
                        div()
                            .small(t)
                            .text_color(c.text_muted)
                            .child(format!("{words} {}", ago(comment))),
                    ),
            )
            .children(body.map(|body| {
                div()
                    .min_w_0()
                    .ml(px(30.0))
                    .p(px(10.0))
                    .rounded(t.radius.lg)
                    .border_1()
                    .border_color(tint.border)
                    .child(body)
            }))
            .into_any_element()
    }

    /// An inline thread as one row: where it is, who started it, how it
    /// stands. Clicking it shows the thread in the diff.
    fn inline_row(
        &mut self,
        thread: &Thread,
        pr: &PrKey,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = &t.colors;
        let hover = c.hover;
        let (highlighted, marker) = self.reveal(thread, pr, cx);
        let menu = ThreadMenu::new(pr.clone(), thread.id.clone(), Origin::Conversation);
        let id = thread.id.clone();
        let first = thread.comments.first().map(|c| &c.author);
        let author = first.map(|a| a.login.clone()).unwrap_or_default();
        let avatar = first.map(|a| ui::avatar(cx, &open_host(cx), a, ui::avatar::ROW));
        let resolved = thread.is_resolved();
        let selector = format!("inline-row-{}", thread.id);
        h_flex()
            .id(gpui::ElementId::Name(
                format!("inline-{}", thread.id).into(),
            ))
            .debug_selector(move || selector)
            .relative()
            .w_full()
            // As the unfolded thread's card and header, so the chevron stays
            // put; the border adds a pixel above and under.
            .h(px(INLINE_HEAD + 2.0 * INLINE_HEAD_PAD + 2.0))
            .px(px(10.0))
            .gap(px(8.0))
            .items_center()
            .rounded(t.radius.lg)
            .border_1()
            .border_color(if highlighted { c.focus } else { c.border })
            .cursor_pointer()
            .hover(move |s| s.bg(hover))
            .on_click(cx.listener(move |this, _, _, cx| {
                let id = id.clone();
                this.session
                    .update(cx, |session, cx| session.jump_to_thread(id, true, cx));
            }))
            .child(self.fold_toggle(thread, pr, true, t))
            .child(
                Icon::new(IconName::MessageSquare)
                    .size(px(13.0))
                    .text_color(threads::rail_color(t, thread)),
            )
            .child(
                crate::sidebar::middle_path(t, threads::location(thread))
                    .max_w(gpui::relative(INLINE_PATH_SHARE)),
            )
            .children(avatar)
            .child(
                div()
                    .flex_none()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(if resolved { c.text_muted } else { c.text })
                    .child(author),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(c.text_muted)
                    .child(threads::preview(thread)),
            )
            .when(thread.outdated, |el| {
                el.child(ui::chip(cx, "outdated", c.warn))
            })
            .child(
                div()
                    .flex_none()
                    .small(t)
                    .text_color(c.text_muted)
                    .child(threads::summary(thread)),
            )
            .children(marker)
            .context_menu(move |popup, _, cx| menu.popup(popup, cx))
            .into_any_element()
    }

    /// The chevron that folds an inline thread to one row, or unfolds it.
    fn fold_toggle(&self, thread: &Thread, pr: &PrKey, folded: bool, t: &Theme) -> AnyElement {
        let (nav, key, id) = (self.nav.clone(), pr.clone(), thread.id.clone());
        let selector = format!("inline-fold-{}", thread.id);
        let hover = t.colors.hover;
        div()
            .id(SharedString::from(selector.clone()))
            .debug_selector(move || selector)
            .flex_none()
            .rounded(t.radius.sm)
            .cursor_pointer()
            .hover(move |s| s.bg(hover))
            .on_click(move |_, _, cx| {
                cx.stop_propagation();
                nav.update(cx, |nav, cx| nav.set_folded(&key, &id, !folded, cx));
            })
            .child(threads::chevron(folded, t))
            .into_any_element()
    }

    /// An inline thread in full: where it is, the code it is on, and its
    /// comments with suggestions drawn as suggestions.
    fn inline_thread(
        &mut self,
        thread: &Thread,
        pr: &PrKey,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = &t.colors;
        let id = thread.id.clone();
        let tag = thread.id.to_string();
        let (highlighted, marker) = self.reveal(thread, pr, cx);
        let menu = ThreadMenu::new(pr.clone(), thread.id.clone(), Origin::Conversation);
        let forge = self.session.read(cx).pr(pr).map(|open| open.pr.forge);
        let jump = {
            let (session, id) = (self.session.clone(), id.clone());
            move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| {
                let id = id.clone();
                session.update(cx, |session, cx| session.jump_to_thread(id, true, cx));
            }
        };
        let hover = c.hover;
        let path = thread
            .anchor
            .as_ref()
            .map(|anchor| anchor.path.clone())
            .unwrap_or_default();
        let head_menu = menu.clone();
        let head = h_flex()
            .id(SharedString::from(format!("inline-head-{tag}")))
            .debug_selector({
                let tag = tag.clone();
                move || format!("inline-head-{tag}")
            })
            .h(px(INLINE_HEAD))
            .flex_none()
            .gap(px(8.0))
            .items_center()
            .child(self.fold_toggle(thread, pr, false, t))
            .child(
                Icon::new(IconName::MessageSquare)
                    .size(px(13.0))
                    .text_color(threads::rail_color(t, thread)),
            )
            .child(
                div()
                    .id(SharedString::from(format!("inline-path-{tag}")))
                    .flex_shrink(1.0)
                    .min_w_0()
                    .cursor_pointer()
                    .hover(|s| s.underline())
                    .on_click(jump.clone())
                    .child(crate::sidebar::middle_path(t, path)),
            )
            .children(threads::line_label(thread).map(|label| {
                div()
                    .flex_none()
                    .small(t)
                    .text_color(c.text_muted)
                    .child(label)
            }))
            .when(thread.outdated, |el| {
                el.child(ui::chip(cx, "outdated", c.warn))
            })
            .when(thread.is_resolved(), |el| {
                el.child(ui::chip(cx, "resolved", c.ok))
            })
            .child(div().flex_1())
            .child(
                div()
                    .id(SharedString::from(format!("jump-{tag}")))
                    .debug_selector({
                        let tag = tag.clone();
                        move || format!("jump-{tag}")
                    })
                    .flex_none()
                    .px(px(6.0))
                    .rounded(t.radius.sm)
                    .small(t)
                    .text_color(c.link)
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover).underline())
                    .on_click(jump)
                    .child("Jump to line"),
            )
            .context_menu(move |popup, _, cx| head_menu.popup(popup, cx));
        let excerpt = self.excerpt(&thread.id);
        let code = match &excerpt {
            Some(Some(excerpt)) => {
                let caption = (thread.outdated && excerpt.original).then(|| {
                    div()
                        .tiny(t)
                        .text_color(c.text_muted)
                        .child("As it was when the thread started")
                });
                v_flex()
                    .debug_selector({
                        let tag = tag.clone();
                        move || format!("inline-code-{tag}")
                    })
                    .gap(px(4.0))
                    .children(caption)
                    .child(thread_code::render(excerpt, t))
                    .into_any_element()
            }
            Some(None) => {
                thread_code::missing(t, "The code is not in the local objects").into_any_element()
            }
            None => thread_code::missing(t, "Reading the code").into_any_element(),
        };
        let marked: Vec<SharedString> = match &excerpt {
            Some(Some(excerpt)) if !thread.outdated => excerpt
                .lines
                .iter()
                .filter(|line| line.marked)
                .map(|line| line.text.clone())
                .collect(),
            _ => Vec::new(),
        };
        let comments: Vec<AnyElement> = thread
            .comments
            .iter()
            .enumerate()
            .map(|(k, comment)| {
                let parts = self.comment_parts(comment, &marked, t, cx);
                let apply = threads::suggestion_actions(pr, thread, comment, t, cx);
                let comment_menu = CommentMenu {
                    link: forge
                        .and_then(|forge| comment_url(forge, &pr.repo, pr.number, comment, true)),
                    text: comment.body.clone(),
                    thread: thread.clone(),
                    writing: self.writing.clone(),
                    reply: false,
                    resolve: false,
                };
                let menu = menu.clone();
                let selector = format!("inline-comment-{}", comment.id);
                v_flex()
                    .id(SharedString::from(format!("inline-comment-{}", comment.id)))
                    .debug_selector(move || selector)
                    .gap(px(4.0))
                    .pt(px(8.0))
                    .when(k > 0, |el| el.border_t_1().border_color(c.border))
                    .child(Self::byline(comment, t, cx))
                    .child(
                        v_flex()
                            .w_full()
                            .min_w_0()
                            .gap(px(6.0))
                            .pl(px(ui::avatar::CARD + 8.0))
                            .children(parts)
                            .children(apply),
                    )
                    .context_menu(move |popup, _, cx| {
                        thread_menu::with_comment(&menu, &comment_menu, popup, cx)
                    })
                    .into_any_element()
            })
            .collect();
        let selector = format!("inline-thread-{tag}");
        ui::card(cx)
            .debug_selector(move || selector)
            .relative()
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .px(px(10.0))
            .pt(px(INLINE_HEAD_PAD))
            .pb(px(10.0))
            .when(highlighted, |el| el.border_color(c.focus))
            .children(marker)
            .child(head)
            .child(code)
            .children(comments)
            .child(
                div()
                    .small(t)
                    .text_color(c.text_muted)
                    .child(threads::summary(thread)),
            )
            .into_any_element()
    }

    /// A comment's body: markdown pieces, and each suggestion block as the
    /// change it proposes over the thread's `marked` lines.
    fn comment_parts(
        &mut self,
        comment: &Comment,
        marked: &[SharedString],
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        if !suggestions::mentions(&comment.body) {
            return vec![self.markdown(comment.id.as_str(), &comment.body, t, cx)];
        }
        suggestions::split_body(&comment.body)
            .into_iter()
            .enumerate()
            .map(|(k, part)| match part {
                BodyPart::Markdown(text) => {
                    self.markdown(&format!("{}#{k}", comment.id), text, t, cx)
                }
                BodyPart::Suggestion(lines) => suggestion_block(comment, marked, &lines, t),
            })
            .collect()
    }
}

impl Panel for ConversationPanel {
    fn title(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.binding.title(None, cx)
    }

    fn inner_padding(&self, _: &App) -> bool {
        false
    }
}

impl Render for ConversationPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.spinners.clear();
        let t = ui::theme(cx);
        let c = &t.colors;
        let session = self.session.read(cx);
        let Some(open) = self.binding.open(session) else {
            return v_flex()
                .size_full()
                .bg(c.surface)
                .child(ui::empty_note(cx, "No pull request open"));
        };
        let pr = open.pr.clone();
        let key = open.key.clone();
        let threads: Arc<[Thread]> = open.threads.clone();
        let diff = open.diff.as_ref().map(|d| (d.diff.clone(), d.generation));
        let meta = pr_header::meta_row(open, session.account(&key.repo.host), cx);
        let ci_summary = ci_box::render(open, &self.boxes, &self.session, &self.spinners, cx);
        let preview = session.is_preview(&key);
        let refs = crate::references::context_for(session, &key);
        self.sync_writing(&key, cx);
        self.actions
            .update(cx, |actions, cx| actions.bind(&key, window, cx));
        self.side.update(cx, |side, cx| side.bind(&key, cx));
        let (sent, queued) = {
            let writing = self.writing.read(cx);
            (writing.sent().to_vec(), writing.queued().to_vec())
        };
        let composer = self.writing.update(cx, |writing, cx| {
            writing.composer(&DraftAnchor::Conversation, None, window, cx)
        });
        let facts = Facts::read(self.session.read(cx), &key, Some(self.writing.read(cx)));
        let confirm_here = self.sync_footer(facts.as_ref().and_then(|f| f.confirm.as_ref()));
        self.actions.update(cx, |actions, cx| {
            actions.set_confirm_elsewhere(confirm_here, cx);
        });
        let me = cx.weak_entity();
        let close = footer::close_button(facts.as_ref(), &me);
        composer.update(cx, |composer, cx| composer.set_leading(close, cx));
        if self.bodies_for.as_ref() != Some(&key) {
            self.bodies.clear();
            self.bodies_for = Some(key.clone());
        }
        self.bodies.set_refs(refs);
        let style = Self::thread_style(cx);
        let any_expanded = {
            let nav = self.nav.read(cx);
            threads
                .iter()
                .any(|t| t.anchor.is_some() && !nav.is_folded(&key, &t.id, t.is_resolved(), style))
        };
        let has_diff = diff.is_some();
        if any_expanded {
            self.ensure_code(&key, diff, &threads, cx);
        }
        self.laid_out = !(any_expanded && has_diff) || self.code.ready;
        let reference = pr.ref_label();
        let keep = preview.then(|| keep_button(&self.session, &key, cx));
        let title_block = v_flex()
            .gap(px(8.0))
            .px(px(16.0))
            .pt(px(12.0))
            .pb(px(12.0))
            .border_b_1()
            .border_color(c.border)
            .child(meta)
            .child(
                div()
                    .display(&t)
                    .text_size(px(18.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(c.text)
                    .child(pr.title.clone()),
            )
            .child(self.actions.clone())
            .child(ci_summary);

        let description = if pr.description.trim().is_empty() {
            div()
                .text_color(c.text_muted)
                .child("No description provided.")
                .into_any_element()
        } else {
            self.markdown("description", &pr.description, &t, cx)
        };
        let entries = timeline(&threads);
        let inline = entries
            .iter()
            .filter(|e| matches!(e, Entry::Inline(_)))
            .count();
        let rendered: Vec<AnyElement> = entries
            .iter()
            .map(|entry| self.entry(entry, style, &key, &t, cx))
            .collect();
        let (merge_box, comment_box) = {
            let session = self.session.read(cx);
            let Some(open) = self.binding.open(session) else {
                return v_flex();
            };
            let footer = Footer {
                open,
                facts: facts.as_ref(),
                state: &self.footer,
                confirm_here,
                panel: me,
                session: &self.session,
                spinners: &self.spinners,
            };
            (
                footer.merge_box(&t, cx),
                footer.comment_box(&composer, &t, cx),
            )
        };
        let style_toggle = ui::segmented(
            cx,
            "thread-style",
            vec![
                (ThreadStyle::Expanded, "Expanded".into()),
                (ThreadStyle::Compact, "Compact".into()),
            ],
            move |option| *option == style,
            |option, _, cx| Self::set_thread_style(*option, cx),
        );
        let timeline_header = h_flex()
            .pt(px(4.0))
            .gap(px(8.0))
            .items_center()
            .small(&t)
            .text_color(c.text_muted)
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(c.text_secondary)
                    .child("Activity"),
            )
            .child(div().flex_1().min_w_0().truncate().child(format!(
                "{} entries, {inline} inline thread{}",
                entries.len(),
                if inline == 1 { "" } else { "s" }
            )))
            .child(style_toggle);

        let body = v_flex()
            .flex_1()
            .w_full()
            .min_w_0()
            .gap(px(12.0))
            .p(px(16.0))
            .text_color(c.text)
            .child(
                div()
                    .debug_selector(|| "pr-description".into())
                    .w_full()
                    .min_w_0()
                    .child(description),
            )
            .child(ui::divider(cx))
            .child(timeline_header)
            .children(rendered)
            .children(sent.iter().map(|comment| local_card(cx, comment, None)))
            .children(queued.iter().map(|comment| {
                let local = LocalComment {
                    body: comment.body.clone().into(),
                    kind: CommentKind::Queued,
                };
                local_card(cx, &local, anchor_label(&comment.anchor))
            }))
            .child(div().pt(px(8.0)).child(merge_box))
            .child(comment_box);

        let wide = self.width.get() >= px(WIDE);
        self.wide.set(wide);
        let content = if wide {
            h_flex()
                .w_full()
                .items_start()
                .child(body)
                .child(
                    div()
                        .w(px(side_column::WIDTH))
                        .flex_none()
                        .pt(px(16.0))
                        .pr(px(16.0))
                        .child(self.side.clone()),
                )
                .into_any_element()
        } else {
            v_flex()
                .w_full()
                .child(
                    div()
                        .px(px(16.0))
                        .pt(px(12.0))
                        .pb(px(4.0))
                        .border_b_1()
                        .border_color(c.border)
                        .child(self.side.clone()),
                )
                .child(body)
                .into_any_element()
        };
        // A width that crosses the line is drawn again in the other layout.
        let (width, laid_wide, me) = (self.width.clone(), self.wide.clone(), cx.entity_id());
        let measure = canvas(
            move |bounds, _, cx| {
                width.set(bounds.size.width);
                if (bounds.size.width >= px(WIDE)) != laid_wide.get() {
                    cx.notify(me);
                }
            },
            |_, (), _, _| {},
        )
        .absolute()
        .size_full();

        v_flex()
            .size_full()
            .relative()
            .bg(c.surface)
            .capture_any_mouse_down(cx.listener(|this, _, _, cx| this.binding.activated(cx)))
            .child(measure)
            .child(header(
                cx,
                h_flex()
                    .gap(px(8.0))
                    .items_center()
                    .child(div().mono(&t).child(reference))
                    .children(keep),
                h_flex()
                    .gap(px(6.0))
                    .items_center()
                    .child(ci_icon(&t, pr.ci, CI_HEADER, &self.spinners))
                    .child(match pr.ci {
                        CiState::None => div().child(ci_label(pr.ci)),
                        state => ui::chip(cx, ci_label(state), ci_color(&t, state)),
                    }),
            ))
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        v_flex()
                            .id("conversation-scroll")
                            .debug_selector(|| "conversation-body".into())
                            .size_full()
                            .track_scroll(&self.scroll)
                            .overflow_y_scroll()
                            .child(title_block)
                            .child(content)
                            .vertical_scrollbar(&self.scroll),
                    )
                    .child(self.autoscroll.overlay(ui::scroll_style(&t).autoscroll)),
            )
    }
}

/// A suggestion block as the change it proposes: the thread's lines it
/// replaces, when known, then its own lines.
fn suggestion_block(
    comment: &Comment,
    marked: &[SharedString],
    lines: &[String],
    t: &Theme,
) -> AnyElement {
    let c = &t.colors;
    let removed = marked.iter().flat_map(|line| {
        crate::review_bar::line_rows("-", line, c.diff_remove_bg, c.diff_remove_gutter, t)
    });
    let added = lines.iter().flat_map(|line| {
        crate::review_bar::line_rows("+", line, c.diff_add_bg, c.diff_add_gutter, t)
    });
    let selector = format!("suggestion-{}", comment.id);
    v_flex()
        .debug_selector(move || selector)
        .w_full()
        .min_w_0()
        .overflow_hidden()
        .rounded(t.radius.md)
        .border_1()
        .border_color(c.border)
        .child(
            div()
                .px(px(8.0))
                .py(px(3.0))
                .border_b_1()
                .border_color(c.border)
                .bg(c.surface_raised)
                .tiny(t)
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(c.text_secondary)
                .child("Suggested change"),
        )
        .child(
            v_flex()
                .font_family(t.fonts.buffer.family())
                .text_size(t.fonts.buffer.size_px())
                .children(removed)
                .children(added),
        )
        .into_any_element()
}

/// The pin on a preview tab's header: keeps the tab open.
pub(crate) fn keep_button(session: &Entity<Session>, key: &PrKey, cx: &App) -> AnyElement {
    let (session, key) = (session.clone(), key.clone());
    ui::icon_button(cx, "keep-tab", IconName::Pin, move |_, _, cx| {
        session.update(cx, |session, cx| session.keep(&key, cx));
    })
    .debug_selector(|| "keep-tab".into())
    .into_any_element()
}

/// A comment written in this session, not sent anywhere yet.
fn local_card(cx: &App, comment: &LocalComment, place: Option<SharedString>) -> AnyElement {
    let t = ui::theme(cx);
    let c = &t.colors;
    let label = match comment.kind {
        CommentKind::Posted => div().child("you commented"),
        CommentKind::Queued => h_flex()
            .gap(px(8.0))
            .items_center()
            .child(ui::chip(cx, "pending in your review", c.warn))
            .children(place),
    };
    ui::card(cx)
        .flex()
        .flex_col()
        .gap(px(6.0))
        .p(px(10.0))
        .child(label.small(&t).text_color(c.text_secondary))
        .child(
            div()
                .mono(&t)
                .small(&t)
                .text_color(c.text)
                .whitespace_normal()
                .child(comment.body.clone()),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge::model::{Anchor, CommentId, Resolution, ReviewState, ThreadId, Timestamp, User};

    fn at(minutes: i64) -> Timestamp {
        chrono::DateTime::from_timestamp(1_800_000_000 + minutes * 60, 0).expect("valid time")
    }

    fn comment(id: &str, minutes: i64, verdict: Option<ReviewState>, body: &str) -> Comment {
        Comment {
            id: CommentId::new(id),
            author: User::new("mira"),
            body: body.into(),
            created_at: at(minutes),
            edited_at: None,
            web_url: None,
            review: None,
            verdict,
            suggestions: Vec::new(),
        }
    }

    fn thread(id: &str, anchored: bool, comments: Vec<Comment>) -> Thread {
        Thread {
            id: ThreadId::new(id),
            anchor: anchored.then(|| Anchor {
                path: "a.rs".into(),
                old_path: None,
                position: None,
                original: None,
            }),
            resolution: Resolution::NotResolvable,
            outdated: false,
            comments,
        }
    }

    #[test]
    fn the_timeline_is_in_time_order_with_verdicts_and_inline_rows() {
        let threads = [
            thread(
                "review",
                false,
                vec![comment("r", 30, Some(ReviewState::Approved), "")],
            ),
            thread(
                "inline",
                true,
                vec![comment("i", 10, None, "x"), comment("j", 50, None, "y")],
            ),
            thread("talk", false, vec![comment("t", 20, None, "hello")]),
            thread("blank", false, vec![comment("b", 5, None, "  ")]),
        ];
        let kinds: Vec<&str> = timeline(&threads)
            .iter()
            .map(|entry| match entry {
                Entry::Comment(c) | Entry::Review(c) => c.id.as_str(),
                Entry::Inline(t) => t.id.as_str(),
            })
            .collect();
        assert_eq!(kinds, ["inline", "t", "r"]);
    }
}
