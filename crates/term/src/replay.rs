//! Recorded PTY output: a file format, a replay into an emulator with no PTY,
//! and a readable dump of the resulting grid for comparing cell by cell.

use std::fmt::Write as _;

use alacritty_terminal::Term;
use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::Config;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor, StdSyncHandler};
use thiserror::Error;

use crate::GridSize;

const MAGIC: &[u8; 4] = b"TRC1";
const HEADER_LEN: usize = 8;

/// The bytes a program printed into a PTY of a known size.
///
/// File layout: `TRC1`, columns and rows as little-endian `u16`, then the raw
/// output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recording {
    pub cols: u16,
    pub rows: u16,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RecordingError {
    #[error("not a terminal recording (bad header)")]
    BadHeader,
    #[error("the recording has a zero-sized terminal")]
    ZeroSize,
}

impl Recording {
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + self.bytes.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&self.cols.to_le_bytes());
        out.extend_from_slice(&self.rows.to_le_bytes());
        out.extend_from_slice(&self.bytes);
        out
    }

    /// # Errors
    /// When the header is missing or names an empty terminal.
    pub fn decode(data: &[u8]) -> Result<Self, RecordingError> {
        let (header, bytes) = data
            .split_at_checked(HEADER_LEN)
            .ok_or(RecordingError::BadHeader)?;
        if &header[..4] != MAGIC {
            return Err(RecordingError::BadHeader);
        }
        let cols = u16::from_le_bytes([header[4], header[5]]);
        let rows = u16::from_le_bytes([header[6], header[7]]);
        if cols == 0 || rows == 0 {
            return Err(RecordingError::ZeroSize);
        }
        Ok(Self {
            cols,
            rows,
            bytes: bytes.to_vec(),
        })
    }

    /// The emulator after reading the whole recording.
    #[must_use]
    pub fn replay(&self) -> Term<VoidListener> {
        replay(self.cols, self.rows, &self.bytes)
    }
}

/// An emulator of the given size after reading `stream`, with default
/// settings and no PTY. A synchronized update left open at the end is applied,
/// as the PTY thread does when its timeout passes.
#[must_use]
pub fn replay(cols: u16, rows: u16, stream: &[u8]) -> Term<VoidListener> {
    let size = GridSize {
        cols,
        rows,
        ..GridSize::default()
    };
    let mut term = Term::new(Config::default(), &size, VoidListener);
    let mut parser = Processor::<StdSyncHandler>::new();
    parser.advance(&mut term, stream);
    parser.stop_sync(&mut term);
    term
}

/// Every cell of the grid, history included, as text a person can review:
/// per row the characters, then one line per run of cells whose colors or
/// flags differ from a blank cell.
#[must_use]
pub fn dump_grid<T>(term: &Term<T>) -> String {
    let grid = term.grid();
    let (cols, rows, history) = (term.columns(), term.screen_lines(), grid.history_size());
    let mut out = String::new();
    let cursor = grid.cursor.point;
    let _ = writeln!(out, "size {cols}x{rows} history {history}");
    let _ = writeln!(
        out,
        "cursor line {} col {} style {:?}",
        cursor.line.0,
        cursor.column.0,
        term.cursor_style()
    );
    let _ = writeln!(out, "mode {:?}", term.mode());

    let first = -(history as i32);
    for line in first..rows as i32 {
        let cells: Vec<&Cell> = (0..cols)
            .map(|col| &grid[Point::new(Line(line), Column(col))])
            .collect();
        let _ = writeln!(out, "{line:>5} |{}", row_text(&cells));
        for (start, end, look) in runs(&cells) {
            let _ = writeln!(out, "        @{start}-{end} {look}");
        }
    }
    out
}

fn row_text(cells: &[&Cell]) -> String {
    let mut text = String::new();
    for cell in cells {
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }
        text.push(if cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) {
            ' '
        } else {
            cell.c
        });
        text.extend(cell.zerowidth().into_iter().flatten());
    }
    text.truncate(text.trim_end().len());
    text
}

/// Runs of consecutive cells with the same non-default colors, flags,
/// underline color or link, as `(first column, last column, description)`.
fn runs(cells: &[&Cell]) -> Vec<(usize, usize, String)> {
    let mut runs: Vec<(usize, usize, String)> = Vec::new();
    for (col, cell) in cells.iter().enumerate() {
        let look = look(cell);
        if look.is_empty() {
            continue;
        }
        match runs.last_mut() {
            Some((_, end, last)) if *end + 1 == col && *last == look => *end = col,
            _ => runs.push((col, col, look)),
        }
    }
    runs
}

fn look(cell: &Cell) -> String {
    let mut look = String::new();
    if cell.fg != Color::Named(NamedColor::Foreground) {
        let _ = write!(look, "fg={} ", color(cell.fg));
    }
    if cell.bg != Color::Named(NamedColor::Background) {
        let _ = write!(look, "bg={} ", color(cell.bg));
    }
    if let Some(underline) = cell.underline_color() {
        let _ = write!(look, "ul={} ", color(underline));
    }
    if let Some(link) = cell.hyperlink() {
        let _ = write!(look, "link={} ", link.uri());
    }
    if !cell.flags.is_empty() {
        let names: Vec<_> = cell.flags.iter_names().map(|(name, _)| name).collect();
        let _ = write!(look, "flags={}", names.join("|"));
    }
    look.trim_end().to_owned()
}

fn color(color: Color) -> String {
    match color {
        Color::Named(named) => format!("{named:?}"),
        Color::Indexed(index) => format!("i{index}"),
        Color::Spec(c) => format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recording_round_trips() {
        let recording = Recording {
            cols: 80,
            rows: 24,
            bytes: b"hello\x1b[31m".to_vec(),
        };
        assert_eq!(Recording::decode(&recording.encode()), Ok(recording));
        assert_eq!(Recording::decode(b"nope"), Err(RecordingError::BadHeader));
        assert_eq!(
            Recording::decode(b"TRC1\0\0\x18\0"),
            Err(RecordingError::ZeroSize)
        );
    }

    #[test]
    fn the_dump_lists_text_and_non_default_runs() {
        let term = replay(10, 2, b"a\x1b[1;31mbc\x1b[0md");
        let dump = dump_grid(&term);
        assert!(
            dump.contains("    0 |abcd\n        @1-2 fg=Red flags=BOLD\n"),
            "{dump}"
        );
    }
}
