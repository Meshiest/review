//! `review`: the application shell, on the user's forges or, with
//! `--fixtures`, on invented data.
//!
//! The binary in `main.rs` and the benchmarks in `examples/` share this
//! library, so a benchmark measures the same shell the user runs.

pub mod agent;
pub mod appearance;
pub mod assets;
pub mod avatars;
pub mod checklist;
pub mod ci;
pub mod ci_settle;
pub mod claude_review;
pub mod commands;
pub mod commits;
pub mod composer;
pub mod config;
pub mod conversation;
pub mod diff_panel;
pub mod diff_style;
pub mod dir_browser;
pub mod dock_skin;
pub mod drafts;
pub mod find;
pub mod fixtures;
pub mod forge_sink;
pub mod gutter;
pub mod inbox;
pub mod launch;
pub mod layout;
pub mod lifecycle;
pub mod live;
pub mod log_panel;
pub mod machines;
pub mod model;
pub mod notify;
pub mod opener;
pub mod palette;
pub mod panels;
pub mod people;
pub mod pr_header;
pub mod references;
pub mod review_bar;
pub mod scenario;
pub mod services;
pub mod session;
pub mod settings;
pub mod settings_panel;
pub mod shell;
pub mod side_column;
pub mod sidebar;
pub mod sidebar_panels;
pub mod sink;
pub mod startup;
pub mod subscriptions;
pub mod suggestions;
pub mod tab_menu;
pub mod terminal_panel;
pub mod terminals;
pub mod thread_code;
pub mod threads;
pub mod ui;
pub mod untrusted;
pub mod watching;
pub mod workspaces;
pub mod worktrees;
pub mod writing;

use std::time::Instant;

use crate::appearance::ThemeFlags;
use crate::diff_panel::{
    NextFile, NextHunk, NextInStack, NextThread, OpenComposer, OpenRangeComposer, PrevFile,
    PrevHunk, PrevInStack, PrevThread, ToggleThread,
};
use crate::model::{PrKey, PrNumber, RepoRef};
use crate::session::Session;
use crate::shell::{
    CloseKeyHelp, CloseTab, NextTab, OpenSettings, PrevTab, ToggleDiffLayout, ToggleKeyHelp,
    ToggleSidebar,
};
use crate::sidebar::{OpenBeside, OpenSelected, SelectNext, SelectPrev, ToggleViewed};
use diff::Layout;
use gpui::{
    App, Bounds, Entity, KeyBinding, TitlebarOptions, Window, WindowBounds, WindowOptions, px, size,
};
use gpui_kit::component::{TitleBar, dock::DockArea};

/// Which pull request to open at startup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpenTarget {
    /// The first inbox row with this number (fixtures).
    Number(u64),
    Key(PrKey),
}

impl std::str::FromStr for OpenTarget {
    type Err = String;

    /// `41`, or `host/owner/name#5` (GitLab owners may be nested groups).
    fn from_str(text: &str) -> Result<Self, String> {
        if let Ok(number) = text.parse() {
            return Ok(Self::Number(number));
        }
        let bad = || format!("expected N or host/owner/name#N, got {text}");
        let (path, number) = text.rsplit_once(['#', '!']).ok_or_else(bad)?;
        let number = number.parse().map_err(|_| bad())?;
        let (host, rest) = path.split_once('/').ok_or_else(bad)?;
        let (owner, name) = rest.rsplit_once('/').ok_or_else(bad)?;
        if host.is_empty() || owner.is_empty() || name.is_empty() {
            return Err(bad());
        }
        Ok(Self::Key(PrKey::new(
            RepoRef::new(host, owner, name),
            PrNumber(number),
        )))
    }
}

/// What the app needs before its first window.
pub struct Setup {
    /// Open this pull request.
    pub open_pr: Option<OpenTarget>,
    /// Start the composer with sample text.
    pub demo_composer: bool,
    /// Start with the diff in split layout.
    pub split_diff: bool,
    /// Size of the synthetic fixture diff; `None` for the small per-pull-request one.
    pub diff_rows: Option<usize>,
    /// Review threads on the fixture pull requests; off to measure a
    /// benchmark without thread cards.
    pub fixture_threads: bool,
    /// A theme family and mode that win over the settings file for this run.
    pub theme: ThemeFlags,
    /// A fixtures run reads and saves its settings here; without it the run
    /// keeps them in memory only, so it never touches the user's file.
    pub settings_path: Option<std::path::PathBuf>,
    /// User themes read with `settings_path` in a fixtures run; `None` reads
    /// none.
    pub themes_path: Option<std::path::PathBuf>,
    /// Invented data instead of the forges.
    pub fixtures: bool,
    /// With `fixtures`, keep drafts and the pending review in this database;
    /// without it they live in memory. A live run uses the app's cache.
    pub store_path: Option<std::path::PathBuf>,
    /// When the process started, for the launch-to-inbox time.
    pub started: Instant,
    /// The start against the forges, begun before the app ran; `None`
    /// starts it in `init`.
    pub boot: Option<live::EarlyBoot>,
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            open_pr: None,
            demo_composer: false,
            split_diff: false,
            diff_rows: None,
            fixture_threads: true,
            theme: ThemeFlags::default(),
            settings_path: None,
            themes_path: None,
            fixtures: false,
            store_path: None,
            started: Instant::now(),
            boot: None,
        }
    }
}

/// The diff panel's single-key bindings: its key context without a text box
/// inside it, since composers are drawn within the diff.
const DIFF_KEYS: &str = "DiffPanel && !Input";
/// The sidebar's single-key bindings: not while its filter box has focus.
const SIDEBAR_KEYS: &str = "Sidebar && !Input";
/// The conversation's keys: not while its comment box has focus.
const CONVERSATION_KEYS: &str = "Conversation && !Input";
/// Window keys a terminal program also uses (Ctrl+B, Ctrl+W): they reach
/// the program while a terminal has focus.
const SHELL_KEYS: &str = "Shell && !Terminal";
/// Single keys anywhere in the window: not while a text box or a terminal
/// takes the typing.
const SHELL_TEXT_KEYS: &str = "Shell && !Terminal && !Input";
/// Find: Cmd+F on macOS, Ctrl+F elsewhere.
const FIND: &str = if cfg!(target_os = "macos") {
    "cmd-f"
} else {
    "ctrl-f"
};
/// `Esc` puts a zoomed dock back, unless something inside takes it first
/// (a picker, a comment box) or a terminal program has the keys.
const ZOOMED_KEYS: &str = "Zoomed && !Terminal";

/// Binds every key. What each does, for the `?` overlay and the command
/// palette, is in [`commands::COMMANDS`].
fn bind_keys(cx: &mut App) {
    // Not while a text box has focus, so typing a letter still types it.
    let diff = Some(DIFF_KEYS);
    let sidebar = Some(SIDEBAR_KEYS);
    let conversation = Some(CONVERSATION_KEYS);
    let find_bar = Some(find::KEY_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("] f", NextFile, diff),
        KeyBinding::new("[ f", PrevFile, diff),
        KeyBinding::new("alt-down", NextFile, diff),
        KeyBinding::new("alt-up", PrevFile, diff),
        KeyBinding::new("n", NextHunk, diff),
        KeyBinding::new("p", PrevHunk, diff),
        KeyBinding::new("] t", NextThread, diff),
        KeyBinding::new("[ t", PrevThread, diff),
        KeyBinding::new("x", ToggleThread, diff),
        KeyBinding::new("v", diff_panel::ToggleFileViewed, diff),
        KeyBinding::new("] s", NextInStack, diff),
        KeyBinding::new("[ s", PrevInStack, diff),
        KeyBinding::new("] s", NextInStack, sidebar),
        KeyBinding::new("[ s", PrevInStack, sidebar),
        // Find, in the diff or the conversation; not from a comment box, whose
        // own Ctrl+F is its own. In the bar it selects the query again.
        KeyBinding::new(FIND, find::OpenFind, diff),
        KeyBinding::new(FIND, find::OpenFind, conversation),
        KeyBinding::new(FIND, find::OpenFind, find_bar),
        KeyBinding::new("f3", find::FindNext, diff),
        KeyBinding::new("f3", find::FindNext, conversation),
        KeyBinding::new("f3", find::FindNext, find_bar),
        KeyBinding::new("shift-f3", find::FindPrev, diff),
        KeyBinding::new("shift-f3", find::FindPrev, conversation),
        KeyBinding::new("shift-f3", find::FindPrev, find_bar),
        KeyBinding::new("alt-c", find::ToggleFindCase, find_bar),
        KeyBinding::new("c", OpenComposer, diff),
        KeyBinding::new("shift-c", OpenRangeComposer, diff),
        KeyBinding::new(
            "ctrl-enter",
            composer::SubmitPrimary,
            Some(composer::KEY_CONTEXT),
        ),
        KeyBinding::new(
            "ctrl-shift-enter",
            composer::SubmitNow,
            Some(composer::KEY_CONTEXT),
        ),
        // Anywhere but a text box or a terminal, even with nothing focused
        // (every tab closed): the keys then go to the shell itself.
        KeyBinding::new("?", ToggleKeyHelp, Some(SHELL_TEXT_KEYS)),
        KeyBinding::new("?", ToggleKeyHelp, Some(shell::KEY_HELP_CONTEXT)),
        KeyBinding::new("escape", CloseKeyHelp, Some(shell::KEY_HELP_CONTEXT)),
        KeyBinding::new("escape", inbox::ClearFilter, Some(inbox::FILTER_CONTEXT)),
        KeyBinding::new(
            "escape",
            settings_panel::ClosePicker,
            Some(settings_panel::PICKER_CONTEXT),
        ),
        KeyBinding::new("j", SelectNext, sidebar),
        KeyBinding::new("down", SelectNext, sidebar),
        KeyBinding::new("k", SelectPrev, sidebar),
        KeyBinding::new("up", SelectPrev, sidebar),
        KeyBinding::new("v", ToggleViewed, sidebar),
        KeyBinding::new("enter", OpenSelected, sidebar),
        KeyBinding::new("enter", inbox::ToggleFold, Some(inbox::FOLD_CONTEXT)),
        KeyBinding::new("space", inbox::ToggleFold, Some(inbox::FOLD_CONTEXT)),
        KeyBinding::new("ctrl-enter", OpenBeside, sidebar),
        KeyBinding::new("ctrl-b", ToggleSidebar, Some(SHELL_KEYS)),
        KeyBinding::new("ctrl-,", OpenSettings, Some(shell::KEY_CONTEXT)),
        KeyBinding::new("ctrl-w", CloseTab, Some(SHELL_KEYS)),
        // Window-wide, terminals and text boxes included: no terminal program
        // or text field needs Ctrl+Tab.
        KeyBinding::new("ctrl-tab", NextTab, Some(shell::KEY_CONTEXT)),
        KeyBinding::new("ctrl-shift-tab", PrevTab, Some(shell::KEY_CONTEXT)),
        KeyBinding::new(
            "shift-escape",
            gpui_kit::component::dock::ToggleZoom,
            Some(SHELL_KEYS),
        ),
        KeyBinding::new("escape", shell::ZoomOut, Some(ZOOMED_KEYS)),
        KeyBinding::new("ctrl-`", terminals::NewTerminal, Some(shell::KEY_CONTEXT)),
        KeyBinding::new(
            "ctrl-shift-p",
            shell::OpenCommandPalette,
            Some(shell::KEY_CONTEXT),
        ),
        // A terminal program keeps Ctrl+P (previous history line).
        KeyBinding::new("ctrl-p", shell::QuickOpen, Some(SHELL_KEYS)),
        // A terminal program keeps Alt+arrows; a text box on macOS moves by
        // word with them, its own binding being deeper.
        KeyBinding::new("alt-left", shell::GoBack, Some(SHELL_KEYS)),
        KeyBinding::new("alt-right", shell::GoForward, Some(SHELL_KEYS)),
        KeyBinding::new("up", terminals::PickerUp, Some(terminals::PICKER_CONTEXT)),
        KeyBinding::new(
            "down",
            terminals::PickerDown,
            Some(terminals::PICKER_CONTEXT),
        ),
        KeyBinding::new(
            "enter",
            terminals::PickerConfirm,
            Some(terminals::PICKER_CONTEXT),
        ),
        KeyBinding::new(
            "escape",
            terminals::PickerCancel,
            Some(terminals::PICKER_CONTEXT),
        ),
        // Only where focus is not a text field, so typing an "s" still types.
        KeyBinding::new("s", ToggleDiffLayout, sidebar),
        KeyBinding::new("s", ToggleDiffLayout, diff),
    ]);
}

/// Initializes the UI layers, the theme, the panels and the session.
pub fn init(cx: &mut App, setup: &Setup) {
    gpui_kit::init(cx);
    term_view::init(cx);
    appearance::install(cx, setup.theme.clone());
    bind_keys(cx);
    diff_panel::init(cx);
    let avatars = avatars::Avatars::install(cx);
    panels::register(cx);
    cx.set_global(panels::DemoComposer(setup.demo_composer));
    keep_drafts_on_quit(cx);

    let session = if setup.fixtures {
        avatars.attach_fixtures();
        if let Some(path) = setup.settings_path.clone() {
            appearance::load_in_background_from(cx, Some(path), setup.themes_path.clone());
        }
        let store = setup.store_path.as_ref().and_then(|path| {
            store::Store::open(path)
                .map_err(|error| eprintln!("review: could not open {}: {error}", path.display()))
                .ok()
        });
        Session::install_fixtures(setup.diff_rows, setup.fixture_threads, store, cx)
    } else {
        let session = Session::install_live(cx);
        live::start(session.clone(), setup.started, setup.boot.as_ref(), cx);
        session
    };
    subscriptions::Subscriptions::install(session.clone(), cx);
    terminals::Terminals::install(setup.fixtures, cx);
    let storage = workspaces::storage(setup.fixtures, setup.settings_path.as_deref());
    let workspaces = workspaces::Workspaces::install(storage, cx);
    if setup.fixtures {
        workspaces.update(cx, workspaces::Workspaces::load_file_in_background);
    }
    claude_review::AgentReview::install(setup.fixtures, cx);
    if setup.demo_composer {
        session.update(cx, |session, cx| {
            session.seed_conversation(Some(panels::DEMO_COMPOSER_TEXT), cx);
        });
    }
    if let Some(target) = setup.open_pr.clone() {
        session.update(cx, |session, cx| session.open_target(&target, cx));
    }
    if setup.split_diff {
        session.update(cx, |session, cx| session.set_diff_layout(Layout::Split, cx));
    }
}

/// The steps `--ci-demo` takes.
pub const CI_DEMO_STEPS: &[&str] = &["checks", "retry", "log", "fold", "watch"];

/// Puts the open pull request's CI into states worth a screenshot, one
/// step every half second after the window settles: the Conversation's
/// pipelines card open, the re-run confirmation, the first failed job's
/// log, a watch event.
pub fn ci_demo(steps: Vec<String>, cx: &mut App) {
    cx.spawn(async move |cx| {
        cx.background_executor()
            .timer(std::time::Duration::from_millis(1500))
            .await;
        for step in steps {
            cx.update(|cx| ci_demo_step(&step, cx));
            cx.background_executor()
                .timer(std::time::Duration::from_millis(500))
                .await;
        }
    })
    .detach();
}

fn ci_demo_step(step: &str, cx: &mut App) {
    use forge::model::CiState;

    let session = Session::global(cx);
    if step == "fold" {
        log_panel::LogPanel::fold_all(cx);
        return;
    }
    if step == "watch" {
        if let Some(subscriptions) = subscriptions::Subscriptions::global(cx) {
            subscriptions.update(cx, |s, cx| s.demo(cx));
        }
        return;
    }
    session.update(cx, |session, cx| {
        let Some(open) = session.open_pr() else {
            return;
        };
        let runs = open.ci.runs.clone();
        let failed_job = runs
            .iter()
            .flat_map(|run| &run.jobs)
            .find(|job| job.state == CiState::Fail)
            .map(|job| job.id.clone());
        match step {
            "checks" => {
                let key = open.key.clone();
                session.show_pipelines(&key, session::OpenMode::Permanent, cx);
            }
            "retry" => {
                if let Some(run) = runs.iter().max_by_key(|run| ci::failed_jobs(run).count()) {
                    let retry = ci::Retry::FailedJobs {
                        run: run.id.clone(),
                    };
                    session.request_retry(retry, cx);
                }
            }
            "log" => {
                if let Some(job) = failed_job {
                    session.open_log(&job, None, cx);
                }
            }
            _ => {}
        }
    });
}

/// The steps `--terminal-demo` takes.
pub const TERMINAL_DEMO_STEPS: &[&str] = &["split", "right", "picker", "open-on"];

/// Terminals for screenshots, three seconds after the window is up (after
/// `--ci-demo`'s log): `split` and `right` open a local `cmd` beside the
/// diff or the CI log, `picker` opens the machine picker, `open-on` opens
/// the focused pull request on this PC (its worktree, then `claude`).
pub fn terminal_demo(
    steps: Vec<String>,
    window: gpui::AnyWindowHandle,
    shell: Entity<shell::Shell>,
    cx: &mut App,
) {
    use gpui::AppContext as _;
    use terminals::{Purpose, TerminalSpec, Terminals, Where};

    cx.spawn(async move |cx| {
        cx.background_executor()
            .timer(std::time::Duration::from_secs(3))
            .await;
        for step in steps {
            let shell = shell.clone();
            let _ = cx.update_window(window, move |_, window, cx| {
                let spec = TerminalSpec {
                    machine: machines::THIS_PC.to_owned(),
                    shell: Some("cmd".to_owned()),
                    command: Some("git --version".to_owned()),
                    ..TerminalSpec::default()
                };
                let Some(terminals) = Terminals::global(cx) else {
                    return;
                };
                match step.as_str() {
                    "split" => shell.update(cx, |shell, cx| {
                        shell.add_terminal(spec, Where::Split, window, cx);
                    }),
                    "right" => shell.update(cx, |shell, cx| {
                        shell.add_terminal(spec, Where::RightPane, window, cx);
                    }),
                    "picker" => terminals.update(cx, |terminals, cx| {
                        terminals.open_picker(Purpose::NewTerminal, window, cx);
                    }),
                    "open-on" => {
                        let Some(key) = Session::global(cx).read(cx).open_key().cloned() else {
                            return;
                        };
                        terminals.update(cx, |terminals, cx| {
                            let machine = machines::Machine::this_pc(None);
                            terminals.open_on(key, machine, Where::Split, cx);
                        });
                    }
                    _ => {}
                }
            });
            cx.background_executor()
                .timer(std::time::Duration::from_millis(500))
                .await;
        }
    })
    .detach();
}

/// `--demo-merge`: asks to merge the focused pull request with its default
/// method, so the confirmation shows.
pub fn demo_merge(cx: &mut App) {
    let session = Session::global(cx);
    session.update(cx, |session, cx| {
        let Some(open) = session.open_pr() else {
            return;
        };
        let (Some(method), Some(head)) = (open.lifecycle.chosen_method(), open.merge_head()) else {
            return;
        };
        let action = lifecycle::Action::Merge {
            method,
            delete_branch: open.lifecycle.delete_branch,
            head,
        };
        let key = open.key.clone();
        session.request_action(&key, action, cx);
    });
}

/// Writes the drafts still waiting for their debounce when the app quits.
fn keep_drafts_on_quit(cx: &mut App) {
    cx.on_app_quit(|cx| {
        let saver = Session::try_global(cx).and_then(|session| session.read(cx).saver());
        let flush = cx
            .background_executor()
            .spawn(async move { saver.inspect(drafts::DraftSaver::flush) });
        async move {
            flush.await;
        }
    })
    .detach();
}

/// The main window: 1440 by 900, centered. A benchmark passes `throttle:
/// false` so gpui does not slow frames down while another window has focus.
pub fn window_options(cx: &App, throttle: bool) -> WindowOptions {
    let bounds = Bounds::centered(None, size(px(1440.0), px(900.0)), cx);
    let mut options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        window_min_size: Some(size(px(900.0), px(560.0))),
        titlebar: Some(TitlebarOptions {
            title: Some("review".into()),
            ..TitleBar::title_bar_options()
        }),
        ..TitleBar::window_options()
    };
    if !throttle {
        options.inactive_frame_interval = None;
        options.low_power_frame_interval = None;
        options.thermal_frame_interval = None;
    }
    options
}

/// An empty dock with the app's skin; the caller fills its center, the shell
/// its left region with the sidebar. See [`dock_skin::new_dock`].
pub fn new_dock(window: &mut Window, cx: &mut App) -> Entity<DockArea> {
    dock_skin::new_dock(window, cx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_targets_parse() {
        assert_eq!("41".parse(), Ok(OpenTarget::Number(41)));
        let key = |host: &str, owner: &str, name: &str, n| {
            OpenTarget::Key(PrKey::new(RepoRef::new(host, owner, name), PrNumber(n)))
        };
        assert_eq!(
            "github.com/Lovelace/review-sandbox#5".parse(),
            Ok(key("github.com", "Lovelace", "review-sandbox", 5))
        );
        assert_eq!(
            "gitlab.com/group/sub/proj!7".parse(),
            Ok(key("gitlab.com", "group/sub", "proj", 7))
        );
        assert!("review-sandbox#5".parse::<OpenTarget>().is_err());
        assert!("github.com/a/b#x".parse::<OpenTarget>().is_err());
    }
}
