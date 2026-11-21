//! Find in the diff: the shown lines and file paths that hold a query, read
//! from the diff's data rather than its painted rows, so lines out of view and
//! the files folded to their header are searched too.

use std::ops::Range;

use diff::{DiffList, FileIx, Row, Side, SideLine, Text};

/// Matches a search keeps at most; past it, it stops and says so.
pub const MAX_MATCHES: usize = 10_000;

/// Where a match is in its file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FindAt {
    /// The file's path, on its header.
    Path,
    /// A code line (0-based) of one side.
    Line(Side, u32),
}

/// One stretch of text that matches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FindMatch {
    pub file: FileIx,
    pub at: FindAt,
    /// The chars of the path or line it covers.
    pub chars: Range<u32>,
}

/// What a search found, in the order the diff shows it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Found {
    pub matches: Vec<FindMatch>,
    /// It stopped at [`MAX_MATCHES`]: there are more.
    pub capped: bool,
}

/// A query, as typed or ignoring case.
#[derive(Clone, Debug)]
pub struct Needle {
    text: String,
    match_case: bool,
    /// Ignoring case: the query's chars, lowercased.
    folded: Vec<char>,
}

impl Needle {
    #[must_use]
    pub fn new(text: &str, match_case: bool) -> Self {
        Self {
            text: text.to_owned(),
            match_case,
            folded: text.chars().map(fold).collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Calls `each` with the byte range of every match in `hay`, left to
    /// right, none overlapping the one before; stops when `each` says so.
    pub fn find_in(&self, hay: &str, mut each: impl FnMut(Range<usize>) -> bool) {
        if self.is_empty() {
            return;
        }
        if self.match_case {
            for (start, found) in hay.match_indices(self.text.as_str()) {
                if !each(start..start + found.len()) {
                    return;
                }
            }
        } else if self.text.is_ascii() {
            // ASCII bytes never occur inside a multi-byte char, so every
            // match starts and ends on a char boundary.
            let (hay, needle) = (hay.as_bytes(), self.text.as_bytes());
            let first = needle[0].to_ascii_lowercase();
            let mut at = 0;
            while at + needle.len() <= hay.len() {
                if hay[at].to_ascii_lowercase() == first
                    && hay[at..at + needle.len()].eq_ignore_ascii_case(needle)
                {
                    if !each(at..at + needle.len()) {
                        return;
                    }
                    at += needle.len();
                } else {
                    at += 1;
                }
            }
        } else {
            let chars: Vec<(usize, char)> = hay.char_indices().map(|(i, c)| (i, fold(c))).collect();
            let len = self.folded.len();
            let mut at = 0;
            while at + len <= chars.len() {
                if chars[at..at + len]
                    .iter()
                    .map(|&(_, c)| c)
                    .eq(self.folded.iter().copied())
                {
                    let end = chars.get(at + len).map_or(hay.len(), |&(i, _)| i);
                    if !each(chars[at].0..end) {
                        return;
                    }
                    at += len;
                } else {
                    at += 1;
                }
            }
        }
    }
}

/// One char lowercased, as one char: case-insensitive matching keeps the
/// text's char positions.
fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// One file as a search reads it.
struct CorpusFile {
    file: FileIx,
    path: String,
    old: Text,
    new: Text,
    /// The lines its rows show, in their order.
    lines: Vec<(Side, u32)>,
}

/// What a search reads, taken from a list on the UI thread: each shown file's
/// path, its texts (shared, not copied) and the lines its rows show, in
/// order. It can be sent to a background thread to search there.
pub struct Corpus {
    files: Vec<CorpusFile>,
}

impl Corpus {
    /// The files `list` shows and their lines in its layout: a unified row
    /// shows its new line (the old one for a removed line), a split row
    /// both. A collapsed file has the lines it shows when open.
    #[must_use]
    pub fn of(list: &DiffList) -> Self {
        let rows = list.rows();
        let headers = list.file_rows();
        let files = headers
            .iter()
            .enumerate()
            .filter_map(|(k, &header)| {
                let header = header as usize;
                let Row::FileHeader { file } = rows[header] else {
                    return None;
                };
                let open;
                let body: &[Row] = if list.is_collapsed(file) {
                    open = list.open_rows(file);
                    &open
                } else {
                    let end = headers.get(k + 1).map_or(rows.len(), |&row| row as usize);
                    &rows[header..end]
                };
                let f = list.diff().file(file);
                Some(CorpusFile {
                    file,
                    path: f.path.clone(),
                    old: f.old.clone(),
                    new: f.new.clone(),
                    lines: shown_lines(body),
                })
            })
            .collect();
        Self { files }
    }

    /// Every match of `needle`, path first in each file, then its lines.
    #[must_use]
    pub fn search(&self, needle: &Needle) -> Found {
        let mut found = Found::default();
        if needle.is_empty() {
            return found;
        }
        'files: for f in &self.files {
            if !add(&mut found, f.file, FindAt::Path, &f.path, needle) {
                break;
            }
            for &(side, line) in &f.lines {
                let text = match side {
                    Side::Old => &f.old,
                    Side::New => &f.new,
                };
                if line >= text.line_count() {
                    continue;
                }
                let at = FindAt::Line(side, line);
                if !add(&mut found, f.file, at, &text.line(line), needle) {
                    break 'files;
                }
            }
        }
        found
    }
}

/// The code lines of `rows`, in order.
fn shown_lines(rows: &[Row]) -> Vec<(Side, u32)> {
    let mut lines = Vec::new();
    for row in rows {
        match *row {
            Row::Line {
                new: Some(line), ..
            } => lines.push((Side::New, line)),
            Row::Line {
                old: Some(line),
                new: None,
                ..
            } => lines.push((Side::Old, line)),
            Row::Pair { left, right, .. } => {
                if let Some(SideLine { line, .. }) = left {
                    lines.push((Side::Old, line));
                }
                if let Some(SideLine { line, .. }) = right {
                    lines.push((Side::New, line));
                }
            }
            _ => {}
        }
    }
    lines
}

/// Adds the matches in `hay`; false once [`MAX_MATCHES`] is reached.
fn add(found: &mut Found, file: FileIx, at: FindAt, hay: &str, needle: &Needle) -> bool {
    let ascii = hay.is_ascii();
    let char_at = |byte: usize| {
        if ascii {
            byte as u32
        } else {
            hay[..byte].chars().count() as u32
        }
    };
    needle.find_in(hay, |bytes| {
        if found.matches.len() == MAX_MATCHES {
            found.capped = true;
            return false;
        }
        found.matches.push(FindMatch {
            file,
            at,
            chars: char_at(bytes.start)..char_at(bytes.end),
        });
        true
    });
    !found.capped
}

#[cfg(test)]
mod tests {
    use super::*;
    use diff::{Diff, FileDiff, Hunk, Layout, RowHeights, Segment};

    fn ranges(needle: &str, match_case: bool, hay: &str) -> Vec<Range<usize>> {
        let mut out = Vec::new();
        Needle::new(needle, match_case).find_in(hay, |range| {
            out.push(range);
            true
        });
        out
    }

    // The match lists here are lists of ranges, one long or not.
    #[allow(clippy::single_range_in_vec_init)]
    #[test]
    fn needles_match_ignoring_case_unless_asked_and_never_overlap() {
        assert_eq!(ranges("ab", false, "xAbab aB"), [1..3, 3..5, 6..8]);
        assert_eq!(ranges("ab", true, "xAbab aB"), [3..5]);
        assert_eq!(ranges("aa", false, "aaa"), [0..2]);
        assert_eq!(ranges("", false, "abc"), Vec::<Range<usize>>::new());
        // Byte ranges around wide chars, either kind of needle.
        assert_eq!(ranges("b", false, "éB"), [2..3]);
        assert_eq!(ranges("É", false, "aé"), [1..3]);
        assert_eq!(ranges("ÉB", false, "xéb"), [1..4]);
    }

    fn list(layout: Layout) -> DiffList {
        let hunk = Hunk {
            old_start: 0,
            new_start: 0,
            segments: vec![
                Segment::Context(1),
                Segment::Change {
                    removed: 1,
                    added: 1,
                },
            ],
        };
        let diff = Diff {
            files: vec![FileDiff {
                path: "src/find.rs".into(),
                old: Text::new("let find = 1;\nold find\n"),
                new: Text::new("let find = 1;\nnew Find é find\n"),
                hunks: vec![hunk],
            }],
            blocks: Vec::new(),
        };
        let heights = RowHeights {
            code: 20.0,
            file_header: 30.0,
        };
        DiffList::new(diff, layout, heights)
    }

    fn places(found: &Found) -> Vec<(FindAt, Range<u32>)> {
        found
            .matches
            .iter()
            .map(|m| (m.at, m.chars.clone()))
            .collect()
    }

    #[test]
    fn the_path_then_the_shown_lines_in_order_by_layout() {
        let needle = Needle::new("find", false);
        let unified = Corpus::of(&list(Layout::Unified)).search(&needle);
        assert_eq!(
            places(&unified),
            [
                (FindAt::Path, 4..8),
                (FindAt::Line(Side::New, 0), 4..8),
                (FindAt::Line(Side::Old, 1), 4..8),
                (FindAt::Line(Side::New, 1), 4..8),
                // Chars, not bytes, past the "é".
                (FindAt::Line(Side::New, 1), 11..15),
            ]
        );
        assert!(!unified.capped);
        // Split shows the context line on both sides.
        let split = Corpus::of(&list(Layout::Split)).search(&needle);
        assert_eq!(split.matches.len(), unified.matches.len() + 1);
        assert_eq!(split.matches[1].at, FindAt::Line(Side::Old, 0));
        assert_eq!(split.matches[2].at, FindAt::Line(Side::New, 0));
    }

    #[test]
    fn collapsed_files_are_searched_as_if_open() {
        let needle = Needle::new("find", true);
        let mut list = list(Layout::Unified);
        let open = Corpus::of(&list).search(&needle);
        list.set_collapsed(vec![true]);
        assert_eq!(Corpus::of(&list).search(&needle), open);
        assert_eq!(open.matches.len(), 4, "\"Find\" is not \"find\"");
    }
}
