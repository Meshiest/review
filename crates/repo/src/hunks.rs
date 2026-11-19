//! Line hunks between two blobs, grouped the way `git diff -U3` groups them.

use std::fmt::Write as _;
use std::ops::Range;

use diff::{Hunk, Segment};
use imara::{Algorithm, Diff, InternedInput};

use crate::blob::Blob;

/// Context lines around each change, as in `git diff -U3`.
pub const CONTEXT: u32 = 3;

/// Hunks of `old` to `new` with [`CONTEXT`] lines of context.
///
/// Myers with git's slider heuristic: that is what `git diff` runs by
/// default, and it is the variant the oracle test matches. Identical blobs
/// give no hunks.
#[must_use]
pub fn compute_hunks(old: &Blob, new: &Blob) -> Vec<Hunk> {
    if old.as_bytes() == new.as_bytes() {
        return Vec::new();
    }
    let input = InternedInput::new(old.as_bytes(), new.as_bytes());
    let mut diff = Diff::compute(Algorithm::Myers, &input);
    diff.postprocess_lines(&input);
    let changes: Vec<imara::Hunk> = diff.hunks().collect();
    group(&changes, old.line_count(), CONTEXT)
}

/// Merges changes whose gap fits in `2 * context` lines (xdiff's rule) and
/// pads each group with context.
fn group(changes: &[imara::Hunk], old_lines: u32, context: u32) -> Vec<Hunk> {
    let mut hunks = Vec::new();
    let mut rest = changes;
    while let Some(first) = rest.first() {
        let len = 1 + rest
            .windows(2)
            .take_while(|pair| pair[1].before.start - pair[0].before.end <= 2 * context)
            .count();
        let (group, tail) = rest.split_at(len);
        rest = tail;
        let last = group.last().unwrap_or(first);

        let lead = context.min(first.before.start);
        let trail = context.min(old_lines - last.before.end);
        let mut segments = Vec::with_capacity(group.len() * 2 + 1);
        push_context(&mut segments, lead);
        for (ix, change) in group.iter().enumerate() {
            if let Some(prev) = ix.checked_sub(1).map(|p| &group[p]) {
                push_context(&mut segments, change.before.start - prev.before.end);
            }
            segments.push(Segment::Change {
                removed: change.before.len() as u32,
                added: change.after.len() as u32,
            });
        }
        push_context(&mut segments, trail);
        hunks.push(Hunk {
            old_start: first.before.start - lead,
            new_start: first.after.start - lead,
            segments,
        });
    }
    hunks
}

fn push_context(segments: &mut Vec<Segment>, lines: u32) {
    if lines > 0 {
        segments.push(Segment::Context(lines));
    }
}

/// Renders `hunk` as `git diff` prints it: the `@@` header without function
/// context, then each line with its prefix, and `\ No newline at end of file`
/// after a last line that has no line ending.
#[must_use]
pub fn render_hunk(old: &Blob, new: &Blob, hunk: &Hunk) -> Vec<u8> {
    let mut out = Vec::new();
    let _ = writeln!(
        VecWriter(&mut out),
        "@@ -{} +{} @@",
        range_text(hunk.old_start, hunk.old_len()),
        range_text(hunk.new_start, hunk.new_len())
    );
    let (mut o, mut n) = (hunk.old_start, hunk.new_start);
    for segment in &hunk.segments {
        match *segment {
            Segment::Context(count) => {
                // Context shows the new side's bytes: a missing final newline
                // on one side only never lands in a context line.
                push_lines(&mut out, b' ', new, n..n + count);
                o += count;
                n += count;
            }
            Segment::Change { removed, added } => {
                push_lines(&mut out, b'-', old, o..o + removed);
                push_lines(&mut out, b'+', new, n..n + added);
                o += removed;
                n += added;
            }
        }
    }
    out
}

fn push_lines(out: &mut Vec<u8>, prefix: u8, blob: &Blob, lines: Range<u32>) {
    for ix in lines {
        out.push(prefix);
        out.extend_from_slice(blob.line_with_eol(ix));
        if !blob.line_with_eol(ix).ends_with(b"\n") {
            out.extend_from_slice(b"\n\\ No newline at end of file\n");
        }
    }
}

/// `start,len` as git prints it: 1-based, `,1` left out, and an empty range
/// names the line before it.
fn range_text(start: u32, len: u32) -> String {
    match len {
        0 => format!("{start},0"),
        1 => format!("{}", start + 1),
        _ => format!("{},{len}", start + 1),
    }
}

struct VecWriter<'a>(&'a mut Vec<u8>);

impl std::fmt::Write for VecWriter<'_> {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.0.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blob(text: &str) -> Blob {
        Blob::new(text.as_bytes()).unwrap()
    }

    fn render(old: &str, new: &str) -> String {
        let (old, new) = (blob(old), blob(new));
        compute_hunks(&old, &new)
            .iter()
            .map(|h| String::from_utf8(render_hunk(&old, &new, h)).unwrap())
            .collect()
    }

    #[test]
    fn identical_and_empty() {
        assert!(compute_hunks(&blob("a\n"), &blob("a\n")).is_empty());
        assert!(compute_hunks(&blob(""), &blob("")).is_empty());
    }

    #[test]
    fn single_change_with_context() {
        let old = "1\n2\n3\n4\n5\n6\n7\n8\n9\n";
        let new = "1\n2\n3\n4\nfive\n6\n7\n8\n9\n";
        assert_eq!(
            render(old, new),
            "@@ -2,7 +2,7 @@\n 2\n 3\n 4\n-5\n+five\n 6\n 7\n 8\n"
        );
    }

    #[test]
    fn addition_to_empty_and_removal_to_empty() {
        assert_eq!(render("", "a\nb\n"), "@@ -0,0 +1,2 @@\n+a\n+b\n");
        assert_eq!(render("a\nb\n", ""), "@@ -1,2 +0,0 @@\n-a\n-b\n");
    }

    #[test]
    fn missing_final_newline_is_marked() {
        assert_eq!(
            render("a\nb", "a\nb\n"),
            "@@ -1,2 +1,2 @@\n a\n-b\n\\ No newline at end of file\n+b\n"
        );
    }

    #[test]
    fn crlf_change_is_a_change() {
        assert_eq!(render("a\r\n", "a\n"), "@@ -1 +1 @@\n-a\r\n+a\n");
    }

    #[test]
    fn nearby_changes_merge_far_ones_split() {
        let lines = |edits: &[usize]| {
            (0..40)
                .map(|i| {
                    if edits.contains(&i) {
                        format!("x{i}\n")
                    } else {
                        format!("{i}\n")
                    }
                })
                .collect::<String>()
        };
        let base = lines(&[]);
        // Gap of 6 unchanged lines merges, a gap of 7 splits.
        assert_eq!(
            compute_hunks(&blob(&base), &blob(&lines(&[10, 17]))).len(),
            1
        );
        assert_eq!(
            compute_hunks(&blob(&base), &blob(&lines(&[10, 18]))).len(),
            2
        );
    }
}
