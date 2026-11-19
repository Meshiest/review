//! Settings the user changed survive a restart: they are saved through the
//! path the app saves with and read back through the one startup uses, into
//! settings equal to the saved ones that resolve to the same theme and fonts.
//! A malformed file is reported and never overwritten.
//!
//! The files live under `.scratch/settings-restore/`, never in the user's
//! config directory.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use app::settings::{FontOverride, Settings, SettingsError, SettingsFile, Startup};
use gpui::WindowAppearance;
use theme::{Mode, ModeSetting};

const USER_THEME: &str = r##"
name = "Mine"
mode = "dark"
extends = "vercel-dark"

[colors]
accent = "#ff8800"
"##;

/// A fresh directory for this run under the workspace's `.scratch`.
fn scratch_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("after the epoch")
        .as_nanos();
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../.scratch/settings-restore")
        .join(format!("{}-{nanos}", std::process::id()))
}

fn changed() -> Settings {
    Settings {
        theme: "vercel".into(),
        mode: ModeSetting::Light,
        load_gravatar: true,
        interface_font: FontOverride {
            family: Some("Segoe UI".into()),
            size: Some(14.0),
            line_height: Some(20),
            fallbacks: None,
        },
        buffer_font: FontOverride {
            family: Some("Cascadia Mono".into()),
            size: Some(12.0),
            line_height: Some(18),
            fallbacks: Some(vec!["Consolas".into(), "Menlo".into()]),
        },
        notifications: app::settings::NotificationSettings {
            ci_finished: false,
            ..app::settings::NotificationSettings::default()
        },
        inbox: app::settings::InboxSettings {
            hidden_repos: vec!["github.com/acme/web".into()],
            ..app::settings::InboxSettings::default()
        },
        conversation: app::settings::ConversationSettings {
            threads: app::settings::ThreadStyle::Compact,
        },
        diff: app::settings::DiffSettings {
            split_ratio: Some(0.4),
            collapse_viewed: false,
        },
    }
}

/// Reads the way startup does, and resolves for a dark OS.
fn restore(settings_path: &Path, themes: &Path) -> (Settings, std::sync::Arc<theme::Theme>) {
    let startup = Startup::read(Some(settings_path.to_path_buf()), Some(themes));
    assert!(startup.problems.is_empty(), "{:?}", startup.problems);
    let theme = startup
        .settings
        .resolve(&startup.registry, WindowAppearance::Dark);
    (startup.settings, theme)
}

#[test]
fn changed_settings_are_restored_equal() {
    let dir = scratch_dir();
    let path = dir.join("settings.toml");
    let themes = dir.join("themes");
    fs::create_dir_all(&themes).expect("create the scratch directory");
    fs::write(themes.join("mine.toml"), USER_THEME).expect("write a user theme");

    let defaults = Startup::read(Some(path.clone()), Some(&themes));
    assert_eq!(defaults.settings, Settings::default(), "no file: defaults");
    let default_theme = defaults
        .settings
        .resolve(&defaults.registry, WindowAppearance::Dark);

    let saved = changed();
    assert_ne!(saved, Settings::default());
    let file = defaults.file.expect("the settings file is writable");
    file.save(&saved).expect("the settings are saved");
    assert!(
        !path.with_extension("toml.tmp").exists(),
        "written through a temp file that is renamed into place"
    );

    let (restored, theme) = restore(&path, &themes);
    assert_eq!(restored, saved);
    assert_eq!(
        theme,
        saved.resolve(&defaults.registry, WindowAppearance::Dark),
        "the restored settings resolve to the same theme"
    );
    assert_eq!(theme.id.as_str(), "vercel-light");
    assert_eq!(theme.mode, Mode::Light, "an explicit mode ignores the OS");
    assert_ne!(theme, default_theme);
    assert_eq!(theme.fonts.interface.family, "Segoe UI");
    assert_eq!(
        (
            theme.fonts.interface.size,
            theme.fonts.interface.line_height
        ),
        (14.0, 20)
    );
    assert_eq!(theme.fonts.buffer.family, "Cascadia Mono");
    assert_eq!(theme.fonts.buffer.fallbacks, ["Consolas", "Menlo"]);
    assert_eq!(
        theme.fonts.interface.fallbacks,
        defaults
            .registry
            .select("vercel", ModeSetting::Light, WindowAppearance::Light)
            .fonts
            .interface
            .fallbacks,
        "a key the user never set keeps the theme's value"
    );

    // A theme from the user's folder is chosen and restored too.
    let user = Settings {
        theme: "Mine".into(),
        mode: ModeSetting::System,
        ..changed()
    };
    file.save(&user).expect("the settings are saved again");
    let (restored, theme) = restore(&path, &themes);
    assert_eq!(restored, user);
    assert_eq!(theme.id, "mine");
    assert_eq!(
        theme.colors.accent,
        defaults.registry.get("mine").unwrap().colors.accent
    );
    assert_ne!(
        theme.colors.accent,
        defaults.registry.get("vercel-dark").unwrap().colors.accent
    );

    fs::remove_dir_all(&dir).expect("remove the scratch directory");
}

#[test]
fn a_malformed_file_is_reported_and_left_untouched() {
    let dir = scratch_dir();
    fs::create_dir_all(&dir).expect("create the scratch directory");
    let path = dir.join("settings.toml");
    let broken = "theme = \"vercel\"\nmode = \n[buffer_font\nsize = 99\n";
    fs::write(&path, broken).expect("write a broken file");

    let startup = Startup::read(Some(path.clone()), None);
    assert_eq!(startup.settings, Settings::default(), "defaults are used");
    assert_eq!(startup.problems.len(), 1);
    assert!(
        startup
            .notice()
            .is_some_and(|notice| notice.contains("settings.toml")),
        "the status bar names the file"
    );
    let file = startup.file.expect("a file to refuse to write");
    assert!(!file.writable());
    assert!(matches!(
        file.save(&changed()),
        Err(SettingsError::Protected { .. })
    ));
    assert_eq!(fs::read_to_string(&path).unwrap(), broken, "left untouched");
    assert!(!path.with_extension("toml.tmp").exists());

    fs::remove_dir_all(&dir).expect("remove the scratch directory");
}

#[test]
fn a_file_broken_by_hand_after_startup_is_not_overwritten() {
    let dir = scratch_dir();
    let path = dir.join("settings.toml");
    let file = SettingsFile::load(path.clone()).file;
    file.save(&changed()).expect("the first save works");

    let edited = "theme = \"vercel\"\nmode = \"sepia\"\n";
    fs::write(&path, edited).expect("the user edits the file");
    assert!(matches!(
        file.save(&Settings::default()),
        Err(SettingsError::Protected { .. })
    ));
    assert_eq!(fs::read_to_string(&path).unwrap(), edited);

    fs::remove_dir_all(&dir).expect("remove the scratch directory");
}
