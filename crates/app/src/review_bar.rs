//! The Diff header's suggestion bar: applying the batched suggestions, and
//! confirming the changes an apply would make. The review itself is
//! submitted from the action bar's review popover.

use forge::Capabilities;
use gpui::{
    Context, Entity, FontWeight, Hsla, InteractiveElement as _, IntoElement, ParentElement, Render,
    SharedString, StatefulInteractiveElement as _, Styled, Subscription, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_kit::component::{h_flex, v_flex};

use crate::model::PrKey;
use crate::session::{ApplyState, Session};
use crate::sink::Verdict;
use crate::suggestions::{self, Change, Piece, Removed};
use crate::ui::{self, Button, ButtonKind, TextExt as _};

/// Tallest the confirmation's change list grows before it scrolls.
const CONFIRM_MAX_HEIGHT: f32 = 260.0;

/// How dim a button that cannot be pressed is drawn.
const DISABLED_OPACITY: f32 = 0.45;

/// Gutter mark of a row that continues the line above.
const CONTINUATION: &str = "\u{21B3}";

/// Shown where the lines being replaced cannot be trusted.
pub const NOT_LOADED: &str = "current lines not loaded, reload the pull request";

/// The verdicts `caps` lets the reviewer choose, in display order.
#[must_use]
pub fn verdicts(caps: &Capabilities) -> Vec<Verdict> {
    Verdict::ALL
        .into_iter()
        .filter(|verdict| *verdict != Verdict::RequestChanges || caps.request_changes)
        .collect()
}

pub struct ReviewBar {
    /// The pull request it reviews; `None` until one is shown.
    pr: Option<PrKey>,
    session: Entity<Session>,
    _session: Subscription,
}

impl ReviewBar {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let session = Session::global(cx);
        Self {
            pr: None,
            _session: cx.observe(&session, |_, _, cx| cx.notify()),
            session,
        }
    }

    /// Shows `pr`'s suggestions from now on.
    pub fn set_pr(&mut self, pr: Option<PrKey>, cx: &mut Context<Self>) {
        self.pr = pr;
        cx.notify();
    }
}

impl Render for ReviewBar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let session = self.session.read(cx);
        let open = self.pr.as_ref().and_then(|key| session.pr(key));
        let (batch, apply) = open
            .map(|open| (open.batch.clone(), open.apply.clone()))
            .unwrap_or_default();
        let changes = self
            .pr
            .as_ref()
            .map(|key| session.confirm_changes_for(key))
            .unwrap_or_default();
        let confirming = !changes.is_empty();
        if batch.is_empty() && apply.is_none() && !confirming {
            return div().into_any_element();
        }
        let t = ui::theme(cx);
        let suggestions =
            suggestion_row(&self.session, self.pr.as_ref(), &batch, apply.as_ref(), &t);
        let confirmation = self.pr.as_ref().filter(|_| confirming).map(|key| {
            let local = open.is_some_and(|open| {
                open.confirm
                    .as_ref()
                    .is_some_and(|confirm| !confirm.local_edits().is_empty())
            });
            confirmation(&self.session, key, &changes, local, &t)
        });
        v_flex()
            .flex_none()
            .gap(px(6.0))
            .px(px(12.0))
            .py(px(8.0))
            .bg(t.colors.surface_raised)
            .border_b_1()
            .border_color(t.colors.border)
            .when(!batch.is_empty() || apply.is_some(), |el| {
                el.child(suggestions)
            })
            .children(confirmation)
            .into_any_element()
    }
}

/// "Apply N suggestions" for the batch, and how the last apply went.
fn suggestion_row(
    session: &gpui::Entity<Session>,
    pr: Option<&PrKey>,
    batch: &[crate::suggestions::SuggestionKey],
    apply: Option<&ApplyState>,
    t: &theme::Theme,
) -> gpui::Div {
    let c = &t.colors;
    let running = matches!(apply, Some(ApplyState::Running(_)));
    let status = match apply {
        Some(ApplyState::Running(n)) => Some((format!("Applying {n} suggestions"), c.text_muted)),
        Some(ApplyState::Applied(n)) => {
            Some((format!("Applied {n}; reading the pull request again"), c.ok))
        }
        Some(ApplyState::Failed(error)) => Some((error.to_string(), c.danger)),
        None => None,
    };
    let keys = batch.to_vec();
    let target = session.clone();
    let pr = pr.cloned();
    h_flex()
        .gap(px(10.0))
        .items_center()
        .when(!batch.is_empty(), |row| {
            row.child(
                Button::new(
                    "apply-batch",
                    if batch.len() == 1 {
                        "Review 1 suggestion".to_owned()
                    } else {
                        format!("Review {} suggestions", batch.len())
                    },
                    ButtonKind::Secondary,
                )
                .on_click(move |_, _, cx| {
                    if !running && let Some(pr) = &pr {
                        target.update(cx, |session, cx| session.request_apply(pr, &keys, cx));
                    }
                }),
            )
        })
        .when_some(status, |row, (text, color)| {
            row.child(div().small(t).truncate().text_color(color).child(text))
        })
}

/// The exact changes a click on Apply would make, as a small diff, and the
/// button that makes them. Nothing is applied from anywhere else.
fn confirmation(
    session: &Entity<Session>,
    pr: &PrKey,
    changes: &[Change],
    local: bool,
    t: &theme::Theme,
) -> gpui::Div {
    let c = &t.colors;
    let (apply, cancel) = (session.clone(), session.clone());
    let (apply_key, cancel_key) = (pr.clone(), pr.clone());
    let label = match (local, changes.len()) {
        (true, 1) => "Commit and push".to_owned(),
        (true, n) => format!("Commit and push {n} changes"),
        (false, 1) => "Apply".to_owned(),
        (false, n) => format!("Apply {n} changes"),
    };
    let verified = suggestions::all_verified(changes);
    v_flex()
        .gap(px(6.0))
        .p(px(8.0))
        .rounded(t.radius.md)
        .border_1()
        .border_color(c.border_strong)
        .bg(c.surface)
        .child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(c.text)
                .child(if local {
                    "This will be committed and pushed to the pull request branch"
                } else {
                    "This will be applied by the forge"
                }),
        )
        .child(
            v_flex()
                .id("apply-confirm-changes")
                .gap(px(8.0))
                .max_h(px(CONFIRM_MAX_HEIGHT))
                .overflow_y_scroll()
                .children(changes.iter().map(|change| change_diff(change, t))),
        )
        .child(
            h_flex()
                .gap(px(6.0))
                .child(if verified {
                    Button::new("apply-confirm", label, ButtonKind::Primary)
                        .on_click(move |_, _, cx| {
                            apply.update(cx, |session, cx| session.confirm_apply(&apply_key, cx));
                        })
                        .into_any_element()
                } else {
                    // Disabled: no handler, dimmed, and the session refuses
                    // the call anyway.
                    div()
                        .id("apply-confirm-disabled")
                        .opacity(DISABLED_OPACITY)
                        .child(Button::new("apply-confirm", label, ButtonKind::Primary))
                        .into_any_element()
                })
                .child(
                    Button::new("apply-cancel", "Cancel", ButtonKind::Ghost).on_click(
                        move |_, _, cx| {
                            cancel.update(cx, |session, cx| session.cancel_apply(&cancel_key, cx))
                        },
                    ),
                ),
        )
}

/// One change: the file and line range, then the lines removed and added.
/// Where the lines now in the file cannot be trusted, a notice stands in
/// for them.
fn change_diff(change: &Change, t: &theme::Theme) -> gpui::Div {
    let c = &t.colors;
    let range = if change.first == change.last {
        format!("{}:{}", change.path, change.first)
    } else {
        format!("{}:{}-{}", change.path, change.first, change.last)
    };
    let removed = match &change.removed {
        Removed::Lines(lines) => lines
            .iter()
            .flat_map(|line| line_rows("-", line, c.diff_remove_bg, c.diff_remove_gutter, t))
            .collect(),
        Removed::Loading | Removed::NotLoaded => Vec::new(),
    };
    let notice = match &change.removed {
        Removed::Lines(_) => None,
        Removed::Loading => Some(("reading the current lines", c.text_muted)),
        Removed::NotLoaded => Some((NOT_LOADED, c.danger)),
    };
    let added = change
        .added
        .iter()
        .flat_map(|line| line_rows("+", line, c.diff_add_bg, c.diff_add_gutter, t));
    v_flex()
        .gap(px(2.0))
        .child(div().small(t).text_color(c.text_muted).child(range))
        .child(
            v_flex()
                .mono(t)
                .rounded(t.radius.sm)
                .overflow_hidden()
                .border_1()
                .border_color(c.border)
                .children(removed)
                .when_some(notice, |el, (text, color)| {
                    el.child(div().px(px(6.0)).text_color(color).child(text))
                })
                .children(added)
                .when(change.added.is_empty(), |el| {
                    el.child(
                        div()
                            .px(px(6.0))
                            .text_color(c.text_muted)
                            .child("the lines are deleted"),
                    )
                }),
        )
}

/// A line as one or more rows: the sign in the gutter on the first, a
/// continuation marker on the rest, so a long line wraps where it is and
/// nothing runs out of the card. Characters that would not show as
/// themselves, and trailing whitespace, are chips with their code points.
pub(crate) fn line_rows(
    sign: &'static str,
    line: &str,
    background: Hsla,
    gutter: Hsla,
    t: &theme::Theme,
) -> Vec<gpui::Div> {
    let c = &t.colors;
    let chip = t.chip_colors(c.text_muted);
    suggestions::rows(line, suggestions::ROW_CHARS)
        .into_iter()
        .enumerate()
        .map(|(ix, row)| {
            let pieces = row.into_iter().map(|piece| match piece {
                Piece::Text(text) => div()
                    .min_w_0()
                    .text_color(c.text)
                    .child(SharedString::from(text)),
                Piece::Mark(mark) => div()
                    .flex_none()
                    .mx(px(1.0))
                    .px(px(3.0))
                    .rounded(t.radius.sm)
                    .border_1()
                    .border_color(chip.border)
                    .bg(chip.fill)
                    .text_color(chip.text)
                    .whitespace_nowrap()
                    .child(SharedString::from(mark)),
            });
            h_flex()
                .bg(background)
                .items_start()
                .child(
                    div()
                        .w(px(20.0))
                        .flex_none()
                        .px(px(6.0))
                        .bg(gutter)
                        .text_color(if ix == 0 { c.text } else { c.text_muted })
                        .child(if ix == 0 { sign } else { CONTINUATION }),
                )
                .child(
                    h_flex()
                        .flex_1()
                        .min_w_0()
                        .flex_wrap()
                        .px(px(6.0))
                        .children(pieces),
                )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge::model::ForgeKind;

    #[test]
    fn every_forge_offers_comment_and_approve() {
        for kind in [ForgeKind::GitHub, ForgeKind::GitLab, ForgeKind::Forgejo] {
            let choices = verdicts(&Capabilities::baseline(kind));
            assert!(choices.contains(&Verdict::Comment), "{kind:?}");
            assert!(choices.contains(&Verdict::Approve), "{kind:?}");
        }
    }

    #[test]
    fn request_changes_is_hidden_where_the_forge_lacks_it() {
        let mut caps = Capabilities::baseline(forge::model::ForgeKind::GitHub);
        assert!(verdicts(&caps).contains(&Verdict::RequestChanges));
        caps.request_changes = false;
        assert_eq!(verdicts(&caps), [Verdict::Comment, Verdict::Approve]);
    }
}
