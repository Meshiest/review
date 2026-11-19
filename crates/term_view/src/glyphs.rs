//! Which codepoints the terminal draws itself or sends to the symbols font.
//!
//! Powerline separators and block elements must fill their cell edge to edge;
//! a font's outlines stop at its ascent and descent, so a row taller than the
//! font shows gaps. Those are described here as shapes in cell space (0..1 on
//! both axes) and painted by the element. Other icons come from the bundled
//! symbols font.

use std::borrow::Cow;

use gpui::App;

/// Family name of the bundled symbols font.
pub const SYMBOLS_FAMILY: &str = "Symbols Nerd Font Mono";

static SYMBOLS_FONT: &[u8] = include_bytes!("../assets/fonts/SymbolsNerdFontMono-Regular.ttf");

/// Registers the bundled symbols font with gpui's text system. Call once at
/// startup, before the first terminal paints.
pub fn init(cx: &App) {
    cx.text_system()
        .add_fonts(vec![Cow::Borrowed(SYMBOLS_FONT)])
        .expect("the bundled symbols font is valid TrueType");
}

/// How one character is painted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Route {
    /// The terminal's text font.
    Text,
    /// The bundled symbols font, shaped on its own so its metrics do not move
    /// the baseline of the row's text.
    Symbols,
    /// Shapes from [`drawing`].
    Drawn,
}

pub(crate) fn route(c: char) -> Route {
    if drawing(c).is_some() {
        return Route::Drawn;
    }
    match c {
        // Private use areas (Powerline, Devicons, Codicons, Material ...),
        // and the power symbols that Nerd Fonts maps outside them.
        '\u{E000}'..='\u{F8FF}'
        | '\u{F0000}'..='\u{FFFFD}'
        | '\u{23FB}'..='\u{23FE}'
        | '\u{2B58}' => Route::Symbols,
        _ => Route::Text,
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Pt {
    pub x: f32,
    pub y: f32,
}

const fn pt(x: f32, y: f32) -> Pt {
    Pt { x, y }
}

/// An axis-aligned rectangle in cell space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Rect {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

const fn rect(x0: f32, y0: f32, x1: f32, y1: f32) -> Rect {
    Rect { x0, y0, x1, y1 }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Cmd {
    Move(Pt),
    Line(Pt),
    /// Control point, control point, end point.
    Cubic(Pt, Pt, Pt),
}

impl Cmd {
    fn mirrored(self) -> Self {
        let flip = |p: Pt| pt(1. - p.x, p.y);
        match self {
            Cmd::Move(p) => Cmd::Move(flip(p)),
            Cmd::Line(p) => Cmd::Line(flip(p)),
            Cmd::Cubic(a, b, c) => Cmd::Cubic(flip(a), flip(b), flip(c)),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Drawing {
    /// Rectangles filled with the foreground at `alpha`.
    Quads { rects: Vec<Rect>, alpha: f32 },
    /// A closed outline filled with the foreground.
    Fill(Vec<Cmd>),
    /// An open line stroked with the foreground. The ends run past the cell
    /// so the element's clip leaves no gap at its edges.
    Stroke(Vec<Cmd>),
}

/// The shape for `c`, or `None` when the font draws it.
pub(crate) fn drawing(c: char) -> Option<Drawing> {
    match c {
        '\u{E0B0}'..='\u{E0BF}' => powerline(c),
        '\u{2580}'..='\u{259F}' => Some(block(c)),
        _ => None,
    }
}

fn polygon(points: &[Pt]) -> Vec<Cmd> {
    let mut cmds = vec![Cmd::Move(points[0])];
    cmds.extend(points[1..].iter().map(|p| Cmd::Line(*p)));
    cmds
}

/// How far a stroked line is extended past the cell, in cell units.
const OVERRUN: f32 = 0.25;

fn powerline(c: char) -> Option<Drawing> {
    // Half-ellipse bulging to the right, the full cell tall.
    const K: f32 = 0.552_284_8;
    let half_ellipse = [
        Cmd::Move(pt(0., 0.)),
        Cmd::Cubic(pt(K, 0.), pt(1., 0.5 - 0.5 * K), pt(1., 0.5)),
        Cmd::Cubic(pt(1., 0.5 + 0.5 * K), pt(K, 1.), pt(0., 1.)),
    ];
    // Overrun left of the cell keeps the stroke's butt ends hidden by the clip.
    let round_line = [
        Cmd::Move(pt(-OVERRUN, 0.)),
        Cmd::Line(pt(0., 0.)),
        half_ellipse[1],
        half_ellipse[2],
        Cmd::Line(pt(-OVERRUN, 1.)),
    ];
    // Direction of the chevron arms is (1, 0.5) per cell, so extending by
    // OVERRUN along it moves the end by (OVERRUN, OVERRUN / 2).
    let chevron = [
        Cmd::Move(pt(-OVERRUN, -OVERRUN / 2.)),
        Cmd::Line(pt(1., 0.5)),
        Cmd::Line(pt(-OVERRUN, 1. + OVERRUN / 2.)),
    ];
    let backslash = [
        Cmd::Move(pt(-OVERRUN, -OVERRUN)),
        Cmd::Line(pt(1. + OVERRUN, 1. + OVERRUN)),
    ];
    let slash = [
        Cmd::Move(pt(1. + OVERRUN, -OVERRUN)),
        Cmd::Line(pt(-OVERRUN, 1. + OVERRUN)),
    ];
    let mirrored = |cmds: &[Cmd]| cmds.iter().map(|c| c.mirrored()).collect::<Vec<_>>();

    Some(match c {
        '\u{E0B0}' => Drawing::Fill(polygon(&[pt(0., 0.), pt(1., 0.5), pt(0., 1.)])),
        '\u{E0B1}' => Drawing::Stroke(chevron.to_vec()),
        '\u{E0B2}' => Drawing::Fill(polygon(&[pt(1., 0.), pt(0., 0.5), pt(1., 1.)])),
        '\u{E0B3}' => Drawing::Stroke(mirrored(&chevron)),
        '\u{E0B4}' => Drawing::Fill(half_ellipse.to_vec()),
        '\u{E0B5}' => Drawing::Stroke(round_line.to_vec()),
        '\u{E0B6}' => Drawing::Fill(mirrored(&half_ellipse)),
        '\u{E0B7}' => Drawing::Stroke(mirrored(&round_line)),
        '\u{E0B8}' => Drawing::Fill(polygon(&[pt(0., 0.), pt(0., 1.), pt(1., 1.)])),
        '\u{E0B9}' | '\u{E0BF}' => Drawing::Stroke(backslash.to_vec()),
        '\u{E0BA}' => Drawing::Fill(polygon(&[pt(1., 0.), pt(1., 1.), pt(0., 1.)])),
        '\u{E0BB}' | '\u{E0BD}' => Drawing::Stroke(slash.to_vec()),
        '\u{E0BC}' => Drawing::Fill(polygon(&[pt(0., 0.), pt(1., 0.), pt(0., 1.)])),
        '\u{E0BE}' => Drawing::Fill(polygon(&[pt(0., 0.), pt(1., 0.), pt(1., 1.)])),
        _ => return None,
    })
}

fn block(c: char) -> Drawing {
    const UL: u8 = 1;
    const UR: u8 = 2;
    const LL: u8 = 4;
    const LR: u8 = 8;

    let eighths = f32::from(u8::try_from(u32::from(c) & 0xF).unwrap_or(0)) / 8.;
    let solid = |rects: Vec<Rect>| Drawing::Quads { rects, alpha: 1. };
    let quadrants = |mask: u8| {
        let cells = [
            (UL, rect(0., 0., 0.5, 0.5)),
            (UR, rect(0.5, 0., 1., 0.5)),
            (LL, rect(0., 0.5, 0.5, 1.)),
            (LR, rect(0.5, 0.5, 1., 1.)),
        ];
        solid(
            cells
                .into_iter()
                .filter(|(bit, _)| mask & bit != 0)
                .map(|(_, r)| r)
                .collect(),
        )
    };
    let shade = |alpha| Drawing::Quads {
        rects: vec![rect(0., 0., 1., 1.)],
        alpha,
    };

    match c {
        '\u{2580}' => solid(vec![rect(0., 0., 1., 0.5)]),
        // 2581..=2587: lower one to seven eighths.
        '\u{2581}'..='\u{2587}' => solid(vec![rect(0., 1. - eighths, 1., 1.)]),
        '\u{2588}' => solid(vec![rect(0., 0., 1., 1.)]),
        // 2589..=258F: left seven eighths down to one eighth.
        '\u{2589}'..='\u{258F}' => solid(vec![rect(0., 0., 2. - eighths, 1.)]),
        '\u{2590}' => solid(vec![rect(0.5, 0., 1., 1.)]),
        '\u{2591}' => shade(0.25),
        '\u{2592}' => shade(0.5),
        '\u{2593}' => shade(0.75),
        '\u{2594}' => solid(vec![rect(0., 0., 1., 0.125)]),
        '\u{2595}' => solid(vec![rect(0.875, 0., 1., 1.)]),
        '\u{2596}' => quadrants(LL),
        '\u{2597}' => quadrants(LR),
        '\u{2598}' => quadrants(UL),
        '\u{2599}' => quadrants(UL | LL | LR),
        '\u{259A}' => quadrants(UL | LR),
        '\u{259B}' => quadrants(UL | UR | LL),
        '\u{259C}' => quadrants(UL | UR | LR),
        '\u{259D}' => quadrants(UR),
        '\u{259E}' => quadrants(UR | LL),
        _ => quadrants(UR | LL | LR),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn points(cmd: Cmd) -> impl Iterator<Item = Pt> {
        let (a, b, c) = match cmd {
            Cmd::Move(p) | Cmd::Line(p) => (Some(p), None, None),
            Cmd::Cubic(a, b, c) => (Some(a), Some(b), Some(c)),
        };
        [a, b, c].into_iter().flatten()
    }

    fn fill_points(c: char) -> Vec<Pt> {
        match drawing(c) {
            Some(Drawing::Fill(cmds)) => cmds.into_iter().flat_map(points).collect(),
            other => panic!("{c:?} is not a filled shape: {other:?}"),
        }
    }

    /// Shoelace area of the polygon through `points`.
    fn area(points: &[Pt]) -> f32 {
        let n = points.len();
        (0..n)
            .map(|i| {
                let (a, b) = (points[i], points[(i + 1) % n]);
                a.x * b.y - b.x * a.y
            })
            .sum::<f32>()
            .abs()
            / 2.
    }

    fn quad_area(c: char) -> f32 {
        let Some(Drawing::Quads { rects, alpha }) = drawing(c) else {
            panic!("{c:?} is not a block");
        };
        assert!(alpha > 0.);
        rects
            .iter()
            .map(|r| (r.x1 - r.x0) * (r.y1 - r.y0))
            .sum::<f32>()
    }

    #[test]
    fn private_use_icons_go_to_the_symbols_font() {
        for c in [
            '\u{E0A0}',
            '\u{E0C0}',
            '\u{E5FF}',
            '\u{F115}',
            '\u{F0001}',
            '\u{23FB}',
        ] {
            assert_eq!(route(c), Route::Symbols, "{c:?}");
        }
    }

    #[test]
    fn separators_and_blocks_are_drawn() {
        for c in ('\u{E0B0}'..='\u{E0BF}').chain('\u{2580}'..='\u{259F}') {
            assert_eq!(route(c), Route::Drawn, "{c:?}");
        }
    }

    #[test]
    fn ordinary_text_stays_in_the_text_font() {
        for c in [
            'a', ' ', '日', '\u{25B6}', '\u{2500}', '\u{25A0}', '\u{E0AF}', '\u{E0C0}',
        ] {
            assert_ne!(route(c), Route::Drawn, "{c:?}");
        }
        for c in ['a', ' ', '日', '\u{2500}'] {
            assert_eq!(route(c), Route::Text, "{c:?}");
        }
    }

    #[test]
    fn solid_arrow_spans_the_full_cell_height() {
        let points = fill_points('\u{E0B0}');
        assert_eq!(points, [pt(0., 0.), pt(1., 0.5), pt(0., 1.)]);
        assert_eq!(area(&points), 0.5);
    }

    #[test]
    fn left_arrow_mirrors_the_right_arrow() {
        let right = fill_points('\u{E0B0}');
        let left = fill_points('\u{E0B2}');
        for (r, l) in right.iter().zip(&left) {
            assert_eq!((1. - r.x, r.y), (l.x, l.y));
        }
    }

    #[test]
    fn round_separator_touches_all_four_cell_edges_and_stays_inside() {
        let points = fill_points('\u{E0B4}');
        assert!(
            points
                .iter()
                .all(|p| (0. ..=1.).contains(&p.x) && (0. ..=1.).contains(&p.y))
        );
        let max = |f: fn(&Pt) -> f32| points.iter().map(f).fold(f32::MIN, f32::max);
        let min = |f: fn(&Pt) -> f32| points.iter().map(f).fold(f32::MAX, f32::min);
        assert_eq!((min(|p| p.x), max(|p| p.x)), (0., 1.));
        assert_eq!((min(|p| p.y), max(|p| p.y)), (0., 1.));
        assert_eq!(points.first(), Some(&pt(0., 0.)));
        assert_eq!(points.last(), Some(&pt(0., 1.)));
    }

    #[test]
    fn round_separators_mirror_each_other() {
        let right = fill_points('\u{E0B4}');
        let left = fill_points('\u{E0B6}');
        assert_eq!(right.len(), left.len());
        for (r, l) in right.iter().zip(&left) {
            assert_eq!((1. - r.x, r.y), (l.x, l.y));
        }
    }

    #[test]
    fn diagonal_triangles_are_half_the_cell_and_tile_it_in_pairs() {
        for c in ['\u{E0B8}', '\u{E0BA}', '\u{E0BC}', '\u{E0BE}'] {
            assert_eq!(area(&fill_points(c)), 0.5, "{c:?}");
        }
        // The lower-left and upper-right triangles share the cell's diagonal.
        let lower_left = fill_points('\u{E0B8}');
        let upper_right = fill_points('\u{E0BE}');
        assert!(lower_left.contains(&pt(1., 1.)) && upper_right.contains(&pt(1., 1.)));
        assert!(lower_left.contains(&pt(0., 0.)) && upper_right.contains(&pt(0., 0.)));
    }

    #[test]
    fn thin_separators_are_strokes_that_run_past_the_cell() {
        for c in [
            '\u{E0B1}', '\u{E0B3}', '\u{E0B5}', '\u{E0B7}', '\u{E0B9}', '\u{E0BB}',
        ] {
            let Some(Drawing::Stroke(cmds)) = drawing(c) else {
                panic!("{c:?} should be a stroke");
            };
            let outside = cmds
                .iter()
                .copied()
                .flat_map(points)
                .any(|p| !(0. ..=1.).contains(&p.x) || !(0. ..=1.).contains(&p.y));
            assert!(outside, "{c:?} must overrun the cell so the clip trims it");
        }
    }

    #[test]
    fn block_elements_cover_the_expected_area() {
        let expected = [
            ('\u{2580}', 0.5),
            ('\u{2581}', 0.125),
            ('\u{2584}', 0.5),
            ('\u{2587}', 0.875),
            ('\u{2588}', 1.),
            ('\u{2589}', 0.875),
            ('\u{258C}', 0.5),
            ('\u{258F}', 0.125),
            ('\u{2590}', 0.5),
            ('\u{2594}', 0.125),
            ('\u{2595}', 0.125),
            ('\u{2596}', 0.25),
            ('\u{259A}', 0.5),
            ('\u{259B}', 0.75),
            ('\u{259F}', 0.75),
        ];
        for (c, want) in expected {
            assert_eq!(quad_area(c), want, "{c:?}");
        }
    }

    #[test]
    fn block_edges_meet_the_cell_edges() {
        let Some(Drawing::Quads { rects, .. }) = drawing('\u{2584}') else {
            panic!("lower half block");
        };
        assert_eq!(rects, [rect(0., 0.5, 1., 1.)]);
        let Some(Drawing::Quads { rects, .. }) = drawing('\u{258C}') else {
            panic!("left half block");
        };
        assert_eq!(rects, [rect(0., 0., 0.5, 1.)]);
    }

    #[test]
    fn shades_are_translucent_full_cells() {
        let alphas: Vec<f32> = ('\u{2591}'..='\u{2593}')
            .map(|c| match drawing(c) {
                Some(Drawing::Quads { rects, alpha }) => {
                    assert_eq!(rects, [rect(0., 0., 1., 1.)]);
                    alpha
                }
                other => panic!("{c:?}: {other:?}"),
            })
            .collect();
        assert_eq!(alphas, [0.25, 0.5, 0.75]);
    }
}
