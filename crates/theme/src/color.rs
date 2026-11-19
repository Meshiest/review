//! sRGB colors as theme files write them, plus the WCAG 2.x contrast math.

use std::fmt;

use gpui::{Hsla, Rgba};

/// An sRGB color with alpha, 8 bits per channel: `#rrggbb` or `#rrggbbaa`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const BLACK: Self = Self::opaque(0, 0, 0);
    pub const WHITE: Self = Self::opaque(255, 255, 255);

    #[must_use]
    pub const fn opaque(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    /// Parses `#rrggbb` or `#rrggbbaa` (either case).
    ///
    /// # Errors
    ///
    /// A message for the theme error when the text is not one of those forms.
    pub fn parse(text: &str) -> Result<Self, String> {
        let invalid = || format!("expected \"#rrggbb\" or \"#rrggbbaa\", found {text:?}");
        let hex = text.strip_prefix('#').ok_or_else(invalid)?;
        if !matches!(hex.len(), 6 | 8) || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(invalid());
        }
        let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).map_err(|_| invalid());
        Ok(Self {
            r: byte(0)?,
            g: byte(2)?,
            b: byte(4)?,
            a: if hex.len() == 8 { byte(6)? } else { 255 },
        })
    }

    #[must_use]
    pub fn alpha(self) -> f32 {
        f32::from(self.a) / 255.0
    }

    #[must_use]
    pub fn with_alpha(self, alpha: f32) -> Self {
        Self {
            a: unit_to_byte(alpha),
            ..self
        }
    }

    /// This color painted over `background` (source-over), as an opaque color.
    #[must_use]
    pub fn over(self, background: Self) -> Self {
        let a = self.alpha();
        let channel =
            |fg: u8, bg: u8| unit_to_byte((f32::from(fg) * a + f32::from(bg) * (1.0 - a)) / 255.0);
        Self::opaque(
            channel(self.r, background.r),
            channel(self.g, background.g),
            channel(self.b, background.b),
        )
    }

    /// WCAG 2.x relative luminance of the color's RGB (alpha ignored).
    #[must_use]
    pub fn relative_luminance(self) -> f64 {
        let linear = |c: u8| {
            let c = f64::from(c) / 255.0;
            if c <= 0.040_45 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(self.r) + 0.7152 * linear(self.g) + 0.0722 * linear(self.b)
    }

    /// WCAG 2.x contrast ratio of this color, composited over `background`,
    /// against `background` (itself flattened over black if translucent).
    #[must_use]
    pub fn contrast_on(self, background: Self) -> f64 {
        let background = background.over(Self::BLACK);
        let (a, b) = (
            self.over(background).relative_luminance(),
            background.relative_luminance(),
        );
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    #[must_use]
    pub fn to_hsla(self) -> Hsla {
        Rgba {
            r: f32::from(self.r) / 255.0,
            g: f32::from(self.g) / 255.0,
            b: f32::from(self.b) / 255.0,
            a: self.alpha(),
        }
        .into()
    }
}

impl From<Color> for Hsla {
    fn from(color: Color) -> Self {
        color.to_hsla()
    }
}

impl fmt::Display for Color {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{:02x}{:02x}{:02x}", self.r, self.g, self.b)?;
        if self.a != 255 {
            write!(f, "{:02x}", self.a)?;
        }
        Ok(())
    }
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "clamped to 0..=255 first"
)]
fn unit_to_byte(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// `a` mixed with `t` of `b` in sRGB, like CSS `color-mix(in srgb, ...)`;
/// the alpha of `a` is kept.
#[must_use]
pub fn mix(a: Hsla, b: Hsla, t: f32) -> Hsla {
    let (a, b) = (a.to_rgb(), b.to_rgb());
    Rgba {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: a.a,
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_forms_and_rejects_others() {
        assert_eq!(Color::parse("#0A0a0a"), Ok(Color::opaque(10, 10, 10)));
        assert_eq!(
            Color::parse("#ffffff17"),
            Ok(Color {
                r: 255,
                g: 255,
                b: 255,
                a: 0x17
            })
        );
        for bad in ["0a0a0a", "#fff", "#0a0a0a0", "#gggggg", "", "#ffffff1"] {
            assert!(Color::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn contrast_matches_wcag_reference_values() {
        let ratio = Color::BLACK.contrast_on(Color::WHITE);
        assert!((ratio - 21.0).abs() < 1e-9);
        // #767676 on white is the classic 4.54:1 gray.
        let gray = Color::parse("#767676").expect("valid");
        assert!((gray.contrast_on(Color::WHITE) - 4.54).abs() < 0.01);
    }

    #[test]
    fn translucent_text_is_composited_first() {
        let half_white = Color::WHITE.with_alpha(0.5);
        let on_black = half_white.contrast_on(Color::BLACK);
        let flat = Color::opaque(128, 128, 128).contrast_on(Color::BLACK);
        assert!((on_black - flat).abs() < 1e-9);
    }
}
