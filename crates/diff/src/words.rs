//! Word-level changes inside a replaced line.
//!
//! A removed line and the added line that replaces it are paired by position
//! inside their change ([`Pairs`]); [`word_diff`] then finds which words of
//! each changed, so a view can emphasize them.

use std::ops::Range;

use imara::sources::words;
use imara::{Algorithm, Diff, InternedInput};

use crate::model::{FileDiff, Segment, Side};

/// Longer lines (in bytes, either side) get no word diff: it is a visual
/// nicety and must stay cheap.
pub const MAX_LINE_BYTES: usize = 4096;

/// A pair that shares less than this fraction of its words is a different
/// line, not an edited one, and gets no emphasis.
pub const MIN_SHARED: f64 = 0.3;

/// The changed byte ranges of a paired line, sorted and disjoint.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WordDiff {
    /// In the removed line.
    pub old: Vec<Range<u32>>,
    /// In the added line.
    pub new: Vec<Range<u32>>,
}

impl WordDiff {
    #[must_use]
    pub fn side(&self, side: Side) -> &[Range<u32>] {
        match side {
            Side::Old => &self.old,
            Side::New => &self.new,
        }
    }
}

/// The words of `old` and `new` that differ, or `None` when the pair is too
/// different ([`MIN_SHARED`]), a line is longer than [`MAX_LINE_BYTES`], or
/// nothing differs.
///
/// Words are runs of letters, digits and `_`, runs of spaces, and single
/// other characters. Changed words separated only by blanks form one range.
#[must_use]
pub fn word_diff(old: &str, new: &str) -> Option<WordDiff> {
    if old.len() > MAX_LINE_BYTES || new.len() > MAX_LINE_BYTES || old == new {
        return None;
    }
    let old_words: Vec<&str> = words(old).collect();
    let new_words: Vec<&str> = words(new).collect();
    let mut input = InternedInput::default();
    input.update_before(old_words.iter().copied());
    input.update_after(new_words.iter().copied());
    let mut diff = Diff::compute(Algorithm::Myers, &input);
    diff.postprocess_no_heuristic(&input);

    // Runs of spaces do not count toward similarity.
    let is_word = |w: &str| !w.starts_with(' ');
    let kept = old_words
        .iter()
        .enumerate()
        .filter(|&(ix, w)| is_word(w) && !diff.is_removed(ix as u32))
        .count();
    let total = old_words
        .iter()
        .chain(&new_words)
        .filter(|w| is_word(w))
        .count();
    if total > 0 && (2 * kept) as f64 / (total as f64) < MIN_SHARED {
        return None;
    }
    let old_ranges = ranges(old, &old_words, |ix| diff.is_removed(ix));
    let new_ranges = ranges(new, &new_words, |ix| diff.is_added(ix));
    (!old_ranges.is_empty() || !new_ranges.is_empty()).then_some(WordDiff {
        old: old_ranges,
        new: new_ranges,
    })
}

/// Byte ranges of the changed words, merging ranges that only blanks separate.
fn ranges(line: &str, words: &[&str], changed: impl Fn(u32) -> bool) -> Vec<Range<u32>> {
    let mut out: Vec<Range<u32>> = Vec::new();
    let mut at = 0u32;
    for (ix, word) in words.iter().enumerate() {
        let (start, end) = (at, at + word.len() as u32);
        at = end;
        if !changed(ix as u32) {
            continue;
        }
        match out.last_mut() {
            Some(last)
                if line[last.end as usize..start as usize]
                    .bytes()
                    .all(|b| b == b' ' || b == b'\t') =>
            {
                last.end = end;
            }
            _ => out.push(start..end),
        }
    }
    out
}

/// Where a file's removed lines meet the added lines that replace them.
///
/// Inside one change (a run of removed lines followed by a run of added
/// lines) the n-th removed line pairs with the n-th added line, as git and
/// GitHub show it; lines beyond the shorter run have no partner.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Pairs {
    /// Changes with lines on both sides, ascending on both sides.
    runs: Vec<Run>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Run {
    old: u32,
    new: u32,
    removed: u32,
    added: u32,
}

impl Pairs {
    /// Indexes the changes of `file`.
    #[must_use]
    pub fn of(file: &FileDiff) -> Self {
        let mut runs = Vec::new();
        for hunk in &file.hunks {
            let (mut old, mut new) = (hunk.old_start, hunk.new_start);
            for &segment in &hunk.segments {
                if let Segment::Change { removed, added } = segment
                    && removed > 0
                    && added > 0
                {
                    runs.push(Run {
                        old,
                        new,
                        removed,
                        added,
                    });
                }
                old += segment.old_len();
                new += segment.new_len();
            }
        }
        Self { runs }
    }

    /// The line on the other side that replaces (or is replaced by) `line`
    /// of `side`, if it has a partner.
    #[must_use]
    pub fn partner(&self, side: Side, line: u32) -> Option<u32> {
        let start = |run: &Run| match side {
            Side::Old => run.old,
            Side::New => run.new,
        };
        let ix = self.runs.partition_point(|run| start(run) <= line);
        let run = self.runs[..ix].last()?;
        let k = line - start(run);
        if k >= run.removed.min(run.added) {
            return None;
        }
        Some(match side {
            Side::Old => run.new + k,
            Side::New => run.old + k,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Hunk, Text};

    /// The changed text of each side.
    fn changed(old: &str, new: &str) -> (Vec<String>, Vec<String>) {
        let diff = word_diff(old, new).expect("a word diff");
        let cut = |line: &str, ranges: &[Range<u32>]| {
            ranges
                .iter()
                .map(|r| line[r.start as usize..r.end as usize].to_owned())
                .collect()
        };
        (cut(old, &diff.old), cut(new, &diff.new))
    }

    #[test]
    fn one_changed_word() {
        let (old, new) = changed("let total = count + 1;", "let total = count + step;");
        assert_eq!(old, ["1"]);
        assert_eq!(new, ["step"]);
    }

    #[test]
    fn pure_insertion_marks_only_the_new_side() {
        let (old, new) = changed("call(a, b)", "call(a, b, c)");
        assert!(old.is_empty());
        assert_eq!(new.concat(), ", c");
    }

    #[test]
    fn changed_words_split_by_blanks_merge() {
        let (old, new) = changed("a old words here", "a new text here");
        assert_eq!(old, ["old words"]);
        assert_eq!(new, ["new text"]);
    }

    #[test]
    fn punctuation_is_its_own_word() {
        let (old, new) = changed("foo.bar(x)", "foo::bar(x)");
        assert_eq!(old, ["."]);
        assert_eq!(new, ["::"]);
    }

    #[test]
    fn ranges_are_sorted_disjoint_char_boundaries() {
        let old = "héllo wörld, α β γ";
        let new = "héllo wörld! α δ γ";
        let diff = word_diff(old, new).expect("a word diff");
        for (line, ranges) in [(old, &diff.old), (new, &diff.new)] {
            let mut end = 0;
            for r in ranges {
                assert!(r.start >= end && r.start < r.end);
                assert!(line.is_char_boundary(r.start as usize));
                assert!(line.is_char_boundary(r.end as usize));
                end = r.end;
            }
        }
    }

    #[test]
    fn unrelated_lines_get_none() {
        assert_eq!(
            word_diff("fn main() {}", "// completely different text"),
            None
        );
        assert_eq!(word_diff("", "something"), None);
        assert_eq!(word_diff("same", "same"), None);
    }

    #[test]
    fn the_shared_threshold_is_30_percent() {
        // 3 of 4 words shared: an edit.
        assert!(word_diff("a b c d", "a b c e").is_some());
        // 1 of 4 shared (25%): a different line.
        assert_eq!(word_diff("a b c d", "a x y z"), None);
        // 1 of 3 shared (33%): still an edit.
        assert!(word_diff("a b c", "a x y").is_some());
    }

    #[test]
    fn long_lines_are_skipped() {
        let long = format!("{} x", "word ".repeat(MAX_LINE_BYTES / 5 + 1));
        assert!(long.len() > MAX_LINE_BYTES);
        assert_eq!(word_diff(&long, &long.replace('x', "y")), None);
        let ok = "word ".repeat(100);
        assert!(word_diff(&ok, &format!("{ok}y")).is_some());
    }

    #[test]
    fn indentation_change_marks_the_blanks() {
        let (old, new) = changed("    let a = 1;", "        let a = 1;");
        assert_eq!(old.concat().len() + 4, new.concat().len());
    }

    fn file(segments: Vec<Segment>, old_start: u32, new_start: u32) -> FileDiff {
        FileDiff {
            path: "a".into(),
            old: Text::new(""),
            new: Text::new(""),
            hunks: vec![Hunk {
                old_start,
                new_start,
                segments,
            }],
        }
    }

    #[test]
    fn lines_pair_in_order_inside_a_change() {
        // Old lines start at 5, new lines at 9: two context lines, then 3
        // removed / 2 added, one context line, a pure removal, a pure addition.
        let f = file(
            vec![
                Segment::Context(2),
                Segment::Change {
                    removed: 3,
                    added: 2,
                },
                Segment::Context(1),
                Segment::Change {
                    removed: 2,
                    added: 0,
                },
                Segment::Change {
                    removed: 0,
                    added: 4,
                },
            ],
            5,
            9,
        );
        let pairs = Pairs::of(&f);
        let old = |l| pairs.partner(Side::Old, l);
        let new = |l| pairs.partner(Side::New, l);
        // Removed lines 7, 8, 9 meet added lines 11, 12.
        assert_eq!((old(7), old(8), old(9)), (Some(11), Some(12), None));
        assert_eq!((new(11), new(12), new(13)), (Some(7), Some(8), None));
        // Context lines, the pure removal and the pure addition have none.
        for line in [5, 6, 10, 11, 12] {
            assert_eq!(old(line), None, "old {line}");
        }
        for line in [9, 10, 14, 15, 16, 17] {
            assert_eq!(new(line), None, "new {line}");
        }
        assert_eq!(old(0), None);
        assert_eq!(new(1000), None);
    }

    #[test]
    fn each_hunk_pairs_on_its_own() {
        let mut f = file(
            vec![Segment::Change {
                removed: 1,
                added: 1,
            }],
            0,
            0,
        );
        f.hunks.push(Hunk {
            old_start: 40,
            new_start: 41,
            segments: vec![Segment::Change {
                removed: 2,
                added: 2,
            }],
        });
        let pairs = Pairs::of(&f);
        assert_eq!(pairs.partner(Side::Old, 0), Some(0));
        assert_eq!(pairs.partner(Side::Old, 1), None);
        assert_eq!(pairs.partner(Side::Old, 41), Some(42));
        assert_eq!(pairs.partner(Side::New, 41), Some(40));
    }
}
