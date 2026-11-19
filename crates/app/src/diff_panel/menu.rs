//! The diff's right-click menu: what it offers on a line, a selection, a
//! hunk header or a file header, which entries are disabled and why, and
//! the permalinks its link actions copy.

use diff::{ContextMode, Diff, FileIx, Side};
use diff_view::{ContextTarget, LineAnchor};
use forge::model::ForgeKind;
use gpui::{Pixels, Point, SharedString};

/// An open menu: where it was asked for, in the window, and what it acts on.
#[derive(Clone, Debug, PartialEq)]
pub struct DiffMenu {
    pub position: Point<Pixels>,
    pub target: ContextTarget,
}

/// What a menu entry does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuAction {
    /// A permalink to the lines at the head (new side) or base (old side).
    CopyLink,
    CopyLines,
    /// Opens a composer on the lines.
    Comment,
    /// Shows the file alone, every line of it.
    OpenFull,
    OpenOnForge,
    CopyFileName,
    /// A file-level draft thread.
    CommentOnFile,
    /// Every line of the file in place, or back to its hunks.
    ToggleAllLines,
    ToggleCollapse,
    CollapseAll,
    ExpandAll,
}

impl MenuAction {
    /// The entry's `debug_selector` is `diff-menu-{id}`.
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::CopyLink => "copy-link",
            Self::CopyLines => "copy-lines",
            Self::Comment => "comment",
            Self::OpenFull => "open-full",
            Self::OpenOnForge => "open-on-forge",
            Self::CopyFileName => "copy-file-name",
            Self::CommentOnFile => "comment-on-file",
            Self::ToggleAllLines => "toggle-all-lines",
            Self::ToggleCollapse => "toggle-collapse",
            Self::CollapseAll => "collapse-all",
            Self::ExpandAll => "expand-all",
        }
    }
}

/// One row of a menu.
#[derive(Clone, Debug, PartialEq)]
pub enum Entry {
    Item {
        action: MenuAction,
        label: SharedString,
        /// Why it cannot be used now; `None` when it can.
        disabled: Option<SharedString>,
    },
    Separator,
}

/// Where the pull request's files live on the forge, for permalinks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Links {
    pub forge: ForgeKind,
    pub host: String,
    pub owner: String,
    pub repo: String,
    /// The head commit the diff shows: new-side lines are at it.
    pub head: Option<String>,
    /// The base commit: old-side lines are at it.
    pub base: Option<String>,
}

impl Links {
    /// The forge page of `start..=end` of `side` of `path`, pinned to the
    /// commit that side is at; why not when that commit is not known.
    pub fn lines_url(
        &self,
        path: &str,
        side: Side,
        start: u32,
        end: u32,
    ) -> Result<String, SharedString> {
        let head = self.head.as_deref().unwrap_or_default();
        let base = self.base.as_deref().unwrap_or_default();
        let anchor = LineAnchor::at_side(path, side, start, end, head, base);
        if anchor.commit.is_empty() {
            return Err(match side {
                Side::New => "The head commit is not known yet".into(),
                Side::Old => "The base commit is not known yet".into(),
            });
        }
        let web_base = format!("https://{}", self.host);
        Ok(anchor.forge_url(self.forge, &web_base, &self.owner, &self.repo))
    }

    /// The forge page of `path` at the head commit.
    pub fn file_url(&self, path: &str) -> Result<String, SharedString> {
        let url = self.lines_url(path, Side::New, 0, 0)?;
        Ok(url
            .split_once('#')
            .map_or(url.as_str(), |(page, _)| page)
            .to_owned())
    }
}

/// The lines a target covers, on one side of one file. Lines are 0-based,
/// `end` inclusive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetLines {
    pub file: FileIx,
    pub path: String,
    pub side: Side,
    pub start: u32,
    pub end: u32,
}

/// The lines of a line, a selection or a hunk (its new lines, or its old
/// ones when it only removes); `None` for a file header.
#[must_use]
pub fn target_lines(target: &ContextTarget, diff: &Diff) -> Option<TargetLines> {
    match target {
        ContextTarget::Line {
            file,
            side,
            line,
            path,
        } => Some(TargetLines {
            file: *file,
            path: path.clone(),
            side: *side,
            start: *line,
            end: *line,
        }),
        ContextTarget::Range {
            file,
            side,
            start,
            end,
            path,
        } => Some(TargetLines {
            file: *file,
            path: path.clone(),
            side: *side,
            start: *start,
            end: *end,
        }),
        ContextTarget::Hunk {
            file,
            old_start,
            new_start,
        } => {
            let file_diff = diff.files.get(file.0 as usize)?;
            let hunk = file_diff
                .hunks
                .iter()
                .find(|h| h.old_start == *old_start && h.new_start == *new_start)?;
            let (side, start, len) = if hunk.new_len() > 0 {
                (Side::New, hunk.new_start, hunk.new_len())
            } else {
                (Side::Old, hunk.old_start, hunk.old_len())
            };
            Some(TargetLines {
                file: *file,
                path: file_diff.path.clone(),
                side,
                start,
                end: start + len.max(1) - 1,
            })
        }
        ContextTarget::FileHeader { .. } => None,
    }
}

/// The file a target is in.
#[must_use]
pub fn target_file(target: &ContextTarget) -> FileIx {
    match target {
        ContextTarget::Line { file, .. }
        | ContextTarget::Range { file, .. }
        | ContextTarget::FileHeader { file, .. }
        | ContextTarget::Hunk { file, .. } => *file,
    }
}

/// What the entries depend on besides the target.
pub struct MenuState<'a> {
    pub links: Option<&'a Links>,
    /// `Err` with the reason when the forge has no file-level comments.
    pub file_comments: Result<(), SharedString>,
    pub context: ContextMode,
    pub collapsed: bool,
}

/// The menu's rows for `target`.
#[must_use]
pub fn entries(
    target: &ContextTarget,
    lines: Option<&TargetLines>,
    state: &MenuState,
) -> Vec<Entry> {
    let item = |action, label: &str, disabled: Option<SharedString>| Entry::Item {
        action,
        label: SharedString::from(label.to_owned()),
        disabled,
    };
    let no_links = || SharedString::from("No pull request is open");
    let lines_link = |lines: &TargetLines| match state.links {
        Some(links) => links
            .lines_url(&lines.path, lines.side, lines.start, lines.end)
            .err(),
        None => Some(no_links()),
    };
    let all_lines = match state.context {
        ContextMode::Hunks => "Expand all lines",
        ContextMode::Full => "Show changes only",
    };
    match (target, lines) {
        (ContextTarget::FileHeader { path, .. }, _) => {
            let file_link = match state.links {
                Some(links) => links.file_url(path).err(),
                None => Some(no_links()),
            };
            vec![
                item(MenuAction::CopyFileName, "Copy file name", None),
                item(MenuAction::CopyLink, "Copy link to file", file_link.clone()),
                item(
                    MenuAction::CommentOnFile,
                    "Comment on this file",
                    state.file_comments.clone().err(),
                ),
                Entry::Separator,
                item(MenuAction::ToggleAllLines, all_lines, None),
                item(MenuAction::OpenFull, "Open file in full", None),
                item(
                    MenuAction::ToggleCollapse,
                    if state.collapsed {
                        "Expand file"
                    } else {
                        "Collapse file"
                    },
                    None,
                ),
                item(MenuAction::CollapseAll, "Collapse all files", None),
                item(MenuAction::ExpandAll, "Expand all files", None),
                Entry::Separator,
                item(MenuAction::OpenOnForge, "Open on forge", file_link),
            ]
        }
        (ContextTarget::Hunk { .. }, Some(lines)) => {
            let link = lines_link(lines);
            vec![
                item(MenuAction::CopyLink, "Copy link to lines", link.clone()),
                item(MenuAction::CopyLines, "Copy lines", None),
                Entry::Separator,
                item(MenuAction::ToggleAllLines, all_lines, None),
                item(MenuAction::OpenFull, "Open file in full", None),
                Entry::Separator,
                item(MenuAction::OpenOnForge, "Open on forge", link),
            ]
        }
        (_, Some(lines)) => {
            let link = lines_link(lines);
            let one = lines.start == lines.end;
            let (copy_link, copy, comment) = if one {
                ("Copy link to line", "Copy line", "Comment on line")
            } else {
                ("Copy link to lines", "Copy lines", "Comment on lines")
            };
            vec![
                item(MenuAction::CopyLink, copy_link, link.clone()),
                item(MenuAction::CopyLines, copy, None),
                item(MenuAction::Comment, comment, None),
                Entry::Separator,
                item(MenuAction::OpenFull, "Open file in full", None),
                item(MenuAction::OpenOnForge, "Open on forge", link),
            ]
        }
        (_, None) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use diff::{FileDiff, Hunk, Segment, Text};

    use super::*;

    const HEAD: &str = "1111111111111111111111111111111111111111";

    fn links(base: Option<&str>) -> Links {
        Links {
            forge: ForgeKind::GitHub,
            host: "github.com".into(),
            owner: "acme".into(),
            repo: "api".into(),
            head: Some(HEAD.into()),
            base: base.map(str::to_owned),
        }
    }

    fn diff() -> Diff {
        Diff {
            files: vec![FileDiff {
                path: "src/a.rs".into(),
                old: Text::new("a\nb\nc\n"),
                new: Text::new("a\nB\nB2\nc\n"),
                hunks: vec![
                    Hunk {
                        old_start: 0,
                        new_start: 0,
                        segments: vec![
                            Segment::Context(1),
                            Segment::Change {
                                removed: 1,
                                added: 2,
                            },
                        ],
                    },
                    Hunk {
                        old_start: 2,
                        new_start: 3,
                        segments: vec![Segment::Change {
                            removed: 1,
                            added: 0,
                        }],
                    },
                ],
            }],
            blocks: Vec::new(),
        }
    }

    fn actions(entries: &[Entry]) -> Vec<(MenuAction, bool)> {
        entries
            .iter()
            .filter_map(|entry| match entry {
                Entry::Item {
                    action, disabled, ..
                } => Some((*action, disabled.is_none())),
                Entry::Separator => None,
            })
            .collect()
    }

    #[test]
    fn links_pin_new_lines_to_the_head_and_old_ones_to_the_base() {
        let links = links(Some("base"));
        assert_eq!(
            links.lines_url("src/a.rs", Side::New, 4, 6).unwrap(),
            format!("https://github.com/acme/api/blob/{HEAD}/src/a.rs#L5-L7")
        );
        assert!(
            links
                .lines_url("src/a.rs", Side::Old, 0, 0)
                .unwrap()
                .contains("/blob/base/src/a.rs#L1")
        );
        assert_eq!(
            links.file_url("src/a.rs").unwrap(),
            format!("https://github.com/acme/api/blob/{HEAD}/src/a.rs")
        );
        let unknown = self::links(None);
        assert!(unknown.lines_url("a", Side::Old, 0, 0).is_err());
    }

    #[test]
    fn hunks_cover_their_new_lines_or_else_their_removed_ones() {
        let diff = diff();
        let hunk = |old_start, new_start| ContextTarget::Hunk {
            file: FileIx(0),
            old_start,
            new_start,
        };
        let lines = target_lines(&hunk(0, 0), &diff).expect("first hunk");
        assert_eq!((lines.side, lines.start, lines.end), (Side::New, 0, 2));
        let removed = target_lines(&hunk(2, 3), &diff).expect("second hunk");
        assert_eq!(
            (removed.side, removed.start, removed.end),
            (Side::Old, 2, 2)
        );
        assert_eq!(target_lines(&hunk(9, 9), &diff), None);
    }

    #[test]
    fn each_target_gets_its_own_entries_and_reasons() {
        let diff = diff();
        let links = links(None);
        let state = MenuState {
            links: Some(&links),
            file_comments: Err("Forgejo has no file comments".into()),
            context: ContextMode::Hunks,
            collapsed: false,
        };
        let old_line = ContextTarget::Line {
            file: FileIx(0),
            side: Side::Old,
            line: 1,
            path: "src/a.rs".into(),
        };
        let lines = target_lines(&old_line, &diff);
        assert_eq!(
            actions(&entries(&old_line, lines.as_ref(), &state)),
            [
                (MenuAction::CopyLink, false),
                (MenuAction::CopyLines, true),
                (MenuAction::Comment, true),
                (MenuAction::OpenFull, true),
                (MenuAction::OpenOnForge, false),
            ],
            "no base commit, no link to an old line"
        );
        let header = ContextTarget::FileHeader {
            file: FileIx(0),
            path: "src/a.rs".into(),
        };
        let header_entries = entries(&header, None, &state);
        let comment = header_entries.iter().find_map(|entry| match entry {
            Entry::Item {
                action: MenuAction::CommentOnFile,
                disabled,
                ..
            } => Some(disabled.clone()),
            _ => None,
        });
        assert_eq!(comment, Some(Some("Forgejo has no file comments".into())));
        assert_eq!(actions(&header_entries).len(), 9);
    }
}
