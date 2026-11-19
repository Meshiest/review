//! Label chips in the forge's own color, made readable in the theme: the
//! label's color tints the fill by the theme's chip recipe, the text is the
//! label's color moved toward the theme's text color until it reads on that
//! fill (WCAG 4.5:1), and the border is that text at the recipe's border
//! strength. Nothing here names a color.

use gpui::{Div, Hsla, ParentElement as _, Rgba, SharedString, Styled as _, div, px};
use theme::{ChipColors, Color, Theme};

use super::{TextExt as _, theme};

/// The contrast label text must reach on its chip.
pub const TEXT_CONTRAST: f64 = 4.5;

fn to_color(color: Hsla) -> Color {
    let Rgba { r, g, b, a } = color.to_rgb();
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "clamped to 0..=255 first"
    )]
    let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    Color {
        r: byte(r),
        g: byte(g),
        b: byte(b),
        a: byte(a),
    }
}

/// The contrast of `text` on `fill` painted over `under`.
#[must_use]
pub fn contrast(text: Hsla, fill: Hsla, under: Hsla) -> f64 {
    let fill = to_color(fill).over(to_color(under));
    to_color(text).contrast_on(fill)
}

/// The forge's `rrggbb` color, if it is one.
#[must_use]
pub fn forge_color(hex: Option<&str>) -> Option<Hsla> {
    let hex = hex?.trim().trim_start_matches('#');
    Color::parse(&format!("#{hex}"))
        .ok()
        .map(|color| Color { a: 255, ..color }.to_hsla())
}

/// Fill, border and text of a chip for a label colored `color`, on `under`
/// (the surface it sits on). A label without a color uses the muted text.
#[must_use]
pub fn label_colors(t: &Theme, color: Option<Hsla>, under: Hsla) -> ChipColors {
    let base = color.unwrap_or(t.colors.text_muted);
    let fill = base.opacity(t.chips.fill_alpha);
    let text = (0..=10)
        .map(|step: u8| theme::mix(base, t.colors.text, f32::from(step) / 10.0))
        .find(|text| contrast(*text, fill, under) >= TEXT_CONTRAST)
        .unwrap_or(t.colors.text);
    // From the readable text, so a label as dark as the surface (or as
    // light) still has an edge.
    let border = text.opacity(t.chips.border_alpha);
    ChipColors { fill, border, text }
}

/// A label as a chip in its forge color.
pub fn label_chip(cx: &gpui::App, name: impl Into<SharedString>, color: Option<&str>) -> Div {
    let t = theme(cx);
    let colors = label_colors(&t, forge_color(color), t.colors.surface);
    div()
        .flex_none()
        .flex()
        .items_center()
        .h(px(20.0))
        .px(px(8.0))
        .rounded_full()
        .border_1()
        .border_color(colors.border)
        .bg(colors.fill)
        .text_color(colors.text)
        .tiny(&t)
        .whitespace_nowrap()
        .child(name.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use theme::ThemeRegistry;

    #[test]
    fn every_label_color_reads_on_its_chip_in_every_theme() {
        let mut colors: Vec<String> = [
            "d73a4a", "0075ca", "cfd3d7", "a2eeef", "7057ff", "008672", "e4e669", "d876e3",
            "ffffff", "000000", "fbca04", "b60205", "0e8a16", "1d76db", "5319e7", "c2e0c6",
        ]
        .map(str::to_owned)
        .to_vec();
        // A sweep of hues and lightness, so no forge color is left out.
        for r in (0..=255).step_by(51) {
            for g in (0..=255).step_by(51) {
                for b in (0..=255).step_by(51) {
                    colors.push(format!("{r:02x}{g:02x}{b:02x}"));
                }
            }
        }
        for t in ThemeRegistry::builtin().themes() {
            for under in [t.colors.surface, t.colors.surface_raised] {
                for hex in &colors {
                    let color = forge_color(Some(hex));
                    assert!(color.is_some(), "{hex} parses");
                    let chip = label_colors(t, color, under);
                    let ratio = contrast(chip.text, chip.fill, under);
                    assert!(
                        ratio >= TEXT_CONTRAST,
                        "{}: label {hex} text {ratio:.2}:1",
                        t.id
                    );
                }
                let none = label_colors(t, None, under);
                assert!(contrast(none.text, none.fill, under) >= TEXT_CONTRAST);
            }
        }
    }

    #[test]
    fn the_text_keeps_the_labels_hue_when_it_already_reads() {
        let t = ThemeRegistry::builtin().themes()[0].clone();
        let color = forge_color(Some("#d73a4a")).expect("parses");
        let chip = label_colors(&t, Some(color), t.colors.surface);
        assert!((chip.text.h - color.h).abs() < 0.05, "still red: {chip:?}");
        assert_eq!(forge_color(Some("not a color")), None);
        assert_eq!(forge_color(None), None);
    }
}
