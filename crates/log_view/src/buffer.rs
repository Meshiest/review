//! The append-only line store.

use std::collections::VecDeque;
use std::ops::Range;

use crate::ansi::{Span, Style, StyleId};

/// Most lines kept; older ones are dropped in blocks.
pub const MAX_LINES: usize = 1_000_000;
/// Most bytes held (text, spans and line records); older blocks are dropped
/// past it even when fewer than [`MAX_LINES`] are held.
pub const MAX_BYTES: usize = 256 * 1024 * 1024;
/// Lines per block. Every block but the last is full, so a line's block is
/// found by division, and dropping the oldest drops whole blocks.
pub const BLOCK_LINES: usize = 4096;

const NO_TIME: i64 = i64::MIN;

/// What a line is, from GitHub's workflow commands.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LineKind {
    #[default]
    Text,
    /// A `##[group]` line; its text is the group's title.
    GroupStart,
    /// A `##[error]` line, marker removed.
    Error,
    /// A `##[warning]` line, marker removed.
    Warning,
}

/// A line's place in its block; the end of its text and spans is where the
/// next line's begins.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LineMeta {
    pub text_start: u32,
    pub span_start: u32,
    pub time_ms: i64,
    pub kind: LineKind,
}

impl LineMeta {
    pub fn new(text_start: usize, span_start: usize, time_ms: Option<i64>, kind: LineKind) -> Self {
        Self {
            text_start: text_start as u32,
            span_start: span_start as u32,
            time_ms: time_ms.unwrap_or(NO_TIME),
            kind,
        }
    }
}

/// One line of the log.
#[derive(Clone, Copy, Debug)]
pub struct LogLine<'a> {
    /// The line's number, counting from 0 for the first line ever received;
    /// it does not change when older lines are dropped.
    pub id: u64,
    /// Printable text: no escapes, no timestamp, no workflow-command marker.
    pub text: &'a str,
    /// Style runs over `text`.
    pub spans: &'a [Span],
    /// The timestamp GitHub put at the line's start, in ms since the epoch.
    pub time_ms: Option<i64>,
    pub kind: LineKind,
}

/// A `##[group]` section: the header line and the lines up to the end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Group {
    /// The header line.
    pub start: u64,
    /// One past the last line of the body; `None` while the group is open.
    pub end: Option<u64>,
}

impl Group {
    /// The body lines: after the header, up to the end or `total`.
    #[must_use]
    pub fn body(&self, total: u64) -> Range<u64> {
        self.start + 1..self.end.unwrap_or(total).max(self.start + 1)
    }
}

/// A change to the groups, at a line number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GroupEvent {
    Start(u64),
    End(u64),
}

/// Lines parsed since the last [`LogBuffer::push`], in the layout of a block.
#[derive(Default)]
pub struct Batch {
    pub(crate) text: String,
    pub(crate) lines: Vec<LineMeta>,
    pub(crate) spans: Vec<Span>,
    pub(crate) groups: Vec<GroupEvent>,
    /// Styles first used in this batch, to append to the buffer's table.
    pub(crate) styles: Vec<Style>,
    pub(crate) max_columns: u32,
}

impl Batch {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty() && self.styles.is_empty() && self.groups.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// Input bytes it holds, to bound how much one lock hold copies.
    #[must_use]
    pub fn text_len(&self) -> usize {
        self.text.len()
    }
}

#[derive(Default)]
struct Block {
    text: String,
    lines: Vec<LineMeta>,
    spans: Vec<Span>,
}

impl Block {
    /// What the block holds, for the byte limit.
    fn bytes(&self) -> usize {
        self.text.len()
            + self.spans.len() * size_of::<Span>()
            + self.lines.len() * size_of::<LineMeta>()
    }

    fn line(&self, index: usize) -> (&str, &[Span], LineMeta) {
        let meta = self.lines[index];
        let next = self.lines.get(index + 1);
        let text_end = next.map_or(self.text.len(), |n| n.text_start as usize);
        let span_end = next.map_or(self.spans.len(), |n| n.span_start as usize);
        (
            &self.text[meta.text_start as usize..text_end],
            &self.spans[meta.span_start as usize..span_end],
            meta,
        )
    }
}

/// The lines of a log, oldest first. Appending is the only way in; when more
/// than the limit is held, the oldest blocks go and [`LogBuffer::dropped`]
/// says how many lines that was.
pub struct LogBuffer {
    blocks: VecDeque<Block>,
    block_lines: usize,
    max_lines: usize,
    max_bytes: usize,
    /// Bytes held, as [`Block::bytes`] counts them.
    bytes: usize,
    /// Lines dropped so far, which is also the id of the oldest line held.
    dropped: u64,
    len: usize,
    styles: Vec<Style>,
    groups: Vec<Group>,
    max_columns: u32,
    version: u64,
}

impl Default for LogBuffer {
    fn default() -> Self {
        Self::with_limits(MAX_LINES, BLOCK_LINES)
    }
}

impl LogBuffer {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A buffer that keeps about `max_lines`, dropping `block_lines` at a
    /// time. For tests; the real limits are [`MAX_LINES`] and [`BLOCK_LINES`].
    #[must_use]
    pub fn with_limits(max_lines: usize, block_lines: usize) -> Self {
        Self {
            blocks: VecDeque::new(),
            block_lines: block_lines.max(1),
            max_lines,
            max_bytes: MAX_BYTES,
            bytes: 0,
            dropped: 0,
            len: 0,
            styles: vec![Style::default()],
            groups: Vec::new(),
            max_columns: 0,
            version: 0,
        }
    }

    /// Sets the byte limit, which is [`MAX_BYTES`] by default.
    #[must_use]
    pub fn with_max_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    /// Bytes held by the lines.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Lines held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The id of the oldest line held; also how many were dropped.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// One past the id of the newest line.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.dropped + self.len as u64
    }

    /// Bumped by every push, so a reader can tell whether anything changed.
    #[must_use]
    pub fn version(&self) -> u64 {
        self.version
    }

    /// The widest line seen, in characters (tabs counted as one), as an upper
    /// bound for the content width. Not reduced when lines are dropped.
    #[must_use]
    pub fn max_columns(&self) -> usize {
        self.max_columns as usize
    }

    /// The line with this id, unless it was dropped or is not there yet.
    #[must_use]
    pub fn line(&self, id: u64) -> Option<LogLine<'_>> {
        let index = usize::try_from(id.checked_sub(self.dropped)?).ok()?;
        let block = self.blocks.get(index / self.block_lines)?;
        let within = index % self.block_lines;
        (within < block.lines.len()).then(|| {
            let (text, spans, meta) = block.line(within);
            LogLine {
                id,
                text,
                spans,
                time_ms: (meta.time_ms != NO_TIME).then_some(meta.time_ms),
                kind: meta.kind,
            }
        })
    }

    /// The lines with ids in `range`, clamped to what is held.
    pub fn lines(&self, range: Range<u64>) -> impl Iterator<Item = LogLine<'_>> {
        (range.start.max(self.dropped)..range.end.min(self.total())).filter_map(|id| self.line(id))
    }

    /// What a span's style id stands for.
    #[must_use]
    pub fn style(&self, id: StyleId) -> Style {
        self.styles
            .get(usize::from(id.0))
            .copied()
            .unwrap_or_default()
    }

    /// Every group, in order of their headers.
    #[must_use]
    pub fn groups(&self) -> &[Group] {
        &self.groups
    }

    /// The group whose header is line `header`.
    #[must_use]
    pub fn group_at(&self, header: u64) -> Option<&Group> {
        let index = self
            .groups
            .binary_search_by_key(&header, |g| g.start)
            .ok()?;
        self.groups.get(index)
    }

    /// The group whose header or body holds `line`.
    #[must_use]
    pub fn group_containing(&self, line: u64) -> Option<&Group> {
        let after = self.groups.partition_point(|g| g.start <= line);
        let group = self.groups.get(after.checked_sub(1)?)?;
        (line < group.end.unwrap_or(u64::MAX)).then_some(group)
    }

    /// Appends a batch from [`crate::LogParser::take`], emptying it. Returns
    /// the ids of the new lines.
    pub fn push(&mut self, batch: &mut Batch) -> Range<u64> {
        let first = self.total();
        self.styles.append(&mut batch.styles);
        for event in batch.groups.drain(..) {
            self.apply(event);
        }
        self.copy_lines(batch);
        self.max_columns = self.max_columns.max(batch.max_columns);
        batch.max_columns = 0;
        batch.text.clear();
        batch.spans.clear();
        batch.lines.clear();
        self.drop_oldest();
        self.version += 1;
        first..self.total()
    }

    /// Parses `bytes` with `parser` and appends the lines; the whole path on
    /// one thread, for tests and small logs.
    pub fn append(&mut self, parser: &mut crate::LogParser, bytes: &[u8]) -> Range<u64> {
        parser.feed(bytes);
        self.push(&mut parser.take())
    }

    fn apply(&mut self, event: GroupEvent) {
        match event {
            GroupEvent::Start(line) => {
                if let Some(open) = self.groups.last_mut()
                    && open.end.is_none()
                {
                    open.end = Some(line);
                }
                self.groups.push(Group {
                    start: line,
                    end: None,
                });
            }
            GroupEvent::End(line) => {
                if let Some(open) = self.groups.last_mut()
                    && open.end.is_none()
                {
                    open.end = Some(line);
                }
            }
        }
    }

    fn copy_lines(&mut self, batch: &Batch) {
        let mut next = 0;
        while next < batch.lines.len() {
            if self
                .blocks
                .back()
                .is_none_or(|b| b.lines.len() >= self.block_lines)
            {
                // A full block never grows again: give back the slack.
                if let Some(full) = self.blocks.back_mut() {
                    full.text.shrink_to_fit();
                    full.spans.shrink_to_fit();
                    full.lines.shrink_to_fit();
                }
                self.blocks.push_back(Block::default());
            }
            let block = self.blocks.back_mut().expect("a block was just ensured");
            let before = block.bytes();
            let take = (self.block_lines - block.lines.len()).min(batch.lines.len() - next);
            let end = next + take;
            let (text_from, span_from) = (
                batch.lines[next].text_start as usize,
                batch.lines[next].span_start as usize,
            );
            let (text_to, span_to) = batch
                .lines
                .get(end)
                .map_or((batch.text.len(), batch.spans.len()), |l| {
                    (l.text_start as usize, l.span_start as usize)
                });
            let (text_base, span_base) = (block.text.len(), block.spans.len());
            block.text.push_str(&batch.text[text_from..text_to]);
            block
                .spans
                .extend_from_slice(&batch.spans[span_from..span_to]);
            block
                .lines
                .extend(batch.lines[next..end].iter().map(|l| LineMeta {
                    text_start: l.text_start - text_from as u32 + text_base as u32,
                    span_start: l.span_start - span_from as u32 + span_base as u32,
                    ..*l
                }));
            self.bytes += block.bytes() - before;
            self.len += take;
            next = end;
        }
    }

    fn drop_oldest(&mut self) {
        while (self.len > self.max_lines || self.bytes > self.max_bytes) && self.blocks.len() > 1 {
            let Some(block) = self.blocks.pop_front() else {
                break;
            };
            self.dropped += block.lines.len() as u64;
            self.len -= block.lines.len();
            self.bytes -= block.bytes();
        }
        let oldest = self.dropped;
        self.groups.retain(|g| g.start >= oldest || g.end.is_none());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LogParser;

    fn filled(lines: usize, max: usize, block: usize) -> LogBuffer {
        let mut buffer = LogBuffer::with_limits(max, block);
        let mut parser = LogParser::new();
        let text: String = (0..lines).map(|n| format!("line {n}\n")).collect();
        buffer.append(&mut parser, text.as_bytes());
        buffer
    }

    #[test]
    fn lines_are_found_by_id_across_blocks() {
        let buffer = filled(25, 1000, 10);
        assert_eq!(buffer.len(), 25);
        for id in [0, 9, 10, 24] {
            assert_eq!(
                buffer.line(id).map(|l| l.text.to_owned()),
                Some(format!("line {id}"))
            );
        }
        assert!(buffer.line(25).is_none());
    }

    #[test]
    fn the_oldest_blocks_are_dropped_and_ids_stay_put() {
        let buffer = filled(100, 35, 10);
        // 100 lines in blocks of 10 with a limit of 35: whole blocks go until
        // 35 or fewer are held.
        assert_eq!(buffer.len(), 30);
        assert_eq!(buffer.dropped(), 70);
        assert_eq!(buffer.total(), 100);
        assert!(buffer.line(69).is_none());
        assert_eq!(buffer.line(70).map(|l| l.text), Some("line 70"));
        assert_eq!(buffer.line(99).map(|l| l.text), Some("line 99"));
    }

    #[test]
    fn appending_after_a_drop_keeps_counting() {
        let mut buffer = LogBuffer::with_limits(20, 10);
        let mut parser = LogParser::new();
        let text: String = (0..40).map(|n| format!("line {n}\n")).collect();
        buffer.append(&mut parser, text.as_bytes());
        assert!(buffer.dropped() > 0);
        let range = buffer.append(&mut parser, b"more\n");
        assert_eq!(range, 40..41);
        assert_eq!(buffer.line(40).map(|l| l.text), Some("more"));
    }

    #[test]
    fn a_stream_of_long_lines_is_capped_by_bytes_before_lines() {
        let mut buffer = LogBuffer::with_limits(1_000_000, 10).with_max_bytes(100_000);
        let mut parser = LogParser::new();
        let line = format!("{}\n", "x".repeat(999));
        for _ in 0..20 {
            buffer.append(&mut parser, line.repeat(50).as_bytes());
        }
        assert_eq!(buffer.total(), 1000);
        assert!(buffer.bytes() <= 100_000, "{}", buffer.bytes());
        assert!(buffer.len() < 200, "{}", buffer.len());
        assert!(buffer.dropped() > 0);
        assert!(buffer.line(999).is_some());
        assert!(buffer.line(0).is_none());
    }

    #[test]
    fn the_byte_count_follows_what_is_held() {
        let buffer = filled(100, 35, 10);
        let text: usize = buffer.lines(0..100).map(|l| l.text.len()).sum();
        assert_eq!(buffer.bytes(), text + buffer.len() * size_of::<LineMeta>());
    }

    #[test]
    fn groups_of_dropped_headers_are_forgotten() {
        let mut buffer = LogBuffer::with_limits(10, 5);
        let mut parser = LogParser::new();
        let mut text = String::new();
        for n in 0..20 {
            text += &format!("##[group]g{n}\nbody\n##[endgroup]\n");
        }
        buffer.append(&mut parser, text.as_bytes());
        assert!(buffer.dropped() > 0);
        assert!(buffer.groups().iter().all(|g| g.start >= buffer.dropped()));
        assert!(!buffer.groups().is_empty());
    }
}
