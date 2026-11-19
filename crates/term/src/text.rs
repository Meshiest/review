use alacritty_terminal::Term;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::Flags;

/// The visible screen as one string per row, trailing blanks trimmed.
///
/// A wide character appears once (its spacer cell is skipped).
pub fn visible_text<T>(term: &Term<T>) -> Vec<String> {
    let grid = term.grid();
    (0..term.screen_lines())
        .map(|row| {
            let mut text = String::new();
            for col in 0..term.columns() {
                let cell = &grid[Point::new(Line(row as i32), Column(col))];
                if cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    continue;
                }
                text.push(cell.c);
            }
            text.truncate(text.trim_end().len());
            text
        })
        .collect()
}
