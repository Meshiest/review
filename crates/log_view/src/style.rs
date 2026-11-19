use gpui::{FontFallbacks, Hsla, Pixels, Rgba, SharedString};
use scroll::{AutoscrollStyle, ScrollStyle, ScrollbarStyle};
use theme::Theme;

use crate::ansi::Color;

/// How much of its color dim text keeps.
const DIM_ALPHA: f32 = 0.66;

/// Colors, font and metrics of the log view. Plain data with no defaults: the
/// caller builds it, normally with [`LogStyle::from_theme`].
#[derive(Clone, Debug, PartialEq)]
pub struct LogStyle {
    /// A monospace family.
    pub font_family: SharedString,
    pub font_fallbacks: Option<FontFallbacks>,
    pub font_size: Pixels,
    pub line_height: Pixels,
    /// Columns a tab advances to.
    pub tab_width: u32,

    pub background: Hsla,
    /// Text without a color of its own.
    pub foreground: Hsla,
    /// Colors 0-7 then the bright 8-15, for `30..37`, `90..97` and 256-color
    /// indexes below 16.
    pub ansi: [Hsla; 16],
    pub gutter_background: Hsla,
    pub gutter_text: Hsla,
    /// The time column's text.
    pub time_text: Hsla,
    pub group_background: Hsla,
    pub group_text: Hsla,
    /// The "earlier lines dropped" row.
    pub notice_text: Hsla,
    pub error_text: Hsla,
    pub warning_text: Hsla,
    pub selection: Hsla,
    pub match_background: Hsla,
    pub current_match_background: Hsla,
    /// The search bar and the jump pill.
    pub overlay_background: Hsla,
    pub overlay_border: Hsla,
    pub overlay_text: Hsla,
    pub overlay_muted: Hsla,
    pub accent: Hsla,
    pub accent_text: Hsla,
    pub radius: Pixels,
    pub scrollbar: ScrollbarStyle,
    pub autoscroll: AutoscrollStyle,
}

impl LogStyle {
    /// The terminal palette and buffer font of `theme`, as `term_view` takes
    /// them, with the surface colors for the chrome around the text.
    #[must_use]
    pub fn from_theme(theme: &Theme) -> Self {
        let (c, t, font) = (&theme.colors, &theme.terminal, &theme.fonts.buffer);
        Self {
            font_family: font.family(),
            font_fallbacks: font.font_fallbacks(),
            font_size: font.size_px(),
            line_height: font.line_height_px(),
            tab_width: 8,
            background: t.background,
            foreground: t.foreground,
            ansi: t.ansi(),
            gutter_background: c.gutter_bg,
            gutter_text: c.gutter_text,
            time_text: c.text_muted,
            group_background: c.surface_raised,
            group_text: c.text,
            notice_text: c.text_muted,
            error_text: t.red,
            warning_text: t.yellow,
            selection: c.selection,
            match_background: Hsla { a: 0.35, ..c.warn },
            current_match_background: Hsla { a: 0.75, ..c.warn },
            overlay_background: c.surface_raised,
            overlay_border: c.border_strong,
            overlay_text: c.text,
            overlay_muted: c.text_muted,
            accent: c.accent,
            accent_text: c.accent_text,
            radius: theme.radius.md,
            scrollbar: ScrollbarStyle {
                thumb: c.border_strong,
                ..ScrollbarStyle::default()
            },
            autoscroll: AutoscrollStyle::default(),
        }
    }

    /// The scroll component's share of the style.
    #[must_use]
    pub fn scroll_style(&self) -> ScrollStyle {
        ScrollStyle {
            scrollbar: self.scrollbar,
            autoscroll: self.autoscroll,
        }
    }

    /// The color a log color stands for; `None` for the default. Bold
    /// brightens the base eight, as terminals do.
    pub(crate) fn resolve(&self, color: Color, bold: bool) -> Option<Hsla> {
        match color {
            Color::Default => None,
            Color::Rgb(r, g, b) => Some(from_bytes(r, g, b)),
            Color::Indexed(index) => Some(self.indexed(index, bold)),
        }
    }

    fn indexed(&self, index: u8, bold: bool) -> Hsla {
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

/// Dim text: the same color, less of it.
pub(crate) fn dim(color: Hsla) -> Hsla {
    Hsla {
        a: color.a * DIM_ALPHA,
        ..color
    }
}

/// An opaque color from 8-bit channels.
fn from_bytes(r: u8, g: u8, b: u8) -> Hsla {
    let unit = |v: u8| f32::from(v) / 255.0;
    Rgba {
        r: unit(r),
        g: unit(g),
        b: unit(b),
        a: 1.0,
    }
    .into()
}

#[cfg(test)]
pub(crate) fn test_style() -> LogStyle {
    let registry = theme::ThemeRegistry::builtin();
    LogStyle::from_theme(&registry.themes()[0])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexed_colors_cover_palette_cube_and_gray_ramp() {
        let style = test_style();
        assert_eq!(style.resolve(Color::Indexed(1), false), Some(style.ansi[1]));
        assert_eq!(style.resolve(Color::Indexed(1), true), Some(style.ansi[9]));
        assert_eq!(style.resolve(Color::Indexed(9), true), Some(style.ansi[9]));
        assert_eq!(
            style.resolve(Color::Indexed(16), false),
            Some(from_bytes(0, 0, 0))
        );
        assert_eq!(
            style.resolve(Color::Indexed(208), false),
            Some(from_bytes(255, 135, 0))
        );
        assert_eq!(
            style.resolve(Color::Indexed(255), false),
            Some(from_bytes(238, 238, 238))
        );
        assert_eq!(style.resolve(Color::Default, true), None);
        assert_eq!(
            style.resolve(Color::Rgb(1, 2, 3), false),
            Some(from_bytes(1, 2, 3))
        );
    }

    #[test]
    fn the_theme_sets_palette_and_font() {
        let registry = theme::ThemeRegistry::builtin();
        let theme = &registry.themes()[0];
        let style = LogStyle::from_theme(theme);
        assert_eq!(style.ansi, theme.terminal.ansi());
        assert_eq!(style.font_family, theme.fonts.buffer.family());
        assert_eq!(style.line_height, theme.fonts.buffer.line_height_px());
    }
}
