//! The end of the Conversation, as GitHub ends it: after the timeline, the
//! merge box (how the checks stand, with Show for the job list; whether it
//! can merge, or what blocks it; Merge with its method), then the comment
//! box with "Close pull request" on the left and "Comment" on the right.
//! Both scroll with the timeline. The writes go through the same session
//! and composer paths as the header's buttons.

use std::rc::Rc;

use forge::model::{CiState, MergeBlocker, PrState};
use gpui::{
    AnyElement, App, Entity, FontWeight, InteractiveElement as _, IntoElement, ParentElement,
    SharedString, StatefulInteractiveElement as _, Styled, WeakEntity, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::{Icon, h_flex, v_flex};
use theme::Theme;

use super::ConversationPanel;
use super::ci_box::{self, JobRow};
use crate::checks_panel::job_menu::JobMenu;
use crate::composer::{Composer, Leading};
use crate::lifecycle::{Action, Load, blocker_label, method_label};
use crate::model::PrKey;
use crate::pr_header::{Facts, method_id};
use crate::session::{OpenPr, Session};
use crate::ui::spinner::SpinnerSlots;
use crate::ui::{self, Button, ButtonKind, CI_HEADER, CI_ROW, TextExt as _, ci_color, ci_icon};

/// What the footer shows open, kept on the panel.
#[derive(Clone, Debug, Default)]
pub(super) struct FooterState {
    /// The job list under the checks summary.
    pub checks_open: bool,
    /// The merge methods under the Merge button.
    pub methods_open: bool,
    /// The action the footer asked for: its confirmation shows here, not
    /// in the header.
    pub asked: Option<Action>,
}

impl ConversationPanel {
    /// Asks for `action` from the footer, which then draws its confirmation.
    fn ask(&mut self, key: &PrKey, action: Action, cx: &mut gpui::Context<Self>) {
        self.footer.asked = Some(action.clone());
        self.footer.methods_open = false;
        self.session.update(cx, |session, cx| {
            session.focus(key, cx);
            session.request_action(key, action, cx);
        });
        cx.notify();
    }

    /// Whether the pending confirmation is the footer's to draw; forgets
    /// the footer's request once nothing waits.
    pub(super) fn sync_footer(&mut self, confirm: Option<&Action>) -> bool {
        match confirm {
            Some(pending) => self.footer.asked.as_ref() == Some(pending),
            None => {
                self.footer.asked = None;
                false
            }
        }
    }
}

/// Edits the panel's footer state from a click.
fn edit(panel: &WeakEntity<ConversationPanel>, cx: &mut App, f: impl FnOnce(&mut FooterState)) {
    panel
        .update(cx, |panel, cx| {
            f(&mut panel.footer);
            cx.notify();
        })
        .ok();
}

/// What the footer draws from, read once a render.
pub(super) struct Footer<'a> {
    pub open: &'a OpenPr,
    pub facts: Option<&'a Facts>,
    pub state: &'a FooterState,
    /// The pending confirmation is the footer's.
    pub confirm_here: bool,
    pub panel: WeakEntity<ConversationPanel>,
    pub session: &'a Entity<Session>,
    pub spinners: &'a SpinnerSlots,
}

impl Footer<'_> {
    /// The merge box: checks, mergeability, the Merge button.
    pub fn merge_box(&self, t: &Theme, cx: &App) -> AnyElement {
        let c = &t.colors;
        let mut card = ui::card(cx)
            .debug_selector(|| "merge-box".into())
            .flex()
            .flex_col()
            .w_full()
            .min_w_0()
            .small(t)
            .child(self.checks(t, cx))
            .child(ui::divider(cx))
            .child(self.mergeability(t));
        if let Some(merge) = self.merge_row(t, cx) {
            card = card.child(ui::divider(cx)).child(merge);
        }
        let notice = self
            .facts
            .and_then(|facts| facts.notice.clone())
            .map(|(text, error)| {
                div()
                    .debug_selector(|| "merge-box-notice".into())
                    .px(px(12.0))
                    .pb(px(10.0))
                    .text_color(if error { c.danger } else { c.ok })
                    .child(text)
            });
        let confirm = self
            .facts
            .zip(self.facts.and_then(|facts| facts.confirm.as_ref()))
            .filter(|_| self.confirm_here)
            .map(|(facts, action)| self.confirmation(facts, action, t));
        card.children(notice).children(confirm).into_any_element()
    }

    fn checks(&self, t: &Theme, cx: &App) -> AnyElement {
        let c = &t.colors;
        let rows = ci_box::job_rows(self.open);
        let (state, headline, counts) = checks_summary(&rows, self.open.pr.ci);
        let toggle = (!rows.is_empty()).then(|| {
            let panel = self.panel.clone();
            let label = if self.state.checks_open {
                "Hide"
            } else {
                "Show"
            };
            Button::new("merge-box-checks", label, ButtonKind::Ghost).on_click(move |_, _, cx| {
                edit(&panel, cx, |state| state.checks_open = !state.checks_open);
            })
        });
        let jobs = self.state.checks_open.then(|| self.job_list(&rows, t, cx));
        v_flex()
            .debug_selector(|| "merge-box-checks-summary".into())
            .px(px(12.0))
            .py(px(10.0))
            .gap(px(8.0))
            .child(
                h_flex()
                    .gap(px(10.0))
                    .items_center()
                    .child(ci_icon(t, state, CI_HEADER, self.spinners))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(c.text)
                                    .child(headline),
                            )
                            .when(!counts.is_empty(), |el| {
                                el.child(div().text_color(c.text_muted).child(counts))
                            }),
                    )
                    .children(toggle),
            )
            .children(jobs)
            .into_any_element()
    }

    /// Every job, failing first; a job with a log opens it.
    fn job_list(&self, rows: &[JobRow], t: &Theme, cx: &App) -> AnyElement {
        let c = &t.colors;
        let hover = c.hover;
        let key = &self.open.key;
        let logs = self
            .session
            .read(cx)
            .capabilities_for(key)
            .is_some_and(|caps| caps.job_logs);
        let rows = rows.iter().enumerate().map(|(ix, row)| {
            let line = h_flex()
                .id(("merge-box-job", ix))
                .debug_selector(move || format!("merge-box-job-{ix}"))
                .gap(px(8.0))
                .px(px(4.0))
                .mx(px(-4.0))
                .min_h(px(24.0))
                .items_center()
                .rounded(t.radius.sm)
                .child(ci_icon(t, row.state, CI_ROW, self.spinners))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(c.text)
                        .child(row.name.clone()),
                )
                .child(
                    div()
                        .flex_none()
                        .mono(t)
                        .tiny(t)
                        .text_color(ci_color(t, row.state))
                        .child(row.detail.clone()),
                );
            let job = row
                .job
                .as_ref()
                .filter(|job| logs && job.started_at.is_some());
            let line = match job {
                Some(job) => {
                    let (session, key, id) = (self.session.clone(), key.clone(), job.id.clone());
                    line.cursor_pointer()
                        .hover(move |s| s.bg(hover))
                        .on_click(move |_, _, cx| {
                            session.update(cx, |session, cx| {
                                session.focus(&key, cx);
                                session.open_log(&id, None, cx);
                            });
                        })
                }
                None => line,
            };
            let menu = JobMenu::new(key.clone(), row.job.as_ref().map(|job| job.id.clone()));
            line.context_menu(move |popup, _, cx| menu.popup(popup, cx))
                .into_any_element()
        });
        v_flex()
            .debug_selector(|| "merge-box-jobs".into())
            .pl(px(26.0))
            .children(rows)
            .into_any_element()
    }

    /// Whether it can merge: no conflicts, or what blocks it.
    fn mergeability(&self, t: &Theme) -> AnyElement {
        let c = &t.colors;
        let conflicts = blocker_label(MergeBlocker::Conflicts);
        let (icon, color, headline, note): (IconName, _, SharedString, Option<SharedString>) =
            match self.facts {
                None => (
                    IconName::CircleDashed,
                    c.text_muted,
                    "Checking whether it can merge".into(),
                    None,
                ),
                Some(facts) => match (facts.state, &facts.mergeable) {
                    (PrState::Merged, _) => {
                        (IconName::CircleCheck, c.accent_fg, "Merged".into(), None)
                    }
                    (PrState::Closed, _) => (
                        IconName::CircleX,
                        c.danger,
                        "Closed without merging".into(),
                        None,
                    ),
                    (_, Load::Ready((true, _, _))) => (
                        IconName::CircleCheck,
                        c.ok,
                        "No conflicts with the base branch".into(),
                        Some("It can be merged".into()),
                    ),
                    (_, Load::Ready((false, reasons, _)))
                        if reasons.iter().any(|reason| reason.as_ref() == conflicts) =>
                    {
                        let rest: Vec<_> = reasons
                            .iter()
                            .filter(|reason| reason.as_ref() != conflicts)
                            .collect();
                        let rest = joined(&rest);
                        (
                            IconName::CircleX,
                            c.danger,
                            "This branch has conflicts that must be resolved".into(),
                            (!rest.is_empty()).then(|| format!("Also: {rest}").into()),
                        )
                    }
                    (_, Load::Ready((false, reasons, _))) => (
                        IconName::TriangleAlert,
                        c.warn,
                        "Merging is blocked".into(),
                        Some(if reasons.is_empty() {
                            "The forge refuses it".into()
                        } else {
                            capitalized(&joined(&reasons.iter().collect::<Vec<_>>())).into()
                        }),
                    ),
                    (_, Load::Idle | Load::Loading) => (
                        IconName::CircleDashed,
                        c.text_muted,
                        "Checking whether it can merge".into(),
                        None,
                    ),
                    (_, Load::Failed(error)) => (
                        IconName::TriangleAlert,
                        c.danger,
                        "Mergeability not read".into(),
                        Some(error.clone()),
                    ),
                    (_, Load::Unsupported) => (
                        IconName::CircleMinus,
                        c.text_muted,
                        "This forge cannot merge from here".into(),
                        None,
                    ),
                },
            };
        h_flex()
            .debug_selector(|| "merge-box-status".into())
            .px(px(12.0))
            .py(px(10.0))
            .gap(px(10.0))
            .items_center()
            .child(
                Icon::new(icon)
                    .size(px(CI_HEADER))
                    .text_color(color)
                    .flex_none(),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(c.text)
                            .child(headline),
                    )
                    .children(note.map(|note| div().text_color(c.text_muted).child(note))),
            )
            .into_any_element()
    }

    /// Merge with its method, while the pull request is open and the forge
    /// merges from here.
    fn merge_row(&self, t: &Theme, cx: &App) -> Option<AnyElement> {
        let facts = self.facts?;
        if !facts.caps.merge || !facts.state.is_open() {
            return None;
        }
        let c = &t.colors;
        let disabled = facts.merge_disabled();
        let label = match (&facts.busy, facts.method) {
            (Some(action), _) => format!("{}...", action.label()),
            (None, Some(method)) => method_label(method).to_owned(),
            (None, None) => "Merge".to_owned(),
        };
        let merge = {
            let (panel, key, action) =
                (self.panel.clone(), facts.key.clone(), facts.merge_action());
            Button::new("merge-box-merge", label, ButtonKind::Primary)
                .disabled(disabled.is_some())
                .on_click(move |_, _, cx| {
                    let Some(action) = action.clone() else {
                        return;
                    };
                    panel
                        .update(cx, |panel, cx| panel.ask(&key, action, cx))
                        .ok();
                })
        };
        let choose = (facts.allowed.len() > 1).then(|| {
            let panel = self.panel.clone();
            ui::icon_button(
                cx,
                "merge-box-method",
                IconName::ChevronDown,
                move |_, _, cx| {
                    edit(&panel, cx, |state| state.methods_open = !state.methods_open);
                },
            )
            .debug_selector(|| "merge-box-method".into())
        });
        let methods =
            (self.state.methods_open && facts.allowed.len() > 1).then(|| self.methods(facts, t));
        let why = disabled
            .filter(|_| facts.busy.is_none())
            .map(|why| div().text_color(c.text_muted).child(why));
        Some(
            v_flex()
                .px(px(12.0))
                .py(px(10.0))
                .gap(px(8.0))
                .child(
                    h_flex()
                        .gap(px(6.0))
                        .items_center()
                        .child(merge)
                        .children(choose)
                        .child(div().flex_1()),
                )
                .children(why)
                .children(methods)
                .into_any_element(),
        )
    }

    fn methods(&self, facts: &Facts, t: &Theme) -> AnyElement {
        let c = &t.colors;
        let hover = c.hover;
        let rows = facts.allowed.iter().map(|&method| {
            let chosen = facts.method == Some(method);
            let id = format!("merge-box-method-{}", method_id(method));
            let selector = id.clone();
            let (session, panel, key) =
                (self.session.clone(), self.panel.clone(), facts.key.clone());
            h_flex()
                .id(SharedString::from(id))
                .debug_selector(move || selector)
                .gap(px(6.0))
                .px(px(8.0))
                .py(px(4.0))
                .items_center()
                .rounded(t.radius.sm)
                .when(chosen, |el| el.bg(c.selected))
                .cursor_pointer()
                .hover(move |s| s.bg(hover))
                .on_click(move |_, _, cx| {
                    session.update(cx, |session, cx| session.set_merge_method(&key, method, cx));
                    edit(&panel, cx, |state| state.methods_open = false);
                })
                .child(
                    Icon::new(if chosen {
                        IconName::CircleCheck
                    } else {
                        IconName::Circle
                    })
                    .size(px(12.0))
                    .text_color(c.text_muted),
                )
                .child(div().text_color(c.text).child(method_label(method)))
        });
        v_flex()
            .debug_selector(|| "merge-box-methods".into())
            .w(px(260.0))
            .p(px(4.0))
            .rounded(t.radius.md)
            .border_1()
            .border_color(c.border_strong)
            .bg(c.surface_menu)
            .children(rows)
            .into_any_element()
    }

    /// The confirmation of what the footer asked: the question naming the
    /// pull request and the head, Confirm and Cancel.
    fn confirmation(&self, facts: &Facts, action: &Action, t: &Theme) -> AnyElement {
        let c = &t.colors;
        let danger = matches!(action, Action::Close);
        let (confirm_session, cancel_session) = (self.session.clone(), self.session.clone());
        let (confirm_key, cancel_key) = (facts.key.clone(), facts.key.clone());
        v_flex()
            .debug_selector(|| "merge-box-confirm-card".into())
            .mx(px(12.0))
            .mb(px(10.0))
            .p(px(10.0))
            .gap(px(8.0))
            .rounded(t.radius.md)
            .border_1()
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
                            "merge-box-confirm",
                            action.label(),
                            if danger {
                                ButtonKind::Danger
                            } else {
                                ButtonKind::Primary
                            },
                        )
                        .on_click(move |_, _, cx| {
                            confirm_session.update(cx, |session, cx| {
                                session.confirm_action(&confirm_key, cx);
                            });
                        }),
                    )
                    .child(
                        Button::new("merge-box-cancel", "Cancel", ButtonKind::Ghost).on_click(
                            move |_, _, cx| {
                                cancel_session.update(cx, |session, cx| {
                                    session.cancel_action(&cancel_key, cx);
                                });
                            },
                        ),
                    ),
            )
            .into_any_element()
    }

    /// The comment box: the Conversation's composer, which carries
    /// [`close_button`] at the start of its buttons.
    pub fn comment_box(&self, composer: &Entity<Composer>, t: &Theme, cx: &App) -> AnyElement {
        v_flex()
            .debug_selector(|| "comment-box".into())
            .w_full()
            .min_w_0()
            .gap(px(6.0))
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(t.colors.text_secondary)
                    .child("Add a comment"),
            )
            .child(ui::card(cx).w_full().min_w_0().child(composer.clone()))
            .into_any_element()
    }
}

/// "Close pull request" (or "Reopen pull request") for the comment box,
/// when the state allows it and the forge does it from here.
pub(super) fn close_button(
    facts: Option<&Facts>,
    panel: &WeakEntity<ConversationPanel>,
) -> Option<Leading> {
    let facts = facts?;
    let action = facts
        .state_actions()
        .into_iter()
        .find(|action| matches!(action, Action::Close | Action::Reopen))
        .filter(|action| facts.unsupported(action).is_none())?;
    let (id, label) = match action {
        Action::Reopen => ("composer-reopen-pr", "Reopen pull request"),
        _ => ("composer-close-pr", "Close pull request"),
    };
    let (panel, key) = (panel.clone(), facts.key.clone());
    Some(Leading {
        id,
        label: label.into(),
        on_click: Rc::new(move |_: &mut Window, cx: &mut App| {
            panel
                .update(cx, |panel, cx| panel.ask(&key, action.clone(), cx))
                .ok();
        }),
    })
}

/// The overall state, a headline and the counts by state.
fn checks_summary(rows: &[JobRow], rollup: CiState) -> (CiState, &'static str, String) {
    let count = |state: CiState| rows.iter().filter(|row| row.state == state).count();
    let failing = count(CiState::Fail);
    let active = rows.iter().filter(|row| row.state.is_active()).count();
    let passed = count(CiState::Pass);
    let skipped = count(CiState::Skipped) + count(CiState::Canceled);
    let counts = [
        (failing, "failing"),
        (active, "in progress"),
        (passed, "successful"),
        (skipped, "skipped"),
    ]
    .into_iter()
    .filter(|(n, _)| *n > 0)
    .map(|(n, what)| format!("{n} {what}"))
    .collect::<Vec<_>>()
    .join(", ");
    let (state, headline) = if rows.is_empty() {
        let headline = match rollup {
            CiState::None => "No checks",
            CiState::Fail => "Some checks were not successful",
            CiState::Running | CiState::Queued => "Some checks have not finished",
            CiState::Pass => "All checks have passed",
            CiState::Skipped | CiState::Canceled => "The checks did not run",
        };
        (rollup, headline)
    } else if failing > 0 {
        (CiState::Fail, "Some checks were not successful")
    } else if active > 0 {
        (CiState::Running, "Some checks have not finished")
    } else if passed > 0 {
        (CiState::Pass, "All checks have passed")
    } else {
        (CiState::Skipped, "The checks did not run")
    };
    (state, headline, counts)
}

fn joined(parts: &[&SharedString]) -> String {
    parts
        .iter()
        .map(|part| part.as_ref())
        .collect::<Vec<&str>>()
        .join(", ")
}

fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(state: CiState) -> JobRow {
        JobRow {
            state,
            name: "job".into(),
            job: None,
            detail: String::new(),
        }
    }

    #[test]
    fn the_summary_names_the_worst_state_and_counts_each() {
        let rows = [row(CiState::Fail), row(CiState::Pass), row(CiState::Pass)];
        let (state, headline, counts) = checks_summary(&rows, CiState::Fail);
        assert_eq!(state, CiState::Fail);
        assert_eq!(headline, "Some checks were not successful");
        assert_eq!(counts, "1 failing, 2 successful");
        let rows = [row(CiState::Running), row(CiState::Pass)];
        assert_eq!(checks_summary(&rows, CiState::Running).0, CiState::Running);
        assert_eq!(
            checks_summary(&[], CiState::None),
            (CiState::None, "No checks", String::new())
        );
    }

    #[test]
    fn blockers_read_as_a_sentence() {
        assert_eq!(capitalized("it is a draft"), "It is a draft");
        assert_eq!(capitalized(""), "");
    }
}
