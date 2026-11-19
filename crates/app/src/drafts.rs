//! Writes drafts to the store without touching the UI thread.
//!
//! Every edit is sent here; the writer thread keeps the newest text per
//! draft and writes it once typing pauses for [`DEBOUNCE`], or at the latest
//! [`MAX_DELAY`] after the first unsaved edit, so a long burst of typing is
//! never more than a second behind. A killed process loses at most that
//! window. Deleting and queuing go through the same thread, so a pending
//! edit can never be written after the draft it belongs to was removed.

use std::collections::HashMap;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use forge::model::Sha;
use futures::channel::oneshot;
use store::{DraftAnchor, NewQueued, PrKey, QueuedComment, Store};

/// Typing must pause this long before a draft is written.
pub const DEBOUNCE: Duration = Duration::from_millis(150);
/// A draft that keeps changing is still written this often.
pub const MAX_DELAY: Duration = Duration::from_millis(1000);

type Key = (PrKey, DraftAnchor);

/// The result of a store write, awaitable from the UI thread.
pub type Reply<T> = oneshot::Receiver<store::Result<T>>;

enum Msg {
    Edit {
        key: Key,
        body: String,
        commit: Option<Sha>,
    },
    Delete {
        key: Key,
        reply: oneshot::Sender<store::Result<()>>,
    },
    Queue {
        pr: PrKey,
        new: NewQueued,
        reply: oneshot::Sender<store::Result<QueuedComment>>,
    },
    FinishReview {
        pr: PrKey,
        reply: oneshot::Sender<store::Result<()>>,
    },
    Flush(mpsc::Sender<()>),
}

struct Unsaved {
    body: String,
    commit: Option<Sha>,
    /// When the newest edit lands in the store.
    due: Instant,
    /// The latest it may wait, from the first edit since the last write.
    deadline: Instant,
}

/// Handle to the writer thread. Cheap to clone; the thread ends, after
/// writing what is pending, when the last clone is dropped.
#[derive(Clone)]
pub struct DraftSaver {
    tx: mpsc::Sender<Msg>,
}

impl DraftSaver {
    /// Starts the writer thread for `store`.
    #[must_use]
    pub fn spawn(store: Store) -> Self {
        Self::spawn_with(store, DEBOUNCE, MAX_DELAY)
    }

    #[must_use]
    pub fn spawn_with(store: Store, debounce: Duration, max_delay: Duration) -> Self {
        let (tx, rx) = mpsc::channel();
        let spawned = thread::Builder::new()
            .name("draft-saver".to_owned())
            .spawn(move || run(&store, &rx, debounce, max_delay));
        if let Err(error) = spawned {
            eprintln!("review: could not start the draft saver: {error}");
        }
        Self { tx }
    }

    /// The text of a draft changed. An empty text removes the draft.
    pub fn edit(&self, pr: &PrKey, anchor: &DraftAnchor, body: String, commit: Option<Sha>) {
        let _ = self.tx.send(Msg::Edit {
            key: (pr.clone(), anchor.clone()),
            body,
            commit,
        });
    }

    /// Removes the draft, dropping any edit still waiting to be written.
    pub fn delete(&self, pr: &PrKey, anchor: &DraftAnchor) -> Reply<()> {
        let (reply, rx) = oneshot::channel();
        let _ = self.tx.send(Msg::Delete {
            key: (pr.clone(), anchor.clone()),
            reply,
        });
        rx
    }

    /// Moves the draft at `new.anchor` into the pending review with the
    /// text it was sent with.
    pub fn queue(&self, pr: &PrKey, new: NewQueued) -> Reply<QueuedComment> {
        let (reply, rx) = oneshot::channel();
        let _ = self.tx.send(Msg::Queue {
            pr: pr.clone(),
            new,
            reply,
        });
        rx
    }

    /// The review went out: drops its queued comments and summary draft.
    pub fn finish_review(&self, pr: &PrKey) -> Reply<()> {
        let (reply, rx) = oneshot::channel();
        let _ = self.tx.send(Msg::FinishReview {
            pr: pr.clone(),
            reply,
        });
        rx
    }

    /// Writes everything pending now and waits for it. For quitting and
    /// tests; never call it from a frame.
    pub fn flush(&self) {
        let (done, wait) = mpsc::channel();
        if self.tx.send(Msg::Flush(done)).is_ok() {
            let _ = wait.recv();
        }
    }
}

fn run(store: &Store, rx: &mpsc::Receiver<Msg>, debounce: Duration, max_delay: Duration) {
    let mut unsaved: HashMap<Key, Unsaved> = HashMap::new();
    loop {
        let wait = unsaved
            .values()
            .map(|entry| entry.due.min(entry.deadline))
            .min()
            .map(|at| at.saturating_duration_since(Instant::now()));
        let received = match wait {
            Some(wait) => rx.recv_timeout(wait),
            None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match received {
            Ok(Msg::Edit { key, body, commit }) => {
                let now = Instant::now();
                let entry = unsaved.entry(key).or_insert_with(|| Unsaved {
                    body: String::new(),
                    commit: None,
                    due: now,
                    deadline: now + max_delay,
                });
                entry.body = body;
                entry.commit = commit;
                entry.due = now + debounce;
            }
            Ok(Msg::Delete { key, reply }) => {
                unsaved.remove(&key);
                let _ = reply.send(store.delete_draft(&key.0, &key.1).wait());
            }
            Ok(Msg::Queue { pr, new, reply }) => {
                unsaved.remove(&(pr.clone(), new.anchor.clone()));
                let _ = reply.send(store.queue_comment(&pr, new).wait());
            }
            Ok(Msg::FinishReview { pr, reply }) => {
                unsaved.retain(|(unsaved_pr, anchor), _| {
                    !(unsaved_pr == &pr && *anchor == DraftAnchor::Review)
                });
                let _ = reply.send(store.finish_review(&pr).wait());
            }
            Ok(Msg::Flush(done)) => {
                write_due(store, &mut unsaved, None);
                let _ = done.send(());
            }
            Err(RecvTimeoutError::Timeout) => write_due(store, &mut unsaved, Some(Instant::now())),
            Err(RecvTimeoutError::Disconnected) => {
                write_due(store, &mut unsaved, None);
                return;
            }
        }
    }
}

/// Writes the entries due at `now`, or all of them without a time.
fn write_due(store: &Store, unsaved: &mut HashMap<Key, Unsaved>, now: Option<Instant>) {
    let due: Vec<Key> = unsaved
        .iter()
        .filter(|(_, entry)| now.is_none_or(|now| entry.due <= now || entry.deadline <= now))
        .map(|(key, _)| key.clone())
        .collect();
    for key in due {
        let Some(entry) = unsaved.remove(&key) else {
            continue;
        };
        let (pr, anchor) = &key;
        let written = if entry.body.trim().is_empty() {
            store.delete_draft(pr, anchor).wait()
        } else {
            store
                .put_draft(pr, anchor, &entry.body, entry.commit.as_ref())
                .wait()
        };
        if let Err(error) = written {
            eprintln!("review: could not save a draft: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge::model::{LineRef, PrNumber, RepoRef, Side};

    fn key() -> PrKey {
        PrKey::new(RepoRef::new("github.com", "acme", "widgets"), PrNumber(4))
    }

    fn line() -> DraftAnchor {
        DraftAnchor::Line {
            path: "a.rs".into(),
            line: LineRef {
                side: Side::New,
                line: 3,
            },
            start: None,
        }
    }

    fn open(name: &str) -> (Store, std::path::PathBuf) {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.scratch/draft-tests")
            .join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(dir.join("cache.db")).expect("open store");
        (store, dir)
    }

    #[test]
    fn the_newest_text_wins_and_flush_writes_it() {
        let (store, dir) = open("newest");
        let saver = DraftSaver::spawn_with(
            store.clone(),
            Duration::from_secs(3600),
            Duration::from_secs(3600),
        );
        for text in ["h", "he", "hel"] {
            saver.edit(&key(), &line(), text.to_owned(), None);
        }
        // Nothing is due for an hour, so only a flush can have written it.
        assert_eq!(store.draft(&key(), &line()).wait().unwrap(), None);
        saver.flush();
        let draft = store.draft(&key(), &line()).wait().unwrap().unwrap();
        assert_eq!(draft.body, "hel");
        drop((saver, store));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_delete_cancels_the_edit_before_it() {
        let (store, dir) = open("delete");
        let saver = DraftSaver::spawn_with(
            store.clone(),
            Duration::from_secs(3600),
            Duration::from_secs(3600),
        );
        saver.edit(&key(), &line(), "typed".to_owned(), None);
        futures::executor::block_on(saver.delete(&key(), &line()))
            .unwrap()
            .unwrap();
        saver.flush();
        assert_eq!(store.draft(&key(), &line()).wait().unwrap(), None);
        drop((saver, store));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_emptied_draft_is_removed_and_queuing_moves_the_text() {
        let (store, dir) = open("queue");
        let saver = DraftSaver::spawn_with(store.clone(), Duration::ZERO, Duration::ZERO);
        saver.edit(&key(), &line(), "x".to_owned(), None);
        saver.flush();
        assert!(store.draft(&key(), &line()).wait().unwrap().is_some());
        saver.edit(&key(), &line(), "  ".to_owned(), None);
        saver.flush();
        assert_eq!(store.draft(&key(), &line()).wait().unwrap(), None);

        saver.edit(&key(), &line(), "final".to_owned(), None);
        let new = NewQueued {
            anchor: line(),
            body: "final".to_owned(),
            commit: None,
            forge_id: None,
            thread: None,
        };
        let queued = futures::executor::block_on(saver.queue(&key(), new))
            .unwrap()
            .unwrap();
        assert_eq!(queued.body, "final");
        saver.flush();
        assert_eq!(store.draft(&key(), &line()).wait().unwrap(), None);
        assert_eq!(store.queued_for(&key()).wait().unwrap().len(), 1);
        drop((saver, store));
        let _ = std::fs::remove_dir_all(dir);
    }
}
