//! The header of a pull request's panels: where it lives (forge host,
//! repository, number, branches, author) and the actions on the pull
//! request as a whole: submitting a review with a verdict, merging, closing
//! and draft state. The same actions, with the reason a blocked one cannot
//! run, are the command palette's commands on the open pull request.
//!
//! The Conversation panel shows the full bar; the Diff panel shows the
//! actions as one compact row.

use std::rc::Rc;

use forge::Capabilities;
use forge::model::{ForgeKind, MergeMethod, PrState, ReviewState, Sha, User};
use gpui::{
    Anchor, AnyElement, App, AppContext as _, ClipboardItem, Context, Entity, FontWeight,
    InteractiveElement as _, IntoElement, ParentElement, Render, SharedString,
    StatefulInteractiveElement as _, Styled, Subscription, Window, anchored, deferred, div,
    prelude::FluentBuilder as _, px,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::{Icon, h_flex, v_flex};
use theme::Theme;

use crate::lifecycle::{Action, Load, blocker_label, method_label, supported};
use crate::model::PrKey;
use crate::palette::Item;
use crate::panels::PanelKind;
use crate::references;
use crate::session::{OpenMode, OpenPr, Session};
use crate::sink::Verdict;
use crate::terminals::{Purpose, Terminals, WorktreeState};
use crate::ui::{self, Button, ButtonKind, TextExt as _};
use crate::writing::Writing;
use gpui_kit::TestSupportExt as _;

/// Why `verdict` cannot be sent on this pull request, if it cannot.
#[must_use]
pub fn verdict_blocked(
    verdict: Verdict,
    caps: &Capabilities,
    own: bool,
    forge: ForgeKind,
) -> Option<String> {
    match verdict {
        Verdict::Comment => None,
        Verdict::RequestChanges if !caps.request_changes => {
            Some(format!("{} has no request for changes", forge.label()))
        }
        Verdict::Approve | Verdict::RequestChanges if own && !caps.own_pull_request_verdicts => {
            let what = if verdict == Verdict::Approve {
                "approve"
            } else {
                "request changes on"
            };
            Some(format!(
                "{} does not let you {what} your own pull request",
                forge.label()
            ))
        }
        Verdict::Approve | Verdict::RequestChanges => None,
    }
}

/// Whether the signed-in account opened `open`.
#[must_use]
pub fn is_own(open: &OpenPr, account: Option<&User>) -> bool {
    account.is_some_and(|me| me.login.eq_ignore_ascii_case(&open.pr.author))
}

/// The viewer's latest review on `open`: the forge's reviewer row, else the
/// newest verdict they left in the threads.
#[must_use]
pub fn my_review(open: &OpenPr, account: Option<&User>) -> Option<ReviewState> {
    let me = account?;
    let row = open
        .forge_pr()
        .and_then(|pr| pr.reviewers.iter().find(|r| r.user.login == me.login))
        .map(|r| r.state)
        .filter(|state| !matches!(state, ReviewState::Requested | ReviewState::Pending));
    row.or_else(|| {
        open.threads
            .iter()
            .flat_map(|thread| &thread.comments)
            .filter(|comment| comment.author.login == me.login)
            .filter_map(|comment| Some((comment.created_at, comment.verdict?)))
            .max_by_key(|(at, _)| *at)
            .map(|(_, verdict)| verdict)
    })
}

fn review_words(state: Option<ReviewState>) -> &'static str {
    match state {
        Some(ReviewState::Approved) => "you approved",
        Some(ReviewState::ChangesRequested) => "you requested changes",
        Some(ReviewState::Commented) => "you commented",
        Some(ReviewState::Dismissed) => "your review was dismissed",
        Some(ReviewState::Requested | ReviewState::Pending) | None => "not reviewed by you",
    }
}

fn state_style(t: &Theme, state: PrState) -> (&'static str, gpui::Hsla) {
    let c = &t.colors;
    match state {
        PrState::Open => ("Open", c.ok),
        PrState::Draft => ("Draft", c.text_muted),
        PrState::Merged => ("Merged", c.accent_fg),
        PrState::Closed => ("Closed", c.danger),
    }
}

/// `host  owner/repo  #n  head -> base  author`, the state chip and the
/// viewer's review, above the title.
pub fn meta_row(open: &OpenPr, account: Option<&User>, cx: &App) -> AnyElement {
    let t = ui::theme(cx);
    let c = &t.colors;
    let key = &open.key;
    let pr = &open.pr;
    let (state, color) = state_style(&t, pr.state);
    let sep = || div().text_color(c.text_faint).child("\u{00B7}");
    let head_repo = open
        .forge_pr()
        .and_then(|source| source.head.repo.clone())
        .unwrap_or_else(|| key.repo.clone());
    let number_url = open
        .forge_pr()
        .map(|source| source.web_url.clone())
        .filter(|url| !url.is_empty())
        .unwrap_or_else(|| references::pr_url(pr.forge, &key.repo, key.number));
    let branch = |id: &'static str, repo: &forge::model::RepoRef, name: &SharedString| {
        ui::web_link(
            id,
            div().mono(&t).child(name.clone()),
            references::branch_url(pr.forge, repo, name),
            cx,
        )
    };
    let branches = (!pr.head.is_empty()).then(|| {
        h_flex()
            .gap(px(4.0))
            .items_center()
            .child(branch("meta-head", &head_repo, &pr.head))
            .child(div().child("\u{2192}"))
            .child(branch("meta-base", &key.repo, &pr.base))
    });
    h_flex()
        .debug_selector(|| "pr-meta".into())
        .gap(px(8.0))
        .items_center()
        .flex_wrap()
        .small(&t)
        .text_color(c.text_muted)
        .child(ui::chip(cx, state, color))
        .child(div().child(key.repo.host.clone()))
        .child(sep())
        .child(ui::web_link(
            "meta-repo",
            div()
                .mono(&t)
                .text_color(c.text_secondary)
                .child(format!("{}/{}", key.repo.owner, key.repo.name)),
            references::repo_url(&key.repo),
            cx,
        ))
        .child(ui::web_link(
            "meta-number",
            div()
                .mono(&t)
                .text_color(c.text)
                .child(format!("{}{}", pr.forge.sigil(), key.number)),
            number_url,
            cx,
        ))
        .when_some(branches, |el, branches| el.child(sep()).child(branches))
        .when(!pr.author.is_empty(), |el| {
            el.child(sep())
                .child(ui::avatar(
                    cx,
                    &key.repo.host,
                    ui::Who::new(&pr.author, pr.author_avatar.as_deref()),
                    ui::avatar::ROW,
                ))
                .child(ui::web_link(
                    "meta-author",
                    div().text_color(c.text_secondary).child(pr.author.clone()),
                    references::user_url(&key.repo.host, &pr.author),
                    cx,
                ))
        })
        .child(sep())
        .child(
            div()
                .debug_selector(|| "my-review".into())
                .child(review_words(my_review(open, account))),
        )
        .into_any_element()
}

/// Which popover of the action bar is open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Menu {
    Review,
    Merge,
    More,
    /// "Review with agent": Claude's pending comments, the agent's terminal
    /// and the workspace settings.
    Agent,
}

impl Menu {
    /// `review`, `merge`, `more` or `agent`, as `--demo-menu` names them.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "review" => Some(Self::Review),
            "merge" => Some(Self::Merge),
            "more" => Some(Self::More),
            "agent" => Some(Self::Agent),
            _ => None,
        }
    }
}

/// A menu the Conversation's action bar starts with open, for screenshots.
#[derive(Clone, Copy, Default)]
pub struct DemoMenu(pub Option<Menu>);

impl gpui::Global for DemoMenu {}

/// The actions on a pull request: the review button and its popover, the
/// merge split button and the "..." menu, with a confirmation for merges and
/// closes.
pub struct PrActions {
    session: Entity<Session>,
    pr: Option<PrKey>,
    writing: Option<Entity<Writing>>,
    menu: Option<Menu>,
    summary: Entity<TextareaState>,
    /// The summary version the box shows.
    epoch: u64,
    /// One row for the Diff header; the Conversation shows the full form.
    compact: bool,
    /// The confirmation shows elsewhere (the Conversation's merge box asked
    /// for it), so the bar does not draw a second one.
    hide_confirm: bool,
    _subscriptions: Vec<Subscription>,
    /// Redraws when the worktree of a pull request changes.
    _terminals: Option<Subscription>,
}

impl PrActions {
    pub fn new(compact: bool, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let session = Session::global(cx);
        let summary = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(2, 6)
                .placeholder("Leave a comment")
        });
        let typed = cx.subscribe(
            &summary,
            |this: &mut Self, state, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change)
                    && let Some(writing) = &this.writing
                {
                    let text = state.read(cx).value().to_string();
                    writing.update(cx, |writing, _| writing.set_review_body(text));
                }
            },
        );
        let observed = cx.observe(&session, |_: &mut Self, _, cx| cx.notify());
        let menu = cx.try_global::<DemoMenu>().and_then(|demo| demo.0);
        Self {
            session,
            pr: None,
            writing: None,
            menu,
            summary,
            epoch: u64::MAX,
            compact,
            hide_confirm: false,
            _subscriptions: vec![typed, observed],
            _terminals: Terminals::global(cx)
                .map(|terminals| cx.observe(&terminals, |_, _, cx| cx.notify())),
        }
    }

    /// Acts on `pr` from now on.
    pub fn bind(&mut self, pr: &PrKey, window: &mut Window, cx: &mut Context<Self>) {
        let writing = self.session.read(cx).writing_for(pr);
        if self.pr.as_ref() == Some(pr) && writing == self.writing {
            return;
        }
        if self.pr.is_some() {
            self.menu = None;
        }
        self.pr = Some(pr.clone());
        self.epoch = u64::MAX;
        self._subscriptions.truncate(2);
        if let Some(writing) = &writing {
            self._subscriptions.push(cx.observe_in(
                writing,
                window,
                |this: &mut Self, _, window, cx| {
                    this.sync_summary(window, cx);
                },
            ));
        }
        self.writing = writing;
        self.sync_summary(window, cx);
    }

    fn sync_summary(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(writing) = &self.writing else {
            return;
        };
        let (epoch, body) = {
            let writing = writing.read(cx);
            (writing.review_epoch(), writing.review_body().to_owned())
        };
        if epoch != self.epoch {
            self.epoch = epoch;
            self.summary
                .update(cx, |state, cx| state.set_value(body, window, cx));
        }
        cx.notify();
    }

    /// The open popover, for tests.
    pub fn menu(&self) -> Option<Menu> {
        self.menu
    }

    /// Whether the bar leaves the pending confirmation to another place
    /// that draws it.
    pub fn set_confirm_elsewhere(&mut self, elsewhere: bool, cx: &mut Context<Self>) {
        if self.hide_confirm != elsewhere {
            self.hide_confirm = elsewhere;
            cx.notify();
        }
    }

    fn toggle(&mut self, menu: Menu, cx: &mut Context<Self>) {
        self.menu = if self.menu == Some(menu) {
            None
        } else {
            Some(menu)
        };
        cx.notify();
    }

    /// Runs `f` on the session with this bar's pull request focused.
    fn act(&self, cx: &mut App, f: impl FnOnce(&mut Session, &PrKey, &mut Context<Session>)) {
        let Some(key) = self.pr.clone() else {
            return;
        };
        self.session.update(cx, |session, cx| {
            session.focus(&key, cx);
            f(session, &key, cx);
        });
    }

    fn submit(&mut self, verdict: Verdict, cx: &mut Context<Self>) {
        let Some(writing) = self.writing.clone() else {
            return;
        };
        if let Some(key) = &self.pr {
            self.session.update(cx, |session, cx| session.keep(key, cx));
        }
        writing.update(cx, |writing, cx| {
            writing.set_verdict(verdict, cx);
            writing.submit_review(cx);
        });
        self.menu = None;
        cx.notify();
    }

    /// "Open on": the machine picker, then the pull request's worktree
    /// there and the machine's command in it.
    fn open_on(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(key), Some(terminals)) = (self.pr.clone(), Terminals::global(cx)) else {
            return;
        };
        self.menu = None;
        terminals.update(cx, |terminals, cx| {
            terminals.open_picker(Purpose::OpenOn(key), window, cx);
        });
    }

    /// "Review with agent": the agent's terminal, started where the
    /// repository's workspace says, or brought forward while it runs.
    fn review_with_agent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(key) = self.pr.clone() else {
            return;
        };
        self.menu = None;
        crate::claude_review::start(&key, window, cx);
        cx.notify();
    }

    /// "Workspace settings…": the settings page on its Workspaces section.
    fn workspace_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        crate::claude_review::open_workspace_settings(None, window, cx);
        cx.notify();
    }

    /// "Review with agent" as a split button: the main part starts or
    /// focuses the agent, the chevron lists Claude's pending comments, whose
    /// count is on the button.
    fn agent_split(&self, pending: usize, cx: &mut Context<Self>) -> AnyElement {
        let t = ui::theme(cx);
        let c = &t.colors;
        let hover = c.hover;
        let open = self.menu == Some(Menu::Agent);
        let inner = px(ui::CONTROL_HEIGHT - 2.0);
        let badge = (pending > 0).then(|| {
            div()
                .debug_selector(|| "agent-pending-count".into())
                .px(px(5.0))
                .rounded(t.radius.sm)
                .bg(c.selected)
                .tiny(&t)
                .text_color(c.text)
                .child(pending.to_string())
        });
        let main = h_flex()
            .id("review-with-agent")
            .h_full()
            .px(px(8.0))
            .gap(px(6.0))
            .items_center()
            .whitespace_nowrap()
            .cursor_pointer()
            .hover(move |s| s.bg(hover))
            .on_click(cx.listener(|this, _, window, cx| this.review_with_agent(window, cx)))
            .child("Review with agent")
            .children(badge)
            .test_support();
        let chevron = div()
            .id("review-with-agent-menu")
            .debug_selector(|| "review-with-agent-menu".into())
            .size(inner)
            .flex()
            .items_center()
            .justify_center()
            .when(open, |el| el.bg(c.selected))
            .cursor_pointer()
            .hover(move |s| s.bg(hover))
            .on_click(cx.listener(|this, _, _, cx| this.toggle(Menu::Agent, cx)))
            .child(
                Icon::new(IconName::ChevronDown)
                    .size(px(12.0))
                    .text_color(c.text_secondary),
            )
            .test_support();
        ui::secondary_style(h_flex().id("review-with-agent-unit"), cx)
            .overflow_hidden()
            .small(&t)
            .child(main)
            .child(div().w(px(1.0)).h_full().bg(c.border_strong))
            .child(chevron)
            .into_any_element()
    }

    /// The agent menu: each of Claude's drafts not yet in the review (a
    /// click shows it in the diff, "Add" queues it), "Add all", "Discard
    /// all", the agent's terminal and the workspace settings.
    fn agent_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = ui::theme(cx);
        let c = &t.colors;
        let hover = c.hover;
        let Some(key) = self.pr.clone() else {
            return div().into_any_element();
        };
        let pending = self
            .writing
            .as_ref()
            .map(|writing| crate::claude_review::pending(writing.read(cx)))
            .unwrap_or_default();
        let rows = pending.iter().enumerate().map(|(ix, comment)| {
            let jump_key = key.clone();
            let jump = comment.anchor.clone();
            let add = comment.anchor.clone();
            let writing = self.writing.clone();
            h_flex()
                .gap(px(6.0))
                .items_center()
                .child(
                    v_flex()
                        .id(("agent-pending", ix))
                        .debug_selector(move || format!("agent-pending-{ix}"))
                        .flex_1()
                        .min_w_0()
                        .px(px(6.0))
                        .py(px(4.0))
                        .rounded(t.radius.sm)
                        .cursor_pointer()
                        .hover(move |s| s.bg(hover))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.menu = None;
                            crate::claude_review::jump_to(&jump_key, &jump, cx);
                            cx.notify();
                        }))
                        .child(
                            div()
                                .truncate()
                                .mono(&t)
                                .text_color(c.text)
                                .child(comment.place.clone()),
                        )
                        .child(
                            div()
                                .truncate()
                                .tiny(&t)
                                .text_color(c.text_muted)
                                .child(comment.first_line.clone()),
                        ),
                )
                .child(
                    Button::new(("agent-add", ix), "Add", ButtonKind::Ghost).on_click(
                        move |_, _, cx| {
                            if let Some(writing) = &writing {
                                writing.update(cx, |writing, cx| writing.queue_draft(&add, cx));
                            }
                        },
                    ),
                )
                .into_any_element()
        });
        let empty = pending.is_empty();
        let (add_key, discard_key) = (key.clone(), key.clone());
        let bulk = (!empty).then(|| {
            h_flex()
                .gap(px(6.0))
                .child(
                    Button::new("agent-add-all", "Add all", ButtonKind::Secondary)
                        .on_click(move |_, _, cx| crate::claude_review::add_all(&add_key, cx)),
                )
                .child(
                    Button::new("agent-discard-all", "Discard all", ButtonKind::Danger).on_click(
                        move |_, _, cx| crate::claude_review::discard_all(&discard_key, cx),
                    ),
                )
        });
        let item = |id: &'static str, label: &'static str| {
            div()
                .id(id)
                .debug_selector(move || id.into())
                .px(px(8.0))
                .py(px(5.0))
                .rounded(t.radius.sm)
                .cursor_pointer()
                .hover(move |s| s.bg(hover))
                .text_color(c.text)
                .child(label)
        };
        let title = if empty {
            "No comments from Claude waiting".to_owned()
        } else {
            format!("Comments from Claude ({})", pending.len())
        };
        v_flex()
            .w(px(380.0))
            .gap(px(4.0))
            .child(
                div()
                    .px(px(6.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(c.text)
                    .child(title),
            )
            .child(
                v_flex()
                    .id("agent-pending-list")
                    .max_h(px(320.0))
                    .overflow_y_scroll()
                    .gap(px(2.0))
                    .children(rows),
            )
            .children(bulk)
            .child(ui::divider(cx))
            .child(
                item("agent-open-terminal", "Open agent terminal").on_click(
                    cx.listener(|this, _, window, cx| this.review_with_agent(window, cx)),
                ),
            )
            .child(
                item("agent-workspace-settings", "Workspace settings\u{2026}").on_click(
                    cx.listener(|this, _, window, cx| this.workspace_settings(window, cx)),
                ),
            )
            .into_any_element()
    }

    fn request(&mut self, action: Action, cx: &mut Context<Self>) {
        self.menu = None;
        self.act(cx, |session, key, cx| {
            session.request_action(key, action, cx)
        });
        cx.notify();
    }
}

/// What the bar needs to know about the pull request, read once a render.
pub(crate) struct Facts {
    pub key: PrKey,
    pub forge: ForgeKind,
    pub state: PrState,
    pub caps: Capabilities,
    pub own: bool,
    pub queued: usize,
    pub has_body: bool,
    pub verdict: Verdict,
    pub submitting: bool,
    pub review_error: Option<SharedString>,
    pub method: Option<MergeMethod>,
    pub allowed: Vec<MergeMethod>,
    pub delete_branch: bool,
    pub head_in_base: bool,
    /// The commit a merge asked now lands.
    pub head: Option<Sha>,
    /// Whether it can merge, the blockers in words, and whether auto-merge
    /// is on.
    pub mergeable: Load<(bool, Vec<SharedString>, bool)>,
    pub confirm: Option<Action>,
    pub busy: Option<Action>,
    pub notice: Option<(SharedString, bool)>,
}

impl Facts {
    /// The facts of `key`, open in `session`, with what is written on it.
    pub(crate) fn read(session: &Session, key: &PrKey, writing: Option<&Writing>) -> Option<Self> {
        let open = session.pr(key)?;
        let caps = session.capabilities_for(key)?;
        let account = session.account(&key.repo.host);
        let life = &open.lifecycle;
        let mergeable = match &life.mergeability {
            Load::Ready(m) => Load::Ready((
                m.mergeable == Some(true),
                m.reasons
                    .iter()
                    .map(|reason| SharedString::from(blocker_label(*reason)))
                    .collect(),
                m.auto_merge,
            )),
            Load::Idle => Load::Idle,
            Load::Loading => Load::Loading,
            Load::Failed(error) => Load::Failed(error.clone()),
            Load::Unsupported => Load::Unsupported,
        };
        Some(Self {
            forge: open.pr.forge,
            state: open.pr.state,
            own: is_own(open, account),
            queued: writing.map_or(0, |w| w.queued().len()),
            has_body: writing.is_some_and(|w| !w.review_body().trim().is_empty()),
            verdict: writing.map_or(Verdict::Comment, Writing::verdict),
            submitting: writing.is_some_and(Writing::submitting),
            review_error: writing.and_then(|w| w.review_error().cloned()),
            method: life.chosen_method(),
            allowed: life
                .settings
                .ready()
                .map(|s| s.allowed.clone())
                .unwrap_or_default(),
            delete_branch: life.delete_branch,
            head_in_base: open.forge_pr().is_none_or(|pr| pr.head_in_base_repo()),
            head: open.merge_head(),
            mergeable,
            confirm: life.confirm.clone(),
            busy: life.busy.clone(),
            notice: life
                .notice
                .as_ref()
                .map(|notice| (notice.text.clone(), notice.error)),
            caps,
            key: key.clone(),
        })
    }

    /// Why the review cannot be submitted with `verdict` now.
    pub(crate) fn submit_blocked(&self, verdict: Verdict) -> Option<String> {
        if self.submitting {
            return Some("Submitting".into());
        }
        if let Some(reason) = verdict_blocked(verdict, &self.caps, self.own, self.forge) {
            return Some(reason);
        }
        (verdict == Verdict::Comment && self.queued == 0 && !self.has_body)
            .then(|| "Queue a comment or write a summary first".to_owned())
    }

    /// Why the pull request cannot be merged now; `None` when it can.
    pub(crate) fn merge_blocked(&self) -> Option<String> {
        if !self.state.is_open() {
            return Some(format!(
                "Cannot merge: {}",
                blocker_label(forge::model::MergeBlocker::NotOpen)
            ));
        }
        match &self.mergeable {
            Load::Ready((true, _, _)) => None,
            Load::Ready((false, reasons, _)) if reasons.is_empty() => {
                Some("Cannot merge: the forge refuses it".into())
            }
            Load::Ready((false, reasons, _)) => {
                Some(format!("Cannot merge: {}", reasons.join(", ")))
            }
            Load::Idle | Load::Loading => Some("Checking whether it can merge".into()),
            Load::Failed(error) => Some(format!("Mergeability not read: {error}")),
            Load::Unsupported => Some("This forge cannot merge from here".into()),
        }
    }

    /// The merge the main button asks for: the chosen method at the head on
    /// screen. `None` while either is unknown.
    pub(crate) fn merge_action(&self) -> Option<Action> {
        Some(Action::Merge {
            method: self.method?,
            delete_branch: self.delete_branch && self.head_in_base,
            head: self.head.clone()?,
        })
    }

    /// Why the merge button cannot be pressed now; `None` when it can.
    pub(crate) fn merge_disabled(&self) -> Option<String> {
        if let Some(why) = self.merge_blocked() {
            return Some(why);
        }
        if let Some(busy) = &self.busy {
            return Some(format!("{}...", busy.label()));
        }
        self.merge_action()
            .is_none()
            .then(|| "The merge method or the head is not known yet".to_owned())
    }

    /// The draft, close and reopen actions its state allows.
    pub(crate) fn state_actions(&self) -> Vec<Action> {
        match self.state {
            PrState::Open | PrState::Draft => {
                vec![Action::SetDraft(self.state == PrState::Open), Action::Close]
            }
            PrState::Closed => vec![Action::Reopen],
            PrState::Merged => Vec::new(),
        }
    }

    /// Why `action` cannot run from here, when the forge cannot do it.
    pub(crate) fn unsupported(&self, action: &Action) -> Option<String> {
        (!supported(&self.caps, action))
            .then(|| format!("{} cannot do this from here", self.forge.label()))
    }
}

impl PrActions {
    fn facts(&self, cx: &App) -> Option<Facts> {
        let key = self.pr.as_ref()?;
        let writing = self.writing.as_ref().map(|w| w.read(cx));
        Facts::read(self.session.read(cx), key, writing)
    }

    /// A button that only opens `menu`, marked with a chevron.
    fn menu_button(
        &self,
        id: &'static str,
        label: String,
        menu: Menu,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = ui::theme(cx);
        let c = &t.colors;
        let open = self.menu == Some(menu);
        ui::secondary_style(div().id(id), cx)
            .debug_selector(move || id.into())
            .gap(px(6.0))
            .px(px(8.0))
            .small(&t)
            .whitespace_nowrap()
            .cursor_pointer()
            .when(open, |el| el.bg(c.selected))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.toggle(menu, cx);
                // The review popover is for writing: its comment box takes the keys.
                if this.menu == Some(Menu::Review) {
                    this.summary
                        .update(cx, |summary, cx| summary.focus(window, cx));
                }
            }))
            .child(label)
            .child(
                Icon::new(IconName::ChevronDown)
                    .size(px(12.0))
                    .text_color(c.text_secondary),
            )
            .test_support()
            .into_any_element()
    }

    fn split(
        &self,
        id: &'static str,
        label: String,
        disabled: bool,
        menu: Menu,
        on_main: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = ui::theme(cx);
        let c = &t.colors;
        let hover = c.hover;
        let open = self.menu == Some(menu);
        // Inside the unit's border: the chevron is as wide as it is tall.
        let inner = px(ui::CONTROL_HEIGHT - 2.0);
        let main = div()
            .id(id)
            .h_full()
            .px(px(8.0))
            .flex()
            .items_center()
            .whitespace_nowrap()
            .when(disabled, |el| el.text_color(c.text_faint))
            .when(!disabled, |el| {
                el.cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .on_click(cx.listener(move |this, _, _, cx| on_main(this, cx)))
            })
            .child(label)
            .test_support();
        let chevron = div()
            .id(SharedString::from(format!("{id}-menu")))
            .debug_selector(move || format!("{id}-menu"))
            .size(inner)
            .flex()
            .items_center()
            .justify_center()
            .when(open, |el| el.bg(c.selected))
            .cursor_pointer()
            .hover(move |s| s.bg(hover))
            .on_click(cx.listener(move |this, _, _, cx| this.toggle(menu, cx)))
            .child(
                Icon::new(IconName::ChevronDown)
                    .size(px(12.0))
                    .text_color(c.text_secondary),
            );
        ui::secondary_style(h_flex().id(SharedString::from(format!("{id}-unit"))), cx)
            .overflow_hidden()
            .small(&t)
            .child(main)
            .child(div().w(px(1.0)).h_full().bg(c.border_strong))
            .child(chevron)
            .into_any_element()
    }

    /// One choice in a popover; a blocked one shows why instead of acting.
    fn choice(
        &self,
        id: String,
        text: impl Into<ChoiceText>,
        chosen: bool,
        blocked: Option<String>,
        on_pick: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let ChoiceText { label, detail } = text.into();
        let t = ui::theme(cx);
        let c = &t.colors;
        let hover = c.hover;
        let disabled = blocked.is_some();
        let selector = id.clone();
        v_flex()
            .id(SharedString::from(id))
            .debug_selector(move || selector)
            .px(px(8.0))
            .py(px(5.0))
            .rounded(t.radius.sm)
            .when(chosen, |el| el.bg(c.selected))
            .when(!disabled, |el| {
                el.cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .on_click(cx.listener(move |this, _, _, cx| on_pick(this, cx)))
            })
            .child(
                h_flex()
                    .gap(px(6.0))
                    .items_center()
                    .text_color(if disabled { c.text_faint } else { c.text })
                    .child(
                        Icon::new(if chosen {
                            IconName::CircleCheck
                        } else {
                            IconName::Circle
                        })
                        .size(px(12.0))
                        .text_color(if disabled {
                            c.text_faint
                        } else {
                            c.text_muted
                        }),
                    )
                    .child(label),
            )
            .children(detail.map(|detail| {
                div()
                    .pl(px(18.0))
                    .tiny(&t)
                    .text_color(c.text_muted)
                    .child(detail)
            }))
            .children(blocked.map(|why| div().pl(px(18.0)).tiny(&t).text_color(c.warn).child(why)))
            .into_any_element()
    }

    fn review_menu(&self, facts: &Facts, cx: &mut Context<Self>) -> AnyElement {
        let t = ui::theme(cx);
        let choices: Vec<AnyElement> = Verdict::ALL
            .into_iter()
            .map(|verdict| {
                let blocked = verdict_blocked(verdict, &facts.caps, facts.own, facts.forge);
                let writing = self.writing.clone();
                self.choice(
                    format!("verdict-{}", verdict_id(verdict)),
                    ChoiceText {
                        label: verdict.label().to_owned(),
                        detail: Some(verdict_detail(verdict)),
                    },
                    facts.verdict == verdict,
                    blocked,
                    move |_, cx| {
                        if let Some(writing) = &writing {
                            writing.update(cx, |writing, cx| writing.set_verdict(verdict, cx));
                        }
                    },
                    cx,
                )
            })
            .collect();
        let verdict = facts.verdict;
        let blocked = facts.submit_blocked(verdict);
        let label = match facts.queued {
            0 => "Submit review".to_owned(),
            n => format!("Submit review with {n} comment{}", plural(n)),
        };
        v_flex()
            .w(px(360.0))
            .gap(px(6.0))
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(t.colors.text)
                    .child("Finish your review"),
            )
            .child(
                div()
                    .rounded(t.radius.md)
                    .border_1()
                    .border_color(t.colors.control_border)
                    .bg(t.colors.field)
                    .overflow_hidden()
                    .child(
                        Textarea::new(&self.summary)
                            .appearance(false)
                            .bordered(false)
                            .text_size(t.fonts.interface.size_px())
                            .line_height(t.fonts.interface.line_height_px()),
                    ),
            )
            .children(choices)
            .children(
                blocked
                    .clone()
                    .map(|why| div().tiny(&t).text_color(t.colors.text_muted).child(why)),
            )
            .child(
                h_flex().justify_end().child(
                    Button::new("review-menu-submit", label, ButtonKind::Primary)
                        .disabled(blocked.is_some())
                        .on_click(cx.listener(move |this, _, _, cx| this.submit(verdict, cx))),
                ),
            )
            .into_any_element()
    }

    fn merge_menu(&self, facts: &Facts, cx: &mut Context<Self>) -> AnyElement {
        let t = ui::theme(cx);
        let methods: Vec<AnyElement> = facts
            .allowed
            .iter()
            .map(|&method| {
                self.choice(
                    format!("merge-method-{}", method_id(method)),
                    method_label(method).to_owned(),
                    facts.method == Some(method),
                    None,
                    move |this, cx| {
                        this.act(cx, |session, key, cx| {
                            session.set_merge_method(key, method, cx)
                        });
                    },
                    cx,
                )
            })
            .collect();
        let delete = facts.delete_branch;
        let delete_box = facts.head_in_base.then(|| {
            ui::checkbox("delete-branch")
                .checked(delete)
                .label("Delete branch")
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.act(cx, |session, key, cx| {
                        session.set_delete_branch(key, !delete, cx)
                    });
                }))
        });
        let auto = match (&facts.mergeable, facts.method, &facts.head) {
            (_, None, _) | (_, _, None) => None,
            _ if !facts.caps.auto_merge || !facts.state.is_open() => None,
            (Load::Ready((_, _, true)), _, _) => Some(Action::CancelAutoMerge),
            (_, Some(method), Some(head)) => Some(Action::AutoMerge {
                method,
                delete_branch: delete && facts.head_in_base,
                head: head.clone(),
            }),
        };
        let auto = auto.map(|action| {
            let label = action.label().to_owned();
            self.choice(
                "merge-auto".to_owned(),
                label,
                false,
                None,
                move |this, cx| this.request(action.clone(), cx),
                cx,
            )
        });
        v_flex()
            .w(px(260.0))
            .gap(px(4.0))
            .children(methods)
            .children(delete_box)
            .children(auto.map(|auto| v_flex().pt(px(4.0)).child(ui::divider(cx)).child(auto)))
            .small(&t)
            .into_any_element()
    }

    fn more_menu(&self, facts: &Facts, cx: &mut Context<Self>) -> AnyElement {
        let rows: Vec<AnyElement> = facts
            .state_actions()
            .into_iter()
            .map(|action| {
                let blocked = facts.unsupported(&action);
                let id = format!("more-{}", action_id(&action));
                self.choice(
                    id,
                    action.label().to_owned(),
                    false,
                    blocked,
                    move |this, cx| this.request(action.clone(), cx),
                    cx,
                )
            })
            .collect();
        let empty = rows.is_empty();
        v_flex()
            .w(px(220.0))
            .gap(px(4.0))
            .children(rows)
            .when(empty, |el| {
                el.child("Nothing to do on a merged pull request")
            })
            .into_any_element()
    }

    fn confirmation(&self, facts: &Facts, action: &Action, cx: &mut Context<Self>) -> AnyElement {
        let t = ui::theme(cx);
        let c = &t.colors;
        let danger = matches!(action, Action::Close);
        let label = action.label();
        ui::card(cx)
            .debug_selector(|| "action-confirm-card".into())
            .flex()
            .flex_col()
            .gap(px(8.0))
            .p(px(10.0))
            .border_color(if danger { c.danger } else { c.warn })
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(c.text)
                    .child(action.question(&facts.key, facts.forge)),
            )
            .child(
                h_flex()
                    .gap(px(8.0))
                    .child(
                        Button::new(
                            "action-confirm",
                            label,
                            if danger {
                                ButtonKind::Danger
                            } else {
                                ButtonKind::Primary
                            },
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.act(cx, |session, key, cx| session.confirm_action(key, cx));
                        })),
                    )
                    .child(
                        Button::new("action-cancel", "Cancel", ButtonKind::Ghost).on_click(
                            cx.listener(|this, _, _, cx| {
                                this.act(cx, |session, key, cx| session.cancel_action(key, cx));
                            }),
                        ),
                    ),
            )
            .into_any_element()
    }
}

/// Work on the session for one pull request, as a palette command runs it.
type SessionAct = Rc<dyn Fn(&mut Session, &PrKey, &mut Context<Session>)>;

/// The command palette's commands on `key`: its panels, the review
/// verdicts, merge, draft, close or reopen, and its links. One its state or
/// its forge does not allow carries the reason instead of running.
pub(crate) fn palette_items(session: &Entity<Session>, key: &PrKey, cx: &App) -> Vec<Item> {
    let state = session.read(cx);
    let Some(open) = state.pr(key) else {
        return Vec::new();
    };
    let writing = state.writing_for(key);
    let Some(facts) = Facts::read(state, key, writing.as_ref().map(|w| w.read(cx))) else {
        return Vec::new();
    };
    let url = open
        .forge_pr()
        .map(|pr| pr.web_url.clone())
        .filter(|url| !url.is_empty())
        .unwrap_or_else(|| references::pr_url(open.pr.forge, &key.repo, key.number));
    let detail = SharedString::from(open.pr.ref_label());
    // Runs `f` on the session with `key` focused, as the header's buttons do.
    let on_session = |f: SessionAct| {
        let (session, key) = (session.clone(), key.clone());
        move |_: &mut Window, cx: &mut App| {
            session.update(cx, |session, cx| {
                session.focus(&key, cx);
                f(session, &key, cx);
            });
        }
    };
    let mut items = Vec::new();
    for (kind, id, title) in [
        (PanelKind::Diff, "pr:diff", "Open the diff"),
        (
            PanelKind::Conversation,
            "pr:conversation",
            "Open the conversation",
        ),
        (PanelKind::Checks, "pr:checks", "Open the checks"),
    ] {
        items.push(Item::new(
            id,
            title,
            on_session(Rc::new(move |session, key, cx| {
                session.show(key, kind, OpenMode::Permanent, cx);
            })),
        ));
    }
    items.push(Item::new(
        "pr:refresh",
        "Refresh the pull request",
        on_session(Rc::new(|session, key, cx| session.refresh_pr(key, cx))),
    ));
    for verdict in [Verdict::Approve, Verdict::RequestChanges, Verdict::Comment] {
        let title = match verdict {
            Verdict::Approve => "Approve",
            Verdict::RequestChanges => "Request changes",
            Verdict::Comment => "Submit the review as a comment",
        };
        let (session, key, writing) = (session.clone(), key.clone(), writing.clone());
        let item = Item::new(
            format!("pr:review-{}", verdict_id(verdict)),
            title,
            move |_, cx| {
                let Some(writing) = &writing else {
                    return;
                };
                session.update(cx, |session, cx| session.keep(&key, cx));
                writing.update(cx, |writing, cx| {
                    writing.set_verdict(verdict, cx);
                    writing.submit_review(cx);
                });
            },
        );
        items.push(item.disabled(facts.submit_blocked(verdict)));
    }
    if facts.caps.merge {
        let title = facts.method.map_or_else(
            || "Merge".to_owned(),
            |method| format!("Merge: {}", method_label(method)),
        );
        let action = facts.merge_action();
        let run = on_session(Rc::new(move |session, key, cx| {
            if let Some(action) = action.clone() {
                session.request_action(key, action, cx);
            }
        }));
        items.push(Item::new("pr:merge", title, run).disabled(facts.merge_disabled()));
    }
    for action in facts.state_actions() {
        let title = match action {
            Action::Close => "Close the pull request",
            Action::Reopen => "Reopen the pull request",
            _ => action.label(),
        };
        let blocked = facts.unsupported(&action);
        let id = format!("pr:{}", action_id(&action));
        let run = on_session(Rc::new(move |session, key, cx| {
            session.request_action(key, action.clone(), cx);
        }));
        items.push(Item::new(id, title, run).disabled(blocked));
    }
    let web = url.clone();
    items.push(
        Item::new("pr:web", "Open on the forge", move |_, cx| {
            references::open_link(&web, cx);
        })
        .disabled((!crate::untrusted::is_web_url(&url)).then_some("It has no web address")),
    );
    items.push(Item::new("pr:copy-link", "Copy the link", move |_, cx| {
        cx.write_to_clipboard(ClipboardItem::new_string(url.clone()));
    }));
    let target = key.clone();
    items.push(
        Item::new("pr:open-on", "Open on a machine…", move |window, cx| {
            if let Some(terminals) = Terminals::global(cx) {
                terminals.update(cx, |terminals, cx| {
                    terminals.open_picker(Purpose::OpenOn(target.clone()), window, cx);
                });
            }
        })
        .disabled(
            Terminals::global(cx)
                .is_none()
                .then_some("Terminals are off"),
        ),
    );
    let target = key.clone();
    items.push(Item::new(
        "pr:review-with-agent",
        "Review with agent",
        move |window, cx| crate::claude_review::start(&target, window, cx),
    ));
    items
        .into_iter()
        .map(|item| item.detail(detail.clone()))
        .collect()
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// A popover choice's label, and the line under it that explains it.
struct ChoiceText {
    label: String,
    detail: Option<&'static str>,
}

impl From<String> for ChoiceText {
    fn from(label: String) -> Self {
        Self {
            label,
            detail: None,
        }
    }
}

/// What `verdict` tells the author, under its choice in the review popover.
fn verdict_detail(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Comment => "General feedback, without approving",
        Verdict::Approve => "Approve these changes",
        Verdict::RequestChanges => "Feedback that must be addressed before merging",
    }
}

fn verdict_id(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Comment => "comment",
        Verdict::Approve => "approve",
        Verdict::RequestChanges => "request-changes",
    }
}

pub(crate) fn method_id(method: MergeMethod) -> &'static str {
    match method {
        MergeMethod::Merge => "merge",
        MergeMethod::Squash => "squash",
        MergeMethod::Rebase => "rebase",
    }
}

pub(crate) fn action_id(action: &Action) -> &'static str {
    match action {
        Action::Merge { .. } => "merge",
        Action::AutoMerge { .. } => "auto-merge",
        Action::CancelAutoMerge => "cancel-auto-merge",
        Action::Close => "close",
        Action::Reopen => "reopen",
        Action::SetDraft(true) => "draft",
        Action::SetDraft(false) => "ready",
    }
}

impl Render for PrActions {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(facts) = self.facts(cx) else {
            return div().into_any_element();
        };
        let t = ui::theme(cx);
        let c = &t.colors;
        let verdict = facts.verdict;
        let review_blocked = facts.submit_blocked(verdict);
        let review_label = match (facts.submitting, facts.queued) {
            (true, _) => "Submitting".to_owned(),
            (false, 0) => "Submit review".to_owned(),
            (false, n) => format!("Submit review ({n})"),
        };
        let review = self.menu_button("review-button", review_label, Menu::Review, cx);
        let merge_blocked = facts.merge_blocked();
        let merge = facts.caps.merge.then(|| {
            let label = match (&facts.busy, facts.method) {
                (Some(action), _) => format!("{}...", action.label()),
                (None, Some(method)) => method_label(method).to_owned(),
                (None, None) => "Merge".to_owned(),
            };
            let action = facts.merge_action();
            self.split(
                "merge-split",
                label,
                facts.merge_disabled().is_some(),
                Menu::Merge,
                move |this, cx| {
                    if let Some(action) = action.clone() {
                        this.request(action, cx);
                    }
                },
                cx,
            )
        });
        let more = ui::icon_button(
            cx,
            "more-menu",
            IconName::Ellipsis,
            cx.listener(|this, _, _, cx| this.toggle(Menu::More, cx)),
        )
        .debug_selector(|| "more-menu".into());
        let popover = self.menu.map(|menu| match menu {
            Menu::Review => self.review_menu(&facts, cx),
            Menu::Merge => self.merge_menu(&facts, cx),
            Menu::More => self.more_menu(&facts, cx),
            Menu::Agent => self.agent_menu(cx),
        });
        let popover = popover.map(|body| {
            deferred(
                anchored().anchor(Anchor::TopLeft).child(
                    div()
                        .debug_selector(|| "action-popover".into())
                        .id("action-popover")
                        .occlude()
                        .mt(px(4.0))
                        .p(px(6.0))
                        .rounded(t.radius.lg)
                        .border_1()
                        .border_color(c.border_strong)
                        .bg(c.surface_menu)
                        .small(&t)
                        .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                            this.menu = None;
                            cx.notify();
                        }))
                        .child(body),
                ),
            )
            .with_priority(1)
        });
        let notes = [
            review_blocked
                .filter(|_| self.menu.is_none() && !self.compact)
                .filter(|why| why.contains("own"))
                .map(|why| ("review-note", why, c.text_muted)),
            merge_blocked
                .filter(|_| facts.caps.merge)
                .map(|why| ("merge-blocked", why, c.text_muted)),
            facts
                .review_error
                .clone()
                .map(|error| ("review-error", error.to_string(), c.danger)),
            facts.notice.clone().map(|(text, error)| {
                (
                    "action-notice",
                    text.to_string(),
                    if error { c.danger } else { c.ok },
                )
            }),
        ];
        let notes = notes.into_iter().flatten().map(|(selector, text, color)| {
            div()
                .debug_selector(move || selector.into())
                .tiny(&t)
                .truncate()
                .text_color(color)
                .child(text)
        });
        let confirm = facts
            .confirm
            .as_ref()
            .filter(|_| !self.hide_confirm)
            .map(|action| self.confirmation(&facts, action, cx));
        let open_on = Button::toolbar("open-on-machine", "Open on…")
            .on_click(cx.listener(|this, _, window, cx| this.open_on(window, cx)));
        let pending = self.writing.as_ref().map_or(0, |writing| {
            crate::claude_review::pending(writing.read(cx)).len()
        });
        let with_agent = self.agent_split(pending, cx);
        let worktree = self.pr.as_ref().and_then(|key| {
            let state = Terminals::global(cx)?.read(cx).worktree(key)?.clone();
            let color = match state {
                WorktreeState::Failed { .. } => c.danger,
                WorktreeState::Choosing(_) => c.warn,
                WorktreeState::Working { .. } | WorktreeState::Ready(_) => c.text_muted,
            };
            Some(
                div()
                    .debug_selector(|| "worktree-state".into())
                    .tiny(&t)
                    .truncate()
                    .text_color(color)
                    .child(state.line()),
            )
        });
        let bar = h_flex()
            .gap(px(8.0))
            .items_center()
            .child(review)
            .children(merge)
            .child(more)
            .child(open_on)
            .child(with_agent);
        let bar = div().relative().child(bar).children(popover);
        if self.compact {
            return v_flex()
                .debug_selector(|| "pr-actions-compact".into())
                .flex_none()
                .gap(px(4.0))
                .px(px(12.0))
                .py(px(4.0))
                .border_b_1()
                .border_color(c.border)
                .child(
                    h_flex()
                        .gap(px(12.0))
                        .items_center()
                        .child(bar)
                        .children(notes)
                        .children(worktree),
                )
                .children(confirm)
                .into_any_element();
        }
        v_flex()
            .debug_selector(|| "pr-actions".into())
            .gap(px(6.0))
            .child(bar)
            .children(notes)
            .children(worktree)
            .children(confirm)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_pull_requests_cannot_be_approved_where_the_forge_forbids_it() {
        let github = Capabilities::baseline(ForgeKind::GitHub);
        let gitlab = Capabilities::baseline(ForgeKind::GitLab);
        let reason = verdict_blocked(Verdict::Approve, &github, true, ForgeKind::GitHub);
        assert_eq!(
            reason.as_deref(),
            Some("GitHub does not let you approve your own pull request")
        );
        assert!(verdict_blocked(Verdict::Comment, &github, true, ForgeKind::GitHub).is_none());
        assert!(verdict_blocked(Verdict::Approve, &github, false, ForgeKind::GitHub).is_none());
        assert_eq!(
            verdict_blocked(Verdict::Approve, &gitlab, true, ForgeKind::GitLab),
            None,
            "GitLab lets authors approve unless the project forbids it"
        );
    }
}
