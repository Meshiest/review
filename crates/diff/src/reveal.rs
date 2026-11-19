//! Context revealed around hunks: the lines hidden before, between and after
//! a file's hunks, and how many of each run are shown.

use crate::model::{FileDiff, Side};

/// How many hidden lines one expand arrow reveals.
pub const EXPAND_STEP: u32 = 20;

/// A run of lines a file's hunks do not show: before hunk `gap`, or after the
/// last hunk for `gap == hunks.len()`. The lines are the same on both sides,
/// starting at `old` and `new` (0-based).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gap {
    pub old: u32,
    pub new: u32,
    pub len: u32,
}

impl FileDiff {
    /// How many gaps the file has: one before each hunk and one after the
    /// last; none without hunks.
    #[must_use]
    pub fn gap_count(&self) -> u32 {
        if self.hunks.is_empty() {
            0
        } else {
            self.hunks.len() as u32 + 1
        }
    }

    /// The gap before hunk `gap`, or after the last one for `gap ==
    /// hunks.len()`; `None` past that or for a file without hunks. A gap is
    /// cut short where a blob has fewer lines than the hunks say.
    #[must_use]
    pub fn gap(&self, gap: u32) -> Option<Gap> {
        if gap >= self.gap_count() {
            return None;
        }
        let ix = gap as usize;
        let (old_end, new_end) = match ix.checked_sub(1) {
            Some(prev) => {
                let hunk = &self.hunks[prev];
                (
                    hunk.old_start + hunk.old_len(),
                    hunk.new_start + hunk.new_len(),
                )
            }
            None => (0, 0),
        };
        let (old_limit, new_limit) = (self.old.line_count(), self.new.line_count());
        let (old_next, new_next) = self
            .hunks
            .get(ix)
            .map_or((old_limit, new_limit), |h| (h.old_start, h.new_start));
        let len = old_next
            .saturating_sub(old_end)
            .min(new_next.saturating_sub(new_end));
        // The first gap ends at its hunk; the others start at the hunk
        // before them.
        let (old, new) = if ix == 0 {
            (old_next - len, new_next - len)
        } else {
            (old_end, new_end)
        };
        let len = len
            .min(old_limit.saturating_sub(old))
            .min(new_limit.saturating_sub(new));
        Some(Gap { old, new, len })
    }
}

/// What an expand arrow reveals.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Expand {
    /// Lines above the hunk under the arrow, from the bottom of the gap.
    Up,
    /// Lines below the hunk before the arrow (or the file's last hunk), from
    /// the top of the gap.
    Down,
    /// The whole gap.
    All,
}

/// How much of one gap is shown: lines from its top and from its bottom.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GapReveal {
    pub top: u32,
    pub bottom: u32,
}

impl GapReveal {
    /// Held inside a gap of `len` lines.
    #[must_use]
    fn fit(self, len: u32) -> Self {
        let top = self.top.min(len);
        Self {
            top,
            bottom: self.bottom.min(len - top),
        }
    }

    /// Lines of a gap of `len` lines that stay hidden.
    #[must_use]
    pub fn hidden(self, len: u32) -> u32 {
        len.saturating_sub(self.top + self.bottom)
    }
}

/// One file's revealed context, by gap.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reveals(Vec<GapReveal>);

impl Reveals {
    /// How much of gap `gap` is shown, within a gap of `len` lines.
    #[must_use]
    pub fn gap(&self, gap: u32, len: u32) -> GapReveal {
        self.0
            .get(gap as usize)
            .copied()
            .unwrap_or_default()
            .fit(len)
    }

    /// Whether no line is revealed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.iter().all(|gap| *gap == GapReveal::default())
    }

    /// Reveals more of gap `gap` of `file`; whether anything changed.
    pub fn expand(&mut self, file: &FileDiff, gap: u32, expand: Expand) -> bool {
        let Some(at) = file.gap(gap) else {
            return false;
        };
        let before = self.gap(gap, at.len);
        let hidden = before.hidden(at.len);
        if hidden == 0 {
            return false;
        }
        let step = hidden.min(EXPAND_STEP);
        let after = match expand {
            Expand::Up => GapReveal {
                bottom: before.bottom + step,
                ..before
            },
            Expand::Down => GapReveal {
                top: before.top + step,
                ..before
            },
            Expand::All => GapReveal {
                top: before.top + hidden,
                ..before
            },
        };
        let ix = gap as usize;
        if self.0.len() <= ix {
            self.0.resize(ix + 1, GapReveal::default());
        }
        self.0[ix] = after;
        true
    }

    /// Held to the gaps of `file`: reveals past its gaps are dropped and the
    /// rest cut to their gap's size.
    #[must_use]
    pub fn fitted(mut self, file: &FileDiff) -> Self {
        self.0.truncate(file.gap_count() as usize);
        for (ix, reveal) in self.0.iter_mut().enumerate() {
            let len = file.gap(ix as u32).map_or(0, |gap| gap.len);
            *reveal = reveal.fit(len);
        }
        self
    }

    /// Whether `line` of `side` is one of the revealed lines of `file`.
    #[must_use]
    pub fn shows(&self, file: &FileDiff, side: Side, line: u32) -> bool {
        self.0.iter().enumerate().any(|(ix, reveal)| {
            let Some(gap) = file.gap(ix as u32) else {
                return false;
            };
            let reveal = reveal.fit(gap.len);
            let start = match side {
                Side::Old => gap.old,
                Side::New => gap.new,
            };
            (start..start + reveal.top).contains(&line)
                || (start + gap.len - reveal.bottom..start + gap.len).contains(&line)
        })
    }
}

/// The arrows on gap `gap` of a file with `gaps` gaps and `hidden` lines
/// still hidden in it: none when nothing is hidden; on the last gap (after
/// the last hunk) one that reveals downward; elsewhere one that reveals the
/// whole gap when it is small, or else upward and, between hunks, downward.
#[must_use]
pub fn arrows(gap: u32, gaps: u32, hidden: u32) -> &'static [Expand] {
    if hidden == 0 {
        &[]
    } else if gap + 1 == gaps {
        &[Expand::Down]
    } else if hidden <= EXPAND_STEP {
        &[Expand::All]
    } else if gap == 0 {
        &[Expand::Up]
    } else {
        &[Expand::Down, Expand::Up]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Text;
    use crate::model::{Hunk, Segment};

    fn numbered(n: u32) -> Text {
        Text::new((0..n).map(|i| format!("l{i}\n")).collect::<String>())
    }

    /// 100 lines; hunks of 4 context lines at 30 and 40 (old) / 31 and 41
    /// (new: one line added in the first).
    fn file() -> FileDiff {
        let hunk = |old_start, new_start, added| Hunk {
            old_start,
            new_start,
            segments: vec![
                Segment::Context(2),
                Segment::Change { removed: 0, added },
                Segment::Context(2),
            ],
        };
        FileDiff {
            path: "a.rs".into(),
            old: numbered(100),
            new: numbered(101),
            hunks: vec![hunk(30, 30, 1), hunk(40, 41, 0)],
        }
    }

    #[test]
    fn gaps_lie_between_the_hunks_and_the_file_ends() {
        let file = file();
        assert_eq!(file.gap_count(), 3);
        assert_eq!(
            file.gap(0),
            Some(Gap {
                old: 0,
                new: 0,
                len: 30
            })
        );
        assert_eq!(
            file.gap(1),
            Some(Gap {
                old: 34,
                new: 35,
                len: 6
            })
        );
        assert_eq!(
            file.gap(2),
            Some(Gap {
                old: 44,
                new: 45,
                len: 56
            })
        );
        assert_eq!(file.gap(3), None);
        assert_eq!(FileDiff::default().gap(0), None);
    }

    #[test]
    fn a_gap_is_cut_to_the_blobs() {
        let mut file = file();
        file.old = numbered(40);
        file.new = numbered(41);
        // The second hunk runs past the end of the blobs: nothing after it,
        // and the gap before it stops at the last line there is.
        assert_eq!(file.gap(2).map(|g| g.len), Some(0));
        assert_eq!(file.gap(1).map(|g| g.len), Some(6));
        file.old = Text::default();
        file.new = Text::default();
        assert!((0..3).all(|gap| file.gap(gap).is_some_and(|g| g.len == 0)));
    }

    #[test]
    fn expanding_reveals_a_step_then_the_rest() {
        let file = file();
        let mut reveals = Reveals::default();
        assert!(reveals.expand(&file, 2, Expand::Down));
        assert_eq!(reveals.gap(2, 56), GapReveal { top: 20, bottom: 0 });
        assert!(reveals.expand(&file, 2, Expand::Up));
        assert!(reveals.expand(&file, 2, Expand::Down));
        assert_eq!(reveals.gap(2, 56).hidden(56), 0);
        // Only 16 were left for the last one.
        assert_eq!(
            reveals.gap(2, 56),
            GapReveal {
                top: 36,
                bottom: 20
            }
        );
        assert!(!reveals.expand(&file, 2, Expand::All));
        assert!(reveals.expand(&file, 1, Expand::All));
        assert_eq!(reveals.gap(1, 6).hidden(6), 0);
        assert!(!reveals.expand(&file, 9, Expand::All));
    }

    #[test]
    fn revealed_lines_are_shown_on_their_own_side() {
        let file = file();
        let mut reveals = Reveals::default();
        reveals.expand(&file, 0, Expand::Up);
        // Old and new lines 10..30 above the first hunk.
        assert!(reveals.shows(&file, Side::Old, 10) && reveals.shows(&file, Side::New, 29));
        assert!(!reveals.shows(&file, Side::Old, 9));
        reveals.expand(&file, 2, Expand::Down);
        // After the second hunk: old 44.., new 45..
        assert!(reveals.shows(&file, Side::New, 45) && reveals.shows(&file, Side::New, 64));
        assert!(!reveals.shows(&file, Side::New, 65) && !reveals.shows(&file, Side::Old, 64));
    }

    #[test]
    fn fitting_drops_what_the_file_has_no_room_for() {
        let file = file();
        let reveals = Reveals(vec![
            GapReveal { top: 0, bottom: 50 },
            GapReveal { top: 4, bottom: 4 },
            GapReveal::default(),
            GapReveal { top: 3, bottom: 0 },
        ]);
        let fitted = reveals.fitted(&file);
        assert_eq!(
            fitted.0,
            [
                GapReveal { top: 0, bottom: 30 },
                GapReveal { top: 4, bottom: 2 },
                GapReveal::default(),
            ]
        );
        assert!(!fitted.is_empty());
        assert!(Reveals::default().fitted(&file).is_empty());
    }

    #[test]
    fn arrows_follow_the_gap_and_what_is_hidden() {
        use Expand::{All, Down, Up};
        assert_eq!(arrows(0, 3, 0), []);
        assert_eq!(arrows(0, 3, 30), [Up]);
        assert_eq!(arrows(0, 3, 20), [All]);
        assert_eq!(arrows(1, 3, 21), [Down, Up]);
        assert_eq!(arrows(1, 3, 6), [All]);
        assert_eq!(arrows(2, 3, 56), [Down]);
        assert_eq!(arrows(2, 3, 5), [Down]);
        assert_eq!(arrows(2, 3, 0), []);
    }
}
