//! What the user is writing on the open pull request: open composers, the
//! pending review's queued comments, the review summary and its verdict,
//! and the thread actions (resolve, delete or edit a queued comment).
//!
//! Every edit goes to the store through [`DraftSaver`]; a draft is deleted
//! only after the [`ReviewSink`] reports the write succeeded, so a failed
//! send or a crash never costs the text. Opening a pull request restores its
//! drafts as open composers and its queued comments as pending cards, and
//! adds the comments of a pending review the forge already has. What a write
//! returns goes out as [`WritingEvent::Threads`] for the session to patch
//! its threads with.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::Utc;
use forge::model::{CommentId, PendingComment, PendingReview, Sha, Thread, ThreadId};
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, SharedString, Task, Window};
use store::{Draft, DraftAnchor, NewQueued, PrKey, QueuedComment, Store};

use crate::composer::{Composer, ComposerEvent, ComposerInit, SubmitAction};
use crate::drafts::DraftSaver;
use crate::session::{CommentKind, LocalComment};
use crate::sink::{
    LogSink, NewComment, Place, ReviewCommand, ReviewSink, SinkError, SinkOutcome, ThreadChange,
    Verdict, draft_anchor, pending_place,
};

/// One thing being written: an open composer, or a draft kept after its
/// composer was closed.
pub struct Slot {
    pub anchor: DraftAnchor,
    /// The newest text.
    pub body: String,
    /// Whether its composer is showing.
    pub open: bool,
    pub composer: Option<Entity<Composer>>,
    /// Take keyboard focus once the composer exists.
    focus: bool,
    /// The lines "Suggest change" starts from.
    suggestion: Option<String>,
    busy: bool,
}

impl Slot {
    /// Whether the composer takes the focus when it is next drawn: it was
    /// opened and has not been shown since.
    #[must_use]
    pub fn wants_focus(&self) -> bool {
        self.open && self.focus
    }
}

/// How the drafts with text stand against the pending review.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DraftTally {
    /// Line and file drafts "Add to review" takes now.
    pub queueable: usize,
    /// Line and file drafts on their way to the review.
    pub adding: usize,
    /// Replies, edits and the conversation's comment, sent on their own.
    pub other: usize,
}

/// Where a thread or queued-comment action stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActionState {
    Busy,
    Failed(SharedString),
}

/// Something happened that a view or the session reacts to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WritingEvent {
    /// A composer was opened by the user.
    Opened(DraftAnchor),
    /// A write changed the pull request's threads.
    Threads(ThreadChange),
    /// The review went out.
    ReviewSubmitted,
}

impl EventEmitter<WritingEvent> for Writing {}

/// One comment on its way to the sink.
struct Write {
    pr: PrKey,
    anchor: DraftAnchor,
    body: String,
    commit: Option<Sha>,
}

pub struct Writing {
    sink: Arc<dyn ReviewSink>,
    store: Option<Store>,
    saver: Option<DraftSaver>,
    pr: Option<PrKey>,
    commit: Option<Sha>,
    slots: Vec<Slot>,
    queued: Vec<QueuedComment>,
    /// Comments sent in this session, shown until the forge returns them
    /// (only for a sink that returns nothing).
    sent: Vec<LocalComment>,
    review_body: String,
    /// Changes when the summary was replaced from outside the summary box.
    review_epoch: u64,
    verdict: Verdict,
    submitting: bool,
    review_error: Option<SharedString>,
    /// Resolve and unresolve in flight or failed, by thread.
    threads: HashMap<ThreadId, ActionState>,
    /// Deletes in flight or failed, by queued comment id.
    queued_actions: HashMap<i64, ActionState>,
    /// Counts opened pull requests, so a late load of an earlier one is
    /// dropped.
    generation: u64,
    load_started: bool,
    next_local_id: i64,
    /// Text a new conversation composer starts with, for screenshots.
    seed_conversation: Option<String>,
    /// Drafts Claude wrote, labeled "from Claude" on their cards until they
    /// are sent or discarded; [`DraftAnchor::Review`] while the summary is
    /// Claude's, unedited.
    from_claude: HashSet<DraftAnchor>,
    /// Claude's newest suggested verdict and summary, until the review is
    /// submitted or the suggestion discarded.
    suggestion: Option<ReviewSuggestion>,
    verdict_by: VerdictBy,
    _load: Option<Task<()>>,
}

/// What a draft Claude wrote says on its card, after its place.
pub const FROM_CLAUDE: &str = "from Claude";

/// A verdict and summary Claude suggested for the review. The summary is
/// Markdown from an agent that read the pull request: it is only ever text
/// in the summary box, never acted on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewSuggestion {
    pub verdict: Verdict,
    pub body: String,
}

/// Who chose the review's verdict.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum VerdictBy {
    #[default]
    Nobody,
    User,
    Claude,
}

/// What a suggestion did to the Submit review form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Suggested {
    /// The form held nothing of the user's: it now holds the suggestion.
    Filled,
    /// The user's own summary or verdict is kept; the suggestion is offered
    /// beside it.
    Offered,
}

/// Where Claude's suggestion stands in the Submit review form.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SuggestionState {
    pub suggestion: Option<ReviewSuggestion>,
    /// The form's verdict is Claude's and unchanged since.
    pub verdict_from_claude: bool,
    /// The form's summary is Claude's and unedited since.
    pub summary_from_claude: bool,
    /// The form does not hold the suggestion, which waits to be used.
    pub offered: bool,
}

impl Writing {
    pub fn new(sink: Arc<dyn ReviewSink>) -> Self {
        Self {
            sink,
            store: None,
            saver: None,
            pr: None,
            commit: None,
            slots: Vec::new(),
            queued: Vec::new(),
            sent: Vec::new(),
            review_body: String::new(),
            review_epoch: 0,
            verdict: Verdict::default(),
            submitting: false,
            review_error: None,
            threads: HashMap::new(),
            queued_actions: HashMap::new(),
            generation: 0,
            load_started: false,
            next_local_id: 0,
            seed_conversation: None,
            from_claude: HashSet::new(),
            suggestion: None,
            verdict_by: VerdictBy::Nobody,
            _load: None,
        }
    }

    pub fn with_log_sink() -> Self {
        Self::new(Arc::new(LogSink))
    }

    /// Where writes go from now on.
    pub fn set_sink(&mut self, sink: Arc<dyn ReviewSink>) {
        self.sink = sink;
    }

    /// Starts the conversation composer with this text, until it is sent.
    pub fn seed_conversation(&mut self, text: Option<&str>) {
        self.seed_conversation = text.map(str::to_owned);
    }

    /// Persist drafts in `store` through `saver`, a writer shared with the
    /// other open pull requests; `None` keeps them in memory.
    pub fn attach(
        &mut self,
        store: Option<Store>,
        saver: Option<DraftSaver>,
        cx: &mut Context<Self>,
    ) {
        if self.store.as_ref().map(Store::path) == store.as_ref().map(Store::path) {
            return;
        }
        self.store = store;
        self.saver = saver;
        self.start_load(cx);
    }

    /// Writes pending drafts now. For quitting; it blocks briefly.
    pub fn saver(&self) -> Option<DraftSaver> {
        self.saver.clone()
    }

    pub fn open_pr(&self) -> Option<&PrKey> {
        self.pr.as_ref()
    }

    /// The open pull request changed; loads its drafts and queued comments.
    pub fn switch(&mut self, pr: Option<PrKey>, commit: Option<Sha>, cx: &mut Context<Self>) {
        self.commit = commit;
        if self.pr == pr {
            self.start_load(cx);
            return;
        }
        self.generation += 1;
        self.pr = pr;
        self.slots.clear();
        self.queued.clear();
        self.sent.clear();
        self.review_body.clear();
        self.review_epoch += 1;
        self.verdict = Verdict::default();
        self.submitting = false;
        self.review_error = None;
        self.threads.clear();
        self.queued_actions.clear();
        self.from_claude.clear();
        self.suggestion = None;
        self.verdict_by = VerdictBy::Nobody;
        self.load_started = false;
        self._load = None;
        self.start_load(cx);
        cx.notify();
    }

    /// Reads the stored drafts once the store and the pull request are both
    /// known, whichever came first, and asks the sink for a pending review
    /// started elsewhere.
    fn start_load(&mut self, cx: &mut Context<Self>) {
        let (Some(store), Some(pr)) = (self.store.clone(), self.pr.clone()) else {
            return;
        };
        if std::mem::replace(&mut self.load_started, true) {
            return;
        }
        let generation = self.generation;
        let (drafts, queued) = (store.drafts_for(&pr), store.queued_for(&pr));
        let pending = self.sink.pending(&pr);
        self._load = Some(cx.spawn(async move |this, cx| {
            let (drafts, queued) = (drafts.await, queued.await);
            this.update(cx, |this, cx| {
                this.loaded(generation, drafts, queued, Ok(None), cx);
            })
            .ok();
            // The forge answers later than the disk; its comments join the
            // queue when they come.
            let pending = pending.await;
            let adopted = match (&pending, generation_store(&this, cx)) {
                (Ok(Some(found)), Some((pr, store))) => {
                    Some(store.adopt_queued(&pr, adoptable(found)).await)
                }
                _ => None,
            };
            this.update(cx, |this, cx| {
                this.adopted(generation, pending, adopted, cx)
            })
            .ok();
        }));
    }

    fn loaded(
        &mut self,
        generation: u64,
        drafts: store::Result<Vec<Draft>>,
        queued: store::Result<Vec<QueuedComment>>,
        pending: Result<Option<PendingReview>, SinkError>,
        cx: &mut Context<Self>,
    ) {
        if generation != self.generation {
            return;
        }
        match drafts {
            Ok(drafts) => {
                for draft in drafts {
                    self.restore(draft);
                }
            }
            Err(error) => eprintln!("review: could not read drafts: {error}"),
        }
        match queued {
            Ok(queued) => self.queued = queued,
            Err(error) => eprintln!("review: could not read queued comments: {error}"),
        }
        self.adopted(generation, pending, None, cx);
        cx.notify();
    }

    /// The forge's pending review arrived: its comments that are not queued
    /// here yet join the queue (the store's copy when there is a store).
    fn adopted(
        &mut self,
        generation: u64,
        pending: Result<Option<PendingReview>, SinkError>,
        stored: Option<store::Result<Vec<QueuedComment>>>,
        cx: &mut Context<Self>,
    ) {
        if generation != self.generation {
            return;
        }
        let on_forge = match &pending {
            Ok(Some(found)) => Some(found.comments.iter().map(|c| c.id.clone()).collect()),
            Ok(None) => Some(Vec::new()),
            Err(_) => None,
        };
        match (pending, stored) {
            (_, Some(Ok(queued))) => self.queued = queued,
            (_, Some(Err(error))) => {
                eprintln!("review: could not keep the forge's review: {error}")
            }
            (Ok(Some(found)), None) => {
                for new in adoptable(&found) {
                    let known = self
                        .queued
                        .iter()
                        .any(|q| q.forge_id.is_some() && q.forge_id == new.forge_id);
                    if !known && let Some(pr) = self.pr.clone() {
                        let id = self.local_id();
                        self.queued.push(queued_comment(id, pr, new));
                    }
                }
            }
            (Ok(None), None) => {}
            (Err(error), None) => eprintln!("review: could not read the pending review: {error}"),
        }
        if let Some(on_forge) = on_forge {
            self.drop_gone(&on_forge);
        }
        cx.notify();
    }

    /// Queued comments the forge had and no longer has (the review was
    /// submitted or discarded elsewhere) leave the queue.
    fn drop_gone(&mut self, on_forge: &[CommentId]) {
        let gone: Vec<i64> = self
            .queued
            .iter()
            .filter(|q| q.forge_id.as_ref().is_some_and(|id| !on_forge.contains(id)))
            .map(|q| q.id)
            .collect();
        if gone.is_empty() {
            return;
        }
        self.queued.retain(|q| !gone.contains(&q.id));
        if let Some(store) = &self.store {
            for id in gone {
                store.delete_queued(id).detach();
            }
        }
    }

    fn restore(&mut self, draft: Draft) {
        match draft.anchor {
            DraftAnchor::Review => {
                if self.review_body.is_empty() {
                    self.review_body = draft.body;
                    self.review_epoch += 1;
                }
            }
            anchor => {
                // Text typed before the load arrived wins over the stored one.
                if self.slot(&anchor).is_some() {
                    return;
                }
                self.slots.push(Slot {
                    open: !matches!(anchor, DraftAnchor::Conversation),
                    anchor,
                    body: draft.body,
                    composer: None,
                    focus: false,
                    suggestion: None,
                    busy: false,
                });
            }
        }
    }

    // ---- reading

    pub fn slots(&self) -> &[Slot] {
        &self.slots
    }

    pub fn slot(&self, anchor: &DraftAnchor) -> Option<&Slot> {
        self.slots.iter().find(|slot| slot.anchor == *anchor)
    }

    fn slot_mut(&mut self, anchor: &DraftAnchor) -> Option<&mut Slot> {
        self.slots.iter_mut().find(|slot| slot.anchor == *anchor)
    }

    /// Whether `anchor` has a composer showing.
    pub fn is_open(&self, anchor: &DraftAnchor) -> bool {
        self.slot(anchor).is_some_and(|slot| slot.open)
    }

    /// Slots with text in them, shown or not.
    pub fn draft_count(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| !slot.body.trim().is_empty())
            .count()
    }

    pub fn queued(&self) -> &[QueuedComment] {
        &self.queued
    }

    pub fn sent(&self) -> &[LocalComment] {
        &self.sent
    }

    pub fn review_body(&self) -> &str {
        &self.review_body
    }

    pub fn review_epoch(&self) -> u64 {
        self.review_epoch
    }

    pub fn verdict(&self) -> Verdict {
        self.verdict
    }

    pub fn submitting(&self) -> bool {
        self.submitting
    }

    pub fn review_error(&self) -> Option<&SharedString> {
        self.review_error.as_ref()
    }

    /// Where resolving or unresolving `thread` stands, if it was asked for.
    pub fn thread_state(&self, thread: &ThreadId) -> Option<&ActionState> {
        self.threads.get(thread)
    }

    /// Where deleting the queued comment `id` stands, if it was asked for.
    pub fn queued_state(&self, id: i64) -> Option<&ActionState> {
        self.queued_actions.get(&id)
    }

    /// Whether the submit bar has something to show.
    pub fn has_review(&self) -> bool {
        !self.queued.is_empty() || !self.review_body.trim().is_empty()
    }

    // ---- composers

    /// Shows the composer for `anchor`, creating it with `suggestion` as the
    /// lines "Suggest change" quotes, and takes focus into it.
    pub fn open(
        &mut self,
        anchor: DraftAnchor,
        suggestion: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.open_with(anchor, suggestion, String::new(), cx);
    }

    fn open_with(
        &mut self,
        anchor: DraftAnchor,
        suggestion: Option<String>,
        text: String,
        cx: &mut Context<Self>,
    ) {
        match self.slot_mut(&anchor) {
            Some(slot) => {
                slot.open = true;
                slot.focus = true;
                if slot.suggestion.is_none() {
                    slot.suggestion = suggestion;
                }
                if let Some(composer) = slot.composer.clone() {
                    composer.update(cx, |composer, cx| composer.request_focus(cx));
                }
            }
            None => self.slots.push(Slot {
                anchor: anchor.clone(),
                body: text,
                open: true,
                composer: None,
                focus: true,
                suggestion,
                busy: false,
            }),
        }
        cx.emit(WritingEvent::Opened(anchor));
        cx.notify();
    }

    /// Shows every kept draft again. The conversation's comment box is
    /// always shown, so it is left as it is.
    pub fn reopen_all(&mut self, cx: &mut Context<Self>) {
        let mut first = None;
        for slot in &mut self.slots {
            if !slot.open && !matches!(slot.anchor, DraftAnchor::Conversation) {
                slot.open = true;
                first.get_or_insert_with(|| slot.anchor.clone());
            }
        }
        if let Some(anchor) = first {
            cx.emit(WritingEvent::Opened(anchor));
            cx.notify();
        }
    }

    /// The composer of `anchor`, built on first use. A composer is made here
    /// (a view with a window) because restoring drafts has none.
    pub fn composer(
        &mut self,
        anchor: &DraftAnchor,
        place: Option<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<Composer> {
        if self.slot(anchor).is_none() {
            let text = match anchor {
                DraftAnchor::Conversation => self.seed_conversation.clone().unwrap_or_default(),
                _ => String::new(),
            };
            self.slots.push(Slot {
                anchor: anchor.clone(),
                body: text,
                open: true,
                composer: None,
                focus: false,
                suggestion: None,
                busy: false,
            });
        }
        let place = self.place_label(anchor, place);
        let preview = self.is_from_claude(anchor);
        let slot = self.slot_mut(anchor).expect("the slot was just made");
        if let Some(composer) = &slot.composer {
            return composer.clone();
        }
        let init = ComposerInit {
            anchor: anchor.clone(),
            text: slot.body.clone(),
            suggestion: slot.suggestion.clone(),
            place,
            focus: std::mem::take(&mut slot.focus),
            preview,
        };
        let composer = cx.new(|cx| Composer::new(init, window, cx));
        slot.composer = Some(composer.clone());
        let key = anchor.clone();
        cx.subscribe(
            &composer,
            move |this, _, event: &ComposerEvent, cx| match event {
                ComposerEvent::Edited(text) => this.edited(&key, text.clone(), cx),
                ComposerEvent::Submit(action) => this.submit(&key, *action, cx),
                ComposerEvent::Close => this.close(&key, cx),
                ComposerEvent::Discard => this.discard(&key, cx),
            },
        )
        .detach();
        composer
    }

    fn edited(&mut self, anchor: &DraftAnchor, body: String, cx: &mut Context<Self>) {
        let Some(slot) = self.slot_mut(anchor) else {
            return;
        };
        let was = !slot.body.trim().is_empty();
        let is = !body.trim().is_empty();
        slot.body.clone_from(&body);
        if let (Some(saver), Some(pr)) = (&self.saver, &self.pr) {
            saver.edit(pr, anchor, body, self.commit.clone());
        }
        // Only a change in how many drafts there are needs the views to
        // rebuild; a keystroke must not re-project the diff.
        if was != is {
            cx.notify();
        }
    }

    /// Esc: hides the composer and keeps its draft.
    pub fn close(&mut self, anchor: &DraftAnchor, cx: &mut Context<Self>) {
        let Some(slot) = self.slot_mut(anchor) else {
            return;
        };
        if slot.body.trim().is_empty() {
            self.remove(anchor, cx);
            return;
        }
        slot.open = false;
        slot.composer = None;
        cx.notify();
    }

    /// Deletes the draft and its composer.
    pub fn discard(&mut self, anchor: &DraftAnchor, cx: &mut Context<Self>) {
        if let (Some(saver), Some(pr)) = (&self.saver, &self.pr) {
            drop(saver.delete(pr, anchor));
        }
        self.remove(anchor, cx);
    }

    fn remove(&mut self, anchor: &DraftAnchor, cx: &mut Context<Self>) {
        self.slots.retain(|slot| slot.anchor != *anchor);
        self.from_claude.remove(anchor);
        cx.notify();
    }

    /// Adds Claude's findings as drafts: an open composer on each line with
    /// the text in it, labeled [`FROM_CLAUDE`], saved like any draft. Nothing
    /// is sent; the user edits, adds to the review or discards each. A line
    /// that already has a draft keeps it; those anchors are returned.
    pub fn add_from_claude(
        &mut self,
        drafts: Vec<(DraftAnchor, String)>,
        cx: &mut Context<Self>,
    ) -> Vec<DraftAnchor> {
        let mut kept = Vec::new();
        for (anchor, body) in drafts {
            if self.slot(&anchor).is_some() {
                kept.push(anchor);
                continue;
            }
            if let (Some(saver), Some(pr)) = (&self.saver, &self.pr) {
                saver.edit(pr, &anchor, body.clone(), self.commit.clone());
            }
            self.from_claude.insert(anchor.clone());
            self.slots.push(Slot {
                anchor,
                body,
                open: true,
                composer: None,
                focus: false,
                suggestion: None,
                busy: false,
            });
        }
        cx.notify();
        kept
    }

    /// Adds the draft at `anchor` to the pending review, as its composer's
    /// "Add to review" does.
    pub fn queue_draft(&mut self, anchor: &DraftAnchor, cx: &mut Context<Self>) {
        self.submit(anchor, SubmitAction::Queue, cx);
    }

    /// The drafts "Add to review" takes: comments on a line or a file with
    /// text, not already being sent. Replies, edits and the conversation's
    /// comment are sent on their own, never queued.
    pub fn queueable(&self) -> Vec<DraftAnchor> {
        self.slots
            .iter()
            .filter(|slot| {
                matches!(
                    slot.anchor,
                    DraftAnchor::Line { .. } | DraftAnchor::File { .. }
                ) && !slot.busy
                    && !slot.body.trim().is_empty()
            })
            .map(|slot| slot.anchor.clone())
            .collect()
    }

    /// The drafts with text, by whether the review can take them now.
    pub fn draft_tally(&self) -> DraftTally {
        let mut tally = DraftTally::default();
        for slot in self
            .slots
            .iter()
            .filter(|slot| !slot.body.trim().is_empty())
        {
            let count = match slot.anchor {
                DraftAnchor::Line { .. } | DraftAnchor::File { .. } if slot.busy => {
                    &mut tally.adding
                }
                DraftAnchor::Line { .. } | DraftAnchor::File { .. } => &mut tally.queueable,
                _ => &mut tally.other,
            };
            *count += 1;
        }
        tally
    }

    /// Adds every [`queueable`](Self::queueable) draft to the pending review.
    pub fn queue_all(&mut self, cx: &mut Context<Self>) {
        for anchor in self.queueable() {
            self.submit(&anchor, SubmitAction::Queue, cx);
        }
    }

    /// Whether Claude wrote the draft at `anchor`.
    pub fn is_from_claude(&self, anchor: &DraftAnchor) -> bool {
        self.from_claude.contains(anchor)
    }

    /// What the card of `anchor`'s draft says above the text: `place`, and
    /// [`FROM_CLAUDE`] after it when Claude wrote the draft.
    pub fn place_label(
        &self,
        anchor: &DraftAnchor,
        place: Option<SharedString>,
    ) -> Option<SharedString> {
        if !self.is_from_claude(anchor) {
            return place;
        }
        Some(match place {
            Some(place) => format!("{place}  \u{b7}  {FROM_CLAUDE}").into(),
            None => FROM_CLAUDE.into(),
        })
    }

    // ---- sending

    fn command(
        &self,
        anchor: &DraftAnchor,
        action: SubmitAction,
        body: String,
    ) -> Option<ReviewCommand> {
        match anchor {
            DraftAnchor::Reply { thread } => {
                return Some(ReviewCommand::Reply {
                    thread: thread.clone(),
                    body,
                });
            }
            DraftAnchor::Edit { queued } => {
                let queued = self.queued.iter().find(|q| q.id == *queued)?;
                return Some(ReviewCommand::EditQueued {
                    comment: forge_pending(queued)?,
                    body,
                });
            }
            _ => {}
        }
        let comment = NewComment {
            place: Place::of(anchor)?,
            body,
            commit: self.commit.clone(),
        };
        Some(match (action, &comment.place) {
            (SubmitAction::Queue, Place::Line { .. } | Place::File { .. }) => {
                ReviewCommand::QueueComment(comment)
            }
            _ => ReviewCommand::SendComment(comment),
        })
    }

    fn set_busy(
        &mut self,
        anchor: &DraftAnchor,
        busy: bool,
        error: Option<SharedString>,
        cx: &mut App,
    ) {
        let Some(slot) = self.slot_mut(anchor) else {
            return;
        };
        slot.busy = busy;
        if let Some(composer) = slot.composer.clone() {
            composer.update(cx, |composer, cx| composer.set_status(busy, error, cx));
        }
    }

    fn submit(&mut self, anchor: &DraftAnchor, action: SubmitAction, cx: &mut Context<Self>) {
        let Some(pr) = self.pr.clone() else {
            return;
        };
        let Some(slot) = self.slot(anchor) else {
            return;
        };
        let body = slot.body.trim().to_owned();
        if body.is_empty() || slot.busy {
            return;
        }
        // A comment only the local queue holds is edited here alone.
        if let DraftAnchor::Edit { queued } = anchor
            && self
                .queued
                .iter()
                .any(|q| q.id == *queued && q.forge_id.is_none())
        {
            self.edited_queued(anchor, &body, None);
            self.discard(anchor, cx);
            return;
        }
        let Some(command) = self.command(anchor, action, body.clone()) else {
            return;
        };
        self.set_busy(anchor, true, None, cx);
        let sent = self.sink.submit(&pr, command);
        let write = Write {
            pr,
            anchor: anchor.clone(),
            body,
            commit: self.commit.clone(),
        };
        let saver = self.saver.clone();
        cx.spawn(async move |this, cx| {
            let result = sent.await;
            // The saver does these in order after any edit still waiting, so
            // a late save cannot bring the draft back.
            let queued = match (&result, &saver) {
                (Ok(outcome @ (SinkOutcome::Queued(_) | SinkOutcome::Done)), Some(saver))
                    if is_queue(&write.anchor, outcome) =>
                {
                    saver
                        .queue(&write.pr, new_queued(&write, outcome))
                        .await
                        .ok()
                        .and_then(Result::ok)
                }
                (Ok(_), Some(saver)) => {
                    if let Ok(Err(error)) = saver.delete(&write.pr, &write.anchor).await {
                        eprintln!("review: could not delete a sent draft: {error}");
                    }
                    None
                }
                _ => None,
            };
            this.update(cx, |this, cx| this.finished(write, result, queued, cx))
                .ok();
        })
        .detach();
    }

    fn finished(
        &mut self,
        write: Write,
        result: Result<SinkOutcome, SinkError>,
        queued: Option<QueuedComment>,
        cx: &mut Context<Self>,
    ) {
        if self.pr.as_ref() != Some(&write.pr) {
            return;
        }
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(error) => {
                // The text is still in the store; make sure its newest version is.
                if let Some(saver) = &self.saver {
                    saver.edit(&write.pr, &write.anchor, write.body, write.commit);
                }
                let message = SharedString::from(format!("Not sent: {error}"));
                self.set_busy(&write.anchor, false, Some(message), cx);
                return;
            }
        };
        let anchor = write.anchor.clone();
        match outcome {
            SinkOutcome::Edited(pending) => {
                self.edited_queued(&anchor, &pending.body, Some(&pending.id));
            }
            SinkOutcome::Done if matches!(anchor, DraftAnchor::Edit { .. }) => {
                self.edited_queued(&anchor, &write.body, None);
            }
            ref queue @ (SinkOutcome::Queued(_) | SinkOutcome::Done)
                if is_queue(&anchor, queue) =>
            {
                let stored = queued.unwrap_or_else(|| {
                    let id = self.local_id();
                    queued_comment(id, write.pr.clone(), new_queued(&write, queue))
                });
                self.queued.push(stored);
            }
            SinkOutcome::Done => self.sent.push(LocalComment {
                body: write.body.into(),
                kind: CommentKind::Posted,
            }),
            SinkOutcome::Posted(change) => cx.emit(WritingEvent::Threads(change)),
            SinkOutcome::Replied { thread, comment } => {
                cx.emit(WritingEvent::Threads(ThreadChange::Add {
                    threads: Vec::new(),
                    replies: vec![(thread, comment)],
                }));
            }
            SinkOutcome::Queued(_) | SinkOutcome::Submitted { .. } | SinkOutcome::Resolved(_) => {}
        }
        self.remove(&anchor, cx);
    }

    /// A queued comment's edit went through: its text, and its forge id
    /// when the forge deleted and added it again.
    fn edited_queued(&mut self, anchor: &DraftAnchor, body: &str, forge_id: Option<&CommentId>) {
        let DraftAnchor::Edit { queued: id } = *anchor else {
            return;
        };
        let Some(queued) = self.queued.iter_mut().find(|q| q.id == id) else {
            return;
        };
        body.clone_into(&mut queued.body);
        let new_id = forge_id.filter(|&new| queued.forge_id.as_ref() != Some(new));
        if let Some(new_id) = new_id {
            queued.forge_id = Some(new_id.clone());
        }
        if let Some(store) = &self.store {
            store.update_queued(id, body, new_id).detach();
        }
    }

    fn local_id(&mut self) -> i64 {
        self.next_local_id -= 1;
        self.next_local_id
    }

    // ---- thread and queue actions

    /// Resolves or unresolves `thread` on the forge.
    pub fn set_resolved(&mut self, thread: &Thread, resolved: bool, cx: &mut Context<Self>) {
        let Some(pr) = self.pr.clone() else {
            return;
        };
        if self.threads.get(&thread.id) == Some(&ActionState::Busy) {
            return;
        }
        let id = thread.id.clone();
        self.threads.insert(id.clone(), ActionState::Busy);
        let sent = self.sink.submit(
            &pr,
            ReviewCommand::SetResolved {
                thread: id.clone(),
                resolved,
            },
        );
        cx.spawn(async move |this, cx| {
            let result = sent.await;
            this.update(cx, |this, cx| {
                if this.pr.as_ref() != Some(&pr) {
                    return;
                }
                match result {
                    Ok(SinkOutcome::Resolved(thread)) => {
                        this.threads.remove(&id);
                        cx.emit(WritingEvent::Threads(ThreadChange::Replace(thread)));
                    }
                    Ok(_) => {
                        this.threads.remove(&id);
                    }
                    Err(error) => {
                        let message = format!("Not changed: {error}").into();
                        this.threads.insert(id, ActionState::Failed(message));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Removes a queued comment, from the forge's review first when it is
    /// there.
    pub fn delete_queued(&mut self, id: i64, cx: &mut Context<Self>) {
        let Some(pr) = self.pr.clone() else {
            return;
        };
        let Some(queued) = self.queued.iter().find(|q| q.id == id).cloned() else {
            return;
        };
        if self.queued_actions.get(&id) == Some(&ActionState::Busy) {
            return;
        }
        self.queued_actions.insert(id, ActionState::Busy);
        let sent = queued
            .forge_id
            .clone()
            .map(|comment| self.sink.submit(&pr, ReviewCommand::DeleteQueued(comment)));
        let store = self.store.clone();
        cx.spawn(async move |this, cx| {
            let result = match sent {
                Some(sent) => sent.await.map(drop),
                None => Ok(()),
            };
            if result.is_ok()
                && let Some(store) = &store
                && let Err(error) = store.delete_queued(id).await
            {
                eprintln!("review: could not drop a deleted comment: {error}");
            }
            this.update(cx, |this, cx| {
                if this.pr.as_ref() != Some(&pr) {
                    return;
                }
                match result {
                    Ok(()) => {
                        this.queued_actions.remove(&id);
                        this.queued.retain(|q| q.id != id);
                        this.slots
                            .retain(|slot| slot.anchor != DraftAnchor::Edit { queued: id });
                    }
                    Err(error) => {
                        let message = format!("Not deleted: {error}").into();
                        this.queued_actions.insert(id, ActionState::Failed(message));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Opens a composer on a queued comment's text; sending it edits the
    /// comment in the forge's review.
    pub fn edit_queued(&mut self, id: i64, cx: &mut Context<Self>) {
        let Some(queued) = self.queued.iter().find(|q| q.id == id) else {
            return;
        };
        let body = queued.body.clone();
        self.open_with(DraftAnchor::Edit { queued: id }, None, body, cx);
    }

    // ---- the review

    /// The summary box changed. A summary the user edits is theirs.
    pub fn set_review_body(&mut self, body: String) {
        if body == self.review_body {
            return;
        }
        if let (Some(saver), Some(pr)) = (&self.saver, &self.pr) {
            saver.edit(pr, &DraftAnchor::Review, body.clone(), None);
        }
        self.review_body = body;
        self.from_claude.remove(&DraftAnchor::Review);
    }

    /// The user's choice of verdict.
    pub fn set_verdict(&mut self, verdict: Verdict, cx: &mut Context<Self>) {
        if self.verdict != verdict {
            self.verdict = verdict;
            self.verdict_by = VerdictBy::User;
            cx.notify();
        }
    }

    /// Takes Claude's suggested verdict and summary, replacing an earlier
    /// suggestion. It fills in the form unless the user already wrote a
    /// summary or picked a verdict, or it suggests approving; then the form
    /// stays as it is and the suggestion waits for
    /// [`use_suggestion`](Self::use_suggestion). Nothing is sent.
    ///
    /// An approval is never put in the form for the user: the agent read the
    /// pull request, whose text may have steered it, and an approval is the
    /// verdict that grants something. It takes the user's own click.
    pub fn suggest_review(
        &mut self,
        suggestion: ReviewSuggestion,
        cx: &mut Context<Self>,
    ) -> Suggested {
        let users = self.verdict_by == VerdictBy::User
            || (!self.review_body.trim().is_empty()
                && !self.from_claude.contains(&DraftAnchor::Review));
        let approves = suggestion.verdict == Verdict::Approve;
        self.suggestion = Some(suggestion);
        if users || approves {
            cx.notify();
            return Suggested::Offered;
        }
        self.use_suggestion(cx);
        Suggested::Filled
    }

    /// Puts the suggestion in the form: its verdict, and its summary in
    /// place of the one there.
    pub fn use_suggestion(&mut self, cx: &mut Context<Self>) {
        let Some(suggestion) = self.suggestion.clone() else {
            return;
        };
        self.replace_review_body(suggestion.body);
        if !self.review_body.is_empty() {
            self.from_claude.insert(DraftAnchor::Review);
        }
        self.verdict = suggestion.verdict;
        self.verdict_by = VerdictBy::Claude;
        cx.notify();
    }

    /// Drops the suggestion, and the verdict and summary it put in the form
    /// that the user has not changed since.
    pub fn discard_suggestion(&mut self, cx: &mut Context<Self>) {
        if self.suggestion.take().is_none() {
            return;
        }
        if self.from_claude.contains(&DraftAnchor::Review) {
            self.replace_review_body(String::new());
        }
        if self.verdict_by == VerdictBy::Claude {
            self.verdict = Verdict::default();
            self.verdict_by = VerdictBy::Nobody;
        }
        cx.notify();
    }

    /// Sets the summary from outside the summary box, which shows it next.
    fn replace_review_body(&mut self, body: String) {
        self.set_review_body(body);
        self.review_epoch += 1;
    }

    /// Claude's newest suggested review, until it is submitted or discarded.
    pub fn suggestion(&self) -> Option<&ReviewSuggestion> {
        self.suggestion.as_ref()
    }

    /// Claude's suggestion, if any, and what of it the form holds.
    pub fn suggestion_state(&self) -> SuggestionState {
        let verdict_from_claude = self.verdict_by == VerdictBy::Claude;
        let offered = self.suggestion.as_ref().is_some_and(|suggestion| {
            !verdict_from_claude
                || self.verdict != suggestion.verdict
                || self.review_body != suggestion.body
        });
        SuggestionState {
            suggestion: self.suggestion.clone(),
            verdict_from_claude,
            summary_from_claude: self.from_claude.contains(&DraftAnchor::Review),
            offered,
        }
    }

    /// Submits the pending review with the chosen verdict and summary.
    pub fn submit_review(&mut self, cx: &mut Context<Self>) {
        let Some(pr) = self.pr.clone() else {
            return;
        };
        if self.submitting {
            return;
        }
        self.submitting = true;
        self.review_error = None;
        let body = self.review_body.trim().to_owned();
        let sent = self.sink.submit(
            &pr,
            ReviewCommand::SubmitReview {
                verdict: self.verdict,
                body,
            },
        );
        let saver = self.saver.clone();
        cx.spawn(async move |this, cx| {
            let result = sent.await;
            if let (Ok(_), Some(saver)) = (&result, &saver)
                && let Ok(Err(error)) = saver.finish_review(&pr).await
            {
                eprintln!("review: could not clear the submitted review: {error}");
            }
            this.update(cx, |this, cx| this.review_submitted(&pr, result, cx))
                .ok();
        })
        .detach();
        cx.notify();
    }

    fn review_submitted(
        &mut self,
        pr: &PrKey,
        result: Result<SinkOutcome, SinkError>,
        cx: &mut Context<Self>,
    ) {
        if self.pr.as_ref() != Some(pr) {
            return;
        }
        self.submitting = false;
        match result {
            Ok(outcome) => {
                let queued = std::mem::take(&mut self.queued);
                match outcome {
                    SinkOutcome::Submitted { change, .. } => {
                        cx.emit(WritingEvent::Threads(change));
                    }
                    _ => self
                        .sent
                        .extend(queued.into_iter().map(|queued| LocalComment {
                            body: queued.body.into(),
                            kind: CommentKind::Posted,
                        })),
                }
                self.slots
                    .retain(|slot| !matches!(slot.anchor, DraftAnchor::Edit { .. }));
                self.review_body.clear();
                self.review_epoch += 1;
                self.verdict = Verdict::default();
                self.verdict_by = VerdictBy::Nobody;
                self.suggestion = None;
                self.from_claude.remove(&DraftAnchor::Review);
                cx.emit(WritingEvent::ReviewSubmitted);
            }
            Err(error) => {
                self.review_error = Some(SharedString::from(format!("Not submitted: {error}")));
            }
        }
        cx.notify();
    }
}

/// The open pull request and the store, while `this` is alive.
fn generation_store(
    this: &gpui::WeakEntity<Writing>,
    cx: &mut gpui::AsyncApp,
) -> Option<(PrKey, Store)> {
    this.read_with(cx, |this, _| Some((this.pr.clone()?, this.store.clone()?)))
        .ok()
        .flatten()
}

/// Whether a write of `anchor` that answered `outcome` joined the queue.
fn is_queue(anchor: &DraftAnchor, outcome: &SinkOutcome) -> bool {
    match outcome {
        SinkOutcome::Queued(_) => true,
        // A sink that returns nothing queues what was asked to be queued.
        SinkOutcome::Done => matches!(anchor, DraftAnchor::Line { .. } | DraftAnchor::File { .. }),
        _ => false,
    }
}

fn new_queued(write: &Write, outcome: &SinkOutcome) -> NewQueued {
    let pending = match outcome {
        SinkOutcome::Queued(pending) => Some(pending),
        _ => None,
    };
    NewQueued {
        anchor: write.anchor.clone(),
        body: write.body.clone(),
        commit: write.commit.clone(),
        forge_id: pending.map(|p| p.id.clone()),
        thread: pending.and_then(|p| p.thread.clone()),
    }
}

fn queued_comment(id: i64, pr: PrKey, new: NewQueued) -> QueuedComment {
    QueuedComment {
        id,
        pr,
        anchor: new.anchor,
        body: new.body,
        commit: new.commit,
        queued_at: Utc::now(),
        forge_id: new.forge_id,
        thread: new.thread,
    }
}

/// The forge's pending comments as queue entries.
fn adoptable(review: &PendingReview) -> Vec<NewQueued> {
    review
        .comments
        .iter()
        .map(|comment| NewQueued {
            anchor: draft_anchor(&comment.place),
            body: comment.body.clone(),
            commit: review.review.commit.clone(),
            forge_id: Some(comment.id.clone()),
            thread: comment.thread.clone(),
        })
        .collect()
}

/// A queued comment as the forge's pending comment, once the forge has it.
fn forge_pending(queued: &QueuedComment) -> Option<PendingComment> {
    Some(PendingComment {
        id: queued.forge_id.clone()?,
        place: pending_place(&queued.anchor)?,
        thread: queued.thread.clone(),
        body: queued.body.clone(),
        created_at: Some(queued.queued_at),
    })
}

/// "src/a.rs  line 31", "lines 29\u{2013}31", with "old" for the removed side.
pub fn anchor_label(anchor: &DraftAnchor) -> Option<SharedString> {
    match anchor {
        DraftAnchor::Line { path, line, start } => {
            let side = match line.side {
                forge::model::Side::Old => "old ",
                forge::model::Side::New => "",
            };
            let range = match start.map(|s| s.line).filter(|&s| s < line.line) {
                Some(start) => format!("{side}lines {start}\u{2013}{}", line.line),
                None => format!("{side}line {}", line.line),
            };
            Some(format!("{path}  {range}").into())
        }
        DraftAnchor::File { path } => Some(path.clone().into()),
        DraftAnchor::Edit { .. } => Some("Editing a queued comment".into()),
        DraftAnchor::Reply { .. } | DraftAnchor::Conversation | DraftAnchor::Review => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge::model::{CommentAnchor, CommentId, LineRef, PendingPlace, PrNumber, RepoRef, Side};

    fn line(side: Side, line: u32, start: Option<u32>) -> DraftAnchor {
        DraftAnchor::Line {
            path: "src/a.rs".into(),
            line: LineRef { side, line },
            start: start.map(|line| LineRef { side, line }),
        }
    }

    #[test]
    fn labels_name_the_line_the_range_and_the_side() {
        let label = |anchor| anchor_label(&anchor).map(|l| l.to_string());
        assert_eq!(
            label(line(Side::New, 31, None)).as_deref(),
            Some("src/a.rs  line 31")
        );
        assert_eq!(
            label(line(Side::New, 31, Some(29))).as_deref(),
            Some("src/a.rs  lines 29\u{2013}31")
        );
        assert_eq!(
            label(line(Side::Old, 8, None)).as_deref(),
            Some("src/a.rs  old line 8")
        );
        assert_eq!(label(DraftAnchor::Conversation), None);
    }

    #[test]
    fn only_queue_outcomes_join_the_queue() {
        let code = line(Side::New, 3, None);
        let pending = PendingComment {
            id: CommentId::new("C"),
            place: PendingPlace::New(CommentAnchor::File { path: "a".into() }),
            thread: None,
            body: String::new(),
            created_at: None,
        };
        assert!(is_queue(&code, &SinkOutcome::Queued(pending)));
        assert!(is_queue(&code, &SinkOutcome::Done));
        assert!(!is_queue(&DraftAnchor::Conversation, &SinkOutcome::Done));
        let reply = DraftAnchor::Reply {
            thread: ThreadId::new("T"),
        };
        assert!(!is_queue(&reply, &SinkOutcome::Done));
    }

    #[test]
    fn a_queued_comment_is_the_forge_s_only_once_the_forge_has_it() {
        let pr = PrKey::new(RepoRef::new("github.com", "a", "b"), PrNumber(1));
        let new = NewQueued {
            anchor: line(Side::New, 3, None),
            body: "x".into(),
            commit: None,
            forge_id: None,
            thread: None,
        };
        let local = queued_comment(-1, pr.clone(), new.clone());
        assert_eq!(forge_pending(&local), None);
        let on_forge = queued_comment(
            -2,
            pr,
            NewQueued {
                forge_id: Some(CommentId::new("C")),
                ..new
            },
        );
        let pending = forge_pending(&on_forge).unwrap();
        assert_eq!(pending.id, CommentId::new("C"));
        assert_eq!(draft_anchor(&pending.place), on_forge.anchor);
    }
}
