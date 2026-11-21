//! Find in the conversation (`Ctrl+F`): the bar at the top right, a search of
//! the text the description, the comments and the threads render, its
//! matches highlighted in their text views, and the scroll to each one. The
//! comments of a folded thread are searched too; the thread unfolds when one
//! of its matches becomes the current one.

use std::ops::Range;

use diff_view::{MAX_MATCHES, Step};
use forge::model::{Comment, ThreadId};
use gpui::{
    AnyElement, App, AppContext as _, Bounds, Context, Entity, IntoElement, ParentElement as _,
    Pixels, ScrollHandle, SharedString, Styled as _, Subscription, Window, div, point, px,
};
use gpui_kit::component::text::{RangeHighlight, RenderedText, TextViewState};

use super::ConversationPanel;
use crate::find::{self, Count, FindBar, FindBarEvent, OpenFind};
use crate::suggestions::{self, BodyPart};
use crate::ui;

/// The bar's distance from the top and right of the conversation.
const BAR_TOP: f32 = 8.0;
const BAR_RIGHT: f32 = 20.0;

/// A text the find searches: a body drawn, or a folded thread's comment.
pub(super) struct Source {
    state: Entity<TextViewState>,
    /// The folded thread it is in, which unfolds to show a match.
    folded: Option<ThreadId>,
}

/// The conversation's find bar, made on first use, and what it found.
#[derive(Default)]
pub(super) struct ConversationFind {
    bar: Option<Entity<FindBar>>,
    open: bool,
    /// The texts of the last render, in the order they are drawn.
    sources: Vec<Source>,
    /// What the last search read: the query, its case and each text.
    searched: Option<(SharedString, bool, Vec<RenderedText>)>,
    /// Each match: its source and its byte range in the source's text.
    matches: Vec<(usize, Range<usize>)>,
    current: Option<usize>,
    capped: bool,
    /// The text views given highlights, to clear when they no longer match.
    painted: Vec<Entity<TextViewState>>,
    _bar: Option<Subscription>,
}

impl ConversationFind {
    /// Starts a render's list of texts.
    pub(super) fn begin(&mut self) {
        self.sources.clear();
    }

    /// A body the render draws.
    pub(super) fn drawn(&mut self, state: &Entity<TextViewState>) {
        self.sources.push(Source {
            state: state.clone(),
            folded: None,
        });
    }
}

/// Scrolls `scroll` so `line` (in window coordinates) sits a third of the
/// way down its view, as far as it goes; for a match a text view reveals.
pub(super) fn reveal_line(scroll: &ScrollHandle, line: Bounds<Pixels>, window: &mut Window) {
    let view = scroll.bounds();
    if view.size.height <= px(0.0) {
        return;
    }
    let offset = scroll.offset();
    let y = (offset.y - (line.top() - view.top()) + view.size.height / 3.0)
        .min(px(0.0))
        .max(-scroll.max_offset().y);
    if (y - offset.y).abs() > px(0.5) {
        scroll.set_offset(point(offset.x, y));
        window.refresh();
    }
}

impl ConversationPanel {
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
        let selected = self
            .find
            .sources
            .iter()
            .map(|source| source.state.read(cx).selected_text())
            .find(|text| !text.is_empty());
        let prefill = find::prefill(selected);
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
    /// conversation.
    fn close_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.find.open = false;
        self.find.searched = None;
        self.find.matches.clear();
        self.find.current = None;
        for state in std::mem::take(&mut self.find.painted) {
            state.update(cx, |state, cx| state.clear_range_highlights(cx));
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
        let Some(ix) = find::stepped(self.find.current, self.find.matches.len(), step) else {
            return;
        };
        self.find.current = Some(ix);
        self.paint_matches(cx);
        self.reveal_match(cx);
        cx.notify();
    }

    /// Adds a folded thread's comments to the texts searched, while the bar
    /// is open.
    pub(super) fn find_in_folded(&mut self, thread: &ThreadId, comments: &[Comment], cx: &mut App) {
        if !self.find.open {
            return;
        }
        for comment in comments {
            for state in self.comment_states(comment, cx) {
                self.find.sources.push(Source {
                    state,
                    folded: Some(thread.clone()),
                });
            }
        }
    }

    /// The text views of a comment's markdown, as [`Self::comment_parts`]
    /// makes them.
    fn comment_states(&mut self, comment: &Comment, cx: &mut App) -> Vec<Entity<TextViewState>> {
        if !suggestions::mentions(&comment.body) {
            return vec![self.bodies.get(comment.id.as_str(), &comment.body, cx)];
        }
        suggestions::split_body(&comment.body)
            .into_iter()
            .enumerate()
            .filter_map(|(k, part)| match part {
                BodyPart::Markdown(text) => {
                    Some(self.bodies.get(&format!("{}#{k}", comment.id), text, cx))
                }
                BodyPart::Suggestion(_) => None,
            })
            .collect()
    }

    /// Searches again when the query or a text changed since the last
    /// search. A new query's first match becomes the current one and is
    /// scrolled to; changed texts keep the current match's place.
    pub(super) fn sync_find(&mut self, cx: &mut Context<Self>) {
        if !self.find.open {
            return;
        }
        let Some(bar) = self.find.bar.clone() else {
            return;
        };
        let (text, match_case, needle) = {
            let bar = bar.read(cx);
            (bar.text(cx), bar.match_case(), bar.needle(cx))
        };
        let texts: Vec<RenderedText> = self
            .find
            .sources
            .iter()
            .map(|source| source.state.read(cx).rendered_text())
            .collect();
        let new_query = self
            .find
            .searched
            .as_ref()
            .is_none_or(|(before, case, _)| *before != text || *case != match_case);
        if !new_query
            && self
                .find
                .searched
                .as_ref()
                .is_some_and(|(_, _, before)| *before == texts)
        {
            return;
        }
        let mut matches = Vec::new();
        let mut capped = false;
        'texts: for (ix, rendered) in texts.iter().enumerate() {
            needle.find_in(rendered.as_str(), |range| {
                if matches.len() == MAX_MATCHES {
                    capped = true;
                    return false;
                }
                matches.push((ix, range));
                true
            });
            if capped {
                break 'texts;
            }
        }
        self.find.current = if matches.is_empty() {
            None
        } else if new_query {
            Some(0)
        } else {
            self.find.current.map(|ix| ix.min(matches.len() - 1))
        };
        self.find.matches = matches;
        self.find.capped = capped;
        self.find.searched = Some((text, match_case, texts));
        self.paint_matches(cx);
        if new_query {
            self.reveal_match(cx);
        }
    }

    /// Gives each text view its matches, the current one stronger, and the
    /// bar its count.
    fn paint_matches(&mut self, cx: &mut Context<Self>) {
        let t = ui::theme(cx);
        let mut by_source: Vec<Vec<RangeHighlight>> = Vec::new();
        by_source.resize_with(self.find.sources.len(), Vec::new);
        for (ix, (source, range)) in self.find.matches.iter().enumerate() {
            let color = if self.find.current == Some(ix) {
                t.colors.find_current
            } else {
                t.colors.find_match
            };
            if let Some(list) = by_source.get_mut(*source) {
                list.push(RangeHighlight::new(range.clone(), color));
            }
        }
        let mut painted = Vec::new();
        for (source, highlights) in self.find.sources.iter().zip(by_source) {
            if highlights.is_empty() {
                continue;
            }
            painted.push(source.state.clone());
            source.state.update(cx, |state, cx| {
                // Ranges from this text's own search are always valid.
                let _ = state.set_range_highlights(highlights, cx);
            });
        }
        for state in std::mem::take(&mut self.find.painted) {
            if !painted.contains(&state) {
                state.update(cx, |state, cx| state.clear_range_highlights(cx));
            }
        }
        self.find.painted = painted;
        let count = Count {
            current: self.find.current,
            total: self.find.matches.len(),
            capped: self.find.capped,
        };
        if let Some(bar) = &self.find.bar {
            bar.update(cx, |bar, cx| bar.set_count(count, cx));
        }
    }

    /// Unfolds the thread the current match is in, if folded, and asks its
    /// text view to bring the match into view.
    fn reveal_match(&mut self, cx: &mut Context<Self>) {
        let Some((source, range)) = self
            .find
            .current
            .and_then(|ix| self.find.matches.get(ix).cloned())
        else {
            return;
        };
        let Some(source) = self.find.sources.get(source) else {
            return;
        };
        if let Some(thread) = &source.folded
            && let Some(key) = self.binding.key(cx)
        {
            self.nav
                .update(cx, |nav, cx| nav.set_folded(&key, thread, false, cx));
        }
        source.state.update(cx, |state, cx| {
            // The range came from this text: it is valid.
            let _ = state.reveal_range(range, cx);
        });
    }

    /// The bar at the top right of the conversation, while it is open.
    pub(super) fn find_overlay(&self) -> Option<AnyElement> {
        let bar = self.find.bar.clone().filter(|_| self.find.open)?;
        Some(
            div()
                .absolute()
                .top(px(BAR_TOP))
                .right(px(BAR_RIGHT))
                .child(bar)
                .into_any_element(),
        )
    }
}
