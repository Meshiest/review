//! The text attributes an SGR escape sequence can set, and how a sequence
//! changes them.

use vte::Params;

/// A foreground or background color as the log wrote it. Mapping to real
/// colors is the view's job, so a theme change needs no re-parse.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Color {
    /// The view's default foreground or background.
    #[default]
    Default,
    /// 0-15 are the theme's palette, 16-255 the xterm cube and gray ramp.
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// Bold, dim, italic, underline and inverse.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Attrs(u8);

impl Attrs {
    pub const BOLD: Self = Self(1);
    pub const DIM: Self = Self(1 << 1);
    pub const ITALIC: Self = Self(1 << 2);
    pub const UNDERLINE: Self = Self(1 << 3);
    pub const INVERSE: Self = Self(1 << 4);

    #[must_use]
    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    fn set(&mut self, other: Self, on: bool) {
        if on {
            self.0 |= other.0;
        } else {
            self.0 &= !other.0;
        }
    }
}

/// Everything SGR sets for a run of text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub attrs: Attrs,
}

/// An index into the buffer's table of distinct styles. `DEFAULT` is the
/// unstyled text, so most lines need no spans at all.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct StyleId(pub u16);

impl StyleId {
    pub const DEFAULT: Self = Self(0);
}

/// A run of text in one style, ending at byte `end` of its line. A run starts
/// where the previous one ended; text past the last run is unstyled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub end: u32,
    pub style: StyleId,
}

/// Applies the parameters of an `SGR` (`CSI ... m`) sequence to `style`.
/// Colors take both the `38;5;n` and the `38:5:n` spellings; parameters it
/// does not know are skipped.
pub(crate) fn apply_sgr(style: &mut Style, params: &Params) {
    let mut iter = params.iter();
    let mut any = false;
    while let Some(param) = iter.next() {
        any = true;
        let code = param.first().copied().unwrap_or(0);
        match code {
            0 => *style = Style::default(),
            1 => style.attrs.set(Attrs::BOLD, true),
            2 => style.attrs.set(Attrs::DIM, true),
            3 => style.attrs.set(Attrs::ITALIC, true),
            4 => style.attrs.set(Attrs::UNDERLINE, param.get(1) != Some(&0)),
            7 => style.attrs.set(Attrs::INVERSE, true),
            21 => style.attrs.set(Attrs::UNDERLINE, true),
            22 => {
                style.attrs.set(Attrs::BOLD, false);
                style.attrs.set(Attrs::DIM, false);
            }
            23 => style.attrs.set(Attrs::ITALIC, false),
            24 => style.attrs.set(Attrs::UNDERLINE, false),
            27 => style.attrs.set(Attrs::INVERSE, false),
            30..=37 => style.fg = Color::Indexed((code - 30) as u8),
            38 => {
                if let Some(color) = extended(param, &mut iter) {
                    style.fg = color;
                }
            }
            39 => style.fg = Color::Default,
            40..=47 => style.bg = Color::Indexed((code - 40) as u8),
            48 => {
                if let Some(color) = extended(param, &mut iter) {
                    style.bg = color;
                }
            }
            49 => style.bg = Color::Default,
            90..=97 => style.fg = Color::Indexed((code - 90) as u8 + 8),
            100..=107 => style.bg = Color::Indexed((code - 100) as u8 + 8),
            _ => {}
        }
    }
    if !any {
        *style = Style::default();
    }
}

/// The color after a `38` or `48`: in the same parameter after colons, or in
/// the next ones after semicolons.
fn extended<'a>(param: &[u16], rest: &mut impl Iterator<Item = &'a [u16]>) -> Option<Color> {
    let mut next = || rest.next().and_then(|p| p.first().copied());
    let byte = |v: u16| u8::try_from(v).ok();
    if param.len() > 1 {
        return match &param[1..] {
            &[5, n] => Some(Color::Indexed(byte(n)?)),
            &[2, r, g, b] | &[2, _, r, g, b] => Some(Color::Rgb(byte(r)?, byte(g)?, byte(b)?)),
            _ => None,
        };
    }
    match next()? {
        5 => Some(Color::Indexed(byte(next()?)?)),
        2 => {
            let (r, g, b) = (next()?, next()?, next()?);
            Some(Color::Rgb(byte(r)?, byte(g)?, byte(b)?))
        }
        _ => None,
    }
}
