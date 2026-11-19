//! Finding text in the log, off the UI thread.

use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;

use memchr::memmem::Finder;

use crate::buffer::LogBuffer;
use crate::feed::Shared;

/// Most matches kept for stepping through; the count goes on past it.
pub const MAX_MATCHES: usize = 100_000;

/// Lines scanned per read-lock hold.
const SCAN_LINES: u64 = 4096;

/// Where a query matched: line `line`, bytes `start..end` of its text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Match {
    pub line: u64,
    pub start: u32,
    pub end: u32,
}

/// What to look for. Smart case: lowercase text matches any case, text with
/// an uppercase letter matches only itself.
#[derive(Clone, Debug)]
pub struct Query {
    needle: String,
    case_sensitive: bool,
}

impl Query {
    /// `None` for an empty query.
    #[must_use]
    pub fn new(text: &str) -> Option<Self> {
        if text.is_empty() {
            return None;
        }
        let case_sensitive = text.chars().any(char::is_uppercase);
        Some(Self {
            needle: if case_sensitive {
                text.to_owned()
            } else {
                text.to_ascii_lowercase()
            },
            case_sensitive,
        })
    }
}

/// Scans lines for a query. Case-insensitive matching folds ASCII only, so
/// the folded copy has the same byte offsets as the line.
struct Scanner<'a> {
    query: &'a Query,
    finder: Finder<'a>,
    folded: String,
}

impl<'a> Scanner<'a> {
    fn new(query: &'a Query) -> Self {
        Self {
            query,
            finder: Finder::new(query.needle.as_bytes()),
            folded: String::new(),
        }
    }

    fn line(&mut self, id: u64, text: &str, out: &mut Vec<Match>) {
        let hay = if self.query.case_sensitive {
            text
        } else {
            self.folded.clear();
            self.folded.push_str(text);
            self.folded.make_ascii_lowercase();
            &self.folded
        };
        let len = self.query.needle.len();
        out.extend(self.finder.find_iter(hay.as_bytes()).map(|at| Match {
            line: id,
            start: at as u32,
            end: (at + len) as u32,
        }));
    }
}

/// All matches in the lines of `range` that are still held, in order.
#[must_use]
pub fn find_all(buffer: &LogBuffer, query: &Query, range: Range<u64>) -> Vec<Match> {
    let mut out = Vec::new();
    let mut scanner = Scanner::new(query);
    for line in buffer.lines(range) {
        scanner.line(line.id, line.text, &mut out);
    }
    out
}

/// The matches found so far.
#[derive(Default)]
pub(crate) struct MatchSet {
    /// In order, at most [`MAX_MATCHES`].
    pub matches: Vec<Match>,
    /// Matches found, including those past the cap.
    pub total: usize,
    /// Lines before this one were scanned.
    pub scanned: u64,
    /// A thread is scanning.
    pub running: bool,
}

/// A search over the log: the query, and the matches a thread has found.
pub(crate) struct Search {
    query: Query,
    set: Arc<Mutex<MatchSet>>,
    cancel: Arc<AtomicBool>,
}

impl Search {
    pub fn new(text: &str) -> Option<Self> {
        Some(Self {
            query: Query::new(text)?,
            set: Arc::default(),
            cancel: Arc::default(),
        })
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut MatchSet) -> R) -> R {
        f(&mut self.set.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// Starts a thread over the lines not scanned yet, unless one is running
    /// or there are none. `total` is the log's length: the caller knows it,
    /// and may hold the buffer's lock, which must not be taken again here.
    pub fn extend(&self, shared: &Arc<Shared>, total: u64) {
        let Some(from) = self.with(|s| (!s.running).then_some(s.scanned)) else {
            return;
        };
        if from >= total {
            return;
        }
        self.with(|s| s.running = true);
        let (query, set, cancel, shared) = (
            self.query.clone(),
            Arc::clone(&self.set),
            Arc::clone(&self.cancel),
            Arc::clone(shared),
        );
        thread::Builder::new()
            .name("log-search".into())
            .spawn(move || scan_thread(&query, &set, &cancel, &shared))
            .expect("the OS can start a thread");
    }
}

impl Drop for Search {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}

fn scan_thread(query: &Query, set: &Mutex<MatchSet>, cancel: &AtomicBool, shared: &Shared) {
    let mut scanner = Scanner::new(query);
    let mut found = Vec::new();
    loop {
        let from = set.lock().unwrap_or_else(PoisonError::into_inner).scanned;
        found.clear();
        let end = {
            let buffer = shared.read();
            let from = from.max(buffer.dropped());
            let end = (from + SCAN_LINES).min(buffer.total());
            for line in buffer.lines(from..end) {
                scanner.line(line.id, line.text, &mut found);
            }
            end.max(from)
        };
        let mut set = set.lock().unwrap_or_else(PoisonError::into_inner);
        let done = end <= from || cancel.load(Ordering::Acquire);
        set.total += found.len();
        let room = MAX_MATCHES.saturating_sub(set.matches.len());
        set.matches.extend(found.iter().take(room));
        set.scanned = set.scanned.max(end);
        if done {
            set.running = false;
        }
        drop(set);
        shared.wake();
        if done {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LogParser;

    fn buffer(text: &str) -> LogBuffer {
        let mut buffer = LogBuffer::new();
        buffer.append(&mut LogParser::new(), text.as_bytes());
        buffer
    }

    fn found(text: &str, query: &str) -> Vec<(u64, u32, u32)> {
        let buffer = buffer(text);
        let query = Query::new(query).expect("not empty");
        find_all(&buffer, &query, 0..buffer.total())
            .into_iter()
            .map(|m| (m.line, m.start, m.end))
            .collect()
    }

    #[test]
    fn every_occurrence_is_a_match_with_byte_offsets() {
        assert_eq!(
            found("abc abc\nxx\nabcabc\n", "abc"),
            [(0, 0, 3), (0, 4, 7), (2, 0, 3), (2, 3, 6)]
        );
    }

    #[test]
    fn lowercase_queries_ignore_case_and_uppercase_ones_do_not() {
        let text = "Error: boom\nerror again\nERROR\n";
        assert_eq!(found(text, "error").len(), 3);
        assert_eq!(found(text, "Error"), [(0, 0, 5)]);
    }

    #[test]
    fn colors_and_timestamps_are_not_searched() {
        let text = "2024-05-01T12:00:00Z \x1b[31mred\x1b[0m 12:00:00\n";
        assert_eq!(found(text, "red"), [(0, 0, 3)]);
        assert_eq!(found(text, "31m"), []);
        assert_eq!(found(text, "2024"), []);
    }

    #[test]
    fn matches_past_unicode_keep_their_byte_offsets() {
        assert_eq!(found("caf\u{e9} x\n", "x"), [(0, 6, 7)]);
    }

    #[test]
    fn the_range_limits_the_lines() {
        let buffer = buffer("a\na\na\na\n");
        let query = Query::new("a").expect("not empty");
        let lines: Vec<u64> = find_all(&buffer, &query, 1..3)
            .iter()
            .map(|m| m.line)
            .collect();
        assert_eq!(lines, [1, 2]);
    }

    #[test]
    fn an_empty_query_is_no_query() {
        assert!(Query::new("").is_none());
    }
}
