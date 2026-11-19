//! The row projection: the model laid out as one continuous list.

use std::ops::Range;

use crate::heights::{HeightIndex, RowHeight, RowOffset};
use crate::model::{Anchor, Block, BlockId, Diff, FileIx, Hunk, Segment, Side};
use crate::reveal::{Expand, Reveals, arrows};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Layout {
    #[default]
    Unified,
    Split,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LineKind {
    Context,
    Removed,
    Added,
}

/// One side's line in a code row: a 0-based line index into that side's blob.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SideLine {
    pub line: u32,
    pub kind: LineKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    FileHeader {
        file: FileIx,
    },
    /// The header of hunk `hunk`, which also stands for the hidden lines
    /// before it (its gap, see [`crate::FileDiff::gap`]). Once every line
    /// of a gap is revealed, its hunk has no header.
    HunkHeader {
        file: FileIx,
        hunk: u32,
    },
    /// After a file's last hunk while lines after it are hidden: the row of
    /// the arrow that reveals them.
    Expander {
        file: FileIx,
    },
    /// Unified: a context line has both sides, a removed line only `old`, an
    /// added line only `new`.
    Line {
        file: FileIx,
        old: Option<u32>,
        new: Option<u32>,
    },
    /// Split: the old line on the left, the new line on the right.
    Pair {
        file: FileIx,
        left: Option<SideLine>,
        right: Option<SideLine>,
    },
    Block {
        id: BlockId,
    },
}

impl Row {
    /// The file and gap a hunk header or expander row stands for.
    #[must_use]
    pub fn gap(self, diff: &Diff) -> Option<(FileIx, u32)> {
        match self {
            Self::HunkHeader { file, hunk } => Some((file, hunk)),
            Self::Expander { file } => Some((file, diff.file(file).hunks.len() as u32)),
            _ => None,
        }
    }

    /// The kind of a unified [`Row::Line`].
    pub fn line_kind(old: Option<u32>, new: Option<u32>) -> LineKind {
        match (old, new) {
            (Some(_), Some(_)) => LineKind::Context,
            (Some(_), None) => LineKind::Removed,
            (None, _) => LineKind::Added,
        }
    }
}

/// How much of a file its rows show.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ContextMode {
    /// The hunks the diff came with, each under a hunk header, with the
    /// context revealed around them.
    #[default]
    Hunks,
    /// Every line of the file, changes inline where they occur, without hunk
    /// headers. Built from the diff's own hunks and the blobs' line counts;
    /// no line is copied.
    Full,
}

/// Heights of the rows that are not blocks.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RowHeights {
    /// Code lines and hunk headers.
    pub code: f32,
    pub file_header: f32,
}

/// A diff and its rows in one layout, with a height index over them.
pub struct DiffList {
    diff: Diff,
    layout: Layout,
    heights: RowHeights,
    rows: Vec<Row>,
    index: HeightIndex,
    /// The row of each block, by block id.
    block_rows: Vec<u32>,
    /// The first row showing each block's lines, by block id; equal to the
    /// block's own row when it covers no shown line.
    span_starts: Vec<u32>,
    /// Rows of the file headers (of the files shown) and the rows to step to
    /// between hunks, in order.
    file_rows: Vec<u32>,
    hunk_rows: Vec<u32>,
    /// By file.
    contexts: Vec<ContextMode>,
    /// By file: only the header row is shown, and no blocks.
    collapsed: Vec<bool>,
    /// By file: the context revealed around the hunks.
    reveals: Vec<Reveals>,
    /// The one file shown, if the list is restricted to one.
    only: Option<FileIx>,
}

impl DiffList {
    #[must_use]
    pub fn new(diff: Diff, layout: Layout, heights: RowHeights) -> Self {
        let files = diff.files.len();
        let mut list = Self {
            diff,
            layout,
            heights,
            rows: Vec::new(),
            index: HeightIndex::new(heights.code.into(), []),
            block_rows: Vec::new(),
            span_starts: Vec::new(),
            file_rows: Vec::new(),
            hunk_rows: Vec::new(),
            contexts: vec![ContextMode::Hunks; files],
            collapsed: vec![false; files],
            reveals: vec![Reveals::default(); files],
            only: None,
        };
        list.project();
        list
    }

    pub fn diff(&self) -> &Diff {
        &self.diff
    }

    pub fn layout(&self) -> Layout {
        self.layout
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn total_height(&self) -> f64 {
        self.index.total_height()
    }

    /// See [`HeightIndex::row_at`].
    pub fn row_at(&self, y: f64) -> RowOffset {
        self.index.row_at(y)
    }

    pub fn top_of(&self, row: usize) -> f64 {
        self.index.top_of(row)
    }

    pub fn row_height(&self, row: usize) -> f64 {
        match self.rows[row] {
            Row::FileHeader { .. } => self.heights.file_header.into(),
            Row::Block { id } => self.diff.block(id).height.into(),
            Row::HunkHeader { .. } | Row::Expander { .. } | Row::Line { .. } | Row::Pair { .. } => {
                self.heights.code.into()
            }
        }
    }

    pub fn block_row(&self, id: BlockId) -> usize {
        self.block_rows[id.0 as usize] as usize
    }

    /// The rows showing a block's lines: from its first line's row up to the
    /// block's own row, exclusive. Empty for a file-top block or one whose
    /// lines no hunk shows.
    pub fn block_span(&self, id: BlockId) -> Range<usize> {
        self.span_starts[id.0 as usize] as usize..self.block_row(id)
    }

    /// The rows of the headers of the files shown, in file order. Index it by
    /// file only through [`DiffList::file_row`]: a list restricted to one
    /// file has just that one.
    pub fn file_rows(&self) -> &[u32] {
        &self.file_rows
    }

    /// The header row of `file`; `None` when the list does not show it.
    pub fn file_row(&self, file: FileIx) -> Option<usize> {
        let ix = match self.only {
            Some(only) if only != file => return None,
            Some(_) => 0,
            None => file.0 as usize,
        };
        self.file_rows.get(ix).map(|&row| row as usize)
    }

    /// The file whose rows `row` is among.
    ///
    /// # Panics
    /// If the list is empty or `row` is out of range.
    pub fn file_of_row(&self, row: usize) -> FileIx {
        let header = self.file_rows.partition_point(|&r| r as usize <= row) - 1;
        match self.rows[self.file_rows[header] as usize] {
            Row::FileHeader { file } => file,
            other => unreachable!("file row holds {other:?}"),
        }
    }

    /// The rows to step to between hunks, in order: the hunk headers, or the
    /// first changed row of a hunk without one (in [`ContextMode::Full`], or
    /// once the lines before it are all revealed).
    pub fn hunk_rows(&self) -> &[u32] {
        &self.hunk_rows
    }

    /// Replaces the blocks and re-projects the rows.
    pub fn set_blocks(&mut self, blocks: Vec<Block>) {
        self.diff.blocks = blocks;
        self.project();
    }

    /// How `file` is shown.
    pub fn context(&self, file: FileIx) -> ContextMode {
        self.contexts[file.0 as usize]
    }

    /// Shows `file`, or every file for `None`, with `mode`.
    pub fn set_context(&mut self, file: Option<FileIx>, mode: ContextMode) {
        let before = self.contexts.clone();
        match file {
            Some(file) => self.contexts[file.0 as usize] = mode,
            None => self.contexts.fill(mode),
        }
        if self.contexts != before {
            self.project();
        }
    }

    pub fn is_collapsed(&self, file: FileIx) -> bool {
        self.collapsed[file.0 as usize]
    }

    /// Collapses the files marked in `collapsed` (by file; missing ones are
    /// open) to their header row. Their blocks are not shown.
    pub fn set_collapsed(&mut self, mut collapsed: Vec<bool>) {
        collapsed.resize(self.diff.files.len(), false);
        if collapsed != self.collapsed {
            self.collapsed = collapsed;
            self.project();
        }
    }

    /// The one file shown, if the list is restricted to one.
    pub fn only(&self) -> Option<FileIx> {
        self.only
    }

    /// Restricts the rows to `file`, or shows every file for `None`. The
    /// blocks of the files left out are not shown.
    pub fn set_only(&mut self, file: Option<FileIx>) {
        if file != self.only {
            self.only = file;
            self.project();
        }
    }

    /// Whether `id`'s row is in the list: not collapsed away or filtered out.
    /// [`DiffList::block_row`] of a block that is not shown is its file's
    /// header row, or 0 when the file is filtered out.
    pub fn shows_block(&self, id: BlockId) -> bool {
        self.rows.get(self.block_row(id)) == Some(&Row::Block { id })
    }

    /// Whether a row shows `line` (0-based) of `side` of `file`.
    pub fn shows(&self, file: FileIx, side: Side, line: u32) -> bool {
        let f = self.diff.file(file);
        match self.context(file) {
            ContextMode::Hunks => {
                f.shows(side, line) || self.reveals[file.0 as usize].shows(f, side, line)
            }
            ContextMode::Full => {
                line < match side {
                    Side::Old => f.old.line_count(),
                    Side::New => f.new.line_count(),
                }
            }
        }
    }

    /// Re-projects the rows from the model; the diff itself is not touched.
    pub fn set_layout(&mut self, layout: Layout) {
        if layout != self.layout {
            self.layout = layout;
            self.project();
        }
    }

    pub fn set_block_height(&mut self, id: BlockId, height: f32) {
        self.diff.blocks[id.0 as usize].height = height;
        if self.shows_block(id) {
            self.index.set_height(self.block_row(id), height.into());
        }
    }

    pub fn heights(&self) -> RowHeights {
        self.heights
    }

    /// Changes the code and file header heights. The rows stay as they are;
    /// only the height index is rebuilt.
    pub fn set_heights(&mut self, heights: RowHeights) {
        if heights != self.heights {
            self.heights = heights;
            self.rebuild_index();
        }
    }

    /// The expand arrows of gap `gap` of `file` (see
    /// [`crate::FileDiff::gap`]), left to right; none outside
    /// [`ContextMode::Hunks`], where nothing is hidden and for a file whose
    /// rows are not shown.
    pub fn arrows(&self, file: FileIx, gap: u32) -> &'static [Expand] {
        if self.context(file) != ContextMode::Hunks
            || self.is_collapsed(file)
            || self.only.is_some_and(|only| only != file)
        {
            return &[];
        }
        let f = self.diff.file(file);
        let Some(at) = f.gap(gap) else {
            return &[];
        };
        let hidden = self.reveals[file.0 as usize]
            .gap(gap, at.len)
            .hidden(at.len);
        arrows(gap, f.gap_count(), hidden)
    }

    /// Reveals more of gap `gap` of `file`, as its `expand` arrow does, and
    /// re-projects that file's rows; whether anything changed. Only the
    /// file's rows are rebuilt, so the cost does not grow with the diff.
    pub fn expand(&mut self, file: FileIx, gap: u32, expand: Expand) -> bool {
        let ix = file.0 as usize;
        if self.context(file) != ContextMode::Hunks
            || !self.reveals[ix].expand(&self.diff.files[ix], gap, expand)
        {
            return false;
        }
        self.reproject_file(file);
        true
    }

    /// The context revealed in `file`.
    pub fn reveals(&self, file: FileIx) -> &Reveals {
        &self.reveals[file.0 as usize]
    }

    /// Replaces the revealed context of every file, by file (missing ones
    /// reveal nothing); each is held to its file's gaps.
    pub fn set_reveals(&mut self, mut reveals: Vec<Reveals>) {
        reveals.resize(self.diff.files.len(), Reveals::default());
        let reveals: Vec<Reveals> = reveals
            .into_iter()
            .zip(&self.diff.files)
            .map(|(reveal, file)| reveal.fitted(file))
            .collect();
        if reveals != self.reveals {
            self.reveals = reveals;
            self.project();
        }
    }

    /// Where the lines under hunk `hunk`'s header start (0-based) and how
    /// many there are on each side: from the header to the next header or
    /// expander, or the end of the file. They take in the context revealed
    /// next to the hunk and the hunks joined to it.
    ///
    /// # Panics
    /// If the file has no such hunk.
    pub fn header_span(&self, file: FileIx, hunk: u32) -> HunkSpan {
        let f = self.diff.file(file);
        let reveals = &self.reveals[file.0 as usize];
        let gap = f.gap(hunk).expect("a gap before every hunk");
        let below = gap.len - reveals.gap(hunk, gap.len).bottom;
        let (old_start, new_start) = (gap.old + below, gap.new + below);
        let mut next = hunk + 1;
        let (old_end, new_end) = loop {
            let h = &f.hunks[next as usize - 1];
            let end = (h.old_start + h.old_len(), h.new_start + h.new_len());
            let Some(after) = f.gap(next) else {
                break end;
            };
            let shown = reveals.gap(next, after.len);
            let joined =
                (next as usize) < f.hunks.len() && after.len > 0 && shown.hidden(after.len) == 0;
            if !joined {
                break (after.old + shown.top, after.new + shown.top);
            }
            next += 1;
        };
        HunkSpan {
            old_start,
            old_len: old_end.saturating_sub(old_start),
            new_start,
            new_len: new_end.saturating_sub(new_start),
        }
    }

    fn project(&mut self) {
        let mut rows = Vec::with_capacity(self.rows.len().max(1024));
        let mut block_rows = vec![0; self.diff.blocks.len()];
        let mut file_rows = Vec::with_capacity(self.diff.files.len());
        let mut hunk_rows = Vec::new();
        let mut blocks = BlocksByFile::new(&self.diff);
        for file_ix in 0..self.diff.files.len() {
            let file = FileIx(file_ix as u32);
            if self.only.is_some_and(|only| only != file) {
                continue;
            }
            let mut emit = Emitter {
                rows: &mut rows,
                base: 0,
                block_rows: &mut block_rows,
                file_rows: &mut file_rows,
                hunk_rows: &mut hunk_rows,
                blocks: blocks.file(file),
                file,
            };
            emit.file(&self.diff, self.layout, self.show(file));
        }
        self.span_starts = self
            .diff
            .blocks
            .iter()
            .enumerate()
            .zip(&block_rows)
            .map(|((ix, block), &row)| {
                let id = BlockId(ix as u32);
                if rows.get(row as usize) == Some(&Row::Block { id }) {
                    span_start(&rows, block, row)
                } else {
                    row
                }
            })
            .collect();
        self.rows = rows;
        self.block_rows = block_rows;
        self.file_rows = file_rows;
        self.hunk_rows = hunk_rows;
        self.rebuild_index();
    }

    fn show(&self, file: FileIx) -> FileShow<'_> {
        let ix = file.0 as usize;
        FileShow {
            mode: self.contexts[ix],
            collapsed: self.collapsed[ix],
            reveals: &self.reveals[ix],
        }
    }

    /// Re-projects one shown file's rows in place: the rows, the height
    /// index and the row numbers after the file move by the rows it gained
    /// or lost, and the rest of the list is left as it is.
    fn reproject_file(&mut self, file: FileIx) {
        let Some(start) = self.file_row(file) else {
            return;
        };
        let end = self
            .file_rows
            .iter()
            .map(|&row| row as usize)
            .find(|&row| row > start)
            .unwrap_or(self.rows.len());
        let mut rows = Vec::with_capacity(end - start);
        let mut hunk_rows = Vec::new();
        let mut blocks = BlocksByFile::new(&self.diff);
        let ix = file.0 as usize;
        let show = FileShow {
            mode: self.contexts[ix],
            collapsed: self.collapsed[ix],
            reveals: &self.reveals[ix],
        };
        let mut emit = Emitter {
            rows: &mut rows,
            base: start as u32,
            block_rows: &mut self.block_rows,
            file_rows: &mut Vec::new(),
            hunk_rows: &mut hunk_rows,
            blocks: blocks.file(file),
            file,
        };
        emit.file(&self.diff, self.layout, show);
        let delta = rows.len() as i64 - (end - start) as i64;
        let shift = |row: &mut u32| *row = (i64::from(*row) + delta) as u32;
        let heights: Vec<RowHeight> = self.heights_of(&rows).collect();
        self.index.splice(start..end, heights);
        self.rows.splice(start..end, rows);
        self.file_rows
            .iter_mut()
            .filter(|row| **row as usize > start)
            .for_each(shift);
        let from = self
            .hunk_rows
            .partition_point(|&row| (row as usize) < start);
        let to = self.hunk_rows.partition_point(|&row| (row as usize) < end);
        let added = hunk_rows.len();
        self.hunk_rows.splice(from..to, hunk_rows);
        self.hunk_rows[from + added..].iter_mut().for_each(shift);
        // A list restricted to one file has no rows after it, and the blocks
        // of the files left out stay at row 0.
        let later = |block: &Block| self.only.is_none() && block.anchor.file() > file;
        for (ix, block) in self.diff.blocks.iter().enumerate() {
            if block.anchor.file() == file {
                let id = BlockId(ix as u32);
                let row = self.block_rows[ix];
                self.span_starts[ix] = if self.rows.get(row as usize) == Some(&Row::Block { id }) {
                    span_start(&self.rows, block, row)
                } else {
                    row
                };
            } else if later(block) {
                shift(&mut self.block_rows[ix]);
                shift(&mut self.span_starts[ix]);
            }
        }
    }

    fn heights_of<'a>(&'a self, rows: &'a [Row]) -> impl Iterator<Item = RowHeight> + 'a {
        let file_header = RowHeight::Sized(self.heights.file_header.into());
        rows.iter().map(move |row| match row {
            Row::FileHeader { .. } => file_header,
            Row::Block { id } => RowHeight::Sized(self.diff.block(*id).height.into()),
            Row::HunkHeader { .. } | Row::Expander { .. } | Row::Line { .. } | Row::Pair { .. } => {
                RowHeight::Code
            }
        })
    }

    fn rebuild_index(&mut self) {
        self.index = HeightIndex::new(self.heights.code.into(), self.heights_of(&self.rows));
    }
}

/// The lines under a hunk header, see [`DiffList::header_span`]. Starts are
/// 0-based.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HunkSpan {
    pub old_start: u32,
    pub old_len: u32,
    pub new_start: u32,
    pub new_len: u32,
}

/// The first row of a block's span: walks up from the block over code rows
/// and other blocks while they still show lines of its range.
fn span_start(rows: &[Row], block: &Block, block_row: u32) -> u32 {
    let Anchor::Line { side, line, .. } = block.anchor else {
        return block_row;
    };
    let first = block.first_line.unwrap_or(line).min(line);
    let mut start = block_row;
    for row in (0..block_row).rev() {
        let shown = match rows[row as usize] {
            Row::Block { .. } => continue,
            Row::Line { old, new, .. } => line_on(side, old, new),
            Row::Pair { left, right, .. } => {
                line_on(side, left.map(|s| s.line), right.map(|s| s.line))
            }
            Row::FileHeader { .. } | Row::HunkHeader { .. } | Row::Expander { .. } => break,
        };
        match shown {
            Some(at) if (first..=line).contains(&at) => start = row,
            Some(at) if at < first => break,
            // A line past the anchor (the block sits at the file's end), or
            // a row of the other side inside the range.
            _ => {}
        }
    }
    start
}

fn line_on(side: Side, old: Option<u32>, new: Option<u32>) -> Option<u32> {
    match side {
        Side::Old => old,
        Side::New => new,
    }
}

/// Block ids grouped by file: those at the file's top, then each side's in
/// line order.
struct BlocksByFile {
    top: Vec<Vec<BlockId>>,
    old: Vec<Vec<(u32, BlockId)>>,
    new: Vec<Vec<(u32, BlockId)>>,
}

impl BlocksByFile {
    fn new(diff: &Diff) -> Self {
        let mut top = vec![Vec::new(); diff.files.len()];
        let mut old = vec![Vec::new(); diff.files.len()];
        let mut new = vec![Vec::new(); diff.files.len()];
        for (ix, block) in diff.blocks.iter().enumerate() {
            let id = BlockId(ix as u32);
            match block.anchor {
                Anchor::FileTop { file } => top[file.0 as usize].push(id),
                Anchor::Line { file, side, line } => {
                    let by_side = match side {
                        Side::Old => &mut old,
                        Side::New => &mut new,
                    };
                    by_side[file.0 as usize].push((line, id));
                }
            }
        }
        for list in old.iter_mut().chain(new.iter_mut()) {
            // Stable, so blocks on one line keep their id order.
            list.sort_by_key(|&(line, _)| line);
        }
        Self { top, old, new }
    }

    fn file(&mut self, file: FileIx) -> FileBlocks {
        let ix = file.0 as usize;
        FileBlocks {
            top: std::mem::take(&mut self.top[ix]),
            old: std::mem::take(&mut self.old[ix]),
            new: std::mem::take(&mut self.new[ix]),
            old_next: 0,
            new_next: 0,
        }
    }
}

/// One file's blocks, consumed in line order as rows are emitted. Both
/// sides' line numbers only grow while walking a file, so this is a merge.
struct FileBlocks {
    top: Vec<BlockId>,
    old: Vec<(u32, BlockId)>,
    new: Vec<(u32, BlockId)>,
    old_next: usize,
    new_next: usize,
}

impl FileBlocks {
    /// Blocks anchored at or before the given lines that are not yet emitted.
    fn take_through(&mut self, old: Option<u32>, new: Option<u32>, out: &mut Vec<BlockId>) {
        if let Some(line) = old {
            while let Some(&(at, id)) = self.old.get(self.old_next)
                && at <= line
            {
                out.push(id);
                self.old_next += 1;
            }
        }
        if let Some(line) = new {
            while let Some(&(at, id)) = self.new.get(self.new_next)
                && at <= line
            {
                out.push(id);
                self.new_next += 1;
            }
        }
    }
}

/// How one file is shown.
struct FileShow<'a> {
    mode: ContextMode,
    collapsed: bool,
    reveals: &'a Reveals,
}

struct Emitter<'a> {
    rows: &'a mut Vec<Row>,
    /// The list row of `rows[0]`: the rows recorded count from it.
    base: u32,
    block_rows: &'a mut [u32],
    file_rows: &'a mut Vec<u32>,
    hunk_rows: &'a mut Vec<u32>,
    blocks: FileBlocks,
    file: FileIx,
}

impl Emitter<'_> {
    /// The list row the next row pushed gets.
    fn next_row(&self) -> u32 {
        self.base + self.rows.len() as u32
    }

    fn file(&mut self, diff: &Diff, layout: Layout, show: FileShow) {
        let file = self.file;
        let header = self.next_row();
        self.file_rows.push(header);
        self.rows.push(Row::FileHeader { file });
        if show.collapsed {
            // Not shown, but each block's row stays inside the file's.
            let blocks = &self.blocks;
            let ids = blocks
                .top
                .iter()
                .chain(blocks.old.iter().map(|(_, id)| id))
                .chain(blocks.new.iter().map(|(_, id)| id));
            for id in ids {
                self.block_rows[id.0 as usize] = header;
            }
            return;
        }
        let mut pending = std::mem::take(&mut self.blocks.top);
        self.flush(&mut pending);
        match show.mode {
            ContextMode::Hunks => self.hunks(diff, layout, show.reveals, &mut pending),
            ContextMode::Full => self.full(diff, layout, &mut pending),
        }
        // Blocks anchored to lines no row shows go at the end of the file.
        self.blocks
            .take_through(Some(u32::MAX), Some(u32::MAX), &mut pending);
        self.flush(&mut pending);
    }

    /// The hunks, each under its header, with the context revealed in the
    /// gaps around them. A gap revealed whole joins its hunk to the one
    /// before it (or to the file's start) without a header; hidden lines
    /// after the last hunk leave an expander row.
    fn hunks(
        &mut self,
        diff: &Diff,
        layout: Layout,
        reveals: &Reveals,
        pending: &mut Vec<BlockId>,
    ) {
        let file = self.file;
        let f = diff.file(file);
        for (hunk_ix, hunk) in f.hunks.iter().enumerate() {
            let hunk_ix = hunk_ix as u32;
            let gap = f.gap(hunk_ix).expect("a gap before every hunk");
            let shown = reveals.gap(hunk_ix, gap.len);
            self.context(layout, (gap.old, gap.new), shown.top, pending);
            let joined = gap.len > 0 && shown.hidden(gap.len) == 0;
            if !joined {
                self.hunk_rows.push(self.next_row());
                self.rows.push(Row::HunkHeader {
                    file,
                    hunk: hunk_ix,
                });
            }
            let below = gap.len - shown.bottom;
            let at = (gap.old + below, gap.new + below);
            self.context(layout, at, shown.bottom, pending);
            self.segments(layout, hunk, pending, joined);
        }
        let tail = f.hunks.len() as u32;
        if let Some(gap) = f.gap(tail) {
            let shown = reveals.gap(tail, gap.len);
            self.context(layout, (gap.old, gap.new), shown.top, pending);
            if shown.hidden(gap.len) > 0 {
                self.rows.push(Row::Expander { file });
            }
            let below = gap.len - shown.bottom;
            let at = (gap.old + below, gap.new + below);
            self.context(layout, at, shown.bottom, pending);
        }
    }

    /// Every line of the file: the context between hunks is filled in from
    /// the blobs' line counts.
    fn full(&mut self, diff: &Diff, layout: Layout, pending: &mut Vec<BlockId>) {
        let file = diff.file(self.file);
        let (mut old, mut new) = (0, 0);
        for hunk in &file.hunks {
            let gap = hunk.old_start.saturating_sub(old);
            self.context(layout, (old, new), gap, pending);
            self.segments(layout, hunk, pending, true);
            old = hunk.old_start + hunk.old_len();
            new = hunk.new_start + hunk.new_len();
        }
        let tail = file.old.line_count().saturating_sub(old);
        self.context(layout, (old, new), tail, pending);
    }

    /// `n` context lines starting at the 0-based lines `(old, new)`.
    fn context(
        &mut self,
        layout: Layout,
        (old, new): (u32, u32),
        n: u32,
        pending: &mut Vec<BlockId>,
    ) {
        for k in 0..n {
            self.code(
                layout,
                Some((old + k, LineKind::Context)),
                Some((new + k, LineKind::Context)),
                pending,
            );
        }
    }

    /// The rows of one hunk's segments. With `mark`, the first changed row is
    /// the one [`DiffList::hunk_rows`] gets for the hunk.
    fn segments(&mut self, layout: Layout, hunk: &Hunk, pending: &mut Vec<BlockId>, mark: bool) {
        let (mut old, mut new) = (hunk.old_start, hunk.new_start);
        let mut marked = !mark;
        for &segment in &hunk.segments {
            match segment {
                Segment::Context(n) => self.context(layout, (old, new), n, pending),
                Segment::Change { removed, added } => {
                    if !marked {
                        self.hunk_rows.push(self.next_row());
                        marked = true;
                    }
                    self.change(layout, (old, new), (removed, added), pending);
                }
            }
            old += segment.old_len();
            new += segment.new_len();
        }
    }

    fn change(
        &mut self,
        layout: Layout,
        (old, new): (u32, u32),
        (removed, added): (u32, u32),
        pending: &mut Vec<BlockId>,
    ) {
        match layout {
            Layout::Unified => {
                for k in 0..removed {
                    self.code(layout, Some((old + k, LineKind::Removed)), None, pending);
                }
                for k in 0..added {
                    self.code(layout, None, Some((new + k, LineKind::Added)), pending);
                }
            }
            Layout::Split => {
                for k in 0..removed.max(added) {
                    let left = (k < removed).then(|| (old + k, LineKind::Removed));
                    let right = (k < added).then(|| (new + k, LineKind::Added));
                    self.code(layout, left, right, pending);
                }
            }
        }
    }

    fn code(
        &mut self,
        layout: Layout,
        old: Option<(u32, LineKind)>,
        new: Option<(u32, LineKind)>,
        pending: &mut Vec<BlockId>,
    ) {
        let file = self.file;
        self.rows.push(match layout {
            Layout::Unified => Row::Line {
                file,
                old: old.map(|(l, _)| l),
                new: new.map(|(l, _)| l),
            },
            Layout::Split => Row::Pair {
                file,
                left: old.map(|(line, kind)| SideLine { line, kind }),
                right: new.map(|(line, kind)| SideLine { line, kind }),
            },
        });
        self.blocks
            .take_through(old.map(|(l, _)| l), new.map(|(l, _)| l), pending);
        self.flush(pending);
    }

    fn flush(&mut self, pending: &mut Vec<BlockId>) {
        for id in pending.drain(..) {
            self.block_rows[id.0 as usize] = self.next_row();
            self.rows.push(Row::Block { id });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Text;
    use crate::model::{FileDiff, Hunk};

    const HEIGHTS: RowHeights = RowHeights {
        code: 20.0,
        file_header: 32.0,
    };

    fn numbered(n: u32, tag: &str) -> Text {
        Text::new((0..n).map(|i| format!("{tag}{i}\n")).collect::<String>())
    }

    /// One file, one hunk: 2 context, 3 removed / 1 added, 1 context, 0 removed / 2 added.
    fn sample(blocks: Vec<Block>) -> Diff {
        let hunk = Hunk {
            old_start: 10,
            new_start: 10,
            segments: vec![
                Segment::Context(2),
                Segment::Change {
                    removed: 3,
                    added: 1,
                },
                Segment::Context(1),
                Segment::Change {
                    removed: 0,
                    added: 2,
                },
            ],
        };
        assert_eq!((hunk.old_len(), hunk.new_len()), (6, 6));
        Diff {
            files: vec![FileDiff {
                path: "a.rs".into(),
                old: numbered(30, "o"),
                new: numbered(30, "n"),
                hunks: vec![hunk],
            }],
            blocks,
        }
    }

    fn anchor(side: Side, line: u32) -> Anchor {
        Anchor::Line {
            file: FileIx(0),
            side,
            line,
        }
    }

    #[test]
    fn unified_and_split_counts() {
        let unified = DiffList::new(sample(vec![]), Layout::Unified, HEIGHTS);
        // header + hunk header + 2 + 3 + 1 + 1 + 2 + the expander of the 14 lines after
        assert_eq!(unified.len(), 12);
        let mut split = DiffList::new(sample(vec![]), Layout::Split, HEIGHTS);
        // header + hunk header + 2 + max(3,1) + 1 + max(0,2) + the expander
        assert_eq!(split.len(), 11);
        split.set_layout(Layout::Unified);
        assert_eq!(split.rows(), unified.rows());
    }

    #[test]
    fn new_heights_rescale_the_index_not_the_rows() {
        let blocks = vec![Block::new(anchor(Side::New, 10), 50.0)];
        let mut list = DiffList::new(sample(blocks), Layout::Unified, HEIGHTS);
        let rows = list.rows().to_vec();
        let before = list.total_height();
        let taller = RowHeights {
            code: 30.0,
            file_header: 40.0,
        };
        list.set_heights(taller);
        assert_eq!(list.rows(), rows);
        assert_eq!(list.heights(), taller);
        // 1 file header, 1 block of 50, 11 code-height rows (the expander too).
        assert_eq!(list.total_height(), 40.0 + 50.0 + 11.0 * 30.0);
        assert!(list.total_height() > before);
        assert_eq!(list.row_height(1), 30.0);
        assert_eq!(list.top_of(2), 70.0);
    }

    #[test]
    fn split_pairs_removed_with_added() {
        let list = DiffList::new(sample(vec![]), Layout::Split, HEIGHTS);
        let fourth = list.rows()[4];
        assert_eq!(
            fourth,
            Row::Pair {
                file: FileIx(0),
                left: Some(SideLine {
                    line: 12,
                    kind: LineKind::Removed
                }),
                right: Some(SideLine {
                    line: 12,
                    kind: LineKind::Added
                }),
            }
        );
        let Row::Pair {
            left: Some(_),
            right: None,
            ..
        } = list.rows()[5]
        else {
            panic!("extra removed line not alone: {:?}", list.rows()[5]);
        };
        let Row::Pair {
            left: None,
            right: Some(_),
            ..
        } = list.rows()[8]
        else {
            panic!("pure addition not alone: {:?}", list.rows()[8]);
        };
    }

    #[test]
    fn unified_kinds_follow_segments() {
        let list = DiffList::new(sample(vec![]), Layout::Unified, HEIGHTS);
        let kinds: Vec<LineKind> = list
            .rows()
            .iter()
            .filter_map(|row| match *row {
                Row::Line { old, new, .. } => Some(Row::line_kind(old, new)),
                _ => None,
            })
            .collect();
        use LineKind::{Added as A, Context as C, Removed as R};
        assert_eq!(kinds, [C, C, R, R, R, A, C, A, A]);
    }

    #[test]
    fn blocks_follow_their_anchor_in_both_layouts() {
        let blocks = vec![
            Block::new(anchor(Side::Old, 13), 80.0),
            Block::new(anchor(Side::New, 10), 50.0),
            Block::new(anchor(Side::New, 29), 40.0),
        ];
        let mut list = DiffList::new(sample(blocks), Layout::Unified, HEIGHTS);
        assert_eq!(list.len(), 15);
        let row_before = |list: &DiffList, id| list.rows()[list.block_row(BlockId(id)) - 1];
        assert_eq!(
            row_before(&list, 0),
            Row::Line {
                file: FileIx(0),
                old: Some(13),
                new: None
            }
        );
        assert_eq!(
            row_before(&list, 1),
            Row::Line {
                file: FileIx(0),
                old: Some(10),
                new: Some(10)
            }
        );
        // Anchored past the hunk: last row of the file.
        assert_eq!(list.block_row(BlockId(2)), list.len() - 1);

        list.set_layout(Layout::Split);
        assert_eq!(list.len(), 14);
        for id in 0..3 {
            assert_eq!(
                list.rows()[list.block_row(BlockId(id))],
                Row::Block { id: BlockId(id) }
            );
        }
    }

    #[test]
    fn file_top_blocks_sit_under_the_header() {
        let blocks = vec![
            Block::new(anchor(Side::New, 11), 40.0),
            Block::new(Anchor::FileTop { file: FileIx(0) }, 30.0),
        ];
        let list = DiffList::new(sample(blocks), Layout::Split, HEIGHTS);
        assert_eq!(list.block_row(BlockId(1)), 1);
        assert_eq!(
            list.rows()[2],
            Row::HunkHeader {
                file: FileIx(0),
                hunk: 0
            }
        );
        assert!(list.block_span(BlockId(1)).is_empty());
        assert_eq!(list.file_rows(), [0]);
        assert_eq!(list.hunk_rows(), [2]);
    }

    #[test]
    fn spans_cover_the_rows_of_a_range() {
        // New lines 10..=13 are context 10, 11, the added 12, context 13.
        let mut ranged = Block::new(anchor(Side::New, 13), 40.0);
        ranged.first_line = Some(11);
        let blocks = vec![ranged, Block::new(anchor(Side::New, 12), 20.0)];
        let mut list = DiffList::new(sample(blocks), Layout::Unified, HEIGHTS);
        let span = list.block_span(BlockId(0));
        let shown: Vec<Option<u32>> = list.rows()[span.clone()]
            .iter()
            .map(|row| match *row {
                Row::Line { new, .. } => new,
                _ => None,
            })
            .collect();
        // 11, the three removed lines (old side only), 12, the other block, 13.
        assert_eq!(
            shown,
            [Some(11), None, None, None, Some(12), None, Some(13)]
        );
        assert_eq!(span.end, list.block_row(BlockId(0)));
        let single = list.block_span(BlockId(1));
        assert_eq!(single.len(), 1);
        list.set_layout(Layout::Split);
        let span = list.block_span(BlockId(0));
        assert!(matches!(
            list.rows()[span.start],
            Row::Pair {
                right: Some(SideLine { line: 11, .. }),
                ..
            }
        ));
    }

    #[test]
    fn set_blocks_reprojects() {
        let mut list = DiffList::new(sample(vec![]), Layout::Unified, HEIGHTS);
        assert_eq!(list.len(), 12);
        list.set_blocks(vec![Block::new(anchor(Side::Old, 12), 50.0)]);
        assert_eq!(list.len(), 13);
        assert_eq!(list.total_height(), 32.0 + 50.0 + 11.0 * 20.0);
    }

    #[test]
    fn hunks_know_the_lines_they_show() {
        let diff = sample(vec![]);
        let file = &diff.files[0];
        assert!(file.shows(Side::New, 10) && file.shows(Side::New, 15));
        assert!(!file.shows(Side::New, 16) && !file.shows(Side::Old, 9));
    }

    #[test]
    fn heights_match_rows_and_block_resize() {
        let blocks = vec![Block::new(anchor(Side::New, 11), 80.0)];
        let mut list = DiffList::new(sample(blocks), Layout::Unified, HEIGHTS);
        let sum: f64 = (0..list.len()).map(|r| list.row_height(r)).sum();
        assert_eq!(list.total_height(), sum);
        let block = list.block_row(BlockId(0));
        let below = list.top_of(block + 1);
        list.set_block_height(BlockId(0), 200.0);
        assert_eq!(list.top_of(block + 1), below + 120.0);
        assert_eq!(list.row_height(block), 200.0);
        // A layout switch keeps the new height.
        list.set_layout(Layout::Split);
        assert_eq!(list.row_height(list.block_row(BlockId(0))), 200.0);
    }

    fn two_files(blocks: Vec<Block>) -> Diff {
        let mut diff = sample(blocks);
        let second = diff.files[0].clone();
        diff.files.push(second);
        diff
    }

    #[test]
    fn full_context_shows_every_line_with_changes_inline() {
        let mut list = DiffList::new(sample(vec![]), Layout::Unified, HEIGHTS);
        list.set_context(None, ContextMode::Full);
        // header + 10 lines before the hunk + the hunk's 9 + 14 after.
        assert_eq!(list.len(), 1 + 10 + 9 + 14);
        assert!(
            list.rows()
                .iter()
                .all(|r| !matches!(r, Row::HunkHeader { .. }))
        );
        // The new side's lines run 0..30 without a gap, and the old side's
        // too, counting the removed lines.
        let (mut old_seen, mut new_seen) = (Vec::new(), Vec::new());
        for row in list.rows() {
            if let Row::Line { old, new, .. } = *row {
                old_seen.extend(old);
                new_seen.extend(new);
            }
        }
        assert_eq!(old_seen, (0..30).collect::<Vec<_>>());
        assert_eq!(new_seen, (0..30).collect::<Vec<_>>());
        // The hunk's first changed row: after 10 lines + 2 context.
        assert_eq!(list.hunk_rows(), [1 + 10 + 2]);
        assert!(list.shows(FileIx(0), Side::New, 29));
        assert!(!list.shows(FileIx(0), Side::New, 30));
        list.set_context(None, ContextMode::Hunks);
        assert_eq!(list.len(), 12);
        assert_eq!(list.hunk_rows(), [1]);
    }

    #[test]
    fn full_context_in_split_keeps_pairs_and_blocks() {
        let blocks = vec![
            Block::new(anchor(Side::New, 3), 40.0),
            Block::new(anchor(Side::New, 20), 50.0),
        ];
        let mut list = DiffList::new(sample(blocks), Layout::Split, HEIGHTS);
        // Outside the hunks, the blocks of lines 3 and 20 crowd up at its
        // first row and the end of the file.
        assert_eq!(list.block_row(BlockId(0)), 3);
        list.set_context(Some(FileIx(0)), ContextMode::Full);
        assert_eq!(list.context(FileIx(0)), ContextMode::Full);
        // header + 10 + 8 pairs + 14 + 2 blocks.
        assert_eq!(list.len(), 1 + 10 + 8 + 14 + 2);
        assert_eq!(
            list.rows()[list.block_row(BlockId(0)) - 1],
            Row::Pair {
                file: FileIx(0),
                left: Some(SideLine {
                    line: 3,
                    kind: LineKind::Context
                }),
                right: Some(SideLine {
                    line: 3,
                    kind: LineKind::Context
                }),
            }
        );
        let sum: f64 = (0..list.len()).map(|r| list.row_height(r)).sum();
        assert_eq!(list.total_height(), sum);
        assert_eq!(list.block_row(BlockId(0)), 5);
        assert!(list.shows_block(BlockId(1)));
    }

    #[test]
    fn full_context_of_a_large_file_shares_its_text() {
        let text = |tag: &str| numbered(20_000, tag);
        let (old, new) = (text("o"), text("n"));
        let (old_ptr, new_ptr) = (old.as_bytes().as_ptr(), new.as_bytes().as_ptr());
        let diff = Diff {
            files: vec![FileDiff {
                path: "big.rs".into(),
                old,
                new,
                hunks: vec![
                    Hunk {
                        old_start: 100,
                        new_start: 100,
                        segments: vec![
                            Segment::Context(3),
                            Segment::Change {
                                removed: 2,
                                added: 3,
                            },
                            Segment::Context(3),
                        ],
                    },
                    Hunk {
                        old_start: 15_000,
                        new_start: 15_001,
                        segments: vec![
                            Segment::Context(3),
                            Segment::Change {
                                removed: 1,
                                added: 0,
                            },
                            Segment::Context(3),
                        ],
                    },
                ],
            }],
            blocks: Vec::new(),
        };
        let mut list = DiffList::new(diff, Layout::Unified, HEIGHTS);
        let hunks_len = list.len();
        list.set_context(None, ContextMode::Full);
        // Every new line once, plus the removed ones: 3 and 1.
        assert_eq!(list.len(), 1 + 20_000 + 3);
        assert!(list.len() > hunks_len);
        assert_eq!(list.hunk_rows().len(), 2);
        let file = list.diff().file(FileIx(0));
        assert_eq!(file.old.as_bytes().as_ptr(), old_ptr);
        assert_eq!(file.new.as_bytes().as_ptr(), new_ptr);
        assert_eq!(list.total_height(), 32.0 + 20_003.0 * 20.0);
        // Rows are small plain data, not copies of lines.
        assert!(std::mem::size_of::<Row>() <= 40);
    }

    #[test]
    fn a_collapsed_file_keeps_only_its_header() {
        let blocks = vec![
            Block::new(anchor(Side::New, 11), 40.0),
            Block::new(Anchor::FileTop { file: FileIx(1) }, 30.0),
        ];
        let mut list = DiffList::new(two_files(blocks), Layout::Unified, HEIGHTS);
        let open = list.len();
        assert_eq!(open, 2 * 12 + 2);
        list.set_collapsed(vec![true]);
        assert!(list.is_collapsed(FileIx(0)) && !list.is_collapsed(FileIx(1)));
        // Header of file 0, then file 1 whole with its block.
        assert_eq!(list.len(), 1 + 12 + 1);
        assert_eq!(list.file_rows(), [0, 1]);
        assert!(!list.shows_block(BlockId(0)));
        assert_eq!(list.block_row(BlockId(0)), 0);
        assert!(list.block_span(BlockId(0)).is_empty());
        assert!(list.shows_block(BlockId(1)));
        // A hidden block's height changes without touching the index.
        let total = list.total_height();
        list.set_block_height(BlockId(0), 300.0);
        assert_eq!(list.total_height(), total);
        list.set_block_height(BlockId(1), 130.0);
        assert_eq!(list.total_height(), total + 100.0);
        list.set_collapsed(Vec::new());
        assert_eq!(list.len(), open);
        assert_eq!(list.row_height(list.block_row(BlockId(0))), 300.0);
    }

    #[test]
    fn restricting_to_one_file_hides_the_others() {
        let blocks = vec![
            Block::new(anchor(Side::New, 11), 40.0),
            Block::new(
                Anchor::Line {
                    file: FileIx(1),
                    side: Side::New,
                    line: 11,
                },
                60.0,
            ),
        ];
        let mut list = DiffList::new(two_files(blocks), Layout::Unified, HEIGHTS);
        assert_eq!(list.file_row(FileIx(1)), Some(13));
        list.set_only(Some(FileIx(1)));
        assert_eq!(list.only(), Some(FileIx(1)));
        assert_eq!(list.len(), 13);
        assert_eq!(list.file_row(FileIx(1)), Some(0));
        assert_eq!(list.file_row(FileIx(0)), None);
        assert_eq!(list.file_of_row(5), FileIx(1));
        assert!(!list.shows_block(BlockId(0)));
        assert!(list.shows_block(BlockId(1)));
        assert_eq!(list.hunk_rows(), [1]);
        list.set_only(None);
        assert_eq!(list.len(), 2 * 12 + 2);
        assert_eq!(list.file_of_row(0), FileIx(0));
        assert_eq!(list.file_of_row(13), FileIx(1));
        assert_eq!(list.file_of_row(list.len() - 1), FileIx(1));
    }

    /// 100 old lines and 101 new: hunk 0 at 30 (one line replaced by two),
    /// hunk 1 at old 60 / new 61 (one line replaced). Gaps: 30 lines before
    /// hunk 0, 25 between (old 35.., new 36..), 35 after (old 65.., new 66..).
    fn gapped(blocks: Vec<Block>) -> Diff {
        let hunk = |old_start, new_start, added| Hunk {
            old_start,
            new_start,
            segments: vec![
                Segment::Context(2),
                Segment::Change { removed: 1, added },
                Segment::Context(2),
            ],
        };
        Diff {
            files: vec![FileDiff {
                path: "g.rs".into(),
                old: numbered(100, "o"),
                new: numbered(101, "n"),
                hunks: vec![hunk(30, 30, 2), hunk(60, 61, 1)],
            }],
            blocks,
        }
    }

    const F: FileIx = FileIx(0);

    fn header_row(list: &DiffList, hunk: u32) -> Option<usize> {
        list.rows()
            .iter()
            .position(|row| *row == Row::HunkHeader { file: F, hunk })
    }

    /// The (old, new) lines of the unified rows from `rows`.
    fn lines(list: &DiffList, rows: Range<usize>) -> Vec<(Option<u32>, Option<u32>)> {
        list.rows()[rows]
            .iter()
            .map(|row| match *row {
                Row::Line { old, new, .. } => (old, new),
                other => panic!("not a line: {other:?}"),
            })
            .collect()
    }

    fn context_lines(old: Range<u32>, new: u32) -> Vec<(Option<u32>, Option<u32>)> {
        old.enumerate()
            .map(|(k, line)| (Some(line), Some(new + k as u32)))
            .collect()
    }

    #[test]
    fn expanding_up_reveals_twenty_lines_above_the_hunk() {
        let mut list = DiffList::new(gapped(vec![]), Layout::Unified, HEIGHTS);
        assert_eq!(list.arrows(F, 1), [Expand::Down, Expand::Up]);
        let len = list.len();
        assert!(list.expand(F, 1, Expand::Up));
        assert_eq!(list.len(), len + 20);
        let header = header_row(&list, 1).expect("still a header: 5 lines are hidden");
        // Old 40..60 is new 41..61, under the header and above the hunk.
        assert_eq!(
            lines(&list, header + 1..header + 21),
            context_lines(40..60, 41)
        );
        assert_eq!(
            list.rows()[header + 21],
            Row::Line {
                file: F,
                old: Some(60),
                new: Some(61)
            }
        );
        assert!(list.shows(F, Side::New, 41) && list.shows(F, Side::Old, 59));
        assert!(!list.shows(F, Side::Old, 39));
        assert_eq!(list.arrows(F, 1), [Expand::All]);
    }

    #[test]
    fn expanding_down_reveals_twenty_lines_below_the_hunk_before() {
        let mut list = DiffList::new(gapped(vec![]), Layout::Unified, HEIGHTS);
        let header = header_row(&list, 1).expect("header");
        assert!(list.expand(F, 1, Expand::Down));
        // The lines come where the header was, which moves down past them.
        assert_eq!(lines(&list, header..header + 20), context_lines(35..55, 36));
        assert_eq!(header_row(&list, 1), Some(header + 20));
        assert_eq!(list.hunk_rows()[1] as usize, header + 20);
        let span = list.header_span(F, 1);
        assert_eq!(
            (span.old_start, span.old_len, span.new_start, span.new_len),
            (60, 5, 61, 5)
        );
        // The first hunk's header now counts the lines revealed under it.
        let first = list.header_span(F, 0);
        assert_eq!(
            (first.old_start, first.old_len, first.new_len),
            (30, 25, 26)
        );
    }

    #[test]
    fn a_gap_revealed_whole_joins_its_hunks() {
        let mut list = DiffList::new(gapped(vec![]), Layout::Unified, HEIGHTS);
        list.expand(F, 1, Expand::Up);
        assert!(list.expand(F, 1, Expand::All));
        assert_eq!(header_row(&list, 1), None);
        assert!(list.arrows(F, 1).is_empty());
        // From hunk 0's header, the new side runs on without a gap through
        // the end of hunk 1.
        let first = header_row(&list, 0).expect("header 0");
        let new: Vec<u32> = list.rows()[first + 1..]
            .iter()
            .filter_map(|row| match *row {
                Row::Line { new, .. } => new,
                _ => None,
            })
            .take_while(|&line| line <= 65)
            .collect();
        assert_eq!(new, (30..66).collect::<Vec<_>>());
        // The step to hunk 1 lands on its first changed row.
        let step = list.hunk_rows()[1] as usize;
        assert_eq!(
            list.rows()[step],
            Row::Line {
                file: F,
                old: Some(62),
                new: None
            }
        );
        // One header spans both hunks.
        let span = list.header_span(F, 0);
        assert_eq!(
            (span.old_start, span.old_len, span.new_start, span.new_len),
            (30, 35, 30, 36)
        );
    }

    #[test]
    fn the_last_gap_has_an_expander_row_until_it_is_shown() {
        let mut list = DiffList::new(gapped(vec![]), Layout::Unified, HEIGHTS);
        assert_eq!(list.rows().last(), Some(&Row::Expander { file: F }));
        assert_eq!(Row::Expander { file: F }.gap(list.diff()), Some((F, 2)));
        assert_eq!(list.arrows(F, 2), [Expand::Down]);
        let end = list.len() - 1;
        assert!(list.expand(F, 2, Expand::Down));
        assert_eq!(lines(&list, end..end + 20), context_lines(65..85, 66));
        assert_eq!(list.rows().last(), Some(&Row::Expander { file: F }));
        // 15 left: the same arrow takes them all.
        assert!(list.expand(F, 2, Expand::Down));
        assert_ne!(list.rows().last(), Some(&Row::Expander { file: F }));
        assert_eq!(
            list.rows().last(),
            Some(&Row::Line {
                file: F,
                old: Some(99),
                new: Some(100)
            })
        );
        assert!(list.arrows(F, 2).is_empty());
        assert!(!list.expand(F, 2, Expand::Down));
    }

    #[test]
    fn no_arrows_where_nothing_is_hidden() {
        // The sample's one hunk starts at line 10 and its file ends 14 lines
        // after it; trimmed to start at 0 and end with the hunk, nothing is
        // hidden.
        let mut diff = sample(vec![]);
        let file = &mut diff.files[0];
        file.hunks[0].old_start = 0;
        file.hunks[0].new_start = 0;
        file.old = numbered(6, "o");
        file.new = numbered(6, "n");
        let mut list = DiffList::new(diff, Layout::Unified, HEIGHTS);
        assert_eq!(list.rows()[1], Row::HunkHeader { file: F, hunk: 0 });
        assert!(list.arrows(F, 0).is_empty() && list.arrows(F, 1).is_empty());
        assert!(
            !list
                .rows()
                .iter()
                .any(|row| matches!(row, Row::Expander { .. }))
        );
        assert!(!list.expand(F, 0, Expand::All));
        // A small first gap takes one arrow for all of it, and the hunk
        // then has no header.
        let mut list = DiffList::new(sample(vec![]), Layout::Unified, HEIGHTS);
        assert_eq!(list.arrows(F, 0), [Expand::All]);
        assert!(list.expand(F, 0, Expand::All));
        assert_eq!(lines(&list, 1..11), context_lines(0..10, 0));
        assert_eq!(header_row(&list, 0), None);
        assert_eq!(list.hunk_rows(), [1 + 10 + 2]);
    }

    #[test]
    fn split_reveals_context_pairs() {
        let mut list = DiffList::new(gapped(vec![]), Layout::Split, HEIGHTS);
        list.expand(F, 1, Expand::Up);
        let header = header_row(&list, 1).expect("header");
        let context = |line| {
            Some(SideLine {
                line,
                kind: LineKind::Context,
            })
        };
        assert_eq!(
            list.rows()[header + 1],
            Row::Pair {
                file: F,
                left: context(40),
                right: context(41)
            }
        );
        assert_eq!(
            list.rows()[header + 20],
            Row::Pair {
                file: F,
                left: context(59),
                right: context(60)
            }
        );
        // The same reveals in either layout.
        let mut unified = DiffList::new(gapped(vec![]), Layout::Unified, HEIGHTS);
        unified.expand(F, 1, Expand::Up);
        list.set_layout(Layout::Unified);
        assert_eq!(list.rows(), unified.rows());
    }

    #[test]
    fn full_context_has_no_arrows_and_keeps_the_reveals() {
        let mut list = DiffList::new(gapped(vec![]), Layout::Unified, HEIGHTS);
        list.expand(F, 2, Expand::Down);
        let revealed = list.rows().to_vec();
        list.set_context(None, ContextMode::Full);
        assert!(list.arrows(F, 0).is_empty() && list.arrows(F, 2).is_empty());
        assert!(!list.expand(F, 0, Expand::Up));
        list.set_context(None, ContextMode::Hunks);
        assert_eq!(list.rows(), revealed);
    }

    /// Everything [`DiffList`] derives from its rows, to compare one built in
    /// pieces with one built whole.
    #[derive(Debug, PartialEq)]
    struct State {
        rows: Vec<Row>,
        block_rows: Vec<usize>,
        spans: Vec<Range<usize>>,
        file_rows: Vec<u32>,
        hunk_rows: Vec<u32>,
        tops: Vec<f64>,
    }

    fn state(list: &DiffList) -> State {
        let ids = (0..list.diff().blocks.len() as u32).map(BlockId);
        State {
            rows: list.rows().to_vec(),
            block_rows: ids.clone().map(|id| list.block_row(id)).collect(),
            spans: ids.map(|id| list.block_span(id)).collect(),
            file_rows: list.file_rows().to_vec(),
            hunk_rows: list.hunk_rows().to_vec(),
            tops: (0..=list.len()).map(|row| list.top_of(row)).collect(),
        }
    }

    #[test]
    fn expanding_a_file_in_place_matches_a_projection_from_scratch() {
        let at = |file, side, line| Anchor::Line {
            file: FileIx(file),
            side,
            line,
        };
        let blocks = vec![
            // In hidden lines of each gap, in hunks, at the top, and in the
            // second file.
            Block::new(at(0, Side::New, 20), 40.0),
            Block::new(at(0, Side::Old, 45), 30.0),
            Block::new(at(0, Side::New, 90), 50.0),
            Block::new(at(0, Side::New, 31), 20.0),
            Block::new(Anchor::FileTop { file: FileIx(0) }, 25.0),
            Block::new(at(1, Side::New, 50), 60.0),
            Block::new(Anchor::FileTop { file: FileIx(1) }, 35.0),
            Block::new(at(2, Side::Old, 70), 45.0),
        ];
        let mut ranged = Block::new(at(0, Side::New, 46), 15.0);
        ranged.first_line = Some(40);
        let mut blocks = blocks;
        blocks.push(ranged);
        let mut diff = gapped(blocks);
        let second = diff.files[0].clone();
        diff.files.push(second.clone());
        diff.files.push(second);
        let steps = [
            (0, 1, Expand::Up),
            (1, 0, Expand::Up),
            (0, 2, Expand::Down),
            (0, 0, Expand::Up),
            (2, 2, Expand::Down),
            (0, 1, Expand::All),
            (0, 0, Expand::All),
            (1, 1, Expand::Down),
            (0, 2, Expand::Down),
        ];
        let setups: [(Layout, Option<FileIx>, Vec<bool>); 4] = [
            (Layout::Unified, None, vec![]),
            (Layout::Split, None, vec![]),
            (Layout::Unified, None, vec![false, true]),
            (Layout::Split, Some(FileIx(1)), vec![]),
        ];
        for (layout, only, collapsed) in setups {
            let mut list = DiffList::new(diff.clone(), layout, HEIGHTS);
            list.set_only(only);
            list.set_collapsed(collapsed.clone());
            for &(file, gap, expand) in &steps {
                let file = FileIx(file);
                list.expand(file, gap, expand);
                let mut whole = DiffList::new(diff.clone(), layout, HEIGHTS);
                whole.set_only(only);
                whole.set_collapsed(collapsed.clone());
                whole.set_reveals((0..3).map(|f| list.reveals(FileIx(f)).clone()).collect());
                assert_eq!(
                    state(&list),
                    state(&whole),
                    "{layout:?} {only:?} {collapsed:?} after {file:?} {gap} {expand:?}"
                );
            }
        }
    }
}
