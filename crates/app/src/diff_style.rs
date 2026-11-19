//! Builds `diff_view`'s style from the active theme: the buffer font, the
//! theme's diff tints, gutter, hunk and empty-side tokens, and its syntax
//! colors. Nothing here picks a color; every value is a theme token.

use diff_view::{DiffStyle, SyntaxPalette};
use gpui::px;
use theme::{Syntax, Theme};

pub fn diff_style(theme: &Theme) -> DiffStyle {
    let c = &theme.colors;
    let buffer = &theme.fonts.buffer;
    DiffStyle {
        font_family: buffer.family(),
        font_size: buffer.size_px(),
        line_height: buffer.line_height_px(),
        file_header_height: px(32.0),
        tab_width: 4,
        background: c.surface,
        text: theme.syntax.plain,
        gutter_background: c.gutter_bg,
        gutter_text: c.gutter_text,
        added_background: c.diff_add_bg,
        added_gutter_background: c.diff_add_gutter,
        removed_background: c.diff_remove_bg,
        removed_gutter_background: c.diff_remove_gutter,
        add_emphasis: c.diff_add_emphasis,
        remove_emphasis: c.diff_remove_emphasis,
        selection: c.selection,
        file_header_dim: c.text_muted,
        checkbox_border: c.control_border,
        checkbox_checked: c.accent,
        checkbox_mark: c.accent_text,
        stat_added: c.ok,
        stat_removed: c.danger,
        stat_empty: c.border_strong,
        divider_active: c.accent,
        empty_background: c.diff_empty_bg,
        file_header_background: c.surface_raised,
        file_header_text: c.text,
        hunk_header_background: c.diff_hunk_bg,
        hunk_header_text: c.diff_hunk_text,
        block_background: c.surface_raised,
        block_border: c.border_strong,
        divider: c.border,
        scrollbar: crate::ui::scroll_style(theme).scrollbar,
        autoscroll: crate::ui::scroll_style(theme).autoscroll,
        palette: palette(&theme.syntax),
    }
}

fn palette(syntax: &Syntax) -> SyntaxPalette {
    SyntaxPalette {
        keyword: syntax.keyword,
        function: syntax.function,
        type_: syntax.type_,
        string: syntax.string,
        number: syntax.number,
        comment: syntax.comment,
        constant: syntax.constant,
        operator: syntax.operator,
        punctuation: syntax.punctuation,
        property: syntax.property,
        variable: syntax.variable,
        attribute: syntax.attribute,
        tag: syntax.tag,
        plain: syntax.plain,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use theme::ThemeRegistry;

    #[test]
    fn every_builtin_theme_maps_its_tokens() {
        for theme in ThemeRegistry::builtin().themes() {
            let style = diff_style(theme);
            assert_eq!(style.background, theme.colors.surface, "{}", theme.id);
            assert_eq!(style.text, theme.syntax.plain, "{}", theme.id);
            assert_eq!(style.palette.plain, style.text, "{}", theme.id);
            assert_eq!(style.palette.keyword, theme.syntax.keyword, "{}", theme.id);
            assert_eq!(style.added_background, theme.colors.diff_add_bg);
            assert_eq!(
                style.removed_gutter_background,
                theme.colors.diff_remove_gutter
            );
            assert_eq!(style.empty_background, theme.colors.diff_empty_bg);
            assert_eq!(style.hunk_header_text, theme.colors.diff_hunk_text);
            assert_eq!(style.font_family, theme.fonts.buffer.family());
            assert_eq!(style.line_height, theme.fonts.buffer.line_height_px());
        }
    }

    #[test]
    fn changed_rows_are_tints_not_saturated_blocks() {
        for theme in ThemeRegistry::builtin().themes() {
            let style = diff_style(theme);
            for tint in [style.added_background, style.removed_background] {
                assert_eq!(tint.a, 1.0, "tints are opaque, so they paint flat");
                assert!(
                    (tint.l - theme.colors.surface.l).abs() < 0.2,
                    "{}: a row tint stays close to the surface in lightness",
                    theme.id
                );
                assert_ne!(tint, theme.colors.surface);
            }
        }
    }
}
