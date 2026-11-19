//! The Diff panel: the open pull request's diff with its review threads as
//! cards between the lines, and keyboard navigation between files, hunks and
//! threads. A file picked in the tree is shown alone, in full or as its
//! changes; each file header carries its controls and a right-click menu.

mod file_actions;
mod file_header;
mod menu;

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use diff::{BlockId, ContextMode, FileIx, Highlighter, Layout, Side};
use diff_view::{DiffView, DiffViewEvent, Step};
use forge::Capabilities;
use forge::model::{Thread, ThreadId};
use gpui::{
    AnyElement, App, AppContext as _, Bounds, ClipboardItem, Context, Div, Entity, FocusHandle,
    Global, InteractiveElement as _, IntoElement, KeyBinding, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Point, Render, SharedString,
    StatefulInteractiveElement as _, Styled, Subscription, Window, actions, canvas, div,
    prelude::FluentBuilder as _, px,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::{Icon, dock::Panel, h_flex, v_flex};
use store::DraftAnchor;
use theme::{ActiveTheme, Theme};

use crate::appearance::{self, Appearance};
use crate::diff_style::diff_style;
use crate::gutter;
use crate::model::{ChangedFile, PrKey};
use crate::opener::OpenStatus;
use crate::panels::{Binding, PanelKind, dock_panel, header};
use crate::pr_header::PrActions;
use crate::review_bar::ReviewBar;
use crate::session::{OpenDiff, Session, SessionEvent, StackStep};
use crate::threads::{CardsEvent, ThreadCards};
use crate::ui::{self, TextExt as _};
use crate::writing::{Writing, WritingEvent};

pub use file_header::{HeaderControls, SLOTS as HEADER_SLOTS};
pub use menu::{DiffMenu, Entry as MenuEntry, Links, MenuAction};

actions!(
    diff_panel,
    [
        NextFile,
        PrevFile,
        NextHunk,
        PrevHunk,
        NextThread,
        PrevThread,
        ToggleThread,
        /// `c`: comment on the line under the pointer.
        OpenComposer,
        /// `Shift+C`: comment on the lines last dragged over.
        OpenRangeComposer,
        /// `] s`: the next pull request down the stack (toward the trunk).
        NextInStack,
        /// `[ s`: the next pull request up the stack.
        PrevInStack,
        /// `v`: marks the file at the top of the view viewed, or not.
        ToggleFileViewed,
        /// `z`: collapses or expands the file at the top of the view.
        ToggleFileCollapsed,
        /// `Shift+Z`: collapses every file, or expands them all when every
        /// one is collapsed.
        ToggleAllFilesCollapsed,
        /// `Esc`: closes the diff's menu.
        CloseMenu
    ]
);

pub const DIFF_KEY_CONTEXT: &str = "DiffPanel";
/// The panel's single-key bindings: not while a composer has the keys.
const DIFF_KEYS: &str = "DiffPanel && !Input";

/// Marks the panel's own key bindings as registered.
struct KeysBound;

impl Global for KeysBound {}

/// Binds the keys of the panel's file actions, once.
pub fn init(cx: &mut App) {
    if cx.has_global::<KeysBound>() {
        return;
    }
    cx.set_global(KeysBound);
    let diff = Some(DIFF_KEYS);
    cx.bind_keys([
        KeyBinding::new("z", ToggleFileCollapsed, diff),
        KeyBinding::new("shift-z", ToggleAllFilesCollapsed, diff),
        KeyBinding::new("escape", CloseMenu, Some(DIFF_KEY_CONTEXT)),
    ]);
}

/// Which files of each pull request were collapsed, by path, for as long as
/// the app runs: a diff built again (a new head, a reopened tab) folds the
/// same files.
#[derive(Default)]
struct CollapseMemory(HashMap<PrKey, HashMap<String, bool>>);

impl Global for CollapseMemory {}

/// Whether marking a file viewed collapses it, as the settings say.
fn collapse_viewed(cx: &App) -> bool {
    cx.try_global::<Appearance>()
        .is_none_or(|state| state.settings.diff.collapse_viewed)
}

/// Shows every file of the focused pull request's diff again, as the file
/// tree's "All changes" bar does.
pub fn show_all_changes(session: &mut Session, cx: &mut Context<Session>) {
    if let Some(open) = session.focused_mut()
        && open.selected_file.take().is_some()
    {
        cx.notify();
    }
}

fn copy(text: String, cx: &mut App) {
    cx.write_to_clipboard(ClipboardItem::new_string(text));
}

/// Code rows kept above a thread's first line when jumping to it, so the
/// lines before it give context.
const THREAD_CONTEXT_ROWS: f64 = 3.0;

struct Loaded {
    key: PrKey,
    generation: u64,
    view: Entity<DiffView>,
    /// The threads its blocks were placed from.
    threads: Arc<[Thread]>,
    /// The viewed marks the view's headers show, by file.
    viewed: Vec<bool>,
    /// Whether the remembered collapse state was put back on this view.
    restored: bool,
    /// The full id of the head commit the diff shows: a rebuilt diff of the
    /// same head keeps the context revealed in this one.
    head: String,
}

/// A drag over gutter lines that has not been let go of.
struct Drag {
    start: gutter::Hit,
    range: gutter::Range,
}

pub struct DiffPanel {
    focus: FocusHandle,
    session: Entity<Session>,
    binding: Binding,
    loaded: Option<Loaded>,
    /// Shared by every view this panel builds, so a rebuilt diff keeps the
    /// highlights already computed for its blobs.
    highlighter: Highlighter,
    cards: Entity<ThreadCards>,
    /// A file clicked in the tree, scrolled to once the diff for the open pull
    /// request is on screen.
    pending_jump: Option<SharedString>,
    /// A thread asked for from the sidebar or the conversation, likewise.
    pending_thread: Option<ThreadId>,
    /// What the user writes on the pull request shown; an empty one until
    /// a pull request is open.
    writing: Entity<Writing>,
    writing_subscriptions: Vec<Subscription>,
    /// Review, merge and the rest, as one compact bar.
    actions: Entity<PrActions>,
    review_bar: Entity<ReviewBar>,
    /// The view's bounds in the window, from the last paint.
    bounds: Rc<Cell<Bounds<Pixels>>>,
    /// Where the pointer last was, relative to the view.
    pointer: Option<Point<Pixels>>,
    hovered: Option<gutter::Hit>,
    drag: Option<Drag>,
    /// The lines last dragged over, for `Shift+C`.
    selection: Option<gutter::Range>,
    /// A composer just opened, to bring into view once its block exists.
    reveal: Option<DraftAnchor>,
    /// The right-click (or `...`) menu, while it is open.
    menu: Option<DiffMenu>,
    /// How a file shown alone is shown: in full, or its changes.
    whole_mode: ContextMode,
    /// The file shown alone and how it was shown before, to put back when
    /// the diff shows every file again.
    whole_restore: Option<(FileIx, ContextMode)>,
}

dock_panel!(DiffPanel, "Diff", bound);

/// What a navigation key moves between.
#[derive(Clone, Copy)]
enum Target {
    File,
    Hunk,
}

impl DiffPanel {
    /// A Diff panel that follows the focused pull request.
    pub fn new(window: &mut Window, cx: &mut App) -> Entity<Self> {
        Self::for_pr(None, window, cx)
    }

    /// A Diff panel bound to `pr`, or following the focus for `None`.
    pub fn for_pr(pr: Option<PrKey>, window: &mut Window, cx: &mut App) -> Entity<Self> {
        init(cx);
        cx.new(|cx| {
            let session = Session::global(cx);
            let binding = Binding::new(pr, PanelKind::Diff, cx);
            cx.observe(&session, |_: &mut Self, _, cx| cx.notify())
                .detach();
            cx.subscribe_in(
                &session,
                window,
                |this: &mut Self, _, event: &SessionEvent, window, cx| {
                    let shown = this.binding.key(cx);
                    match event {
                        SessionEvent::FileSelected { pr, path } if Some(pr) == shown.as_ref() => {
                            this.pending_jump = Some(path.clone());
                            cx.notify();
                        }
                        SessionEvent::JumpToThread {
                            pr,
                            thread,
                            focus_diff,
                        } if Some(pr) == shown.as_ref() => {
                            this.pending_thread = Some(thread.clone());
                            if *focus_diff {
                                window.focus(&this.focus, cx);
                            }
                            cx.notify();
                        }
                        _ => {}
                    }
                },
            )
            .detach();
            cx.observe_global::<ActiveTheme>(|this: &mut Self, cx| this.restyle(cx))
                .detach();
            cx.observe_global::<Appearance>(|this: &mut Self, cx| {
                this.apply_collapse_setting(cx);
            })
            .detach();
            let writing = cx.new(|_| Writing::with_log_sink());
            let review_bar = cx.new(ReviewBar::new);
            let actions = cx.new(|cx| PrActions::new(true, window, cx));
            let cards = cx.new(|_| ThreadCards::new(writing.clone()));
            // A folded or selected card changes what the diff draws; the view
            // sits in a cached dock panel, so it is told directly.
            cx.observe(&cards, |this: &mut Self, _, cx| {
                if let Some(loaded) = &this.loaded {
                    loaded.view.update(cx, |_, cx| cx.notify());
                }
            })
            .detach();
            cx.subscribe_in(
                &cards,
                window,
                |this: &mut Self, cards, event: &CardsEvent, window, cx| {
                    let CardsEvent::Selected(ix) = *event;
                    cards.update(cx, |cards, cx| cards.set_current(Some(ix), cx));
                    window.focus(&this.focus, cx);
                },
            )
            .detach();
            Self {
                focus: cx.focus_handle(),
                session,
                binding,
                loaded: None,
                highlighter: Highlighter::with_default_workers(),
                cards,
                pending_jump: None,
                pending_thread: None,
                writing,
                writing_subscriptions: Vec::new(),
                actions,
                review_bar,
                bounds: Rc::default(),
                pointer: None,
                hovered: None,
                drag: None,
                selection: None,
                reveal: None,
                menu: None,
                whole_mode: ContextMode::Full,
                whole_restore: None,
            }
        })
    }

    /// Follows the writing state of the pull request shown, once it has
    /// one: the cards draw from it, the review bar shows its suggestions.
    fn sync_writing(&mut self, cx: &mut Context<Self>) {
        let Some(key) = self.binding.key(cx) else {
            return;
        };
        let Some(writing) = self.session.read(cx).writing_for(&key) else {
            return;
        };
        if writing == self.writing {
            return;
        }
        self.writing = writing.clone();
        self.writing_subscriptions = vec![
            cx.observe(&writing, |this: &mut Self, _, cx| this.sync_extras(cx)),
            cx.subscribe(&writing, |this: &mut Self, _, event: &WritingEvent, _| {
                if let WritingEvent::Opened(anchor) = event {
                    this.reveal = Some(anchor.clone());
                }
            }),
        ];
        self.cards
            .update(cx, |cards, _| cards.set_writing(writing.clone()));
        // A composer opened before this panel followed the writing state
        // (a scripted run, a jump from another panel) was not heard opening.
        self.reveal = writing
            .read(cx)
            .slots()
            .iter()
            .find(|slot| slot.wants_focus())
            .map(|slot| slot.anchor.clone());
        self.review_bar
            .update(cx, |bar, cx| bar.set_pr(Some(key), cx));
        self.sync_extras(cx);
    }

    /// Shows `thread` once the diff is up, as a jump does; for a panel the
    /// jump itself built, which was not there to hear it.
    pub fn reveal_thread(
        &mut self,
        thread: ThreadId,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pending_thread = Some(thread);
        if focus {
            window.focus(&self.focus, cx);
        }
        cx.notify();
    }

    /// Scrolls to `path` once the diff is up.
    pub fn reveal_file(&mut self, path: SharedString, cx: &mut Context<Self>) {
        self.pending_jump = Some(path);
        cx.notify();
    }

    /// The pull request this panel draws now.
    pub fn pr(&self, cx: &App) -> Option<PrKey> {
        self.binding.key(cx)
    }

    /// The list of the open pull request, once it has been built.
    pub fn view(&self) -> Option<&Entity<DiffView>> {
        self.loaded.as_ref().map(|loaded| &loaded.view)
    }

    pub fn cards(&self) -> &Entity<ThreadCards> {
        &self.cards
    }

    fn restyle(&mut self, cx: &mut Context<Self>) {
        if let Some(loaded) = &self.loaded {
            let t = ui::theme(cx);
            let style = diff_style(&t);
            let (_, rails) = self.cards.read(cx).blocks(&t, cx);
            loaded.view.update(cx, |view, cx| {
                view.set_style(style, cx);
                view.set_rails(rails);
            });
        }
        cx.notify();
    }

    /// Builds the view for the open pull request's current diff, keeping the
    /// scroll offset when it replaces an earlier diff of the same one, and
    /// places its threads as blocks.
    fn ensure_view(
        &mut self,
        key: &PrKey,
        open: &OpenDiff,
        threads: &Arc<[Thread]>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<DiffView> {
        let t = ui::theme(cx);
        if let Some(loaded) = self
            .loaded
            .as_mut()
            .filter(|loaded| &loaded.key == key && loaded.generation == open.generation)
        {
            if !Arc::ptr_eq(&loaded.threads, threads) {
                loaded.threads = threads.clone();
                let view = loaded.view.clone();
                let (blocks, rails) = self.cards.update(cx, |cards, cx| {
                    let view = view.read(cx);
                    let revealed = view.reveals();
                    cards.set_threads(threads.clone(), view.list().diff(), &revealed, &t, cx)
                });
                loaded
                    .view
                    .update(cx, |view, _| view.set_blocks(blocks, rails));
            }
            return loaded.view.clone();
        }
        let same_pr = self
            .loaded
            .as_ref()
            .is_some_and(|loaded| &loaded.key == key);
        if !same_pr {
            self.cards.update(cx, |cards, _| cards.reset());
        }
        let layout = self.session.read(cx).diff_layout();
        let style = diff_style(&t);
        let scroll = self
            .loaded
            .as_ref()
            .filter(|_| same_pr)
            .map(|loaded| loaded.view.read(cx).scroll_y());
        // A diff built again at the same head keeps what was revealed in it.
        let revealed = self
            .loaded
            .as_ref()
            .filter(|loaded| same_pr && loaded.head == open.head_sha)
            .map(|loaded| loaded.view.read(cx).reveals())
            .unwrap_or_default();
        let mut diff = (*open.diff).clone();
        let (blocks, rails) = self.cards.update(cx, |cards, cx| {
            cards.set_threads(threads.clone(), &diff, &revealed, &t, cx)
        });
        diff.blocks = blocks;
        let highlighter = self.highlighter.clone();
        let cards = self.cards.clone();
        let controls = HeaderControls {
            panel: cx.weak_entity(),
            session: self.session.clone(),
            key: key.clone(),
            paths: diff.files.iter().map(|file| file.path.clone()).collect(),
        };
        let view = cx.new(|_| {
            let mut view = DiffView::new(diff, layout, style);
            view.set_highlighter(highlighter);
            view.set_rails(rails);
            view.set_block_renderer(Rc::new(move |id, width, window, cx| {
                cards.update(cx, |cards, cx| cards.render_block(id, width, window, cx))
            }));
            view.set_header_renderer(
                file_header::SLOTS,
                Rc::new(move |place, _, cx| controls.render(place, cx)),
            );
            view.set_reveals(&revealed);
            if let Some(y) = scroll {
                view.set_scroll_y(y);
            }
            view
        });
        self.whole_restore = None;
        self.menu = None;
        self.loaded = Some(Loaded {
            key: key.clone(),
            generation: open.generation,
            view: view.clone(),
            threads: threads.clone(),
            viewed: Vec::new(),
            restored: false,
            head: open.head_sha.clone(),
        });
        cx.observe(&view, |this: &mut Self, _, cx| this.rehover(cx))
            .detach();
        cx.subscribe(
            &view,
            |this: &mut Self, _, event: &DiffViewEvent, cx| match event {
                DiffViewEvent::ToggleViewed(file) => this.toggle_viewed(*file, cx),
                DiffViewEvent::SplitRatioChanged(ratio) => {
                    let ratio = *ratio;
                    appearance::update_quiet(cx, move |settings| {
                        settings.diff.split_ratio = Some(ratio);
                    });
                }
                DiffViewEvent::FileCollapsed(..) => this.remember_collapsed(cx),
                // Composers on lines now shown move under them.
                DiffViewEvent::ContextRevealed(_) => this.sync_extras(cx),
                DiffViewEvent::ContextMenu { position, target } => {
                    this.open_menu(*position, target.clone(), cx);
                }
            },
        )
        .detach();
        let fold_viewed = collapse_viewed(cx);
        view.update(cx, |view, cx| view.set_collapse_viewed(fold_viewed, cx));
        if let Some(ratio) = cx
            .try_global::<Appearance>()
            .and_then(|state| state.settings.diff.split_ratio)
        {
            view.update(cx, |view, cx| view.set_split_ratio(ratio, cx));
        }
        let (session, generation) = (self.session.clone(), open.generation);
        window.on_next_frame(move |_, cx| {
            let painted = session.update(cx, |session, _| session.mark_first_paint(generation));
            if let Some((elapsed, label)) = painted {
                eprintln!(
                    "review: {label} first diff paint {:.1} ms after open",
                    elapsed.as_secs_f64() * 1000.0
                );
            }
        });
        view
    }

    /// The pull request's changed file shown as `file` in the view.
    fn changed_file(&self, file: FileIx, cx: &App) -> Option<(PrKey, ChangedFile)> {
        let loaded = self.loaded.as_ref()?;
        let path = loaded
            .view
            .read(cx)
            .list()
            .diff()
            .files
            .get(file.0 as usize)?
            .path
            .clone();
        let open = self.session.read(cx).pr(&loaded.key)?;
        let changed = open.files.iter().find(|f| f.path.as_ref() == path)?.clone();
        Some((loaded.key.clone(), changed))
    }

    /// A header's Viewed checkbox, or `v`: flips the file's mark the way the
    /// file tree does (stored by path and blob, sent to a forge that keeps
    /// it); the headers follow in [`Self::sync_viewed`].
    fn toggle_viewed(&mut self, file: FileIx, cx: &mut Context<Self>) {
        let Some((key, changed)) = self.changed_file(file, cx) else {
            return;
        };
        self.session.update(cx, |session, cx| {
            session.focus(&key, cx);
            session.toggle_viewed(&changed, cx);
        });
        self.sync_viewed(cx);
    }

    fn toggle_current_viewed(
        &mut self,
        _: &ToggleFileViewed,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = self
            .loaded
            .as_ref()
            .and_then(|loaded| loaded.view.read(cx).current_file());
        if let Some(file) = current {
            self.toggle_viewed(file, cx);
        }
    }

    /// Gives the headers the marks the session holds, whatever changed them
    /// (a header, the tree, `v`, the forge).
    fn sync_viewed(&mut self, cx: &mut Context<Self>) {
        let Some(loaded) = self.loaded.as_mut() else {
            return;
        };
        let session = self.session.read(cx);
        let Some(open) = session.pr(&loaded.key) else {
            return;
        };
        let marks: Vec<bool> = loaded
            .view
            .read(cx)
            .list()
            .diff()
            .files
            .iter()
            .map(|file| {
                open.files
                    .iter()
                    .find(|f| f.path.as_ref() == file.path)
                    .is_some_and(|f| open.viewed.contains(f))
            })
            .collect();
        if marks == loaded.viewed {
            return;
        }
        loaded.viewed.clone_from(&marks);
        loaded
            .view
            .update(cx, |view, cx| view.set_viewed_all(marks, cx));
        // Marks that land after a composer opened must not fold it away.
        let view = loaded.view.clone();
        let waiting: Vec<DraftAnchor> = self
            .writing
            .read(cx)
            .slots()
            .iter()
            .filter(|slot| slot.wants_focus())
            .map(|slot| slot.anchor.clone())
            .collect();
        for anchor in &waiting {
            open_file_of(anchor, &view, cx);
        }
        self.remember_collapsed(cx);
    }

    /// Places the open composers and queued comments again after they
    /// changed, and brings a composer the user just opened into view.
    fn sync_extras(&mut self, cx: &mut Context<Self>) {
        if let Some(loaded) = &self.loaded {
            let t = ui::theme(cx);
            let view = loaded.view.clone();
            let placed = self.cards.update(cx, |cards, cx| {
                let view = view.read(cx);
                cards.replace_extras(view.list().diff(), &view.reveals(), &t, cx)
            });
            if let Some((blocks, rails)) = placed {
                view.update(cx, |view, cx| {
                    view.set_blocks(blocks, rails);
                    cx.notify();
                });
            }
            if let Some(anchor) = self.reveal.take() {
                self.reveal_composer(&anchor, &view, cx);
            }
        }
        cx.notify();
    }

    fn reveal_composer(
        &mut self,
        anchor: &DraftAnchor,
        view: &Entity<DiffView>,
        cx: &mut Context<Self>,
    ) {
        open_file_of(anchor, view, cx);
        let block = self.cards.read(cx).block_for(anchor, cx);
        let Some(block) = block else {
            return;
        };
        view.update(cx, |view, cx| {
            let row = view.list().block_row(block);
            let top = Self::thread_row(view, block);
            if !view.rows_in_view().contains(&row) {
                let margin = Self::thread_margin(view);
                view.scroll_to_row(top, margin);
            }
            cx.notify();
        });
    }

    fn capabilities(&self, cx: &App) -> Option<Capabilities> {
        let key = self.binding.key(cx)?;
        self.session.read(cx).capabilities_for(&key)
    }

    fn hit_at(&self, pointer: Point<Pixels>, cx: &App) -> Option<gutter::Hit> {
        let view = self.loaded.as_ref()?.view.read(cx);
        gutter::hit(view, f32::from(pointer.x), f32::from(pointer.y))
    }

    /// The pointer or the view moved: which line is under the pointer now.
    fn rehover(&mut self, cx: &mut Context<Self>) {
        let hit = self.pointer.and_then(|pointer| self.hit_at(pointer, cx));
        if hit != self.hovered {
            self.hovered = hit;
            cx.notify();
        }
    }

    fn pointer_moved(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pointer = Some(event.position - self.bounds.get().origin);
        let hit = self.pointer.and_then(|pointer| self.hit_at(pointer, cx));
        let mut changed = hit != self.hovered;
        self.hovered = hit;
        if self.drag.is_some() && event.pressed_button != Some(MouseButton::Left) {
            self.finish_drag(window, cx);
            return;
        }
        if let (Some(drag), Some(loaded)) = (&mut self.drag, &self.loaded) {
            let range = gutter::Range::drag(&drag.start, hit.as_ref(), loaded.view.read(cx).list());
            if range != drag.range {
                drag.range = range;
                changed = true;
            }
        }
        if changed {
            cx.notify();
        }
    }

    fn pointer_left(&mut self, cx: &mut Context<Self>) {
        self.pointer = None;
        if self.hovered.take().is_some() {
            cx.notify();
        }
    }

    fn pressed(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.binding.activated(cx);
        // A click inside a composer focuses its text box; keep it there.
        if !self.focus.contains_focused(window, cx) {
            window.focus(&self.focus, cx);
        }
        let pointer = event.position - self.bounds.get().origin;
        match self.hit_at(pointer, cx) {
            Some(hit) if hit.in_gutter => {
                self.selection = None;
                self.drag = Some(Drag {
                    range: gutter::Range::of(&hit),
                    start: hit,
                });
                cx.notify();
            }
            _ => {
                if self.selection.take().is_some() {
                    cx.notify();
                }
            }
        }
    }

    fn released(&mut self, _: &MouseUpEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.finish_drag(window, cx);
    }

    fn finish_drag(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let Some(drag) = self.drag.take() else {
            return;
        };
        self.selection = Some(drag.range);
        self.open_range(&drag.range, cx);
        cx.notify();
    }

    /// Opens a composer on `range`: one line, or lines `first` to `last`
    /// where the forge takes ranges.
    fn open_range(&mut self, range: &gutter::Range, cx: &mut Context<Self>) {
        let Some(loaded) = &self.loaded else {
            return;
        };
        let multi_line = self
            .capabilities(cx)
            .is_some_and(|caps| caps.multi_line_comments);
        let view = loaded.view.read(cx);
        let diff = view.list().diff();
        let file = diff.file(range.file);
        let anchor = range.anchor(&file.path, multi_line);
        let first = if multi_line { range.first } else { range.last };
        let suggestion =
            (range.side == Side::New && range.last < file.new.line_count()).then(|| {
                (first..=range.last)
                    .map(|line| file.new.line(line))
                    .collect::<Vec<_>>()
                    .join("\n")
            });
        self.writing
            .update(cx, |writing, cx| writing.open(anchor, suggestion, cx));
    }

    /// The line `c` comments on: the one under the pointer, or else the
    /// first line in view.
    fn current_line(&self, cx: &App) -> Option<gutter::Range> {
        if let Some(hit) = &self.hovered {
            return Some(gutter::Range::of(hit));
        }
        let view = self.loaded.as_ref()?.view.read(cx);
        let list = view.list();
        view.rows_in_view().find_map(|row| {
            let file = gutter::file_of(list, row)?;
            [Side::New, Side::Old].into_iter().find_map(|side| {
                let line = gutter::line_on(list, row, side)?;
                Some(gutter::Range {
                    file,
                    side,
                    first: line,
                    last: line,
                    rows: (row, row),
                })
            })
        })
    }

    fn open_composer(&mut self, _: &OpenComposer, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(range) = self.current_line(cx) {
            self.open_range(&range, cx);
        }
    }

    fn open_range_composer(
        &mut self,
        _: &OpenRangeComposer,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(range) = self.selection.or_else(|| self.current_line(cx)) {
            self.open_range(&range, cx);
        }
    }

    /// The "add comment" button on the hovered line, and the band over the
    /// lines being dragged over or last dragged over.
    fn gutter_overlays(&self, view: &Entity<DiffView>, t: &Theme, cx: &App) -> Vec<AnyElement> {
        let c = &t.colors;
        let view = view.read(cx);
        let list = view.list();
        let scroll = view.scroll_y();
        let line = list.heights().code;
        let mut overlays = Vec::new();
        let range = self.drag.as_ref().map(|d| d.range).or(self.selection);
        if let Some(range) = range {
            let top = list.top_of(range.rows.0) - scroll;
            let bottom = list.top_of(range.rows.1) + list.row_height(range.rows.1) - scroll;
            overlays.push(
                div()
                    .absolute()
                    .left_0()
                    .w_full()
                    .top(px(top as f32))
                    .h(px((bottom - top) as f32))
                    .bg(c.selection.opacity(0.4))
                    .into_any_element(),
            );
        }
        if let (Some(hit), None) = (&self.hovered, &self.drag) {
            let top = list.top_of(hit.row) - scroll;
            let size = line - 4.0;
            overlays.push(
                div()
                    .absolute()
                    .left(px(hit.pane as f32 + 2.0))
                    .top(px(top as f32 + 2.0))
                    .size(px(size))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(t.radius.sm)
                    .bg(c.accent)
                    .text_color(c.accent_text)
                    .cursor_pointer()
                    .child(Icon::new(IconName::Plus).size(px((size - 4.0).max(8.0))))
                    .into_any_element(),
            );
        }
        overlays
    }

    /// The row a thread is shown from: its first commented line, or the
    /// card itself when it has no line in view.
    fn thread_row(view: &DiffView, block: BlockId) -> usize {
        let span = view.list().block_span(block);
        if span.is_empty() {
            view.list().block_row(block)
        } else {
            span.start
        }
    }

    fn thread_margin(view: &DiffView) -> f64 {
        f64::from(view.list().heights().code) * THREAD_CONTEXT_ROWS
    }

    /// Scrolls to a thread and makes it the current one.
    fn show_thread(&mut self, thread: usize, cx: &mut Context<Self>) {
        let Some(view) = self.view().cloned() else {
            return;
        };
        let Some(block) = self.cards.read(cx).block_of(thread) else {
            return;
        };
        // A thread in a collapsed (often viewed) file would stay hidden.
        let path = self.cards.read(cx).threads()[thread]
            .anchor
            .as_ref()
            .map(|anchor| anchor.path.clone());
        if let Some(path) = path {
            open_file(&path, &view, cx);
        }
        view.update(cx, |view, cx| {
            let row = Self::thread_row(view, block);
            let margin = Self::thread_margin(view);
            view.scroll_to_row(row, margin);
            cx.notify();
        });
        self.cards
            .update(cx, |cards, cx| cards.set_current(Some(thread), cx));
    }

    fn take_pending_thread(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.pending_thread.take() else {
            return;
        };
        let index = self
            .cards
            .read(cx)
            .threads()
            .iter()
            .position(|thread| thread.id == id);
        if let Some(index) = index {
            self.show_thread(index, cx);
        }
    }

    fn step(&mut self, target: Target, step: Step, cx: &mut Context<Self>) {
        let Some(view) = self.view().cloned() else {
            return;
        };
        view.update(cx, |view, cx| {
            let rows: Vec<usize> = match target {
                Target::File => view.list().file_rows(),
                Target::Hunk => view.list().hunk_rows(),
            }
            .iter()
            .map(|&row| row as usize)
            .collect();
            if view.step_to(rows, step, 0.0).is_some() {
                cx.notify();
            }
        });
    }

    /// Threads in the order they appear, with the row each is shown from.
    fn thread_order(&self, view: &DiffView, cx: &App) -> Vec<(usize, usize)> {
        let cards = self.cards.read(cx);
        let mut order: Vec<(usize, usize)> = cards
            .placements()
            .iter()
            .enumerate()
            .map(|(ix, p)| (Self::thread_row(view, BlockId(ix as u32)), p.thread))
            .collect();
        order.sort_unstable();
        order
    }

    /// The current thread when its card or lines are on screen.
    fn current_in_view(&self, view: &DiffView, cx: &App) -> Option<usize> {
        let cards = self.cards.read(cx);
        let current = cards.current()?;
        let block = cards.block_of(current)?;
        let in_view = view.rows_in_view();
        let shown = in_view.contains(&view.list().block_row(block))
            || in_view.contains(&Self::thread_row(view, block));
        shown.then_some(current)
    }

    /// `]t` / `[t`: from the current thread while it is on screen, otherwise
    /// from the top of the view.
    fn step_thread(&mut self, step: Step, cx: &mut Context<Self>) {
        let Some(view) = self.view().cloned() else {
            return;
        };
        let order = self.thread_order(view.read(cx), cx);
        let from = self
            .current_in_view(view.read(cx), cx)
            .and_then(|current| order.iter().position(|&(_, thread)| thread == current));
        let target = match from {
            Some(at) => match step {
                Step::Next => order.get(at + 1),
                Step::Prev => at.checked_sub(1).and_then(|at| order.get(at)),
            }
            .map(|&(_, thread)| thread),
            None => {
                let rows: Vec<usize> = order.iter().map(|&(row, _)| row).collect();
                let found = view.update(cx, |view, _| {
                    let margin = Self::thread_margin(view);
                    view.step_to(rows, step, margin)
                });
                found.and_then(|row| order.iter().find(|&&(r, _)| r == row).map(|&(_, t)| t))
            }
        };
        if let Some(thread) = target {
            self.show_thread(thread, cx);
        }
    }

    /// `x`: folds or unfolds the current thread, or the first one on screen.
    fn toggle_thread(&mut self, _: &ToggleThread, _: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.view().cloned() else {
            return;
        };
        let thread = self.current_in_view(view.read(cx), cx).or_else(|| {
            let in_view = view.read(cx).rows_in_view();
            self.thread_order(view.read(cx), cx)
                .into_iter()
                .find(|(row, _)| in_view.contains(row))
                .map(|(_, thread)| thread)
        });
        if let Some(thread) = thread {
            self.cards.update(cx, |cards, cx| {
                cards.set_current(Some(thread), cx);
                cards.toggle(thread, cx);
            });
        }
    }

    /// A two-button segmented control for unified and split.
    fn layout_toggle(&self, current: Layout, t: &Theme, cx: &mut Context<Self>) -> Div {
        let c = &t.colors;
        let hover_text = c.text;
        let button = |label: &'static str, layout: Layout| {
            let on = current == layout;
            div()
                .id(label)
                .h_full()
                .px(px(9.0))
                .flex()
                .items_center()
                .tiny(t)
                .cursor_pointer()
                .when(on, |el| el.bg(c.selected).text_color(c.accent_fg))
                .when(!on, |el| {
                    el.text_color(c.text_muted)
                        .hover(move |style| style.text_color(hover_text))
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.session
                        .update(cx, |session, cx| session.set_diff_layout(layout, cx));
                }))
                .child(label)
        };
        h_flex()
            .h(px(22.0))
            .border_1()
            .border_color(c.border_strong)
            .rounded(t.radius.md)
            .overflow_hidden()
            .child(button("Unified", Layout::Unified))
            .child(button("Split", Layout::Split))
    }
}

/// The header's progress note, and whether it is an error.
fn status_note(status: &OpenStatus) -> Option<(SharedString, bool)> {
    match status {
        OpenStatus::Loading => Some(("loading".into(), false)),
        OpenStatus::Fetching(None) => Some(("fetching".into(), false)),
        OpenStatus::Fetching(Some(line)) => Some((format!("fetching: {line}").into(), false)),
        OpenStatus::Building => Some(("computing diff".into(), false)),
        OpenStatus::Ready => None,
        OpenStatus::Stale(error) => Some((
            format!("fetch failed, showing local objects: {error}").into(),
            true,
        )),
        OpenStatus::Failed(error) => Some((error.clone(), true)),
    }
}

impl Panel for DiffPanel {
    fn title(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.binding.title(None, cx)
    }

    fn inner_padding(&self, _: &App) -> bool {
        false
    }
}

impl Render for DiffPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_writing(cx);
        if let Some(key) = self.binding.key(cx) {
            self.actions
                .update(cx, |actions, cx| actions.bind(&key, window, cx));
        }
        let t = ui::theme(cx);
        let c = &t.colors;
        let session = self.session.read(cx);
        let Some(open) = self.binding.open(session) else {
            return v_flex()
                .id("diff-panel")
                .size_full()
                .bg(c.surface)
                .child(ui::empty_note(cx, "No pull request open"));
        };
        let layout = session.diff_layout();
        let key = open.key.clone();
        let keep = session
            .is_preview(&key)
            .then(|| crate::conversation::keep_button(&self.session, &key, cx));
        let pr = &open.pr;
        let summary = format!("{} files changed", open.files.len());
        let (added, removed) = open.stat();
        let sha = open
            .diff
            .as_ref()
            .map_or_else(|| pr.sha.clone(), |d| d.head.clone());
        let branches = if pr.head.is_empty() {
            format!("@ {sha}")
        } else {
            format!("{} into {}  @ {sha}", pr.head, pr.base)
        };
        let note = status_note(&open.status);
        let diff = open.diff.clone();
        let threads = open.threads.clone();
        let open_threads = threads
            .iter()
            .filter(|thread| thread.anchor.is_some() && !thread.is_resolved())
            .count();
        let drafts = self.writing.read(cx).draft_count();
        let writing = self.writing.clone();
        let selected = open.selected_file.clone();

        let toggle = self.layout_toggle(layout, &t, cx);
        let body = match diff {
            Some(diff) => {
                let view = self.ensure_view(&key, &diff, &threads, window, cx);
                view.update(cx, |view, _| view.set_layout(layout));
                self.sync_viewed(cx);
                self.restore_collapsed(cx);
                let jump = self.pending_jump.take().is_some();
                self.sync_shown_file(selected.as_ref(), jump, cx);
                self.take_pending_thread(cx);
                let overlays = self.gutter_overlays(&view, &t, cx);
                let bounds = self.bounds.clone();
                div()
                    .debug_selector(|| "diff-body".into())
                    .id("diff-body")
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .on_mouse_move(cx.listener(Self::pointer_moved))
                    .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                        if !*hovered {
                            this.pointer_left(cx);
                        }
                    }))
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::pressed))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::released))
                    .on_mouse_up_out(MouseButton::Left, cx.listener(Self::released))
                    .child(view)
                    .child(
                        canvas(move |at, _, _| bounds.set(at), |_, (), _, _| {})
                            .absolute()
                            .top_0()
                            .left_0()
                            .size_full(),
                    )
                    .children(overlays)
                    .into_any_element()
            }
            None => {
                let text = note
                    .as_ref()
                    .map_or_else(|| "No changes".into(), |(text, _)| text.clone());
                div()
                    .flex_1()
                    .min_h_0()
                    .child(ui::empty_note(cx, &text))
                    .into_any_element()
            }
        };
        let alone = self
            .view()
            .is_some_and(|view| view.read(cx).shown_file().is_some());
        let whole = alone.then(|| self.whole_file_controls(&t, cx));
        let menu = self
            .menu
            .as_ref()
            .map(|menu| self.menu_overlay(menu, &t, cx));
        let leading = h_flex()
            .flex_1()
            .gap(px(10.0))
            .min_w_0()
            .overflow_hidden()
            .items_center()
            .children(keep)
            .child(div().mono(&t).min_w_0().truncate().child(branches))
            .children(note.map(|(text, error)| {
                div()
                    .truncate()
                    .text_color(if error { c.danger } else { c.text_muted })
                    .child(text)
            }));
        v_flex()
            .id("diff-panel")
            .key_context(DIFF_KEY_CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &NextFile, _, cx| {
                this.step(Target::File, Step::Next, cx);
            }))
            .on_action(cx.listener(|this, _: &PrevFile, _, cx| {
                this.step(Target::File, Step::Prev, cx);
            }))
            .on_action(cx.listener(|this, _: &NextHunk, _, cx| {
                this.step(Target::Hunk, Step::Next, cx);
            }))
            .on_action(cx.listener(|this, _: &PrevHunk, _, cx| {
                this.step(Target::Hunk, Step::Prev, cx);
            }))
            .on_action(cx.listener(|this, _: &NextThread, _, cx| {
                this.step_thread(Step::Next, cx);
            }))
            .on_action(cx.listener(|this, _: &PrevThread, _, cx| {
                this.step_thread(Step::Prev, cx);
            }))
            .on_action(cx.listener(Self::toggle_thread))
            .on_action(cx.listener(Self::toggle_current_viewed))
            .on_action(cx.listener(Self::toggle_file_collapsed))
            .on_action(cx.listener(Self::toggle_all_collapsed))
            .on_action(cx.listener(Self::close_menu))
            .on_action(cx.listener(Self::open_composer))
            .on_action(cx.listener(Self::open_range_composer))
            .on_action(cx.listener(|this, _: &NextInStack, _, cx| {
                this.session
                    .update(cx, |session, cx| session.step_stack(StackStep::Down, cx));
            }))
            .on_action(cx.listener(|this, _: &PrevInStack, _, cx| {
                this.session
                    .update(cx, |session, cx| session.step_stack(StackStep::Up, cx));
            }))
            .size_full()
            .bg(c.surface)
            .child(header(
                cx,
                leading,
                h_flex()
                    .flex_none()
                    .gap(px(10.0))
                    .items_center()
                    .when(drafts > 0, |el| {
                        el.child(
                            div()
                                .id("drafts-chip")
                                .cursor_pointer()
                                .on_click(move |_, _, cx| {
                                    writing.update(cx, |writing, cx| writing.reopen_all(cx));
                                })
                                .child(ui::chip(
                                    cx,
                                    format!("{drafts} draft{}", if drafts == 1 { "" } else { "s" }),
                                    c.accent_fg,
                                )),
                        )
                    })
                    .when(open_threads > 0, |el| {
                        el.child(ui::chip(
                            cx,
                            format!(
                                "{open_threads} open thread{}",
                                if open_threads == 1 { "" } else { "s" }
                            ),
                            c.info,
                        ))
                    })
                    .child(div().child(summary))
                    .child(div().mono(&t).text_color(c.ok).child(format!("+{added}")))
                    .child(
                        div()
                            .mono(&t)
                            .text_color(c.danger)
                            .child(format!("-{removed}")),
                    )
                    .children(whole)
                    .child(toggle),
            ))
            .child(self.actions.clone())
            .child(self.review_bar.clone())
            .child(body)
            .children(menu)
    }
}

/// Expands the file a new comment is on: a collapsed (often viewed) file
/// would never draw its composer.
fn open_file_of(anchor: &DraftAnchor, view: &Entity<DiffView>, cx: &mut App) {
    if let DraftAnchor::Line { path, .. } | DraftAnchor::File { path } = anchor {
        open_file(path, view, cx);
    }
}

/// Expands the file at `path`, if the diff has it.
fn open_file(path: &str, view: &Entity<DiffView>, cx: &mut App) {
    view.update(cx, |view, cx| {
        let ix = view.list().diff().files.iter().position(|f| f.path == path);
        if let Some(ix) = ix.and_then(|ix| u32::try_from(ix).ok()) {
            view.set_file_collapsed(FileIx(ix), false, cx);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use diff::{Diff, FileDiff, Hunk, Segment, Text};
    use forge::model::{Anchor, Comment, CommentId, LineRef, Position, Resolution, Side, User};

    use crate::threads::place;

    fn thread(id: &str, side: Side, line: Option<u32>, original: Option<u32>) -> Thread {
        let position = |line| Position {
            line: LineRef { side, line },
            start: None,
            commit: None,
        };
        Thread {
            id: ThreadId::new(id),
            anchor: Some(Anchor {
                path: "a.rs".into(),
                old_path: None,
                position: line.map(position),
                original: original.map(position),
            }),
            resolution: Resolution::Unresolved,
            outdated: line.is_none(),
            comments: vec![Comment {
                id: CommentId::new(id),
                author: User::new("mira"),
                body: "body".into(),
                created_at: chrono::Utc::now(),
                edited_at: None,
                web_url: None,
                review: None,
                verdict: None,
                suggestions: Vec::new(),
            }],
        }
    }

    fn diff() -> Diff {
        let text = |n: u32| Text::new((0..n).map(|i| format!("l{i}\n")).collect::<String>());
        Diff {
            files: vec![FileDiff {
                path: "a.rs".into(),
                old: text(40),
                new: text(40),
                hunks: vec![Hunk {
                    old_start: 9,
                    new_start: 9,
                    segments: vec![Segment::Context(3)],
                }],
            }],
            blocks: Vec::new(),
        }
    }

    #[test]
    fn threads_land_on_their_line_or_the_file_top() {
        let threads = [
            thread("current", Side::New, Some(11), None),
            thread("removed", Side::Old, Some(10), None),
            thread("outdated-shown", Side::New, None, Some(12)),
            thread("outdated-gone", Side::New, None, Some(30)),
        ];
        let placed = place(&threads, &diff());
        let anchors: Vec<_> = placed
            .iter()
            .map(|p| (p.anchor, p.outdated_group))
            .collect();
        let file = FileIx(0);
        assert_eq!(
            anchors,
            [
                (
                    diff::Anchor::Line {
                        file,
                        side: diff::Side::New,
                        line: 10
                    },
                    false
                ),
                (
                    diff::Anchor::Line {
                        file,
                        side: diff::Side::Old,
                        line: 9
                    },
                    false
                ),
                (
                    diff::Anchor::Line {
                        file,
                        side: diff::Side::New,
                        line: 11
                    },
                    false
                ),
                (diff::Anchor::FileTop { file }, true),
            ]
        );
    }
}
