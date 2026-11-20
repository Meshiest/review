//! An inbox row's right-click menu and the line its actions confirm in.
//!
//! Reviews, merges, closes, draft state and re-review requests go through
//! the same session paths as the pull request header: the pull request is
//! loaded quietly (no tab, the focus stays) and released once the action is
//! done. Approve sends at once; request changes and comment confirm in one
//! line naming the repository and number, with a summary; merge and close
//! use the lifecycle's own confirmation, with the head it recorded. Results
//! patch the row.

use forge::model::{MergeMethod, PrState, ReviewState as ForgeReview, Reviewer, User};
use gpui::{
    AnyElement, App, AppContext as _, ClipboardItem, Context, Entity, Focusable as _, FontWeight,
    InteractiveElement as _, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement as _, Styled, Subscription, Window, div, px,
};
use gpui_kit::component::{
    Sizable as _, h_flex,
    input::{Input, InputState},
    menu::{PopupMenu, PopupMenuItem},
    tooltip::Tooltip,
    v_flex,
};

use super::marks::{Marks, ReviewedMark};
use super::repo_id;
use crate::claude_review;
use crate::lifecycle::{Action, Load, blocker_label, method_label, supported};
use crate::model::{ForgeKind, PrKey, PullRequest, ReviewState};
use crate::panels::PanelKind;
use crate::people::{PeopleWrite, reviewed_by};
use crate::pr_header::verdict_blocked;
use crate::session::{OpenMode, Session};
use crate::sidebar::Sidebar;
use crate::sink::Verdict;
use crate::subscriptions::Subscriptions;
use crate::terminals::{Purpose, Terminals};
use crate::ui::{self, Button, ButtonKind, TextExt as _};
use crate::untrusted::is_web_url;

/// What an entry of a row's menu does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowAction {
    /// Brings the pull request's panel forward, as the row's click does.
    Open(PanelKind, OpenMode),
    Verdict(Verdict),
    /// Merge with this method; `None` takes the forge's default once it is
    /// known.
    Merge(Option<MergeMethod>),
    Close,
    Reopen,
    /// `true` converts to draft, `false` marks it ready.
    SetDraft(bool),
    /// Asks everyone who reviewed to review again.
    RequestReReview,
    /// The machine picker for the pull request's worktree.
    OpenOnMachine,
    ReviewWithAgent,
    CopyReference,
    CopyLink,
    CopyBranch,
    CopyNumber,
    OpenOnForge,
    /// `true` sets the reviewed mark, `false` clears it.
    MarkReviewed(bool),
    MarkRead,
    MarkUnread,
    /// `true` starts watching its activity, `false` stops.
    Watch(bool),
    HideRepo,
}

/// One entry of a row's menu; a blocked one is shown disabled with why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowEntry {
    pub action: RowAction,
    pub label: String,
    pub blocked: Option<String>,
}

impl RowEntry {
    fn new(action: RowAction, label: impl Into<String>, blocked: Option<String>) -> Self {
        Self {
            action,
            label: label.into(),
            blocked,
        }
    }
}

/// `owner/name#12 on host`, as confirmations name a pull request.
fn target(key: &PrKey, forge: ForgeKind) -> String {
    format!(
        "{}/{}{}{} on {}",
        key.repo.owner,
        key.repo.name,
        forge.sigil(),
        key.number,
        key.repo.host
    )
}

fn not_open(state: PrState) -> Option<String> {
    match state {
        PrState::Open | PrState::Draft => None,
        PrState::Merged => Some("It is merged".to_owned()),
        PrState::Closed => Some("It is closed".to_owned()),
    }
}

/// The menu of `key`'s row, in groups: opening it, reviews, the pull
/// request's state, other machines, copying and the forge, the local marks.
#[must_use]
pub fn entries(key: &PrKey, cx: &App) -> Vec<Vec<RowEntry>> {
    let session = Session::global(cx).read(cx);
    let Some(row) = session.inbox.get(key) else {
        return Vec::new();
    };
    let caps = session.capabilities_of(key, row.forge);
    let own = session
        .account(&key.repo.host)
        .is_some_and(|me| me.login.eq_ignore_ascii_case(&row.author));
    let open = vec![
        RowEntry::new(
            RowAction::Open(PanelKind::Conversation, OpenMode::Preview),
            "Open",
            None,
        ),
        RowEntry::new(
            RowAction::Open(PanelKind::Conversation, OpenMode::Permanent),
            "Open in a new tab",
            None,
        ),
        RowEntry::new(
            RowAction::Open(PanelKind::Diff, OpenMode::Permanent),
            "Open diff",
            None,
        ),
    ];
    let reviews = [Verdict::Approve, Verdict::RequestChanges, Verdict::Comment]
        .into_iter()
        .map(|verdict| {
            let label = match verdict {
                Verdict::Approve => "Approve",
                Verdict::RequestChanges => "Request changes",
                Verdict::Comment => "Comment",
            };
            let blocked =
                not_open(row.state).or_else(|| verdict_blocked(verdict, &caps, own, row.forge));
            RowEntry::new(RowAction::Verdict(verdict), label, blocked)
        })
        .collect();

    let mut state = merge_entries(session, row, &caps);
    let lifecycle = match row.state {
        PrState::Open => vec![
            (
                RowAction::SetDraft(true),
                Action::SetDraft(true),
                "Convert to draft",
            ),
            (RowAction::Close, Action::Close, "Close"),
        ],
        PrState::Draft => vec![
            (
                RowAction::SetDraft(false),
                Action::SetDraft(false),
                "Mark ready",
            ),
            (RowAction::Close, Action::Close, "Close"),
        ],
        PrState::Closed => vec![(RowAction::Reopen, Action::Reopen, "Reopen")],
        PrState::Merged => Vec::new(),
    };
    state.extend(lifecycle.into_iter().map(|(row_action, action, label)| {
        let blocked = (!supported(&caps, &action))
            .then(|| format!("{} cannot do this from here", row.forge.label()));
        RowEntry::new(row_action, label, blocked)
    }));
    let asked = session
        .forge_record(key)
        .map(|pr| reviewed_by(&pr))
        .unwrap_or_default();
    let blocked = not_open(row.state).or_else(|| {
        asked
            .is_empty()
            .then(|| "No one has reviewed it yet".to_owned())
    });
    state.push(RowEntry::new(
        RowAction::RequestReReview,
        "Request re-review",
        blocked,
    ));

    let machines = vec![
        RowEntry::new(
            RowAction::OpenOnMachine,
            "Open on…",
            Terminals::global(cx)
                .is_none()
                .then(|| "Terminals are not available".to_owned()),
        ),
        RowEntry::new(
            RowAction::ReviewWithAgent,
            "Review with agent",
            Terminals::global(cx)
                .is_none()
                .then(|| "Terminals are not available".to_owned()),
        ),
    ];

    let copy = vec![
        RowEntry::new(RowAction::CopyReference, "Copy reference", None),
        RowEntry::new(RowAction::CopyLink, "Copy link", None),
        RowEntry::new(RowAction::CopyBranch, "Copy branch name", None),
        RowEntry::new(RowAction::CopyNumber, "Copy number", None),
        RowEntry::new(RowAction::OpenOnForge, "Open on forge", None),
    ];

    let marks = Marks::try_global(cx)
        .map(|marks| marks.read(cx).row(row))
        .unwrap_or_default();
    let mut local = vec![
        if marks.reviewed == Some(ReviewedMark::Current) {
            RowEntry::new(RowAction::MarkReviewed(false), "Clear reviewed mark", None)
        } else {
            RowEntry::new(RowAction::MarkReviewed(true), "Mark reviewed", None)
        },
        if marks.unread {
            RowEntry::new(RowAction::MarkRead, "Mark read", None)
        } else {
            RowEntry::new(RowAction::MarkUnread, "Mark unread", None)
        },
    ];
    if let Some(subscriptions) = Subscriptions::global(cx) {
        local.push(if subscriptions.read(cx).is_watched(key, cx) {
            RowEntry::new(RowAction::Watch(false), "Stop watching", None)
        } else {
            RowEntry::new(RowAction::Watch(true), "Watch", None)
        });
    }
    local.push(RowEntry::new(
        RowAction::HideRepo,
        "Hide this repository",
        None,
    ));
    vec![open, reviews, state, machines, copy, local]
}

/// The merge entries: one per method the repository allows when that is
/// known, else one that takes the forge's default; disabled with the
/// blockers when the forge said it cannot merge.
fn merge_entries(
    session: &Session,
    row: &PullRequest,
    caps: &forge::Capabilities,
) -> Vec<RowEntry> {
    let single =
        |blocked: Option<String>| vec![RowEntry::new(RowAction::Merge(None), "Merge", blocked)];
    if !caps.merge {
        return single(Some("This forge cannot merge from here".to_owned()));
    }
    if !row.state.is_open() {
        return single(Some(format!(
            "Cannot merge: {}",
            blocker_label(forge::model::MergeBlocker::NotOpen)
        )));
    }
    let Some(lifecycle) = session.merge_preview(&row.key) else {
        return single(None);
    };
    let blocked = match &lifecycle.mergeability {
        Load::Ready(m) if m.mergeable == Some(true) => None,
        Load::Ready(m) if m.reasons.is_empty() => {
            Some("Cannot merge: the forge refuses it".to_owned())
        }
        Load::Ready(m) => Some(format!(
            "Cannot merge: {}",
            m.reasons
                .iter()
                .map(|reason| blocker_label(*reason))
                .collect::<Vec<_>>()
                .join(", ")
        )),
        // Asked when the entry is picked.
        Load::Idle | Load::Loading => None,
        Load::Failed(error) => Some(format!("Mergeability not read: {error}")),
        Load::Unsupported => Some("This forge cannot merge from here".to_owned()),
    };
    match lifecycle.settings.ready() {
        Some(settings) => settings
            .allowed
            .iter()
            .map(|&method| {
                RowEntry::new(
                    RowAction::Merge(Some(method)),
                    method_label(method),
                    blocked.clone(),
                )
            })
            .collect(),
        None => single(blocked),
    }
}

/// `key`'s row menu.
pub(crate) fn popup(
    menu: PopupMenu,
    key: &PrKey,
    _: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    entries(key, cx)
        .into_iter()
        .enumerate()
        .fold(menu, |menu, (ix, group)| {
            let menu = if ix > 0 { menu.separator() } else { menu };
            group.into_iter().fold(menu, |menu, entry| {
                let RowEntry {
                    action,
                    label,
                    blocked,
                } = entry;
                let item = match blocked {
                    // Disabled, with why on hover rather than under each entry.
                    Some(why) => {
                        let why = SharedString::from(why);
                        PopupMenuItem::element(move |_, cx| {
                            let why = why.clone();
                            div()
                                .id(SharedString::from(format!("blocked-{label}")))
                                .w_full()
                                .text_color(ui::theme(cx).colors.text_faint)
                                .tooltip(move |window, cx| {
                                    Tooltip::new(why.clone()).build(window, cx)
                                })
                                .child(label.clone())
                        })
                        .disabled(true)
                    }
                    None => {
                        let key = key.clone();
                        PopupMenuItem::new(label)
                            .on_click(move |_, window, cx| run(action.clone(), &key, window, cx))
                    }
                };
                menu.item(item)
            })
        })
}

/// Does what `action` of `key`'s row menu says.
pub fn run(action: RowAction, key: &PrKey, window: &mut Window, cx: &mut App) {
    let session = Session::global(cx);
    let Some(row) = session.read(cx).inbox.get(key).cloned() else {
        return;
    };
    let copy = |text: String, cx: &mut App| cx.write_to_clipboard(ClipboardItem::new_string(text));
    match action {
        RowAction::Open(kind, mode) => {
            Sidebar::global(cx).update(cx, |sidebar, cx| sidebar.show_row(key, kind, mode, cx));
        }
        RowAction::OpenOnMachine => {
            open_for_machines(key, cx);
            if let Some(terminals) = Terminals::global(cx) {
                let key = key.clone();
                terminals.update(cx, |terminals, cx| {
                    terminals.open_picker(Purpose::OpenOn(key), window, cx);
                });
            }
        }
        RowAction::ReviewWithAgent => {
            open_for_machines(key, cx);
            claude_review::start(key, window, cx);
        }
        RowAction::CopyNumber => copy(row.key.number.to_string(), cx),
        RowAction::Watch(_) => {
            if let Some(subscriptions) = Subscriptions::global(cx) {
                subscriptions.update(cx, |subscriptions, cx| subscriptions.toggle(key, cx));
            }
        }
        RowAction::HideRepo => {
            let repo = repo_id(&key.repo);
            Sidebar::global(cx).update(cx, |sidebar, cx| sidebar.set_repo_hidden(&repo, true, cx));
        }
        RowAction::CopyReference => copy(row.ref_label(), cx),
        RowAction::CopyLink => copy(row.web_link(), cx),
        RowAction::CopyBranch => copy(row.head.to_string(), cx),
        RowAction::OpenOnForge => {
            let url = row.web_link();
            if is_web_url(&url) {
                cx.open_url(&url);
            }
        }
        RowAction::MarkReviewed(reviewed) => {
            Marks::global(cx).update(cx, |marks, cx| marks.set_reviewed(&row, reviewed, cx));
        }
        RowAction::MarkRead => {
            Marks::global(cx).update(cx, |marks, cx| marks.mark_seen(&row, cx));
        }
        RowAction::MarkUnread => {
            Marks::global(cx).update(cx, |marks, cx| marks.mark_unread(key, cx));
        }
        RowAction::Verdict(_)
        | RowAction::Merge(_)
        | RowAction::Close
        | RowAction::Reopen
        | RowAction::SetDraft(_)
        | RowAction::RequestReReview => {
            Sidebar::global(cx).update(cx, |sidebar, cx| {
                sidebar.start_row_action(&row, action, window, cx);
            });
        }
    }
}

/// Opens `key` for the machine actions, which work on an open pull request,
/// unless it already is.
fn open_for_machines(key: &PrKey, cx: &mut App) {
    Session::global(cx).update(cx, |session, cx| {
        if !session.is_open(key) || session.is_background(key) {
            session.show(key, PanelKind::Conversation, OpenMode::Permanent, cx);
        }
    });
}

/// An action from a row's menu, in the line under the inbox's filter.
pub(crate) struct Acting {
    key: PrKey,
    forge: ForgeKind,
    step: Step,
    _watch: Option<Subscription>,
}

enum Step {
    /// A verdict waiting for its confirmation; request changes and comment
    /// carry their summary box.
    Verdict {
        verdict: Verdict,
        summary: Option<Entity<InputState>>,
    },
    /// The verdict went to the review sink: the writing's review epoch then.
    Submitted { verdict: Verdict, epoch: u64 },
    /// Reopen asks first here; the lifecycle does not.
    ConfirmReopen,
    /// A merge asked before the forge said whether and how it can merge.
    WaitMerge(Option<MergeMethod>),
    /// Handed to the lifecycle: its confirmation, the write, its notice.
    Lifecycle,
    /// Everyone who reviewed was asked again.
    ReReview { asked: usize },
}

/// What a verdict makes of the viewer's reviewer row.
fn review_state(verdict: Verdict) -> (ForgeReview, ReviewState) {
    match verdict {
        Verdict::Approve => (ForgeReview::Approved, ReviewState::Approved),
        Verdict::RequestChanges => (ForgeReview::ChangesRequested, ReviewState::ChangesRequested),
        Verdict::Comment => (ForgeReview::Commented, ReviewState::Commented),
    }
}

impl Sidebar {
    /// Starts `action` from `row`'s menu: a confirmation line, or straight to
    /// the session for what needs none.
    pub(crate) fn start_row_action(
        &mut self,
        row: &PullRequest,
        action: RowAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.end_row_action(cx);
        let key = row.key.clone();
        let step = match action {
            RowAction::Verdict(verdict) => {
                let summary = (verdict != Verdict::Approve).then(|| {
                    let placeholder = match verdict {
                        Verdict::RequestChanges => "What needs to change",
                        _ => "Your comment",
                    };
                    cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
                });
                if let Some(summary) = &summary {
                    let handle = summary.read(cx).focus_handle(cx);
                    window.focus(&handle, cx);
                }
                Step::Verdict { verdict, summary }
            }
            RowAction::Reopen => Step::ConfirmReopen,
            RowAction::Merge(method) => {
                self.load_quietly(&key, cx);
                Step::WaitMerge(method)
            }
            RowAction::Close | RowAction::SetDraft(_) => {
                let action = match action {
                    RowAction::SetDraft(draft) => Action::SetDraft(draft),
                    _ => Action::Close,
                };
                self.load_quietly(&key, cx);
                self.session
                    .update(cx, |session, cx| session.request_action(&key, action, cx));
                Step::Lifecycle
            }
            RowAction::RequestReReview => {
                self.load_quietly(&key, cx);
                let asked = self
                    .session
                    .read(cx)
                    .forge_record(&key)
                    .map(|pr| reviewed_by(&pr))
                    .unwrap_or_default();
                let count = asked.len();
                self.session.update(cx, |session, cx| {
                    session.people_write(&key, PeopleWrite::RequestAgain(asked), cx);
                });
                Step::ReReview { asked: count }
            }
            RowAction::Open(..)
            | RowAction::OpenOnMachine
            | RowAction::ReviewWithAgent
            | RowAction::Watch(_)
            | RowAction::HideRepo
            | RowAction::CopyNumber
            | RowAction::CopyReference
            | RowAction::CopyLink
            | RowAction::CopyBranch
            | RowAction::OpenOnForge
            | RowAction::MarkReviewed(_)
            | RowAction::MarkRead
            | RowAction::MarkUnread => return,
        };
        let watch = match &step {
            Step::Verdict {
                summary: Some(summary),
                ..
            } => Some(cx.observe(summary, |_, _, cx| cx.notify())),
            _ => None,
        };
        let approve = matches!(
            step,
            Step::Verdict {
                verdict: Verdict::Approve,
                ..
            }
        );
        self.inbox.acting = Some(Acting {
            key,
            forge: row.forge,
            step,
            _watch: watch,
        });
        // Choosing Approve from the menu is the decision; it asks nothing more.
        if approve {
            self.confirm_row_action(cx);
            return;
        }
        self.advance_row_action(cx);
        cx.notify();
    }

    fn load_quietly(&self, key: &PrKey, cx: &mut Context<Self>) {
        self.session
            .update(cx, |session, cx| session.load_quietly(key, cx));
    }

    /// The pull request a row's action is on, if one is under way.
    #[must_use]
    pub fn row_action_key(&self) -> Option<&PrKey> {
        self.inbox.acting.as_ref().map(|acting| &acting.key)
    }

    /// The line's confirm button: sends what it asked.
    pub fn confirm_row_action(&mut self, cx: &mut Context<Self>) {
        enum Send {
            Verdict(Verdict, Option<String>),
            Reopen,
            Confirmed,
        }
        let Some(acting) = &self.inbox.acting else {
            return;
        };
        let key = acting.key.clone();
        let send = match &acting.step {
            Step::Verdict { verdict, summary } => Send::Verdict(
                *verdict,
                summary
                    .as_ref()
                    .map(|summary| summary.read(cx).value().trim().to_owned()),
            ),
            Step::ConfirmReopen => Send::Reopen,
            Step::Lifecycle => Send::Confirmed,
            Step::Submitted { .. } | Step::WaitMerge(_) | Step::ReReview { .. } => return,
        };
        match send {
            Send::Verdict(_, Some(body)) if body.is_empty() => return,
            Send::Verdict(verdict, body) => {
                self.load_quietly(&key, cx);
                let Some(writing) = self.session.read(cx).writing_for(&key) else {
                    return;
                };
                let epoch = writing.read(cx).review_epoch();
                writing.update(cx, |writing, cx| {
                    if let Some(body) = body {
                        writing.set_review_body(body);
                    }
                    writing.set_verdict(verdict, cx);
                    writing.submit_review(cx);
                });
                let watch = cx.observe(&writing, |this: &mut Self, _, cx| {
                    this.advance_row_action(cx);
                    cx.notify();
                });
                if let Some(acting) = self.inbox.acting.as_mut() {
                    acting.step = Step::Submitted { verdict, epoch };
                    acting._watch = Some(watch);
                }
            }
            Send::Reopen => {
                self.load_quietly(&key, cx);
                self.session.update(cx, |session, cx| {
                    session.request_action(&key, Action::Reopen, cx);
                });
                acting_step(self, Step::Lifecycle);
            }
            Send::Confirmed => {
                self.session
                    .update(cx, |session, cx| session.confirm_action(&key, cx));
            }
        }
        self.advance_row_action(cx);
        cx.notify();
    }

    /// Cancel or dismiss: the line goes, and with it a waiting confirmation.
    pub fn end_row_action(&mut self, cx: &mut Context<Self>) {
        let Some(acting) = self.inbox.acting.take() else {
            return;
        };
        let key = acting.key;
        self.session.update(cx, |session, cx| {
            if session
                .pr(&key)
                .is_some_and(|open| open.lifecycle.confirm.is_some())
            {
                session.cancel_action(&key, cx);
            }
        });
        self.release_idle(cx);
        cx.notify();
    }

    /// Moves a row's action on when the session answered: a merge that
    /// waited for the forge asks for its confirmation, a submitted verdict
    /// patches the row.
    pub(crate) fn advance_row_action(&mut self, cx: &mut Context<Self>) {
        let Some(acting) = &self.inbox.acting else {
            return;
        };
        let key = acting.key.clone();
        match &acting.step {
            Step::WaitMerge(method) => {
                let method = *method;
                let request = {
                    let session = self.session.read(cx);
                    session.pr(&key).and_then(|open| {
                        let lifecycle = &open.lifecycle;
                        let can = matches!(
                            &lifecycle.mergeability,
                            Load::Ready(m) if m.mergeable == Some(true)
                        );
                        let chosen = method.or_else(|| lifecycle.chosen_method())?;
                        let head = open.merge_head()?;
                        let in_base = open.forge_pr().is_none_or(|pr| pr.head_in_base_repo());
                        can.then_some(Action::Merge {
                            method: chosen,
                            delete_branch: lifecycle.delete_branch && in_base,
                            head,
                        })
                    })
                };
                if let Some(action) = request {
                    self.session.update(cx, |session, cx| {
                        if let Some(method) = method {
                            session.set_merge_method(&key, method, cx);
                        }
                        session.request_action(&key, action, cx);
                    });
                    acting_step(self, Step::Lifecycle);
                }
            }
            Step::Submitted { verdict, epoch } => {
                let (verdict, epoch) = (*verdict, *epoch);
                let done = self
                    .session
                    .read(cx)
                    .writing_for(&key)
                    .is_some_and(|writing| writing.read(cx).review_epoch() > epoch);
                // The row shows the new review state, so the line goes; a
                // failure keeps it, with the error.
                if done {
                    self.patch_verdict(&key, verdict, cx);
                    self.end_row_action(cx);
                }
            }
            Step::Verdict { .. }
            | Step::ConfirmReopen
            | Step::Lifecycle
            | Step::ReReview { .. } => {}
        }
    }

    /// The row as the forge would answer after the viewer's verdict.
    fn patch_verdict(&mut self, key: &PrKey, verdict: Verdict, cx: &mut Context<Self>) {
        let (forge_state, overall) = review_state(verdict);
        self.session.update(cx, |session, cx| {
            let account = session
                .account(&key.repo.host)
                .cloned()
                .unwrap_or_else(|| User::new(""));
            if let Some(mut pr) = session.forge_record(key)
                && !account.login.is_empty()
            {
                match pr
                    .reviewers
                    .iter_mut()
                    .find(|r| r.user.login.eq_ignore_ascii_case(&account.login))
                {
                    Some(row) => {
                        row.state = forge_state;
                        row.requested_again = false;
                        row.stale = false;
                    }
                    None => pr.reviewers.push(Reviewer::new(account, forge_state)),
                }
                session.patch_forge_pr(key, pr, cx);
            }
            // A fixture row has no record to read the viewer's state from.
            if let Some(row) = session.inbox.get(key).filter(|row| row.source.is_none()) {
                let row = PullRequest {
                    review: overall,
                    ..row.clone()
                };
                session.patch_row(row);
            }
            cx.notify();
        });
    }

    /// Drops the pull requests loaded only for a row's action once nothing
    /// is under way on them.
    pub(crate) fn release_idle(&mut self, cx: &mut Context<Self>) {
        let acting = self.inbox.acting.as_ref().map(|acting| acting.key.clone());
        let idle: Vec<PrKey> = {
            let session = self.session.read(cx);
            session
                .background_keys()
                .into_iter()
                .filter(|key| Some(key) != acting.as_ref())
                .filter(|key| {
                    let writing_busy = session
                        .writing_for(key)
                        .is_some_and(|writing| writing.read(cx).submitting());
                    let open_busy = session.pr(key).is_some_and(|open| {
                        open.lifecycle.busy.is_some()
                            || open.lifecycle.confirm.is_some()
                            || open.people.busy.is_some()
                    });
                    !writing_busy && !open_busy
                })
                .collect()
        };
        if idle.is_empty() {
            return;
        }
        self.session.update(cx, |session, cx| {
            for key in &idle {
                session.release_background(key, cx);
            }
        });
    }

    /// The line under the filter while a row's action is under way.
    pub(crate) fn row_action_line(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let acting = self.inbox.acting.as_ref()?;
        let t = ui::theme(cx);
        let c = &t.colors;
        let named = target(&acting.key, acting.forge);
        let session = self.session.read(cx);
        let open = session.pr(&acting.key);
        let mut confirm: Option<(String, ButtonKind, bool)> = None;
        let mut input = None;
        let (text, error) = match &acting.step {
            Step::Verdict { verdict, summary } => {
                let empty = summary
                    .as_ref()
                    .is_some_and(|summary| summary.read(cx).value().trim().is_empty());
                confirm = Some((verdict.label().to_owned(), ButtonKind::Primary, empty));
                input = summary.clone();
                let what = match verdict {
                    Verdict::Approve => "Approve",
                    Verdict::RequestChanges => "Request changes on",
                    Verdict::Comment => "Comment on",
                };
                (format!("{what} {named}?"), false)
            }
            Step::Submitted { .. } => {
                let error = session
                    .writing_for(&acting.key)
                    .map(|w| w.read(cx))
                    .filter(|w| !w.submitting())
                    .and_then(|w| w.review_error().cloned());
                match error {
                    Some(error) => (error.to_string(), true),
                    None => (format!("Submitting the review of {named}"), false),
                }
            }
            Step::ConfirmReopen => {
                confirm = Some(("Reopen".to_owned(), ButtonKind::Primary, false));
                (Action::Reopen.question(&acting.key, acting.forge), false)
            }
            Step::WaitMerge(_) => match open.map(|open| &open.lifecycle.mergeability) {
                Some(Load::Ready(m)) if m.mergeable != Some(true) => {
                    let reasons: Vec<&str> = m.reasons.iter().map(|r| blocker_label(*r)).collect();
                    (
                        format!("Cannot merge {named}: {}", reasons.join(", ")),
                        true,
                    )
                }
                Some(Load::Failed(error)) => (format!("Mergeability not read: {error}"), true),
                Some(Load::Unsupported) => ("This forge cannot merge from here".to_owned(), true),
                _ => (format!("Checking whether {named} can merge"), false),
            },
            Step::Lifecycle => {
                let lifecycle = open.map(|open| &open.lifecycle);
                match lifecycle {
                    Some(life) if life.confirm.is_some() => {
                        let action = life.confirm.clone().unwrap_or(Action::Close);
                        let kind = if action == Action::Close {
                            ButtonKind::Danger
                        } else {
                            ButtonKind::Primary
                        };
                        confirm = Some((action.label().to_owned(), kind, false));
                        (action.question(&acting.key, acting.forge), false)
                    }
                    Some(life) if life.busy.is_some() => {
                        let label = life.busy.as_ref().map_or("Working", Action::label);
                        (format!("{label}: {named}"), false)
                    }
                    Some(life) => life.notice.as_ref().map_or_else(
                        || (format!("Nothing waiting on {named}"), false),
                        |notice| (format!("{} {named}", notice.text), notice.error),
                    ),
                    None => (format!("{named} is not loaded"), true),
                }
            }
            Step::ReReview { asked } => match open.map(|open| &open.people) {
                Some(people) if people.busy.is_some() => {
                    (format!("Asking {asked} reviewers of {named} again"), false)
                }
                Some(people) if people.error.is_some() => (
                    people
                        .error
                        .as_ref()
                        .map(|(_, error)| error.to_string())
                        .unwrap_or_default(),
                    true,
                ),
                _ => (format!("Asked {asked} reviewers of {named} again"), false),
            },
        };
        let text_color = if error { c.danger } else { c.text };
        let cancel_label = if confirm.is_some() {
            "Cancel"
        } else {
            "Dismiss"
        };
        let buttons = h_flex()
            .gap(px(6.0))
            .children(confirm.map(|(label, kind, disabled)| {
                Button::new("inbox-action-confirm", label, kind)
                    .disabled(disabled)
                    .on_click(cx.listener(|this, _, _, cx| this.confirm_row_action(cx)))
            }))
            .child(
                Button::new("inbox-action-cancel", cancel_label, ButtonKind::Ghost)
                    .on_click(cx.listener(|this, _, _, cx| this.end_row_action(cx))),
            );
        Some(
            v_flex()
                .id("inbox-action")
                .debug_selector(|| "inbox-action".into())
                .flex_none()
                .px(px(10.0))
                .py(px(6.0))
                .gap(px(6.0))
                .border_b_1()
                .border_color(c.border)
                .bg(c.surface_raised)
                .small(&t)
                .child(
                    div()
                        .debug_selector(|| "inbox-action-text".into())
                        .text_color(text_color)
                        .font_weight(FontWeight::MEDIUM)
                        .child(SharedString::from(text)),
                )
                .children(input.map(|input| Input::new(&input).xsmall()))
                .child(buttons)
                .into_any_element(),
        )
    }
}

fn acting_step(sidebar: &mut Sidebar, step: Step) {
    if let Some(acting) = sidebar.inbox.acting.as_mut() {
        acting.step = step;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirmations_name_repository_number_and_host() {
        let key = PrKey::new(
            crate::model::RepoRef::new("gitlab.com", "group", "proj"),
            crate::model::PrNumber(7),
        );
        assert_eq!(
            target(&key, ForgeKind::GitLab),
            "group/proj!7 on gitlab.com"
        );
        assert_eq!(not_open(PrState::Merged).as_deref(), Some("It is merged"));
        assert_eq!(not_open(PrState::Draft), None);
    }
}
