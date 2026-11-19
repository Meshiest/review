//! Shaped-line cache and the text and runs a log line is shaped from.

use std::collections::HashMap;

use gpui::{
    Font, FontStyle, FontWeight, Hsla, Pixels, ShapedLine, SharedString, StrikethroughStyle,
    TextRun, UnderlineStyle, Window, font, px,
};

use crate::ansi::{Attrs, Style, StyleId};
use crate::buffer::{LogBuffer, LogLine};
use crate::style::{LogStyle, dim};

/// Window edges are rounded to this many columns, so scrolling sideways by a
/// few pixels does not shape the visible lines again.
pub(crate) const COLUMN_STEP: usize = 64;

/// The four faces ANSI text uses.
pub(crate) struct Fonts {
    regular: Font,
    bold: Font,
    italic: Font,
    bold_italic: Font,
}

impl Fonts {
    pub fn new(style: &LogStyle) -> Self {
        let mut regular = font(style.font_family.clone());
        regular.fallbacks = style.font_fallbacks.clone();
        let with = |weight, slant| Font {
            weight,
            style: slant,
            ..regular.clone()
        };
        Self {
            bold: with(FontWeight::BOLD, FontStyle::Normal),
            italic: with(FontWeight::NORMAL, FontStyle::Italic),
            bold_italic: with(FontWeight::BOLD, FontStyle::Italic),
            regular,
        }
    }

    fn face(&self, bold: bool, italic: bool) -> &Font {
        match (bold, italic) {
            (false, false) => &self.regular,
            (true, false) => &self.bold,
            (false, true) => &self.italic,
            (true, true) => &self.bold_italic,
        }
    }

    pub fn regular(&self) -> &Font {
        &self.regular
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum LineKey {
    /// A line's text from column `first_col`; `hint` is the number of lines a
    /// collapsed group hides, 0 for none.
    Text {
        line: u64,
        first_col: u32,
        hint: u32,
    },
    Time(u64),
    Digit(u8),
    Dropped(u64),
}

struct Entry {
    line: ShapedLine,
    /// Some run has a background, which `paint` leaves to
    /// [`ShapedLine::paint_background`].
    has_background: bool,
    last_used: u64,
    /// Columns the text was shaped for.
    columns: usize,
    /// The text went on past `columns`.
    cut: bool,
}

/// Shaped lines kept across frames; those not used in the last frame are
/// dropped, so the cache stays about one screen big.
#[derive(Default)]
pub(crate) struct LineCache {
    entries: HashMap<LineKey, Entry>,
    frame: u64,
    shaped_this_frame: usize,
    runs: Vec<TextRun>,
    text: String,
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

    /// Marks `key` used this frame, shaping it with `make` on a miss or when
    /// the cached line covers fewer than `columns` columns and was cut.
    /// `make` writes the text and runs into the buffers and returns whether
    /// it cut the text at `columns`.
    pub fn ensure(
        &mut self,
        key: LineKey,
        columns: usize,
        font_size: Pixels,
        window: &Window,
        make: impl FnOnce(&mut String, &mut Vec<TextRun>) -> bool,
    ) {
        if let Some(entry) = self.entries.get_mut(&key)
            && (!entry.cut || entry.columns >= columns)
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
            .shape_line(text, font_size, &self.runs, None);
        self.shaped_this_frame += 1;
        let has_background = self.runs.iter().any(|run| run.background_color.is_some());
        self.entries.insert(
            key,
            Entry {
                line,
                has_background,
                last_used: self.frame,
                columns,
                cut,
            },
        );
    }
}

/// The row at the top of a log that has lost its oldest lines.
pub(crate) fn dropped_notice(count: u64) -> String {
    let noun = if count == 1 { "line" } else { "lines" };
    format!("{count} earlier {noun} dropped")
}

/// A run of one look; adjacent runs that look the same are merged.
#[derive(Clone, Copy, PartialEq)]
struct Look {
    color: Hsla,
    background: Option<Hsla>,
    bold: bool,
    italic: bool,
    underline: bool,
}

impl Look {
    fn of(style: Style, base: Hsla, log_style: &LogStyle) -> Self {
        let attrs = style.attrs;
        let bold = attrs.contains(Attrs::BOLD);
        let mut color = log_style.resolve(style.fg, bold).unwrap_or(base);
        let mut background = log_style.resolve(style.bg, false);
        if attrs.contains(Attrs::INVERSE) {
            let front = background.unwrap_or(log_style.background);
            background = Some(color);
            color = front;
        }
        if attrs.contains(Attrs::DIM) {
            color = dim(color);
        }
        Self {
            color,
            background,
            bold,
            italic: attrs.contains(Attrs::ITALIC),
            underline: attrs.contains(Attrs::UNDERLINE),
        }
    }
}

fn run(len: usize, look: Look, fonts: &Fonts) -> TextRun {
    TextRun {
        len,
        font: fonts.face(look.bold, look.italic).clone(),
        color: look.color,
        background_color: look.background,
        underline: look.underline.then(|| UnderlineStyle {
            thickness: px(1.0),
            color: Some(look.color),
            wavy: false,
        }),
        strikethrough: None::<StrikethroughStyle>,
    }
}

/// A plain run of `len` bytes in `color`.
pub(crate) fn plain_run(len: usize, color: Hsla, fonts: &Fonts) -> TextRun {
    run(
        len,
        Look {
            color,
            background: None,
            bold: false,
            italic: false,
            underline: false,
        },
        fonts,
    )
}

/// What to turn a line into text for.
pub(crate) struct LineText<'a> {
    pub line: &'a LogLine<'a>,
    pub buffer: &'a LogBuffer,
    /// The color of text the log gave none.
    pub base: Hsla,
    /// Appended in the notice color, for the lines a collapsed group hides.
    pub hint: Option<&'a str>,
}

/// Writes columns `first..first + columns` of the line to `text`, tabs
/// expanded, with a run per style change; returns whether the line went on
/// past the window.
pub(crate) fn push_line(
    source: &LineText,
    style: &LogStyle,
    fonts: &Fonts,
    first: usize,
    columns: usize,
    text: &mut String,
    runs: &mut Vec<TextRun>,
) -> bool {
    let end = first + columns;
    let tab_width = style.tab_width.max(1) as usize;
    let (mut column, mut span) = (0, 0);
    let mut look = look_at(source, 0, style);
    let mut open: Option<(Look, usize)> = None;
    let mut cut = false;
    for (at, ch) in source.line.text.char_indices() {
        if column >= end {
            cut = true;
            break;
        }
        let mut changed = false;
        while source
            .line
            .spans
            .get(span)
            .is_some_and(|s| s.end as usize <= at)
        {
            span += 1;
            changed = true;
        }
        if changed {
            look = look_at(source, span, style);
        }
        let (width, glyph) = if ch == '\t' {
            ((column / tab_width + 1) * tab_width - column, ' ')
        } else {
            (1, ch)
        };
        // The part of the character's columns inside the window.
        let visible = (column + width).min(end).saturating_sub(column.max(first));
        column += width;
        if visible == 0 {
            continue;
        }
        let before = text.len();
        text.extend(std::iter::repeat_n(glyph, visible));
        let len = text.len() - before;
        match &mut open {
            Some((current, run_len)) if *current == look => *run_len += len,
            _ => {
                if let Some((done, run_len)) = open.replace((look, len)) {
                    runs.push(run(run_len, done, fonts));
                }
            }
        }
    }
    if let Some((done, run_len)) = open {
        runs.push(run(run_len, done, fonts));
    }
    if let Some(hint) = source.hint {
        text.push_str(hint);
        runs.push(plain_run(hint.len(), style.notice_text, fonts));
    }
    cut
}

fn look_at(source: &LineText, span: usize, style: &LogStyle) -> Look {
    let id = source
        .line
        .spans
        .get(span)
        .map_or(StyleId::DEFAULT, |s| s.style);
    Look::of(source.buffer.style(id), source.base, style)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LogParser;
    use crate::style::test_style;

    struct Shaped {
        text: String,
        runs: Vec<(usize, Hsla, bool)>,
        cut: bool,
    }

    fn shape(input: &str, first: usize, columns: usize) -> Shaped {
        let mut buffer = LogBuffer::new();
        buffer.append(&mut LogParser::new(), input.as_bytes());
        let style = test_style();
        let fonts = Fonts::new(&style);
        let line = buffer.line(0).expect("one line");
        let source = LineText {
            line: &line,
            buffer: &buffer,
            base: style.foreground,
            hint: None,
        };
        let (mut text, mut runs) = (String::new(), Vec::new());
        let cut = push_line(
            &source, &style, &fonts, first, columns, &mut text, &mut runs,
        );
        assert_eq!(runs.iter().map(|r| r.len).sum::<usize>(), text.len());
        let runs = runs
            .iter()
            .map(|r| (r.len, r.color, r.background_color.is_some()))
            .collect();
        Shaped { text, runs, cut }
    }

    #[test]
    fn the_notice_counts_the_dropped_lines() {
        assert_eq!(dropped_notice(4096), "4096 earlier lines dropped");
        assert_eq!(dropped_notice(1), "1 earlier line dropped");
    }

    #[test]
    fn tabs_expand_to_stops_and_runs_cover_the_text() {
        let shaped = shape("\tab\tc\n", 0, 400);
        assert_eq!(shaped.text, "        ab      c");
        assert_eq!(shaped.runs.len(), 1);
        assert!(!shaped.cut);
    }

    #[test]
    fn colors_split_runs_and_equal_neighbors_merge() {
        let style = test_style();
        let shaped = shape("\x1b[31mred \x1b[1;31mbold\x1b[0m plain\n", 0, 400);
        assert_eq!(shaped.text, "red bold plain");
        assert_eq!(
            shaped.runs,
            [
                (4, style.ansi[1], false),
                (4, style.ansi[9], false),
                (6, style.foreground, false)
            ]
        );
    }

    #[test]
    fn inverse_swaps_and_background_is_flagged() {
        let style = test_style();
        let shaped = shape("\x1b[7mx\x1b[42my\n", 0, 400);
        assert_eq!(shaped.runs[0], (1, style.background, true));
        assert!(shaped.runs[1].2);
    }

    #[test]
    fn a_window_cuts_the_line_and_says_so() {
        let line = format!("{}\n", "0123456789".repeat(10));
        let shaped = shape(&line, 0, 25);
        assert_eq!(shaped.text, "0123456789012345678901234");
        assert!(shaped.cut);
        let shaped = shape(&line, 30, 25);
        assert_eq!(shaped.text, "0123456789012345678901234");
        assert!(shaped.cut);
        let shaped = shape(&line, 90, 64);
        assert_eq!(shaped.text, "0123456789");
        assert!(!shaped.cut);
    }

    #[test]
    fn a_window_that_starts_inside_a_tab_keeps_its_remaining_columns() {
        let shaped = shape("\tx\n", 5, 64);
        assert_eq!(shaped.text, "   x");
    }

    #[test]
    fn a_hint_is_appended_in_the_notice_color() {
        let mut buffer = LogBuffer::new();
        buffer.append(&mut LogParser::new(), b"title\n");
        let style = test_style();
        let fonts = Fonts::new(&style);
        let line = buffer.line(0).expect("one line");
        let source = LineText {
            line: &line,
            buffer: &buffer,
            base: style.foreground,
            hint: Some(" (3 lines)"),
        };
        let (mut text, mut runs) = (String::new(), Vec::new());
        push_line(&source, &style, &fonts, 0, 400, &mut text, &mut runs);
        assert_eq!(text, "title (3 lines)");
        assert_eq!(runs.last().map(|r| r.color), Some(style.notice_text));
    }
}
