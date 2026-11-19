//! Back and forward, as in a browser: where the user was before each jump
//! to a thread, a file or another pull request's tab.

use gpui::{App, Context, SharedString, Window};
use gpui_kit::component::dock::{DockPlacement, PaneRef, PanelId};

use crate::model::PrKey;
use crate::panels::{ConversationPanel, DiffPanel, PanelKind};
use crate::session::{OpenMode, SessionEvent};
use crate::tab_menu;

use super::Shell;

/// How many places Back can return through.
const LIMIT: usize = 100;

/// A tab and where in it the user was.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Place {
    panel: PanelId,
    /// The pull request and kind of a pull request's tab, to open it again
    /// once it was closed.
    pr: Option<(PrKey, PanelKind)>,
    spot: Spot,
}

/// Where in its tab a place is.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Spot {
    /// A tab with no position kept: a terminal, the checks.
    Tab,
    /// How far the conversation was scrolled down, in pixels.
    Conversation(f32),
    /// The file shown alone, if one was, and how far the diff was scrolled.
    Diff {
        file: Option<SharedString>,
        scrolled: f64,
    },
}

/// The places behind the one shown and ahead of it.
pub(super) struct History<T> {
    back: Vec<T>,
    forward: Vec<T>,
}

impl<T> Default for History<T> {
    fn default() -> Self {
        Self {
            back: Vec::new(),
            forward: Vec::new(),
        }
    }
}

impl<T: PartialEq> History<T> {
    /// A jump away from `here`: Back returns to it, and the places ahead
    /// are dropped.
    pub(super) fn record(&mut self, here: T) {
        self.forward.clear();
        if self.back.last() == Some(&here) {
            return;
        }
        self.back.push(here);
        if self.back.len() > LIMIT {
            self.back.remove(0);
        }
    }

    /// The place behind `here`, which Forward then returns to.
    pub(super) fn back(&mut self, here: Option<T>) -> Option<T> {
        let to = self.back.pop()?;
        self.forward.extend(here);
        Some(to)
    }

    /// The place ahead of `here`, which Back then returns to.
    pub(super) fn forward(&mut self, here: Option<T>) -> Option<T> {
        let to = self.forward.pop()?;
        self.back.extend(here);
        Some(to)
    }

    pub(super) fn can_back(&self) -> bool {
        !self.back.is_empty()
    }

    pub(super) fn can_forward(&self) -> bool {
        !self.forward.is_empty()
    }
}

/// Which way to go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Direction {
    Back,
    Forward,
}

impl Shell {
    /// The tab shown in the main area's group with focus, else in the group
    /// of the tab shown last.
    fn shown_tab(&self, window: &Window, cx: &App) -> Option<PanelId> {
        let dock = self.dock.read(cx);
        let in_center = |id: &PanelId| {
            tab_menu::place_of(dock, *id).is_some_and(|place| place.region == DockPlacement::Center)
        };
        let panel = self
            .focused_panel(window, cx)
            .filter(in_center)
            .or(self.last_shown.filter(in_center))?;
        let node = tab_menu::place_of(dock, panel)?.node;
        let PaneRef::Tabs { panels, active_ix } =
            dock.layout(DockPlacement::Center)?.find_node(node)?.kind()
        else {
            return None;
        };
        panels.get(active_ix).copied()
    }

    /// Where the user is now.
    pub(super) fn here(&self, window: &Window, cx: &App) -> Option<Place> {
        let panel = self.shown_tab(window, cx)?;
        let session = self.session.read(cx);
        let pr = session
            .panels()
            .iter()
            .find(|entry| PanelId::from(entry.id) == panel)
            .and_then(|entry| Some((entry.pr.clone()?, entry.kind)));
        let view = self.dock.read(cx).panel(panel)?.view();
        let spot = if let Ok(conversation) = view.clone().downcast::<ConversationPanel>() {
            Spot::Conversation(conversation.read(cx).scrolled())
        } else if let Ok(diff) = view.downcast::<DiffPanel>() {
            let diff = diff.read(cx);
            let file = diff
                .pr(cx)
                .and_then(|key| session.pr(&key)?.selected_file.clone());
            let scrolled = diff.view().map_or(0.0, |view| view.read(cx).scroll_y());
            Spot::Diff { file, scrolled }
        } else {
            Spot::Tab
        };
        Some(Place { panel, pr, spot })
    }

    /// Records where the user was before `event` moved them, now that it
    /// has: a jump always, an open only when it brought another tab forward.
    pub(super) fn record_move(
        &mut self,
        before: Option<Place>,
        event: &SessionEvent,
        window: &Window,
        cx: &App,
    ) {
        let Some(mut before) = before else {
            return;
        };
        let moved = match event {
            SessionEvent::JumpToThread { .. } => true,
            // The file was picked before the event was heard: the diff of
            // its pull request showed the one before it.
            SessionEvent::FileSelected { pr, previous, .. } => {
                if let (Some((key, PanelKind::Diff)), Spot::Diff { file, .. }) =
                    (&before.pr, &mut before.spot)
                    && key == pr
                {
                    file.clone_from(previous);
                }
                true
            }
            SessionEvent::Show(show) => {
                show.pr.is_some() && self.shown_tab(window, cx) != Some(before.panel)
            }
            SessionEvent::ClosePanel(_) => false,
        };
        if moved {
            self.history.record(before);
        }
    }

    /// `Alt+Left` and `Alt+Right`, and the mouse's back and forward buttons.
    /// A closed pull request tab opens again; other closed tabs are skipped.
    pub(super) fn go(&mut self, direction: Direction, window: &mut Window, cx: &mut Context<Self>) {
        let mut here = self.here(window, cx);
        loop {
            let to = match direction {
                Direction::Back => self.history.back(here.take()),
                Direction::Forward => self.history.forward(here.take()),
            };
            let Some(to) = to else {
                return;
            };
            if self.return_to(to, window, cx) {
                cx.notify();
                return;
            }
        }
    }

    /// Whether Back has somewhere to go.
    pub fn can_go_back(&self) -> bool {
        self.history.can_back()
    }

    /// Whether Forward has somewhere to go.
    pub fn can_go_forward(&self) -> bool {
        self.history.can_forward()
    }

    /// Brings `place` forward and puts its tab where it was; false when its
    /// tab was closed and cannot open again.
    fn return_to(&mut self, place: Place, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.dock.read(cx).panel(place.panel).is_none() {
            let Some((key, kind)) = place.pr else {
                return false;
            };
            // Put back once the shell has built the tab, unrecorded.
            self.reopening = Some(place.spot);
            self.session.update(cx, |session, cx| {
                session.show(&key, kind, OpenMode::Preview, cx)
            });
            return true;
        };
        if let Some((key, _)) = &place.pr {
            self.session
                .update(cx, |session, cx| session.focus(key, cx));
        }
        self.dock
            .update(cx, |dock, cx| dock.select_panel(place.panel, window, cx));
        if let Some(view) = self.dock.read(cx).panel(place.panel) {
            window.focus(&view.focus_handle(cx), cx);
        }
        self.last_shown = Some(place.panel);
        if place.pr.is_some() {
            self.last_pr_panel = Some(place.panel);
        }
        self.put_back(place.panel, place.spot, cx);
        true
    }

    /// Scrolls `panel` back to `spot`.
    pub(super) fn put_back(&self, panel: PanelId, spot: Spot, cx: &mut App) {
        let Some(view) = self.dock.read(cx).panel(panel).map(|panel| panel.view()) else {
            return;
        };
        match spot {
            Spot::Tab => {}
            Spot::Conversation(scrolled) => {
                if let Ok(conversation) = view.downcast::<ConversationPanel>() {
                    conversation.update(cx, |panel, cx| panel.scroll_to(scrolled, cx));
                }
            }
            Spot::Diff { file, scrolled } => {
                if let Ok(diff) = view.downcast::<DiffPanel>() {
                    diff.update(cx, |panel, cx| panel.return_to(file, scrolled, cx));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{History, LIMIT};

    #[test]
    fn back_and_forward_retrace_the_jumps() {
        let mut history = History::default();
        history.record(1);
        history.record(2);
        assert_eq!(history.back(Some(3)), Some(2));
        assert_eq!(history.back(Some(2)), Some(1));
        assert_eq!(history.back(Some(1)), None, "nothing further back");
        assert_eq!(history.forward(Some(1)), Some(2));
        assert_eq!(history.forward(Some(2)), Some(3));
        assert!(!history.can_forward());
        assert!(history.can_back());
    }

    #[test]
    fn a_jump_after_going_back_drops_the_places_ahead() {
        let mut history = History::default();
        history.record(1);
        assert_eq!(history.back(Some(2)), Some(1));
        assert!(history.can_forward());
        history.record(1);
        assert!(!history.can_forward());
        assert_eq!(history.back(Some(4)), Some(1), "the same place once");
        assert!(!history.can_back());
    }

    #[test]
    fn only_the_latest_places_are_kept() {
        let mut history = History::default();
        for place in 0..LIMIT + 10 {
            history.record(place);
        }
        let mut count = 0;
        let mut oldest = None;
        while let Some(place) = history.back(None) {
            count += 1;
            oldest = Some(place);
        }
        assert_eq!(count, LIMIT);
        assert_eq!(oldest, Some(10));
    }
}
