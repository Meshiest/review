//! Refetching the inbox by hand: a button in the inbox's tab, and F5 or
//! Ctrl+R while the inbox has the keys. Every section is asked again at
//! once, without waiting for the next poll; rows are patched in place, so
//! the selection and each section's scroll stay. The icon turns while the
//! forges answer, and its tooltip says when each one last did and how.

use chrono::Utc;
use gpui::{
    AnyElement, App, Context, InteractiveElement as _, IntoElement, KeyBinding, MouseButton,
    ParentElement, StatefulInteractiveElement as _, Styled, div, prelude::FluentBuilder as _, px,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::{Icon, tooltip::Tooltip};
use theme::Theme;

use crate::model::relative_time;
use crate::session::{ForgeState, ForgeStatus, InboxRefresh, Session};
use crate::sidebar::Sidebar;

/// The inbox panel's own key context, beside the sidebar's.
pub const INBOX_KEYS: &str = "Inbox";

gpui::actions!(
    inbox,
    [
        /// `F5` or `Ctrl+R` in the inbox: refetches every section now.
        Refresh,
    ]
);

/// The inbox's key bindings.
pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("f5", Refresh, Some(INBOX_KEYS)),
        KeyBinding::new("ctrl-r", Refresh, Some(INBOX_KEYS)),
    ]);
}

/// Refetches every inbox section now, if something can.
pub fn refresh(cx: &mut App) {
    Session::global(cx).update(cx, |session, cx| session.refresh_inbox(cx));
}

/// The refresh button's tooltip for `session`.
#[must_use]
pub fn tooltip_text(session: &Session) -> String {
    tooltip_for(&session.forges, &session.inbox_refresh)
}

/// What to do, then one line per forge: when it last answered a refresh by
/// hand, and any trouble it has.
fn tooltip_for(forges: &[ForgeStatus], refresh: &InboxRefresh) -> String {
    let now = Utc::now();
    let mut lines = vec![if refresh.running {
        "Refreshing the inbox".to_owned()
    } else {
        "Refresh the inbox (F5)".to_owned()
    }];
    for forge in forges {
        let last = refresh
            .last
            .iter()
            .find(|(host, _)| *host == forge.host)
            .map(|(_, at)| {
                let minutes = u32::try_from((now - *at).num_minutes().max(0)).unwrap_or(u32::MAX);
                match relative_time(minutes).as_str() {
                    "now" => "refreshed just now".to_owned(),
                    ago => format!("refreshed {ago} ago"),
                }
            })
            .unwrap_or_else(|| "not refreshed by hand yet".to_owned());
        let state = match &forge.state {
            ForgeState::AuthError(why) | ForgeState::Error(why) => {
                format!(", {}: {why}", forge.state.label())
            }
            ForgeState::RateLimited => ", rate limited".to_owned(),
            ForgeState::Connecting | ForgeState::Ok => String::new(),
        };
        lines.push(format!("{}: {last}{state}", forge.host));
    }
    lines.join("\n")
}

impl Sidebar {
    /// The refresh button in the inbox's tab; none when nothing can refetch
    /// the inbox. While it runs, its icon is a spinner slot the shell turns.
    pub(crate) fn refresh_button(&self, t: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let session = self.session.read(cx);
        if !session.can_refresh_inbox() {
            return None;
        }
        let running = session.inbox_refresh.running;
        let tip = tooltip_text(session);
        let c = &t.colors;
        let hover = c.hover;
        let icon = if running {
            self.inbox.refresh_slots.clear();
            self.inbox
                .refresh_slots
                .slot(13.0, c.text_muted)
                .into_any_element()
        } else {
            Icon::new(IconName::RefreshCw)
                .size(px(13.0))
                .text_color(c.text_muted)
                .into_any_element()
        };
        Some(
            div()
                .id("inbox-refresh")
                .debug_selector(|| "inbox-refresh".into())
                .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                .size(px(18.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(t.radius.sm)
                .cursor_pointer()
                .hover(move |s| s.bg(hover))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(|_, _, cx| {
                    cx.stop_propagation();
                    refresh(cx);
                })
                .when(running, |el| el.bg(c.selected))
                .child(icon)
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tooltip_names_each_forge_and_its_trouble() {
        let mut forges = crate::fixtures::forge_statuses();
        forges[1].state = ForgeState::Error("timed out".into());
        let lines: Vec<String> = {
            let refresh = InboxRefresh {
                running: false,
                last: vec![(forges[0].host.clone(), Utc::now())],
            };
            let text = tooltip_for(&forges, &refresh);
            text.lines().map(str::to_owned).collect()
        };
        assert_eq!(lines.len(), 4);
        assert!(lines[1].ends_with("refreshed just now"), "{lines:?}");
        assert!(lines[2].contains("not refreshed") && lines[2].ends_with("offline: timed out"));
    }
}
