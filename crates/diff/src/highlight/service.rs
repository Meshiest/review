//! A background highlighter: a small thread pool, a priority queue and a
//! cache of results by blob id.

use std::collections::{HashMap, HashSet, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;

use super::{Highlights, Language, highlight, stays_plain};
use crate::{BlobId, Text};

type Key = (BlobId, Language);

/// Cached results are dropped oldest first beyond this many bytes.
const CACHE_BYTES: usize = 128 * 1024 * 1024;

/// Called (on a worker thread) when a result lands. Returns false to be
/// dropped from the list, e.g. when its receiver is gone.
pub type Wake = Box<dyn Fn() -> bool + Send + Sync>;

struct Job {
    text: Text,
    /// Also wanted at low priority, so leaving the visible set keeps it.
    background: bool,
}

#[derive(Default)]
struct State {
    cache: HashMap<Key, Arc<Highlights>>,
    /// Insertion order of `cache`, for eviction.
    order: VecDeque<Key>,
    cached_bytes: usize,
    pending: HashMap<Key, Job>,
    running: HashSet<Key>,
    /// Wanted now, most important first.
    visible: Vec<Key>,
    background: VecDeque<Key>,
    shutdown: bool,
}

impl State {
    fn insert(&mut self, key: Key, value: Arc<Highlights>) {
        self.cached_bytes += value.byte_size();
        if self.cache.insert(key, value).is_none() {
            self.order.push_back(key);
        }
        while self.cached_bytes > CACHE_BYTES
            && let Some(old) = self.order.pop_front()
        {
            if let Some(gone) = self.cache.remove(&old) {
                self.cached_bytes = self.cached_bytes.saturating_sub(gone.byte_size());
            }
        }
    }

    /// Queues `text` unless it is cached, queued or running; oversize blobs
    /// are answered at once with plain highlights.
    fn admit(&mut self, text: Text, language: Language, background: bool) {
        let key = (text.id(), language);
        if self.cache.contains_key(&key) || self.running.contains(&key) {
            return;
        }
        if stays_plain(&text) {
            self.insert(key, Arc::new(Highlights::plain()));
            return;
        }
        match self.pending.get_mut(&key) {
            Some(job) => job.background |= background,
            None => {
                self.pending.insert(key, Job { text, background });
            }
        }
        if background {
            self.background.push_back(key);
        }
    }

    fn take_next(&mut self) -> Option<(Key, Text)> {
        let key = self
            .visible
            .iter()
            .copied()
            .find(|key| self.pending.contains_key(key))
            .or_else(|| {
                while let Some(key) = self.background.pop_front() {
                    if self.pending.contains_key(&key) {
                        return Some(key);
                    }
                }
                None
            })?;
        let job = self.pending.remove(&key)?;
        self.running.insert(key);
        Some((key, job.text))
    }
}

struct Shared {
    state: Mutex<State>,
    work: Condvar,
    wakes: Mutex<Vec<Wake>>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        // A panic in a worker cannot leave the state half-updated: results
        // are inserted in one call.
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn wake_listeners(&self) {
        let mut wakes = self
            .wakes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        wakes.retain(|wake| wake());
    }
}

/// Highlights blobs on background threads and caches the results by blob id.
///
/// The UI asks for what it shows with [`Highlighter::set_visible`] every time
/// that set changes, reads results with [`Highlighter::get`] (never blocking
/// on a parse) and is told through [`Highlighter::on_ready`] when something
/// new has landed. Clones share the workers and the cache.
#[derive(Clone)]
pub struct Highlighter {
    shared: Arc<Shared>,
    /// Dropping the last handle stops the workers.
    _guard: Arc<Guard>,
}

struct Guard(Arc<Shared>);

impl Drop for Guard {
    fn drop(&mut self) {
        self.0.lock().shutdown = true;
        self.0.work.notify_all();
    }
}

impl Highlighter {
    /// A highlighter with `workers` threads (at least one).
    #[must_use]
    pub fn new(workers: usize) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::default(),
            work: Condvar::new(),
            wakes: Mutex::default(),
        });
        for ix in 0..workers.max(1) {
            let shared = shared.clone();
            // A failed spawn leaves fewer workers; with none, text stays plain.
            let _ = thread::Builder::new()
                .name(format!("highlight-{ix}"))
                .spawn(move || worker(&shared));
        }
        Self {
            _guard: Arc::new(Guard(shared.clone())),
            shared,
        }
    }

    /// Half the cores, at most four, so scrolling keeps the rest.
    #[must_use]
    pub fn with_default_workers() -> Self {
        let cores = thread::available_parallelism().map_or(2, usize::from);
        Self::new((cores / 2).clamp(1, 4))
    }

    /// The cached highlights, if they are ready. Never waits for a parse.
    pub fn get(&self, id: BlobId, language: Language) -> Option<Arc<Highlights>> {
        self.shared.lock().cache.get(&(id, language)).cloned()
    }

    /// Replaces the set of blobs wanted on screen, most important first.
    /// Queued blobs that are no longer visible are dropped unless they were
    /// also requested with [`Highlighter::prefetch`].
    pub fn set_visible(&self, wanted: &[(Text, Language)]) {
        let mut state = self.shared.lock();
        let old = std::mem::take(&mut state.visible);
        state.visible = wanted
            .iter()
            .map(|(text, language)| (text.id(), *language))
            .collect();
        for key in old {
            if !state.visible.contains(&key)
                && state.pending.get(&key).is_some_and(|job| !job.background)
            {
                state.pending.remove(&key);
            }
        }
        for (text, language) in wanted {
            state.admit(text.clone(), *language, false);
        }
        drop(state);
        self.shared.work.notify_all();
    }

    /// Queues blobs at low priority, behind everything visible.
    pub fn prefetch(&self, blobs: impl IntoIterator<Item = (Text, Language)>) {
        let mut state = self.shared.lock();
        for (text, language) in blobs {
            state.admit(text, language, true);
        }
        drop(state);
        self.shared.work.notify_all();
    }

    /// Highlights on the calling thread (or returns the cached result).
    /// For tools and tests; the UI must use [`Highlighter::set_visible`].
    pub fn highlight_now(&self, text: &Text, language: Language) -> Arc<Highlights> {
        let key = (text.id(), language);
        if let Some(done) = self.get(text.id(), language) {
            return done;
        }
        let result = Arc::new(highlight(text, language));
        let mut state = self.shared.lock();
        // Another thread may have finished the same blob meanwhile; keep the
        // first so every caller sees one `Arc`.
        if let Some(done) = state.cache.get(&key) {
            return done.clone();
        }
        state.insert(key, result.clone());
        result
    }

    /// Registers a callback for when new results are cached.
    pub fn on_ready(&self, wake: impl Fn() -> bool + Send + Sync + 'static) {
        self.shared
            .wakes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(Box::new(wake));
    }

    /// True when nothing is queued or running.
    pub fn is_idle(&self) -> bool {
        let state = self.shared.lock();
        state.pending.is_empty() && state.running.is_empty()
    }
}

fn worker(shared: &Shared) {
    loop {
        let (key, text) = {
            let mut state = shared.lock();
            loop {
                if state.shutdown {
                    return;
                }
                if let Some(next) = state.take_next() {
                    break next;
                }
                state = shared
                    .work
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        };
        // A grammar bug must not take the pool down; the blob stays plain.
        let result = catch_unwind(AssertUnwindSafe(|| highlight(&text, key.1)))
            .unwrap_or_else(|_| Highlights::plain());
        {
            let mut state = shared.lock();
            state.running.remove(&key);
            state.insert(key, Arc::new(result));
        }
        shared.wake_listeners();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use super::*;
    use crate::HighlightKind;

    fn wait_for(highlighter: &Highlighter, text: &Text, language: Language) -> Arc<Highlights> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(done) = highlighter.get(text.id(), language) {
                return done;
            }
            assert!(Instant::now() < deadline, "highlights never arrived");
            thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn same_blob_id_gives_the_same_arc() {
        let highlighter = Highlighter::new(1);
        let text = Text::new("fn main() {}\n");
        let first = highlighter.highlight_now(&text, Language::Rust);
        assert!(!first.is_plain());
        let again = highlighter.highlight_now(&text, Language::Rust);
        assert!(Arc::ptr_eq(&first, &again));
        assert!(Arc::ptr_eq(
            &first,
            &highlighter.get(text.id(), Language::Rust).unwrap()
        ));

        // The same id with the same content in a new `Text` still hits.
        let same_id = Text::with_id(text.id(), "fn main() {}\n");
        assert!(Arc::ptr_eq(
            &first,
            &highlighter.highlight_now(&same_id, Language::Rust)
        ));
        // Another blob does not.
        let other = Text::new("fn main() {}\n");
        assert!(!Arc::ptr_eq(
            &first,
            &highlighter.highlight_now(&other, Language::Rust)
        ));
    }

    #[test]
    fn background_results_land_in_the_cache_and_wake_listeners() {
        let highlighter = Highlighter::new(2);
        let woke = Arc::new(AtomicUsize::new(0));
        let counter = woke.clone();
        highlighter.on_ready(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            true
        });
        let text = Text::new("let s = \"a\nb\";\n");
        highlighter.set_visible(&[(text.clone(), Language::Rust)]);
        let done = wait_for(&highlighter, &text, Language::Rust);
        assert!(done.line(1).iter().any(|s| s.kind == HighlightKind::String));
        let deadline = Instant::now() + Duration::from_secs(5);
        while woke.load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < deadline, "no wake");
            thread::sleep(Duration::from_millis(1));
        }
        // The cached result is what `get` keeps returning.
        assert!(Arc::ptr_eq(
            &done,
            &highlighter.get(text.id(), Language::Rust).unwrap()
        ));
    }

    #[test]
    fn oversize_blobs_are_answered_plain_without_queueing() {
        let highlighter = Highlighter::new(1);
        let big = Text::new("let a = 1;\n".repeat(super::super::MAX_LINES as usize + 1));
        highlighter.set_visible(&[(big.clone(), Language::Rust)]);
        let done = highlighter
            .get(big.id(), Language::Rust)
            .expect("cached at once");
        assert!(done.is_plain());
        assert!(highlighter.is_idle());
    }

    #[test]
    fn visible_blobs_run_before_background_ones() {
        let mut state = State::default();
        let (background, visible, dropped) = (
            Text::new(
                "fn b() {}
",
            ),
            Text::new(
                "fn c() {}
",
            ),
            Text::new(
                "fn d() {}
",
            ),
        );
        state.admit(background.clone(), Language::Rust, true);
        state.admit(visible.clone(), Language::Rust, false);
        state.visible = vec![(visible.id(), Language::Rust)];
        // Queued as visible once, no longer wanted: not in `visible`, not
        // background, so it is only found through `pending`.
        state.admit(dropped.clone(), Language::Rust, false);

        let (first, _) = state.take_next().expect("a job");
        assert_eq!(first.0, visible.id());
        let (second, _) = state.take_next().expect("a job");
        assert_eq!(second.0, background.id());
        assert!(state.take_next().is_none(), "unwanted work is never picked");
    }
}
