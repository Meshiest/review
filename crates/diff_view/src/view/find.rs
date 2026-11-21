//! The matches of a find, painted over their lines and paths, and the jump
//! to one of them.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use diff::{FileIx, Row, Side, SideLine};
use gpui::Context;

use super::DiffView;
use crate::find::{FindAt, FindMatch};

/// The matches shown, and the current one.
#[derive(Default)]
pub(super) struct Marks {
    matches: Arc<[FindMatch]>,
    /// The matches at each place, as a range of `matches`: a line's are
    /// next to each other.
    at: HashMap<(FileIx, FindAt), Range<usize>>,
    current: Option<usize>,
}

impl Marks {
    fn new(matches: Arc<[FindMatch]>, current: Option<usize>) -> Self {
        let mut at: HashMap<(FileIx, FindAt), Range<usize>> = HashMap::new();
        for (ix, m) in matches.iter().enumerate() {
            at.entry((m.file, m.at))
                .and_modify(|range| range.end = ix + 1)
                .or_insert(ix..ix + 1);
        }
        Self {
            matches,
            at,
            current,
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.matches.is_empty()
    }

    /// The matches at `place`, each with whether it is the current one.
    pub(super) fn at(
        &self,
        place: (FileIx, FindAt),
    ) -> impl Iterator<Item = (&Range<u32>, bool)> + '_ {
        let range = self.at.get(&place).cloned().unwrap_or_default();
        range.map(|ix| (&self.matches[ix].chars, self.current == Some(ix)))
    }
}

impl DiffView {
    /// Paints `matches` (in the order the diff shows them) as found, the
    /// `current` one more strongly, in place of any shown before.
    pub fn set_find_matches(
        &mut self,
        matches: Arc<[FindMatch]>,
        current: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        self.marks = Marks::new(matches, current);
        cx.notify();
    }

    /// Paints no matches.
    pub fn clear_find(&mut self, cx: &mut Context<Self>) {
        if !self.marks.is_empty() {
            self.marks = Marks::default();
            cx.notify();
        }
    }

    pub fn find_matches(&self) -> &[FindMatch] {
        &self.marks.matches
    }

    pub fn current_find(&self) -> Option<usize> {
        self.marks.current
    }

    /// Makes match `ix` the current one and brings it into view: its file
    /// is opened if it is collapsed, and the view scrolls when its row is
    /// not wholly visible below the pinned header, to put it a third of
    /// the way down. Returns its row; `None` when the rows do not show it.
    pub fn reveal_find(&mut self, ix: usize, cx: &mut Context<Self>) -> Option<usize> {
        let found = self.marks.matches.get(ix)?.clone();
        self.marks.current = Some(ix);
        cx.notify();
        if self.is_file_collapsed(found.file) {
            self.set_file_collapsed(found.file, false, cx);
        }
        let row = self.find_row(&found)?;
        let top = self.list.top_of(row);
        let bottom = top + self.list.row_height(row);
        let viewport = self.scroll.state.y.viewport;
        let pinned = match found.at {
            FindAt::Path => 0.0,
            FindAt::Line(..) => f64::from(self.style.file_header_height),
        };
        let here = self.scroll_y();
        if top < here + pinned || bottom > here + viewport {
            let margin = match found.at {
                FindAt::Path => 0.0,
                FindAt::Line(..) => (viewport / 3.0).max(pinned),
            };
            self.set_scroll_y(top - margin);
        }
        Some(row)
    }

    /// The row that shows `found` now.
    #[must_use]
    pub fn find_row(&self, found: &FindMatch) -> Option<usize> {
        let header = self.list.file_row(found.file)?;
        let FindAt::Line(side, line) = found.at else {
            return Some(header);
        };
        let rows = self.list.rows();
        let mut body = rows[header + 1..]
            .iter()
            .take_while(|row| !matches!(row, Row::FileHeader { .. }));
        let shows = |row: &Row| match (*row, side) {
            (Row::Line { new, .. }, Side::New) => new == Some(line),
            (Row::Line { old, new, .. }, Side::Old) => new.is_none() && old == Some(line),
            (Row::Pair { left, .. }, Side::Old) => left.map(|l: SideLine| l.line) == Some(line),
            (Row::Pair { right, .. }, Side::New) => right.map(|r: SideLine| r.line) == Some(line),
            _ => false,
        };
        body.position(shows).map(|ix| header + 1 + ix)
    }
}
