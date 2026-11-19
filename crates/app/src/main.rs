//! `review`: the application shell.

use std::time::{Duration, Instant};

use app::appearance::{self, ThemeFlags};
use app::launch::LaunchProbe;
use app::live::{BootPaths, EarlyBoot};
use app::scenario::{self, Scenario};
use app::session::Session;
use app::shell::Shell;
use app::startup::{RunFiles, RunFlags, UserDirs};
use app::{OpenTarget, Setup, assets, layout, new_dock, window_options};
use gpui::{App, AppContext as _};
use theme::{ModeSetting, ThemeRegistry};

/// Command line flags. They exist for screenshots and repeatable runs.
#[derive(Default)]
struct Args {
    /// Open this pull request: a number (fixtures) or `host/owner/name#N`.
    open_pr: Option<OpenTarget>,
    /// Then open these in a tab group beside it, for side by side shots.
    open_beside: Vec<OpenTarget>,
    /// Open this pull request's conversation in the preview tab.
    preview_pr: Option<OpenTarget>,
    /// Start the conversation's action bar with this menu open:
    /// `review`, `merge` or `more`.
    demo_menu: Option<app::pr_header::Menu>,
    /// Start the conversation's side column with this picker open:
    /// `reviewers`, `assignees` or `labels`.
    demo_picker: Option<app::people::Field>,
    /// Ask to merge the focused pull request, leaving the confirmation up.
    demo_merge: bool,
    /// Invented data instead of the forges.
    fixtures: bool,
    /// A built-in layout name; skips the saved layout and never writes it.
    layout: Option<String>,
    /// Start the composer with sample text.
    demo_composer: bool,
    /// Start with the diff in split layout.
    split_diff: bool,
    /// Quit after this many seconds.
    exit_after: Option<f64>,
    /// A theme family, such as `vercel`.
    theme: Option<String>,
    /// `dark`, `light` or `system`.
    mode: Option<ModeSetting>,
    /// After this many seconds, switch the theme to its other mode, to watch a
    /// live theme change.
    flip_after: Option<f64>,
    /// Open `--open-pr` this many seconds after launch instead of at once,
    /// to time an open in a settled window.
    open_after: Option<f64>,
    /// With `--fixtures`, read and save the settings here instead of keeping
    /// them in memory only.
    settings: Option<std::path::PathBuf>,
    /// Open the settings panel once the window is up.
    open_settings: bool,
    /// With `--fixtures`, keep drafts and the pending review in this
    /// database instead of memory.
    store: Option<std::path::PathBuf>,
    /// Read the dock layout from here and save it here (the sidebar's
    /// beside it). Without it a fixtures run keeps the layout in memory.
    layout_file: Option<std::path::PathBuf>,
    /// A live run's cache, avatars and repositories here instead of the
    /// config's data dir.
    data_dir: Option<std::path::PathBuf>,
    /// Write on the open pull request (see `app::scenario`).
    scenario: Scenario,
    /// Draw at full rate even while another window has focus, as the
    /// benchmarks do, so timings do not wait on the background frame rate.
    unthrottled: bool,
    /// Print the time to the first inbox painted from the cache, then quit
    /// (see `app::launch`).
    report_launch: bool,
    /// CI and watch states for screenshots, in order, once the window is
    /// up: `checks`, `retry`, `log`, `fold`, `watch` (see `app::ci_demo`).
    ci_demo: Vec<String>,
    /// Group the inbox by repository, open the Repos list, hide a
    /// repository.
    demo_inbox: bool,
    /// Drag the first inbox section shorter and fold the second.
    demo_sections: bool,
    /// Open the settings with the interface font's list open.
    demo_font_picker: bool,
    /// Terminal states for screenshots, in order (see `app::terminal_demo`).
    terminal_demo: Vec<String>,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut args = Self::default();
        let mut it = std::env::args().skip(1);
        while let Some(flag) = it.next() {
            let mut value = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
            match flag.as_str() {
                "--open-pr" => {
                    args.open_pr = Some(
                        value("--open-pr")?
                            .parse()
                            .map_err(|e| format!("--open-pr: {e}"))?,
                    )
                }
                "--open-beside" => args.open_beside.push(
                    value("--open-beside")?
                        .parse()
                        .map_err(|e| format!("--open-beside: {e}"))?,
                ),
                "--preview-pr" => {
                    args.preview_pr = Some(
                        value("--preview-pr")?
                            .parse()
                            .map_err(|e| format!("--preview-pr: {e}"))?,
                    );
                }
                "--demo-menu" => {
                    let name = value("--demo-menu")?;
                    args.demo_menu = Some(
                        app::pr_header::Menu::from_name(&name)
                            .ok_or_else(|| format!("--demo-menu: unknown menu {name}"))?,
                    );
                }
                "--demo-picker" => {
                    let name = value("--demo-picker")?;
                    args.demo_picker = Some(
                        app::people::Field::from_name(&name)
                            .ok_or_else(|| format!("--demo-picker: unknown picker {name}"))?,
                    );
                }
                "--demo-merge" => args.demo_merge = true,
                "--demo-inbox" => args.demo_inbox = true,
                "--demo-sections" => args.demo_sections = true,
                "--demo-font-picker" => args.demo_font_picker = true,
                "--fixtures" => args.fixtures = true,
                "--open-settings" => args.open_settings = true,
                "--store" => args.store = Some(value("--store")?.into()),
                "--layout-file" => args.layout_file = Some(value("--layout-file")?.into()),
                "--data-dir" => args.data_dir = Some(value("--data-dir")?.into()),
                "--compose" => args.scenario.compose = Some(value("--compose")?),
                "--compose-preview" => args.scenario.preview = true,
                "--compose-reply" => args.scenario.reply = Some(value("--compose-reply")?),
                "--queue" => args.scenario.queue.push(value("--queue")?),
                "--submit" => args.scenario.submit = true,
                "--send-reply" => args.scenario.send_reply = Some(value("--send-reply")?),
                "--resolve" => args.scenario.resolve = true,
                "--press" => args.scenario.press.push(value("--press")?),
                "--mouse" => args.scenario.mouse.push(value("--mouse")?),
                "--announce-edits" => args.scenario.announce = true,
                "--confirm-apply" => args.scenario.confirm_apply = true,
                "--settings" => args.settings = Some(value("--settings")?.into()),
                "--unthrottled" => args.unthrottled = true,
                "--report-launch" => args.report_launch = true,
                "--ci-demo" => {
                    let step = value("--ci-demo")?;
                    if !app::CI_DEMO_STEPS.contains(&step.as_str()) {
                        return Err(format!(
                            "--ci-demo: unknown step {step}, expected one of {:?}",
                            app::CI_DEMO_STEPS
                        ));
                    }
                    args.ci_demo.push(step);
                }
                "--terminal-demo" => {
                    let step = value("--terminal-demo")?;
                    if !app::TERMINAL_DEMO_STEPS.contains(&step.as_str()) {
                        return Err(format!(
                            "--terminal-demo: unknown step {step}, expected one of {:?}",
                            app::TERMINAL_DEMO_STEPS
                        ));
                    }
                    args.terminal_demo.push(step);
                }
                "--layout" => {
                    let name = value("--layout")?;
                    if !layout::PRESETS.contains(&name.as_str()) {
                        return Err(format!(
                            "--layout: unknown layout {name}, expected one of {:?}",
                            layout::PRESETS
                        ));
                    }
                    args.layout = Some(name);
                }
                "--demo-composer" => args.demo_composer = true,
                "--split-diff" => args.split_diff = true,
                "--exit-after" => {
                    args.exit_after = Some(
                        value("--exit-after")?
                            .parse()
                            .map_err(|e| format!("--exit-after: {e}"))?,
                    );
                }
                "--open-after" => {
                    args.open_after = Some(
                        value("--open-after")?
                            .parse()
                            .map_err(|e| format!("--open-after: {e}"))?,
                    );
                }
                "--flip-after" => {
                    args.flip_after = Some(
                        value("--flip-after")?
                            .parse()
                            .map_err(|e| format!("--flip-after: {e}"))?,
                    );
                }
                "--theme" => {
                    let name = value("--theme")?;
                    let registry = ThemeRegistry::builtin();
                    let families = registry.families();
                    let Some(family) = families
                        .iter()
                        .find(|family| family.name.eq_ignore_ascii_case(&name))
                    else {
                        let known: Vec<_> = families.iter().map(|f| f.name.as_str()).collect();
                        return Err(format!(
                            "--theme: unknown theme {name}, expected one of {known:?}"
                        ));
                    };
                    args.theme = Some(family.name.clone());
                }
                "--mode" => {
                    args.mode = Some(match value("--mode")?.as_str() {
                        "dark" => ModeSetting::Dark,
                        "light" => ModeSetting::Light,
                        "system" => ModeSetting::System,
                        other => {
                            return Err(format!(
                                "--mode: unknown mode {other}, expected dark, light or system"
                            ));
                        }
                    });
                }
                other => return Err(format!("unknown flag {other}")),
            }
        }
        Ok(args)
    }
}

fn main() {
    let started = Instant::now();
    let args = match Args::parse() {
        Ok(args) => args,
        Err(message) => {
            eprintln!("review: {message}");
            std::process::exit(2);
        }
    };
    // With no display, gpui on Linux falls back to a headless platform and
    // the app quits at once with status 0 and no message.
    #[cfg(target_os = "linux")]
    if ["WAYLAND_DISPLAY", "WAYLAND_SOCKET", "DISPLAY"]
        .iter()
        .all(|name| std::env::var_os(name).is_none_or(|value| value.is_empty()))
    {
        eprintln!("review: no display; set WAYLAND_DISPLAY or DISPLAY (or run under xvfb-run)");
        std::process::exit(1);
    }
    let files = RunFiles::plan(
        &RunFlags {
            fixtures: args.fixtures,
            preset: args.layout.is_some(),
            layout_file: args.layout_file.clone(),
            settings: args.settings.clone(),
            store: args.store.clone(),
            data_dir: args.data_dir.clone(),
        },
        &UserDirs::platform(),
    );
    // Config and cache reads overlap the window system's start.
    let boot = (!args.fixtures).then(|| {
        EarlyBoot::start_with(BootPaths {
            settings: files.settings.clone(),
            themes: files.themes.clone(),
            data_dir: files.data_dir.clone(),
        })
    });

    let flip_after = args.flip_after;
    let flags = ThemeFlags {
        family: args.theme.clone(),
        mode: args.mode,
    };
    gpui_kit::application()
        .with_assets(assets::AppAssets)
        .run(move |cx: &mut App| {
            app::init(
                cx,
                &Setup {
                    open_pr: None,
                    demo_composer: args.demo_composer,
                    split_diff: args.split_diff,
                    diff_rows: None,
                    fixture_threads: true,
                    theme: flags.clone(),
                    settings_path: files.settings.clone().filter(|_| args.fixtures),
                    themes_path: files.themes.clone().filter(|_| args.fixtures),
                    fixtures: args.fixtures,
                    store_path: files.store.clone(),
                    started,
                    boot,
                },
            );

            // A preset layout, or a fixtures run without a layout file,
            // neither reads nor saves one.
            let layout_file = files.layout.clone();
            let saved = layout_file.as_deref().and_then(layout::load_from);
            let saved_sidebar = layout_file
                .as_deref()
                .and_then(|file| layout::load_sidebar_from(&layout::sidebar_path(file)));
            let unthrottled = args.unthrottled;
            let open_now = args.open_pr.clone().filter(|_| args.open_after.is_none());
            let open_beside = args.open_beside.clone();
            let preview_pr = args.preview_pr.clone();
            let demo_merge = args.demo_merge;
            cx.set_global(app::pr_header::DemoMenu(args.demo_menu));
            cx.set_global(app::side_column::DemoPicker(args.demo_picker));
            let report_launch = args.report_launch;
            let open_settings = args.open_settings || args.demo_font_picker;
            let demo_font_picker = args.demo_font_picker;
            let demo_inbox = args.demo_inbox;
            let demo_sections = args.demo_sections;
            let columns = args.layout.as_deref() == Some("columns");

            let opened =
                gpui_kit::open_window(window_options(cx, !unthrottled), cx, move |window, cx| {
                    let dock = new_dock(window, cx);
                    if let Some(state) = saved {
                        dock.update(cx, |dock, cx| {
                            dock.load(state, window, cx).expect("a parsed layout loads")
                        });
                    }
                    cx.new(|cx| {
                        let mut shell = Shell::new(dock, layout_file.is_some(), window, cx);
                        if let Some(file) = layout_file {
                            shell = shell.with_layout_file(file);
                        }
                        if let Some(state) = saved_sidebar {
                            shell.load_sidebar(state, window, cx);
                        }
                        if let Some(target) = open_now {
                            cx.defer_in(window, move |shell, _, cx| {
                                shell.open_target(&target, cx);
                                for target in &open_beside {
                                    shell.open_beside(target, cx);
                                }
                            });
                        }
                        if let Some(target) = preview_pr {
                            cx.defer_in(window, move |shell, _, cx| {
                                shell.preview_target(&target, cx);
                            });
                        }
                        if demo_merge {
                            cx.defer_in(window, |_, _, cx| app::demo_merge(cx));
                        }
                        if open_settings {
                            cx.defer_in(window, |shell, window, cx| {
                                shell.show_settings(window, cx)
                            });
                        }
                        if demo_font_picker {
                            cx.defer_in(window, |_, window, cx| {
                                app::settings_panel::demo_font_picker(window, cx);
                            });
                        }
                        if columns {
                            cx.defer_in(window, |shell, window, cx| {
                                shell.columns_preset(window, cx);
                            });
                        }
                        if demo_inbox {
                            cx.defer_in(window, |_, _, cx| app::inbox::demo_tree(cx));
                        }
                        if demo_sections {
                            cx.defer_in(window, |_, _, cx| app::inbox::demo_sections(cx));
                        }
                        if report_launch {
                            let probe = LaunchProbe::new(Session::global(cx), started);
                            shell.with_probe(Box::new(probe))
                        } else {
                            shell
                        }
                    })
                })
                .expect("open the main window");
            cx.activate(true);
            let scripted = args.scenario.compose.is_some()
                || args.scenario.reply.is_some()
                || !args.scenario.queue.is_empty()
                || !args.scenario.mouse.is_empty()
                || args.scenario.submit
                || args.scenario.send_reply.is_some()
                || args.scenario.resolve
                || args.scenario.confirm_apply;
            if scripted {
                scenario::start(args.scenario.clone(), opened.0, cx);
            }

            if !args.ci_demo.is_empty() {
                app::ci_demo(args.ci_demo.clone(), cx);
            }
            if !args.terminal_demo.is_empty() {
                app::terminal_demo(args.terminal_demo.clone(), opened.0, opened.1.clone(), cx);
            }

            if let Some(seconds) = flip_after {
                cx.spawn(async move |cx| {
                    cx.background_executor()
                        .timer(Duration::from_secs_f64(seconds))
                        .await;
                    cx.update(|cx| {
                        let mode = if theme::current(cx).is_dark() {
                            ModeSetting::Light
                        } else {
                            ModeSetting::Dark
                        };
                        appearance::update(cx, |settings| settings.mode = mode);
                    });
                })
                .detach();
            }

            if let (Some(seconds), Some(target)) = (args.open_after, args.open_pr.clone()) {
                let shell = opened.1.clone();
                cx.spawn(async move |cx| {
                    cx.background_executor()
                        .timer(Duration::from_secs_f64(seconds))
                        .await;
                    cx.update(|cx| {
                        shell.update(cx, |shell, cx| shell.open_target(&target, cx));
                    });
                })
                .detach();
            }

            if let Some(seconds) = args.exit_after {
                cx.spawn(async move |cx| {
                    cx.background_executor()
                        .timer(Duration::from_secs_f64(seconds))
                        .await;
                    cx.update(|cx| cx.quit());
                })
                .detach();
            }
        });
}
