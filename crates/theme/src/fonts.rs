//! Font settings and the bundled Geist families (SIL OFL 1.1; see
//! `assets/fonts/README.md`).

use std::borrow::Cow;

use gpui::{App, Font, FontFallbacks, Pixels, SharedString, font, px};
use serde::{Deserialize, Serialize};

/// The Vercel theme's interface family, bundled.
pub const GEIST: &str = "Geist";
/// The Vercel theme's buffer family, bundled.
pub const GEIST_MONO: &str = "Geist Mono";

/// An interface or buffer font as settings store it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FontSettings {
    pub family: String,
    /// px.
    pub size: f32,
    /// Whole px, so a focused input never shifts its text by a sub-pixel.
    pub line_height: u32,
    /// Families tried in order for glyphs `family` lacks.
    #[serde(default)]
    pub fallbacks: Vec<String>,
}

impl FontSettings {
    #[must_use]
    pub fn family(&self) -> SharedString {
        SharedString::from(self.family.clone())
    }

    #[must_use]
    pub fn size_px(&self) -> Pixels {
        px(self.size)
    }

    #[must_use]
    #[expect(clippy::cast_precision_loss, reason = "line heights are at most 200")]
    pub fn line_height_px(&self) -> Pixels {
        px(self.line_height as f32)
    }

    /// The fallbacks for gpui, or `None` when there are none.
    #[must_use]
    pub fn font_fallbacks(&self) -> Option<FontFallbacks> {
        (!self.fallbacks.is_empty()).then(|| FontFallbacks::from_fonts(self.fallbacks.clone()))
    }

    /// A gpui font of this family at normal weight, with the fallbacks.
    #[must_use]
    pub fn font(&self) -> Font {
        Font {
            fallbacks: self.font_fallbacks(),
            ..font(self.family())
        }
    }
}

/// Adds the bundled Geist and Geist Mono faces to gpui's text system. Call
/// once before opening a window.
///
/// # Panics
///
/// If gpui rejects the embedded fonts, which a test rules out.
pub fn register(cx: &App) {
    cx.text_system()
        .add_fonts(bundled())
        .expect("the bundled Geist fonts are valid TrueType");
}

fn bundled() -> Vec<Cow<'static, [u8]>> {
    [
        include_bytes!("../assets/fonts/Geist-Regular.ttf").as_slice(),
        include_bytes!("../assets/fonts/Geist-Medium.ttf").as_slice(),
        include_bytes!("../assets/fonts/Geist-SemiBold.ttf").as_slice(),
        include_bytes!("../assets/fonts/GeistMono-Regular.ttf").as_slice(),
        include_bytes!("../assets/fonts/GeistMono-Medium.ttf").as_slice(),
    ]
    .into_iter()
    .map(Cow::Borrowed)
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_fonts_are_truetype() {
        for bytes in bundled() {
            assert_eq!(&bytes[..4], &[0, 1, 0, 0], "sfnt version 1.0");
        }
    }
}
