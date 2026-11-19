//! The right-click menu of a comment: its link and text to the clipboard,
//! the comment on the forge, a reply, and resolving its thread.

use forge::model::{Comment, ForgeKind, PrNumber, RepoRef, Thread};
use gpui::{App, ClipboardItem, Entity};
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};

use crate::references;
use crate::untrusted::is_web_url;
use crate::writing::Writing;

/// The page of `comment`, on the forge: the address the forge reported, or
/// the permalink anchor of its kind on the pull request's page. `inline` is
/// a review comment, which GitHub anchors as `discussion_r<id>`. `None`
/// when neither is known (a GitHub node id has no number to anchor on).
#[must_use]
pub fn comment_url(
    forge: ForgeKind,
    repo: &RepoRef,
    number: PrNumber,
    comment: &Comment,
    inline: bool,
) -> Option<String> {
    if let Some(url) = comment.web_url.as_deref().filter(|url| is_web_url(url)) {
        return Some(url.to_owned());
    }
    let id = comment.id.as_str();
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let anchor = match (forge, inline) {
        (ForgeKind::GitHub, true) => format!("discussion_r{id}"),
        (ForgeKind::GitLab, _) => format!("note_{id}"),
        (ForgeKind::GitHub | ForgeKind::Forgejo, _) => format!("issuecomment-{id}"),
    };
    Some(format!(
        "{}#{anchor}",
        references::pr_url(forge, repo, number)
    ))
}

/// What a comment's menu acts on.
#[derive(Clone)]
pub struct CommentMenu {
    pub link: Option<String>,
    pub text: String,
    pub thread: Thread,
    pub writing: Entity<Writing>,
    /// The menu offers Reply: only where the thread has a place to write it.
    pub reply: bool,
    /// The forge lets the viewer resolve or unresolve the thread.
    pub resolve: bool,
}

impl CommentMenu {
    /// Puts the comment's link on the clipboard; false when it has none.
    pub fn copy_link(&self, cx: &mut App) -> bool {
        self.link.as_deref().map(|url| copy(url, cx)).is_some()
    }

    pub fn copy_text(&self, cx: &mut App) {
        copy(&self.text, cx);
    }

    /// Opens the comment on the forge, if its address is a web URL.
    pub fn open_on_forge(&self, cx: &mut App) {
        if let Some(url) = self.link.as_deref().filter(|url| is_web_url(url)) {
            cx.open_url(url);
        }
    }

    pub fn popup(&self, popup: PopupMenu) -> PopupMenu {
        let no_link = self.link.is_none();
        let (menu, text, open) = (self.clone(), self.clone(), self.clone());
        let mut popup = popup
            .item(
                PopupMenuItem::new("Copy link to comment")
                    .disabled(no_link)
                    .on_click(move |_, _, cx| {
                        menu.copy_link(cx);
                    }),
            )
            .item(PopupMenuItem::new("Copy text").on_click(move |_, _, cx| text.copy_text(cx)))
            .item(
                PopupMenuItem::new("Open comment on forge")
                    .disabled(no_link)
                    .on_click(move |_, _, cx| open.open_on_forge(cx)),
            );
        if self.reply {
            let (writing, thread) = (self.writing.clone(), self.thread.clone());
            popup =
                popup
                    .separator()
                    .item(PopupMenuItem::new("Reply").on_click(move |_, _, cx| {
                        let anchor = super::reply_anchor(&thread);
                        writing.update(cx, |writing, cx| writing.open(anchor.clone(), None, cx));
                    }));
        }
        if self.resolve {
            let resolved = self.thread.is_resolved();
            let (writing, thread) = (self.writing.clone(), self.thread.clone());
            let label = if resolved { "Unresolve" } else { "Resolve" };
            popup = popup.item(PopupMenuItem::new(label).on_click(move |_, _, cx| {
                writing.update(cx, |writing, cx| {
                    writing.set_resolved(&thread, !resolved, cx);
                });
            }));
        }
        popup
    }
}

/// Puts `text` on the clipboard.
pub fn copy(text: &str, cx: &mut App) {
    cx.write_to_clipboard(ClipboardItem::new_string(text.to_owned()));
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use forge::model::{CommentId, User};

    use super::*;

    fn comment(id: &str, web_url: Option<&str>) -> Comment {
        Comment {
            id: CommentId::new(id),
            author: User::new("mira"),
            body: "text".into(),
            created_at: Utc::now(),
            edited_at: None,
            web_url: web_url.map(str::to_owned),
            review: None,
            verdict: None,
            suggestions: Vec::new(),
        }
    }

    #[test]
    fn permalinks_follow_each_forges_anchors() {
        let repo = RepoRef::new("h.example", "acme", "api");
        let url =
            |forge, id, inline| comment_url(forge, &repo, PrNumber(7), &comment(id, None), inline);
        assert_eq!(
            url(ForgeKind::GitHub, "42", true).as_deref(),
            Some("https://h.example/acme/api/pull/7#discussion_r42")
        );
        assert_eq!(
            url(ForgeKind::GitHub, "42", false).as_deref(),
            Some("https://h.example/acme/api/pull/7#issuecomment-42")
        );
        assert_eq!(
            url(ForgeKind::GitLab, "42", true).as_deref(),
            Some("https://h.example/acme/api/-/merge_requests/7#note_42")
        );
        assert_eq!(
            url(ForgeKind::Forgejo, "42", false).as_deref(),
            Some("https://h.example/acme/api/pulls/7#issuecomment-42")
        );
        assert_eq!(url(ForgeKind::GitHub, "PRRC_kwDO", true), None);
    }

    #[test]
    fn the_forges_own_address_wins_when_it_is_a_web_url() {
        let repo = RepoRef::new("h.example", "acme", "api");
        let url = |web: &str| {
            comment_url(
                ForgeKind::GitHub,
                &repo,
                PrNumber(7),
                &comment("PRRC_kwDO", Some(web)),
                true,
            )
        };
        assert_eq!(
            url("https://h.example/acme/api/pull/7#discussion_r9").as_deref(),
            Some("https://h.example/acme/api/pull/7#discussion_r9")
        );
        assert_eq!(url("javascript:alert(1)"), None);
    }
}
