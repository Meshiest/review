//! Which theme is on screen: the registry, the user's settings, and what
//! follows the OS. Every change goes through [`update`], which applies it live
//! and saves it.

use std::sync::Arc;
use std::time::Duration;

use gpui::{App, Global, Task, Window, WindowAppearance};
use theme::{ModeSetting, Theme, ThemeRegistry};

use crate::avatars::Avatars;
use crate::session::Session;
use crate::settings::{Settings, SettingsError, SettingsFile, Startup};

/// A change is written this long after the last one, so a held stepper button
/// is one write.
const SAVE_DEBOUNCE: Duration = Duration::from_millis(300);

/// Command-line overrides of the theme; they win over the settings file for
/// this run.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ThemeFlags {
    pub family: Option<String>,
    pub mode: Option<ModeSetting>,
}

impl ThemeFlags {
    fn apply(&self, settings: &mut Settings) {
        if let Some(family) = &self.family {
            settings.theme.clone_from(family);
        }
        if let Some(mode) = self.mode {
            settings.mode = mode;
        }
    }
}

/// Every known theme, the user's settings and where they are saved, as a gpui
/// global.
pub struct Appearance {
    pub registry: ThemeRegistry,
    pub settings: Settings,
    file: Option<Arc<SettingsFile>>,
    flags: ThemeFlags,
    /// Changes not written yet.
    dirty: bool,
    save: Option<Task<()>>,
}

impl Global for Appearance {}

impl Appearance {
    /// Where the settings are saved; `None` for a run that does not save.
    #[must_use]
    pub fn file(&self) -> Option<&SettingsFile> {
        self.file.as_deref()
    }

    fn resolve(&self, appearance: WindowAppearance) -> Arc<Theme> {
        self.settings.resolve(&self.registry, appearance)
    }
}

/// Registers the fonts, installs the built-in themes with default settings
/// (plus `flags`) and applies the result for the OS's current appearance. The
/// user's settings and themes arrive later through [`loaded`].
pub fn install(cx: &mut App, flags: ThemeFlags) {
    theme::fonts::register(cx);
    let mut settings = Settings::default();
    flags.apply(&mut settings);
    let appearance = Appearance {
        registry: ThemeRegistry::builtin(),
        settings,
        file: None,
        flags,
        dirty: false,
        save: None,
    };
    let selected = appearance.resolve(cx.window_appearance());
    cx.set_global(appearance);
    theme::apply(cx, selected);
    // A change made in the last moments before quitting is still written.
    cx.on_app_quit(|cx| {
        let pending = cx.try_global::<Appearance>().and_then(|state| {
            let file = state.file.clone()?;
            state.dirty.then(|| (file, state.settings.clone()))
        });
        let executor = cx.background_executor().clone();
        async move {
            if let Some((file, settings)) = pending {
                let _ = executor.spawn(async move { file.save(&settings) }).await;
            }
        }
    })
    .detach();
}

/// The user's settings and themes were read: use them, and report what was
/// wrong with them. Returns the status-bar line, if any.
pub fn loaded(cx: &mut App, startup: Startup) -> Option<String> {
    let notice = startup.notice();
    let Startup {
        registry,
        mut settings,
        file,
        ..
    } = startup;
    let state = cx.global_mut::<Appearance>();
    // What was changed before the file arrived stays: only an untouched
    // session takes the file's settings.
    if !state.dirty {
        state.flags.apply(&mut settings);
        state.settings = settings;
    }
    state.registry = registry;
    state.file = file.map(Arc::new);
    let unsaved = state.dirty;
    sync_avatars(cx);
    refresh(cx);
    if unsaved {
        schedule_save(cx);
    }
    notice
}

/// Reads the settings and user themes off the main thread and applies them,
/// for runs that did not get them from the boot thread.
pub fn load_in_background(cx: &mut App, settings: Option<std::path::PathBuf>) {
    load_in_background_from(cx, settings, SettingsFile::themes_dir());
}

/// [`load_in_background`] with the user themes read from `themes`.
pub fn load_in_background_from(
    cx: &mut App,
    settings: Option<std::path::PathBuf>,
    themes: Option<std::path::PathBuf>,
) {
    cx.spawn(async move |cx| {
        let startup = cx
            .background_executor()
            .spawn(async move { Startup::read(settings, themes.as_deref()) })
            .await;
        cx.update(|cx| {
            if let Some(notice) = loaded(cx, startup) {
                report(cx, notice);
            }
        });
    })
    .detach();
}

fn report(cx: &mut App, notice: String) {
    if let Some(session) = Session::try_global(cx) {
        session.update(cx, |session, cx| {
            session.notice = Some(notice.into());
            cx.notify();
        });
    }
}

/// Re-resolves the theme from the current settings and applies it when it
/// differs from the one on screen.
fn refresh(cx: &mut App) {
    let next = cx.global::<Appearance>().resolve(cx.window_appearance());
    if *next != *theme::current(cx) {
        theme::apply(cx, next);
    }
}

/// Changes the settings, applies them to every window and saves them.
pub fn update(cx: &mut App, change: impl FnOnce(&mut Settings)) {
    let state = cx.global_mut::<Appearance>();
    let before = state.settings.clone();
    change(&mut state.settings);
    if state.settings == before {
        return;
    }
    state.dirty = true;
    sync_avatars(cx);
    refresh(cx);
    schedule_save(cx);
}

/// Changes settings that do not touch the theme (what the inbox shows) and
/// saves them, without restyling every window.
pub fn update_quiet(cx: &mut App, change: impl FnOnce(&mut Settings)) {
    let state = cx.global_mut::<Appearance>();
    let before = state.settings.clone();
    change(&mut state.settings);
    if state.settings == before {
        return;
    }
    state.dirty = true;
    schedule_save(cx);
}

/// Passes the Gravatar setting on to the avatar store.
fn sync_avatars(cx: &mut App) {
    let allow = cx.global::<Appearance>().settings.load_gravatar;
    if let Some(avatars) = cx.try_global::<Avatars>().cloned() {
        avatars.set_gravatar(allow, cx);
    }
}

fn schedule_save(cx: &mut App) {
    let Some(file) = cx.global::<Appearance>().file.clone() else {
        return;
    };
    let task = cx.spawn(async move |cx| {
        cx.background_executor().timer(SAVE_DEBOUNCE).await;
        let settings = cx.update(|cx| cx.global::<Appearance>().settings.clone());
        let saved = settings.clone();
        let result = cx
            .background_executor()
            .spawn(async move { file.save(&settings) })
            .await;
        cx.update(|cx| match result {
            Ok(()) => {
                let state = cx.global_mut::<Appearance>();
                state.dirty = state.settings != saved;
            }
            Err(error) => report_save_error(cx, &error),
        });
    });
    cx.global_mut::<Appearance>().save = Some(task);
}

fn report_save_error(cx: &mut App, error: &SettingsError) {
    // The text is still the user's: stop retrying, keep the change for this run.
    cx.global_mut::<Appearance>().dirty = false;
    report(cx, error.to_string());
}

/// Follows the OS light/dark setting while the mode is `System`. The window
/// reports the change; the subscription lives as long as the window.
pub fn follow_os(window: &mut Window, cx: &mut App) {
    window
        .observe_window_appearance(|window, cx| restyle(window.appearance(), cx))
        .detach();
    // This window may already differ from the application-wide appearance read
    // at install time.
    restyle(window.appearance(), cx);
}

fn restyle(appearance: WindowAppearance, cx: &mut App) {
    let state = cx.global::<Appearance>();
    if state.settings.mode != ModeSetting::System {
        return;
    }
    let next = state.resolve(appearance);
    if *next != *theme::current(cx) {
        theme::apply(cx, next);
    }
}
