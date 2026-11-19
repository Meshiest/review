//! Text selection in the diff: carets in source lines, how a click or drag
//! grows them, what they paint and what they copy.
//!
//! A caret names a source line and a char offset in it, never a pixel or a
//! tab-expanded column, so a selection outlives scrolling, re-shaping and a
//! change of tab width. Rows are cached beside it for ordering and are found
//! again when the rows are re-projected (see [`Selection::rebase`]).

use diff::{Diff, DiffList, FileIx, Row, Side, Text};

/// A code line: the side of a file's blob and a 0-based line in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CodeLine {
    pub file: FileIx,
    pub side: Side,
    pub line: u32,
}

/// The code line `row` shows to a selection locked to `lock` (split view's
/// side); `None` for headers, blocks and a half with no line.
pub(crate) fn code_line(row: Row, lock: Option<Side>) -> Option<CodeLine> {
    match row {
        Row::Line {
            file,
            new: Some(line),
            ..
        } => Some(CodeLine {
            file,
            side: Side::New,
            line,
        }),
        Row::Line {
            file,
            old: Some(line),
            ..
        } => Some(CodeLine {
            file,
            side: Side::Old,
            line,
        }),
        Row::Pair { file, left, right } => {
            let side = lock?;
            let shown = match side {
                Side::Old => left,
                Side::New => right,
            }?;
            Some(CodeLine {
                file,
                side,
                line: shown.line,
            })
        }
        _ => None,
    }
}

pub(crate) fn blob(diff: &Diff, at: CodeLine) -> &Text {
    let file = diff.file(at.file);
    match at.side {
        Side::Old => &file.old,
        Side::New => &file.new,
    }
}

/// A position between two chars of a code line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Caret {
    pub at: CodeLine,
    /// Chars from the start of the line.
    pub col: u32,
    /// The row showing the line, as of the last projection.
    pub row: usize,
}

impl Caret {
    fn key(&self) -> (usize, u32) {
        (self.row, self.col)
    }
}

/// The stretch a click selects, and so what a drag grows by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unit {
    Char,
    Word,
    Line,
}

impl Unit {
    /// What `clicks` presses in a row select.
    pub fn of_clicks(clicks: usize) -> Self {
        match clicks {
            0 | 1 => Self::Char,
            2 => Self::Word,
            _ => Self::Line,
        }
    }
}

/// A stretch of one line, as two carets with the first not after the second.
pub(crate) type Stretch = (Caret, Caret);

/// A selection: the unit the press took, fixed, and the unit under the
/// pointer, which follows it. The selected text runs from the earlier start
/// to the later end of the two.
#[derive(Clone, Debug)]
pub(crate) struct Selection {
    anchor: Stretch,
    head: Stretch,
    unit: Unit,
    /// Split view: the only side the selection takes lines from.
    lock: Option<Side>,
}

impl Selection {
    pub fn new(anchor: Stretch, unit: Unit, lock: Option<Side>) -> Self {
        Self {
            anchor,
            head: anchor,
            unit,
            lock,
        }
    }

    pub fn unit(&self) -> Unit {
        self.unit
    }

    pub fn lock(&self) -> Option<Side> {
        self.lock
    }

    pub fn anchor_row(&self) -> usize {
        self.anchor.0.row
    }

    /// Moves the end that follows the pointer.
    pub fn drag(&mut self, head: Stretch) {
        self.head = head;
    }

    /// The selected stretch, first caret first.
    pub fn range(&self) -> Stretch {
        let (head_start, head_end) = self.head;
        if head_start.key() < self.anchor.0.key() {
            (head_start, self.anchor.1)
        } else {
            (self.anchor.0, head_end)
        }
    }

    pub fn is_empty(&self) -> bool {
        let (start, end) = self.range();
        start.key() == end.key()
    }

    /// Whether `row` is within the selection's rows.
    pub fn covers(&self, row: usize) -> bool {
        let (start, end) = self.range();
        (start.row..=end.row).contains(&row)
    }

    /// What of the code line on `row`, `chars` chars long, is selected.
    pub fn span(&self, row: usize, chars: u32) -> Option<Span> {
        let (start, end) = self.range();
        if row < start.row || row > end.row {
            return None;
        }
        let from = if row == start.row { start.col } else { 0 }.min(chars);
        let (to, newline) = if row == end.row {
            (end.col.min(chars), false)
        } else {
            (chars, true)
        };
        (from < to || newline).then_some(Span { from, to, newline })
    }

    /// The selected source text: each line as it is in its file, joined by
    /// the line ending of the file it came from. Headers and blocks between
    /// the ends, and the other side in split view, contribute nothing.
    pub fn text(&self, list: &DiffList) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let (start, end) = self.range();
        let diff = list.diff();
        let mut out = String::new();
        let mut ending = "";
        for row in start.row..=end.row {
            let Some(at) = code_line(list.rows()[row], self.lock) else {
                continue;
            };
            let text = blob(diff, at);
            let line = text.line(at.line);
            let from = if row == start.row { start.col } else { 0 };
            let to = if row == end.row { end.col } else { u32::MAX };
            out.push_str(ending);
            out.push_str(slice_chars(&line, from, to));
            ending = if text.uses_crlf() { "\r\n" } else { "\n" };
        }
        Some(out)
    }

    /// Finds the rows of the carets again after the rows were projected anew.
    /// False when a line is gone.
    pub fn rebase(&mut self, list: &DiffList) -> bool {
        let mut carets = [
            &mut self.anchor.0,
            &mut self.anchor.1,
            &mut self.head.0,
            &mut self.head.1,
        ];
        let mut done = [false; 4];
        for (row, shown) in list.rows().iter().enumerate() {
            let Some(at) = code_line(*shown, self.lock) else {
                continue;
            };
            for (caret, done) in carets.iter_mut().zip(&mut done) {
                if caret.at == at {
                    caret.row = row;
                    *done = true;
                }
            }
            if done.iter().all(|&d| d) {
                return true;
            }
        }
        false
    }
}

/// The selected chars of one line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Span {
    pub from: u32,
    pub to: u32,
    /// The selection goes on past the end of the line.
    pub newline: bool,
}

/// The chars `from..to` of `line`, cut at its ends.
fn slice_chars(line: &str, from: u32, to: u32) -> &str {
    let byte = |col: u32| {
        line.char_indices()
            .nth(col as usize)
            .map_or(line.len(), |(at, _)| at)
    };
    let start = byte(from);
    let end = byte(to).max(start);
    &line[start..end]
}

fn advance(column: usize, ch: char, tab_width: u32) -> usize {
    if ch == '\t' {
        let tab = tab_width.max(1) as usize;
        (column / tab + 1) * tab
    } else {
        column + 1
    }
}

/// The column, with tabs expanded, at which char `index` of `line` starts.
pub(crate) fn display_col(line: &str, tab_width: u32, index: u32) -> usize {
    line.chars()
        .take(index as usize)
        .fold(0, |column, ch| advance(column, ch, tab_width))
}

/// A point on a line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Located {
    /// The char the point is over; the line's length past its end.
    pub cell: u32,
    /// The gap between chars nearest to it.
    pub boundary: u32,
}

/// Which char of `line` the display column `at` (fractional, tabs expanded)
/// is over, and which gap between chars it is nearest.
pub(crate) fn locate(line: &str, tab_width: u32, at: f32) -> Located {
    let (mut column, mut index) = (0, 0u32);
    for ch in line.chars() {
        let next = advance(column, ch, tab_width);
        if at < next as f32 {
            let middle = (column + next) as f32 / 2.0;
            let boundary = if at < middle { index } else { index + 1 };
            return Located {
                cell: index,
                boundary,
            };
        }
        column = next;
        index += 1;
    }
    Located {
        cell: index,
        boundary: index,
    }
}

#[derive(PartialEq, Eq)]
enum Class {
    Word,
    Space,
    Other,
}

fn class(ch: char) -> Class {
    if ch.is_alphanumeric() || ch == '_' {
        Class::Word
    } else if ch.is_whitespace() {
        Class::Space
    } else {
        Class::Other
    }
}

/// The run of chars of one kind (word, blanks, punctuation) around char
/// `cell` of `line`; empty past its end.
pub(crate) fn word_range(line: &str, cell: u32) -> (u32, u32) {
    let chars: Vec<char> = line.chars().collect();
    let cell = cell as usize;
    let Some(&here) = chars.get(cell) else {
        let end = chars.len() as u32;
        return (end, end);
    };
    let kind = class(here);
    let start = chars[..cell]
        .iter()
        .rposition(|&ch| class(ch) != kind)
        .map_or(0, |at| at + 1);
    let end = chars[cell..]
        .iter()
        .position(|&ch| class(ch) != kind)
        .map_or(chars.len(), |at| cell + at);
    (start as u32, end as u32)
}

/// How many chars `line` has.
pub(crate) fn char_count(line: &str) -> u32 {
    line.chars().count() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_columns_expand_tabs_to_stops() {
        assert_eq!(display_col("\tab", 4, 0), 0);
        assert_eq!(display_col("\tab", 4, 1), 4);
        assert_eq!(display_col("\tab", 4, 3), 6);
        assert_eq!(display_col("a\tb", 4, 2), 4);
        assert_eq!(display_col("ab", 4, 99), 2);
    }

    #[test]
    fn a_point_is_over_a_char_and_nearest_a_gap() {
        let at = |line, column| locate(line, 4, column);
        assert_eq!(
            at("abc", 0.2),
            Located {
                cell: 0,
                boundary: 0
            }
        );
        assert_eq!(
            at("abc", 0.7),
            Located {
                cell: 0,
                boundary: 1
            }
        );
        assert_eq!(
            at("abc", 2.4),
            Located {
                cell: 2,
                boundary: 2
            }
        );
        // Past the end, and before the start.
        assert_eq!(
            at("abc", 9.0),
            Located {
                cell: 3,
                boundary: 3
            }
        );
        assert_eq!(
            at("abc", -3.0),
            Located {
                cell: 0,
                boundary: 0
            }
        );
        // A tab covers four columns: `x` after it starts at column 4.
        assert_eq!(at("\tx", 3.9).cell, 0);
        assert_eq!(at("\tx", 4.1).cell, 1);
        assert_eq!(at("\tx", 1.0).boundary, 0);
        assert_eq!(at("\tx", 3.0).boundary, 1);
    }

    #[test]
    fn a_word_is_a_run_of_one_kind_of_char() {
        let line = "let foo_bar = a::b;";
        assert_eq!(word_range(line, 5), (4, 11), "foo_bar");
        assert_eq!(word_range(line, 3), (3, 4), "the blank");
        assert_eq!(word_range(line, 15), (15, 17), "the ::");
        assert_eq!(word_range(line, 0), (0, 3));
        assert_eq!(word_range(line, 99), (19, 19), "past the end");
        assert_eq!(word_range("", 0), (0, 0));
        assert_eq!(word_range("  \tx", 1), (0, 3));
    }

    #[test]
    fn slices_cut_at_chars_not_bytes() {
        assert_eq!(slice_chars("aé日b", 1, 3), "é日");
        assert_eq!(slice_chars("abc", 2, u32::MAX), "c");
        assert_eq!(slice_chars("abc", 5, 9), "");
        assert_eq!(slice_chars("abc", 2, 1), "");
        assert_eq!(char_count("aé日"), 3);
    }

    fn caret(row: usize, col: u32) -> Caret {
        Caret {
            at: CodeLine {
                file: FileIx(0),
                side: Side::New,
                line: row as u32,
            },
            col,
            row,
        }
    }

    fn at(row: usize, col: u32) -> Stretch {
        (caret(row, col), caret(row, col))
    }

    #[test]
    fn the_range_runs_from_the_earlier_end_to_the_later() {
        let mut selection = Selection::new(at(5, 3), Unit::Char, None);
        assert!(selection.is_empty());
        selection.drag(at(7, 1));
        assert_eq!(selection.range(), (caret(5, 3), caret(7, 1)));
        selection.drag(at(2, 4));
        assert_eq!(selection.range(), (caret(2, 4), caret(5, 3)));
        selection.drag(at(5, 3));
        assert!(selection.is_empty());
    }

    #[test]
    fn a_word_press_keeps_its_word_when_the_drag_goes_either_way() {
        let word = (caret(5, 4), caret(5, 9));
        let mut selection = Selection::new(word, Unit::Word, None);
        selection.drag((caret(5, 4), caret(5, 9)));
        assert_eq!(selection.range(), word);
        selection.drag((caret(8, 0), caret(8, 3)));
        assert_eq!(selection.range(), (caret(5, 4), caret(8, 3)));
        selection.drag((caret(3, 2), caret(3, 6)));
        assert_eq!(selection.range(), (caret(3, 2), caret(5, 9)));
    }

    #[test]
    fn a_span_covers_the_ends_partly_and_the_rows_between_fully() {
        let mut selection = Selection::new(at(2, 3), Unit::Char, None);
        selection.drag(at(4, 2));
        let span = |row, chars| selection.span(row, chars);
        assert_eq!(span(1, 9), None);
        assert_eq!(
            span(2, 9),
            Some(Span {
                from: 3,
                to: 9,
                newline: true
            })
        );
        assert_eq!(
            span(3, 5),
            Some(Span {
                from: 0,
                to: 5,
                newline: true
            })
        );
        assert_eq!(
            span(3, 0),
            Some(Span {
                from: 0,
                to: 0,
                newline: true
            })
        );
        assert_eq!(
            span(4, 9),
            Some(Span {
                from: 0,
                to: 2,
                newline: false
            })
        );
        assert_eq!(span(5, 9), None);
    }
}
