//! The code an inline thread is on, for the Conversation's expanded threads:
//! the hunk lines up to the anchor (a few above it), on the side the thread
//! was written on, with the commented lines marked and syntax colors from
//! the diff highlighter.
//!
//! Lines come from the pull request's diff, which the opener built from the
//! local git objects; an outdated thread's original lines are read from the
//! commit it was written on when the object store has it. Nothing here asks
//! the network, and [`compute`] runs off the main thread.

use std::collections::HashMap;
use std::sync::Arc;

use diff::{FileDiff, HighlightKind, Highlighter, Highlights, Language, Segment, Side, Text};
use forge::model::{Position, Thread, ThreadId};
use gpui::{
    App, Div, Global, HighlightStyle, IntoElement as _, ParentElement as _, SharedString,
    Styled as _, StyledText, div, px,
};
use gpui_kit::component::{h_flex, v_flex};
use theme::Theme;

use crate::ui::TextExt as _;

/// Lines shown above the first commented line.
pub const ABOVE: u32 = 3;
/// Columns a tab advances to.
const TAB: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Added,
    Removed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodeLine {
    /// 1-based, on the excerpt's side.
    pub number: u32,
    pub text: SharedString,
    /// Byte ranges of `text` and their kinds.
    pub spans: Vec<(std::ops::Range<usize>, HighlightKind)>,
    pub kind: LineKind,
    /// One of the lines the thread is on.
    pub marked: bool,
}

/// The lines of one thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Excerpt {
    pub side: Side,
    pub lines: Vec<CodeLine>,
    /// Read from the commit the thread was written on, not the current diff.
    pub original: bool,
}

/// Where a thread's lines are, 0-based and inclusive, on one side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Span {
    side: Side,
    first: u32,
    last: u32,
}

fn side_of(side: forge::model::Side) -> Side {
    match side {
        forge::model::Side::Old => Side::Old,
        forge::model::Side::New => Side::New,
    }
}

fn span_of(position: &Position) -> Option<Span> {
    let last = position.line.line.checked_sub(1)?;
    let first = position
        .start
        .filter(|start| start.side == position.line.side && start.line <= position.line.line)
        .and_then(|start| start.line.checked_sub(1))
        .unwrap_or(last);
    Some(Span {
        side: side_of(position.line.side),
        first,
        last,
    })
}

/// The kind of each line of `side` a hunk shows, by line.
fn kinds(file: &FileDiff, side: Side, line: u32) -> Option<(u32, Vec<LineKind>)> {
    let hunk = file.hunks.iter().find(|hunk| hunk.shows(side, line))?;
    let start = match side {
        Side::Old => hunk.old_start,
        Side::New => hunk.new_start,
    };
    let mut out = Vec::new();
    for segment in &hunk.segments {
        match (*segment, side) {
            (Segment::Context(n), _) => out.extend((0..n).map(|_| LineKind::Context)),
            (Segment::Change { removed, .. }, Side::Old) => {
                out.extend((0..removed).map(|_| LineKind::Removed));
            }
            (Segment::Change { added, .. }, Side::New) => {
                out.extend((0..added).map(|_| LineKind::Added));
            }
        }
    }
    Some((start, out))
}

/// `line` with tabs expanded, and its highlight spans moved to match.
fn expand(
    line: &str,
    spans: &[diff::Span],
) -> (String, Vec<(std::ops::Range<usize>, HighlightKind)>) {
    let mut out = String::with_capacity(line.len());
    // Where each byte of `line` lands in `out`, plus the end.
    let mut at = Vec::with_capacity(line.len() + 1);
    let mut column = 0;
    for ch in line.chars() {
        for _ in 0..ch.len_utf8() {
            at.push(out.len());
        }
        if ch == '\t' {
            let width = TAB - column % TAB;
            out.extend(std::iter::repeat_n(' ', width));
            column += width;
        } else {
            out.push(ch);
            column += 1;
        }
    }
    at.push(out.len());
    let mapped = spans
        .iter()
        .filter_map(|span| {
            let start = *at.get(span.start as usize)?;
            let end = *at.get(span.end as usize)?;
            (start < end).then_some((start..end, span.kind))
        })
        .collect();
    (out, mapped)
}

fn line(
    text: &Text,
    ix: u32,
    highlights: Option<&Highlights>,
) -> (String, Vec<(std::ops::Range<usize>, HighlightKind)>) {
    let raw = text.line(ix);
    let spans = highlights.map_or(&[][..], |h| h.line(ix));
    expand(&raw, spans)
}

/// The excerpt of `span` from a diff file: the hunk's lines from a few
/// above the first commented line to the last. `None` when the side does
/// not have those lines.
fn from_diff(file: &FileDiff, span: Span, highlights: Option<&Highlights>) -> Option<Excerpt> {
    let text = match span.side {
        Side::Old => &file.old,
        Side::New => &file.new,
    };
    if span.last >= text.line_count() {
        return None;
    }
    let hunk = kinds(file, span.side, span.last);
    let mut start = span.first.saturating_sub(ABOVE);
    if let Some((hunk_start, _)) = &hunk {
        start = start.max(*hunk_start);
    }
    let lines = (start..=span.last)
        .map(|ix| {
            let kind = hunk
                .as_ref()
                .and_then(|(hunk_start, kinds)| kinds.get(ix.checked_sub(*hunk_start)? as usize))
                .copied()
                .unwrap_or(LineKind::Context);
            let (text, spans) = line(text, ix, highlights);
            CodeLine {
                number: ix + 1,
                text: text.into(),
                spans,
                kind,
                marked: (span.first..=span.last).contains(&ix),
            }
        })
        .collect();
    Some(Excerpt {
        side: span.side,
        lines,
        original: false,
    })
}

/// The excerpt of `span` from a whole file as it was at some commit.
fn from_text(text: &Text, span: Span, highlights: Option<&Highlights>) -> Option<Excerpt> {
    if span.last >= text.line_count() {
        return None;
    }
    let lines = (span.first.saturating_sub(ABOVE)..=span.last)
        .map(|ix| {
            let (text, spans) = line(text, ix, highlights);
            CodeLine {
                number: ix + 1,
                text: text.into(),
                spans,
                kind: LineKind::Context,
                marked: (span.first..=span.last).contains(&ix),
            }
        })
        .collect();
    Some(Excerpt {
        side: span.side,
        lines,
        original: true,
    })
}

/// Reads a file of a pull request at `commit` from the object store: the new
/// side at that commit, or the base side.
pub trait Original {
    fn read(&self, path: &str, side: Side, commit: &str) -> Option<Text>;
}

/// A pull request's files at other commits, from its object store.
pub struct StoreOriginal {
    pub store: repo::Store,
    pub number: u64,
}

impl Original for StoreOriginal {
    fn read(&self, path: &str, side: Side, commit: &str) -> Option<Text> {
        let commit = self.store.resolve_commit(commit).ok()?;
        let (base, _) = self.store.pr_range(self.number).ok()?;
        let options = repo::FileListOptions {
            renames: false,
            detect_binary: false,
        };
        let list = self.store.file_list(Some(base), commit, options).ok()?;
        let change = list.files.iter().find(|file| file.path == path)?;
        let entry = match side {
            Side::Old => change.old,
            Side::New => change.new,
        }
        .filter(|entry| entry.mode != repo::FileMode::Submodule)?;
        self.store.blob(entry.id).ok()?.to_text()
    }
}

/// What [`compute`] works from.
pub struct Job {
    pub diff: Arc<diff::Diff>,
    pub threads: Arc<[Thread]>,
    pub highlighter: Highlighter,
}

/// The excerpt of every anchored thread that has one.
#[must_use]
pub fn compute(job: &Job, original: Option<&dyn Original>) -> HashMap<ThreadId, Arc<Excerpt>> {
    let mut cache: HashMap<(usize, Side), Option<Arc<Highlights>>> = HashMap::new();
    let mut highlights = |file: usize, side: Side| {
        cache
            .entry((file, side))
            .or_insert_with(|| {
                let diff_file = &job.diff.files[file];
                let language = Language::detect(&diff_file.path)?;
                let text = match side {
                    Side::Old => &diff_file.old,
                    Side::New => &diff_file.new,
                };
                Some(job.highlighter.highlight_now(text, language))
            })
            .clone()
    };
    let mut out = HashMap::new();
    for thread in job.threads.iter() {
        let Some(anchor) = &thread.anchor else {
            continue;
        };
        let file = job.diff.files.iter().position(|f| {
            f.path == anchor.path || anchor.old_path.as_deref() == Some(f.path.as_str())
        });
        let current = anchor.position.as_ref().filter(|_| !thread.outdated);
        if let (Some(position), Some(file)) = (current, file)
            && let Some(span) = span_of(position)
            && let Some(excerpt) = from_diff(
                &job.diff.files[file],
                span,
                highlights(file, span.side).as_deref(),
            )
        {
            out.insert(thread.id.clone(), Arc::new(excerpt));
            continue;
        }
        let Some(position) = anchor.original.as_ref().or(anchor.position.as_ref()) else {
            continue;
        };
        let Some(span) = span_of(position) else {
            continue;
        };
        let read = original
            .zip(position.commit.as_ref())
            .and_then(|(original, commit)| {
                let text = original.read(&anchor.path, span.side, commit.as_str())?;
                let language = Language::detect(&anchor.path);
                let colors =
                    language.map(|language| job.highlighter.highlight_now(&text, language));
                from_text(&text, span, colors.as_deref())
            });
        // Without the commit, the lines the current diff has at that place.
        let excerpt = read.or_else(|| {
            let file = file?;
            from_diff(
                &job.diff.files[file],
                span,
                highlights(file, span.side).as_deref(),
            )
            .map(|excerpt| Excerpt {
                original: false,
                ..excerpt
            })
        });
        if let Some(excerpt) = excerpt {
            out.insert(thread.id.clone(), Arc::new(excerpt));
        }
    }
    out
}

/// One highlighter for the Conversation's code, with its own cache.
pub struct SharedHighlighter(pub Highlighter);

impl Global for SharedHighlighter {}

/// The app's code highlighter for excerpts, made on first use.
pub fn highlighter(cx: &mut App) -> Highlighter {
    if let Some(shared) = cx.try_global::<SharedHighlighter>() {
        return shared.0.clone();
    }
    let highlighter = Highlighter::new(1);
    cx.set_global(SharedHighlighter(highlighter.clone()));
    highlighter
}

/// The excerpt drawn as code: line numbers, the side's tint on changed
/// lines, an accent edge on the commented ones, syntax colors.
pub fn render(excerpt: &Excerpt, t: &Theme) -> Div {
    let c = &t.colors;
    let palette = crate::diff_style::diff_style(t).palette;
    let width = excerpt
        .lines
        .last()
        .map_or(1, |line| line.number.to_string().len());
    v_flex()
        .w_full()
        .min_w_0()
        .overflow_hidden()
        .rounded(t.radius.md)
        .border_1()
        .border_color(c.border)
        .bg(c.surface)
        .font_family(t.fonts.buffer.family())
        .text_size(t.fonts.buffer.size_px())
        .line_height(t.fonts.buffer.line_height_px())
        .children(excerpt.lines.iter().map(|line| {
            let (background, gutter, sign) = match line.kind {
                LineKind::Added => (c.diff_add_bg, c.diff_add_gutter, "+"),
                LineKind::Removed => (c.diff_remove_bg, c.diff_remove_gutter, "-"),
                LineKind::Context => (c.surface, c.gutter_bg, " "),
            };
            let styles = line.spans.iter().map(|(range, kind)| {
                (
                    range.clone(),
                    HighlightStyle {
                        color: Some(palette.color(*kind)),
                        ..HighlightStyle::default()
                    },
                )
            });
            let code = StyledText::new(line.text.clone()).with_highlights(styles);
            h_flex()
                .w_full()
                .bg(background)
                .border_l_2()
                .border_color(if line.marked {
                    c.accent
                } else {
                    gpui::transparent_black()
                })
                .child(
                    div()
                        .flex_none()
                        .px(px(6.0))
                        .bg(gutter)
                        .text_color(if line.marked { c.text } else { c.gutter_text })
                        .child(format!("{:>width$}", line.number)),
                )
                .child(
                    div()
                        .flex_none()
                        .w(px(14.0))
                        .text_color(c.gutter_text)
                        .child(sign),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_color(palette.plain)
                        .child(code),
                )
                .into_any_element()
        }))
}

/// Shown where the code of a thread cannot be read.
pub fn missing(t: &Theme, text: &'static str) -> Div {
    div().small(t).text_color(t.colors.text_muted).child(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use diff::Hunk;
    use forge::model::{Anchor, LineRef, Resolution};

    fn file() -> FileDiff {
        let old: String = (1..=12).map(|n| format!("old {n}\n")).collect();
        let new: String = (1..=12).map(|n| format!("new {n}\n")).collect();
        FileDiff {
            path: "src/a.rs".into(),
            old: Text::new(old),
            new: Text::new(new),
            // Lines 3..=5 the same, then 2 removed and 3 added, then 2 the same.
            hunks: vec![Hunk {
                old_start: 2,
                new_start: 2,
                segments: vec![
                    Segment::Context(3),
                    Segment::Change {
                        removed: 2,
                        added: 3,
                    },
                    Segment::Context(2),
                ],
            }],
        }
    }

    fn thread(side: forge::model::Side, start: Option<u32>, line: u32, outdated: bool) -> Thread {
        let position = Position {
            line: LineRef { side, line },
            start: start.map(|line| LineRef { side, line }),
            commit: None,
        };
        Thread {
            id: ThreadId::new("t"),
            anchor: Some(Anchor {
                path: "src/a.rs".into(),
                old_path: None,
                position: (!outdated).then(|| position.clone()),
                original: outdated.then_some(position),
            }),
            resolution: Resolution::Unresolved,
            outdated,
            comments: Vec::new(),
        }
    }

    fn excerpt(thread: Thread) -> Option<Excerpt> {
        let job = Job {
            diff: Arc::new(diff::Diff {
                files: vec![file()],
                blocks: Vec::new(),
            }),
            threads: Arc::new([thread]),
            highlighter: Highlighter::new(1),
        };
        compute(&job, None)
            .remove(&ThreadId::new("t"))
            .map(|e| (*e).clone())
    }

    fn summary(excerpt: &Excerpt) -> Vec<(u32, LineKind, bool)> {
        excerpt
            .lines
            .iter()
            .map(|line| (line.number, line.kind, line.marked))
            .collect()
    }

    #[test]
    fn a_new_side_thread_shows_three_lines_above_within_its_hunk() {
        use LineKind::{Added, Context};
        let excerpt = excerpt(thread(forge::model::Side::New, None, 8, false)).expect("lines");
        assert_eq!(excerpt.side, Side::New);
        assert_eq!(
            summary(&excerpt),
            [
                (5, Context, false),
                (6, Added, false),
                (7, Added, false),
                (8, Added, true)
            ]
        );
        assert_eq!(excerpt.lines[3].text.as_ref(), "new 8");
        // Near the top of the hunk, it starts at the hunk.
        let top = self::excerpt(thread(forge::model::Side::New, None, 4, false)).expect("lines");
        assert_eq!(top.lines.first().map(|l| l.number), Some(3));
    }

    #[test]
    fn ranges_mark_every_line_and_old_side_threads_show_removed_lines() {
        use LineKind::{Added, Context, Removed};
        let range = excerpt(thread(forge::model::Side::New, Some(6), 7, false)).expect("lines");
        assert_eq!(
            summary(&range),
            [
                (3, Context, false),
                (4, Context, false),
                (5, Context, false),
                (6, Added, true),
                (7, Added, true)
            ]
        );
        let old = excerpt(thread(forge::model::Side::Old, None, 7, false)).expect("lines");
        assert_eq!(old.side, Side::Old);
        assert_eq!(
            old.lines.last().map(|l| (l.kind, l.text.to_string())),
            Some((Removed, "old 7".into()))
        );
        assert!(old.lines.iter().all(|l| l.kind != Added));
    }

    #[test]
    fn an_outdated_thread_without_its_commit_falls_back_to_the_diff() {
        let outdated = excerpt(thread(forge::model::Side::New, None, 8, true)).expect("lines");
        assert!(!outdated.original);
        assert!(excerpt(thread(forge::model::Side::New, None, 400, true)).is_none());
    }

    struct Fixed(Text);

    impl Original for Fixed {
        fn read(&self, path: &str, side: Side, commit: &str) -> Option<Text> {
            (path == "src/a.rs" && side == Side::New && commit == "abc").then(|| self.0.clone())
        }
    }

    #[test]
    fn an_outdated_thread_reads_its_original_commit() {
        let mut thread = thread(forge::model::Side::New, None, 2, true);
        if let Some(original) = thread.anchor.as_mut().and_then(|a| a.original.as_mut()) {
            original.commit = Some(forge::model::Sha::new("abc"));
        }
        let job = Job {
            diff: Arc::new(diff::Diff {
                files: vec![file()],
                blocks: Vec::new(),
            }),
            threads: Arc::new([thread]),
            highlighter: Highlighter::new(1),
        };
        let fixed = Fixed(Text::new("first\n\tsecond\n"));
        let got = compute(&job, Some(&fixed));
        let excerpt = got.get(&ThreadId::new("t")).expect("an excerpt");
        assert!(excerpt.original);
        assert_eq!(
            excerpt.lines[1].text.as_ref(),
            "    second",
            "tabs expanded"
        );
        assert!(excerpt.lines[1].marked);
    }

    #[test]
    fn tabs_move_the_highlight_spans() {
        let (text, spans) = expand(
            "\tfn x",
            &[diff::Span {
                start: 1,
                end: 3,
                kind: HighlightKind::Keyword,
            }],
        );
        assert_eq!(text, "    fn x");
        assert_eq!(spans, [(4..6, HighlightKind::Keyword)]);
    }
}
