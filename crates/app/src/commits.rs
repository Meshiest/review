//! A pull request's commits: the list read from local git (merge base to
//! head, newest first), the "N commits" chip and list in the Conversation
//! panel, and one commit's diff for the Diff panel.

use std::sync::Arc;

use chrono::Utc;
use forge::model::{RepoRef, Timestamp};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, Entity, FontWeight, InteractiveElement as _, IntoElement as _,
    ParentElement as _, SharedString, Styled as _, Task, div, px,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::{Icon, h_flex, v_flex};
use repo::{FileListOptions, ObjectId};

use crate::model::{PrKey, relative_time};
use crate::opener::{self, Built};
use crate::services::ObjectStores;
use crate::session::{OpenDiff, Session};
use crate::ui::{self, TextExt as _, fold};

/// One commit of a pull request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrCommit {
    /// The full id.
    pub sha: SharedString,
    /// The message's first line.
    pub title: SharedString,
    pub author: SharedString,
    pub authored: Timestamp,
    /// The first parent, which its diff starts from; `None` for a root commit.
    pub parent: Option<SharedString>,
    /// A merge commit: its diff is against the first parent alone.
    pub merge: bool,
}

impl PrCommit {
    pub fn from_repo(info: &repo::CommitInfo) -> Self {
        Self {
            sha: info.id.to_string().into(),
            title: info.title.clone().into(),
            author: info.author.clone().into(),
            authored: Timestamp::from_timestamp(info.authored, 0).unwrap_or_default(),
            parent: info.parents.first().map(|id| id.to_string().into()),
            merge: info.is_merge(),
        }
    }

    /// The id as git abbreviates it.
    pub fn short(&self) -> &str {
        self.sha.get(..7).unwrap_or(&self.sha)
    }

    /// How long ago it was authored: "5m", "3d".
    pub fn age(&self) -> String {
        let minutes = (Utc::now() - self.authored).num_minutes().max(0);
        relative_time(u32::try_from(minutes).unwrap_or(u32::MAX))
    }
}

/// The commit the Diff panel shows alone, in place of all changes.
pub struct CommitView {
    pub commit: PrCommit,
    /// Its diff, once built.
    pub diff: Option<OpenDiff>,
    /// Lines added and removed.
    pub stat: (u32, u32),
    /// Why its diff could not be built.
    pub error: Option<SharedString>,
    pub(crate) task: Option<Task<()>>,
}

impl CommitView {
    pub fn new(commit: PrCommit) -> Self {
        Self {
            commit,
            diff: None,
            stat: (0, 0),
            error: None,
            task: None,
        }
    }

    /// Takes its built diff, under the diff generation `generation`.
    pub fn ready(&mut self, diff: diff::Diff, generation: u64) {
        self.stat = opener::stat(&diff);
        self.diff = Some(OpenDiff {
            diff: Arc::new(diff),
            generation,
            head: self.commit.short().to_owned().into(),
            head_sha: self.commit.sha.to_string(),
            complete: true,
        });
    }
}

/// Builds `commit`'s diff from its first parent, renames paired. Blocks on
/// git.
///
/// # Errors
/// If the repository's objects or the commit cannot be read.
pub fn build(objects: &ObjectStores, repo: &RepoRef, commit: &str) -> Result<Built, String> {
    let store = objects.resolve(repo)?;
    let id = ObjectId::from_hex(commit.as_bytes()).map_err(|error| error.to_string())?;
    let options = FileListOptions {
        renames: true,
        detect_binary: false,
    };
    let list = store
        .commit_file_list(id, options)
        .map_err(|error| error.to_string())?;
    Ok(opener::build_diff(&store, &list, None))
}

/// "3 commits": a click, Enter or Space shows or hides the list under the
/// header. Nothing while the commits are being read.
pub fn chip(
    commits: Option<&[PrCommit]>,
    open: bool,
    toggle: impl Fn(&mut App) + 'static,
    cx: &App,
) -> Option<AnyElement> {
    let commits = commits?;
    let t = ui::theme(cx);
    let c = &t.colors;
    let count = commits.len();
    let label = format!("{count} commit{}", if count == 1 { "" } else { "s" });
    let row = div()
        .id("commits-chip")
        .debug_selector(|| "commits-chip".into())
        .flex_none()
        .rounded(t.radius.sm);
    let chevron = if open {
        IconName::ChevronDown
    } else {
        IconName::ChevronRight
    };
    Some(
        fold::toggle_row(row, &t, move |_, _, cx| toggle(cx))
            .child(
                ui::chip(cx, label, c.info)
                    .gap(px(2.0))
                    .child(Icon::new(chevron).size(px(12.0))),
            )
            .into_any_element(),
    )
}

/// The commits, newest first: short id, title, author and age. A click,
/// Enter or Space on one shows its diff in the Diff panel; the one shown
/// is marked.
pub fn list(
    session: &Entity<Session>,
    key: &PrKey,
    commits: &[PrCommit],
    shown: Option<&SharedString>,
    cx: &App,
) -> AnyElement {
    let t = ui::theme(cx);
    let c = &t.colors;
    let rows = commits.iter().enumerate().map(|(ix, commit)| {
        let on = shown == Some(&commit.sha);
        let short = commit.short().to_owned();
        let (session, key, sha) = (session.clone(), key.clone(), commit.sha.clone());
        let row = h_flex()
            .id(("commit-row", ix))
            .debug_selector(move || format!("commit-{short}"))
            .w_full()
            .min_w_0()
            .h(px(28.0))
            .px(px(10.0))
            .gap(px(10.0))
            .items_center()
            .when(ix > 0, |el| el.border_t_1().border_color(c.border))
            .when(on, |el| el.bg(c.selected));
        fold::toggle_row(row, &t, move |_, _, cx| {
            session.update(cx, |session, cx| {
                session.show_commit(&key, Some(sha.clone()), cx);
            });
        })
        .child(
            div()
                .flex_none()
                .mono(&t)
                .text_color(if on { c.accent_fg } else { c.text_secondary })
                .child(commit.short().to_owned()),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(c.text)
                .child(commit.title.clone()),
        )
        .when(commit.merge, |el| {
            el.child(ui::chip(cx, "merge", c.text_muted))
        })
        .child(
            div()
                .flex_none()
                .max_w(px(160.0))
                .truncate()
                .text_color(c.text_muted)
                .child(commit.author.clone()),
        )
        .child(
            div()
                .flex_none()
                .w(px(32.0))
                .text_right()
                .text_color(c.text_muted)
                .child(commit.age()),
        )
    });
    v_flex()
        .debug_selector(|| "commit-list".into())
        .w_full()
        .small(&t)
        .border_1()
        .border_color(c.border)
        .rounded(t.radius.md)
        .overflow_hidden()
        .when(commits.is_empty(), |el| {
            el.child(
                div()
                    .px(px(10.0))
                    .py(px(6.0))
                    .text_color(c.text_muted)
                    .child("No commits between the base and the head"),
            )
        })
        .children(rows)
        .into_any_element()
}

/// The Diff panel's line over one commit's diff: which commit, that it is
/// read-only, and the way back to all changes.
pub fn banner(commit: &PrCommit, show_all: impl Fn(&mut App) + 'static, cx: &App) -> AnyElement {
    let t = ui::theme(cx);
    let c = &t.colors;
    h_flex()
        .debug_selector(|| "commit-banner".into())
        .flex_none()
        .w_full()
        .min_w_0()
        .gap(px(10.0))
        .px(px(12.0))
        .py(px(6.0))
        .items_center()
        .bg(c.surface_raised)
        .border_b_1()
        .border_color(c.border)
        .small(&t)
        .child(
            div()
                .flex_none()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(c.text)
                .child(format!("Commit {}:", commit.short())),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(c.text)
                .child(commit.title.clone()),
        )
        .child(
            div()
                .flex_none()
                .text_color(c.text_muted)
                .child(if commit.merge {
                    "against its first parent, read-only"
                } else {
                    "read-only"
                }),
        )
        .child(
            ui::Button::toolbar("show-all-changes", "Show all changes")
                .on_click(move |_, _, cx| show_all(cx)),
        )
        .into_any_element()
}
