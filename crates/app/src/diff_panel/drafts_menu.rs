//! The header's "N drafts" chip and its menu: one entry per draft comment
//! not yet in the review, each jumping to where it is written.

use forge::model::Thread;
use gpui::{
    AnyElement, App, Div, Entity, FocusHandle, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, Styled as _, div, px,
};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::v_flex;
use store::DraftAnchor;

use crate::model::PrKey;
use crate::session::Session;
use crate::ui::{self, MenuTrigger, TextExt as _};
use crate::writing::{Writing, anchor_label};

/// How many characters of a draft's first line its entry shows.
const EXCERPT_CHARS: usize = 60;

/// One draft as its menu entry shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DraftEntry {
    pub anchor: DraftAnchor,
    /// Where it is: `src/a.rs  line 31`, `Conversation`.
    pub place: SharedString,
    /// Its first line of text, cut short.
    pub excerpt: SharedString,
}

/// The drafts with text in them, in the order they were started.
pub(crate) fn entries(writing: &Writing, threads: &[Thread]) -> Vec<DraftEntry> {
    writing
        .slots()
        .iter()
        .filter(|slot| !slot.body.trim().is_empty())
        .map(|slot| DraftEntry {
            anchor: slot.anchor.clone(),
            place: place(&slot.anchor, writing, threads),
            excerpt: excerpt(&slot.body),
        })
        .collect()
}

/// Where `anchor` is, in words.
fn place(anchor: &DraftAnchor, writing: &Writing, threads: &[Thread]) -> SharedString {
    match anchor {
        DraftAnchor::Line { .. } | DraftAnchor::File { .. } => {
            anchor_label(anchor).unwrap_or_default()
        }
        DraftAnchor::Reply { thread } => {
            let on = threads
                .iter()
                .find(|t| t.id == *thread)
                .and_then(|t| t.anchor.as_ref())
                .map(|at| match at.position.as_ref().or(at.original.as_ref()) {
                    Some(position) => format!("{}  line {}", at.path, position.line.line),
                    None => at.path.clone(),
                });
            match on {
                Some(on) => format!("Reply on {on}").into(),
                None => "Reply".into(),
            }
        }
        DraftAnchor::Edit { queued } => writing
            .queued()
            .iter()
            .find(|q| q.id == *queued)
            .and_then(|q| anchor_label(&q.anchor))
            .map_or_else(
                || "Editing a queued comment".into(),
                |label| format!("Editing {label}").into(),
            ),
        DraftAnchor::Conversation => "Conversation".into(),
        DraftAnchor::Review => "Review summary".into(),
    }
}

/// The first line with text in it, cut to [`EXCERPT_CHARS`].
fn excerpt(body: &str) -> SharedString {
    let line = body
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    match line.char_indices().nth(EXCERPT_CHARS) {
        Some((cut, _)) => format!("{}\u{2026}", line[..cut].trim_end()).into(),
        None => line.to_owned().into(),
    }
}

/// The "N drafts" chip. A click, or Enter or Space while it has the focus,
/// opens the menu of drafts; picking one jumps to it and Back returns.
pub(super) fn chip(
    count: usize,
    focus: &FocusHandle,
    key: PrKey,
    writing: Entity<Writing>,
    session: Entity<Session>,
    cx: &App,
) -> AnyElement {
    let t = ui::theme(cx);
    let c = &t.colors;
    let label = format!("{count} draft{}", if count == 1 { "" } else { "s" });
    let ring = c.focus;
    let base = div()
        .id("drafts-chip")
        .debug_selector(|| "drafts-chip".into())
        .flex_none()
        // A ring in the bar's color, the focus color while it has the focus.
        .border_1()
        .border_color(c.surface)
        .rounded(t.radius.md)
        .cursor_pointer()
        .track_focus(focus)
        .focus_visible(move |s| s.border_color(ring))
        .child(ui::chip(cx, label, c.accent_fg));
    MenuTrigger::new(base)
        .dropdown_menu_with_anchor(gpui::Anchor::TopRight, move |menu, _, cx| {
            let threads = session
                .read(cx)
                .pr(&key)
                .map(|open| open.threads.clone())
                .unwrap_or_default();
            let entries = entries(writing.read(cx), &threads);
            let menu = menu.min_w(px(240.0)).max_w(px(420.0));
            entries
                .into_iter()
                .enumerate()
                .fold(menu, |menu, (ix, entry)| {
                    let (session, key) = (session.clone(), key.clone());
                    let DraftEntry {
                        anchor,
                        place,
                        excerpt,
                    } = entry;
                    menu.item(
                        PopupMenuItem::element(move |_, cx| row(ix, &place, &excerpt, cx))
                            .on_click(move |_, _, cx| {
                                let (key, anchor) = (key.clone(), anchor.clone());
                                session.update(cx, |session, cx| {
                                    session.jump_to_draft(key, anchor, cx);
                                });
                            }),
                    )
                })
        })
        .into_any_element()
}

/// An entry: where the draft is, and its first line under it.
fn row(ix: usize, place: &SharedString, excerpt: &SharedString, cx: &App) -> Div {
    let t = ui::theme(cx);
    v_flex()
        .debug_selector(move || format!("draft-entry-{ix}"))
        .w_full()
        .min_w_0()
        .py(px(2.0))
        .child(div().truncate().child(place.clone()))
        .child(
            div()
                .truncate()
                .small(&t)
                .text_color(t.colors.text_muted)
                .child(excerpt.clone()),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_excerpt_is_the_first_line_with_text_cut_short() {
        assert_eq!(excerpt("\n  Off by one?\nmore"), "Off by one?");
        assert_eq!(excerpt("   "), "");
        let long = "word ".repeat(30);
        let cut = excerpt(&long);
        assert!(cut.ends_with('\u{2026}'), "{cut}");
        assert!(cut.chars().count() <= EXCERPT_CHARS + 1, "{cut}");
    }

    #[test]
    fn places_name_the_conversation_and_fall_back_for_unknown_threads() {
        let writing = Writing::with_log_sink();
        let reply = DraftAnchor::Reply {
            thread: forge::model::ThreadId::new("gone"),
        };
        assert_eq!(place(&reply, &writing, &[]), "Reply");
        assert_eq!(
            place(&DraftAnchor::Conversation, &writing, &[]),
            "Conversation"
        );
        assert_eq!(
            place(&DraftAnchor::Edit { queued: 7 }, &writing, &[]),
            "Editing a queued comment"
        );
    }
}
