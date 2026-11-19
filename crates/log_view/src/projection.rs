//! Which lines are shown: the log less the bodies of collapsed groups.

use std::ops::Range;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Hidden {
    start: u64,
    len: u64,
    /// Lines hidden by the ranges before this one.
    before: u64,
}

/// Maps between rows (what is shown, counting from 0) and line ids.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Projection {
    first: u64,
    total: u64,
    hidden: Vec<Hidden>,
    hidden_total: u64,
}

impl Projection {
    /// Lines `first..total` less `hidden`, which must be sorted and disjoint;
    /// parts outside the lines held are ignored.
    pub fn new(first: u64, total: u64, hidden: impl IntoIterator<Item = Range<u64>>) -> Self {
        let mut ranges = Vec::new();
        let mut hidden_total = 0;
        for range in hidden {
            let (start, end) = (range.start.max(first), range.end.min(total));
            if start >= end {
                continue;
            }
            ranges.push(Hidden {
                start,
                len: end - start,
                before: hidden_total,
            });
            hidden_total += end - start;
        }
        Self {
            first,
            total,
            hidden: ranges,
            hidden_total,
        }
    }

    /// Rows shown.
    pub fn rows(&self) -> u64 {
        self.total.saturating_sub(self.first) - self.hidden_total
    }

    /// Lines hidden before the `k`-th range.
    fn hidden_through(&self, k: usize) -> u64 {
        k.checked_sub(1)
            .map_or(0, |i| self.hidden[i].before + self.hidden[i].len)
    }

    /// The row of `line`, or for a hidden line the row of the next shown one.
    pub fn row_of(&self, line: u64) -> u64 {
        let line = line.clamp(self.first, self.total);
        let k = self.hidden.partition_point(|h| h.start + h.len <= line);
        match self.hidden.get(k) {
            Some(h) if h.start <= line => h.start - self.first - h.before,
            _ => line - self.first - self.hidden_through(k),
        }
    }

    /// The line shown on `row`.
    pub fn line_at(&self, row: u64) -> u64 {
        let k = self
            .hidden
            .partition_point(|h| h.start - self.first - h.before <= row);
        self.first + row + self.hidden_through(k)
    }

    /// Whether a collapsed group hides `line`.
    pub fn is_hidden(&self, line: u64) -> bool {
        let k = self.hidden.partition_point(|h| h.start + h.len <= line);
        self.hidden.get(k).is_some_and(|h| h.start <= line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lines 0..20 with 5..8 and 12..14 hidden: rows show 0-4, 8-11, 14-19.
    fn sample() -> Projection {
        Projection::new(0, 20, [5..8, 12..14])
    }

    #[test]
    fn rows_skip_hidden_lines() {
        let p = sample();
        assert_eq!(p.rows(), 15);
        let shown: Vec<u64> = (0..p.rows()).map(|row| p.line_at(row)).collect();
        assert_eq!(shown, [0, 1, 2, 3, 4, 8, 9, 10, 11, 14, 15, 16, 17, 18, 19]);
    }

    #[test]
    fn row_of_inverts_line_at_for_shown_lines() {
        let p = sample();
        for row in 0..p.rows() {
            assert_eq!(p.row_of(p.line_at(row)), row);
        }
    }

    #[test]
    fn a_hidden_line_maps_to_the_next_shown_row() {
        let p = sample();
        assert!(p.is_hidden(6) && !p.is_hidden(8) && !p.is_hidden(4));
        assert_eq!(p.row_of(6), p.row_of(8));
        assert_eq!(p.row_of(12), p.row_of(14));
    }

    #[test]
    fn ranges_outside_the_lines_held_are_clipped() {
        let p = Projection::new(10, 20, [0..12, 18..40]);
        assert_eq!(p.rows(), 6);
        assert_eq!(p.line_at(0), 12);
        assert_eq!(p.line_at(5), 17);
        assert_eq!(p.row_of(0), 0);
    }

    #[test]
    fn nothing_hidden_is_the_identity() {
        let p = Projection::new(5, 9, []);
        assert_eq!(p.rows(), 4);
        assert_eq!((p.line_at(0), p.line_at(3), p.row_of(7)), (5, 8, 2));
    }
}
