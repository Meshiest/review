//! Caller-rendered elements: blocks, with the rail that ties a block to its
//! lines and how overlapping rails share the gutter, and the controls drawn
//! over the file headers.

use std::ops::Range;
use std::rc::Rc;

use diff::{BlockId, ContextMode, FileIx};
use gpui::{AnyElement, App, Hsla, Pixels, Size, Window};

/// Builds the element of one block for a given width. Called during
/// prepaint for each block in view, every frame it is in view; the view
/// lays the element out at that width, measures its height and paints it.
/// It must not read or update the [`crate::DiffView`] it renders into.
pub type RenderBlock = Rc<dyn Fn(BlockId, Pixels, &mut Window, &mut App) -> AnyElement>;

/// Builds the controls of one file header, drawn over the header the view
/// paints (its chevron, path and viewed checkbox). Called during prepaint
/// for each header in view and for the pinned one, every frame; the element
/// is laid out at the header's size. Only its interactive children should
/// take the mouse: a press on the rest of the header folds the file. Like
/// [`RenderBlock`], it must not read or update the view.
pub type RenderHeader = Rc<dyn Fn(HeaderPlace, &mut Window, &mut App) -> AnyElement>;

/// Room kept free in every file header for the controls of a
/// [`RenderHeader`], beside what the view paints there.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct HeaderSlots {
    /// Right after the path; a long path is cut short to keep it.
    pub after_path: Pixels,
    /// Left of the viewed checkbox, where the view paints the file's line
    /// counts and change bar; none are painted without it.
    pub stats: Pixels,
    /// Right of the "Viewed" label, to the header's right end.
    pub trailing: Pixels,
}

/// Where the controls of one file header go, relative to its top left.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HeaderPlace {
    pub file: FileIx,
    /// The copy held at the top of the view while its file scrolls under it.
    pub pinned: bool,
    pub size: Size<Pixels>,
    /// Where the path ends (or is cut short): the controls after it start here.
    pub after_path: Pixels,
    /// The left end of the [`HeaderSlots::trailing`] room.
    pub trailing: Pixels,
    pub context: ContextMode,
}

/// The colored line from a block's lines to the block: a marker on the
/// commented rows, then a rail down the gutter that turns into the block.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rail {
    pub color: Hsla,
    /// Drawn dashed, for a thread whose code has changed since.
    pub dashed: bool,
    /// How far below the block's top the rail turns into it: the middle of
    /// the block's first line, so it points at the header.
    pub elbow: Pixels,
}

/// The most rails that sit side by side; more share the last lane.
pub(crate) const MAX_LANES: u8 = 3;

/// A lane per rail so rails whose row spans overlap sit apart: each takes
/// the lowest lane free over its span. `spans` are `(block row span,
/// side)`; only rails on the same side compete.
pub(crate) fn assign_lanes<S: PartialEq + Copy>(spans: &[Option<(Range<usize>, S)>]) -> Vec<u8> {
    let mut order: Vec<usize> = (0..spans.len()).filter(|&ix| spans[ix].is_some()).collect();
    order.sort_by_key(|&ix| spans[ix].as_ref().map(|(span, _)| span.start));
    let mut lanes = vec![0u8; spans.len()];
    // The end of the span holding each lane, per side seen so far.
    let mut busy: Vec<(S, u8, usize)> = Vec::new();
    for ix in order {
        let Some((span, side)) = &spans[ix] else {
            continue;
        };
        busy.retain(|&(_, _, end)| end > span.start);
        let lane = (0..MAX_LANES)
            .find(|lane| !busy.iter().any(|&(s, l, _)| s == *side && l == *lane))
            .unwrap_or(MAX_LANES - 1);
        lanes[ix] = lane;
        busy.push((*side, lane, span.end + 1));
    }
    lanes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlapping_spans_take_separate_lanes() {
        let spans = [
            Some((10..20, 'r')),
            Some((12..15, 'r')),
            Some((13..14, 'l')),
            None,
            Some((30..31, 'r')),
            Some((14..22, 'r')),
        ];
        assert_eq!(assign_lanes(&spans), [0, 1, 0, 0, 0, 2]);
    }

    #[test]
    fn lanes_stop_at_the_last_one() {
        let spans: Vec<_> = (0..6).map(|_| Some((0..10, ()))).collect();
        assert_eq!(assign_lanes(&spans), [0, 1, 2, 2, 2, 2]);
    }
}
