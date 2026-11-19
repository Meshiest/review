//! The dock panels: diff, conversation, checks and log.
//!
//! Panel content is a flat, opaque `surface` because text scrolls there; only
//! the header strip uses the `bar` material.

use forge::model::ForgeKind;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, Context, Div, Entity, EntityId, InteractiveElement as _, IntoElement,
    ParentElement, StatefulInteractiveElement as _, Styled, div, px,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::dock::{PanelInfo, PanelState, register_panel};
use gpui_kit::component::{Icon, h_flex};

pub use crate::checks_panel::ChecksPanel;
pub use crate::conversation::ConversationPanel;
pub use crate::diff_panel::{DIFF_KEY_CONTEXT, DiffPanel};
pub use crate::log_panel::LogPanel;
use crate::model::{PrKey, PrNumber, RepoRef};
use crate::session::{OpenPr, PanelEntry, Session};
use crate::settings_panel::{SETTINGS, SettingsPanel};
use crate::tab_menu::tab_handle;
use crate::terminal_panel::{TERMINAL, TerminalPanel};
use crate::terminals::TerminalSpec;
use crate::ui::{self, TextExt as _};

pub const DIFF: &str = "Diff";
pub const CONVERSATION: &str = "Conversation";
pub const CHECKS: &str = "Checks";
pub const LOG: &str = "Log";

/// Widest a tab's label grows; a longer one ellipsizes past it. The skin's
/// tab sizes to what it holds, so the cap lives on the label.
pub const TAB_TITLE_MAX: f32 = 240.0;

/// The dock panels the session knows by kind: one Diff, Conversation and
/// Checks per open pull request, and one Log that follows the focused one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PanelKind {
    Diff,
    Conversation,
    Checks,
    Log,
}

impl PanelKind {
    /// The name the dock persists the panel under, also its tab label.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Diff => DIFF,
            Self::Conversation => CONVERSATION,
            Self::Checks => CHECKS,
            Self::Log => LOG,
        }
    }
}

/// Whether the conversation composer starts with sample text, for screenshots.
#[derive(Clone, Copy, Default)]
pub struct DemoComposer(pub bool);

impl gpui::Global for DemoComposer {}

pub(crate) const DEMO_COMPOSER_TEXT: &str = "Nice split. Two things before this merges:\n\n- `rows.rs` rounds the row height per side, so the Windows job drifts by a pixel at 125% scale\n- could the hunk cache key include the whitespace option?\n\n```suggestion\nlet height = (line_height * rows).ceil();\n```";

/// How a saved layout builds each panel again. Every panel is wrapped in
/// its tab's controls ([`tab_handle`]), including the Settings and Terminal
/// panels, whose own modules register them unwrapped first.
pub fn register(cx: &mut App) {
    register_panel(cx, DIFF, |build, window, cx| {
        let pr = restore(build.info(), cx);
        tab_handle(DiffPanel::for_pr(pr, window, cx))
    });
    register_panel(cx, CONVERSATION, |build, window, cx| {
        let pr = restore(build.info(), cx);
        tab_handle(ConversationPanel::for_pr(pr, window, cx))
    });
    register_panel(cx, CHECKS, |build, window, cx| {
        let pr = restore(build.info(), cx);
        tab_handle(ChecksPanel::for_pr(pr, window, cx))
    });
    register_panel(cx, LOG, |_, window, cx| {
        tab_handle(LogPanel::new(window, cx))
    });
    crate::settings_panel::register(cx);
    crate::sidebar_panels::register(cx);
    crate::terminal_panel::register(cx);
    register_panel(cx, SETTINGS, |_, window, cx| {
        tab_handle(SettingsPanel::new(window, cx))
    });
    register_panel(cx, TERMINAL, |build, _, cx| {
        let spec = match build.info() {
            PanelInfo::Panel(value) => serde_json::from_value(value.clone()).unwrap_or_default(),
            _ => TerminalSpec::default(),
        };
        tab_handle(TerminalPanel::placeholder(spec, cx))
    });
}

/// The pull request a saved panel was bound to, opened again so the panel
/// has something to draw.
fn restore(info: &PanelInfo, cx: &mut App) -> Option<PrKey> {
    let PanelInfo::Panel(value) = info else {
        return None;
    };
    let repo: RepoRef = serde_json::from_value(value.get("repo")?.clone()).ok()?;
    let number = value.get("number")?.as_u64()?;
    let key = PrKey::new(repo, PrNumber(number));
    let session = Session::global(cx);
    session.update(cx, |session, cx| session.open(&key, cx));
    Some(key)
}

/// Which pull request a panel draws, and its entry in the session's panel
/// list. A bound panel draws its own pull request whatever has focus; an
/// unbound one (the Log, or a benchmark's fixed layout) follows the focus.
pub struct Binding {
    session: Entity<Session>,
    pr: Option<PrKey>,
    kind: PanelKind,
    id: EntityId,
}

impl Binding {
    /// Registers the panel being built in `cx` with the session.
    pub fn new<P: 'static>(pr: Option<PrKey>, kind: PanelKind, cx: &mut Context<P>) -> Self {
        let session = Session::global(cx);
        let id = cx.entity_id();
        session.update(cx, |session, _| {
            session.register_panel(PanelEntry {
                id,
                kind,
                pr: pr.clone(),
            });
        });
        Self {
            session,
            pr,
            kind,
            id,
        }
    }

    pub fn session(&self) -> &Entity<Session> {
        &self.session
    }

    /// The pull request it is bound to, if any.
    pub fn bound(&self) -> Option<&PrKey> {
        self.pr.as_ref()
    }

    /// The pull request it draws now.
    pub fn key(&self, cx: &App) -> Option<PrKey> {
        self.pr
            .clone()
            .or_else(|| self.session.read(cx).open_key().cloned())
    }

    /// The open pull request it draws now.
    pub fn open<'a>(&self, session: &'a Session) -> Option<&'a OpenPr> {
        match &self.pr {
            Some(key) => session.pr(key),
            None => session.open_pr(),
        }
    }

    /// Its tab came forward, or it was clicked: its pull request takes the
    /// focus.
    pub fn activated(&self, cx: &mut App) {
        if let Some(key) = self.pr.clone() {
            self.session
                .update(cx, |session, cx| session.focus(&key, cx));
        }
    }

    /// It left the dock.
    pub fn removed(&self, cx: &mut App) {
        let id = self.id;
        self.session
            .update(cx, |session, cx| session.panel_removed(id, cx));
    }

    /// What the layout saves: the panel's name and its pull request.
    pub fn dump(&self, name: &str) -> PanelState {
        let info = match &self.pr {
            Some(key) => serde_json::json!({ "repo": key.repo, "number": key.number.0 }),
            None => serde_json::Value::Null,
        };
        PanelState {
            panel_name: name.to_owned(),
            children: Vec::new(),
            info: PanelInfo::panel(info),
        }
    }

    /// The tab title: `repo#n Kind`, `repo!n` on GitLab, in italics while
    /// the pull request is in the preview tab, then `extra` and the close
    /// button. A double click keeps it.
    pub fn title(&self, extra: Option<AnyElement>, cx: &App) -> AnyElement {
        let label = self.label(cx);
        let close = close_button(self.id, close_selector(self.pr.as_ref(), self.kind), cx);
        // The dock's tab draws the arrow; a tab is pressed like a button.
        h_flex()
            .cursor_pointer()
            .gap(px(6.0))
            .items_center()
            .child(label)
            .children(extra)
            .child(close)
            .into_any_element()
    }

    fn label(&self, cx: &App) -> AnyElement {
        let session = self.session.read(cx);
        let Some(key) = self.pr.clone() else {
            return tab_label(div()).child(self.kind.name()).into_any_element();
        };
        let sigil = session
            .pr(&key)
            .map_or_else(|| ForgeKind::GitHub.sigil(), |open| open.pr.forge.sigil());
        let label = format!(
            "{}{sigil}{} {}",
            key.repo.name,
            key.number,
            self.kind.name()
        );
        let preview = session.is_preview(&key);
        let selector = tab_selector(&key, self.kind);
        let target = self.session.clone();
        tab_label(div().id("pr-tab-title"))
            .debug_selector(move || selector)
            .cursor_pointer()
            .when(preview, |el| el.italic())
            .on_click(move |event, _, cx| {
                if event.click_count() >= 2 {
                    target.update(cx, |session, cx| session.keep(&key, cx));
                }
            })
            .child(label)
            .into_any_element()
    }
}

/// A tab's label: as wide as its text up to [`TAB_TITLE_MAX`], then
/// ellipsized.
pub fn tab_label<E: Styled>(el: E) -> E {
    el.flex_shrink(1.0)
        .min_w_0()
        .max_w(px(TAB_TITLE_MAX))
        .overflow_hidden()
        .whitespace_nowrap()
        .text_ellipsis()
}

/// The `debug_selector` of a pull request panel's tab title.
#[must_use]
pub fn tab_selector(key: &PrKey, kind: PanelKind) -> String {
    format!("tab-{}-{}-{}", key.repo.name, key.number, kind.name())
}

/// The `debug_selector` of a panel's close button.
#[must_use]
pub fn close_selector(key: Option<&PrKey>, kind: PanelKind) -> String {
    match key {
        Some(key) => format!("close-{}", tab_selector(key, kind)),
        None => format!("close-{}", kind.name()),
    }
}

/// The close button at the end of a tab title. It stops the click so the
/// tab is not also selected.
pub fn close_button(id: EntityId, selector: String, cx: &App) -> AnyElement {
    let t = ui::theme(cx);
    let c = &t.colors;
    let hover = c.hover;
    let session = Session::global(cx);
    div()
        .id(("tab-close", id.as_u64()))
        .debug_selector(move || selector)
        .size(px(16.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(t.radius.sm)
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(move |_, _, cx| {
            cx.stop_propagation();
            session.update(cx, |session, cx| session.close_panel(id, cx));
        })
        .child(
            Icon::new(IconName::Close)
                .size(px(12.0))
                .text_color(c.text_muted),
        )
        .into_any_element()
}

/// The dock traits of a panel. With `spinners`, the panel's `spinners` field
/// follows whether its tab is displayed, so the shell turns its CI icons
/// only while it is on screen. With `bound`, the panel's `binding` field
/// focuses its pull request when its tab comes forward, tells the session
/// when it leaves the dock, and is what the layout saves.
macro_rules! dock_panel {
    ($ty:ident, $name:literal) => {
        $crate::panels::dock_panel!(@common $ty);

        impl ::gpui_kit::component::dock::BasePanel for $ty {
            fn panel_name(&self) -> &'static str {
                $name
            }
        }
    };
    ($ty:ident, $name:literal, bound $(, $spinners:ident)?) => {
        $crate::panels::dock_panel!(@common $ty);

        impl ::gpui_kit::component::dock::BasePanel for $ty {
            fn panel_name(&self) -> &'static str {
                $name
            }

            fn set_active(
                &mut self,
                active: bool,
                _: &mut ::gpui::Window,
                cx: &mut ::gpui::Context<Self>,
            ) {
                $( self.$spinners.set_shown(active); )?
                if active {
                    self.binding.activated(cx);
                }
                // Reaches the shell, which then asks for frames while the
                // panel's icons spin.
                cx.notify();
            }

            fn on_removed(&mut self, _: &mut ::gpui::Window, cx: &mut ::gpui::Context<Self>) {
                $( self.$spinners.set_shown(false); )?
                self.binding.removed(cx);
            }

            fn dump(&self, _: &::gpui::App) -> ::gpui_kit::component::dock::PanelState {
                self.binding.dump($name)
            }
        }
    };
    (@common $ty:ident) => {
        impl ::gpui::EventEmitter<::gpui_kit::component::dock::PanelEvent> for $ty {}

        impl ::gpui::Focusable for $ty {
            fn focus_handle(&self, _: &::gpui::App) -> ::gpui::FocusHandle {
                self.focus.clone()
            }
        }
    };
}

pub(crate) use dock_panel;

/// A header strip: the `bar` material, 32px, with a leading and trailing slot.
pub(crate) fn header(cx: &App, leading: impl IntoElement, trailing: impl IntoElement) -> Div {
    let t = ui::theme(cx);
    ui::bar(cx)
        .h(px(32.0))
        .flex_none()
        .flex()
        .px(px(12.0))
        .gap(px(12.0))
        .items_center()
        .justify_between()
        .small(&t)
        .text_color(t.colors.text_secondary)
        .child(leading)
        .child(trailing)
}
