//! A file's line counts and the five-block change bar a file header shows
//! beside them, as GitHub draws them.

use diff::{FileDiff, Segment};

/// How many blocks the change bar has.
pub const BAR_BLOCKS: usize = 5;

/// One block of the change bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BarBlock {
    Added,
    Removed,
    Unchanged,
}

/// Lines added and removed in `file`'s hunks.
#[must_use]
pub fn line_counts(file: &FileDiff) -> (u32, u32) {
    file.hunks
        .iter()
        .flat_map(|hunk| &hunk.segments)
        .fold((0, 0), |(added, removed), segment| match *segment {
            Segment::Change {
                added: a,
                removed: r,
            } => (added + a, removed + r),
            Segment::Context(_) => (added, removed),
        })
}

/// The bar for `added` and `removed` lines: one block per line up to five,
/// split between additions and deletions by their share, each kind that is
/// there keeping at least one block.
#[must_use]
pub fn change_bar(added: u32, removed: u32) -> [BarBlock; BAR_BLOCKS] {
    let mut bar = [BarBlock::Unchanged; BAR_BLOCKS];
    let total = u64::from(added) + u64::from(removed);
    if total == 0 {
        return bar;
    }
    let filled = total.min(BAR_BLOCKS as u64);
    let share = (u64::from(added) * filled + total / 2) / total;
    let mut green = share.clamp(u64::from(added > 0), filled);
    if removed > 0 && green == filled {
        green -= 1;
    }
    for (ix, block) in bar.iter_mut().enumerate().take(filled as usize) {
        *block = if (ix as u64) < green {
            BarBlock::Added
        } else {
            BarBlock::Removed
        };
    }
    bar
}

#[cfg(test)]
mod tests {
    use diff::{Hunk, Text};

    use super::*;
    use BarBlock::{Added as A, Removed as R, Unchanged as U};

    #[test]
    fn the_bar_splits_five_blocks_by_share_and_keeps_each_kind() {
        assert_eq!(change_bar(0, 0), [U; 5]);
        assert_eq!(change_bar(2, 1), [A, A, R, U, U]);
        assert_eq!(change_bar(38, 9), [A, A, A, A, R]);
        assert_eq!(change_bar(1, 400), [A, R, R, R, R]);
        assert_eq!(change_bar(400, 0), [A; 5]);
        assert_eq!(change_bar(0, 3), [R, R, R, U, U]);
        assert_eq!(change_bar(10, 10), [A, A, A, R, R]);
    }

    #[test]
    fn counts_add_up_the_changes_of_every_hunk() {
        let hunk = |segments| Hunk {
            old_start: 0,
            new_start: 0,
            segments,
        };
        let file = FileDiff {
            path: "a".into(),
            old: Text::new(""),
            new: Text::new(""),
            hunks: vec![
                hunk(vec![
                    Segment::Context(3),
                    Segment::Change {
                        removed: 1,
                        added: 2,
                    },
                ]),
                hunk(vec![Segment::Change {
                    removed: 4,
                    added: 0,
                }]),
            ],
        };
        assert_eq!(line_counts(&file), (2, 5));
    }
}
