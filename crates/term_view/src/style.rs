use crate::glyphs::SYMBOLS_FAMILY;
use gpui::{FontFallbacks, Hsla, Pixels, Rgba, SharedString};
use scroll::{ScrollStyle, ScrollbarStyle};
use term::alacritty_terminal::vte::ansi::{Color, NamedColor};
use theme::{FontSettings, Terminal, Theme};

/// How opaque the selection is when the theme gives no color of its own.
const SELECTION_ALPHA: f32 = 0.3;

/// Font, size and colors of a terminal, built from a theme's `[terminal]` table.
#[derive(Clone, Debug)]
pub struct TermStyle {
    pub font_family: SharedString,
    /// Families tried, in order, for glyphs the primary font lacks. The symbols
    /// font is also routed explicitly, since DirectWrite looks these names up
    /// in the system collection only.
    pub font_fallbacks: Option<FontFallbacks>,
    pub font_size: Pixels,
    /// Row height as a multiple of the font size.
    pub line_height: f32,
    pub foreground: Rgba,
    pub background: Rgba,
    pub cursor: Rgba,
    /// Colors 0-7 then the bright 8-15.
    pub ansi: [Rgba; 16],
    /// Behind selected text.
    pub selection: Hsla,
    /// The scrollback scrollbar and the autoscroll marker.
    pub scroll: ScrollStyle,
    /// Space between the pane's edges and the grid, on every side. The
    /// background fills it; a press in it takes the nearest cell.
    pub padding: Pixels,
}

impl TermStyle {
    /// A style from a theme's `[terminal]` table and its buffer font. The font's
    /// own fallbacks come after the symbols font.
    #[must_use]
    pub fn new(terminal: &Terminal, font: &FontSettings) -> Self {
        let fallbacks = std::iter::once(SYMBOLS_FAMILY.to_owned())
            .chain(font.fallbacks.iter().cloned())
            .collect();
        Self {
            font_family: font.family(),
            font_fallbacks: Some(FontFallbacks::from_fonts(fallbacks)),
            font_size: font.size_px(),
            line_height: font.line_height as f32 / font.size,
            foreground: terminal.foreground.into(),
            background: terminal.background.into(),
            cursor: terminal.cursor.into(),
            ansi: terminal.ansi().map(Rgba::from),
            selection: Hsla {
                a: SELECTION_ALPHA,
                ..Hsla::from(Rgba::from(terminal.foreground))
            },
            scroll: ScrollStyle::default(),
            padding: Pixels::ZERO,
        }
    }

    /// [`Self::new`] with a theme's terminal colors and buffer font, and its
    /// selection and scrollbar colors.
    #[must_use]
    pub fn from_theme(theme: &Theme) -> Self {
        let mut style = Self::new(&theme.terminal, &theme.fonts.buffer);
        style.selection = theme.colors.selection;
        style.scroll.scrollbar = ScrollbarStyle {
            thumb: theme.colors.border_strong,
            ..ScrollbarStyle::default()
        };
        style
    }

    /// The RGB value of a terminal color. `bold` brightens the base eight
    /// colors, as terminals traditionally do.
    pub(crate) fn resolve(&self, color: Color, bold: bool) -> Rgba {
        match color {
            Color::Spec(c) => from_bytes(c.r, c.g, c.b),
            Color::Indexed(index) => self.indexed(index, bold),
            Color::Named(named) => self.named(named, bold),
        }
    }

    fn named(&self, named: NamedColor, bold: bool) -> Rgba {
        let index = named as usize;
        match named {
            NamedColor::Foreground | NamedColor::BrightForeground => self.foreground,
            NamedColor::Background => self.background,
            NamedColor::Cursor => self.cursor,
            NamedColor::DimForeground => dim(self.foreground),
            _ if index < 16 => self.indexed(index as u8, bold),
            // DimBlack.. are 259..=266.
            _ => dim(self.ansi[index.saturating_sub(259).min(7)]),
        }
    }

    fn indexed(&self, index: u8, bold: bool) -> Rgba {
        match index {
            0..=7 if bold => self.ansi[usize::from(index) + 8],
            0..=15 => self.ansi[usize::from(index)],
            16..=231 => {
                // The xterm 6x6x6 cube: fixed by the protocol, not by a theme.
                const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
                let i = usize::from(index - 16);
                from_bytes(LEVELS[i / 36], LEVELS[i / 6 % 6], LEVELS[i % 6])
            }
            232..=255 => {
                let v = 8 + 10 * (index - 232);
                from_bytes(v, v, v)
            }
        }
    }
}

/// An opaque color from 8-bit channels.
pub(crate) fn from_bytes(r: u8, g: u8, b: u8) -> Rgba {
    let unit = |v: u8| f32::from(v) / 255.0;
    Rgba {
        r: unit(r),
        g: unit(g),
        b: unit(b),
        a: 1.0,
    }
}

pub(crate) fn dim(color: Rgba) -> Rgba {
    Rgba {
        r: color.r * 0.66,
        g: color.g * 0.66,
        b: color.b * 0.66,
        a: color.a,
    }
}

/// The style of the default theme, for tests that only need some palette.
#[cfg(test)]
pub(crate) fn test_style() -> TermStyle {
    let registry = theme::ThemeRegistry::builtin();
    TermStyle::from_theme(&registry.themes()[0])
}

#[cfg(test)]
mod tests {
    use super::*;
    use theme::ThemeRegistry;

    #[test]
    fn indexed_colors_cover_palette_cube_and_gray_ramp() {
        let style = test_style();
        assert_eq!(style.resolve(Color::Indexed(1), false), style.ansi[1]);
        assert_eq!(style.resolve(Color::Indexed(1), true), style.ansi[9]);
        assert_eq!(style.resolve(Color::Indexed(9), true), style.ansi[9]);
        assert_eq!(
            style.resolve(Color::Indexed(16), false),
            from_bytes(0, 0, 0)
        );
        assert_eq!(
            style.resolve(Color::Indexed(208), false),
            from_bytes(255, 135, 0)
        );
        assert_eq!(
            style.resolve(Color::Indexed(231), false),
            from_bytes(255, 255, 255)
        );
        assert_eq!(
            style.resolve(Color::Indexed(232), false),
            from_bytes(8, 8, 8)
        );
        assert_eq!(
            style.resolve(Color::Indexed(255), false),
            from_bytes(238, 238, 238)
        );
    }

    #[test]
    fn named_colors_map_to_the_palette() {
        let style = test_style();
        assert_eq!(
            style.resolve(Color::Named(NamedColor::Red), false),
            style.ansi[1]
        );
        assert_eq!(
            style.resolve(Color::Named(NamedColor::BrightRed), false),
            style.ansi[9]
        );
        assert_eq!(
            style.resolve(Color::Named(NamedColor::Foreground), true),
            style.foreground
        );
        assert_eq!(
            style.resolve(Color::Named(NamedColor::Background), false),
            style.background
        );
    }

    #[test]
    fn a_theme_sets_colors_font_and_row_height() {
        let registry = ThemeRegistry::builtin();
        let theme = &registry.themes()[0];
        let style = TermStyle::from_theme(theme);
        assert_eq!(style.ansi, theme.terminal.ansi().map(Rgba::from));
        assert_eq!(Rgba::from(theme.terminal.cursor), style.cursor);
        assert_eq!(style.font_family, theme.fonts.buffer.family());
        let row = f32::from(style.font_size) * style.line_height;
        assert!((row - theme.fonts.buffer.line_height as f32).abs() < 1e-3);
    }
}
