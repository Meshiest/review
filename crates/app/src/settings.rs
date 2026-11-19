//! `<config dir>/review/settings.toml`: the theme family, the mode and the
//! two fonts, which the settings panel edits live.
//!
//! A key that is missing falls back to the theme's own default, so the file
//! holds only what the user changed and a theme switch still brings its own
//! fonts along. Reading and writing block; the app calls them from a
//! background task.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::WindowAppearance;
use serde::{Deserialize, Serialize};
use theme::{DEFAULT_FAMILY, FontSettings, ModeSetting, Theme, ThemeError, ThemeRegistry};

/// Font sizes the settings accept, in px.
pub const SIZE_RANGE: (f32, f32) = (8.0, 32.0);
/// Line heights the settings accept, in whole px.
pub const LINE_HEIGHT_RANGE: (u32, u32) = (10, 64);

const HEADER: &str = "# review settings. The settings panel (Ctrl+,) writes this file;\n\
                      # delete a key to go back to the theme's default for it.\n\
                      #   theme   a theme family, such as \"Vercel\" or one of your own\n\
                      #   mode    \"system\", \"dark\" or \"light\"\n\
                      #   load_gravatar   true to fetch Gravatar avatars (a third party; GitLab uses it)\n\
                      #   [notifications]: enabled, watch_mine and one switch per kind of toast\n\
                      #   [inbox] hidden_repos: \"host/owner/name\" of repositories the inbox hides;\n\
                      #     mark_seen_on_open, mark_reviewed_when_viewed, unread_first and\n\
                      #     dim_reviewed: the local review marks\n\
                      #   [diff] collapse_viewed: false keeps viewed files open in the diff\n\
                      #   [interface_font] and [buffer_font]: family, size (px), line_height\n\
                      #   (whole px) and fallbacks\n\n";

fn default_theme() -> String {
    DEFAULT_FAMILY.to_owned()
}

/// What the user changed about one font; `None` keeps the theme's value.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FontOverride {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<f32>,
    /// Whole px, so a focused input never shifts its text by a sub-pixel.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line_height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallbacks: Option<Vec<String>>,
}

impl FontOverride {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// `base` with this override's keys in place of its own.
    #[must_use]
    pub fn resolve(&self, base: &FontSettings) -> FontSettings {
        FontSettings {
            family: self.family.clone().unwrap_or_else(|| base.family.clone()),
            size: self.size.unwrap_or(base.size),
            line_height: self.line_height.unwrap_or(base.line_height),
            fallbacks: self
                .fallbacks
                .clone()
                .unwrap_or_else(|| base.fallbacks.clone()),
        }
    }

    fn check(&self, section: &str) -> Result<(), String> {
        if self.family.as_deref().is_some_and(|f| f.trim().is_empty()) {
            return Err(format!("{section}.family is empty"));
        }
        if let Some(size) = self.size
            && !(SIZE_RANGE.0..=SIZE_RANGE.1).contains(&size)
        {
            return Err(format!(
                "{section}.size {size} is outside {}..{}",
                SIZE_RANGE.0, SIZE_RANGE.1
            ));
        }
        if let Some(line_height) = self.line_height
            && !(LINE_HEIGHT_RANGE.0..=LINE_HEIGHT_RANGE.1).contains(&line_height)
        {
            return Err(format!(
                "{section}.line_height {line_height} is outside {}..{}",
                LINE_HEIGHT_RANGE.0, LINE_HEIGHT_RANGE.1
            ));
        }
        Ok(())
    }
}

/// The user's choices.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// A theme family name.
    pub theme: String,
    pub mode: ModeSetting,
    /// Whether avatars may load from Gravatar, which is not the forge's own
    /// host. Off until the user turns it on.
    pub load_gravatar: bool,
    #[serde(skip_serializing_if = "FontOverride::is_empty")]
    pub interface_font: FontOverride,
    #[serde(skip_serializing_if = "FontOverride::is_empty")]
    pub buffer_font: FontOverride,
    #[serde(skip_serializing_if = "NotificationSettings::is_default")]
    pub notifications: NotificationSettings,
    #[serde(skip_serializing_if = "InboxSettings::is_default")]
    pub inbox: InboxSettings,
    #[serde(skip_serializing_if = "ConversationSettings::is_default")]
    pub conversation: ConversationSettings,
    #[serde(skip_serializing_if = "DiffSettings::is_default")]
    pub diff: DiffSettings,
}

/// The diff view's choices.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DiffSettings {
    /// The left pane's share of a split diff, as the divider was last left.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub split_ratio: Option<f32>,
    /// Marking a file viewed collapses it in the diff; un-marking expands it.
    #[serde(skip_serializing_if = "is_true")]
    pub collapse_viewed: bool,
}

impl Default for DiffSettings {
    fn default() -> Self {
        Self {
            split_ratio: None,
            collapse_viewed: true,
        }
    }
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde passes a reference"
)]
fn is_true(value: &bool) -> bool {
    *value
}

impl DiffSettings {
    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// How inline threads show in a pull request's Conversation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadStyle {
    /// The path, the code it is on and every comment in full.
    #[default]
    Expanded,
    /// One row per thread.
    Compact,
}

/// The Conversation's choices.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConversationSettings {
    pub threads: ThreadStyle,
}

impl ConversationSettings {
    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// What the inbox shows. Hiding a repository is display only: it is still
/// read, watched and counted. The marks are local review tracking, never
/// sent to a forge.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InboxSettings {
    /// `host/owner/name` of each hidden repository, sorted.
    pub hidden_repos: Vec<String>,
    /// Opening a pull request marks it seen: new commits, comments or
    /// reviews after that show an unread dot.
    #[serde(skip_serializing_if = "is_true")]
    pub mark_seen_on_open: bool,
    /// Viewing every file marks the pull request reviewed.
    #[serde(skip_serializing_if = "is_true")]
    pub mark_reviewed_when_viewed: bool,
    /// Unread pull requests first in each section.
    #[serde(skip_serializing_if = "is_false")]
    pub unread_first: bool,
    /// Reviewed pull requests with nothing new are drawn faded.
    #[serde(skip_serializing_if = "is_true")]
    pub dim_reviewed: bool,
}

impl Default for InboxSettings {
    fn default() -> Self {
        Self {
            hidden_repos: Vec::new(),
            mark_seen_on_open: true,
            mark_reviewed_when_viewed: true,
            unread_first: false,
            dim_reviewed: true,
        }
    }
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde passes a reference"
)]
fn is_false(value: &bool) -> bool {
    !*value
}

impl InboxSettings {
    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    #[must_use]
    pub fn is_hidden(&self, repo: &str) -> bool {
        self.hidden_repos
            .binary_search_by(|r| r.as_str().cmp(repo))
            .is_ok()
    }

    /// Hides or shows `repo`; returns whether that changed anything.
    pub fn set_hidden(&mut self, repo: &str, hidden: bool) -> bool {
        match (
            self.hidden_repos.binary_search_by(|r| r.as_str().cmp(repo)),
            hidden,
        ) {
            (Err(at), true) => self.hidden_repos.insert(at, repo.to_owned()),
            (Ok(at), false) => {
                self.hidden_repos.remove(at);
            }
            _ => return false,
        }
        true
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: default_theme(),
            mode: ModeSetting::System,
            load_gravatar: false,
            interface_font: FontOverride::default(),
            buffer_font: FontOverride::default(),
            notifications: NotificationSettings::default(),
            inbox: InboxSettings::default(),
            conversation: ConversationSettings::default(),
            diff: DiffSettings::default(),
        }
    }
}

/// Which changes on a watched pull request or run raise a system toast,
/// and what is watched without being asked. Toasts only fire while the app
/// runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NotificationSettings {
    /// Every toast; off keeps the badges and the activity list.
    pub enabled: bool,
    /// Pull requests you authored or are asked to review are watched
    /// unless you turn watching off on one.
    pub watch_mine: bool,
    pub ci_failed: bool,
    /// Checks pass again after a failure.
    pub ci_recovered: bool,
    /// Checks finished and passed.
    pub ci_finished: bool,
    /// A review with a verdict on your pull request.
    pub reviews: bool,
    /// A comment on your pull request.
    pub comments: bool,
    pub merged_closed: bool,
}

impl NotificationSettings {
    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

impl Default for NotificationSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            watch_mine: true,
            ci_failed: true,
            ci_recovered: true,
            ci_finished: true,
            reviews: true,
            comments: true,
            merged_closed: true,
        }
    }
}

impl Settings {
    /// The theme these settings ask for under `appearance`, with the user's
    /// fonts in place of the theme's defaults.
    #[must_use]
    pub fn resolve(&self, registry: &ThemeRegistry, appearance: WindowAppearance) -> Arc<Theme> {
        let base = registry.select(&self.theme, self.mode, appearance);
        if self.interface_font.is_empty() && self.buffer_font.is_empty() {
            return base;
        }
        let interface = self.interface_font.resolve(&base.fonts.interface);
        let buffer = self.buffer_font.resolve(&base.fonts.buffer);
        Arc::new(Theme::clone(&*base).with_fonts(interface, buffer))
    }

    /// Back to the theme's own fonts; the theme and mode stay.
    pub fn reset_fonts(&mut self) {
        self.interface_font = FontOverride::default();
        self.buffer_font = FontOverride::default();
    }

    /// # Errors
    /// If the text is not valid settings TOML or a value is out of range.
    pub fn parse(text: &str) -> Result<Self, ParseError> {
        let mut settings: Self = toml::from_str(text).map_err(|error| ParseError::Toml {
            message: error.message().to_owned(),
            line: error
                .span()
                .map(|span| text[..span.start].matches('\n').count() + 1),
        })?;
        // Kept sorted and unique for the lookups; a hand edit may not be.
        settings.inbox.hidden_repos.sort();
        settings.inbox.hidden_repos.dedup();
        settings
            .interface_font
            .check("interface_font")
            .and_then(|()| settings.buffer_font.check("buffer_font"))
            .map_err(ParseError::Invalid)?;
        Ok(settings)
    }

    #[must_use]
    pub fn render(&self) -> String {
        let body = toml::to_string_pretty(self).expect("settings are plain TOML values");
        format!("{HEADER}{body}")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("{}", match line {
        Some(line) => format!("line {line}: {message}"),
        None => message.clone(),
    })]
    Toml {
        message: String,
        line: Option<usize>,
    },
    #[error("{0}")]
    Invalid(String),
}

/// A settings file that cannot be read or must not be written.
#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("{}: {source}", path.display())]
    Io { path: PathBuf, source: io::Error },
    #[error("{}: {source}", path.display())]
    Parse { path: PathBuf, source: ParseError },
    #[error("{}: not saved, the file has errors; fix it and restart", path.display())]
    Protected { path: PathBuf },
}

/// Where the settings live and whether the app may write there.
#[derive(Clone, Debug)]
pub struct SettingsFile {
    path: PathBuf,
    /// False once the file was found malformed: the user's text stays as it is.
    writable: bool,
}

/// What reading a settings file produced.
pub struct Loaded {
    pub settings: Settings,
    pub file: SettingsFile,
    pub error: Option<SettingsError>,
}

impl SettingsFile {
    /// `<config dir>/review`.
    #[must_use]
    pub fn config_dir() -> Option<PathBuf> {
        dirs::config_dir().map(|dir| dir.join("review"))
    }

    /// `<config dir>/review/settings.toml`.
    #[must_use]
    pub fn default_path() -> Option<PathBuf> {
        Self::config_dir().map(|dir| dir.join("settings.toml"))
    }

    /// `<config dir>/review/themes`.
    #[must_use]
    pub fn themes_dir() -> Option<PathBuf> {
        Self::config_dir().map(|dir| dir.join("themes"))
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn writable(&self) -> bool {
        self.writable
    }

    /// Reads `path`. A missing file is the defaults; one that cannot be read
    /// or parsed is the defaults, reported in `error`, and never written.
    #[must_use]
    pub fn load(path: PathBuf) -> Loaded {
        let read = match fs::read_to_string(&path) {
            Ok(text) => Settings::parse(&text)
                .map(Some)
                .map_err(|source| SettingsError::Parse {
                    path: path.clone(),
                    source,
                }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(SettingsError::Io {
                path: path.clone(),
                source,
            }),
        };
        match read {
            Ok(settings) => Loaded {
                settings: settings.unwrap_or_default(),
                file: Self {
                    path,
                    writable: true,
                },
                error: None,
            },
            Err(error) => Loaded {
                settings: Settings::default(),
                file: Self {
                    path,
                    writable: false,
                },
                error: Some(error),
            },
        }
    }

    /// Writes `settings` through a temp file and a rename, so a crash cannot
    /// leave half a file.
    ///
    /// # Errors
    /// [`SettingsError::Protected`] when the file was malformed at load or has
    /// become malformed since (the user is editing it by hand); the file is
    /// left as it is. [`SettingsError::Io`] when the write fails.
    pub fn save(&self, settings: &Settings) -> Result<(), SettingsError> {
        let protected = || SettingsError::Protected {
            path: self.path.clone(),
        };
        if !self.writable {
            return Err(protected());
        }
        if let Ok(existing) = fs::read_to_string(&self.path)
            && Settings::parse(&existing).is_err()
        {
            return Err(protected());
        }
        write_atomic(&self.path, &settings.render()).map_err(|source| SettingsError::Io {
            path: self.path.clone(),
            source,
        })
    }
}

fn write_atomic(path: &Path, text: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, text)?;
    fs::rename(&tmp, path)
}

/// Everything the first frames wait on, read on the boot thread.
pub struct Startup {
    pub registry: ThemeRegistry,
    pub settings: Settings,
    pub file: Option<SettingsFile>,
    /// One line per file that could not be used, for the status bar.
    pub problems: Vec<String>,
}

impl Startup {
    /// Reads the settings file and the user themes directory. Blocks on the
    /// disk.
    #[must_use]
    pub fn read(settings: Option<PathBuf>, themes: Option<&Path>) -> Self {
        let mut problems = Vec::new();
        let (registry, theme_errors) = match themes {
            Some(dir) => ThemeRegistry::load(dir),
            None => (ThemeRegistry::builtin(), Vec::new()),
        };
        problems.extend(theme_errors.iter().map(ThemeError::to_string));
        let (settings, file) = match settings {
            Some(path) => {
                let loaded = SettingsFile::load(path);
                problems.extend(loaded.error.map(|e| e.to_string()));
                (loaded.settings, Some(loaded.file))
            }
            None => (Settings::default(), None),
        };
        Self {
            registry,
            settings,
            file,
            problems,
        }
    }

    /// [`Self::read`] on the user's own settings file and themes directory.
    #[must_use]
    pub fn read_default() -> Self {
        Self::read(
            SettingsFile::default_path(),
            SettingsFile::themes_dir().as_deref(),
        )
    }

    /// The problems as one status-bar line.
    #[must_use]
    pub fn notice(&self) -> Option<String> {
        let (first, rest) = self.problems.split_first()?;
        // The status bar has one line; a TOML error's source excerpt does not.
        let first = first.lines().next().unwrap_or_default();
        Some(if rest.is_empty() {
            first.to_owned()
        } else {
            format!("{first} (and {} more)", rest.len())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_keys_fall_back_to_the_theme() {
        let settings = Settings::parse("theme = \"vercel\"\n[buffer_font]\nsize = 15\n").unwrap();
        assert_eq!(settings.theme, "vercel");
        assert_eq!(settings.mode, ModeSetting::System);
        assert!(settings.interface_font.is_empty());
        let registry = ThemeRegistry::builtin();
        let resolved = settings.resolve(&registry, WindowAppearance::Dark);
        let base = registry.select("vercel", ModeSetting::System, WindowAppearance::Dark);
        assert_eq!(resolved.fonts.buffer.size, 15.0);
        assert_eq!(resolved.fonts.buffer.family, base.fonts.buffer.family);
        assert_eq!(resolved.fonts.interface, base.fonts.interface);
    }

    #[test]
    fn gravatar_is_off_unless_asked_for() {
        assert!(!Settings::parse("theme = \"vercel\"").unwrap().load_gravatar);
        assert!(
            Settings::parse("load_gravatar = true")
                .unwrap()
                .load_gravatar
        );
        assert!(Settings::parse("load_gravatar = \"yes\"").is_err());
        assert!(
            Settings::default()
                .render()
                .contains("load_gravatar = false")
        );
    }

    #[test]
    fn an_empty_file_is_the_defaults() {
        assert_eq!(Settings::parse("").unwrap(), Settings::default());
    }

    #[test]
    fn rendered_settings_parse_back_equal() {
        let settings = Settings {
            theme: "vercel".into(),
            mode: ModeSetting::Light,
            load_gravatar: true,
            interface_font: FontOverride {
                family: Some("Segoe UI".into()),
                size: Some(14.0),
                line_height: Some(20),
                fallbacks: Some(vec!["Arial".into()]),
            },
            buffer_font: FontOverride {
                family: Some("Consolas".into()),
                ..FontOverride::default()
            },
            notifications: NotificationSettings {
                comments: false,
                ..NotificationSettings::default()
            },
            inbox: InboxSettings {
                hidden_repos: vec!["github.com/a/b".into()],
                mark_seen_on_open: false,
                unread_first: true,
                ..InboxSettings::default()
            },
            conversation: ConversationSettings {
                threads: ThreadStyle::Compact,
            },
            diff: DiffSettings {
                split_ratio: Some(0.625),
                collapse_viewed: false,
            },
        };
        assert_eq!(Settings::parse(&settings.render()).unwrap(), settings);
        assert!(
            !Settings::default()
                .render()
                .lines()
                .any(|line| line.starts_with('[')),
            "unset fonts write no tables"
        );
    }

    #[test]
    fn viewed_files_collapse_unless_turned_off() {
        assert!(Settings::default().diff.collapse_viewed);
        let off = Settings::parse(
            "[diff]
collapse_viewed = false",
        )
        .unwrap();
        assert!(!off.diff.collapse_viewed);
        let key = |rendered: String| {
            rendered
                .lines()
                .any(|line| line.starts_with("collapse_viewed"))
        };
        assert!(off.render().contains("collapse_viewed = false"));
        assert!(
            !key(Settings::default().render()),
            "the default is not written"
        );
    }

    #[test]
    fn the_thread_style_is_saved_by_name() {
        let settings = Settings::parse(
            "[conversation]
threads = \"compact\"",
        )
        .unwrap();
        assert_eq!(settings.conversation.threads, ThreadStyle::Compact);
        assert_eq!(
            Settings::default().conversation.threads,
            ThreadStyle::Expanded
        );
        assert!(
            Settings::parse(
                "[conversation]
threads = \"tiny\""
            )
            .is_err()
        );
    }

    #[test]
    fn review_marks_are_on_and_sorting_off_unless_changed() {
        let inbox = Settings::default().inbox;
        assert!(inbox.mark_seen_on_open && inbox.mark_reviewed_when_viewed && inbox.dim_reviewed);
        assert!(!inbox.unread_first);
        let changed = Settings::parse(
            "[inbox]
unread_first = true
dim_reviewed = false",
        )
        .unwrap()
        .inbox;
        assert!(changed.unread_first && !changed.dim_reviewed);
        assert!(changed.mark_seen_on_open, "a missing key keeps its default");
    }

    const HIDDEN: &str = r#"
[inbox]
hidden_repos = ["z/y/x", "a/b/c", "z/y/x"]
"#;

    #[test]
    fn hidden_repos_stay_sorted_and_unique() {
        let mut inbox = Settings::parse(HIDDEN).unwrap().inbox;
        assert_eq!(inbox.hidden_repos, ["a/b/c", "z/y/x"]);
        assert!(inbox.is_hidden("z/y/x") && !inbox.is_hidden("m/n/o"));
        assert!(inbox.set_hidden("m/n/o", true));
        assert!(!inbox.set_hidden("m/n/o", true), "already hidden");
        assert_eq!(inbox.hidden_repos, ["a/b/c", "m/n/o", "z/y/x"]);
        assert!(inbox.set_hidden("a/b/c", false));
        assert_eq!(inbox.hidden_repos, ["m/n/o", "z/y/x"]);
    }

    #[test]
    fn out_of_range_and_unknown_keys_are_errors() {
        assert!(Settings::parse("[buffer_font]\nsize = 3").is_err());
        assert!(Settings::parse("[buffer_font]\nline_height = 0").is_err());
        assert!(Settings::parse("[interface_font]\nfamily = \"\"").is_err());
        assert!(Settings::parse("colour = 1").is_err());
        assert!(Settings::parse("mode = \"sepia\"").is_err());
    }
}
