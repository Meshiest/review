//! The built-in themes and the user's theme directory.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use gpui::WindowAppearance;

use crate::file::{Mode, RawTheme, ThemeData, ThemeError};
use crate::runtime::Theme;

/// A theme file compiled into the binary.
#[derive(Clone, Copy, Debug)]
pub struct Builtin {
    pub id: &'static str,
    pub source: &'static str,
}

/// Every built-in theme file. The first is the default.
pub const BUILTIN: &[Builtin] = &[
    Builtin {
        id: "vercel-dark",
        source: include_str!("../themes/vercel-dark.toml"),
    },
    Builtin {
        id: "vercel-light",
        source: include_str!("../themes/vercel-light.toml"),
    },
];

/// The family used when settings name none or one that does not exist.
pub const DEFAULT_FAMILY: &str = "Vercel";

/// The `mode` setting: follow the OS, or pin one.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, serde::Deserialize, serde::Serialize,
)]
#[serde(rename_all = "lowercase")]
pub enum ModeSetting {
    #[default]
    System,
    Dark,
    Light,
}

impl ModeSetting {
    /// The mode to show for a window with this appearance.
    #[must_use]
    pub fn resolve(self, appearance: WindowAppearance) -> Mode {
        match self {
            Self::Dark => Mode::Dark,
            Self::Light => Mode::Light,
            Self::System => match appearance {
                WindowAppearance::Dark | WindowAppearance::VibrantDark => Mode::Dark,
                WindowAppearance::Light | WindowAppearance::VibrantLight => Mode::Light,
            },
        }
    }
}

/// A family and the ids of the modes it has.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Family {
    pub name: String,
    pub dark: Option<String>,
    pub light: Option<String>,
}

impl Family {
    #[must_use]
    pub fn modes(&self) -> Vec<Mode> {
        [(Mode::Dark, &self.dark), (Mode::Light, &self.light)]
            .into_iter()
            .filter_map(|(mode, id)| id.as_ref().map(|_| mode))
            .collect()
    }
}

/// Every usable theme: the built-ins, then the user's.
#[derive(Clone, Debug)]
pub struct ThemeRegistry {
    themes: Vec<Arc<Theme>>,
}

impl ThemeRegistry {
    /// The built-in themes only.
    ///
    /// # Panics
    ///
    /// If a built-in file is invalid, which the crate's tests rule out.
    #[must_use]
    pub fn builtin() -> Self {
        let themes = BUILTIN
            .iter()
            .map(|b| {
                let data = ThemeData::parse(b.id, &builtin_file(b.id), b.source)
                    .unwrap_or_else(|e| panic!("built-in theme: {e}"));
                Arc::new(Theme::from_data(data))
            })
            .collect();
        Self { themes }
    }

    /// The built-ins plus every `*.toml` in `user_dir` (the app passes
    /// `<config dir>/review/themes`). A missing directory is not an error; each
    /// file that fails is skipped and its error returned.
    ///
    /// This reads the disk: call it off the main thread.
    #[must_use]
    pub fn load(user_dir: &Path) -> (Self, Vec<ThemeError>) {
        let mut sources = Vec::new();
        let mut errors = Vec::new();
        if let Ok(entries) = std::fs::read_dir(user_dir) {
            let mut paths: Vec<_> = entries
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| {
                    p.extension()
                        .is_some_and(|e| e.eq_ignore_ascii_case("toml"))
                })
                .collect();
            paths.sort();
            for path in paths {
                let file = path.display().to_string();
                match std::fs::read_to_string(&path) {
                    Ok(source) => {
                        let id = path
                            .file_stem()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        sources.push((id, file, source));
                    }
                    Err(source) => errors.push(ThemeError::Io { file, source }),
                }
            }
        }
        let mut registry = Self::builtin();
        let (themes, more) = resolve_user(sources);
        errors.extend(more);
        registry
            .themes
            .extend(themes.into_iter().map(|d| Arc::new(Theme::from_data(d))));
        (registry, errors)
    }

    /// Every theme, built-ins first.
    #[must_use]
    pub fn themes(&self) -> &[Arc<Theme>] {
        &self.themes
    }

    #[must_use]
    pub fn get(&self, id: &str) -> Option<&Arc<Theme>> {
        self.themes.iter().find(|t| t.id == id)
    }

    /// The families in registry order, each with the modes it has.
    #[must_use]
    pub fn families(&self) -> Vec<Family> {
        let mut families: Vec<Family> = Vec::new();
        for theme in &self.themes {
            let index = match families.iter().position(|f| f.name == theme.name) {
                Some(i) => i,
                None => {
                    families.push(Family {
                        name: theme.name.clone(),
                        dark: None,
                        light: None,
                    });
                    families.len() - 1
                }
            };
            let slot = match theme.mode {
                Mode::Dark => &mut families[index].dark,
                Mode::Light => &mut families[index].light,
            };
            slot.get_or_insert_with(|| theme.id.clone());
        }
        families
    }

    /// The theme of `family` (case-insensitive) in `mode`, if it has one.
    #[must_use]
    pub fn find(&self, family: &str, mode: Mode) -> Option<&Arc<Theme>> {
        self.themes
            .iter()
            .find(|t| t.mode == mode && t.name.eq_ignore_ascii_case(family))
    }

    /// The theme settings ask for: `family` in the resolved mode, else that
    /// family's other mode, else the default family.
    ///
    /// # Panics
    ///
    /// Never: the default family is built in with both modes.
    #[must_use]
    pub fn select(
        &self,
        family: &str,
        mode: ModeSetting,
        appearance: WindowAppearance,
    ) -> Arc<Theme> {
        let mode = mode.resolve(appearance);
        let other = match mode {
            Mode::Dark => Mode::Light,
            Mode::Light => Mode::Dark,
        };
        self.find(family, mode)
            .or_else(|| self.find(family, other))
            .or_else(|| self.find(DEFAULT_FAMILY, mode))
            .cloned()
            .expect("the default family is built in with both modes")
    }
}

impl Default for ThemeRegistry {
    fn default() -> Self {
        Self::builtin()
    }
}

fn builtin_file(id: &str) -> String {
    format!("built-in {id}")
}

/// Resolves user files, following `extends` through other user files and the
/// built-ins.
fn resolve_user(sources: Vec<(String, String, String)>) -> (Vec<ThemeData>, Vec<ThemeError>) {
    let mut errors = Vec::new();
    let mut raw: BTreeMap<String, (String, RawTheme)> = BTreeMap::new();
    for (id, file, source) in sources {
        if BUILTIN.iter().any(|b| b.id == id) {
            errors.push(ThemeError::ShadowsBuiltin { file, id });
            continue;
        }
        match RawTheme::parse(&file, &source) {
            Ok(theme) => {
                raw.insert(id, (file, theme));
            }
            Err(e) => errors.push(e),
        }
    }
    let mut themes = Vec::new();
    for (id, (file, _)) in &raw {
        match flatten(id, &raw, &mut Vec::new()).and_then(|t| t.resolve(id, file)) {
            Ok(data) => themes.push(data),
            Err(e) => errors.push(e),
        }
    }
    (themes, errors)
}

/// `id` merged over its `extends` chain. `chain` holds the ids being resolved,
/// to report loops.
fn flatten(
    id: &str,
    user: &BTreeMap<String, (String, RawTheme)>,
    chain: &mut Vec<String>,
) -> Result<RawTheme, ThemeError> {
    if let Some(builtin) = BUILTIN.iter().find(|b| b.id == id) {
        return RawTheme::parse(&builtin_file(id), builtin.source);
    }
    let Some((file, theme)) = user.get(id) else {
        let file = chain.last().map_or_else(String::new, |child| {
            user.get(child).map(|(f, _)| f.clone()).unwrap_or_default()
        });
        return Err(ThemeError::UnknownParent {
            file,
            parent: id.to_owned(),
        });
    };
    if chain.iter().any(|c| c == id) {
        chain.push(id.to_owned());
        return Err(ThemeError::Cycle {
            file: file.clone(),
            chain: chain.clone(),
        });
    }
    let Some(parent) = theme.extends.as_deref() else {
        return Ok(theme.clone());
    };
    chain.push(id.to_owned());
    let parent = flatten(parent, user, chain)?;
    chain.pop();
    Ok(theme.clone().merge(parent))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(files: &[(&str, &str)]) -> (Vec<ThemeData>, Vec<ThemeError>) {
        resolve_user(
            files
                .iter()
                .map(|(id, src)| ((*id).to_owned(), format!("{id}.toml"), (*src).to_owned()))
                .collect(),
        )
    }

    #[test]
    fn families_pair_dark_and_light() {
        let registry = ThemeRegistry::builtin();
        let names: Vec<_> = registry
            .families()
            .into_iter()
            .map(|f| (f.name.clone(), f.modes()))
            .collect();
        assert_eq!(
            names,
            [("Vercel".to_owned(), vec![Mode::Dark, Mode::Light]),]
        );
    }

    #[test]
    fn select_follows_the_system_and_falls_back() {
        let registry = ThemeRegistry::builtin();
        let pick = |family, mode, appearance| registry.select(family, mode, appearance).id.clone();
        assert_eq!(
            pick("vercel", ModeSetting::System, WindowAppearance::Light),
            "vercel-light"
        );
        assert_eq!(
            pick("VERCEL", ModeSetting::Dark, WindowAppearance::Light),
            "vercel-dark"
        );
        assert_eq!(
            pick("nope", ModeSetting::System, WindowAppearance::VibrantDark),
            "vercel-dark"
        );
    }

    #[test]
    fn user_themes_extend_builtins_and_each_other() {
        let (themes, errors) = user(&[
            (
                "a",
                "name = \"Mine\"\nextends = \"vercel-dark\"\n[colors]\naccent = \"#00ff00\"\n",
            ),
            ("b", "name = \"Mine\"\nextends = \"a\"\nmode = \"light\"\n"),
        ]);
        assert!(errors.is_empty(), "{errors:?}");
        let b = themes.iter().find(|t| t.id == "b").expect("b resolves");
        assert_eq!(b.mode, Mode::Light);
        assert_eq!(b.colors.accent.to_string(), "#00ff00");
    }

    #[test]
    fn user_errors_name_the_file() {
        let (themes, errors) = user(&[
            ("loop1", "name = \"L\"\nextends = \"loop2\"\n"),
            ("loop2", "name = \"L\"\nextends = \"loop1\"\n"),
            ("orphan", "name = \"O\"\nextends = \"missing\"\n"),
            ("vercel-dark", "name = \"V\"\n"),
        ]);
        assert!(themes.is_empty());
        let text: Vec<String> = errors.iter().map(ToString::to_string).collect();
        assert!(
            text.iter().any(|t| t.contains("vercel-dark.toml: the id")),
            "{text:?}"
        );
        assert!(
            text.iter()
                .any(|t| t.contains("loop1.toml: extends loop: loop1 -> loop2 -> loop1")),
            "{text:?}"
        );
        assert!(
            text.iter()
                .any(|t| t == "orphan.toml: extends \"missing\", which is not a known theme"),
            "{text:?}"
        );
    }

    #[test]
    fn load_reads_a_directory_and_tolerates_a_missing_one() {
        let (registry, errors) = ThemeRegistry::load(Path::new("does/not/exist"));
        assert!(errors.is_empty());
        assert_eq!(registry.themes().len(), BUILTIN.len());
    }
}
