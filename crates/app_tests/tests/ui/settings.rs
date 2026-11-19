//! Saved settings reach a real window: when the file is read, the theme
//! global switches and the open diff restyles to the buffer font. A change
//! made in the window is saved and comes back on the next start.

use std::time::Duration;

use app::appearance::{self, Appearance};
use app::settings::{FontOverride, Settings, SettingsFile};
use gpui::{App, TestAppContext, px};
use theme::{Mode, ModeSetting};

use crate::harness::{Options, Scratch, ShellWindow, open_shell};

fn saved() -> Settings {
    Settings {
        theme: "vercel".into(),
        mode: ModeSetting::Light,
        buffer_font: FontOverride {
            family: Some("Cascadia Mono".into()),
            size: Some(15.0),
            line_height: Some(23),
            fallbacks: None,
        },
        interface_font: FontOverride {
            size: Some(14.0),
            ..FontOverride::default()
        },
        ..Settings::default()
    }
}

/// The height the open diff gives a code row.
fn code_row_height(shell: &mut ShellWindow) -> f32 {
    let diff = shell.diff_panel();
    shell.update(|_, cx| {
        let view = diff.read(cx).view().cloned().expect("the diff is built");
        view.read(cx).list().heights().code
    })
}

fn load(shell: &mut ShellWindow, settings: &std::path::Path, themes: &std::path::Path) {
    let (settings, themes) = (settings.to_owned(), themes.to_owned());
    shell.update(|_, cx| appearance::load_in_background_from(cx, Some(settings), Some(themes)));
    shell.settle();
}

fn settings_of(cx: &App) -> Settings {
    cx.global::<Appearance>().settings.clone()
}

#[gpui::test]
fn saved_settings_restyle_the_open_window(first: &mut TestAppContext, next: &mut TestAppContext) {
    let scratch = Scratch::new("settings-restore");
    let path = scratch.join("settings.toml");
    let themes = scratch.join("themes");
    std::fs::create_dir_all(&themes).expect("create the themes directory");
    SettingsFile::load(path.clone())
        .file
        .save(&saved())
        .expect("write the settings file");

    let mut shell = open_shell(
        first,
        Options {
            open_pr: Some(41),
            ..Options::default()
        },
    );
    let before = shell.update(|_, cx| theme::current(cx).clone());
    let row_before = code_row_height(&mut shell);
    assert_eq!(row_before, f32::from(before.fonts.buffer.line_height_px()));

    load(&mut shell, &path, &themes);
    let theme = shell.update(|_, cx| theme::current(cx).clone());
    assert_eq!(shell.update(|_, cx| settings_of(cx)), saved());
    assert_eq!(
        (theme.id.as_str(), theme.mode),
        ("vercel-light", Mode::Light)
    );
    assert_eq!(theme.fonts.buffer.family, "Cascadia Mono");
    assert_eq!(theme.fonts.interface.size, 14.0);
    assert_eq!(
        code_row_height(&mut shell),
        23.0,
        "the diff in the window took the buffer line height"
    );
    assert_ne!(row_before, 23.0);
    // The settings panel changes settings through `appearance::update`.
    shell.update(|_, cx| appearance::update(cx, |settings| settings.mode = ModeSetting::Dark));
    shell.settle();
    let dark = shell.update(|_, cx| theme::current(cx).clone());
    assert_eq!(dark.id, "vercel-dark", "the window follows at once");
    shell.advance(Duration::from_secs(1));
    let written = SettingsFile::load(path.clone()).settings;
    assert_eq!(written.mode, ModeSetting::Dark, "the change was saved");

    let mut restarted = open_shell(
        next,
        Options {
            open_pr: Some(41),
            ..Options::default()
        },
    );
    load(&mut restarted, &path, &themes);
    let theme = restarted.update(|_, cx| theme::current(cx).clone());
    assert_eq!(theme.id, "vercel-dark");
    assert_eq!(code_row_height(&mut restarted), 23.0);
}

/// The settings panel, once it is open.
fn settings_panel(shell: &mut ShellWindow) -> gpui::Entity<app::settings_panel::SettingsPanel> {
    shell
        .update(|_, cx| app::settings_panel::SettingsPanel::existing(cx))
        .expect("the settings are open")
}

fn picker(shell: &mut ShellWindow) -> Option<&'static str> {
    let panel = settings_panel(shell);
    shell.update(|_, cx| panel.read(cx).open_picker())
}

#[gpui::test]
fn the_font_list_floats_over_the_form(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    shell.keys("ctrl-,");
    let below = shell.bounds_of("family-buffer-font");

    shell.click("family-interface-font");
    assert_eq!(picker(&mut shell), Some("interface-font"));
    assert!(shell.drawn("font-picker"));
    assert_eq!(
        shell.bounds_of("family-buffer-font"),
        below,
        "the form under it did not move"
    );
    let panel = settings_panel(&mut shell);
    assert!(
        shell.update(|window, cx| panel.read(cx).search_has_focus(window, cx)),
        "typing goes to the search box"
    );
    let button = shell.bounds_of("family-interface-font");
    let list = shell.bounds_of("font-picker");
    assert!(list.top() >= button.bottom(), "it hangs under its button");

    shell.keys("escape");
    assert_eq!(picker(&mut shell), None, "Esc closes it");

    shell.click("family-interface-font");
    let outside = gpui::point(button.right() + px(300.0), button.top() - px(40.0));
    shell.click_at(outside);
    assert_eq!(picker(&mut shell), None, "a click outside closes it");
}

#[gpui::test]
fn the_font_list_flips_up_near_the_bottom(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    shell.keys("ctrl-,");
    shell.click("family-interface-font");
    let (button, list) = (
        shell.bounds_of("family-interface-font"),
        shell.bounds_of("font-picker"),
    );
    shell.keys("escape");
    // Too short for the list under the button, with room above it.
    let height = f32::from(button.bottom() + list.size.height / 2.0) + 30.0;
    shell.resize(1440.0, height);
    shell.click("family-interface-font");
    let button = shell.bounds_of("family-interface-font");
    let list = shell.bounds_of("font-picker");
    assert!(
        list.bottom() <= button.top(),
        "no room below, so it opens upward: {list:?} over {button:?}"
    );
}
