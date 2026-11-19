//! A deterministic synthetic diff for benchmarks and demos.

use std::fmt::Write as _;

use crate::Text;
use crate::model::{Anchor, Block, Diff, FileDiff, FileIx, Hunk, Segment, Side};
use crate::rows::Layout;

#[derive(Clone, Copy, Debug)]
pub struct SynthConfig {
    pub seed: u64,
    /// Stop after this many files...
    pub max_files: usize,
    /// ...or once the projection in this layout has at least this many rows.
    pub target_rows: Option<(Layout, usize)>,
    /// On average one block per this many code rows; 0 for none.
    pub block_every: u32,
}

impl SynthConfig {
    /// A diff of at least `rows` rows in `layout`.
    #[must_use]
    pub fn rows(seed: u64, layout: Layout, rows: usize) -> Self {
        Self {
            seed,
            max_files: usize::MAX,
            target_rows: Some((layout, rows)),
            block_every: 400,
        }
    }
}

/// SplitMix64: tiny, fast and stable across versions, so a seed always
/// produces the same diff.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in `lo..=hi`.
    fn range(&mut self, lo: u32, hi: u32) -> u32 {
        lo + (self.next() % u64::from(hi - lo + 1)) as u32
    }

    /// True with probability `percent` / 100.
    fn chance(&mut self, percent: u32) -> bool {
        self.range(1, 100) <= percent
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.next() as usize % items.len()]
    }
}

const KEYWORDS: &[&str] = &[
    "fn", "let", "mut", "pub", "impl", "struct", "enum", "match", "if", "else", "for", "while",
    "return", "use", "const", "static", "self", "Self", "where", "async", "await", "move", "ref",
];
const IDENTS: &[&str] = &[
    "buffer",
    "offset",
    "row_count",
    "layout",
    "window",
    "cx",
    "state",
    "item",
    "index",
    "range",
    "summary",
    "cursor",
    "height",
    "width",
    "bounds",
    "origin",
    "style",
    "text_system",
    "font_id",
    "line",
    "hunk",
    "path",
    "repo",
    "commit",
    "author",
    "review",
    "thread",
    "comment",
    "config",
    "HashMap",
    "Vec",
    "Option",
    "Result",
    "String",
    "Arc",
    "Rc",
    "RefCell",
    "Pixels",
    "Point",
];
const PUNCT: &[&str] = &[
    "(", ")", "{", "}", ",", ";", ".", "::", " = ", " => ", " + ", "&", "->", "<", ">", "?",
];
const DIRS: &[&str] = &[
    "src", "crates", "core", "ui", "net", "store", "util", "tests", "bench", "ext",
];
const EXTS: &[&str] = &["rs", "ts", "py", "go", "c", "toml", "md"];

/// Generates the diff. Equal configs give equal diffs.
#[must_use]
pub fn synthesize(config: &SynthConfig) -> Diff {
    let mut rng = Rng(config.seed);
    let mut diff = Diff::default();
    let mut rows = 0usize;
    while diff.files.len() < config.max_files {
        if let Some((_, target)) = config.target_rows
            && rows >= target
        {
            break;
        }
        let file = FileIx(diff.files.len() as u32);
        let mut file_gen = FileGen::new(&mut rng, config.block_every);
        let (file_diff, file_rows) = file_gen.file(file, &mut diff.blocks);
        rows += match config.target_rows {
            Some((Layout::Split, _)) => file_rows.split,
            _ => file_rows.unified,
        };
        diff.files.push(file_diff);
    }
    diff
}

#[derive(Default)]
struct RowCounts {
    unified: usize,
    split: usize,
}

struct FileGen<'r> {
    rng: &'r mut Rng,
    block_every: u32,
    old: String,
    new: String,
    old_lines: u32,
    new_lines: u32,
    indent_with_tabs: bool,
    depth: u32,
    counts: RowCounts,
}

impl<'r> FileGen<'r> {
    fn new(rng: &'r mut Rng, block_every: u32) -> Self {
        let indent_with_tabs = rng.chance(30);
        Self {
            rng,
            block_every,
            old: String::new(),
            new: String::new(),
            old_lines: 0,
            new_lines: 0,
            indent_with_tabs,
            depth: 0,
            counts: RowCounts::default(),
        }
    }

    fn file(&mut self, file: FileIx, blocks: &mut Vec<Block>) -> (FileDiff, RowCounts) {
        let path = self.path();
        // A few very long files with many hunks, most with a handful.
        let hunk_count = if self.rng.chance(10) {
            self.rng.range(15, 60)
        } else {
            self.rng.range(1, 8)
        };
        self.counts = RowCounts {
            unified: 1,
            split: 1,
        };
        let mut hunks = Vec::with_capacity(hunk_count as usize);
        for _ in 0..hunk_count {
            let gap = self.rng.range(4, 250);
            self.shared(gap);
            hunks.push(self.hunk(file, blocks));
        }
        let tail = self.rng.range(0, 80);
        self.shared(tail);
        let file_diff = FileDiff {
            path,
            old: Text::new(std::mem::take(&mut self.old)),
            new: Text::new(std::mem::take(&mut self.new)),
            hunks,
        };
        (file_diff, std::mem::take(&mut self.counts))
    }

    fn path(&mut self) -> String {
        let mut path = String::new();
        for _ in 0..self.rng.range(1, 4) {
            path.push_str(self.rng.pick(DIRS));
            path.push('/');
        }
        let stem = self.rng.pick(IDENTS).to_lowercase();
        let ext = self.rng.pick(EXTS);
        let _ = write!(path, "{stem}_{}.{ext}", self.rng.range(0, 999));
        path
    }

    fn hunk(&mut self, file: FileIx, blocks: &mut Vec<Block>) -> Hunk {
        let (old_start, new_start) = (self.old_lines, self.new_lines);
        let mut segments = Vec::new();
        let lead = self.rng.range(0, 3);
        segments.push(Segment::Context(lead));
        for ix in 0..self.rng.range(1, 4) {
            if ix > 0 {
                segments.push(Segment::Context(self.rng.range(1, 6)));
            }
            let big = self.rng.chance(5);
            let removed = if big {
                self.rng.range(0, 150)
            } else {
                self.rng.range(0, 8)
            };
            let added = if big {
                self.rng.range(0, 200)
            } else {
                self.rng.range(u32::from(removed == 0), 12)
            };
            segments.push(Segment::Change { removed, added });
        }
        segments.push(Segment::Context(3));
        segments.retain(|s| s.old_len() + s.new_len() > 0);

        self.counts.unified += 1;
        self.counts.split += 1;
        let (mut old, mut new) = (old_start, new_start);
        for &segment in &segments {
            match segment {
                Segment::Context(n) => {
                    for k in 0..n {
                        self.maybe_block(file, Side::New, new + k, blocks);
                    }
                    self.shared(n);
                    self.count(n as usize, n as usize);
                }
                Segment::Change { removed, added } => {
                    for k in 0..removed {
                        self.maybe_block(file, Side::Old, old + k, blocks);
                        let line = self.line();
                        self.push_old(&line);
                    }
                    for k in 0..added {
                        self.maybe_block(file, Side::New, new + k, blocks);
                        let line = self.line();
                        self.push_new(&line);
                    }
                    self.count((removed + added) as usize, removed.max(added) as usize);
                }
            }
            old += segment.old_len();
            new += segment.new_len();
        }
        Hunk {
            old_start,
            new_start,
            segments,
        }
    }

    fn count(&mut self, unified: usize, split: usize) {
        self.counts.unified += unified;
        self.counts.split += split;
    }

    fn maybe_block(&mut self, file: FileIx, side: Side, line: u32, blocks: &mut Vec<Block>) {
        if self.block_every == 0 || self.rng.range(1, self.block_every) != 1 {
            return;
        }
        let height = self.rng.range(60, 260) as f32;
        blocks.push(Block::new(Anchor::Line { file, side, line }, height));
        self.count(1, 1);
    }

    /// Unchanged lines, appended to both blobs.
    fn shared(&mut self, n: u32) {
        for _ in 0..n {
            let line = self.line();
            self.push_old(&line);
            self.push_new(&line);
        }
    }

    fn push_old(&mut self, line: &str) {
        self.old.push_str(line);
        self.old.push('\n');
        self.old_lines += 1;
    }

    fn push_new(&mut self, line: &str) {
        self.new.push_str(line);
        self.new.push('\n');
        self.new_lines += 1;
    }

    /// One code-like line: indentation, then tokens up to a target length
    /// that is mostly 20-80 columns, sometimes long.
    fn line(&mut self) -> String {
        let rng = &mut *self.rng;
        if rng.chance(8) {
            return String::new();
        }
        self.depth = match rng.range(0, 5) {
            0 => self.depth.saturating_sub(1),
            1 => (self.depth + 1).min(6),
            _ => self.depth,
        };
        let target = match rng.range(1, 100) {
            1..=70 => rng.range(20, 80),
            71..=94 => rng.range(80, 140),
            _ => rng.range(150, 400),
        } as usize;
        let mut line = String::with_capacity(target + 16);
        for _ in 0..self.depth {
            line.push_str(if self.indent_with_tabs { "\t" } else { "    " });
        }
        if rng.chance(10) {
            line.push_str("// ");
        }
        while line.len() < target {
            match rng.range(0, 9) {
                0 | 1 => line.push_str(rng.pick(KEYWORDS)),
                2..=5 => line.push_str(rng.pick(IDENTS)),
                6 => {
                    let _ = write!(line, "\"{}\"", rng.pick(IDENTS));
                }
                7 => {
                    let _ = write!(line, "{}", rng.range(0, 4096));
                }
                _ => line.push_str(rng.pick(PUNCT)),
            }
            line.push(' ');
        }
        line.truncate(line.trim_end().len());
        line
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rows::{DiffList, Layout, RowHeights};

    const HEIGHTS: RowHeights = RowHeights {
        code: 20.0,
        file_header: 32.0,
    };

    #[test]
    fn same_seed_same_diff() {
        let config = SynthConfig {
            seed: 7,
            max_files: 20,
            target_rows: None,
            block_every: 50,
        };
        let (a, b) = (synthesize(&config), synthesize(&config));
        assert_eq!(a.files.len(), 20);
        for (fa, fb) in a.files.iter().zip(&b.files) {
            assert_eq!(fa.path, fb.path);
            assert_eq!(fa.new.as_bytes(), fb.new.as_bytes());
            assert_eq!(fa.hunks, fb.hunks);
        }
        assert_eq!(a.blocks, b.blocks);
        let other = synthesize(&SynthConfig { seed: 8, ..config });
        assert_ne!(a.files[0].new.as_bytes(), other.files[0].new.as_bytes());
    }

    #[test]
    fn hunks_fit_their_blobs_and_context_matches() {
        let diff = synthesize(&SynthConfig {
            seed: 3,
            max_files: 30,
            target_rows: None,
            block_every: 0,
        });
        for file in &diff.files {
            for hunk in &file.hunks {
                assert!(hunk.old_start + hunk.old_len() <= file.old.line_count());
                assert!(hunk.new_start + hunk.new_len() <= file.new.line_count());
                let (mut old, mut new) = (hunk.old_start, hunk.new_start);
                for &segment in &hunk.segments {
                    if let Segment::Context(n) = segment {
                        for k in 0..n {
                            assert_eq!(file.old.line(old + k), file.new.line(new + k));
                        }
                    }
                    old += segment.old_len();
                    new += segment.new_len();
                }
            }
        }
    }

    #[test]
    fn row_target_is_met_in_its_layout() {
        for layout in [Layout::Unified, Layout::Split] {
            let config = SynthConfig::rows(1, layout, 20_000);
            let list = DiffList::new(synthesize(&config), layout, HEIGHTS);
            assert!(list.len() >= 20_000, "{layout:?}: {}", list.len());
            assert!(list.len() < 30_000, "{layout:?}: {}", list.len());
        }
    }

    #[test]
    fn lines_have_tabs_and_long_ones() {
        let diff = synthesize(&SynthConfig {
            seed: 11,
            max_files: 40,
            target_rows: None,
            block_every: 0,
        });
        let lines = diff
            .files
            .iter()
            .flat_map(|f| (0..f.new.line_count()).map(|ix| f.new.line(ix)));
        let (mut tabs, mut long) = (0, 0);
        for line in lines {
            tabs += usize::from(line.starts_with('\t'));
            long += usize::from(line.len() > 150);
        }
        assert!(tabs > 0 && long > 0, "tabs {tabs}, long {long}");
    }
}
