//! Colors and sizes for the scrollbar and the autoscroll marker. Plain data:
//! the caller maps its theme onto it.

use gpui::{Hsla, Pixels, px, rgb};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrollbarStyle {
    /// Thickness at rest.
    pub width: Pixels,
    /// Thickness while the pointer is over the bar or it is dragged.
    pub hover_width: Pixels,
    /// The shortest the thumb gets, along the track.
    pub min_thumb: Pixels,
    /// Behind the thumb; fades in with the hover width.
    pub track: Hsla,
    pub thumb: Hsla,
    /// The thumb's color at full hover width.
    pub thumb_hover: Hsla,
    /// The thumb's color while it is dragged.
    pub thumb_active: Hsla,
}

impl Default for ScrollbarStyle {
    fn default() -> Self {
        let c = |hex: u32| Hsla::from(rgb(hex));
        Self {
            width: px(8.0),
            hover_width: px(12.0),
            min_thumb: px(24.0),
            track: Hsla {
                a: 0.0,
                ..c(0x000000)
            },
            thumb: c(0x3b404a),
            thumb_hover: c(0x4d535f),
            thumb_active: c(0x646b79),
        }
    }
}

/// The anchor marker drawn at the press point of an autoscroll.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AutoscrollStyle {
    pub diameter: Pixels,
    pub fill: Hsla,
    pub border: Hsla,
    /// The arrows and the center dot.
    pub arrow: Hsla,
}

impl Default for AutoscrollStyle {
    fn default() -> Self {
        let c = |hex: u32| Hsla::from(rgb(hex));
        Self {
            diameter: px(28.0),
            fill: Hsla {
                a: 0.92,
                ..c(0x22262d)
            },
            border: c(0x646b79),
            arrow: c(0xd4d2cf),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ScrollStyle {
    pub scrollbar: ScrollbarStyle,
    pub autoscroll: AutoscrollStyle,
}

/// `a` to `b` by `t` (0 to 1), channel by channel.
#[must_use]
pub fn mix(a: Hsla, b: Hsla, t: f32) -> Hsla {
    let lerp = |from: f32, to: f32| from + (to - from) * t;
    Hsla {
        h: lerp(a.h, b.h),
        s: lerp(a.s, b.s),
        l: lerp(a.l, b.l),
        a: lerp(a.a, b.a),
    }
}
