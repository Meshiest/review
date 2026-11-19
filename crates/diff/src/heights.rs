//! Row heights on a `sum_tree`: scroll offset to row and row to y in O(log n).

use std::cmp::Ordering;
use std::ops::Range;

use sum_tree::{Bias, Dimension, Dimensions, Item, SeekTarget, SumTree, Summary};

/// The height class of one row.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RowHeight {
    /// A row of the shared code height.
    Code,
    /// A row with its own height (headers, blocks).
    Sized(f64),
}

/// Consecutive code rows collapse into one span, so the tree holds about one
/// item per header or block rather than one per row.
#[derive(Clone, Copy, Debug)]
enum Span {
    Code { rows: u32 },
    Sized { height: f64 },
}

/// Summaries are in rows and heights; the code height is the tree context,
/// so changing it needs no rebuild.
#[derive(Clone, Copy, Debug, Default)]
struct SpanSummary {
    rows: usize,
    code_rows: usize,
    sized_height: f64,
}

impl Summary for SpanSummary {
    type Context<'a> = f64;

    fn zero(_: f64) -> Self {
        Self::default()
    }

    fn add_summary(&mut self, other: &Self, _: f64) {
        self.rows += other.rows;
        self.code_rows += other.code_rows;
        self.sized_height += other.sized_height;
    }
}

impl Item for Span {
    type Summary = SpanSummary;

    fn summary(&self, _: f64) -> SpanSummary {
        match *self {
            Self::Code { rows } => SpanSummary {
                rows: rows as usize,
                code_rows: rows as usize,
                sized_height: 0.0,
            },
            Self::Sized { height } => SpanSummary {
                rows: 1,
                code_rows: 0,
                sized_height: height,
            },
        }
    }
}

/// Adds `n` code rows after `spans`, into the last span when it is code.
fn push_code(spans: &mut Vec<Span>, n: u32) {
    if n == 0 {
        return;
    }
    match spans.last_mut() {
        Some(Span::Code { rows }) => *rows += n,
        _ => spans.push(Span::Code { rows: n }),
    }
}

fn push_row(spans: &mut Vec<Span>, row: RowHeight) {
    match row {
        RowHeight::Code => push_code(spans, 1),
        RowHeight::Sized(height) => spans.push(Span::Sized { height }),
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
struct Rows(usize);

impl Dimension<'_, SpanSummary> for Rows {
    fn zero(_: f64) -> Self {
        Self(0)
    }

    fn add_summary(&mut self, summary: &SpanSummary, _: f64) {
        self.0 += summary.rows;
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Y(f64);

impl Dimension<'_, SpanSummary> for Y {
    fn zero(_: f64) -> Self {
        Self(0.0)
    }

    fn add_summary(&mut self, summary: &SpanSummary, code_height: f64) {
        self.0 += summary.code_rows as f64 * code_height + summary.sized_height;
    }
}

impl SeekTarget<'_, SpanSummary, Y> for Y {
    fn cmp(&self, cursor_location: &Y, _: f64) -> Ordering {
        self.0.total_cmp(&cursor_location.0)
    }
}

/// A position inside the list: a row and an offset from that row's top.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RowOffset {
    pub row: usize,
    pub offset: f64,
}

#[derive(Clone)]
pub struct HeightIndex {
    tree: SumTree<Span>,
    code_height: f64,
}

impl HeightIndex {
    #[must_use]
    pub fn new(code_height: f64, rows: impl IntoIterator<Item = RowHeight>) -> Self {
        let mut spans = Vec::new();
        for row in rows {
            push_row(&mut spans, row);
        }
        Self {
            tree: SumTree::from_iter(spans, code_height),
            code_height,
        }
    }

    /// Replaces the rows in `range` with `rows`, in O(log n) plus the rows
    /// put in.
    ///
    /// # Panics
    /// If `range` reaches past the end.
    pub fn splice(&mut self, range: Range<usize>, rows: impl IntoIterator<Item = RowHeight>) {
        assert!(
            range.start <= range.end && range.end <= self.len(),
            "splice {range:?} out of 0..{}",
            self.len()
        );
        let code_height = self.code_height;
        let mut cursor = self.tree.cursor::<Rows>(code_height);
        let mut tree = cursor.slice(&Rows(range.start), Bias::Right);
        let mut spans = Vec::new();
        // The code rows of the span the range starts inside, before it.
        if let Some(Span::Code { .. }) = cursor.item() {
            push_code(&mut spans, (range.start - cursor.start().0) as u32);
        }
        for row in rows {
            push_row(&mut spans, row);
        }
        cursor.seek_forward(&Rows(range.end), Bias::Right);
        // The code rows of the span the range ends inside, after it.
        if let Some(&Span::Code { rows }) = cursor.item() {
            push_code(
                &mut spans,
                (cursor.start().0 + rows as usize - range.end) as u32,
            );
            cursor.next();
        }
        for span in spans {
            tree.push(span, code_height);
        }
        tree.append(cursor.suffix(), code_height);
        drop(cursor);
        self.tree = tree;
    }

    pub fn len(&self) -> usize {
        self.tree.summary().rows
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn code_height(&self) -> f64 {
        self.code_height
    }

    pub fn total_height(&self) -> f64 {
        Y::from_summary(self.tree.summary(), self.code_height).0
    }

    /// The row at scroll offset `y` and how far into it `y` is. Offsets past
    /// either end clamp to the first or last row.
    ///
    /// # Panics
    /// If the index is empty.
    pub fn row_at(&self, y: f64) -> RowOffset {
        assert!(!self.is_empty(), "row_at on an empty index");
        let y = y.max(0.0);
        let (start, _, item) =
            self.tree
                .find::<Dimensions<Y, Rows>, _>(self.code_height, &Y(y), Bias::Right);
        let Some(span) = item else {
            // Past the end: the last row, offset from its top.
            let row = self.len() - 1;
            return RowOffset {
                row,
                offset: y - self.top_of(row),
            };
        };
        let Dimensions(Y(span_y), Rows(span_row), ()) = start;
        match *span {
            Span::Sized { .. } => RowOffset {
                row: span_row,
                offset: y - span_y,
            },
            Span::Code { rows } => {
                let k = (((y - span_y) / self.code_height) as usize).min(rows as usize - 1);
                RowOffset {
                    row: span_row + k,
                    offset: y - span_y - k as f64 * self.code_height,
                }
            }
        }
    }

    /// The y of a row's top. `row == len()` gives the total height.
    ///
    /// # Panics
    /// If `row > len()`.
    pub fn top_of(&self, row: usize) -> f64 {
        assert!(row <= self.len(), "row {row} out of range");
        let (start, _, item) =
            self.tree
                .find::<Dimensions<Rows, Y>, _>(self.code_height, &Rows(row), Bias::Right);
        let Dimensions(Rows(span_row), Y(span_y), ()) = start;
        match item {
            Some(Span::Code { .. }) => span_y + (row - span_row) as f64 * self.code_height,
            Some(Span::Sized { .. }) | None => span_y,
        }
    }

    /// Changes one sized row's height in O(log n).
    ///
    /// # Panics
    /// If `row` is not a sized row.
    pub fn set_height(&mut self, row: usize, height: f64) {
        let mut cursor = self.tree.cursor::<Rows>(self.code_height);
        let mut tree = cursor.slice(&Rows(row), Bias::Right);
        assert!(
            matches!(cursor.item(), Some(Span::Sized { .. })) && cursor.start().0 == row,
            "row {row} is not a sized row"
        );
        tree.push(Span::Sized { height }, self.code_height);
        cursor.next();
        tree.append(cursor.suffix(), self.code_height);
        drop(cursor);
        self.tree = tree;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CODE: f64 = 20.0;

    /// 3 code, header 30, 2 code, block 100, 4 code.
    fn sample() -> HeightIndex {
        use RowHeight::{Code, Sized};
        HeightIndex::new(
            CODE,
            [
                Code,
                Code,
                Code,
                Sized(30.0),
                Code,
                Code,
                Sized(100.0),
                Code,
                Code,
                Code,
                Code,
            ],
        )
    }

    fn naive_tops(index: &HeightIndex, heights: &[f64]) -> Vec<f64> {
        let mut y = 0.0;
        let mut tops = Vec::with_capacity(heights.len());
        for h in heights {
            tops.push(y);
            y += h;
        }
        assert_eq!(index.len(), heights.len());
        tops
    }

    #[test]
    fn tops_match_a_running_sum() {
        let index = sample();
        let heights = [20., 20., 20., 30., 20., 20., 100., 20., 20., 20., 20.];
        let tops = naive_tops(&index, &heights);
        for (row, top) in tops.iter().enumerate() {
            assert_eq!(index.top_of(row), *top, "row {row}");
        }
        assert_eq!(index.top_of(index.len()), 310.0);
        assert_eq!(index.total_height(), 310.0);
    }

    #[test]
    fn offset_row_offset_round_trip() {
        let index = sample();
        let mut y = 0.0;
        while y < index.total_height() {
            let at = index.row_at(y);
            assert!(at.offset >= 0.0, "y {y}: {at:?}");
            assert!(
                (index.top_of(at.row) + at.offset - y).abs() < 1e-9,
                "y {y}: {at:?}"
            );
            y += 0.5;
        }
    }

    #[test]
    fn row_offset_row_round_trip() {
        let index = sample();
        for row in 0..index.len() {
            assert_eq!(
                index.row_at(index.top_of(row)),
                RowOffset { row, offset: 0.0 }
            );
            assert_eq!(index.row_at(index.top_of(row) + 0.25).row, row);
        }
    }

    #[test]
    fn offsets_clamp_at_both_ends() {
        let index = sample();
        assert_eq!(
            index.row_at(-5.0),
            RowOffset {
                row: 0,
                offset: 0.0
            }
        );
        assert_eq!(index.row_at(1e9).row, index.len() - 1);
        assert_eq!(index.row_at(index.total_height()).row, index.len() - 1);
    }

    #[test]
    fn resizing_a_block_moves_only_later_rows() {
        let mut index = sample();
        let before: Vec<f64> = (0..=index.len()).map(|r| index.top_of(r)).collect();
        index.set_height(6, 250.0);
        for (row, &top) in before.iter().enumerate() {
            let moved = if row > 6 { 150.0 } else { 0.0 };
            assert_eq!(index.top_of(row), top + moved, "row {row}");
        }
        assert_eq!(
            index.row_at(before[6] + 200.0),
            RowOffset {
                row: 6,
                offset: 200.0
            }
        );
        assert_eq!(index.len(), 11);
    }

    #[test]
    fn a_splice_matches_an_index_built_whole() {
        use RowHeight::{Code, Sized};
        let base = [
            Code,
            Code,
            Code,
            Sized(30.0),
            Code,
            Code,
            Sized(100.0),
            Code,
            Code,
            Code,
            Code,
        ];
        let inserts: [&[RowHeight]; 4] = [&[], &[Code], &[Code, Sized(7.0), Code], &[Sized(9.0)]];
        for start in 0..=base.len() {
            for end in start..=base.len() {
                for insert in inserts {
                    let mut index = HeightIndex::new(CODE, base);
                    index.splice(start..end, insert.iter().copied());
                    let mut whole = base[..start].to_vec();
                    whole.extend_from_slice(insert);
                    whole.extend_from_slice(&base[end..]);
                    let expected = HeightIndex::new(CODE, whole.iter().copied());
                    assert_eq!(index.len(), expected.len(), "{start}..{end} {insert:?}");
                    for row in 0..=expected.len() {
                        assert_eq!(
                            index.top_of(row),
                            expected.top_of(row),
                            "{start}..{end} {insert:?} row {row}"
                        );
                    }
                    for (row, height) in whole.iter().enumerate() {
                        if let Sized(_) = height {
                            index.set_height(row, 1.0);
                        }
                    }
                }
            }
        }
    }

    #[test]
    #[should_panic(expected = "not a sized row")]
    fn resizing_a_code_row_panics() {
        sample().set_height(1, 50.0);
    }

    #[test]
    fn large_index_round_trips() {
        let rows: Vec<RowHeight> = (0..100_000)
            .map(|r| {
                if r % 97 == 0 {
                    RowHeight::Sized(13.0 + (r % 7) as f64)
                } else {
                    RowHeight::Code
                }
            })
            .collect();
        let index = HeightIndex::new(CODE, rows);
        assert_eq!(index.len(), 100_000);
        for row in (0..100_000).step_by(311) {
            let top = index.top_of(row);
            assert_eq!(index.row_at(top + 1.0), RowOffset { row, offset: 1.0 });
        }
    }
}
