//! Local review tracking for the inbox rows, never sent to a forge: a seen
//! mark set when a pull request is opened (new commits, comments or reviews
//! after it show an unread dot), a reviewed mark set by hand or when every
//! file is viewed, and the viewed progress of the last time it was open.
//!
//! The marks live in the store's `review_marks` table when there is a store,
//! in memory otherwise; every change is written through at once.

use std::collections::HashMap;

use chrono::Utc;
use gpui::{App, AppContext as _, Context, Entity, Global, SharedString, Subscription};
use store::{ReviewMark, Reviewed, Seen, ViewedProgress};

use crate::appearance::Appearance;
use crate::model::{PrKey, PullRequest};
use crate::session::Session;
use crate::settings::InboxSettings;

/// Whether a reviewed mark still holds: it was set at the row's head.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewedMark {
    Current,
    /// New commits came after it.
    Changed,
}

/// How much of a pull request was viewed when it was last open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Progress {
    pub viewed: u32,
    pub total: u32,
    /// Counted at the row's head; after a push the count is of older files.
    pub current: bool,
}

impl Progress {
    /// The share viewed, 0 to 1.
    #[must_use]
    pub fn fraction(self) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            (self.viewed as f32 / self.total as f32).min(1.0)
        }
    }
}

/// What an inbox row shows of its marks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RowMarks {
    pub unread: bool,
    pub reviewed: Option<ReviewedMark>,
    pub progress: Option<Progress>,
}

impl RowMarks {
    /// Reviewed at this head with nothing new since: drawn faded when the
    /// setting asks.
    #[must_use]
    pub fn settled(self) -> bool {
        !self.unread && self.reviewed == Some(ReviewedMark::Current)
    }
}

/// The marks of `row` as its row shows them.
#[must_use]
pub fn row_marks(mark: Option<&ReviewMark>, row: &PullRequest) -> RowMarks {
    let Some(mark) = mark else {
        return RowMarks::default();
    };
    let head = row.sha.as_ref();
    let changed_since = |seen: &Seen| {
        seen.head != head
            || row.comments.unwrap_or(0) > seen.comments
            || row.review_count() > seen.reviews
    };
    RowMarks {
        unread: mark.unread || mark.seen.as_ref().is_some_and(changed_since),
        reviewed: mark.reviewed.as_ref().map(|reviewed| {
            if reviewed.head == head {
                ReviewedMark::Current
            } else {
                ReviewedMark::Changed
            }
        }),
        progress: mark.progress.as_ref().map(|progress| Progress {
            viewed: progress.viewed,
            total: progress.total,
            current: progress.head == head,
        }),
    }
}

/// The marks of every pull request, kept in step with the session.
pub struct Marks {
    store: Option<store::Store>,
    by_pr: HashMap<PrKey, ReviewMark>,
    /// Per open pull request, the diff generation and the number of viewed
    /// marks last recorded, so a notify that changed neither writes nothing.
    recorded: HashMap<PrKey, (u64, usize)>,
    /// The focused pull request as it read when last marked seen: a mark
    /// is set when the focus moves to it or something new arrives while it
    /// has it, so marking it unread by hand sticks until it is opened again.
    focus_seen: Option<(PrKey, Signature)>,
    _session: Subscription,
}

/// What the seen mark compares: head, comments and reviews.
type Signature = (SharedString, u32, u32);

fn signature(row: &PullRequest) -> Signature {
    (
        row.sha.clone(),
        row.comments.unwrap_or(0),
        row.review_count(),
    )
}

struct GlobalMarks(Entity<Marks>);

impl Global for GlobalMarks {}

/// One change [`Marks::sync`] found to make.
enum Change {
    Seen(Box<PullRequest>),
    Progress {
        key: PrKey,
        head: String,
        viewed: u32,
        total: u32,
    },
}

fn inbox_settings(cx: &App) -> InboxSettings {
    cx.try_global::<Appearance>()
        .map(|state| state.settings.inbox.clone())
        .unwrap_or_default()
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

impl Marks {
    /// The marks, made on first use; they follow the session from then on.
    pub fn global(cx: &mut App) -> Entity<Self> {
        if let Some(marks) = cx.try_global::<GlobalMarks>() {
            return marks.0.clone();
        }
        let session = Session::global(cx);
        let marks = cx.new(|cx| {
            let mut marks = Self {
                store: None,
                by_pr: HashMap::new(),
                recorded: HashMap::new(),
                focus_seen: None,
                _session: cx.observe(&session, |this: &mut Self, _, cx| this.sync(cx)),
            };
            marks.sync(cx);
            marks
        });
        cx.set_global(GlobalMarks(marks.clone()));
        marks
    }

    #[must_use]
    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalMarks>().map(|marks| marks.0.clone())
    }

    #[must_use]
    pub fn get(&self, key: &PrKey) -> Option<&ReviewMark> {
        self.by_pr.get(key)
    }

    /// What `row` shows of its marks.
    #[must_use]
    pub fn row(&self, row: &PullRequest) -> RowMarks {
        row_marks(self.by_pr.get(&row.key), row)
    }

    /// Marks `row` seen as it reads now; it is no longer unread.
    pub fn mark_seen(&mut self, row: &PullRequest, cx: &mut Context<Self>) {
        if self.seen_as(row) {
            return;
        }
        let mut mark = self.by_pr.get(&row.key).cloned().unwrap_or_default();
        mark.seen = Some(Seen {
            head: row.sha.to_string(),
            comments: row.comments.unwrap_or(0),
            reviews: row.review_count(),
            at: Utc::now(),
        });
        mark.unread = false;
        self.put(&row.key, mark, cx);
    }

    /// Marks `key` unread until it is opened again; a reviewed mark goes.
    pub fn mark_unread(&mut self, key: &PrKey, cx: &mut Context<Self>) {
        let mut mark = self.by_pr.get(key).cloned().unwrap_or_default();
        mark.unread = true;
        mark.reviewed = None;
        self.put(key, mark, cx);
    }

    /// Sets or clears the reviewed mark of `row`, at its head.
    pub fn set_reviewed(&mut self, row: &PullRequest, reviewed: bool, cx: &mut Context<Self>) {
        let mut mark = self.by_pr.get(&row.key).cloned().unwrap_or_default();
        mark.reviewed = reviewed.then(|| Reviewed {
            head: row.sha.to_string(),
            at: Utc::now(),
        });
        self.put(&row.key, mark, cx);
    }

    /// Records how much of `key` is viewed at `head`; with every file viewed
    /// and the setting on, it is marked reviewed too.
    fn record_progress(
        &mut self,
        key: &PrKey,
        progress: ViewedProgress,
        mark_reviewed: bool,
        cx: &mut Context<Self>,
    ) {
        let mut mark = self.by_pr.get(key).cloned().unwrap_or_default();
        let all = progress.total > 0 && progress.viewed >= progress.total;
        let reviewed_here = mark
            .reviewed
            .as_ref()
            .is_some_and(|reviewed| reviewed.head == progress.head);
        if mark_reviewed && all && !reviewed_here {
            mark.reviewed = Some(Reviewed {
                head: progress.head.clone(),
                at: Utc::now(),
            });
        }
        mark.progress = Some(progress);
        self.put(key, mark, cx);
    }

    fn put(&mut self, key: &PrKey, mark: ReviewMark, cx: &mut Context<Self>) {
        if self.by_pr.get(key) == Some(&mark) || (mark.is_empty() && !self.by_pr.contains_key(key))
        {
            return;
        }
        if let Some(store) = &self.store {
            store.put_mark(key, mark.clone()).detach();
        }
        if mark.is_empty() {
            self.by_pr.remove(key);
        } else {
            self.by_pr.insert(key.clone(), mark);
        }
        cx.notify();
    }

    /// Follows the session: reads the marks of a new store, marks the
    /// focused pull request seen, and records the viewed progress of the
    /// open ones.
    fn sync(&mut self, cx: &mut Context<Self>) {
        let session = Session::global(cx);
        let store = session.read(cx).store();
        if self.store.as_ref().map(store::Store::path) != store.map(store::Store::path) {
            let store = store.cloned();
            self.attach(store, cx);
        }
        let settings = inbox_settings(cx);
        let session = session.read(cx);
        let mut changes = Vec::new();
        let focused = session
            .open_key()
            .filter(|key| !session.is_background(key))
            .and_then(|key| session.inbox.get(key));
        match focused {
            Some(row) => {
                let now = (row.key.clone(), signature(row));
                if self.focus_seen.as_ref() != Some(&now) {
                    self.focus_seen = Some(now);
                    if settings.mark_seen_on_open {
                        changes.push(Change::Seen(Box::new(row.clone())));
                    }
                }
            }
            None => self.focus_seen = None,
        }
        for open in session.open_prs() {
            let Some(diff) = open.diff.as_ref().filter(|diff| diff.complete) else {
                continue;
            };
            let signature = (diff.generation, open.viewed.len());
            if self.recorded.get(&open.key) == Some(&signature) {
                continue;
            }
            self.recorded.insert(open.key.clone(), signature);
            changes.push(Change::Progress {
                key: open.key.clone(),
                head: open.pr.sha.to_string(),
                viewed: count(open.viewed.count(&open.files)),
                total: count(open.files.len()),
            });
        }
        self.recorded
            .retain(|key, _| session.open_prs().any(|open| &open.key == key));
        for change in changes {
            match change {
                Change::Seen(row) => self.mark_seen(&row, cx),
                Change::Progress {
                    key,
                    head,
                    viewed,
                    total,
                } => {
                    let progress = ViewedProgress {
                        head,
                        viewed,
                        total,
                    };
                    self.record_progress(&key, progress, settings.mark_reviewed_when_viewed, cx);
                }
            }
        }
    }

    /// Whether `row` is marked seen as it reads now.
    fn seen_as(&self, row: &PullRequest) -> bool {
        self.by_pr.get(&row.key).is_some_and(|mark| {
            !mark.unread
                && mark.seen.as_ref().is_some_and(|seen| {
                    seen.head == row.sha.as_ref()
                        && seen.comments == row.comments.unwrap_or(0)
                        && seen.reviews == row.review_count()
                })
        })
    }

    /// Takes the marks of `store`, the session's new store. Marks made in
    /// memory before they arrive win over the stored ones.
    fn attach(&mut self, store: Option<store::Store>, cx: &mut Context<Self>) {
        self.store.clone_from(&store);
        let Some(store) = store else {
            return;
        };
        for (key, mark) in &self.by_pr {
            store.put_mark(key, mark.clone()).detach();
        }
        let read = store.marks();
        cx.spawn(async move |this, cx| {
            let Ok(marks) = read.await else {
                return;
            };
            this.update(cx, |this, cx| {
                for (key, mark) in marks {
                    this.by_pr.entry(key).or_insert(mark);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

/// Marks for every row of the inbox, for screenshots and the inbox
/// benchmark: progress bars, unread dots and reviewed marks, in memory.
pub fn demo(cx: &mut App) {
    let marks = Marks::global(cx);
    let rows = Session::global(cx).read(cx).inbox.prs.clone();
    marks.update(cx, |marks, cx| {
        for (n, row) in rows.iter().enumerate() {
            let total = 4 + count(n % 20);
            let viewed = count(n * 7) % (total + 1);
            let head = row.sha.to_string();
            let mark = ReviewMark {
                seen: Some(Seen {
                    // Every third has a push since it was seen.
                    head: if n % 3 == 0 {
                        String::new()
                    } else {
                        head.clone()
                    },
                    comments: row.comments.unwrap_or(0),
                    reviews: row.review_count(),
                    at: Utc::now(),
                }),
                unread: false,
                reviewed: (n % 4 == 1).then(|| Reviewed {
                    head: head.clone(),
                    at: Utc::now(),
                }),
                progress: Some(ViewedProgress {
                    head,
                    viewed,
                    total,
                }),
            };
            marks.by_pr.insert(row.key.clone(), mark);
        }
        cx.notify();
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::Fixtures;

    fn row() -> PullRequest {
        let mut row = Fixtures::load().inbox.prs[0].clone();
        row.sha = "abc1234".into();
        row.comments = Some(3);
        row
    }

    fn seen(head: &str, comments: u32) -> Seen {
        Seen {
            head: head.to_owned(),
            comments,
            reviews: row().review_count(),
            at: Utc::now(),
        }
    }

    #[test]
    fn a_row_is_unread_after_a_push_a_comment_or_by_hand() {
        let row = row();
        assert_eq!(row_marks(None, &row), RowMarks::default(), "never opened");
        let mut mark = ReviewMark {
            seen: Some(seen("abc1234", 3)),
            ..ReviewMark::default()
        };
        assert!(!row_marks(Some(&mark), &row).unread);
        mark.seen = Some(seen("abc1234", 2));
        assert!(row_marks(Some(&mark), &row).unread, "a new comment");
        mark.seen = Some(seen("0ld0000", 3));
        assert!(row_marks(Some(&mark), &row).unread, "a new push");
        mark.seen = Some(seen("abc1234", 3));
        mark.unread = true;
        assert!(row_marks(Some(&mark), &row).unread, "by hand");
    }

    #[test]
    fn reviewed_and_progress_hold_for_the_head_they_were_set_at() {
        let row = row();
        let mark = ReviewMark {
            reviewed: Some(Reviewed {
                head: "abc1234".to_owned(),
                at: Utc::now(),
            }),
            progress: Some(ViewedProgress {
                head: "0ld0000".to_owned(),
                viewed: 12,
                total: 23,
            }),
            ..ReviewMark::default()
        };
        let marks = row_marks(Some(&mark), &row);
        assert_eq!(marks.reviewed, Some(ReviewedMark::Current));
        assert!(marks.settled());
        let progress = marks.progress.expect("progress");
        assert_eq!((progress.viewed, progress.total), (12, 23));
        assert!(!progress.current, "counted before the last push");
    }
}
