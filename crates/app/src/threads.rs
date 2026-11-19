//! Review threads in the diff: where each one sits, and the card drawn there.
//!
//! [`place`] decides each anchored thread's block: under its line (the last
//! line of a range), on the side it was written on; an outdated thread at its
//! original line while the diff still shows that line, otherwise in an
//! "Outdated threads" group at the top of its file. [`ThreadCards`] renders
//! the blocks for `diff_view` and keeps which threads are collapsed.
//!
//! The card's anatomy (rail into the header, avatar and name, body indented
//! under the name, replies split by a hairline, resolved threads folded to one
//! row) is adapted from voximity/dispatch2 (crates/app/src/ui/diff_view.rs,
//! commit 39fd9fd), AGPL-3.0.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::Utc;
use diff::{Anchor, Block, BlockId, FileIx, Reveals, Side};
use diff_view::Rail;
use forge::ResolveMode;
use forge::model::{Comment, LineRef, Resolution, ReviewState, Thread, ThreadId};
use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, Context, Div, Entity, EventEmitter, FocusHandle,
    FontWeight, Hsla, InteractiveElement as _, IntoElement, ParentElement as _, Pixels,
    SharedString, StatefulInteractiveElement as _, Styled, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::text::TextViewState;
use gpui_kit::component::{Icon, h_flex, v_flex};
use store::{DraftAnchor, QueuedComment};
use theme::Theme;

pub mod comment_menu;
pub mod thread_menu;

use self::comment_menu::{CommentMenu, comment_url};
use self::thread_menu::{Origin, ThreadMenu};
use crate::model::relative_time;
use crate::references::{self, RefContext};
use crate::session::Session;
use crate::suggestions::{self, Offer, SuggestionKey};
use crate::ui::{self, Button, ButtonKind, TextExt as _, fold};
use crate::untrusted;
use crate::writing::{ActionState, Writing, anchor_label};

/// Space above the card inside its block, and the header row's height.
const CARD_TOP: f32 = 6.0;
const CARD_BOTTOM: f32 = 8.0;
const HEADER: f32 = 20.0;
const ENTRY_PAD_Y: f32 = 6.0;
const COLLAPSED: f32 = 30.0;
/// Where the rail turns into the card: the middle of its first row (both
/// the expanded header and the collapsed row are centered there).
pub const ELBOW: f32 = CARD_TOP + 1.0 + ENTRY_PAD_Y + HEADER / 2.0;
/// The indent of a body under its author's name, past the avatar.
const BODY_INDENT: f32 = ui::avatar::CARD + 8.0;

/// Height guesses for blocks the view has not measured yet.
const COMPOSER_HEIGHT: f32 = 190.0;
const REPLY_ROW: f32 = 34.0;

/// What a block after the threads' shows.
#[derive(Clone, PartialEq, Eq)]
enum ExtraKind {
    /// Index into the pending review's comments.
    Pending(usize),
    Composer(DraftAnchor),
}

#[derive(Clone, PartialEq, Eq)]
struct Extra {
    kind: ExtraKind,
    spot: Spot,
}

/// A block's anchor in the diff, and the first line of its range.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Spot {
    anchor: Anchor,
    first_line: Option<u32>,
}

fn pending_height(body: &str) -> f32 {
    CARD_TOP
        + CARD_BOTTOM
        + 2.0
        + HEADER
        + 2.0 * ENTRY_PAD_Y
        + 4.0
        + (body.len() / 90 + 1) as f32 * 20.0
}

/// The anchor a reply to `thread` is drafted under.
pub fn reply_anchor(thread: &Thread) -> DraftAnchor {
    DraftAnchor::Reply {
        thread: thread.id.clone(),
    }
}

/// A stable element number for a thread's buttons.
pub fn ix_of(thread: &Thread) -> usize {
    hash_ix(thread.id.as_str())
}

fn hash_ix(text: &str) -> usize {
    text.bytes().fold(0usize, |h, b| {
        h.wrapping_mul(31).wrapping_add(usize::from(b))
    })
}

/// Whether the open pull request's forge lets the viewer resolve threads
/// (Forgejo shows resolved state read-only).
fn can_resolve(thread: &Thread, cx: &App) -> bool {
    let writable = Session::try_global(cx)
        .and_then(|session| session.read(cx).capabilities())
        .is_some_and(|caps| caps.resolve == ResolveMode::ReadWrite);
    writable && thread.anchor.is_some() && thread.is_resolvable()
}

/// The context the diff view reveals around the hunks, by path; see
/// [`diff_view::DiffView::reveals`].
pub type Revealed = HashMap<String, Reveals>;

/// Where a draft's line sits in `diff`: under the line when a hunk or the
/// context `revealed` around them shows it, otherwise at the top of its file.
fn spot_of(anchor: &DraftAnchor, diff: &diff::Diff, revealed: &Revealed) -> Option<Spot> {
    let file_of = |path: &str| {
        let ix = diff.files.iter().position(|f| f.path == path)?;
        Some(FileIx(u32::try_from(ix).ok()?))
    };
    let (path, line, start) = match anchor {
        DraftAnchor::Line { path, line, start } => (path, line, start),
        DraftAnchor::File { path } => {
            return Some(Spot {
                anchor: Anchor::FileTop {
                    file: file_of(path)?,
                },
                first_line: None,
            });
        }
        _ => return None,
    };
    let file = file_of(path)?;
    let side = side_of(line.side);
    let first_line = start
        .filter(|start: &LineRef| start.side == line.side && start.line < line.line)
        .and_then(|start| start.line.checked_sub(1));
    let index = line.line.checked_sub(1)?;
    let file_diff = diff.file(file);
    let shown = file_diff.shows(side, index)
        || revealed
            .get(path.as_str())
            .is_some_and(|reveals| reveals.shows(file_diff, side, index));
    if shown {
        Some(Spot {
            anchor: Anchor::Line {
                file,
                side,
                line: index,
            },
            first_line,
        })
    } else {
        Some(Spot {
            anchor: Anchor::FileTop { file },
            first_line: None,
        })
    }
}

/// Where one thread's card goes.
#[derive(Clone, Debug, PartialEq)]
pub struct Placement {
    /// Index into the thread list.
    pub thread: usize,
    pub anchor: Anchor,
    pub first_line: Option<u32>,
    /// Outdated, and its line is not in the diff any more: listed at the top
    /// of the file.
    pub outdated_group: bool,
}

/// One block per anchored thread whose file is in `diff`, in thread order.
/// Threads on files the diff does not have are left out.
pub fn place(threads: &[Thread], diff: &diff::Diff) -> Vec<Placement> {
    threads
        .iter()
        .enumerate()
        .filter_map(|(ix, thread)| place_one(ix, thread, diff))
        .collect()
}

fn place_one(ix: usize, thread: &Thread, diff: &diff::Diff) -> Option<Placement> {
    let anchor = thread.anchor.as_ref()?;
    let file = diff.files.iter().position(|f| {
        f.path == anchor.path || anchor.old_path.as_deref() == Some(f.path.as_str())
    })?;
    let file = FileIx(file as u32);
    let file_diff = diff.file(file);
    let top = |outdated_group| Placement {
        thread: ix,
        anchor: Anchor::FileTop { file },
        first_line: None,
        outdated_group,
    };
    // The forge's position on the current diff when it has one; an outdated
    // thread it could not map forward falls back to where it was written.
    let (position, mapped) = match (&anchor.position, &anchor.original) {
        (Some(position), _) => (position, true),
        (None, Some(original)) => (original, false),
        (None, None) => return Some(top(false)),
    };
    let side = side_of(position.line.side);
    let line = position.line.line.checked_sub(1)?;
    let shown = file_diff.shows(side, line);
    if !shown && (thread.outdated || !mapped) {
        return Some(top(true));
    }
    let first_line = position
        .start
        .filter(|start| start.side == position.line.side && start.line < position.line.line)
        .and_then(|start| start.line.checked_sub(1));
    Some(Placement {
        thread: ix,
        anchor: Anchor::Line { file, side, line },
        first_line,
        outdated_group: false,
    })
}

fn side_of(side: forge::model::Side) -> Side {
    match side {
        forge::model::Side::Old => Side::Old,
        forge::model::Side::New => Side::New,
    }
}

/// "line 8", "lines 29-31", with "old" for the removed side.
pub fn line_label(thread: &Thread) -> Option<String> {
    let anchor = thread.anchor.as_ref()?;
    let position = anchor.best_position()?;
    let side = match position.line.side {
        forge::model::Side::Old => "old ",
        forge::model::Side::New => "",
    };
    let end = position.line.line;
    Some(match position.start.map(|s| s.line).filter(|&s| s < end) {
        Some(start) => format!("{side}lines {start}\u{2013}{end}"),
        None => format!("{side}line {end}"),
    })
}

/// The thread's file and line, for a compact row: `src/a.rs:31`.
pub fn location(thread: &Thread) -> String {
    let Some(anchor) = &thread.anchor else {
        return String::new();
    };
    match anchor.best_position() {
        Some(position) => format!("{}:{}", anchor.path, position.line.line),
        None => anchor.path.clone(),
    }
}

/// "3 comments, resolved by mira".
pub fn summary(thread: &Thread) -> String {
    let count = match thread.comments.len() {
        1 => "1 comment".to_owned(),
        n => format!("{n} comments"),
    };
    match &thread.resolution {
        Resolution::Resolved { by: Some(by) } => format!("{count}, resolved by {}", by.login),
        Resolution::Resolved { by: None } => format!("{count}, resolved"),
        Resolution::Unresolved | Resolution::NotResolvable => count,
    }
}

/// The first comment's text on one line, for a collapsed row.
pub fn preview(thread: &Thread) -> String {
    thread
        .comments
        .first()
        .map(|c| c.body.split_whitespace().collect::<Vec<_>>().join(" "))
        .unwrap_or_default()
}

pub fn ago(comment: &Comment) -> String {
    let minutes = (Utc::now() - comment.created_at).num_minutes().max(0);
    relative_time(u32::try_from(minutes).unwrap_or(u32::MAX))
}

/// The fold indicator of a thread; the row it sits in, or the control
/// around it, is the click target.
pub(crate) fn chevron(collapsed: bool, t: &Theme) -> Div {
    div()
        .size(px(HEADER))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .child(
            Icon::new(if collapsed {
                IconName::ChevronRight
            } else {
                IconName::ChevronDown
            })
            .size(px(14.0))
            .text_color(t.colors.text_muted),
        )
}

/// The rail and marker color: open threads stand out, resolved ones recede.
pub fn rail_color(t: &Theme, thread: &Thread) -> Hsla {
    match thread.resolution {
        Resolution::Unresolved => t.colors.info,
        Resolution::Resolved { .. } => t.colors.text_faint,
        Resolution::NotResolvable => t.colors.text_muted,
    }
}

/// The host of the open pull request's forge, for fetching avatars.
pub fn open_host(cx: &App) -> SharedString {
    Session::try_global(cx)
        .and_then(|session| Some(session.read(cx).open_key()?.repo.host.clone().into()))
        .unwrap_or_default()
}

/// The words for a review verdict, with its icon and color.
pub fn verdict_style(t: &Theme, verdict: ReviewState) -> (IconName, Hsla, &'static str) {
    let c = &t.colors;
    match verdict {
        ReviewState::Approved => (IconName::CircleCheck, c.ok, "approved these changes"),
        ReviewState::ChangesRequested => (IconName::FileDiff, c.danger, "requested changes"),
        ReviewState::Dismissed => (IconName::Ban, c.text_muted, "had a review dismissed"),
        ReviewState::Commented | ReviewState::Requested | ReviewState::Pending => {
            (IconName::MessageSquare, c.text_muted, "reviewed")
        }
    }
}

/// Parsed markdown bodies, one per comment, kept for as long as the pull
/// request is open so a card that scrolls back into view is not parsed again.
/// An edited text is parsed again.
/// References in them (`#12`, `@user`) become links by the pull request's
/// [`RefContext`].
#[derive(Default)]
pub struct Bodies {
    parsed: HashMap<SharedString, (SharedString, Entity<TextViewState>)>,
    refs: Option<RefContext>,
}

impl Bodies {
    pub fn get(&mut self, key: &str, text: &str, cx: &mut App) -> Entity<TextViewState> {
        if let Some((parsed, state)) = self.parsed.get(key)
            && parsed == text
        {
            return state.clone();
        }
        let source = untrusted::markdown_source(text);
        let source = match &self.refs {
            Some(refs) => references::linkify_markdown(&source, refs).into_owned(),
            None => source.into_owned(),
        };
        let state = cx.new(|cx| TextViewState::markdown(&source, cx));
        self.parsed.insert(
            SharedString::from(key.to_owned()),
            (SharedString::from(text.to_owned()), state.clone()),
        );
        state
    }

    /// Links references by `refs` from now on; bodies parsed under another
    /// context are parsed again.
    pub fn set_refs(&mut self, refs: Option<RefContext>) {
        if self.refs != refs {
            self.refs = refs;
            self.parsed.clear();
        }
    }

    pub fn clear(&mut self) {
        self.parsed.clear();
    }
}

/// A comment body: markdown in the theme's interface font, with forge
/// text's link and image rules.
pub fn body(state: &Entity<TextViewState>, t: &Theme) -> impl IntoElement {
    // As wide as what holds it, so long lines wrap there.
    untrusted::text_view(state)
        .w_full()
        .min_w_0()
        .text_color(t.colors.text)
        .text_size(t.fonts.interface.size_px())
}

/// A request from a card to the panel it is drawn in.
#[derive(Clone, Debug)]
pub enum CardsEvent {
    /// A card was clicked: it becomes the current thread.
    Selected(usize),
}

/// The thread cards of one pull request: what each block shows and which
/// threads are folded.
pub struct ThreadCards {
    writing: Entity<Writing>,
    threads: Arc<[Thread]>,
    /// By block id.
    placements: Vec<Placement>,
    /// The blocks after the threads': the pending review's comments and the
    /// open line composers, by block id less the number of placements.
    extras: Vec<Extra>,
    /// Threads folded or unfolded by hand, against their default (resolved
    /// threads start folded).
    toggled: HashSet<ThreadId>,
    /// The focus of each card's fold row: the row is drawn as another
    /// element when the card folds, and keeps the focus through it.
    row_focus: HashMap<ThreadId, FocusHandle>,
    bodies: Bodies,
    current: Option<usize>,
    /// No composers or pending comments: the diff is one commit's.
    read_only: bool,
}

impl EventEmitter<CardsEvent> for ThreadCards {}

impl ThreadCards {
    pub fn new(writing: Entity<Writing>) -> Self {
        Self {
            writing,
            threads: Arc::new([]),
            placements: Vec::new(),
            extras: Vec::new(),
            toggled: HashSet::new(),
            row_focus: HashMap::new(),
            bodies: Bodies::default(),
            current: None,
            read_only: false,
        }
    }

    /// Forgets the threads and folds, for another pull request.
    /// Draws composers and queued comments from `writing` from now on.
    pub fn set_writing(&mut self, writing: Entity<Writing>) {
        self.writing = writing;
    }

    /// A read-only diff (one commit's) shows no composers and no pending
    /// comments: they anchor to all changes.
    pub fn set_read_only(&mut self, read_only: bool) {
        self.read_only = read_only;
    }

    pub fn reset(&mut self) {
        self.threads = Arc::new([]);
        self.placements.clear();
        self.extras.clear();
        self.toggled.clear();
        self.bodies.clear();
        self.current = None;
    }

    /// Takes a new set of threads placed on `diff`, with the context
    /// `revealed` around its hunks; fold state is kept by thread id. Returns
    /// the blocks and rails for the diff view.
    pub fn set_threads(
        &mut self,
        threads: Arc<[Thread]>,
        diff: &diff::Diff,
        revealed: &Revealed,
        t: &Theme,
        cx: &App,
    ) -> (Vec<Block>, Vec<Option<Rail>>) {
        if !Arc::ptr_eq(&threads, &self.threads) {
            self.bodies.clear();
            self.current = None;
        }
        let refs = Session::try_global(cx).and_then(|session| {
            let pr = self.writing.read(cx).open_pr()?;
            references::context_for(session.read(cx), pr)
        });
        self.bodies.set_refs(refs);
        self.placements = place(&threads, diff);
        self.threads = threads;
        self.place_extras(diff, revealed, cx);
        self.blocks(t, cx)
    }

    /// Places the pending review's comments and the open composers again,
    /// after either (or the context revealed) changed. Returns the blocks and
    /// rails for the diff view. `None` when the same blocks are already in
    /// place.
    pub fn replace_extras(
        &mut self,
        diff: &diff::Diff,
        revealed: &Revealed,
        t: &Theme,
        cx: &App,
    ) -> Option<(Vec<Block>, Vec<Option<Rail>>)> {
        self.place_extras(diff, revealed, cx)
            .then(|| self.blocks(t, cx))
    }

    /// Returns whether the blocks differ from the ones placed before.
    fn place_extras(&mut self, diff: &diff::Diff, revealed: &Revealed, cx: &App) -> bool {
        if self.read_only {
            let placed = !self.extras.is_empty();
            self.extras.clear();
            return placed;
        }
        let writing = self.writing.read(cx);
        let mut extras = Vec::new();
        for (ix, queued) in writing.queued().iter().enumerate() {
            if let Some(spot) = spot_of(&queued.anchor, diff, revealed) {
                extras.push(Extra {
                    kind: ExtraKind::Pending(ix),
                    spot,
                });
            }
        }
        for slot in writing.slots() {
            if slot.open
                && let Some(spot) = spot_of(&slot.anchor, diff, revealed)
            {
                extras.push(Extra {
                    kind: ExtraKind::Composer(slot.anchor.clone()),
                    spot,
                });
            }
        }
        let changed = extras != self.extras;
        self.extras = extras;
        changed
    }

    pub fn threads(&self) -> &Arc<[Thread]> {
        &self.threads
    }

    pub fn placements(&self) -> &[Placement] {
        &self.placements
    }

    /// The block of a thread, by its index.
    pub fn block_of(&self, thread: usize) -> Option<BlockId> {
        self.placements
            .iter()
            .position(|p| p.thread == thread)
            .map(|ix| BlockId(ix as u32))
    }

    /// The block a composer for `anchor` shows in: its own, or the card of
    /// the thread being replied to.
    pub fn block_for(&self, anchor: &DraftAnchor, cx: &App) -> Option<BlockId> {
        let _ = cx;
        if let DraftAnchor::Reply { thread } = anchor {
            let ix = self.threads.iter().position(|t| t.id == *thread)?;
            return self.block_of(ix);
        }
        self.extras
            .iter()
            .position(|extra| extra.kind == ExtraKind::Composer(anchor.clone()))
            .map(|ix| BlockId((self.placements.len() + ix) as u32))
    }

    pub fn current(&self) -> Option<usize> {
        self.current
    }

    pub fn set_current(&mut self, thread: Option<usize>, cx: &mut Context<Self>) {
        if self.current != thread {
            self.current = thread;
            cx.notify();
        }
    }

    /// Resolved threads start folded; one with a reply being written does not.
    pub fn is_collapsed(&self, thread: usize, cx: &App) -> bool {
        let Some(t) = self.threads.get(thread) else {
            return false;
        };
        let replying = self.writing.read(cx).is_open(&reply_anchor(t));
        !replying && t.is_resolved() != self.toggled.contains(&t.id)
    }

    pub fn toggle(&mut self, thread: usize, cx: &mut Context<Self>) {
        let Some(t) = self.threads.get(thread) else {
            return;
        };
        if !self.toggled.remove(&t.id) {
            self.toggled.insert(t.id.clone());
        }
        cx.notify();
    }

    /// The blocks and their rails, by block id.
    pub fn blocks(&self, t: &Theme, cx: &App) -> (Vec<Block>, Vec<Option<Rail>>) {
        let threads = self.placements.iter().map(|p| {
            let thread = &self.threads[p.thread];
            let mut block = Block::new(p.anchor, self.estimate(p.thread, cx));
            block.first_line = p.first_line;
            let rail = matches!(p.anchor, Anchor::Line { .. }).then(|| Rail {
                color: rail_color(t, thread),
                dashed: thread.outdated,
                elbow: px(ELBOW),
            });
            (block, rail)
        });
        let queued = self.writing.read(cx).queued();
        let extras = self.extras.iter().map(|extra| {
            let (height, color) = match &extra.kind {
                ExtraKind::Pending(ix) => (
                    queued.get(*ix).map_or(80.0, |q| pending_height(&q.body)),
                    t.colors.warn,
                ),
                ExtraKind::Composer(_) => (COMPOSER_HEIGHT, t.colors.accent_fg),
            };
            let mut block = Block::new(extra.spot.anchor, height);
            block.first_line = extra.spot.first_line;
            let rail = matches!(extra.spot.anchor, Anchor::Line { .. }).then(|| Rail {
                color,
                dashed: false,
                elbow: px(ELBOW),
            });
            (block, rail)
        });
        threads.chain(extras).unzip()
    }

    /// A first guess at a card's height; the view measures the real one
    /// when the card comes into view.
    fn estimate(&self, thread: usize, cx: &App) -> f32 {
        let frame = CARD_TOP + CARD_BOTTOM + 2.0;
        if self.is_collapsed(thread, cx) {
            return frame + COLLAPSED;
        }
        let comments = &self.threads[thread].comments;
        let lines: usize = comments.iter().map(|c| c.body.len() / 90 + 1).sum();
        let reply = if self
            .writing
            .read(cx)
            .is_open(&reply_anchor(&self.threads[thread]))
        {
            COMPOSER_HEIGHT
        } else {
            REPLY_ROW
        };
        frame
            + comments.len() as f32 * (HEADER + 2.0 * ENTRY_PAD_Y + 4.0)
            + lines as f32 * 20.0
            + reply
    }

    /// The element of one block, `width` wide.
    pub fn render_block(
        &mut self,
        block: BlockId,
        width: Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = ui::theme(cx);
        let index = block.0 as usize;
        if index >= self.placements.len() {
            return self.render_extra(index - self.placements.len(), width, window, &t, cx);
        }
        let placement = self.placements[index].clone();
        let threads = self.threads.clone();
        let ix = placement.thread;
        let thread = &threads[ix];
        // The first outdated card of a file carries the group's caption.
        let caption = placement.outdated_group
            && (block.0 == 0
                || !self.placements[block.0 as usize - 1].outdated_group
                || self.placements[block.0 as usize - 1].anchor.file() != placement.anchor.file());
        let card = if self.is_collapsed(ix, cx) {
            self.collapsed_card(ix, thread, &t, cx)
        } else {
            self.full_card(ix, thread, &t, window, cx)
        };
        let current = self.current == Some(ix);
        let c = &t.colors;
        v_flex()
            .w(width)
            .pt(px(CARD_TOP))
            .pb(px(CARD_BOTTOM))
            .pr(px(12.0))
            .when(caption, |el| {
                el.child(
                    div()
                        .pb(px(6.0))
                        .tiny(&t)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(c.text_muted)
                        .child("Outdated threads: their lines are no longer in this diff"),
                )
            })
            .child(
                card.id(("thread-card", ix))
                    .rounded(t.radius.lg)
                    .border_1()
                    .border_color(if current { c.focus } else { c.border })
                    .bg(c.surface_raised)
                    .overflow_hidden()
                    .font_family(t.fonts.interface.family())
                    .text_size(t.fonts.interface.size_px())
                    .text_color(c.text)
                    .on_click(cx.listener(move |_, _, _, cx| cx.emit(CardsEvent::Selected(ix)))),
            )
            .into_any_element()
    }

    /// A pending-review card or an open line composer.
    fn render_extra(
        &mut self,
        ix: usize,
        width: Pixels,
        window: &mut Window,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(kind) = self.extras.get(ix).map(|extra| extra.kind.clone()) else {
            return div().into_any_element();
        };
        let frame = v_flex()
            .w(width)
            .pt(px(CARD_TOP))
            .pb(px(CARD_BOTTOM))
            .pr(px(12.0))
            .font_family(t.fonts.interface.family())
            .text_size(t.fonts.interface.size_px());
        match kind {
            ExtraKind::Composer(anchor) => {
                let place = anchor_label(&anchor);
                let composer = self.writing.update(cx, |writing, cx| {
                    writing.composer(&anchor, place, window, cx)
                });
                frame.child(composer).into_any_element()
            }
            ExtraKind::Pending(queued) => {
                let Some(queued) = self.writing.read(cx).queued().get(queued).cloned() else {
                    return div().into_any_element();
                };
                frame
                    .child(self.pending_card(&queued, t, window, cx))
                    .into_any_element()
            }
        }
    }

    /// A comment added to the review and not yet submitted: the queued rail
    /// color, a "pending" chip, no replies; Edit and Delete at its foot, or
    /// the composer that edits it.
    fn pending_card(
        &mut self,
        queued: &QueuedComment,
        t: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let c = &t.colors;
        let key = format!("queued-{}", queued.id);
        let state = self.bodies.get(&key, &queued.body, cx);
        let label = anchor_label(&queued.anchor);
        let edit = DraftAnchor::Edit { queued: queued.id };
        let editing = self.writing.read(cx).is_open(&edit);
        let card = div()
            .rounded(t.radius.lg)
            .border_1()
            .border_color(t.chip_colors(c.warn).border)
            .bg(c.surface_raised)
            .overflow_hidden()
            .text_color(c.text)
            .child(
                v_flex()
                    .px(px(10.0))
                    .py(px(ENTRY_PAD_Y))
                    .gap(px(2.0))
                    .child(
                        h_flex()
                            .h(px(HEADER))
                            .gap(px(8.0))
                            .items_center()
                            .child(ui::avatar::letter(t, "you", ui::avatar::CARD))
                            .child(
                                div()
                                    .flex_none()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("You"),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .small(t)
                                    .text_color(c.text_muted)
                                    .children(label),
                            )
                            .child(ui::chip(cx, "pending", c.warn)),
                    )
                    .when(!editing, |el| {
                        el.child(div().pl(px(BODY_INDENT)).child(body(&state, t)))
                    }),
            );
        if editing {
            let composer = self
                .writing
                .update(cx, |writing, cx| writing.composer(&edit, None, window, cx));
            return card.child(div().border_t_1().border_color(c.border).child(composer));
        }
        card.child(self.pending_actions(queued, t, cx))
    }

    fn pending_actions(&self, queued: &QueuedComment, t: &Theme, cx: &mut Context<Self>) -> Div {
        let c = &t.colors;
        let id = queued.id;
        let ix = hash_ix(&format!("queued-{id}"));
        let state = self.writing.read(cx).queued_state(id).cloned();
        let busy = state == Some(ActionState::Busy);
        let (edit, delete) = (self.writing.clone(), self.writing.clone());
        h_flex()
            .px(px(10.0))
            .py(px(4.0))
            .gap(px(4.0))
            .items_center()
            .border_t_1()
            .border_color(c.border)
            .child(
                Button::new(("queued-edit", ix), "Edit", ButtonKind::Ghost).on_click(
                    move |_, _, cx| {
                        cx.stop_propagation();
                        edit.update(cx, |writing, cx| writing.edit_queued(id, cx));
                    },
                ),
            )
            .child(
                Button::new(
                    ("queued-delete", ix),
                    if busy { "Deleting" } else { "Delete" },
                    ButtonKind::Ghost,
                )
                .on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    delete.update(cx, |writing, cx| writing.delete_queued(id, cx));
                }),
            )
            .when_some(failure(state.as_ref()), |row, error| {
                row.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .small(t)
                        .text_color(c.danger)
                        .child(error),
                )
            })
    }

    /// The menu of `thread`, for the pull request these cards are on.
    fn menu(&self, thread: &Thread, cx: &Context<Self>) -> Option<ThreadMenu> {
        let key = self.writing.read(cx).open_pr()?.clone();
        Some(ThreadMenu::new(
            key,
            thread.id.clone(),
            Origin::Diff(cx.weak_entity()),
        ))
    }

    fn row_focus(&mut self, thread: &Thread, cx: &mut Context<Self>) -> FocusHandle {
        self.row_focus
            .entry(thread.id.clone())
            .or_insert_with(|| cx.focus_handle().tab_stop(true))
            .clone()
    }

    /// A click, Enter or Space on a fold row: folds or unfolds the card and selects it.
    fn fold_listener(
        ix: usize,
        cx: &mut Context<Self>,
    ) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
        cx.listener(move |this, _, _, cx| {
            this.toggle(ix, cx);
            // Selected the card without moving the focus off the row, so Enter
            // and Space go on folding it.
            this.set_current(Some(ix), cx);
        })
    }

    /// Resolved threads: one row with the author, the start of the first
    /// comment and the count.
    fn collapsed_card(
        &mut self,
        ix: usize,
        thread: &Thread,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> Div {
        let c = &t.colors;
        let first = thread.comments.first().map(|c| &c.author);
        let author = first.map(|a| a.login.clone()).unwrap_or_default();
        let avatar = first.map(|a| ui::avatar(cx, &open_host(cx), a, ui::avatar::CARD));
        let chevron = chevron(true, t);
        let focus = self.row_focus(thread, cx);
        let menu = self.menu(thread, cx);
        let row = h_flex()
            .id(("thread-fold", ix))
            .debug_selector(move || format!("card-row-{ix}"))
            .h(px(COLLAPSED))
            .pl(px(10.0))
            .pr(px(6.0))
            .gap(px(8.0))
            .items_center();
        let row = fold::toggle_row_focused(row, t, &focus, Self::fold_listener(ix, cx))
            .children(avatar)
            .child(
                div()
                    .flex_none()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(c.text_secondary)
                    .child(author),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(c.text_muted)
                    .child(preview(thread)),
            )
            .when(thread.outdated, |el| {
                el.child(ui::chip(cx, "outdated", c.warn))
            })
            .child(
                div()
                    .flex_none()
                    .small(t)
                    .text_color(c.text_muted)
                    .child(summary(thread)),
            )
            .child(chevron);
        match menu {
            Some(menu) => div().child(row.context_menu(move |popup, _, cx| menu.popup(popup, cx))),
            None => div().child(row),
        }
    }

    fn full_card(
        &mut self,
        ix: usize,
        thread: &Thread,
        t: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let c = &t.colors;
        let pr_page = self.writing.read(cx).open_pr().and_then(|key| {
            let forge = Session::try_global(cx)?.read(cx).pr(key)?.pr.forge;
            Some((forge, key.repo.clone(), key.number))
        });
        let mut chevron = Some(chevron(false, t));
        let focus = self.row_focus(thread, cx);
        let thread_menu = self.menu(thread, cx);
        let mut card = div().flex().flex_col();
        for (k, comment) in thread.comments.iter().enumerate() {
            let state = self.bodies.get(comment.id.as_str(), &comment.body, cx);
            let head = h_flex()
                .h(px(HEADER))
                .gap(px(8.0))
                .items_center()
                .child(ui::avatar(
                    cx,
                    &open_host(cx),
                    &comment.author,
                    ui::avatar::CARD,
                ))
                .child(
                    div()
                        .flex_none()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(comment.author.login.clone()),
                )
                .child(
                    div()
                        .flex_none()
                        .small(t)
                        .text_color(c.text_muted)
                        .child(ago(comment)),
                );
            let head = if k == 0 {
                let head = head
                    .id(("thread-fold", ix))
                    .debug_selector(move || format!("card-head-{ix}"))
                    .mx(px(-4.0))
                    .px(px(4.0))
                    .rounded(t.radius.sm);
                fold::toggle_row_focused(head, t, &focus, Self::fold_listener(ix, cx))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .small(t)
                            .text_color(c.text_muted)
                            .children(line_label(thread)),
                    )
                    .when(thread.outdated, |el| {
                        el.child(ui::chip(cx, "outdated", c.warn))
                    })
                    .when(thread.is_resolved(), |el| {
                        el.child(ui::chip(cx, "resolved", c.ok))
                    })
                    .children(chevron.take())
                    .into_any_element()
            } else {
                head.into_any_element()
            };
            let apply = self.suggestion_row(thread, comment, t, cx);
            // Reply and Resolve come with the thread's own entries.
            let menu = CommentMenu {
                link: pr_page.as_ref().and_then(|(forge, repo, number)| {
                    comment_url(*forge, repo, *number, comment, thread.anchor.is_some())
                }),
                text: comment.body.clone(),
                thread: thread.clone(),
                writing: self.writing.clone(),
                reply: thread_menu.is_none(),
                resolve: thread_menu.is_none() && can_resolve(thread, cx),
            };
            let thread_menu = thread_menu.clone();
            card = card.child(
                v_flex()
                    .id(("comment", hash_ix(comment.id.as_str())))
                    .px(px(10.0))
                    .py(px(ENTRY_PAD_Y))
                    .gap(px(2.0))
                    .when(k > 0, |el| el.border_t_1().border_color(c.border))
                    .child(head)
                    .child(div().pl(px(BODY_INDENT)).child(body(&state, t)))
                    .children(apply)
                    .context_menu(move |popup, _, cx| match &thread_menu {
                        Some(thread_menu) => {
                            thread_menu::with_comment(thread_menu, &menu, popup, cx)
                        }
                        None => menu.popup(popup),
                    }),
            );
        }
        if let Resolution::Resolved { by } = &thread.resolution {
            let text = match by {
                Some(by) => format!("Resolved by {}", by.login),
                None => "Resolved".to_owned(),
            };
            card = card.child(
                h_flex()
                    .px(px(10.0))
                    .py(px(5.0))
                    .gap(px(6.0))
                    .items_center()
                    .border_t_1()
                    .border_color(c.border)
                    .small(t)
                    .text_color(c.text_muted)
                    .child(Icon::new(IconName::Check).size(px(12.0)))
                    .child(text),
            );
        }
        card.child(self.reply_row(thread, t, window, cx))
    }

    fn suggestion_row(
        &self,
        thread: &Thread,
        comment: &Comment,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let pr = self.writing.read(cx).open_pr().cloned()?;
        suggestion_actions(&pr, thread, comment, t, cx)
            .map(|row| row.pl(px(BODY_INDENT)).into_any_element())
    }

    /// The Resolve or Unresolve button of a thread, where the forge lets
    /// the viewer change it.
    fn resolve_button(&self, thread: &Thread, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !can_resolve(thread, cx) {
            return None;
        }
        let resolved = thread.is_resolved();
        let busy = self.writing.read(cx).thread_state(&thread.id) == Some(&ActionState::Busy);
        let label = match (resolved, busy) {
            (false, false) => "Resolve",
            (false, true) => "Resolving",
            (true, false) => "Unresolve",
            (true, true) => "Unresolving",
        };
        let (writing, target) = (self.writing.clone(), thread.clone());
        let ix = ix_of(thread);
        Some(
            div()
                .flex_none()
                .debug_selector(move || format!("resolve-{ix}"))
                .child(
                    Button::new(("resolve", ix), label, ButtonKind::Ghost).on_click(
                        move |_, _, cx| {
                            cx.stop_propagation();
                            writing.update(cx, |writing, cx| {
                                writing.set_resolved(&target, !resolved, cx)
                            });
                        },
                    ),
                )
                .into_any_element(),
        )
    }

    /// The foot of a card: Reply and Resolve, or the reply being written.
    fn reply_row(
        &mut self,
        thread: &Thread,
        t: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = &t.colors;
        let anchor = reply_anchor(thread);
        if self.writing.read(cx).is_open(&anchor) {
            let composer = self.writing.update(cx, |writing, cx| {
                writing.composer(&anchor, None, window, cx)
            });
            return div()
                .border_t_1()
                .border_color(c.border)
                .child(composer)
                .into_any_element();
        }
        let writing = self.writing.clone();
        let resolve = self.resolve_button(thread, cx);
        let failed = failure(self.writing.read(cx).thread_state(&thread.id));
        h_flex()
            .px(px(10.0))
            .py(px(4.0))
            .gap(px(4.0))
            .items_center()
            .border_t_1()
            .border_color(c.border)
            .child(
                Button::new(("reply", ix_of(thread)), "Reply", ButtonKind::Ghost).on_click(
                    move |_, _, cx| {
                        cx.stop_propagation();
                        writing.update(cx, |writing, cx| writing.open(anchor.clone(), None, cx));
                    },
                ),
            )
            .children(resolve)
            .when_some(failed, |row, error| {
                row.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .small(t)
                        .text_color(c.danger)
                        .child(error),
                )
            })
            .into_any_element()
    }
}

/// "Apply suggestion" and "Add to batch" under a comment whose suggestion
/// can be applied on pull request `pr`, or why it cannot; nothing when the
/// comment has none.
pub fn suggestion_actions(
    pr: &crate::model::PrKey,
    thread: &Thread,
    comment: &Comment,
    t: &Theme,
    cx: &App,
) -> Option<Div> {
    if !suggestions::mentions(&comment.body) && comment.suggestions.is_empty() {
        return None;
    }
    let session = Session::try_global(cx)?;
    let key = SuggestionKey {
        thread: thread.id.clone(),
        comment: comment.id.clone(),
    };
    let offer = session.read(cx).suggestion(pr, &key)?;
    let notice = match &offer {
        Offer::Html => Some(("suggestion contains HTML, apply it on the forge", false)),
        Offer::Refused(why) => Some((why.reason(), true)),
        Offer::Apply(_) => None,
    };
    if let Some((text, refused)) = notice {
        let color = if refused {
            t.colors.danger
        } else {
            t.colors.text_muted
        };
        return Some(div().pt(px(4.0)).small(t).text_color(color).child(text));
    }
    let batched = session
        .read(cx)
        .pr(pr)
        .is_some_and(|open| open.batch.contains(&key));
    let ix = hash_ix(comment.id.as_str());
    let (apply, batch) = (session.clone(), session);
    let (apply_key, batch_key) = (key.clone(), key);
    let (apply_pr, batch_pr) = (pr.clone(), pr.clone());
    Some(
        h_flex()
            .pt(px(4.0))
            .gap(px(6.0))
            .child(
                Button::new(
                    ("apply-suggestion", ix),
                    "Apply suggestion",
                    ButtonKind::Secondary,
                )
                .on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    let key = apply_key.clone();
                    apply.update(cx, |session, cx| {
                        session.request_apply(&apply_pr, &[key], cx)
                    });
                }),
            )
            .child(
                Button::new(
                    ("batch-suggestion", ix),
                    if batched {
                        "Remove from batch"
                    } else {
                        "Add to batch"
                    },
                    ButtonKind::Ghost,
                )
                .on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    let key = batch_key.clone();
                    batch.update(cx, |session, cx| session.toggle_batch(&batch_pr, key, cx));
                }),
            )
            .when(batched, |row| {
                row.child(ui::chip(cx, "in batch", t.colors.accent_fg))
            }),
    )
}

/// The message of a failed action.
fn failure(state: Option<&ActionState>) -> Option<SharedString> {
    match state {
        Some(ActionState::Failed(message)) => Some(message.clone()),
        Some(ActionState::Busy) | None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge::model::{CommentId, LineRef, Position, User};

    fn thread(side: forge::model::Side, start: Option<u32>, line: u32) -> Thread {
        Thread {
            id: ThreadId::new("t"),
            anchor: Some(forge::model::Anchor {
                path: "src/a.rs".into(),
                old_path: None,
                position: Some(Position {
                    line: LineRef { side, line },
                    start: start.map(|line| LineRef { side, line }),
                    commit: None,
                }),
                original: None,
            }),
            resolution: Resolution::Resolved {
                by: Some(User::new("wen")),
            },
            outdated: false,
            comments: vec![Comment {
                id: CommentId::new("c"),
                author: User::new("mira"),
                body: "Prefer  `% 2`\nhere.".into(),
                created_at: Utc::now(),
                edited_at: None,
                web_url: None,
                review: None,
                verdict: None,
                suggestions: Vec::new(),
            }],
        }
    }

    #[test]
    fn labels_name_the_side_and_the_range() {
        use forge::model::Side::{New, Old};
        assert_eq!(line_label(&thread(New, None, 8)).as_deref(), Some("line 8"));
        // GitHub repeats the line as the start of a single-line comment.
        assert_eq!(
            line_label(&thread(New, Some(8), 8)).as_deref(),
            Some("line 8")
        );
        assert_eq!(
            line_label(&thread(New, Some(29), 31)).as_deref(),
            Some("lines 29\u{2013}31")
        );
        assert_eq!(
            line_label(&thread(Old, None, 8)).as_deref(),
            Some("old line 8")
        );
        assert_eq!(location(&thread(New, Some(29), 31)), "src/a.rs:31");
    }

    #[test]
    fn collapsed_rows_summarize_and_flatten() {
        let thread = thread(forge::model::Side::New, None, 3);
        assert_eq!(summary(&thread), "1 comment, resolved by wen");
        assert_eq!(preview(&thread), "Prefer `% 2` here.");
    }

    #[test]
    fn a_draft_on_revealed_context_sits_under_its_line() {
        let text = diff::Text::new((0..60).map(|i| format!("l{i}\n")).collect::<String>());
        let file = diff::FileDiff {
            path: "src/a.rs".into(),
            old: text.clone(),
            new: text,
            hunks: vec![diff::Hunk {
                old_start: 40,
                new_start: 40,
                segments: vec![diff::Segment::Context(3)],
            }],
        };
        let diff = diff::Diff {
            files: vec![file],
            blocks: Vec::new(),
        };
        // New line 30 (1-based 31) is in the hidden lines above the hunk.
        let anchor = DraftAnchor::Line {
            path: "src/a.rs".into(),
            line: LineRef {
                side: forge::model::Side::New,
                line: 31,
            },
            start: None,
        };
        let top = spot_of(&anchor, &diff, &Revealed::new()).expect("a spot");
        assert_eq!(top.anchor, Anchor::FileTop { file: FileIx(0) });
        let mut reveals = Reveals::default();
        reveals.expand(&diff.files[0], 0, diff::Expand::Up);
        let revealed = Revealed::from([("src/a.rs".to_owned(), reveals)]);
        let under = spot_of(&anchor, &diff, &revealed).expect("a spot");
        assert_eq!(
            under.anchor,
            Anchor::Line {
                file: FileIx(0),
                side: Side::New,
                line: 30
            }
        );
    }
}
