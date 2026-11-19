//! Turns a snapshot of the grid into shaped text and quads in pixels, ready to
//! paint. Rows are expensive (shaping), so a frame's rows are cached by the
//! view and reused while nothing they depend on changed; the cursor is
//! separate and cheap, so blinking never re-shapes them.

use gpui::{
    Bounds, Font, FontFeatures, FontStyle, FontWeight, Hsla, Path, PathBuilder, Pixels, Point,
    ShapedLine, SharedString, Size, StrikethroughStyle, TextRun, UnderlineStyle, Window, point, px,
    size,
};

use crate::glyphs::{Cmd, Drawing, Pt, Rect, Route, SYMBOLS_FAMILY, drawing};
use crate::snapshot::{
    CursorKind, CursorSnapshot, GlyphCell, RowSnapshot, Segment, StyleRun, UnderCursor,
};
use crate::style::TermStyle;

#[derive(Default)]
pub(crate) struct PaintRow {
    pub backgrounds: Vec<(Bounds<Pixels>, Hsla)>,
    pub selection: Vec<Bounds<Pixels>>,
    pub lines: Vec<(Point<Pixels>, ShapedLine)>,
    pub drawn: Vec<Drawn>,
}

/// A separator or block element, already laid out in pixels.
pub(crate) enum Drawn {
    Quads(Vec<(Bounds<Pixels>, Hsla)>),
    Path {
        clip: Bounds<Pixels>,
        path: Path<Pixels>,
        color: Hsla,
    },
}

/// The cursor, laid out. Painted in this order: fills, outline, text, drawn.
#[derive(Default)]
pub(crate) struct CursorPaint {
    pub fills: Vec<(Bounds<Pixels>, Hsla)>,
    pub outline: Option<Bounds<Pixels>>,
    pub lines: Vec<(Point<Pixels>, ShapedLine)>,
    pub drawn: Vec<Drawn>,
}

/// Shapes and lays out cells for one style, cell size and scale.
pub(crate) struct Painter<'a> {
    pub style: &'a TermStyle,
    pub cell: Size<Pixels>,
    pub scale: f32,
}

impl Painter<'_> {
    pub(crate) fn rows(
        &self,
        snapshot: &[RowSnapshot],
        origin: Point<Pixels>,
        window: &Window,
    ) -> Vec<PaintRow> {
        let cell = self.cell;
        snapshot
            .iter()
            .enumerate()
            .map(|(row, snap)| {
                let y = origin.y + cell.height * row as f32;
                let mut lines: Vec<_> = snap
                    .segments
                    .iter()
                    .map(|segment| {
                        let x = origin.x + cell.width * segment.col as f32;
                        (point(x, y), self.shape(segment, window))
                    })
                    .collect();
                let mut drawn = Vec::new();
                for glyph in &snap.glyphs {
                    let at = point(origin.x + cell.width * glyph.col as f32, y);
                    match glyph.route {
                        Route::Symbols => lines.push((at, self.shape_symbol(glyph, window))),
                        Route::Drawn => {
                            let bounds = snap_bounds(Bounds::new(at, cell), self.scale);
                            drawn.extend(self.draw(glyph.c, glyph.fg.into(), bounds));
                        }
                        Route::Text => {}
                    }
                }
                let span = |col: usize, cols: usize| {
                    Bounds::new(
                        point(origin.x + cell.width * col as f32, y),
                        size(cell.width * cols as f32, cell.height),
                    )
                };
                PaintRow {
                    backgrounds: snap
                        .backgrounds
                        .iter()
                        .map(|s| (span(s.col, s.cols), s.color.into()))
                        .collect(),
                    selection: snap.selection.iter().map(|s| span(s.col, s.cols)).collect(),
                    lines,
                    drawn,
                }
            })
            .collect()
    }

    pub(crate) fn cursor(
        &self,
        cursor: &CursorSnapshot,
        origin: Point<Pixels>,
        window: &Window,
    ) -> CursorPaint {
        let cell = self.cell;
        let at = point(
            origin.x + cell.width * cursor.col as f32,
            origin.y + cell.height * cursor.row as f32,
        );
        let bounds = Bounds::new(at, size(cell.width * cursor.cols as f32, cell.height));
        let color: Hsla = self.style.cursor.into();
        let thickness = px(2.);
        let mut paint = CursorPaint::default();
        match cursor.kind {
            CursorKind::SolidBlock => {
                paint.fills.push((bounds, color));
                if let Some(under) = &cursor.under {
                    self.redraw_under(under, cursor.cols == 2, at, &mut paint, window);
                }
            }
            CursorKind::HollowBlock => paint.outline = Some(bounds),
            CursorKind::Underline => paint.fills.push((
                Bounds::new(
                    point(at.x, at.y + cell.height - thickness),
                    size(bounds.size.width, thickness),
                ),
                color,
            )),
            CursorKind::Beam => paint
                .fills
                .push((Bounds::new(at, size(thickness, cell.height)), color)),
        }
        paint
    }

    /// The character under a block cursor, in the background color so it
    /// reads against the cursor.
    fn redraw_under(
        &self,
        under: &UnderCursor,
        wide: bool,
        at: Point<Pixels>,
        paint: &mut CursorPaint,
        window: &Window,
    ) {
        let color = self.style.background;
        match under.route {
            Route::Text => {
                let segment = Segment {
                    col: 0,
                    text: under.text.clone(),
                    runs: vec![StyleRun {
                        len: under.text.len(),
                        fg: color,
                        bold: under.bold,
                        italic: under.italic,
                        underline: false,
                        strikeout: false,
                    }],
                    wide,
                };
                paint.lines.push((at, self.shape(&segment, window)));
            }
            Route::Symbols => {
                let glyph = GlyphCell {
                    col: 0,
                    c: under.c,
                    route: Route::Symbols,
                    fg: color,
                };
                paint.lines.push((at, self.shape_symbol(&glyph, window)));
            }
            Route::Drawn => {
                let bounds = snap_bounds(Bounds::new(at, self.cell), self.scale);
                paint.drawn.extend(self.draw(under.c, color.into(), bounds));
            }
        }
    }

    fn font(&self, bold: bool, italic: bool) -> Font {
        font(self.style, bold, italic)
    }

    fn shape(&self, segment: &Segment, window: &Window) -> ShapedLine {
        let runs: Vec<TextRun> = segment.runs.iter().map(|run| self.text_run(run)).collect();
        let force_width = if segment.wide {
            self.cell.width * 2.
        } else {
            self.cell.width
        };
        window.text_system().shape_line(
            SharedString::from(segment.text.clone()),
            self.style.font_size,
            &runs,
            Some(force_width),
        )
    }

    /// One icon on its own line. The symbols font has different metrics, and
    /// sharing a line would shift the baseline of the row's text.
    fn shape_symbol(&self, glyph: &GlyphCell, window: &Window) -> ShapedLine {
        let text = SharedString::from(glyph.c.to_string());
        let run = TextRun {
            len: text.len(),
            font: Font {
                family: SYMBOLS_FAMILY.into(),
                features: FontFeatures::disable_ligatures(),
                fallbacks: None,
                weight: FontWeight::NORMAL,
                style: FontStyle::Normal,
            },
            color: Hsla::from(glyph.fg),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        window
            .text_system()
            .shape_line(text, self.style.font_size, &[run], Some(self.cell.width))
    }

    fn draw(&self, c: char, color: Hsla, cell: Bounds<Pixels>) -> Option<Drawn> {
        match drawing(c)? {
            Drawing::Quads { rects, alpha } => Some(Drawn::Quads(
                rects
                    .iter()
                    .map(|rect| {
                        (
                            snap_bounds(to_pixels(rect, cell), self.scale),
                            color.opacity(alpha),
                        )
                    })
                    .collect(),
            )),
            Drawing::Fill(cmds) => {
                let mut builder = PathBuilder::fill();
                trace(&mut builder, &cmds, cell);
                builder.close();
                Some(Drawn::Path {
                    clip: cell,
                    path: builder.build().ok()?,
                    color,
                })
            }
            Drawing::Stroke(cmds) => {
                let mut builder = PathBuilder::stroke((cell.size.width * 0.1).max(px(1.)));
                trace(&mut builder, &cmds, cell);
                Some(Drawn::Path {
                    clip: cell,
                    path: builder.build().ok()?,
                    color,
                })
            }
        }
    }

    fn text_run(&self, run: &StyleRun) -> TextRun {
        let color = Hsla::from(run.fg);
        TextRun {
            len: run.len,
            font: self.font(run.bold, run.italic),
            color,
            background_color: None,
            underline: run.underline.then_some(UnderlineStyle {
                thickness: px(1.),
                color: Some(color),
                wavy: false,
            }),
            strikethrough: run.strikeout.then_some(StrikethroughStyle {
                thickness: px(1.),
                color: Some(color),
            }),
        }
    }
}

fn font(style: &TermStyle, bold: bool, italic: bool) -> Font {
    Font {
        family: style.font_family.clone(),
        features: FontFeatures::disable_ligatures(),
        fallbacks: style.font_fallbacks.clone(),
        weight: if bold {
            FontWeight::BOLD
        } else {
            FontWeight::NORMAL
        },
        style: if italic {
            FontStyle::Italic
        } else {
            FontStyle::Normal
        },
    }
}

/// Cell size in logical pixels, rounded to whole device pixels so rows and
/// background rectangles meet without seams.
pub(crate) fn cell_size(style: &TermStyle, window: &Window) -> Size<Pixels> {
    let scale = window.scale_factor();
    let snap = |value: Pixels| px((f32::from(value) * scale).round().max(1.) / scale);
    let text_system = window.text_system();
    let font_id = text_system.resolve_font(&font(style, false, false));
    let font_size = style.font_size;
    let advance = text_system
        .advance(font_id, font_size, 'm')
        .map_or(font_size * 0.6, |advance| advance.width);
    size(snap(advance), snap(font_size * style.line_height))
}

fn to_pixels(rect: &Rect, cell: Bounds<Pixels>) -> Bounds<Pixels> {
    Bounds::from_corners(
        at(
            Pt {
                x: rect.x0,
                y: rect.y0,
            },
            cell,
        ),
        at(
            Pt {
                x: rect.x1,
                y: rect.y1,
            },
            cell,
        ),
    )
}

fn at(p: Pt, cell: Bounds<Pixels>) -> Point<Pixels> {
    point(
        cell.origin.x + cell.size.width * p.x,
        cell.origin.y + cell.size.height * p.y,
    )
}

fn trace(builder: &mut PathBuilder, cmds: &[Cmd], cell: Bounds<Pixels>) {
    for cmd in cmds {
        match *cmd {
            Cmd::Move(p) => builder.move_to(at(p, cell)),
            Cmd::Line(p) => builder.line_to(at(p, cell)),
            Cmd::Cubic(a, b, end) => {
                builder.cubic_bezier_to(at(end, cell), at(a, cell), at(b, cell))
            }
        }
    }
}

/// Rounds the edges to whole device pixels so neighboring shapes meet exactly.
fn snap_bounds(bounds: Bounds<Pixels>, scale: f32) -> Bounds<Pixels> {
    let snap = |value: Pixels| px((f32::from(value) * scale).round() / scale);
    Bounds::from_corners(
        point(snap(bounds.origin.x), snap(bounds.origin.y)),
        point(snap(bounds.right()), snap(bounds.bottom())),
    )
}

/// How many whole cells fit in `length`.
pub(crate) fn fit(length: Pixels, cell: Pixels) -> u16 {
    ((length / cell).floor() as u16).max(1)
}

pub(crate) fn to_u16(value: Pixels) -> u16 {
    f32::from(value).round().clamp(1., f32::from(u16::MAX)) as u16
}
