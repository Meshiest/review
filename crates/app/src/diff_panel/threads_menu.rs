//! The header's "N open threads" chip and its menu: one entry per open
//! thread, each jumping to its card in the diff the way `]t` does. A thread
//! on a file this diff does not have is listed, disabled.

use forge::model::{Thread, ThreadId};
use gpui::{
    AnyElement, App, Entity, FocusHandle, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, Styled as _, div, px,
};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};

use super::drafts_menu::{excerpt, row};
use crate::session::Session;
use crate::threads::{ThreadCards, line_label};
use crate::ui::{self, MenuTrigger};

/// One open thread as its menu entry shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ThreadEntry {
    pub id: ThreadId,
    /// Where it is: `src/a.rs  line 31`.
    pub place: SharedString,
    /// Who started it and the first line of what they wrote.
    pub excerpt: SharedString,
    /// Its card is in the diff, so the entry can jump to it.
    pub shown: bool,
}

/// Whether `thread` is counted and listed: on code, and not resolved.
pub(crate) fn is_open(thread: &Thread) -> bool {
    thread.anchor.is_some() && !thread.is_resolved()
}

/// The open threads, in the order the forge lists them.
pub(crate) fn entries(cards: &ThreadCards) -> Vec<ThreadEntry> {
    cards
        .threads()
        .iter()
        .enumerate()
        .filter(|(_, thread)| is_open(thread))
        .map(|(ix, thread)| ThreadEntry {
            id: thread.id.clone(),
            place: place(thread),
            excerpt: first_comment(thread),
            shown: cards.block_of(ix).is_some(),
        })
        .collect()
}

/// `src/a.rs  line 31`, marked when the thread is outdated.
fn place(thread: &Thread) -> SharedString {
    let path = thread
        .anchor
        .as_ref()
        .map_or("", |anchor| anchor.path.as_str());
    let mut place = match line_label(thread) {
        Some(line) => format!("{path}  {line}"),
        None => path.to_owned(),
    };
    if thread.outdated {
        place.push_str("  (outdated)");
    }
    place.into()
}

/// `login: first line`, cut short.
fn first_comment(thread: &Thread) -> SharedString {
    let Some(first) = thread.comments.first() else {
        return SharedString::default();
    };
    excerpt(&format!(
        "{}: {}",
        first.author.login,
        first_line(&first.body)
    ))
}

fn first_line(body: &str) -> &str {
    body.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
}

/// The "N open threads" chip. A click, or Enter or Space while it has the
/// focus, opens the menu of open threads; picking one scrolls the diff to it
/// and Back returns.
pub(super) fn chip(
    count: usize,
    focus: &FocusHandle,
    cards: Entity<ThreadCards>,
    session: Entity<Session>,
    cx: &App,
) -> AnyElement {
    let t = ui::theme(cx);
    let c = &t.colors;
    let label = format!("{count} open thread{}", if count == 1 { "" } else { "s" });
    let ring = c.focus;
    let base = div()
        .id("threads-chip")
        .debug_selector(|| "threads-chip".into())
        .flex_none()
        // A ring in the bar's color, the focus color while it has the focus.
        .border_1()
        .border_color(c.surface)
        .rounded(t.radius.md)
        .cursor_pointer()
        .track_focus(focus)
        .focus_visible(move |s| s.border_color(ring))
        .child(ui::chip(cx, label, c.info));
    MenuTrigger::new(base)
        .dropdown_menu_with_anchor(gpui::Anchor::TopRight, move |menu, _, cx| {
            let menu = menu.min_w(px(240.0)).max_w(px(420.0));
            entries(cards.read(cx))
                .into_iter()
                .enumerate()
                .fold(menu, |menu, (ix, entry)| {
                    let session = session.clone();
                    let ThreadEntry {
                        id,
                        place,
                        excerpt,
                        shown,
                    } = entry;
                    let selector = SharedString::from(format!("thread-entry-{ix}"));
                    menu.item(
                        PopupMenuItem::element(move |_, cx| {
                            row(selector.clone(), &place, &excerpt, cx)
                        })
                        .disabled(!shown)
                        .on_click(move |_, _, cx| {
                            let id = id.clone();
                            session.update(cx, |session, cx| session.jump_to_thread(id, true, cx));
                        }),
                    )
                })
        })
        .into_any_element()
}
