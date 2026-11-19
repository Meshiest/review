//! Themes as data: color tokens, syntax colors, radii and font
//! settings.
//!
//! Theme files are TOML (`themes/*.toml` are the built-ins, embedded; users
//! add their own to a directory and may `extends` any other theme). A
//! [`ThemeRegistry`] holds them; [`apply`] installs one as the [`ActiveTheme`]
//! global and maps it onto gpui-component's theme.

mod color;
mod component;
mod file;
pub mod fonts;
mod registry;
mod runtime;
mod tokens;

pub use color::{Color, mix};
pub use file::{Mode, ThemeData, ThemeError, ThemeFonts};
pub use fonts::FontSettings;
pub use registry::{BUILTIN, Builtin, DEFAULT_FAMILY, Family, ModeSetting, ThemeRegistry};
pub use runtime::{ActiveTheme, ChipColors, CurrentTheme, Theme, apply, current};
pub use tokens::{Chips, Colors, Radius, Syntax, Terminal};
