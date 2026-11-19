//! The right-click menu of a review thread, the same wherever the thread
//! shows: a row of the sidebar's Threads section, a card in the diff, an
//! inline thread in the Conversation. [`ThreadNav`] holds what the
//! Conversation does with threads asked for from elsewhere: the one to
//! scroll to and briefly highlight, and the ones folded or unfolded by hand.

use std::collections::HashMap;
use std::time::Duration;

use forge::ResolveMode;
use forge::model::{Thread, ThreadId};
use gpui::{
    App, AppContext as _, Context, Entity, Global, InteractiveElement as _, ParentElement as _,
    Styled as _, Task, WeakEntity, Window, div,
};
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::v_flex;

use super::comment_menu::{CommentMenu, comment_url, copy};
use super::{ThreadCards, reply_anchor};
use crate::conversation::ConversationPanel;
use crate::model::PrKey;
use crate::panels::PanelKind;
use crate::session::{OpenMode, Session};
use crate::settings::ThreadStyle;
use crate::ui::{self, TextExt as _};
use crate::untrusted::is_web_url;
use crate::writing::ActionState;

/// How long a thread shown in the Conversation stays highlighted.
pub const HIGHLIGHT: Duration = Duration::from_millis(1500);

/// What the Conversation does with threads asked for from elsewhere.
#[derive(Default)]
pub struct ThreadNav {
    /// The thread the Conversation of that pull request scrolls to once it
    /// has laid the thread out.
    reveal: Option<(PrKey, ThreadId)>,
    highlight: Option<(PrKey, ThreadId)>,
    /// Inline threads folded (`true`) or unfolded by hand, against the
    /// Conversation's thread style.
    folds: HashMap<(PrKey, ThreadId), bool>,
    _fade: Option<Task<()>>,
}

struct GlobalNav(Entity<ThreadNav>);

impl Global for GlobalNav {}

impl ThreadNav {
    /// The one shared state, made on first use.
    pub fn global(cx: &mut App) -> Entity<Self> {
        if let Some(nav) = Self::try_global(cx) {
            return nav;
        }
        let nav = cx.new(|_| Self::default());
        cx.set_global(GlobalNav(nav.clone()));
        nav
    }

    #[must_use]
    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalNav>().map(|nav| nav.0.clone())
    }

    /// Asks the Conversation of `key` to unfold `thread`, scroll to it and
    /// highlight it for [`HIGHLIGHT`].
    pub fn reveal(&mut self, key: &PrKey, thread: &ThreadId, cx: &mut Context<Self>) {
        let target = (key.clone(), thread.clone());
        self.folds.insert(target.clone(), false);
        self.reveal = Some(target.clone());
        self.highlight = Some(target);
        self._fade = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(HIGHLIGHT).await;
            this.update(cx, |this, cx| {
                // A Conversation that never laid the thread out stops waiting.
                this.highlight = None;
                this.reveal = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// The thread the Conversation of `key` should scroll to.
    #[must_use]
    pub fn pending_reveal(&self, key: &PrKey) -> Option<&ThreadId> {
        self.reveal
            .as_ref()
            .filter(|(at, _)| at == key)
            .map(|(_, thread)| thread)
    }

    /// The Conversation scrolled to the thread it was asked for.
    pub fn revealed(&mut self, key: &PrKey) {
        if self.pending_reveal(key).is_some() {
            self.reveal = None;
        }
    }

    #[must_use]
    pub fn is_highlighted(&self, key: &PrKey, thread: &ThreadId) -> bool {
        self.highlight
            .as_ref()
            .is_some_and(|(at, id)| at == key && id == thread)
    }

    /// Whether the Conversation shows `thread` folded to one row: by
    /// default when it is resolved or threads are compact.
    #[must_use]
    pub fn is_folded(
        &self,
        key: &PrKey,
        thread: &ThreadId,
        resolved: bool,
        style: ThreadStyle,
    ) -> bool {
        self.folds
            .get(&(key.clone(), thread.clone()))
            .copied()
            .unwrap_or(resolved || style == ThreadStyle::Compact)
    }

    pub fn set_folded(
        &mut self,
        key: &PrKey,
        thread: &ThreadId,
        folded: bool,
        cx: &mut Context<Self>,
    ) {
        self.folds.insert((key.clone(), thread.clone()), folded);
        cx.notify();
    }
}

/// Brings `key`'s Conversation forward, scrolled to `thread`, unfolded and
/// highlighted.
pub fn show_in_conversation(key: &PrKey, thread: &ThreadId, cx: &mut App) {
    ThreadNav::global(cx).update(cx, |nav, cx| nav.reveal(key, thread, cx));
    Session::global(cx).update(cx, |session, cx| {
        session.show(key, PanelKind::Conversation, OpenMode::Preview, cx);
    });
}

/// Shows `thread` at its line in `key`'s Diff.
pub fn jump_to_line(key: &PrKey, thread: &ThreadId, focus_diff: bool, cx: &mut App) {
    Session::global(cx).update(cx, |session, cx| {
        session.focus(key, cx);
        session.jump_to_thread(thread.clone(), focus_diff, cx);
    });
}

/// What a thread menu entry does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadAction {
    ShowInConversation,
    JumpToLine,
    Reply,
    /// `true` resolves, `false` unresolves.
    SetResolved(bool),
    /// Folds the thread where the menu was opened, or unfolds it.
    ToggleFolded,
    CopyLink,
    OpenOnForge,
}

impl ThreadAction {
    /// The entry's `debug_selector`, after `thread-menu-`.
    fn slug(self) -> &'static str {
        match self {
            Self::ShowInConversation => "show-in-conversation",
            Self::JumpToLine => "jump-to-line",
            Self::Reply => "reply",
            Self::SetResolved(true) => "resolve",
            Self::SetResolved(false) => "unresolve",
            Self::ToggleFolded => "fold",
            Self::CopyLink => "copy-link",
            Self::OpenOnForge => "open-on-forge",
        }
    }
}

/// One entry of the menu; a blocked one is shown disabled with why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadEntry {
    pub action: ThreadAction,
    pub label: &'static str,
    pub blocked: Option<String>,
}

impl ThreadEntry {
    fn new(action: ThreadAction, label: &'static str, blocked: Option<String>) -> Self {
        Self {
            action,
            label,
            blocked,
        }
    }
}

/// Where the menu was opened: what folding acts on.
#[derive(Clone)]
pub enum Origin {
    /// The sidebar's Threads section: folding acts on the Conversation.
    Sidebar,
    Diff(WeakEntity<ThreadCards>),
    Conversation,
}

/// The thread a menu is for, read again from the session when it opens.
#[derive(Clone)]
pub struct ThreadMenu {
    pub key: PrKey,
    pub thread: ThreadId,
    pub origin: Origin,
}

impl ThreadMenu {
    #[must_use]
    pub fn new(key: PrKey, thread: ThreadId, origin: Origin) -> Self {
        Self {
            key,
            thread,
            origin,
        }
    }

    fn thread(&self, cx: &App) -> Option<Thread> {
        Session::global(cx)
            .read(cx)
            .pr(&self.key)?
            .threads
            .iter()
            .find(|thread| thread.id == self.thread)
            .cloned()
    }

    /// The thread's page on the forge: its first comment's permalink.
    #[must_use]
    pub fn link(&self, cx: &App) -> Option<String> {
        let thread = self.thread(cx)?;
        let open_forge = Session::global(cx).read(cx).pr(&self.key)?.pr.forge;
        comment_url(
            open_forge,
            &self.key.repo,
            self.key.number,
            thread.comments.first()?,
            thread.anchor.is_some(),
        )
    }

    /// Whether the thread is folded where the menu was opened.
    fn is_folded(&self, cx: &App) -> bool {
        match &self.origin {
            Origin::Diff(cards) => cards.upgrade().is_some_and(|cards| {
                let cards = cards.read(cx);
                cards
                    .threads()
                    .iter()
                    .position(|t| t.id == self.thread)
                    .is_some_and(|ix| cards.is_collapsed(ix, cx))
            }),
            Origin::Sidebar | Origin::Conversation => {
                ThreadNav::try_global(cx).is_some_and(|nav| {
                    nav.read(cx).is_folded(
                        &self.key,
                        &self.thread,
                        self.thread(cx).is_some_and(|t| t.is_resolved()),
                        ConversationPanel::thread_style(cx),
                    )
                })
            }
        }
    }

    /// The entries in groups: where to see it, what to do to it, its link.
    #[must_use]
    pub fn entries(&self, cx: &App) -> Vec<Vec<ThreadEntry>> {
        use ThreadAction as A;

        let Some(thread) = self.thread(cx) else {
            return Vec::new();
        };
        let session = Session::global(cx).read(cx);
        let writing = session.writing_for(&self.key);
        let not_open = || {
            writing
                .is_none()
                .then(|| "The pull request is not open".to_owned())
        };
        let no_line = thread
            .anchor
            .is_none()
            .then(|| "It is not on a line".to_owned());
        let resolved = thread.is_resolved();
        let resolve_label = if resolved { "Unresolve" } else { "Resolve" };
        let busy = writing
            .as_ref()
            .and_then(|w| w.read(cx).thread_state(&thread.id).cloned())
            == Some(ActionState::Busy);
        let resolve_blocked = not_open()
            .or_else(|| self.resolve_blocked(&thread, cx))
            .or_else(|| busy.then(|| "A change to it is on its way".to_owned()));
        let fold_label = if self.is_folded(cx) {
            "Expand"
        } else {
            "Collapse"
        };
        let no_link = self
            .link(cx)
            .filter(|url| is_web_url(url))
            .is_none()
            .then(|| "The forge gave no address for it".to_owned());
        vec![
            vec![
                ThreadEntry::new(A::ShowInConversation, "Show in conversation", None),
                ThreadEntry::new(A::JumpToLine, "Jump to line", no_line),
            ],
            vec![
                ThreadEntry::new(A::Reply, "Reply", not_open()),
                ThreadEntry::new(A::SetResolved(!resolved), resolve_label, resolve_blocked),
                ThreadEntry::new(A::ToggleFolded, fold_label, None),
            ],
            vec![
                ThreadEntry::new(A::CopyLink, "Copy link", no_link.clone()),
                ThreadEntry::new(A::OpenOnForge, "Open on forge", no_link),
            ],
        ]
    }

    /// Why the viewer cannot resolve or unresolve `thread` from here.
    fn resolve_blocked(&self, thread: &Thread, cx: &App) -> Option<String> {
        let session = Session::global(cx).read(cx);
        let forge = session.pr(&self.key)?.pr.forge;
        if session.capabilities_of(&self.key, forge).resolve != ResolveMode::ReadWrite {
            return Some(format!(
                "{} does not let you resolve threads from here",
                forge.label()
            ));
        }
        (!thread.is_resolvable()).then(|| "This thread cannot be resolved".to_owned())
    }

    /// Does what `action` says.
    pub fn run(&self, action: ThreadAction, cx: &mut App) {
        let Some(thread) = self.thread(cx) else {
            return;
        };
        let writing = Session::global(cx).read(cx).writing_for(&self.key);
        match action {
            ThreadAction::ShowInConversation => show_in_conversation(&self.key, &thread.id, cx),
            ThreadAction::JumpToLine => jump_to_line(&self.key, &thread.id, true, cx),
            ThreadAction::Reply => {
                let Some(writing) = writing else {
                    return;
                };
                let anchor = reply_anchor(&thread);
                writing.update(cx, |writing, cx| writing.open(anchor, None, cx));
                // The reply is written under the thread's card in the diff.
                if !matches!(self.origin, Origin::Diff(_)) {
                    jump_to_line(&self.key, &thread.id, true, cx);
                }
            }
            ThreadAction::SetResolved(resolved) => {
                if let Some(writing) = writing {
                    writing.update(cx, |writing, cx| {
                        writing.set_resolved(&thread, resolved, cx);
                    });
                }
            }
            ThreadAction::ToggleFolded => self.toggle_folded(cx),
            ThreadAction::CopyLink => {
                if let Some(url) = self.link(cx) {
                    copy(&url, cx);
                }
            }
            ThreadAction::OpenOnForge => {
                if let Some(url) = self.link(cx).filter(|url| is_web_url(url)) {
                    cx.open_url(&url);
                }
            }
        }
    }

    fn toggle_folded(&self, cx: &mut App) {
        let folded = self.is_folded(cx);
        match &self.origin {
            Origin::Diff(cards) => {
                if let Some(cards) = cards.upgrade() {
                    cards.update(cx, |cards, cx| {
                        if let Some(ix) = cards.threads().iter().position(|t| t.id == self.thread) {
                            cards.toggle(ix, cx);
                        }
                    });
                }
            }
            Origin::Sidebar | Origin::Conversation => {
                ThreadNav::global(cx).update(cx, |nav, cx| {
                    nav.set_folded(&self.key, &self.thread, !folded, cx);
                });
            }
        }
    }

    /// The menu, built when it opens.
    pub fn popup(&self, menu: PopupMenu, cx: &App) -> PopupMenu {
        self.entries(cx)
            .into_iter()
            .enumerate()
            .fold(menu, |menu, (ix, group)| {
                let menu = if ix > 0 { menu.separator() } else { menu };
                group.into_iter().fold(menu, |menu, entry| {
                    let this = self.clone();
                    let action = entry.action;
                    menu.item(entry_item(
                        format!("thread-menu-{}", action.slug()),
                        entry.label,
                        entry.blocked,
                        move |cx| this.run(action, cx),
                    ))
                })
            })
    }
}

/// A menu entry with a `debug_selector`; a blocked one is disabled and
/// says why under its label.
pub(crate) fn entry_item(
    selector: String,
    label: &'static str,
    blocked: Option<String>,
    run: impl Fn(&mut App) + 'static,
) -> PopupMenuItem {
    match blocked {
        Some(why) => PopupMenuItem::element(move |_, cx| {
            let t = ui::theme(cx);
            let selector = format!("{selector}-blocked");
            v_flex()
                .debug_selector(move || selector.clone())
                .child(div().text_color(t.colors.text_faint).child(label))
                .child(
                    div()
                        .tiny(&t)
                        .text_color(t.colors.text_muted)
                        .child(why.clone()),
                )
        })
        .disabled(true),
        None => PopupMenuItem::element(move |_, _| {
            let selector = selector.clone();
            div().debug_selector(move || selector.clone()).child(label)
        })
        .on_click(move |_, _: &mut Window, cx| run(cx)),
    }
}

/// The comment menu's entries after a thread's, for a right-click on one
/// comment of it.
pub fn with_comment(
    thread: &ThreadMenu,
    comment: &CommentMenu,
    menu: PopupMenu,
    cx: &App,
) -> PopupMenu {
    comment.popup(thread.popup(menu, cx).separator())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_action_has_its_own_selector() {
        use ThreadAction as A;

        let actions = [
            A::ShowInConversation,
            A::JumpToLine,
            A::Reply,
            A::SetResolved(true),
            A::SetResolved(false),
            A::ToggleFolded,
            A::CopyLink,
            A::OpenOnForge,
        ];
        let mut slugs: Vec<&str> = actions.iter().map(|a| a.slug()).collect();
        slugs.sort_unstable();
        slugs.dedup();
        assert_eq!(slugs.len(), actions.len());
    }
}
