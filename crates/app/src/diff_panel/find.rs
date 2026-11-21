//! Find in the diff (`Ctrl+F`): the bar at the top right, a search off the UI
//! thread over the lines and paths the diff shows, and the jump to each
//! match. A match in a collapsed file is found too; the file opens when the
//! match becomes the current one.

use std::sync::Arc;

use diff::Layout;
use diff_view::{Corpus, DiffView, FindMatch, Found, Step};
use gpui::{
    AnyElement, AppContext as _, Context, Entity, EntityId, IntoElement, ParentElement as _,
    SharedString, Styled as _, Subscription, Task, Window, div, px,
};

use super::DiffPanel;
use crate::find::{self, Count, FindBar, FindBarEvent, OpenFind};

/// The space between the bar and the pinned file header above it.
const BAR_GAP: f32 = 6.0;
/// The bar's distance from the view's right edge, clear of the scrollbar.
const BAR_RIGHT: f32 = 20.0;

/// A diff panel's find bar, made on first use, and its search.
#[derive(Default)]
pub(super) struct DiffFind {
    bar: Option<Entity<FindBar>>,
    open: bool,
    /// What the last search ran on: a change searches again.
    searched: Option<Stamp>,
    /// The last search stopped at its limit.
    capped: bool,
    task: Option<Task<()>>,
    _bar: Option<Subscription>,
}

/// What a search's result depends on.
#[derive(Clone, PartialEq, Eq)]
struct Stamp {
    view: EntityId,
    layout: Layout,
    /// Rows come and go as context is revealed, files fold or blocks open.
    rows: usize,
    text: SharedString,
    match_case: bool,
}

impl Stamp {
    /// Whether `self` asks for other matches than `before`, not just the same
    /// ones on other rows.
    fn is_new_query(&self, before: Option<&Self>) -> bool {
        before.is_none_or(|before| {
            (before.view, &before.text, before.match_case)
                != (self.view, &self.text, self.match_case)
        })
    }
}

impl DiffPanel {
    /// Whether the find bar is open.
    pub fn is_find_open(&self) -> bool {
        self.find.open
    }

    /// The find bar, once it has been opened.
    pub fn find_bar(&self) -> Option<&Entity<FindBar>> {
        self.find.bar.as_ref()
    }

    /// `Ctrl+F`: opens the bar with the cursor in it, the selected text (one
    /// line of it) as its query when there is one.
    pub(super) fn open_find(&mut self, _: &OpenFind, window: &mut Window, cx: &mut Context<Self>) {
        let prefill = find::prefill(self.view().and_then(|view| view.read(cx).copy_selection()));
        let bar = match &self.find.bar {
            Some(bar) => bar.clone(),
            None => {
                let bar = cx.new(|cx| FindBar::new(window, cx));
                self.find._bar = Some(cx.subscribe_in(&bar, window, Self::find_event));
                self.find.bar = Some(bar.clone());
                bar
            }
        };
        if !self.find.open {
            self.find.open = true;
            self.find.searched = None;
        }
        bar.update(cx, |bar, cx| bar.focus(prefill, window, cx));
        cx.notify();
    }

    fn find_event(
        &mut self,
        _: &Entity<FindBar>,
        event: &FindBarEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match *event {
            FindBarEvent::Changed => cx.notify(),
            FindBarEvent::Step(step) => self.step_find(step, cx),
            FindBarEvent::Close => self.close_find(window, cx),
        }
    }

    /// Closes the bar, clears the highlights and gives the keys back to the
    /// diff.
    fn close_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.find.open = false;
        self.find.searched = None;
        self.find.task = None;
        if let Some(view) = self.view().cloned() {
            view.update(cx, DiffView::clear_find);
        }
        if let Some(bar) = &self.find.bar {
            bar.update(cx, |bar, cx| bar.set_count(Count::default(), cx));
        }
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// The next or previous match, wrapping, scrolled into view.
    pub(super) fn step_find(&mut self, step: Step, cx: &mut Context<Self>) {
        if !self.find.open {
            return;
        }
        let Some(view) = self.view().cloned() else {
            return;
        };
        view.update(cx, |view, cx| {
            let total = view.find_matches().len();
            if let Some(ix) = find::stepped(view.current_find(), total, step) {
                view.reveal_find(ix, cx);
            }
        });
        self.show_count(cx);
    }

    /// Searches again when the query or the rows changed since the last
    /// search; the matches land when it is done.
    pub(super) fn sync_find(&mut self, cx: &mut Context<Self>) {
        if !self.find.open {
            return;
        }
        let (Some(bar), Some(view)) = (self.find.bar.clone(), self.view().cloned()) else {
            return;
        };
        let (text, match_case, needle) = {
            let bar = bar.read(cx);
            (bar.text(cx), bar.match_case(), bar.needle(cx))
        };
        let list = view.read(cx).list();
        let stamp = Stamp {
            view: view.entity_id(),
            layout: list.layout(),
            rows: list.len(),
            text,
            match_case,
        };
        if self.find.searched.as_ref() == Some(&stamp) {
            return;
        }
        let new_query = stamp.is_new_query(self.find.searched.as_ref());
        self.find.searched = Some(stamp);
        let (corpus, previous, top) = {
            let view = view.read(cx);
            let previous = view
                .current_find()
                .and_then(|ix| view.find_matches().get(ix).cloned());
            (Corpus::of(view.list()), previous, view.rows_in_view().start)
        };
        let search = cx.background_spawn(async move { corpus.search(&needle) });
        self.find.task = Some(cx.spawn(async move |this, cx| {
            let found = search.await;
            this.update(cx, |this, cx| {
                this.show_found(found, new_query, previous, top, cx);
            })
            .ok();
        }));
    }

    /// Paints what a search found. A new query's current match is the first
    /// at or after the top of the view, scrolled to; the same query on
    /// changed rows keeps the current match.
    fn show_found(
        &mut self,
        found: Found,
        new_query: bool,
        previous: Option<FindMatch>,
        top: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.view().cloned().filter(|_| self.find.open) else {
            return;
        };
        self.find.capped = found.capped;
        let matches: Arc<[FindMatch]> = found.matches.into();
        view.update(cx, |view, cx| {
            let current = if new_query {
                first_from(view, &matches, top)
            } else {
                previous.and_then(|previous| matches.iter().position(|m| *m == previous))
            };
            view.set_find_matches(matches.clone(), current, cx);
            if new_query && let Some(ix) = current {
                view.reveal_find(ix, cx);
            }
        });
        self.show_count(cx);
    }

    fn show_count(&mut self, cx: &mut Context<Self>) {
        let (Some(bar), Some(view)) = (&self.find.bar, self.view()) else {
            return;
        };
        let view = view.read(cx);
        let count = Count {
            current: view.current_find(),
            total: view.find_matches().len(),
            capped: self.find.capped,
        };
        bar.update(cx, |bar, cx| bar.set_count(count, cx));
    }

    /// The bar at the top right of the diff, under the pinned file header,
    /// while it is open.
    pub(super) fn find_overlay(
        &self,
        view: &Entity<DiffView>,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let bar = self.find.bar.clone().filter(|_| self.find.open)?;
        let header = view.read(cx).list().heights().file_header;
        Some(
            div()
                .absolute()
                .top(px(header + BAR_GAP))
                .right(px(BAR_RIGHT))
                .child(bar)
                .into_any_element(),
        )
    }
}

/// The first match at or after `top`, in the order the diff shows them, or
/// the first of all when none is; `None` when there are none.
fn first_from(view: &DiffView, matches: &[FindMatch], top: usize) -> Option<usize> {
    if matches.is_empty() {
        return None;
    }
    let list = view.list();
    if top >= list.len() {
        return Some(0);
    }
    let file = list.file_of_row(top);
    let start = matches.partition_point(|m| m.file < file);
    let after = matches[start..]
        .iter()
        .position(|m| m.file != file || view.find_row(m).is_some_and(|row| row >= top));
    Some(after.map_or(0, |ix| start + ix))
}
