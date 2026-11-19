//! Which files a run reads and writes, from its flags.
//!
//! A `--fixtures` run, and every benchmark and screenshot run built on it,
//! keeps the layout, settings and drafts in memory unless a flag names a
//! file; it never opens the user's layout, sidebar, settings, cache or
//! avatars. A live run uses the user's files, or the ones its flags name.

use std::path::{Path, PathBuf};

use crate::layout;
use crate::settings::SettingsFile;

/// The user's own app directories, where a live run keeps its files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserDirs {
    /// `settings.toml`, `config.toml`, `themes/`.
    pub config: Option<PathBuf>,
    /// `layout.json`, `sidebar.json`.
    pub data: Option<PathBuf>,
    /// The cache, avatars and app-owned repositories.
    pub data_local: Option<PathBuf>,
}

impl UserDirs {
    /// The platform's directories, each under `review`.
    #[must_use]
    pub fn platform() -> Self {
        Self {
            config: SettingsFile::config_dir(),
            data: layout::layout_path().and_then(|path| path.parent().map(Path::to_path_buf)),
            data_local: dirs::data_local_dir().map(|dir| dir.join("review")),
        }
    }

    /// All three under `root`, for tests.
    #[must_use]
    pub fn under(root: &Path) -> Self {
        Self {
            config: Some(root.join("config")),
            data: Some(root.join("data")),
            data_local: Some(root.join("local")),
        }
    }

    /// Whether `path` is inside one of the directories.
    #[must_use]
    pub fn contains(&self, path: &Path) -> bool {
        [&self.config, &self.data, &self.data_local]
            .into_iter()
            .flatten()
            .any(|dir| path.starts_with(dir))
    }
}

/// The flags that pick files.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunFlags {
    pub fixtures: bool,
    /// `--layout NAME`: a built-in layout, neither read nor saved.
    pub preset: bool,
    /// `--layout-file`: read the layout from here and save it here.
    pub layout_file: Option<PathBuf>,
    /// `--settings`: read and save the settings here.
    pub settings: Option<PathBuf>,
    /// `--store`: drafts and the pending review in this database.
    pub store: Option<PathBuf>,
    /// `--data-dir`: a live run's cache, avatars and repositories here.
    pub data_dir: Option<PathBuf>,
}

/// The files a run uses; `None` keeps that state in memory.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunFiles {
    /// Read at start and saved on change; the sidebar's file sits beside it.
    pub layout: Option<PathBuf>,
    pub settings: Option<PathBuf>,
    /// User themes, read with the settings.
    pub themes: Option<PathBuf>,
    /// A fixtures run's drafts. A live run keeps its cache under `data_dir`.
    pub store: Option<PathBuf>,
    /// A live run's data dir, overriding the config's.
    pub data_dir: Option<PathBuf>,
}

impl RunFiles {
    /// What `flags` mean on a machine whose user directories are `dirs`.
    #[must_use]
    pub fn plan(flags: &RunFlags, dirs: &UserDirs) -> Self {
        let user_layout = || dirs.data.as_ref().map(|dir| dir.join("layout.json"));
        let layout = match (&flags.layout_file, flags.preset, flags.fixtures) {
            (Some(file), _, _) => Some(file.clone()),
            (None, true, _) | (None, false, true) => None,
            (None, false, false) => user_layout(),
        };
        let settings = match (&flags.settings, flags.fixtures) {
            (Some(file), _) => Some(file.clone()),
            (None, true) => None,
            (None, false) => dirs.config.as_ref().map(|dir| dir.join("settings.toml")),
        };
        // Themes live beside the settings they go with.
        let themes = match (&flags.settings, flags.fixtures) {
            (Some(file), _) => file.parent().map(|dir| dir.join("themes")),
            (None, true) => None,
            (None, false) => dirs.config.as_ref().map(|dir| dir.join("themes")),
        };
        Self {
            layout,
            settings,
            themes,
            store: flags.store.clone().filter(|_| flags.fixtures),
            data_dir: flags.data_dir.clone().filter(|_| !flags.fixtures),
        }
    }

    /// Every file or directory this run may touch.
    pub fn paths(&self) -> impl Iterator<Item = &Path> {
        [
            &self.layout,
            &self.settings,
            &self.themes,
            &self.store,
            &self.data_dir,
        ]
        .into_iter()
        .flatten()
        .map(PathBuf::as_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirs() -> UserDirs {
        UserDirs::under(Path::new("user"))
    }

    #[test]
    fn a_fixtures_run_touches_no_user_file() {
        let plan = RunFiles::plan(
            &RunFlags {
                fixtures: true,
                ..RunFlags::default()
            },
            &dirs(),
        );
        assert_eq!(plan, RunFiles::default(), "everything in memory");
        let scratch = Path::new(".scratch/run");
        let plan = RunFiles::plan(
            &RunFlags {
                fixtures: true,
                layout_file: Some(scratch.join("layout.json")),
                settings: Some(scratch.join("settings.toml")),
                store: Some(scratch.join("drafts.db")),
                data_dir: Some(scratch.join("data")),
                ..RunFlags::default()
            },
            &dirs(),
        );
        assert!(plan.paths().all(|path| !dirs().contains(path)));
        assert_eq!(plan.themes, Some(scratch.join("themes")));
        assert_eq!(plan.data_dir, None, "fixtures have no data dir");
    }

    #[test]
    fn a_live_run_uses_the_user_files_unless_told_otherwise() {
        let live = RunFiles::plan(&RunFlags::default(), &dirs());
        assert_eq!(live.layout, Some(Path::new("user/data/layout.json").into()));
        assert_eq!(
            live.settings,
            Some(Path::new("user/config/settings.toml").into())
        );
        let preset = RunFiles::plan(
            &RunFlags {
                preset: true,
                ..RunFlags::default()
            },
            &dirs(),
        );
        assert_eq!(preset.layout, None);
        let bench = RunFiles::plan(
            &RunFlags {
                layout_file: Some("b/layout.json".into()),
                settings: Some("b/settings.toml".into()),
                data_dir: Some("b/data".into()),
                ..RunFlags::default()
            },
            &dirs(),
        );
        assert!(bench.paths().all(|path| !dirs().contains(path)));
    }
}
