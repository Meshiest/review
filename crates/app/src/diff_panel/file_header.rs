//! The controls drawn over each file header of the diff, as GitHub has them:
//! after the path, copy its name and expand all its lines; after the viewed
//! checkbox, a comment on the whole file and the file's menu. The view
//! itself paints the chevron, the path, the line counts with their change
//! bar and the checkbox.

use std::sync::Arc;

use diff::{ContextMode, FileIx};
use diff_view::{ContextTarget, HeaderPlace, HeaderSlots};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, ClickEvent, Context, Div, Entity, InteractiveElement as _, IntoElement as _,
    ParentElement as _, Pixels, SharedString, Stateful, StatefulInteractiveElement as _,
    Styled as _, WeakEntity, div, px,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::Icon;
use gpui_kit::component::tooltip::Tooltip;

use super::DiffPanel;
use crate::model::PrKey;
use crate::session::Session;
use crate::ui;
use store::DraftAnchor;

/// The gap between two controls, and between the controls and what they
/// follow.
const GAP: f32 = 4.0;
/// How dim a control that cannot be used is drawn.
const DISABLED_OPACITY: f32 = 0.45;

/// The room the controls take in every header; the line counts and change
/// bar the view paints take `stats`.
pub const SLOTS: HeaderSlots = HeaderSlots {
    after_path: px(2.0 * ui::CONTROL_HEIGHT + 3.0 * GAP),
    stats: px(160.0),
    trailing: px(2.0 * ui::CONTROL_HEIGHT + 3.0 * GAP),
};

type Run = fn(&mut DiffPanel, FileIx, &ClickEvent, &mut Context<DiffPanel>);

/// What every header's controls draw from.
pub struct HeaderControls {
    pub panel: WeakEntity<DiffPanel>,
    pub session: Entity<Session>,
    pub key: PrKey,
    /// Each file's path, by [`FileIx`]: the panel is being updated while
    /// the headers draw, so it cannot be asked.
    pub paths: Arc<[String]>,
}

impl HeaderControls {
    /// The `debug_selector` of a control: `{name}-{file}`, with `pinned-`
    /// before it on the pinned header's copy.
    #[must_use]
    pub fn selector(name: &str, file: FileIx, pinned: bool) -> String {
        let pin = if pinned { "pinned-" } else { "" };
        format!("{pin}{name}-{}", file.0)
    }

    /// The four controls, each placed on its own: every element here is
    /// laid out again on every frame the header is in view.
    pub fn render(&self, place: HeaderPlace, cx: &mut App) -> AnyElement {
        let file = place.file;
        let comments = file_comments(&self.session, &self.key, cx);
        let commented = self
            .paths
            .get(file.0 as usize)
            .is_some_and(|path| has_comments(&self.session, &self.key, path, cx));
        let top = ((place.size.height - px(ui::CONTROL_HEIGHT)) / 2.0).round();
        let step = px(ui::CONTROL_HEIGHT + GAP);
        let lead = place.after_path + px(GAP);
        let trail = place.trailing + px(GAP);
        let all_lines = match place.context {
            ContextMode::Hunks => "Expand all lines",
            ContextMode::Full => "Show changes only",
        };
        let controls = [
            Control {
                name: "copy-name",
                left: lead,
                icon: IconName::Copy,
                tip: "Copy file name".into(),
                disabled: None,
                lit: false,
                run: |panel, file, _, cx| {
                    panel.copy_file_name(file, cx);
                },
            },
            Control {
                name: "all-lines",
                left: lead + step,
                icon: IconName::ChevronsUpDown,
                tip: all_lines.into(),
                disabled: None,
                lit: false,
                run: |panel, file, _, cx| panel.toggle_all_lines(file, cx),
            },
            Control {
                name: "comment-file",
                left: trail,
                icon: IconName::MessageSquare,
                tip: comments
                    .clone()
                    .err()
                    .unwrap_or_else(|| "Comment on this file".into()),
                disabled: comments.err(),
                lit: commented,
                run: |panel, file, _, cx| {
                    panel.comment_on_file(file, cx);
                },
            },
            Control {
                name: "file-menu",
                left: trail + step,
                icon: IconName::Ellipsis,
                tip: "More actions".into(),
                disabled: None,
                lit: false,
                run: |panel, file, event, cx| {
                    if let Some(path) = panel.path_of(file, cx) {
                        let target = ContextTarget::FileHeader { file, path };
                        panel.open_menu(event.position(), target, cx);
                    }
                },
            },
        ];
        div()
            .relative()
            .w(place.size.width)
            .h(place.size.height)
            .children(controls.map(|control| {
                let panel = self.panel.clone();
                let (run, enabled) = (control.run, control.disabled.is_none());
                let selector = Self::selector(control.name, file, place.pinned);
                button(control, selector, cx)
                    .absolute()
                    .top(top)
                    .when(enabled, |el| {
                        el.on_click(move |event, _, cx| {
                            cx.stop_propagation();
                            panel
                                .update(cx, |panel, cx| run(panel, file, event, cx))
                                .ok();
                        })
                    })
            }))
            .into_any_element()
    }
}

/// One control of a header.
struct Control {
    name: &'static str,
    left: Pixels,
    icon: IconName,
    tip: SharedString,
    /// Why it cannot be used; its tooltip says so and it takes no click.
    disabled: Option<SharedString>,
    /// Drawn in the accent color: the file has comments or drafts.
    lit: bool,
    run: Run,
}

/// Whether the forge of `key` takes comments on a whole file; the reason
/// when it does not.
pub fn file_comments(session: &Entity<Session>, key: &PrKey, cx: &App) -> Result<(), SharedString> {
    let session = session.read(cx);
    let Some(open) = session.pr(key) else {
        return Err("The pull request is not open".into());
    };
    match session.capabilities_for(key) {
        Some(caps) if caps.file_comments => Ok(()),
        _ => Err(format!("{} has no comments on a whole file", open.pr.forge.label()).into()),
    }
}

/// Whether `path` has a thread, an unsent draft or a queued comment on it,
/// on either side of a rename.
fn has_comments(session: &Entity<Session>, key: &PrKey, path: &str, cx: &App) -> bool {
    let session = session.read(cx);
    let threads = session.pr(key).is_some_and(|open| {
        open.threads.iter().any(|thread| {
            thread.anchor.as_ref().is_some_and(|anchor| {
                anchor.path == path || anchor.old_path.as_deref() == Some(path)
            })
        })
    });
    let on_file = |anchor: &DraftAnchor| match anchor {
        DraftAnchor::Line { path: at, .. } | DraftAnchor::File { path: at } => at == path,
        _ => false,
    };
    threads
        || session.writing_for(key).is_some_and(|writing| {
            let writing = writing.read(cx);
            writing
                .slots()
                .iter()
                .any(|slot| on_file(&slot.anchor) && !slot.body.trim().is_empty())
                || writing
                    .queued()
                    .iter()
                    .any(|queued| on_file(&queued.anchor))
        })
}

/// A square icon button with a tooltip, in the toolbar style. Its press
/// never reaches the header under it, which folds the file. A disabled one
/// is dimmed; the caller gives it no click handler.
fn button(control: Control, selector: String, cx: &App) -> Stateful<Div> {
    let t = ui::theme(cx);
    let tip = control.tip;
    ui::secondary_style(div().id(control.name), cx)
        .left(control.left)
        .w(px(ui::CONTROL_HEIGHT))
        .block_mouse_except_scroll()
        .debug_selector(move || selector)
        .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
        .child(
            Icon::new(control.icon)
                .size(px(ui::CONTROL_ICON))
                .text_color(if control.lit {
                    t.colors.accent
                } else {
                    t.colors.text_secondary
                }),
        )
        .map(|el| match control.disabled {
            Some(_) => el.opacity(DISABLED_OPACITY),
            None => el.cursor_pointer(),
        })
}
