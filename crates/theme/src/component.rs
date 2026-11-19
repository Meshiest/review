//! Maps a [`Theme`] onto gpui-component's theme, so its widgets draw in the
//! theme's colors. Every value here comes from a theme token; the mixes only
//! make hover and pressed steps of a token.

use gpui::{App, Hsla, hsla};
use gpui_kit::component::{Theme as ComponentTheme, ThemeColor, ThemeMode};

use crate::color::mix;
use crate::runtime::Theme;

/// How far hover and pressed states move a solid fill.
const STEP: f32 = 0.12;

pub(crate) fn apply(cx: &mut App, theme: &Theme) {
    let mode = if theme.is_dark() {
        ThemeMode::Dark
    } else {
        ThemeMode::Light
    };
    // `change` loads the registered theme for the mode; edits come after it so
    // nothing is loaded over them.
    ComponentTheme::change(mode, None, cx);
    ComponentTheme::update(cx, |t| {
        map_colors(&mut t.colors, theme);
        t.font_family = theme.fonts.interface.family();
        t.font_size = theme.fonts.interface.size_px();
        t.mono_font_family = theme.fonts.buffer.family();
        t.mono_font_size = theme.fonts.buffer.size_px();
        t.radius = theme.radius.md;
        t.radius_lg = theme.radius.lg;
        t.shadow = false;
    });
}

fn white() -> Hsla {
    hsla(0.0, 0.0, 1.0, 1.0)
}

fn black() -> Hsla {
    hsla(0.0, 0.0, 0.0, 1.0)
}

/// `(hover, pressed)` for a solid fill: lighter then darker for a dark fill,
/// the reverse for a light one.
fn steps(fill: Hsla) -> (Hsla, Hsla) {
    if fill.l < 0.6 {
        (mix(fill, white(), STEP), mix(fill, black(), STEP))
    } else {
        (mix(fill, black(), STEP), mix(fill, black(), 2.0 * STEP))
    }
}

fn map_colors(t: &mut ThemeColor, theme: &Theme) {
    let c = &theme.colors;
    let clear = Hsla::transparent_black();
    let tint = |color: Hsla| color.opacity(theme.chips.fill_alpha);

    t.background = c.surface;
    t.foreground = c.text;
    t.border = c.border;
    t.muted = c.surface_raised;
    t.muted_foreground = c.text_muted;
    t.accent = c.hover;
    t.accent_foreground = c.text;
    t.ring = c.focus;
    t.caret = c.accent_fg;
    t.selection = c.selection;
    t.link = c.link;
    t.link_hover = c.focus;
    t.link_active = c.focus;
    t.overlay = black().opacity(0.5);
    t.window_border = c.border_strong;

    t.popover = c.surface_menu;
    t.popover_foreground = c.text;

    let (primary_hover, primary_active) = steps(c.accent);
    t.primary = c.accent;
    t.primary_hover = primary_hover;
    t.primary_active = primary_active;
    t.primary_foreground = c.accent_text;
    t.secondary = c.surface_raised;
    t.secondary_hover = c.hover;
    t.secondary_active = c.active;
    t.secondary_foreground = c.text;

    t.button = c.surface_raised;
    t.button_hover = c.hover;
    t.button_active = c.active;
    t.button_foreground = c.text;
    t.button_primary = t.primary;
    t.button_primary_hover = t.primary_hover;
    t.button_primary_active = t.primary_active;
    t.button_primary_foreground = t.primary_foreground;
    t.button_secondary = t.secondary;
    t.button_secondary_hover = t.secondary_hover;
    t.button_secondary_active = t.secondary_active;
    t.button_secondary_foreground = t.secondary_foreground;

    // Checkbox outlines use this too, so it is the 3:1 control border.
    t.input = c.control_border;
    t.list = clear;
    t.list_even = clear;
    t.list_head = c.surface_raised;
    t.list_hover = c.hover;
    t.list_active = c.selected;
    t.list_active_border = c.focus;
    t.list_highlight_foreground = c.accent_fg;

    t.tab = clear;
    t.tab_bar = c.surface_raised;
    t.tab_bar_segmented = c.field;
    t.tab_active = c.surface;
    t.tab_active_foreground = c.text;
    t.tab_foreground = c.text_muted;

    t.title_bar = c.surface_raised;
    t.title_bar_border = c.border;
    t.status_bar = c.surface_raised;
    t.status_bar_border = c.border;

    t.sidebar = c.surface;
    t.sidebar_foreground = c.text;
    t.sidebar_border = c.border;
    t.sidebar_accent = c.hover;
    t.sidebar_accent_foreground = c.text;
    t.sidebar_primary = c.accent;
    t.sidebar_primary_foreground = c.accent_text;

    t.scrollbar = clear;
    t.scrollbar_thumb = c.border_strong;
    t.scrollbar_thumb_hover = c.text_muted;

    t.drag_border = c.focus;
    t.drop_target = c.selected;

    t.table = c.surface;
    t.table_head = c.surface_raised;
    t.table_head_foreground = c.text_muted;
    t.table_even = clear;
    t.table_hover = c.hover;
    t.table_active = c.selected;
    t.table_active_border = c.focus;
    t.table_row_border = c.border;
    t.table_foot = c.surface_raised;
    t.table_foot_foreground = c.text_muted;

    t.group_box = c.surface_raised;
    t.group_box_foreground = c.text;
    t.accordion = c.surface;
    t.skeleton = c.surface_raised;
    t.progress_bar = c.accent;
    t.slider_bar = c.accent;
    t.slider_thumb = c.accent;
    t.switch = c.border_strong;
    t.switch_thumb = c.text;
    t.description_list_label = c.surface_raised;
    t.description_list_label_foreground = c.text_muted;

    (t.danger, (t.danger_hover, t.danger_active)) = (c.danger, steps(c.danger));
    (t.success, (t.success_hover, t.success_active)) = (c.ok, steps(c.ok));
    (t.warning, (t.warning_hover, t.warning_active)) = (c.warn, steps(c.warn));
    (t.info, (t.info_hover, t.info_active)) = (c.info, steps(c.info));
    t.danger_foreground = c.state_text;
    t.success_foreground = c.state_text;
    t.warning_foreground = c.state_text;
    t.info_foreground = c.state_text;

    t.red = c.danger;
    t.red_light = tint(c.danger);
    t.green = c.ok;
    t.green_light = tint(c.ok);
    t.yellow = c.warn;
    t.yellow_light = tint(c.warn);
    t.blue = c.info;
    t.blue_light = tint(c.info);
    t.cyan = c.accent_2;
    t.cyan_light = tint(c.accent_2);
    t.magenta = c.accent;
    t.magenta_light = c.selected;
}
