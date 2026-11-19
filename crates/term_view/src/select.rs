//! Mouse selection on the emulator's grid: where a press starts it, how a
//! drag grows it and what a finished one leaves behind.
//!
//! The emulator keeps the selection in grid lines (history is negative), so
//! it follows its text when output scrolls the screen, and is dropped when
//! that text falls out of the scrollback.

use term::alacritty_terminal::Term;
use term::alacritty_terminal::event::EventListener;
use term::alacritty_terminal::grid::Scroll;
use term::alacritty_terminal::index::{Column, Line, Point};
use term::alacritty_terminal::selection::{Selection, SelectionType};

use crate::geometry::{Edge, Hit};

/// The grid position of a cell of the visible rows, with the view scrolled
/// back by `display_offset`.
pub(crate) fn grid_point(row: usize, col: usize, display_offset: usize) -> Point {
    Point::new(Line(row as i32 - display_offset as i32), Column(col))
}

/// Which keys were held when the button went down.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Held {
    /// Extends the selection there is instead of starting a new one.
    pub shift: bool,
    /// Selects a rectangle of cells.
    pub alt: bool,
}

/// Handles a press at `hit`: one click starts a selection by cells (a block
/// with Alt), two select a word, three or more a line.
pub(crate) fn begin<L: EventListener>(term: &mut Term<L>, hit: Hit, clicks: usize, held: Held) {
    let point = grid_point(hit.row, hit.col, term.grid().display_offset());
    if held.shift
        && clicks == 1
        && let Some(selection) = term.selection.as_mut()
    {
        selection.update(point, hit.side);
        return;
    }
    let kind = match clicks {
        1 if held.alt => SelectionType::Block,
        1 => SelectionType::Simple,
        2 => SelectionType::Semantic,
        _ => SelectionType::Lines,
    };
    term.selection = Some(Selection::new(kind, point, hit.side));
}

/// Extends the selection to `hit` while the button is down. A pointer past
/// the top or bottom of the pane also scrolls the history that way.
pub(crate) fn drag<L: EventListener>(term: &mut Term<L>, hit: Hit, edge: Edge) {
    match edge {
        Edge::Inside => {}
        Edge::Above(lines) => term.scroll_display(Scroll::Delta(lines)),
        Edge::Below(lines) => term.scroll_display(Scroll::Delta(-lines)),
    }
    let point = grid_point(hit.row, hit.col, term.grid().display_offset());
    if let Some(selection) = term.selection.as_mut() {
        selection.update(point, hit.side);
    }
}

/// Ends the gesture. A click that selected nothing leaves no selection;
/// returns whether there is one.
pub(crate) fn finish<L: EventListener>(term: &mut Term<L>) -> bool {
    if term.selection.as_ref().is_some_and(Selection::is_empty) {
        term.selection = None;
    }
    term.selection.is_some()
}

/// The selected text, if there is any.
pub(crate) fn text<L: EventListener>(term: &Term<L>) -> Option<String> {
    term.selection_to_string().filter(|text| !text.is_empty())
}

#[cfg(test)]
mod tests {
    use term::alacritty_terminal::event::VoidListener;
    use term::alacritty_terminal::grid::Dimensions;
    use term::alacritty_terminal::index::Side;
    use term::alacritty_terminal::term::Config;
    use term::alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};
    use term::{GridSize, visible_text};

    use super::*;

    /// A terminal of `rows` lines and `history` lines of scrollback that has
    /// read `stream`.
    fn term(cols: u16, rows: u16, history: usize, stream: &[u8]) -> Term<VoidListener> {
        let size = GridSize {
            cols,
            rows,
            ..GridSize::default()
        };
        let config = Config {
            scrolling_history: history,
            ..Config::default()
        };
        let mut term = Term::new(config, &size, VoidListener);
        feed(&mut term, stream);
        term
    }

    fn feed(term: &mut Term<VoidListener>, stream: &[u8]) {
        Processor::<StdSyncHandler>::new().advance(term, stream);
    }

    fn at(row: usize, col: usize, side: Side) -> Hit {
        Hit { row, col, side }
    }

    const NONE: Held = Held {
        shift: false,
        alt: false,
    };

    /// A press at the left of `from` and a drag to the right of `to`.
    fn select(term: &mut Term<VoidListener>, from: (usize, usize), to: (usize, usize)) {
        begin(term, at(from.0, from.1, Side::Left), 1, NONE);
        drag(term, at(to.0, to.1, Side::Right), Edge::Inside);
        finish(term);
    }

    #[test]
    fn dragging_selects_cells_across_rows() {
        let mut term = term(10, 3, 0, b"abcdef\r\nghijkl\r\nmnop");
        select(&mut term, (0, 3), (1, 1));
        assert_eq!(text(&term).as_deref(), Some("def\ngh"));
    }

    #[test]
    fn a_click_without_a_drag_selects_nothing() {
        let mut term = term(10, 2, 0, b"abc");
        begin(&mut term, at(0, 1, Side::Left), 1, NONE);
        assert!(!finish(&mut term));
        assert!(term.selection.is_none());
        assert_eq!(text(&term), None);
    }

    #[test]
    fn a_double_click_selects_the_word_and_a_triple_click_the_line() {
        let mut term = term(30, 2, 0, b"call foo_bar(baz) now");
        begin(&mut term, at(0, 7, Side::Left), 2, NONE);
        assert_eq!(text(&term).as_deref(), Some("foo_bar"));
        begin(&mut term, at(0, 7, Side::Left), 3, NONE);
        assert_eq!(text(&term).as_deref(), Some("call foo_bar(baz) now\n"));
    }

    #[test]
    fn a_block_selection_takes_the_same_columns_of_each_row() {
        let mut term = term(10, 3, 0, b"abcdef\r\nghijkl\r\nmnopqr");
        let alt = Held { alt: true, ..NONE };
        begin(&mut term, at(0, 1, Side::Left), 1, alt);
        drag(&mut term, at(2, 2, Side::Right), Edge::Inside);
        assert_eq!(text(&term).as_deref(), Some("bc\nhi\nno"));
    }

    #[test]
    fn shift_extends_the_selection_there_is() {
        let mut term = term(10, 2, 0, b"abcdefghij");
        select(&mut term, (0, 2), (0, 3));
        assert_eq!(text(&term).as_deref(), Some("cd"));
        let shift = Held {
            shift: true,
            ..NONE
        };
        begin(&mut term, at(0, 7, Side::Right), 1, shift);
        assert_eq!(text(&term).as_deref(), Some("cdefgh"));
    }

    #[test]
    fn wide_characters_are_copied_once_and_selected_whole() {
        let mut term = term(10, 2, 0, "ab日本cd".as_bytes());
        // From the spacer cell of 日 (cell 3) to the first cell of 本 (cell 4).
        begin(&mut term, at(0, 3, Side::Left), 1, NONE);
        drag(&mut term, at(0, 4, Side::Right), Edge::Inside);
        assert_eq!(text(&term).as_deref(), Some("日本"));
        select(&mut term, (0, 0), (0, 2));
        assert_eq!(text(&term).as_deref(), Some("ab日"));
        select(&mut term, (0, 5), (0, 7));
        assert_eq!(text(&term).as_deref(), Some("本cd"));
    }

    #[test]
    fn a_selection_follows_its_text_when_output_scrolls() {
        let mut term = term(10, 3, 100, b"one\r\ntwo\r\nthree");
        select(&mut term, (1, 0), (1, 2));
        assert_eq!(text(&term).as_deref(), Some("two"));
        feed(&mut term, b"\r\nfour\r\nfive\r\nsix");
        assert_eq!(visible_text(&term), ["four", "five", "six"]);
        assert_eq!(text(&term).as_deref(), Some("two"), "now in the history");
    }

    #[test]
    fn selecting_while_scrolled_back_picks_history_rows() {
        let mut term = term(10, 3, 100, b"one\r\ntwo\r\nthree\r\nfour\r\nfive");
        term.scroll_display(Scroll::Delta(2));
        // The top visible row is "one".
        select(&mut term, (0, 0), (1, 2));
        assert_eq!(text(&term).as_deref(), Some("one\ntwo"));
    }

    #[test]
    fn a_selection_is_dropped_when_its_text_leaves_the_scrollback() {
        let mut term = term(10, 2, 2, b"one\r\ntwo");
        select(&mut term, (0, 0), (0, 2));
        assert_eq!(text(&term).as_deref(), Some("one"));
        feed(&mut term, b"\r\nthree\r\nfour\r\nfive");
        assert_eq!(term.grid().history_size(), 2);
        assert_eq!(text(&term), None);
    }

    #[test]
    fn dragging_past_the_top_scrolls_into_the_history_and_keeps_selecting() {
        let mut term = term(10, 3, 100, b"1\r\n2\r\n3\r\n4\r\n5\r\n6");
        begin(&mut term, at(2, 0, Side::Left), 1, NONE);
        drag(&mut term, at(0, 0, Side::Left), Edge::Above(2));
        assert_eq!(term.grid().display_offset(), 2);
        assert_eq!(text(&term).as_deref(), Some("2\n3\n4\n5"));
        drag(&mut term, at(2, 0, Side::Left), Edge::Below(2));
        assert_eq!(term.grid().display_offset(), 0);
    }
}
