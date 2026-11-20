//! Opens the real shell in a gpui test window: fixtures or fake forges, an
//! in-memory or scratch store, and a scratch directory for every file a test
//! writes. Nothing here sends OS input: keys and clicks go through gpui's
//! test platform into this window only.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Once, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use app::appearance::ThemeFlags;
use app::config::Config;
use app::model::PrKey;
use app::notify::Toaster;
use app::panels::{DiffPanel, LogPanel, PanelKind};
use app::services::{Services, Spawn};
use app::session::{OpenMode, Session};
use app::shell::Shell;
use app::subscriptions::Subscriptions;
use app::watching::Toast;
use app::writing::Writing;
use app::{OpenTarget, Setup, new_dock, window_options};
use diff::Side;
use diff_view::DiffView;
use gpui::{
    App, AppContext as _, Bounds, ElementId, Entity, Modifiers, MouseButton, MouseDownEvent,
    MouseUpEvent, Pixels, Point, Render, ScrollDelta, ScrollEvent, TestAppContext,
    VisualTestContext, Window, point, px, size,
};
use gpui_base::test_support::ElementSnapshot;
use gpui_kit::component::dock::{DockArea, DockAreaState, PanelId};

/// How long [`ShellWindow::settle_until`] waits on work outside gpui's
/// executor (the store's and the forge hub's threads). A bound, not a speed:
/// the condition usually holds after the first pass.
const OUTSIDE_WAIT: Duration = Duration::from_secs(30);

/// A fresh directory under the workspace's `.scratch/app-tests`, removed
/// when dropped.
pub struct Scratch {
    dir: PathBuf,
}

/// Older than this, a scratch directory belongs to a run that has ended.
const STALE_SCRATCH: Duration = Duration::from_secs(60 * 60);

/// Removes directories left by earlier runs. The drop's removal fails on
/// Windows while the test app still holds the store open, so it cannot be
/// relied on alone.
fn sweep_stale(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .is_ok_and(|at| at.elapsed().is_ok_and(|age| age > STALE_SCRATCH));
        if stale {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

impl Scratch {
    #[must_use]
    pub fn new(name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        static SWEEP: Once = Once::new();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("after the epoch")
            .as_nanos();
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.scratch/app-tests");
        SWEEP.call_once(|| sweep_stale(&root));
        let dir = root.join(format!(
            "{name}-{}-{nanos}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("create the scratch directory");
        Self { dir }
    }

    pub fn join(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// What the dock starts with.
#[derive(Clone)]
pub enum StartLayout {
    /// Nothing, as a first start or `--layout default` does.
    Empty,
    /// A saved layout, loaded the way startup loads the user's file.
    Saved(Box<DockAreaState>),
}

/// How [`open_shell`] starts the app.
pub struct Options {
    pub layout: StartLayout,
    /// Saves layout changes here, as the shell saves the user's file;
    /// `None` keeps them in memory.
    pub layout_file: Option<PathBuf>,
    pub theme: ThemeFlags,
    /// Drafts and the pending review in this database; `None` keeps them
    /// in memory.
    pub store: Option<PathBuf>,
    /// Opens this fixture pull request once the window is up.
    pub open_pr: Option<u64>,
    /// Rows of the synthetic fixture diff; `None` for the small one.
    pub diff_rows: Option<usize>,
    /// A saved sidebar, loaded as startup loads the user's.
    pub sidebar: Option<Box<DockAreaState>>,
    /// Settings read from and saved to this file, user themes from the
    /// `themes` directory beside it; `None` keeps them in memory.
    pub settings: Option<PathBuf>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            layout: StartLayout::Empty,
            layout_file: None,
            theme: ThemeFlags::default(),
            store: None,
            open_pr: None,
            diff_rows: None,
            sidebar: None,
            settings: None,
        }
    }
}

/// The Vercel theme.
#[must_use]
pub fn flat() -> ThemeFlags {
    ThemeFlags {
        family: Some("Vercel".into()),
        mode: Some(theme::ModeSetting::Dark),
    }
}

/// Takes the toasts a test would raise, so none reaches the OS.
struct NoToasts;

impl Toaster for NoToasts {
    fn show(&self, _: &Toast) {}
}

/// The shell in a test window, with its context.
pub struct ShellWindow {
    pub cx: VisualTestContext,
    pub shell: Entity<Shell>,
}

/// Initializes the app on fixtures, as `--fixtures` does, and opens its main
/// window with the shell in it.
pub fn open_shell(cx: &mut TestAppContext, options: Options) -> ShellWindow {
    let setup = Setup {
        fixtures: true,
        theme: options.theme.clone(),
        store_path: options.store.clone(),
        settings_path: options.settings.clone(),
        themes_path: options
            .settings
            .as_ref()
            .and_then(|file| file.parent().map(|dir| dir.join("themes"))),
        diff_rows: options.diff_rows,
        ..Setup::default()
    };
    // The highlighter's workers, the store and the forge hub run on threads
    // of their own and wake the app's tasks from there, as in the real app.
    // The test scheduler treats that as nondeterminism unless allowed.
    cx.executor().allow_parking();
    let (window, shell) = cx.update(|cx| {
        app::init(cx, &setup);
        if let Some(subscriptions) = Subscriptions::global(cx) {
            let toaster: Arc<dyn Toaster> = Arc::new(NoToasts);
            subscriptions.update(cx, |s, _| s.set_toaster(toaster));
        }
        let Options {
            layout: start,
            layout_file,
            sidebar,
            ..
        } = options;
        gpui_kit::open_window(window_options(cx, false), cx, move |window, cx| {
            let dock = new_dock(window, cx);
            match start {
                StartLayout::Saved(state) => dock.update(cx, |dock, cx| {
                    dock.load(*state, window, cx)
                        .expect("a parsed layout loads");
                }),
                StartLayout::Empty => {}
            }
            let persist = layout_file.is_some();
            cx.new(|cx| {
                let mut shell = Shell::new(dock, persist, window, cx);
                if let Some(state) = sidebar {
                    shell.load_sidebar(*state, window, cx);
                }
                match layout_file {
                    Some(path) => shell.with_layout_file(path),
                    None => shell,
                }
            })
        })
        .expect("open the main window")
    });
    let mut shell = ShellWindow {
        cx: VisualTestContext::from_window(window, cx),
        shell,
    };
    shell.settle();
    if let Some(number) = options.open_pr {
        shell.open_pr(number);
    }
    shell
}

impl ShellWindow {
    /// Resizes the window, as dragging its edge does.
    pub fn resize(&mut self, width: f32, height: f32) {
        self.cx.simulate_resize(size(px(width), px(height)));
        self.settle();
    }

    /// Draws a fresh frame, so positions read from the last one are current:
    /// some layouts (a resized split) settle a frame after the change.
    pub fn redraw(&mut self) {
        self.cx.update(|window, _| window.refresh());
        self.settle();
    }

    /// Runs every task that can run, and draws the window.
    pub fn settle(&mut self) {
        self.cx.run_until_parked();
    }

    /// Settles until `done` holds, waiting for work on threads outside gpui's
    /// executor (the store, the forge hub, git).
    ///
    /// # Panics
    /// If `done` still fails after [`OUTSIDE_WAIT`].
    pub fn settle_until(&mut self, what: &str, mut done: impl FnMut(&mut App) -> bool) {
        let deadline = Instant::now() + OUTSIDE_WAIT;
        loop {
            self.settle();
            if self.cx.update(|_, cx| done(cx)) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Moves the test clock on, firing the timers due by then: debounces,
    /// polls.
    pub fn advance(&mut self, by: Duration) {
        self.cx.executor().advance_clock(by);
        self.settle();
    }

    /// Space-separated keys, as in a key binding: `"j j enter"`, `"] f"`,
    /// `"ctrl-enter"`.
    pub fn keys(&mut self, keys: &str) {
        self.cx.simulate_keystrokes(keys);
        self.settle();
    }

    /// Types `text` into whatever has focus.
    pub fn type_text(&mut self, text: &str) {
        self.cx.simulate_input(text);
        self.settle();
    }

    /// Clicks the middle of `bounds`.
    pub fn click_at(&mut self, position: Point<Pixels>) {
        self.cx
            .simulate_mouse_move(position, None, Modifiers::none());
        self.cx.simulate_click(position, Modifiers::none());
        self.settle();
    }

    /// Where the element with this `debug_selector` was drawn in the last
    /// frame. Needs a debug build: release builds record no selectors.
    pub fn bounds_of(&mut self, selector: &'static str) -> Bounds<Pixels> {
        self.redraw();
        self.cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("no element drawn with debug selector {selector:?}"))
    }

    /// Clicks the element with this `debug_selector`.
    pub fn click(&mut self, selector: &'static str) {
        let center = self.bounds_of(selector).center();
        self.click_at(center);
    }

    /// Clicks the element with this `debug_selector`, built at run time.
    pub fn click_named(&mut self, selector: &str) {
        self.click(String::leak(selector.to_owned()));
    }

    /// Whether an element with this `debug_selector` was drawn last frame.
    pub fn drawn(&mut self, selector: &str) -> bool {
        self.redraw();
        self.cx
            .debug_bounds(String::leak(selector.to_owned()))
            .is_some()
    }

    /// Presses at `from`, moves to `to` in steps with the button held, and
    /// lets go there: a drag.
    pub fn drag(&mut self, from: Point<Pixels>, to: Point<Pixels>) {
        let none = Modifiers::none();
        self.cx.simulate_mouse_move(from, None, none);
        self.cx.simulate_mouse_down(from, MouseButton::Left, none);
        for step in 1..=4u8 {
            let along = f32::from(step) / 4.0;
            let at = point(
                from.x + (to.x - from.x) * along,
                from.y + (to.y - from.y) * along,
            );
            self.cx.simulate_mouse_move(at, MouseButton::Left, none);
        }
        self.cx.simulate_mouse_up(to, MouseButton::Left, none);
        self.settle();
    }

    /// Turns the wheel over `position` by `dy` pixels (down is positive).
    pub fn scroll_at(&mut self, position: Point<Pixels>, dy: f32) {
        self.cx
            .simulate_mouse_move(position, None, Modifiers::none());
        self.cx.simulate_event(ScrollEvent {
            position,
            delta: ScrollDelta::Pixels(point(px(0.0), px(-dy))),
            ..ScrollEvent::default()
        });
        self.settle();
    }

    /// The sidebar the sections draw.
    pub fn sidebar(&mut self) -> Entity<app::sidebar::Sidebar> {
        self.cx.update(|_, cx| app::sidebar::Sidebar::global(cx))
    }

    /// The dock the sidebar's sections are in: the one dock, whose left
    /// region is the sidebar.
    pub fn sidebar_dock(&mut self) -> Entity<DockArea> {
        self.dock()
    }

    /// The panel for `section`, wherever in the dock it is.
    pub fn section(&mut self, section: app::sidebar_panels::Section) -> PanelId {
        let dock = self.sidebar_dock();
        self.cx.update(|_, cx| {
            app::sidebar_panels::section_panel(dock.read(cx), section, cx)
                .unwrap_or_else(|| panic!("no {section:?} section"))
        })
    }

    /// The sidebar, the dock's left region, as the layout saves it.
    pub fn sidebar_layout(&mut self) -> gpui_kit::component::dock::PanelState {
        let dock = self.sidebar_dock();
        self.cx.update(|_, cx| {
            dock.read(cx)
                .dump(cx)
                .left_dock
                .map(|left| left.panel().clone())
                .unwrap_or_default()
        })
    }

    /// Clicks with a modifier held, as Ctrl+click does.
    pub fn click_with(&mut self, selector: &str, modifiers: Modifiers) {
        let center = self.bounds_of(String::leak(selector.to_owned())).center();
        self.cx.simulate_mouse_move(center, None, modifiers);
        self.cx.simulate_click(center, modifiers);
        self.settle();
    }

    /// Double-clicks the element with this `debug_selector`: two presses,
    /// the second counted as the second click.
    pub fn double_click(&mut self, selector: &str) {
        let position = self.bounds_of(String::leak(selector.to_owned())).center();
        self.cx
            .simulate_mouse_move(position, None, Modifiers::none());
        for click_count in 1..=2 {
            self.cx.simulate_event(MouseDownEvent {
                button: MouseButton::Left,
                position,
                modifiers: Modifiers::none(),
                click_count,
                first_mouse: false,
            });
            self.cx.simulate_event(MouseUpEvent {
                button: MouseButton::Left,
                position,
                modifiers: Modifiers::none(),
                click_count,
            });
        }
        self.settle();
    }

    /// The element with this id as gpui-base's observation saw it in the
    /// last frame. gpui-component's buttons, checkboxes and tab bars register
    /// themselves, with their state; the app's own elements do not.
    pub fn element(&mut self, id: impl Into<ElementId>) -> Option<ElementSnapshot> {
        let id = id.into();
        self.redraw();
        self.cx
            .update(|window, _| gpui_base::test_support::find(window, &[], &id))
    }

    /// The open popup menu's bounds. gpui-component's menu entries
    /// register no id of their own, so tests find them by position in it.
    pub fn menu_items(&mut self) -> Option<Bounds<Pixels>> {
        self.element(gpui::SharedString::from("popup-menu"))
            .map(|menu| menu.bounds())
    }

    /// Clicks entry `ix` of an open popup menu of `count` equal entries.
    pub fn click_menu_item(&mut self, ix: usize, count: usize) {
        let items = self.menu_items().expect("an open popup menu");
        let row = items.size.height / count as f32;
        let at = point(items.center().x, items.top() + row * (ix as f32 + 0.5));
        self.click_at(at);
    }

    /// Clicks the middle of the registered element with this id.
    pub fn click_element(&mut self, id: impl Into<ElementId>) {
        let id = id.into();
        let Some(element) = self.element(id.clone()) else {
            let known = self
                .cx
                .update(|window, _| gpui_base::test_support::registered_paths(window));
            panic!("no element {id:?} in the last frame; registered: {known}");
        };
        assert!(element.visible(), "{id:?} is not visible");
        self.click_at(element.bounds().center());
    }

    pub fn session(&mut self) -> Entity<Session> {
        self.cx.update(|_, cx| Session::global(cx))
    }

    pub fn dock(&mut self) -> Entity<DockArea> {
        self.read_shell(|shell, _| shell.dock().clone())
    }

    pub fn read_shell<R>(&mut self, read: impl FnOnce(&Shell, &App) -> R) -> R {
        let shell = self.shell.clone();
        self.cx.update(|_, cx| read(shell.read(cx), cx))
    }

    /// Runs `f` with the window, as a listener in it would.
    pub fn update<R>(&mut self, f: impl FnOnce(&mut Window, &mut App) -> R) -> R {
        self.cx.update(f)
    }

    /// The key of the fixture pull request with this number.
    pub fn key(&mut self, number: u64) -> PrKey {
        let session = self.session();
        self.cx.update(|_, cx| {
            session
                .read(cx)
                .resolve_target(&OpenTarget::Number(number))
                .unwrap_or_else(|| panic!("no fixture #{number}"))
        })
    }

    /// Opens the fixture pull request with this number in a permanent
    /// tab, as Enter on its inbox row does.
    pub fn open_pr(&mut self, number: u64) {
        self.show(number, PanelKind::Diff, OpenMode::Permanent);
    }

    /// Opens `number` and brings its `kind` panel forward.
    pub fn show(&mut self, number: u64, kind: PanelKind, mode: OpenMode) {
        let key = self.key(number);
        let session = self.session();
        self.cx.update(|_, cx| {
            session.update(cx, |session, cx| session.show(&key, kind, mode, cx));
        });
        self.settle();
    }

    /// The number of the focused pull request.
    pub fn focused(&mut self) -> Option<u64> {
        let session = self.session();
        self.cx
            .update(|_, cx| session.read(cx).open_key().map(|key| key.number.0))
    }

    /// The numbers of the open pull requests, in the order they opened.
    pub fn open_numbers(&mut self) -> Vec<u64> {
        let session = self.session();
        self.cx.update(|_, cx| {
            session
                .read(cx)
                .open_prs()
                .map(|open| open.key.number.0)
                .collect()
        })
    }

    /// The dock panel the session knows by `name` (`Diff`, `Log`, ...)
    /// for the focused pull request.
    pub fn panel_id(&mut self, name: &'static str) -> PanelId {
        let kind = match name {
            app::panels::DIFF => PanelKind::Diff,
            app::panels::CONVERSATION => PanelKind::Conversation,
            _ => PanelKind::Log,
        };
        let session = self.session();
        self.cx.update(|_, cx| {
            let session = session.read(cx);
            let pr = if kind == PanelKind::Log {
                None
            } else {
                session.open_key()
            };
            let id = session.panel_of(pr, kind);
            PanelId::from(id.unwrap_or_else(|| panic!("no {name} panel")))
        })
    }

    /// The dock panel of `kind` bound to pull request `number`, if the dock
    /// holds one.
    pub fn panel_of(&mut self, number: u64, kind: PanelKind) -> Option<PanelId> {
        let key = self.key(number);
        let session = self.session();
        let dock = self.dock();
        self.cx.update(|_, cx| {
            let id = session
                .read(cx)
                .panels()
                .iter()
                .find(|entry| entry.kind == kind && entry.pr.as_ref() == Some(&key))
                .map(|entry| PanelId::from(entry.id))?;
            dock.read(cx).panel(id).is_some().then_some(id)
        })
    }

    /// The panel with this name, as its own type.
    pub fn panel<P: Render>(&mut self, name: &'static str) -> Entity<P> {
        let id = self.panel_id(name);
        let dock = self.dock();
        self.cx.update(|_, cx| {
            dock.read(cx)
                .panel(id)
                .unwrap_or_else(|| panic!("the dock holds no {name} panel"))
                .view()
                .downcast::<P>()
                .unwrap_or_else(|_| panic!("the {name} panel has another type"))
        })
    }

    pub fn diff_panel(&mut self) -> Entity<DiffPanel> {
        self.panel(app::panels::DIFF)
    }

    pub fn log_panel(&mut self) -> Entity<LogPanel> {
        self.panel(app::panels::LOG)
    }

    /// What the user writes on the focused pull request.
    pub fn writing(&mut self) -> Entity<Writing> {
        let session = self.session();
        self.cx
            .update(|_, cx| session.read(cx).writing().expect("a pull request is open"))
    }

    /// The open pull request's diff view.
    pub fn diff_view(&mut self) -> Entity<DiffView> {
        let diff = self.diff_panel();
        self.cx
            .update(|_, cx| diff.read(cx).view().cloned().expect("the diff is built"))
    }

    /// The code lines the last frame drew, top to bottom: a window position
    /// on each line's text and what the view says is there (row, side,
    /// 0-based line).
    pub fn drawn_lines(&mut self) -> Vec<(Point<Pixels>, (usize, Side, u32))> {
        let body = self.bounds_of("diff-body");
        let view = self.diff_view();
        self.cx.update(|_, cx| {
            let view = view.read(cx);
            let gutter = view.gutter_width().expect("the diff was drawn");
            let list = view.list();
            let scroll = view.scroll_y();
            view.rows_in_view()
                .filter_map(|row| {
                    let middle = list.top_of(row) - scroll + list.row_height(row) / 2.0;
                    #[expect(clippy::cast_possible_truncation, reason = "window pixels")]
                    let local = point(gutter + px(40.0), px(middle as f32));
                    if local.y < px(0.0) || local.y > body.size.height {
                        return None;
                    }
                    let hit = view.hit_test(local)?;
                    Some((body.origin + local, hit))
                })
                .collect()
        })
    }

    /// Each tab group's tabs by panel name, from the dock's own saved form.
    pub fn groups(&mut self) -> Vec<Vec<String>> {
        let dock = self.dock();
        let state = self.cx.update(|_, cx| dock.read(cx).dump(cx));
        let mut groups = Vec::new();
        collect_groups(&state.center, &mut groups);
        groups
    }

    /// Whether the dock no longer holds the panel `id`.
    pub fn dock_lacks(&mut self, id: PanelId) -> bool {
        let dock = self.dock();
        self.cx.update(|_, cx| dock.read(cx).panel(id).is_none())
    }

    /// Whether the tab of `id` is the shown one of its group.
    pub fn is_shown(&mut self, id: PanelId) -> bool {
        use gpui_kit::component::dock::{DockPlacement, PaneRef};

        let dock = self.dock();
        self.cx.update(|_, cx| {
            let dock = dock.read(cx);
            let Some(tree) = dock.layout(DockPlacement::Center) else {
                return false;
            };
            let Some(node) = tree.find_panel_node(id) else {
                return false;
            };
            matches!(
                tree.find_node(node).map(|node| node.kind()),
                Some(PaneRef::Tabs { panels, active_ix }) if panels.get(active_ix) == Some(&id)
            )
        })
    }

    /// Which panel each tab group of the dock shows, by panel name, from the
    /// dock's own saved form.
    pub fn shown_tabs(&mut self) -> Vec<String> {
        let dock = self.dock();
        let state = self.cx.update(|_, cx| dock.read(cx).dump(cx));
        let mut shown = Vec::new();
        collect_shown(&state.center, &mut shown);
        shown
    }
}

fn collect_groups(panel: &gpui_kit::component::dock::PanelState, out: &mut Vec<Vec<String>>) {
    use gpui_kit::component::dock::PanelInfo;

    if let PanelInfo::Tabs { .. } = panel.info {
        out.push(
            panel
                .children
                .iter()
                .map(|child| child.panel_name.clone())
                .collect(),
        );
        return;
    }
    for child in &panel.children {
        collect_groups(child, out);
    }
}

fn collect_shown(panel: &gpui_kit::component::dock::PanelState, out: &mut Vec<String>) {
    use gpui_kit::component::dock::PanelInfo;

    if let PanelInfo::Tabs { active_index } = panel.info {
        if let Some(child) = panel.children.get(active_index) {
            out.push(child.panel_name.clone());
        }
        return;
    }
    for child in &panel.children {
        collect_shown(child, out);
    }
}

/// A [`Spawn`] that runs each job on a thread of its own, as the app does.
#[must_use]
pub fn thread_spawn() -> Spawn {
    Arc::new(|future| {
        std::thread::spawn(move || futures::executor::block_on(future));
    })
}

/// Services on fake forges: a config naming each forge's host with
/// `data_dir` in `scratch`, drafts in memory, and a [`forge::ForgeHub`] over
/// `forges`. Each of `repos` is registered with a clone that is not a git
/// repository, so opening one of its pull requests fails at once on git
/// instead of fetching anything.
#[must_use]
pub fn fake_services(
    scratch: &Scratch,
    forges: Vec<Arc<dyn forge::Forge>>,
    repos: &[forge::model::RepoRef],
) -> Services {
    use std::fmt::Write as _;

    let toml_path = |path: &Path| path.display().to_string().replace('\\', "/");
    let not_a_repo = scratch.join("not-a-repo");
    std::fs::create_dir_all(&not_a_repo).expect("create the clone directory");
    let mut text = format!(
        "data_dir = \"{}\"
",
        toml_path(&scratch.join("data"))
    );
    for forge in &forges {
        let kind = match forge.kind() {
            forge::model::ForgeKind::GitHub => "github",
            forge::model::ForgeKind::GitLab => "gitlab",
            forge::model::ForgeKind::Forgejo => "forgejo",
        };
        let _ = write!(
            text,
            "
[[forges]]
kind = \"{kind}\"
host = \"{}\"
",
            forge.host()
        );
    }
    for repo in repos {
        let _ = write!(
            text,
            "
[[repos]]
host = \"{}\"
repo = \"{}\"
clone = \"{}\"
",
            repo.host,
            repo.path(),
            toml_path(&not_a_repo)
        );
    }
    let config = Config::parse(&text).expect("the test config parses");
    let hub = forge::ForgeHub::with_backends(forges).expect("a hub over the fakes");
    Services::new(Arc::new(config), None, Some(Arc::new(hub)), thread_spawn())
}

/// A forge that answers CI reads from fixed runs, records re-runs,
/// lifecycle writes and reviewer, assignee and label writes, serves a short
/// log, lists [`FAKE_PEOPLE`] and [`fake_labels`], says every pull request
/// can merge by merge commit or squash, and turns everything else down as
/// unsupported.
pub struct FakeForge {
    kind: forge::model::ForgeKind,
    host: String,
    runs: Mutex<Vec<forge::model::CiRun>>,
    /// How many times the runs were read.
    ci_reads: AtomicUsize,
    retries: Mutex<Vec<String>>,
    /// The pull request lifecycle writes answer with, patched.
    pull_request: Mutex<Option<forge::model::PullRequest>>,
    /// Whether snapshot reads answer with `pull_request`; off, they are
    /// unsupported.
    snapshots: AtomicBool,
    /// Other pull requests `pull_request` reads find.
    others: Mutex<Vec<forge::model::PullRequest>>,
    /// Every collaborator query asked, in order.
    queries: Mutex<Vec<String>>,
    /// What batched CI reads answer, by number; one not listed is
    /// unsupported, so rows of other tests keep what they show.
    ci_states: Mutex<std::collections::HashMap<u64, forge::model::CiState>>,
    /// The pull requests each batched CI read asked about.
    ci_state_reads: Mutex<Vec<Vec<u64>>>,
}

/// The accounts a [`FakeForge`] lists as collaborators. It refuses to ask
/// `ghost` for a review.
pub const FAKE_PEOPLE: [&str; 4] = ["priya", "prakash", "dara", "ghost"];

/// The labels a [`FakeForge`] repository has.
#[must_use]
pub fn fake_labels() -> Vec<forge::model::Label> {
    ["bug", "docs", "ui"]
        .into_iter()
        .map(|name| forge::model::Label {
            name: name.to_owned(),
            color: Some("0075ca".to_owned()),
            description: Some(format!("{name} work")),
        })
        .collect()
}

impl FakeForge {
    #[must_use]
    pub fn new(
        kind: forge::model::ForgeKind,
        host: &str,
        runs: Vec<forge::model::CiRun>,
    ) -> Arc<Self> {
        Self::with_pull_request(kind, host, runs, None)
    }

    /// A fake whose lifecycle writes answer with `pull_request`.
    #[must_use]
    pub fn with_pull_request(
        kind: forge::model::ForgeKind,
        host: &str,
        runs: Vec<forge::model::CiRun>,
        pull_request: Option<forge::model::PullRequest>,
    ) -> Arc<Self> {
        Arc::new(Self {
            kind,
            host: host.to_owned(),
            runs: Mutex::new(runs),
            ci_reads: AtomicUsize::new(0),
            retries: Mutex::default(),
            pull_request: Mutex::new(pull_request),
            snapshots: AtomicBool::new(false),
            others: Mutex::default(),
            queries: Mutex::default(),
            ci_states: Mutex::default(),
            ci_state_reads: Mutex::default(),
        })
    }

    /// From now on a batched CI read answers `state` for `number`.
    pub fn set_ci(&self, number: u64, state: forge::model::CiState) {
        self.ci_states
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(number, state);
    }

    /// The pull requests each batched CI read asked about, in order.
    pub fn ci_state_reads(&self) -> Vec<Vec<u64>> {
        self.ci_state_reads
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// From now on the runs read back as `runs`, as after they moved on.
    pub fn set_runs(&self, runs: Vec<forge::model::CiRun>) {
        *self.runs.lock().unwrap_or_else(PoisonError::into_inner) = runs;
    }

    /// How many times the runs have been read.
    pub fn ci_reads(&self) -> usize {
        self.ci_reads.load(Ordering::SeqCst)
    }

    /// Pushes a new head to the pull request: from now on its snapshot
    /// reads answer with `sha` as the head, as after a push to its branch.
    pub fn move_head(&self, sha: &str) {
        let mut held = self
            .pull_request
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(pr) = held.as_mut() {
            pr.head.sha = forge::model::Sha::new(sha);
        }
        self.snapshots.store(true, Ordering::SeqCst);
    }

    /// The pull request with `state`, as a lifecycle write answers.
    fn answer(&self, state: forge::model::PrState) -> forge::Result<forge::model::PullRequest> {
        let mut held = self
            .pull_request
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let pr = held.as_mut().ok_or_else(|| forge::Error::NotFound {
            request: "pull request".to_owned(),
        })?;
        pr.state = state;
        Ok(pr.clone())
    }

    /// A pull request `pull_request` reads find besides the main one.
    pub fn add_pull_request(&self, pr: forge::model::PullRequest) {
        self.others
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(pr);
    }

    /// The collaborator queries asked so far.
    pub fn queries(&self) -> Vec<String> {
        self.queries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The held pull request after `edit`, as a people write answers.
    fn edit(
        &self,
        edit: impl FnOnce(&mut forge::model::PullRequest),
    ) -> forge::Result<forge::model::PullRequest> {
        let mut held = self
            .pull_request
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let pr = held.as_mut().ok_or_else(|| forge::Error::NotFound {
            request: "pull request".to_owned(),
        })?;
        edit(pr);
        Ok(pr.clone())
    }

    /// The re-runs asked for so far: `run <id>` or `job <id>`.
    pub fn retries(&self) -> Vec<String> {
        self.retries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn record(&self, retry: String) {
        self.retries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(retry);
    }

    fn unsupported<T>(&self, feature: &'static str) -> forge::Result<T> {
        Err(forge::Error::Unsupported {
            kind: self.kind,
            feature,
        })
    }
}

/// The log every job of a [`FakeForge`] has.
pub const FAKE_LOG: &str = "Set up job\nRun tests\nerror: 1 test failed\n";

#[async_trait::async_trait]
impl forge::Forge for FakeForge {
    fn kind(&self) -> forge::model::ForgeKind {
        self.kind
    }

    fn host(&self) -> &str {
        &self.host
    }

    fn capabilities(&self) -> forge::Capabilities {
        forge::Capabilities::baseline(self.kind)
    }

    async fn whoami(&self) -> forge::Result<forge::model::User> {
        Ok(forge::model::User::new("tester"))
    }

    async fn inbox(
        &self,
        _: &forge::model::InboxSection,
    ) -> forge::Result<Vec<forge::model::PullRequest>> {
        self.unsupported("inbox")
    }

    async fn pull_request(
        &self,
        _: &forge::model::RepoRef,
        number: forge::model::PrNumber,
    ) -> forge::Result<forge::model::PullRequest> {
        let held = self
            .pull_request
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let others = self
            .others
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        held.into_iter()
            .chain(others)
            .find(|pr| pr.number == number)
            .ok_or_else(|| forge::Error::NotFound {
                request: format!("pull request {number}"),
            })
    }

    async fn collaborators(
        &self,
        _: &forge::model::RepoRef,
        query: &str,
    ) -> forge::Result<Vec<forge::model::User>> {
        self.queries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(query.to_owned());
        Ok(FAKE_PEOPLE
            .into_iter()
            .filter(|login| login.starts_with(query.trim()))
            .map(forge::model::User::new)
            .collect())
    }

    async fn labels(&self, _: &forge::model::RepoRef) -> forge::Result<Vec<forge::model::Label>> {
        Ok(fake_labels())
    }

    async fn request_reviewers(
        &self,
        _: &forge::model::RepoRef,
        number: forge::model::PrNumber,
        users: &[forge::model::User],
        teams: &[forge::model::User],
    ) -> forge::Result<forge::model::PullRequest> {
        if users.iter().any(|user| user.login == "ghost") {
            return Err(forge::Error::NotFound {
                request: "reviewer ghost".to_owned(),
            });
        }
        let names: Vec<String> = users.iter().chain(teams).map(|u| u.login.clone()).collect();
        self.record(format!("request {number} {}", names.join(",")));
        self.edit(|pr| {
            for user in users.iter().chain(teams) {
                pr.reviewers.push(forge::model::Reviewer::new(
                    user.clone(),
                    forge::model::ReviewState::Requested,
                ));
            }
        })
    }

    async fn remove_reviewers(
        &self,
        _: &forge::model::RepoRef,
        number: forge::model::PrNumber,
        users: &[forge::model::User],
        teams: &[forge::model::User],
    ) -> forge::Result<forge::model::PullRequest> {
        let names: Vec<String> = users.iter().chain(teams).map(|u| u.login.clone()).collect();
        self.record(format!("unrequest {number} {}", names.join(",")));
        self.edit(|pr| pr.reviewers.retain(|r| !names.contains(&r.user.login)))
    }

    async fn set_assignees(
        &self,
        _: &forge::model::RepoRef,
        number: forge::model::PrNumber,
        users: &[forge::model::User],
    ) -> forge::Result<forge::model::PullRequest> {
        let names: Vec<String> = users.iter().map(|u| u.login.clone()).collect();
        self.record(format!("assign {number} {}", names.join(",")));
        self.edit(|pr| pr.assignees = users.to_vec())
    }

    async fn set_labels(
        &self,
        _: &forge::model::RepoRef,
        number: forge::model::PrNumber,
        labels: &[String],
    ) -> forge::Result<forge::model::PullRequest> {
        self.record(format!("label {number} {}", labels.join(",")));
        let catalog = fake_labels();
        self.edit(|pr| {
            pr.labels = catalog
                .into_iter()
                .filter(|label| labels.contains(&label.name))
                .collect();
        })
    }

    async fn threads(
        &self,
        _: &forge::model::RepoRef,
        _: forge::model::PrNumber,
    ) -> forge::Result<Vec<forge::model::Thread>> {
        self.unsupported("threads")
    }

    async fn checks(
        &self,
        _: &forge::model::RepoRef,
        _: forge::model::PrNumber,
    ) -> forge::Result<Vec<forge::model::CheckRun>> {
        self.unsupported("checks")
    }

    async fn stack(
        &self,
        _: &forge::model::RepoRef,
        _: forge::model::PrNumber,
    ) -> forge::Result<forge::model::Stack> {
        self.unsupported("stacks")
    }

    async fn snapshot(
        &self,
        _: &forge::model::RepoRef,
        _: forge::model::PrNumber,
    ) -> forge::Result<forge::model::Snapshot> {
        if !self.snapshots.load(Ordering::SeqCst) {
            return self.unsupported("snapshots");
        }
        let pull_request = self
            .pull_request
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .ok_or_else(|| forge::Error::NotFound {
                request: "pull request".to_owned(),
            })?;
        Ok(forge::model::Snapshot {
            pull_request,
            threads: Vec::new(),
            checks: Vec::new(),
        })
    }

    async fn ci_runs(
        &self,
        _: &forge::model::RepoRef,
        _: &forge::model::Sha,
    ) -> forge::Result<Vec<forge::model::CiRun>> {
        self.ci_reads.fetch_add(1, Ordering::SeqCst);
        Ok(self
            .runs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone())
    }

    async fn ci_job(
        &self,
        _: &forge::model::RepoRef,
        job: &forge::model::JobId,
    ) -> forge::Result<forge::model::CiJob> {
        self.runs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .flat_map(|run| &run.jobs)
            .find(|known| &known.id == job)
            .cloned()
            .ok_or_else(|| forge::Error::NotFound {
                request: format!("job {}", job.as_str()),
            })
    }

    async fn retry_job(
        &self,
        _: &forge::model::RepoRef,
        job: &forge::model::JobId,
    ) -> forge::Result<()> {
        self.record(format!("job {}", job.as_str()));
        Ok(())
    }

    async fn retry_failed_jobs(
        &self,
        _: &forge::model::RepoRef,
        run: &forge::model::RunId,
    ) -> forge::Result<()> {
        self.record(format!("run {}", run.as_str()));
        Ok(())
    }

    async fn mergeability(
        &self,
        _: &forge::model::RepoRef,
        _: forge::model::PrNumber,
    ) -> forge::Result<forge::model::Mergeability> {
        Ok(forge::model::Mergeability {
            mergeable: Some(true),
            reasons: Vec::new(),
            auto_merge: false,
        })
    }

    async fn merge_methods(
        &self,
        _: &forge::model::RepoRef,
    ) -> forge::Result<forge::model::MergeSettings> {
        use forge::model::MergeMethod;
        Ok(forge::model::MergeSettings::new(
            [MergeMethod::Merge, MergeMethod::Squash],
            MergeMethod::Squash,
        ))
    }

    async fn merge(
        &self,
        _: &forge::model::RepoRef,
        number: forge::model::PrNumber,
        options: &forge::model::MergeOptions,
    ) -> forge::Result<forge::model::PullRequest> {
        self.record(format!(
            "merge {number} {:?} delete={} at {}",
            options.method,
            options.delete_branch,
            options.sha.as_str()
        ));
        self.answer(forge::model::PrState::Merged)
    }

    async fn close(
        &self,
        _: &forge::model::RepoRef,
        number: forge::model::PrNumber,
    ) -> forge::Result<forge::model::PullRequest> {
        self.record(format!("close {number}"));
        self.answer(forge::model::PrState::Closed)
    }

    async fn ci_states(
        &self,
        _: &forge::model::RepoRef,
        numbers: &[forge::model::PrNumber],
    ) -> forge::Result<Vec<forge::model::CiState>> {
        let asked: Vec<u64> = numbers.iter().map(|number| number.0).collect();
        self.ci_state_reads
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(asked.clone());
        let states = self
            .ci_states
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        asked
            .iter()
            .map(|number| states.get(number).copied())
            .collect::<Option<Vec<_>>>()
            .map_or_else(|| self.unsupported("CI states"), Ok)
    }

    async fn job_log(
        &self,
        _: &forge::model::RepoRef,
        _: &forge::model::JobId,
        from: u64,
    ) -> forge::Result<forge::model::LogChunk> {
        let bytes = usize::try_from(from)
            .ok()
            .and_then(|from| FAKE_LOG.as_bytes().get(from..))
            .unwrap_or_default()
            .to_vec();
        Ok(forge::model::LogChunk {
            offset: from,
            next_offset: from + bytes.len() as u64,
            bytes,
            complete: true,
        })
    }
}

/// Puts the session on `services` with an inbox of `prs`, one section, as a
/// live start does once the forges answer.
pub fn go_live(
    shell: &mut ShellWindow,
    services: Services,
    prs: Vec<forge::model::PullRequest>,
    kind: forge::model::ForgeKind,
) {
    use app::model::{Inbox, PullRequest, Section, SectionState};

    let prs: Vec<PullRequest> = prs
        .into_iter()
        .map(|pr| PullRequest::from_forge(Arc::new(pr), kind))
        .collect();
    let inbox = Inbox {
        sections: vec![Section {
            title: "Review requested".into(),
            items: (0..prs.len()).collect(),
            expanded_by_default: true,
            state: SectionState::Fresh,
            repo: None,
        }],
        prs,
    };
    let session = shell.session();
    shell.update(|_, cx| {
        session.update(cx, |session, cx| {
            session.set_services(services, cx);
            session.set_inbox(inbox, cx);
        });
    });
    shell.settle();
}
