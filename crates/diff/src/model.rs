use crate::Text;

/// Index of a file in [`Diff::files`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileIx(pub u32);

/// Index of a block in [`Diff::blocks`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Side {
    Old,
    New,
}

/// A run of lines inside a hunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Segment {
    /// Lines present in both blobs.
    Context(u32),
    /// `removed` old lines replaced by `added` new lines (either may be 0).
    Change { removed: u32, added: u32 },
}

impl Segment {
    pub fn old_len(self) -> u32 {
        match self {
            Self::Context(n) => n,
            Self::Change { removed, .. } => removed,
        }
    }

    pub fn new_len(self) -> u32 {
        match self {
            Self::Context(n) => n,
            Self::Change { added, .. } => added,
        }
    }
}

/// A hunk as line ranges: it starts at `old_start` / `new_start` (0-based)
/// and walks both blobs through its segments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: u32,
    pub new_start: u32,
    pub segments: Vec<Segment>,
}

impl Hunk {
    pub fn old_len(&self) -> u32 {
        self.segments.iter().map(|s| s.old_len()).sum()
    }

    pub fn new_len(&self) -> u32 {
        self.segments.iter().map(|s| s.new_len()).sum()
    }

    /// Whether `line` of `side` is one of this hunk's rows.
    pub fn shows(&self, side: Side, line: u32) -> bool {
        let (start, len) = match side {
            Side::Old => (self.old_start, self.old_len()),
            Side::New => (self.new_start, self.new_len()),
        };
        (start..start + len).contains(&line)
    }
}

#[derive(Clone, Debug, Default)]
pub struct FileDiff {
    pub path: String,
    pub old: Text,
    pub new: Text,
    pub hunks: Vec<Hunk>,
}

impl FileDiff {
    /// Whether some hunk shows `line` (0-based) of `side`.
    pub fn shows(&self, side: Side, line: u32) -> bool {
        self.hunks.iter().any(|hunk| hunk.shows(side, line))
    }
}

/// Where a block sits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Anchor {
    /// After the row that shows `line` of `side` in `file`; at the end of the
    /// file when no hunk shows that line.
    Line { file: FileIx, side: Side, line: u32 },
    /// Under the file's header, above its first hunk.
    FileTop { file: FileIx },
}

impl Anchor {
    pub fn file(self) -> FileIx {
        match self {
            Self::Line { file, .. } | Self::FileTop { file } => file,
        }
    }

    /// The side a block sits on in split view; file-top blocks span both.
    pub fn side(self) -> Option<Side> {
        match self {
            Self::Line { side, .. } => Some(side),
            Self::FileTop { .. } => None,
        }
    }
}

/// A caller-sized row between code rows (a comment thread, a composer).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Block {
    pub anchor: Anchor,
    pub height: f32,
    /// The first line of a multi-line range on the anchor's side; the anchor
    /// line is the last. `None` for a single line.
    pub first_line: Option<u32>,
}

impl Block {
    #[must_use]
    pub fn new(anchor: Anchor, height: f32) -> Self {
        Self {
            anchor,
            height,
            first_line: None,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Diff {
    pub files: Vec<FileDiff>,
    pub blocks: Vec<Block>,
}

impl Diff {
    pub fn file(&self, file: FileIx) -> &FileDiff {
        &self.files[file.0 as usize]
    }

    pub fn block(&self, id: BlockId) -> &Block {
        &self.blocks[id.0 as usize]
    }

    /// The source text of `line` (0-based) of `side` in `file`, without its
    /// line ending or any diff marker; `None` when the file or line is not
    /// there.
    #[must_use]
    pub fn line_text(&self, file: FileIx, side: Side, line: u32) -> Option<String> {
        let file = self.files.get(file.0 as usize)?;
        let text = match side {
            Side::Old => &file.old,
            Side::New => &file.new,
        };
        (line < text.line_count()).then(|| text.line(line).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_text_is_the_bare_source_line_of_either_side() {
        let diff = Diff {
            files: vec![FileDiff {
                path: "a.rs".into(),
                old: Text::new("one\r\n\ttwo\n"),
                new: Text::new("one\nTWO\n"),
                hunks: Vec::new(),
            }],
            blocks: Vec::new(),
        };
        let f = FileIx(0);
        assert_eq!(diff.line_text(f, Side::Old, 0).as_deref(), Some("one"));
        assert_eq!(diff.line_text(f, Side::Old, 1).as_deref(), Some("\ttwo"));
        assert_eq!(diff.line_text(f, Side::New, 1).as_deref(), Some("TWO"));
        assert_eq!(diff.line_text(f, Side::New, 9), None);
        assert_eq!(diff.line_text(FileIx(1), Side::New, 0), None);
    }
}
