//! Theme files: TOML parsing, `extends` merging and validation into
//! [`ThemeData`], which holds no gpui types.

use std::fmt;

use serde::Deserialize;

use crate::color::Color;
use crate::fonts::FontSettings;
use crate::tokens::{Chips, Colors, Radius, Syntax, Terminal};

/// One mode per file; a family is the dark and light files sharing a `name`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Dark,
    Light,
}

impl Mode {
    #[must_use]
    pub fn is_dark(self) -> bool {
        self == Self::Dark
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The interface and buffer fonts a theme suggests; settings override them.
#[derive(Clone, Debug, PartialEq)]
pub struct ThemeFonts {
    /// Labels, buttons, prose.
    pub interface: FontSettings,
    /// Diff, log, terminal, composer.
    pub buffer: FontSettings,
    /// Titles, headings and buttons, when the theme sets them apart from the
    /// interface font. `None` means the interface font.
    pub display: Option<String>,
}

/// A fully resolved theme file: every token present and valid.
#[derive(Clone, Debug, PartialEq)]
pub struct ThemeData {
    /// File stem, such as `vercel-dark`; what `extends` names.
    pub id: String,
    /// Family name, such as `Vercel`.
    pub name: String,
    pub mode: Mode,
    pub radius: Radius<f32>,
    pub fonts: ThemeFonts,
    pub colors: Colors<Color>,
    pub chips: Chips<f32>,
    pub syntax: Syntax<Color>,
    pub terminal: Terminal<Color>,
}

/// Why a theme file could not be used. `file` names the file (a path, or
/// `built-in <id>`).
#[derive(Debug, thiserror::Error)]
pub enum ThemeError {
    #[error("{file}: {source}")]
    Io {
        file: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{file}: {source}")]
    Toml {
        file: String,
        #[source]
        source: toml::de::Error,
    },
    #[error("{file}: {key}: {message}")]
    Invalid {
        file: String,
        key: String,
        message: String,
    },
    #[error("{file}: missing {key} (set it, or extend a theme that has it)")]
    Missing { file: String, key: String },
    #[error("{file}: extends {parent:?}, which is not a known theme")]
    UnknownParent { file: String, parent: String },
    #[error("{file}: extends loop: {}", chain.join(" -> "))]
    Cycle { file: String, chain: Vec<String> },
    #[error("{file}: the id {id:?} belongs to a built-in theme; rename the file")]
    ShadowsBuiltin { file: String, id: String },
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawFont {
    family: Option<String>,
    size: Option<f32>,
    line_height: Option<u32>,
    fallbacks: Option<Vec<String>>,
}

impl RawFont {
    fn merge(self, parent: Self) -> Self {
        Self {
            family: self.family.or(parent.family),
            size: self.size.or(parent.size),
            line_height: self.line_height.or(parent.line_height),
            fallbacks: self.fallbacks.or(parent.fallbacks),
        }
    }

    fn resolve(self, file: &str, section: &str) -> Result<FontSettings, ThemeError> {
        let key = |k: &str| format!("fonts.{section}.{k}");
        let missing = |k: &str| ThemeError::Missing {
            file: file.to_owned(),
            key: key(k),
        };
        let invalid = |k: &str, message: &str| ThemeError::Invalid {
            file: file.to_owned(),
            key: key(k),
            message: message.to_owned(),
        };
        let family = self.family.ok_or_else(|| missing("family"))?;
        if family.trim().is_empty() {
            return Err(invalid("family", "is empty"));
        }
        let size = self.size.ok_or_else(|| missing("size"))?;
        if !(6.0..=72.0).contains(&size) {
            return Err(invalid("size", "must be between 6 and 72 px"));
        }
        let line_height = self.line_height.ok_or_else(|| missing("line_height"))?;
        if !(1..=200).contains(&line_height) {
            return Err(invalid("line_height", "must be between 1 and 200 px"));
        }
        Ok(FontSettings {
            family,
            size,
            line_height,
            fallbacks: self.fallbacks.unwrap_or_default(),
        })
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawFonts {
    interface: RawFont,
    buffer: RawFont,
    display: Option<String>,
}

/// A theme file as written: every field optional, so `extends` can fill gaps.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct RawTheme {
    name: Option<String>,
    mode: Option<Mode>,
    pub(crate) extends: Option<String>,
    radius: Radius<Option<f32>>,
    fonts: RawFonts,
    colors: Colors<Option<String>>,
    chips: Chips<Option<f32>>,
    syntax: Syntax<Option<String>>,
    terminal: Terminal<Option<String>>,
}

impl RawTheme {
    pub(crate) fn parse(file: &str, source: &str) -> Result<Self, ThemeError> {
        toml::from_str(source).map_err(|source| ThemeError::Toml {
            file: file.to_owned(),
            source,
        })
    }

    /// Fills this file's unset tokens from `parent`. `name` is never
    /// inherited, so a user theme always names its own family.
    pub(crate) fn merge(self, parent: Self) -> Self {
        let or = |a: Option<String>, b: Option<String>| a.or(b);
        Self {
            name: self.name,
            mode: self.mode.or(parent.mode),
            extends: None,
            radius: self.radius.zip(parent.radius, Option::or),
            fonts: RawFonts {
                interface: self.fonts.interface.merge(parent.fonts.interface),
                buffer: self.fonts.buffer.merge(parent.fonts.buffer),
                display: self.fonts.display.or(parent.fonts.display),
            },
            colors: self.colors.zip(parent.colors, or),
            chips: self.chips.zip(parent.chips, Option::or),
            syntax: self.syntax.zip(parent.syntax, or),
            terminal: self.terminal.zip(parent.terminal, or),
        }
    }

    /// Checks that every token is present and valid.
    pub(crate) fn resolve(self, id: &str, file: &str) -> Result<ThemeData, ThemeError> {
        let missing = |key: &str| ThemeError::Missing {
            file: file.to_owned(),
            key: key.to_owned(),
        };
        let name = self.name.ok_or_else(|| missing("name"))?;
        let mode = self.mode.ok_or_else(|| missing("mode"))?;
        let radius = self.radius.try_map(|key, v| {
            let v = v.ok_or_else(|| missing(&format!("radius.{key}")))?;
            if !(0.0..=64.0).contains(&v) {
                return Err(invalid(
                    file,
                    &format!("radius.{key}"),
                    "must be 0 to 64 px",
                ));
            }
            Ok(v)
        })?;
        let chips = self.chips.try_map(|key, v| {
            let v = v.ok_or_else(|| missing(&format!("chips.{key}")))?;
            if !(0.0..=1.0).contains(&v) {
                return Err(invalid(file, &format!("chips.{key}"), "must be 0 to 1"));
            }
            Ok(v)
        })?;
        Ok(ThemeData {
            id: id.to_owned(),
            name,
            mode,
            radius,
            fonts: ThemeFonts {
                interface: self.fonts.interface.resolve(file, "interface")?,
                buffer: self.fonts.buffer.resolve(file, "buffer")?,
                display: self
                    .fonts
                    .display
                    .filter(|family| !family.trim().is_empty()),
            },
            colors: self
                .colors
                .try_map(colors_in(file, Colors::<()>::SECTION))?,
            chips,
            syntax: self
                .syntax
                .try_map(colors_in(file, Syntax::<()>::SECTION))?,
            terminal: self
                .terminal
                .try_map(colors_in(file, Terminal::<()>::SECTION))?,
        })
    }
}

fn invalid(file: &str, key: &str, message: &str) -> ThemeError {
    ThemeError::Invalid {
        file: file.to_owned(),
        key: key.to_owned(),
        message: message.to_owned(),
    }
}

fn colors_in(
    file: &str,
    section: &'static str,
) -> impl Fn(&'static str, Option<String>) -> Result<Color, ThemeError> {
    move |key, value| {
        let key = format!("{section}.{key}");
        let value = value.ok_or_else(|| ThemeError::Missing {
            file: file.to_owned(),
            key: key.clone(),
        })?;
        Color::parse(&value).map_err(|message| invalid(file, &key, &message))
    }
}

impl ThemeData {
    /// Parses one self-contained theme file (no `extends`).
    ///
    /// # Errors
    ///
    /// When the TOML is malformed, a key is unknown, missing or invalid, or
    /// the file uses `extends` (resolve those through
    /// [`ThemeRegistry`](crate::ThemeRegistry)).
    pub fn parse(id: &str, file: &str, source: &str) -> Result<Self, ThemeError> {
        let raw = RawTheme::parse(file, source)?;
        if let Some(parent) = raw.extends {
            return Err(ThemeError::UnknownParent {
                file: file.to_owned(),
                parent,
            });
        }
        raw.resolve(id, file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BUILTIN;

    fn vercel_dark() -> &'static str {
        BUILTIN[0].source
    }

    #[test]
    fn every_builtin_parses() {
        for builtin in BUILTIN {
            let data = ThemeData::parse(builtin.id, builtin.id, builtin.source)
                .unwrap_or_else(|e| panic!("{e}"));
            assert_eq!(
                format!("{}-{}", data.name.to_lowercase(), data.mode),
                builtin.id
            );
        }
    }

    #[test]
    fn a_bad_color_names_the_file_and_key() {
        let source: String = vercel_dark()
            .lines()
            .map(|line| {
                if line
                    .split('=')
                    .next()
                    .is_some_and(|key| key.trim() == "surface")
                {
                    "surface = \"#0a0a0\"".to_owned()
                } else {
                    line.to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let err = ThemeData::parse("x", "themes/x.toml", &source).expect_err("bad color");
        let text = err.to_string();
        assert!(
            text.starts_with("themes/x.toml: colors.surface: expected"),
            "{text}"
        );
    }

    #[test]
    fn an_unknown_key_is_rejected() {
        let err = RawTheme::parse("x.toml", "[colors]\nsurfce = \"#000000\"\n").expect_err("typo");
        assert!(err.to_string().contains("surfce"), "{err}");
    }

    #[test]
    fn a_missing_token_is_named() {
        let source = vercel_dark().replace("diff_hunk_text       = \"#50a8ff\"", "");
        let err = ThemeData::parse("x", "x.toml", &source).expect_err("missing");
        assert_eq!(
            err.to_string(),
            "x.toml: missing colors.diff_hunk_text (set it, or extend a theme that has it)"
        );
    }

    #[test]
    fn merge_overrides_only_what_the_child_sets() {
        let parent = RawTheme::parse("p", vercel_dark()).expect("parses");
        let child = RawTheme::parse(
            "c",
            "name = \"Mine\"\nextends = \"vercel-dark\"\n[colors]\naccent = \"#ff0000\"\n[radius]\nmd = 2\n",
        )
        .expect("parses");
        let data = child.merge(parent).resolve("mine", "c").expect("resolves");
        let base = ThemeData::parse("vercel-dark", "p", vercel_dark()).expect("parses");
        assert_eq!(data.name, "Mine");
        assert_eq!(data.colors.accent, Color::opaque(255, 0, 0));
        assert_eq!(data.radius.md, 2.0);
        assert_eq!(data.radius.lg, base.radius.lg);
        assert_eq!(data.colors.surface, base.colors.surface);
        assert_eq!(data.syntax, base.syntax);
    }
}
