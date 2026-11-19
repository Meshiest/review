//! Scroll position along one or two axes, in plain `f64` pixels.

/// One axis of a scrollable view: where it is, how long the content is and how
/// much of it is visible.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ScrollAxis {
    pub offset: f64,
    pub content: f64,
    pub viewport: f64,
}

impl ScrollAxis {
    /// The largest offset: the content's end at the viewport's end.
    #[must_use]
    pub fn max(&self) -> f64 {
        (self.content - self.viewport).max(0.0)
    }

    /// True when there is something to scroll to.
    #[must_use]
    pub fn overflows(&self) -> bool {
        self.content > self.viewport && self.viewport > 0.0
    }

    /// Sets the offset, clamped to `0..=max`; returns whether it changed.
    pub fn set(&mut self, offset: f64) -> bool {
        let offset = offset.clamp(0.0, self.max());
        let changed = offset != self.offset;
        self.offset = offset;
        changed
    }

    /// Scrolls by `delta` (positive moves toward the end); returns whether
    /// the offset changed.
    pub fn by(&mut self, delta: f64) -> bool {
        self.set(self.offset + delta)
    }

    /// Re-clamps after the content or viewport changed.
    pub fn clamp(&mut self) {
        self.set(self.offset);
    }
}

/// Both axes of a view. A view that does not scroll horizontally leaves `x`
/// at its default, which never overflows.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ScrollState {
    pub x: ScrollAxis,
    pub y: ScrollAxis,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offsets_clamp_to_the_content() {
        let mut axis = ScrollAxis {
            offset: 0.0,
            content: 1000.0,
            viewport: 400.0,
        };
        assert_eq!(axis.max(), 600.0);
        assert!(axis.set(900.0));
        assert_eq!(axis.offset, 600.0);
        assert!(!axis.set(700.0));
        assert!(axis.by(-1000.0));
        assert_eq!(axis.offset, 0.0);
    }

    #[test]
    fn content_that_fits_does_not_overflow() {
        let mut axis = ScrollAxis {
            offset: 50.0,
            content: 300.0,
            viewport: 400.0,
        };
        assert!(!axis.overflows());
        axis.clamp();
        assert_eq!(axis.offset, 0.0);
        assert!(!ScrollAxis::default().overflows());
    }
}
