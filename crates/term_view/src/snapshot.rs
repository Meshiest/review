//! The visible grid, copied out of the emulator as styled runs.
//!
//! Taking the copy is the only work done under the terminal lock; shaping and
//! painting happen afterward. Runs of cells with the same style become one
//! entry, so painting shapes one string per row rather than one per cell.

use gpui::Rgba;
use term::alacritty_terminal::Term;
use term::alacritty_terminal::event::EventListener;
use term::alacritty_terminal::grid::Dimensions;
use term::alacritty_terminal::grid::Grid;
use term::alacritty_terminal::index::{Column, Line, Point};
use term::alacritty_terminal::selection::SelectionRange;
use term::alacritty_terminal::term::cell::{Cell, Flags};
use term::alacritty_terminal::vte::ansi::CursorShape;

use crate::glyphs::{Route, route};
use crate::style::{TermStyle, dim};

#[derive(Debug, Default, PartialEq)]
pub(crate) struct RowSnapshot {
    /// Text to shape, in column order. A wide character is its own segment.
    pub segments: Vec<Segment>,
    /// Cell backgrounds that differ from the terminal background.
    pub backgrounds: Vec<Span>,
    /// Cells painted apart from the text; their text slot holds a blank.
    pub glyphs: Vec<GlyphCell>,
    /// Selected cells, as runs of columns.
    pub selection: Vec<ColumnSpan>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ColumnSpan {
    pub col: usize,
    pub cols: usize,
}

#[derive(Debug, PartialEq)]
pub(crate) struct GlyphCell {
    pub col: usize,
    pub c: char,
    pub route: Route,
    pub fg: Rgba,
}

#[derive(Debug, PartialEq)]
pub(crate) struct Segment {
    pub col: usize,
    pub text: String,
    pub runs: Vec<StyleRun>,
    /// One character that occupies two cells.
    pub wide: bool,
}

/// `len` bytes of the segment's text drawn in one style.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct StyleRun {
    pub len: usize,
    pub fg: Rgba,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikeout: bool,
}

#[derive(Debug, PartialEq)]
pub(crate) struct Span {
    pub col: usize,
    pub cols: usize,
    pub color: Rgba,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CursorKind {
    /// A filled cell with the character redrawn on top.
    SolidBlock,
    HollowBlock,
    Underline,
    Beam,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CursorSnapshot {
    pub row: usize,
    pub col: usize,
    pub cols: usize,
    pub kind: CursorKind,
    /// The character under the cursor, to redraw over a solid block.
    pub under: Option<UnderCursor>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct UnderCursor {
    pub c: char,
    pub route: Route,
    /// The character with its combining marks.
    pub text: String,
    pub bold: bool,
    pub italic: bool,
}

/// Copies the visible grid, at most `max_cols` by `max_rows` cells.
pub(crate) fn snapshot<L: EventListener>(
    term: &Term<L>,
    style: &TermStyle,
    selection: Option<SelectionRange>,
    max_cols: usize,
    max_rows: usize,
) -> Vec<RowSnapshot> {
    let grid = term.grid();
    let offset = grid.display_offset();
    let cols = term.columns().min(max_cols);
    let rows = term.screen_lines().min(max_rows);

    (0..rows)
        .map(|row| {
            let line = Line(row as i32 - offset as i32);
            let mut builder = RowBuilder::default();
            for col in 0..cols {
                let cell = &grid[Point::new(line, Column(col))];
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                builder.push(col, cell, style);
            }
            if let Some(range) = selection {
                builder.row.selection = selected_columns(range, grid, line, cols);
            }
            builder.finish()
        })
        .collect()
}

/// The runs of columns of `line` inside the selection. A wide character is
/// selected in both its cells or in neither.
fn selected_columns(
    range: SelectionRange,
    grid: &Grid<Cell>,
    line: Line,
    cols: usize,
) -> Vec<ColumnSpan> {
    let mut spans: Vec<ColumnSpan> = Vec::new();
    if line < range.start.line || line > range.end.line {
        return spans;
    }
    let inside = |col: usize| range.contains(Point::new(line, Column(col)));
    let flags = |col: usize| grid[Point::new(line, Column(col))].flags;
    for col in 0..cols {
        let selected = inside(col)
            || (flags(col).contains(Flags::WIDE_CHAR_SPACER) && col > 0 && inside(col - 1))
            || (flags(col).contains(Flags::WIDE_CHAR) && col + 1 < cols && inside(col + 1));
        if !selected {
            continue;
        }
        match spans.last_mut() {
            Some(last) if last.col + last.cols == col => last.cols += 1,
            _ => spans.push(ColumnSpan { col, cols: 1 }),
        }
    }
    spans
}

/// The cursor as it shows in the visible grid, if it does: not hidden and
/// not scrolled out of view.
pub(crate) fn cursor_snapshot<L: EventListener>(
    term: &Term<L>,
    focused: bool,
    max_cols: usize,
    max_rows: usize,
) -> Option<CursorSnapshot> {
    let grid = term.grid();
    let offset = grid.display_offset();
    let cols = term.columns().min(max_cols);
    let rows = term.screen_lines().min(max_rows);

    let cursor = term.renderable_content().cursor;
    let row = usize::try_from(cursor.point.line.0 + offset as i32)
        .ok()
        .filter(|row| *row < rows)?;
    let col = cursor.point.column.0;
    if cursor.shape == CursorShape::Hidden || col >= cols {
        return None;
    }
    let cell = &grid[cursor.point];
    let wide = cell.flags.contains(Flags::WIDE_CHAR);
    let kind = match cursor.shape {
        CursorShape::Block | CursorShape::HollowBlock if focused => CursorKind::SolidBlock,
        CursorShape::Block | CursorShape::HollowBlock => CursorKind::HollowBlock,
        CursorShape::Underline => CursorKind::Underline,
        CursorShape::Beam => CursorKind::Beam,
        CursorShape::Hidden => return None,
    };
    let under = (kind == CursorKind::SolidBlock && cell.c != ' ').then(|| {
        let mut text = String::from(cell.c);
        text.extend(cell.zerowidth().into_iter().flatten());
        UnderCursor {
            c: cell.c,
            route: if wide { Route::Text } else { route(cell.c) },
            text,
            bold: cell.flags.contains(Flags::BOLD),
            italic: cell.flags.contains(Flags::ITALIC),
        }
    });
    Some(CursorSnapshot {
        row,
        col,
        cols: if wide { 2 } else { 1 },
        kind,
        under,
    })
}

#[derive(Default)]
struct RowBuilder {
    row: RowSnapshot,
    narrow: Option<Segment>,
}

impl RowBuilder {
    fn push(
        &mut self,
        col: usize,
        cell: &term::alacritty_terminal::term::cell::Cell,
        style: &TermStyle,
    ) {
        let flags = cell.flags;
        let bold = flags.contains(Flags::BOLD);
        let mut fg = style.resolve(cell.fg, bold);
        let mut bg = style.resolve(cell.bg, false);
        if flags.contains(Flags::DIM) {
            fg = dim(fg);
        }
        if flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }
        if flags.contains(Flags::HIDDEN) {
            fg = bg;
        }

        let wide = flags.contains(Flags::WIDE_CHAR);
        let cells = if wide { 2 } else { 1 };
        self.push_background(col, cells, bg, style);

        let run = StyleRun {
            len: 0,
            fg,
            bold,
            italic: flags.contains(Flags::ITALIC),
            underline: flags.intersects(Flags::ALL_UNDERLINES),
            strikeout: flags.contains(Flags::STRIKEOUT),
        };
        let route = if wide { Route::Text } else { route(cell.c) };
        if route != Route::Text {
            self.row.glyphs.push(GlyphCell {
                col,
                c: cell.c,
                route,
                fg,
            });
        }
        let mut text = String::new();
        text.push(
            if route != Route::Text || flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) {
                ' '
            } else {
                cell.c
            },
        );
        text.extend(cell.zerowidth().into_iter().flatten());

        if wide {
            self.flush_narrow();
            self.row.segments.push(Segment {
                col,
                runs: vec![StyleRun {
                    len: text.len(),
                    ..run
                }],
                text,
                wide: true,
            });
            return;
        }
        let segment = self.narrow.get_or_insert_with(|| Segment {
            col,
            text: String::new(),
            runs: Vec::new(),
            wide: false,
        });
        match segment.runs.last_mut() {
            Some(last) if same_style(last, &run) => last.len += text.len(),
            _ => segment.runs.push(StyleRun {
                len: text.len(),
                ..run
            }),
        }
        segment.text.push_str(&text);
    }

    fn push_background(&mut self, col: usize, cols: usize, color: Rgba, style: &TermStyle) {
        if color == style.background {
            return;
        }
        match self.row.backgrounds.last_mut() {
            Some(last) if last.color == color && last.col + last.cols == col => last.cols += cols,
            _ => self.row.backgrounds.push(Span { col, cols, color }),
        }
    }

    fn flush_narrow(&mut self) {
        let Some(mut segment) = self.narrow.take() else {
            return;
        };
        trim_plain_spaces(&mut segment);
        if !segment.text.is_empty() {
            self.row.segments.push(segment);
        }
    }

    fn finish(mut self) -> RowSnapshot {
        self.flush_narrow();
        self.row
    }
}

fn same_style(a: &StyleRun, b: &StyleRun) -> bool {
    a.fg == b.fg
        && a.bold == b.bold
        && a.italic == b.italic
        && a.underline == b.underline
        && a.strikeout == b.strikeout
}

/// Drops trailing blanks that draw nothing, so empty screen area is not shaped.
fn trim_plain_spaces(segment: &mut Segment) {
    while segment.text.ends_with(' ') {
        let Some(last) = segment.runs.last_mut() else {
            break;
        };
        if last.underline || last.strikeout {
            break;
        }
        segment.text.pop();
        last.len -= 1;
        if last.len == 0 {
            segment.runs.pop();
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::style::from_bytes;
    use term::alacritty_terminal::event::VoidListener;
    use term::alacritty_terminal::grid::Scroll;
    use term::alacritty_terminal::index::Side;
    use term::alacritty_terminal::selection::{Selection, SelectionType};
    use term::alacritty_terminal::term::Config;
    use term::alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

    use super::*;

    struct Size(usize, usize);

    impl Dimensions for Size {
        fn total_lines(&self) -> usize {
            self.1
        }

        fn screen_lines(&self) -> usize {
            self.1
        }

        fn columns(&self) -> usize {
            self.0
        }
    }

    fn term(stream: &[u8]) -> Term<VoidListener> {
        let mut term = Term::new(Config::default(), &Size(20, 4), VoidListener);
        Processor::<StdSyncHandler>::new().advance(&mut term, stream);
        term
    }

    fn snap(stream: &[u8]) -> (Vec<RowSnapshot>, TermStyle) {
        let style = crate::style::test_style();
        (snapshot(&term(stream), &style, None, 20, 4), style)
    }

    #[test]
    fn one_segment_per_row_with_a_run_per_style_change() {
        let (snap, style) = snap(b"plain \x1b[31mred\x1b[0m \x1b[1mbold\x1b[0m");
        let [segment] = &snap[0].segments[..] else {
            panic!("expected one segment: {:?}", snap[0]);
        };
        assert_eq!(segment.text, "plain red bold");
        let lens: Vec<_> = segment.runs.iter().map(|r| r.len).collect();
        assert_eq!(lens, [6, 3, 1, 4]);
        assert_eq!(segment.runs[0].fg, style.foreground);
        assert_eq!(segment.runs[1].fg, style.ansi[1]);
        assert!(segment.runs[3].bold);
        assert!(snap[1].segments.is_empty());
    }

    #[test]
    fn backgrounds_merge_and_inverse_swaps() {
        let (snap, style) = snap(b"\x1b[44mab\x1b[0m \x1b[7mcd");
        let backgrounds = &snap[0].backgrounds;
        assert_eq!(
            backgrounds[0],
            Span {
                col: 0,
                cols: 2,
                color: style.ansi[4]
            }
        );
        assert_eq!(
            backgrounds[1],
            Span {
                col: 3,
                cols: 2,
                color: style.foreground
            }
        );
        let last = snap[0].segments[0].runs.last().unwrap();
        assert_eq!(last.fg, style.background);
    }

    #[test]
    fn wide_characters_are_separate_segments_at_their_column() {
        let (snap, _) = snap("ab日本c".as_bytes());
        let segments: Vec<_> = snap[0]
            .segments
            .iter()
            .map(|s| (s.col, s.text.as_str()))
            .collect();
        assert_eq!(segments, [(0, "ab"), (2, "日"), (4, "本"), (6, "c")]);
    }

    #[test]
    fn drawn_and_symbol_cells_leave_a_blank_in_the_text() {
        let (snap, style) = snap("a\u{E0B0}b\u{F115}c".as_bytes());
        let row = &snap[0];
        assert_eq!(row.segments[0].text, "a b c");
        assert_eq!(
            row.glyphs,
            [
                GlyphCell {
                    col: 1,
                    c: '\u{E0B0}',
                    route: Route::Drawn,
                    fg: style.foreground
                },
                GlyphCell {
                    col: 3,
                    c: '\u{F115}',
                    route: Route::Symbols,
                    fg: style.foreground
                },
            ]
        );
    }

    #[test]
    fn focused_block_cursor_carries_the_character_under_it() {
        let term = term(b"xy\x1b[1D");
        let cursor = cursor_snapshot(&term, true, 20, 4).expect("cursor visible");
        assert_eq!(
            (cursor.row, cursor.col, cursor.cols, cursor.kind),
            (0, 1, 1, CursorKind::SolidBlock)
        );
        assert_eq!(cursor.under.map(|u| u.c), Some('y'));
    }

    #[test]
    fn the_rows_do_not_change_with_the_cursor() {
        let (snap, _) = snap(b"x");
        assert!(snap[0].backgrounds.is_empty());
        assert_eq!(snap[0].segments[0].text, "x");
    }

    #[test]
    fn unfocused_cursor_is_an_outline() {
        let cursor = cursor_snapshot(&term(b"x"), false, 20, 4);
        assert_eq!(cursor.map(|c| c.kind), Some(CursorKind::HollowBlock));
    }

    #[test]
    fn hidden_cursor_is_not_reported() {
        assert_eq!(cursor_snapshot(&term(b"x\x1b[?25l"), true, 20, 4), None);
    }

    #[test]
    fn a_cursor_scrolled_out_of_view_is_not_reported() {
        let mut term = term(b"1\r\n2\r\n3\r\n4\r\n5\r\n6");
        term.scroll_display(Scroll::Delta(2));
        assert_eq!(cursor_snapshot(&term, true, 20, 4), None);
        term.scroll_display(Scroll::Bottom);
        assert!(cursor_snapshot(&term, true, 20, 4).is_some());
    }

    #[test]
    fn scrolled_back_rows_show_history() {
        let mut term = term(b"one\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix");
        let style = crate::style::test_style();
        term.scroll_display(Scroll::Delta(2));
        let rows = snapshot(&term, &style, None, 20, 4);
        let text: Vec<_> = rows.iter().map(|r| r.segments[0].text.as_str()).collect();
        assert_eq!(text, ["one", "two", "three", "four"]);
    }

    #[test]
    fn selected_columns_are_runs_per_row() {
        let mut term = term("ab日本cd\r\nefgh".as_bytes());
        let style = crate::style::test_style();
        let mut selection = Selection::new(
            SelectionType::Simple,
            Point::new(Line(0), Column(1)),
            Side::Left,
        );
        selection.update(Point::new(Line(1), Column(1)), Side::Right);
        term.selection = Some(selection);
        let range = term.selection.as_ref().and_then(|s| s.to_range(&term));
        let rows = snapshot(&term, &style, range, 20, 4);
        assert_eq!(rows[0].selection, [ColumnSpan { col: 1, cols: 19 }]);
        assert_eq!(rows[1].selection, [ColumnSpan { col: 0, cols: 2 }]);
        assert!(rows[2].selection.is_empty());
    }

    #[test]
    fn a_wide_character_is_selected_whole() {
        let mut term = term("ab日本cd".as_bytes());
        let style = crate::style::test_style();
        let mut selection = Selection::new(
            SelectionType::Simple,
            Point::new(Line(0), Column(2)),
            Side::Left,
        );
        selection.update(Point::new(Line(0), Column(2)), Side::Right);
        term.selection = Some(selection);
        let range = term.selection.as_ref().and_then(|s| s.to_range(&term));
        let rows = snapshot(&term, &style, range, 20, 4);
        assert_eq!(rows[0].selection, [ColumnSpan { col: 2, cols: 2 }]);
        assert_eq!(term.selection_to_string().as_deref(), Some("日"));
    }

    #[test]
    fn true_color_and_indexed_foregrounds() {
        let (snap, _) = snap(b"\x1b[38;2;1;2;3ma\x1b[38;5;208mb");
        let runs = &snap[0].segments[0].runs;
        assert_eq!(runs[0].fg, from_bytes(1, 2, 3));
        assert_eq!(runs[1].fg, from_bytes(255, 135, 0));
    }
}
