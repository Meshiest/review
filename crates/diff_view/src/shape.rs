//! Shaped-line cache and the text prepared for shaping.

use std::collections::HashMap;
use std::ops::Range;

use diff::{FileIx, HighlightKind, HunkSpan, LineKind, Side, Span};
use gpui::{Font, Hsla, Pixels, ShapedLine, SharedString, TextRun, Window};

use crate::SyntaxPalette;

/// Longest prefix of a line that is shaped; there is no horizontal scroll yet.
pub(crate) const MAX_COLUMNS: usize = 400;
/// Shaping cuts are rounded up to this many columns, so a pane that is
/// resized by a few pixels does not shape its cut lines again.
const COLUMN_STEP: usize = 64;

/// How many columns of code to shape for a pane `visible` columns wide: the
/// visible ones and some slack, rounded up to [`COLUMN_STEP`], at most
/// [`MAX_COLUMNS`]. Shaping only what can be seen keeps long lines cheap,
/// above all in split view where each pane shows half as many.
pub(crate) fn shaped_columns(visible: usize) -> usize {
    (visible + 1)
        .div_ceil(COLUMN_STEP)
        .saturating_mul(COLUMN_STEP)
        .clamp(COLUMN_STEP, MAX_COLUMNS)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum LineKey {
    Code {
        file: FileIx,
        side: Side,
        line: u32,
    },
    Digit(u8),
    Marker(LineKind),
    /// The path of a file; `dim` for a file marked viewed.
    FileHeader {
        file: FileIx,
        dim: bool,
    },
    /// The word next to the viewed checkbox.
    ViewedLabel,
    /// A file's added and removed line counts, `+38 -9`.
    Stats(FileIx),
    /// A hunk header's `@@` line, by the lines it names.
    HunkHeader {
        file: FileIx,
        span: HunkSpan,
    },
}

struct Entry {
    line: ShapedLine,
    /// Some run has a background (emphasized words), which `paint` leaves
    /// to [`ShapedLine::paint_background`].
    has_background: bool,
    last_used: u64,
    /// How good the colors it was shaped with were; see [`Quality`].
    quality: Quality,
    /// The column the text was cut at, if it was longer.
    cut: Option<usize>,
}

/// A line shaped before its blob's highlights arrived is `Plain`; it is
/// shaped again, once, when they do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Quality {
    Plain,
    Highlighted,
}

/// Shaped lines kept across frames, keyed by what they show rather than by
/// row, so a layout switch reuses them. Entries not used in the last frame
/// are dropped, so the cache stays about one screen big.
#[derive(Default)]
pub(crate) struct LineCache {
    entries: HashMap<LineKey, Entry>,
    frame: u64,
    shaped_this_frame: usize,
    runs: Vec<TextRun>,
    text: String,
}

/// The font settings every shaped line shares.
pub(crate) struct ShapeStyle<'a> {
    pub font: &'a Font,
    pub font_size: Pixels,
    pub tab_width: u32,
}

impl LineCache {
    pub fn begin_frame(&mut self) {
        self.frame += 1;
        self.shaped_this_frame = 0;
    }

    pub fn end_frame(&mut self) {
        let frame = self.frame;
        self.entries.retain(|_, entry| entry.last_used == frame);
    }

    /// Forgets every shaped line, e.g. after the colors changed.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn shaped_this_frame(&self) -> usize {
        self.shaped_this_frame
    }

    /// The shaped line and whether it needs its background painted.
    pub fn get(&self, key: LineKey) -> Option<(&ShapedLine, bool)> {
        self.entries
            .get(&key)
            .map(|entry| (&entry.line, entry.has_background))
    }

    /// Marks `key` used this frame, shaping it with `make` on a miss, when
    /// the cached line is of lower `quality`, or when it was cut before
    /// `columns`. `make` writes the display text and its runs into the
    /// buffers, and returns the column it cut the text at, if it did.
    pub fn ensure(
        &mut self,
        key: LineKey,
        quality: Quality,
        columns: usize,
        style: &ShapeStyle,
        window: &Window,
        make: impl FnOnce(&mut String, &mut Vec<TextRun>) -> Option<usize>,
    ) {
        if let Some(entry) = self.entries.get_mut(&key)
            && entry.quality >= quality
            && entry.cut.is_none_or(|cut| cut >= columns)
        {
            entry.last_used = self.frame;
            return;
        }
        self.text.clear();
        self.runs.clear();
        let cut = make(&mut self.text, &mut self.runs);
        let text = SharedString::from(self.text.clone());
        let line = window
            .text_system()
            .shape_line(text, style.font_size, &self.runs, None);
        self.shaped_this_frame += 1;
        let has_background = self.runs.iter().any(|run| run.background_color.is_some());
        self.entries.insert(
            key,
            Entry {
                line,
                has_background,
                last_used: self.frame,
                quality,
                cut,
            },
        );
    }
}

pub(crate) fn run(len: usize, font: &Font, color: Hsla, background: Option<Hsla>) -> TextRun {
    TextRun {
        len,
        font: font.clone(),
        color,
        background_color: background,
        underline: None,
        strikethrough: None,
    }
}

/// One code line to turn into text and runs.
pub(crate) struct Code<'a> {
    pub line: &'a str,
    /// Highlight spans in bytes of `line`; gaps between them are plain.
    pub spans: &'a [Span],
    /// Changed words in bytes of `line`, sorted and disjoint.
    pub emphasis: &'a [Range<u32>],
}

/// How code is turned into runs.
pub(crate) struct CodeStyle<'a> {
    pub font: &'a Font,
    pub palette: &'a SyntaxPalette,
    pub tab_width: u32,
    /// Behind the `emphasis` ranges.
    pub emphasis_background: Hsla,
}

/// Appends `code.line` to `text` with tabs expanded to spaces and cut at
/// `columns` (at most [`MAX_COLUMNS`]), and its runs to `runs`: syntax
/// colors, with the emphasis background behind changed words. Neighboring
/// runs that look the same are merged. Returns the column it cut at, if the
/// line was longer.
pub(crate) fn push_code(
    code: &Code,
    style: &CodeStyle,
    columns: usize,
    text: &mut String,
    runs: &mut Vec<TextRun>,
) -> Option<usize> {
    let limit = columns.min(MAX_COLUMNS);
    let tab_width = style.tab_width.max(1) as usize;
    let start = text.len();
    let (mut column, mut next_span, mut next_emphasis) = (0, 0, 0);
    let mut open: Option<((Hsla, Option<Hsla>), usize)> = None;
    let mut cut = None;
    for (at, ch) in code.line.char_indices() {
        if column >= limit {
            cut = Some(limit);
            break;
        }
        while code
            .spans
            .get(next_span)
            .is_some_and(|s| s.end as usize <= at)
        {
            next_span += 1;
        }
        while code
            .emphasis
            .get(next_emphasis)
            .is_some_and(|r| r.end as usize <= at)
        {
            next_emphasis += 1;
        }
        let kind = code
            .spans
            .get(next_span)
            .filter(|s| s.start as usize <= at)
            .map_or(HighlightKind::Plain, |s| s.kind);
        let emphasized = code
            .emphasis
            .get(next_emphasis)
            .is_some_and(|r| r.start as usize <= at);
        let look = (
            style.palette.color(kind),
            emphasized.then_some(style.emphasis_background),
        );
        let before = text.len();
        if ch == '\t' {
            let next = (column / tab_width + 1) * tab_width;
            text.extend(std::iter::repeat_n(' ', next - column));
            column = next;
        } else {
            text.push(ch);
            column += 1;
        }
        let len = text.len() - before;
        match &mut open {
            Some((current, run_len)) if *current == look => *run_len += len,
            _ => {
                if let Some(((color, background), run_len)) = open.replace((look, len)) {
                    runs.push(run(run_len, style.font, color, background));
                }
            }
        }
    }
    if let Some(((color, background), len)) = open {
        runs.push(run(len, style.font, color, background));
    }
    debug_assert_eq!(
        runs.iter().map(|r| r.len).sum::<usize>(),
        text.len() - start
    );
    cut
}

#[cfg(test)]
#[expect(clippy::single_range_in_vec_init, reason = "one range is the point")]
mod tests {
    use super::*;
    use gpui::font;

    fn color(h: f32) -> Hsla {
        Hsla {
            h,
            s: 1.0,
            l: 0.5,
            a: 1.0,
        }
    }

    fn palette() -> SyntaxPalette {
        let mut palette = SyntaxPalette::uniform(color(0.0));
        palette.keyword = color(0.1);
        palette.string = color(0.4);
        palette
    }

    /// (byte length, color, has the emphasis background) of each run.
    type Shaped = (String, Vec<(usize, Hsla, bool)>);

    fn code_with(line: &str, spans: &[Span], emphasis: &[Range<u32>]) -> Shaped {
        let (mut text, mut runs) = (String::new(), Vec::new());
        let _ = push_code(
            &Code {
                line,
                spans,
                emphasis,
            },
            &CodeStyle {
                font: &font("Consolas"),
                palette: &palette(),
                tab_width: 4,
                emphasis_background: color(0.7),
            },
            MAX_COLUMNS,
            &mut text,
            &mut runs,
        );
        let shape = runs
            .iter()
            .map(|r| (r.len, r.color, r.background_color == Some(color(0.7))))
            .collect();
        (text, shape)
    }

    fn code(line: &str, spans: &[Span]) -> (String, Vec<(usize, Hsla)>) {
        let (text, runs) = code_with(line, spans, &[]);
        (text, runs.into_iter().map(|(len, c, _)| (len, c)).collect())
    }

    fn span(start: u32, end: u32, kind: HighlightKind) -> Span {
        Span { start, end, kind }
    }

    #[test]
    fn tabs_expand_to_stops() {
        let (text, runs) = code("\tab\tc", &[]);
        assert_eq!(text, "    ab  c");
        assert_eq!(runs.len(), 1);
    }

    #[test]
    fn long_lines_are_cut() {
        let (text, runs) = code(&"x".repeat(1000), &[]);
        assert_eq!(text.len(), MAX_COLUMNS);
        assert_eq!(runs, [(MAX_COLUMNS, palette().plain)]);
    }

    #[test]
    fn lines_are_cut_at_the_columns_asked_for() {
        let style = CodeStyle {
            font: &font("Consolas"),
            palette: &palette(),
            tab_width: 4,
            emphasis_background: color(0.7),
        };
        let push = |line: &str, columns| {
            let (mut text, mut runs) = (String::new(), Vec::new());
            let code = Code {
                line,
                spans: &[],
                emphasis: &[],
            };
            let cut = push_code(&code, &style, columns, &mut text, &mut runs);
            (text, cut)
        };
        assert_eq!(push("abcdef", 4), ("abcd".to_owned(), Some(4)));
        assert_eq!(push("abcd", 4), ("abcd".to_owned(), None));
        // A tab counts the columns it covers.
        assert_eq!(push("\tab", 5), ("    a".to_owned(), Some(5)));
        assert_eq!(push(&"x".repeat(1000), 10_000).1, Some(MAX_COLUMNS));
    }

    #[test]
    fn shaped_columns_round_up_and_stay_in_range() {
        assert_eq!(shaped_columns(0), COLUMN_STEP);
        assert_eq!(shaped_columns(40), COLUMN_STEP);
        assert_eq!(shaped_columns(63), COLUMN_STEP);
        assert_eq!(shaped_columns(64), 2 * COLUMN_STEP);
        assert_eq!(shaped_columns(100), 2 * COLUMN_STEP);
        assert_eq!(shaped_columns(5_000), MAX_COLUMNS);
    }

    #[test]
    fn spans_become_runs_in_output_bytes() {
        // `\tlet s = "é"`: the tab widens to 4 bytes, `é` is 2 bytes.
        let line = "\tlet s = \"é\"";
        let spans = [
            span(1, 4, HighlightKind::Keyword),
            span(9, 13, HighlightKind::String),
        ];
        let (text, runs) = code(line, &spans);
        assert_eq!(text, "    let s = \"é\"");
        let p = palette();
        assert_eq!(
            runs,
            [(4, p.plain), (3, p.keyword), (5, p.plain), (4, p.string)]
        );
        assert_eq!(runs.iter().map(|r| r.0).sum::<usize>(), text.len());
    }

    #[test]
    fn same_color_neighbors_merge() {
        let spans = [
            span(0, 1, HighlightKind::Punctuation),
            span(1, 2, HighlightKind::Operator),
        ];
        let (_, runs) = code("+-x", &spans);
        assert_eq!(runs.len(), 1, "uniform colors collapse into one run");
    }

    #[test]
    fn runs_cover_the_text() {
        let spans = [span(2, 5, HighlightKind::Keyword)];
        for line in ["", "x", "let a = b;", "αβγ δεζ ηθι", &"word ".repeat(200)] {
            let (text, runs) = code(line, &spans);
            assert_eq!(
                runs.iter().map(|r| r.0).sum::<usize>(),
                text.len(),
                "{line:?}"
            );
        }
    }

    #[test]
    fn emphasis_sits_under_the_syntax_colors() {
        // `let x = 1;` with `x = 1` emphasized and `let` a keyword.
        let spans = [span(0, 3, HighlightKind::Keyword)];
        let (text, runs) = code_with("let x = 1;", &spans, &[4..9]);
        assert_eq!(text, "let x = 1;");
        let p = palette();
        assert_eq!(
            runs,
            [
                (3, p.keyword, false),
                (1, p.plain, false),
                (5, p.plain, true),
                (1, p.plain, false)
            ]
        );
        // A keyword inside the emphasis keeps its color and gains the background.
        let (_, runs) = code_with("let x", &spans, &[0..5]);
        assert_eq!(runs, [(3, p.keyword, true), (2, p.plain, true)]);
    }

    #[test]
    fn emphasis_follows_tab_expansion() {
        // The tab is 1 byte in the line and 4 in the text; `b` is emphasized.
        let (text, runs) = code_with("\ta b", &[], &[3..4]);
        assert_eq!(text, "    a b");
        let p = palette();
        assert_eq!(runs, [(6, p.plain, false), (1, p.plain, true)]);
        // An emphasized tab paints its whole width.
        let (_, runs) = code_with("\ta", &[], &[0..1]);
        assert_eq!(runs, [(4, p.plain, true), (1, p.plain, false)]);
    }

    #[test]
    fn emphasis_past_the_cut_is_ignored() {
        let line = "x".repeat(MAX_COLUMNS + 50);
        let (text, runs) = code_with(&line, &[], &[10..20, 380..450]);
        assert_eq!(text.len(), MAX_COLUMNS);
        assert_eq!(runs.iter().map(|r| r.0).sum::<usize>(), MAX_COLUMNS);
        assert_eq!(runs.last().map(|r| r.2), Some(true));
    }
}
