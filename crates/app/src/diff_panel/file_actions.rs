//! What the Diff panel does to files: collapsing them (and remembering it),
//! showing one alone, the file header's controls and the right-click menu.

use std::collections::HashMap;

use diff::{ContextMode, FileIx};
use diff_view::ContextTarget;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, Context, Div, InteractiveElement as _, IntoElement as _, ParentElement as _,
    Pixels, Point, SharedString, StatefulInteractiveElement as _, Styled as _, Window, anchored,
    deferred, div, px,
};
use gpui_kit::component::{h_flex, v_flex};
use store::DraftAnchor;
use theme::Theme;

use super::menu::{self, DiffMenu, Entry, Links, MenuAction};
use super::{
    CloseMenu, CollapseMemory, DiffPanel, ToggleAllFilesCollapsed, ToggleFileCollapsed,
    collapse_viewed, copy, file_header, show_all_changes,
};
use crate::gutter;
use crate::ui::{self, Button, TextExt as _};
use crate::untrusted::is_web_url;

impl DiffPanel {
    /// Records which files are collapsed now, for a diff of the same pull
    /// request built later. A commit's diff is not remembered.
    pub(super) fn remember_collapsed(&self, cx: &mut App) {
        let Some(loaded) = self
            .loaded
            .as_ref()
            .filter(|loaded| loaded.restored && loaded.commit.is_none())
        else {
            return;
        };
        let view = loaded.view.read(cx);
        let state: HashMap<String, bool> = view
            .list()
            .diff()
            .files
            .iter()
            .zip(view.collapsed_files())
            .map(|(file, &collapsed)| (file.path.clone(), collapsed))
            .collect();
        cx.default_global::<CollapseMemory>()
            .0
            .insert(loaded.key.clone(), state);
    }

    /// Folds a newly built view's files as they were last left, once its
    /// viewed marks are in; files not remembered keep what the marks did.
    pub(super) fn restore_collapsed(&mut self, cx: &mut Context<Self>) {
        let Some(loaded) = self.loaded.as_mut().filter(|loaded| !loaded.restored) else {
            return;
        };
        loaded.restored = true;
        if loaded.commit.is_some() {
            return;
        }
        let view = loaded.view.clone();
        let Some(memory) = cx
            .try_global::<CollapseMemory>()
            .and_then(|memory| memory.0.get(&loaded.key))
        else {
            return;
        };
        let read = view.read(cx);
        let collapsed: Vec<bool> = read
            .list()
            .diff()
            .files
            .iter()
            .zip(read.collapsed_files())
            .map(|(file, &now)| memory.get(&file.path).copied().unwrap_or(now))
            .collect();
        view.update(cx, |view, cx| view.set_collapsed_files(collapsed, cx));
    }

    /// Follows the collapse setting: the view folds viewed files, or not.
    pub(super) fn apply_collapse_setting(&mut self, cx: &mut Context<Self>) {
        let Some(loaded) = &self.loaded else {
            return;
        };
        let fold = collapse_viewed(cx);
        loaded
            .view
            .update(cx, |view, cx| view.set_collapse_viewed(fold, cx));
        self.remember_collapsed(cx);
    }

    /// `z`: collapses or expands the file at the top of the view.
    pub(super) fn toggle_file_collapsed(
        &mut self,
        _: &ToggleFileCollapsed,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(view) = self.view().cloned() {
            view.update(cx, |view, cx| view.toggle_current_file(cx));
        }
    }

    /// `Shift+Z`: collapses every file, or expands them all when every one
    /// is collapsed already.
    pub(super) fn toggle_all_collapsed(
        &mut self,
        _: &ToggleAllFilesCollapsed,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(view) = self.view().cloned() {
            view.update(cx, |view, cx| {
                if view.collapsed_files().iter().all(|&collapsed| collapsed) {
                    view.expand_all(cx);
                } else {
                    view.collapse_all(cx);
                }
            });
        }
    }

    /// The path of `file` in the view's diff.
    pub fn path_of(&self, file: FileIx, cx: &App) -> Option<String> {
        let view = self.view()?.read(cx);
        Some(view.list().diff().files.get(file.0 as usize)?.path.clone())
    }

    /// "Copy file name": the file's path, to the clipboard.
    pub fn copy_file_name(&mut self, file: FileIx, cx: &mut Context<Self>) -> bool {
        let Some(path) = self.path_of(file, cx) else {
            return false;
        };
        copy(path, cx);
        true
    }

    /// "Expand all lines": every line of the file in place, every hunk
    /// unfolded; again, back to its hunks.
    pub fn toggle_all_lines(&mut self, file: FileIx, cx: &mut Context<Self>) {
        let Some(view) = self.view().cloned() else {
            return;
        };
        let next = match view.read(cx).context(file) {
            ContextMode::Hunks => ContextMode::Full,
            ContextMode::Full => ContextMode::Hunks,
        };
        view.update(cx, |view, cx| {
            view.set_file_collapsed(file, false, cx);
            view.set_file_context(file, next, cx);
        });
        if view.read(cx).shown_file() == Some(file) {
            self.whole_mode = next;
        }
        cx.notify();
    }

    /// "Comment on this file": a file-level draft thread, where the forge
    /// has them. False, and nothing opens, where it has not.
    pub fn comment_on_file(&mut self, file: FileIx, cx: &mut Context<Self>) -> bool {
        let Some(key) = self.binding.key(cx) else {
            return false;
        };
        if file_header::file_comments(&self.session, &key, cx).is_err() {
            return false;
        }
        let (Some(path), Some(view)) = (self.path_of(file, cx), self.view().cloned()) else {
            return false;
        };
        // The composer sits at the top of the file, which a fold would hide.
        view.update(cx, |view, cx| view.set_file_collapsed(file, false, cx));
        self.writing.update(cx, |writing, cx| {
            writing.open(DraftAnchor::File { path }, None, cx);
        });
        true
    }

    /// Shows `file` alone, every line of it, as a click in the tree does.
    pub fn open_in_full(&mut self, file: FileIx, cx: &mut Context<Self>) {
        let (Some(key), Some(path)) = (self.binding.key(cx), self.path_of(file, cx)) else {
            return;
        };
        self.whole_mode = ContextMode::Full;
        if let Some(view) = self.view().cloned()
            && view.read(cx).shown_file() == Some(file)
        {
            view.update(cx, |view, cx| {
                view.set_file_context(file, ContextMode::Full, cx);
            });
        }
        self.session.update(cx, |session, cx| {
            session.focus(&key, cx);
            session.select_file(path.into(), cx);
        });
    }

    /// Shows the file picked in the tree (`selected`) alone, or every file;
    /// a new pick of the file already shown goes back to its top.
    pub(super) fn sync_shown_file(
        &mut self,
        selected: Option<&SharedString>,
        jump: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.view().cloned() else {
            return;
        };
        let want = selected.and_then(|path| {
            let files = &view.read(cx).list().diff().files;
            let ix = files.iter().position(|file| file.path == path.as_ref())?;
            Some(FileIx(u32::try_from(ix).ok()?))
        });
        if want == view.read(cx).shown_file() {
            if jump && want.is_some() {
                view.update(cx, |view, cx| {
                    if view.set_scroll_y(0.0) {
                        cx.notify();
                    }
                });
            }
            return;
        }
        let restore = self.whole_restore.take();
        let before = want.map(|file| view.read(cx).context(file));
        let mode = self.whole_mode;
        view.update(cx, |view, cx| {
            if let Some((file, context)) = restore {
                view.set_file_context(file, context, cx);
            }
            if let Some(file) = want {
                view.set_file_context(file, mode, cx);
                // Picked to be read: a fold would leave only its header.
                view.set_file_collapsed(file, false, cx);
            }
            view.show_file(want, cx);
        });
        self.whole_restore = want.zip(before);
    }

    /// How the file shown alone is shown: in full or as its changes.
    pub fn set_whole_mode(&mut self, mode: ContextMode, cx: &mut Context<Self>) {
        self.whole_mode = mode;
        if let Some(view) = self.view().cloned()
            && let Some(file) = view.read(cx).shown_file()
        {
            view.update(cx, |view, cx| view.set_file_context(file, mode, cx));
        }
        cx.notify();
    }

    /// Opens the diff's menu on `target` at `position` in the window.
    pub fn open_menu(
        &mut self,
        position: Point<Pixels>,
        target: ContextTarget,
        cx: &mut Context<Self>,
    ) {
        self.menu = Some(DiffMenu { position, target });
        cx.notify();
    }

    /// The menu that is open, if any.
    pub fn menu(&self) -> Option<&DiffMenu> {
        self.menu.as_ref()
    }

    pub(super) fn close_menu(&mut self, _: &CloseMenu, _: &mut Window, cx: &mut Context<Self>) {
        if self.menu.take().is_some() {
            cx.notify();
        } else {
            cx.propagate();
        }
    }

    /// Where the pull request's files are on the forge: at its head, or
    /// at the commit shown and its first parent.
    pub fn links(&self, cx: &App) -> Option<Links> {
        let key = self.binding.key(cx)?;
        let session = self.session.read(cx);
        let open = session.pr(&key)?;
        let head = open
            .shown_diff()
            .map(|diff| diff.head_sha.clone())
            .filter(|sha| !sha.is_empty());
        let base = match &open.commit_view {
            Some(view) => view.commit.parent.as_ref().map(ToString::to_string),
            None => open
                .forge_pr()
                .map(|pr| pr.merge_base.as_ref().unwrap_or(&pr.base.sha))
                .map(|sha| sha.as_str().to_owned()),
        }
        .filter(|sha| !sha.is_empty());
        Some(Links {
            forge: open.pr.forge,
            host: key.repo.host.clone(),
            owner: key.repo.owner.clone(),
            repo: key.repo.name.clone(),
            head,
            base,
        })
    }

    /// The rows of the menu for `target`, disabled ones with their reason.
    pub fn menu_entries(&self, target: &ContextTarget, cx: &App) -> Vec<Entry> {
        let Some(view) = self.view() else {
            return Vec::new();
        };
        let view = view.read(cx);
        let file = menu::target_file(target);
        let links = self.links(cx);
        let file_comments = match self.binding.key(cx) {
            Some(key) => file_header::file_comments(&self.session, &key, cx),
            None => Err("No pull request is open".into()),
        };
        let state = menu::MenuState {
            links: links.as_ref(),
            file_comments,
            context: view.context(file),
            collapsed: view.is_file_collapsed(file),
        };
        let lines = menu::target_lines(target, view.list().diff());
        let mut entries = menu::entries(target, lines.as_ref(), &state);
        if self.read_only() {
            for entry in &mut entries {
                if let Entry::Item {
                    action: MenuAction::Comment,
                    disabled,
                    ..
                } = entry
                {
                    *disabled = Some(file_header::COMMIT_READ_ONLY.into());
                }
            }
        }
        entries
    }

    /// The link a link action on `target` copies or opens.
    fn target_url(&self, target: &ContextTarget, cx: &App) -> Option<String> {
        let links = self.links(cx)?;
        if let ContextTarget::FileHeader { path, .. } = target {
            return links.file_url(path).ok();
        }
        let view = self.view()?.read(cx);
        let lines = menu::target_lines(target, view.list().diff())?;
        links
            .lines_url(&lines.path, lines.side, lines.start, lines.end)
            .ok()
    }

    /// Runs a menu entry on `target`, as a click on it does. False when it
    /// could not act (a link with no commit, no file comments).
    pub fn run_menu_action(
        &mut self,
        action: MenuAction,
        target: &ContextTarget,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(view) = self.view().cloned() else {
            return false;
        };
        let file = menu::target_file(target);
        let lines = menu::target_lines(target, view.read(cx).list().diff());
        match action {
            MenuAction::CopyLink => {
                let Some(url) = self.target_url(target, cx) else {
                    return false;
                };
                copy(url, cx);
            }
            MenuAction::OpenOnForge => {
                let Some(url) = self.target_url(target, cx).filter(|url| is_web_url(url)) else {
                    return false;
                };
                cx.open_url(&url);
            }
            MenuAction::CopyLines => {
                let Some(lines) = lines else {
                    return false;
                };
                let view = view.read(cx);
                let text: Vec<String> = (lines.start..=lines.end)
                    .filter_map(|line| view.line_text(lines.file, lines.side, line))
                    .collect();
                copy(text.join("\n"), cx);
            }
            MenuAction::Comment => {
                let Some(lines) = lines else {
                    return false;
                };
                let range = gutter::Range {
                    file: lines.file,
                    side: lines.side,
                    first: lines.start,
                    last: lines.end,
                    rows: (0, 0),
                };
                self.open_range(&range, cx);
            }
            MenuAction::OpenFull => self.open_in_full(file, cx),
            MenuAction::CopyFileName => return self.copy_file_name(file, cx),
            MenuAction::CommentOnFile => return self.comment_on_file(file, cx),
            MenuAction::ToggleAllLines => self.toggle_all_lines(file, cx),
            MenuAction::ToggleCollapse => view.update(cx, |view, cx| {
                let collapsed = !view.is_file_collapsed(file);
                view.set_file_collapsed(file, collapsed, cx);
            }),
            MenuAction::CollapseAll => view.update(cx, |view, cx| view.collapse_all(cx)),
            MenuAction::ExpandAll => view.update(cx, |view, cx| view.expand_all(cx)),
        }
        true
    }

    /// The open menu, drawn over everything where it was asked for.
    pub(super) fn menu_overlay(
        &self,
        menu: &DiffMenu,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = &t.colors;
        let hover = c.hover;
        let rows: Vec<AnyElement> = self
            .menu_entries(&menu.target, cx)
            .into_iter()
            .map(|entry| match entry {
                Entry::Separator => div().h(px(1.0)).my(px(4.0)).bg(c.border).into_any_element(),
                Entry::Item {
                    action,
                    label,
                    disabled,
                } => {
                    let target = menu.target.clone();
                    let selector = format!("diff-menu-{}", action.id());
                    v_flex()
                        .id(action.id())
                        .debug_selector(move || selector)
                        .min_h(px(26.0))
                        .px(px(10.0))
                        .py(px(4.0))
                        .justify_center()
                        .rounded(t.radius.sm)
                        .text_color(if disabled.is_some() {
                            c.text_muted
                        } else {
                            c.text
                        })
                        .child(label)
                        .map(|el| match disabled {
                            Some(reason) => {
                                el.child(div().tiny(t).text_color(c.text_muted).child(reason))
                            }
                            None => el.cursor_pointer().hover(move |s| s.bg(hover)).on_click(
                                cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.menu = None;
                                    this.run_menu_action(action, &target, cx);
                                    cx.notify();
                                }),
                            ),
                        })
                        .into_any_element()
                }
            })
            .collect();
        deferred(
            anchored()
                .position(menu.position)
                .snap_to_window_with_margin(px(8.0))
                .child(
                    v_flex()
                        .id("diff-menu")
                        .debug_selector(|| "diff-menu".into())
                        .occlude()
                        .w(px(250.0))
                        .p(px(4.0))
                        .rounded(t.radius.md)
                        .border_1()
                        .border_color(c.border_strong)
                        .bg(c.surface_menu)
                        .small(t)
                        .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                            this.menu = None;
                            cx.notify();
                        }))
                        .children(rows),
                ),
        )
        .with_priority(1)
        .into_any_element()
    }

    /// For a file shown alone: back to every file, and a two-button
    /// segmented control for the full file or its changes only.
    pub(super) fn whole_file_controls(&self, cx: &mut Context<Self>) -> Div {
        let current = self.whole_mode;
        let session = self.session.clone();
        let me = cx.weak_entity();
        h_flex()
            .gap(px(10.0))
            .items_center()
            .child(
                Button::toolbar("all-changes", "All changes").on_click(move |_, _, cx| {
                    session.update(cx, show_all_changes);
                }),
            )
            .child(ui::toggle_group(
                cx,
                vec![
                    (ContextMode::Full, "whole-file-full", "Full file"),
                    (ContextMode::Hunks, "whole-file-changes", "Changes"),
                ],
                move |mode| *mode == current,
                move |mode, _, cx| {
                    let mode = *mode;
                    me.update(cx, |this, cx| this.set_whole_mode(mode, cx)).ok();
                },
            ))
    }
}
