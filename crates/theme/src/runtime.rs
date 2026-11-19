//! The resolved theme in gpui types, the global that holds the active one, and
//! [`apply`], which installs it.

use std::sync::Arc;

use gpui::{App, Global, Hsla, SharedString, px};

use crate::component;
use crate::file::{Mode, ThemeData, ThemeFonts};
use crate::fonts::FontSettings;
use crate::tokens::{Chips, Colors, Radius, Syntax, Terminal};

/// A theme ready to draw with. Views read it through [`current`] or
/// [`CurrentTheme`]; they never hold color literals.
#[derive(Clone, Debug, PartialEq)]
pub struct Theme {
    pub id: String,
    pub name: String,
    pub mode: Mode,
    pub radius: Radius,
    /// The effective fonts: the theme's defaults until [`Theme::with_fonts`].
    pub fonts: ThemeFonts,
    pub colors: Colors,
    pub chips: Chips,
    pub syntax: Syntax,
    pub terminal: Terminal,
}

/// A status chip's colors from the tint recipe.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChipColors {
    pub fill: Hsla,
    pub border: Hsla,
    pub text: Hsla,
}

impl Theme {
    #[must_use]
    pub fn from_data(data: ThemeData) -> Self {
        let hsla = |_: &str, c: crate::Color| c.to_hsla();
        Self {
            id: data.id,
            name: data.name,
            mode: data.mode,
            radius: data.radius.map(|_, r| px(r)),
            fonts: data.fonts,
            colors: data.colors.map(hsla),
            chips: data.chips,
            syntax: data.syntax.map(hsla),
            terminal: data.terminal.map(hsla),
        }
    }

    /// This theme with the user's font settings in place of its defaults.
    #[must_use]
    pub fn with_fonts(mut self, interface: FontSettings, buffer: FontSettings) -> Self {
        self.fonts = ThemeFonts {
            interface,
            buffer,
            display: self.fonts.display,
        };
        self
    }

    /// The family for titles, headings and buttons.
    #[must_use]
    pub fn display_family(&self) -> SharedString {
        self.fonts
            .display
            .clone()
            .map_or_else(|| self.fonts.interface.family(), SharedString::from)
    }

    #[must_use]
    pub fn is_dark(&self) -> bool {
        self.mode.is_dark()
    }

    /// Fill, border and text for a chip in `state` (usually one of `ok`,
    /// `warn`, `danger`, `info` or `accent_fg`).
    #[must_use]
    pub fn chip_colors(&self, state: Hsla) -> ChipColors {
        ChipColors {
            fill: state.opacity(self.chips.fill_alpha),
            border: state.opacity(self.chips.border_alpha),
            text: state,
        }
    }
}

/// The active theme, as a gpui global. Replace it only through [`apply`].
#[derive(Clone, Debug)]
pub struct ActiveTheme(pub Arc<Theme>);

impl Global for ActiveTheme {}

/// The active theme.
///
/// # Panics
///
/// If [`apply`] has not run yet.
#[must_use]
pub fn current(cx: &App) -> &Theme {
    &cx.global::<ActiveTheme>().0
}

/// `cx.current_theme()` on [`App`] (and so on every gpui context that derefs
/// to it). Named so it does not collide with gpui-component's `cx.theme()`.
pub trait CurrentTheme {
    fn current_theme(&self) -> &Theme;
}

impl CurrentTheme for App {
    fn current_theme(&self) -> &Theme {
        current(self)
    }
}

/// Installs `theme` as the active theme, maps it onto gpui-component's theme
/// (so its inputs, dock, menus, lists and scrollbars follow) and refreshes
/// every window. Styles derived from the theme elsewhere (the diff and
/// terminal styles) are rebuilt by observers of [`ActiveTheme`].
pub fn apply(cx: &mut App, theme: impl Into<Arc<Theme>>) {
    let theme = theme.into();
    cx.set_global(ActiveTheme(theme.clone()));
    component::apply(cx, &theme);
    cx.refresh_windows();
}
