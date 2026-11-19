//! The off-thread half: a worker that parses what arrives and appends it.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard};
use std::thread;
use std::time::{Duration, Instant};

use async_channel::{Receiver as WakeReceiver, Sender as WakeSender};

use crate::buffer::LogBuffer;
use crate::parser::LogParser;

/// Input one lock hold may append: a bigger backlog is appended in several
/// holds, so a reader never waits long.
const FLUSH_BYTES: usize = 1 << 20;

pub(crate) struct Shared {
    buffer: RwLock<LogBuffer>,
    /// Chunks sent and not yet appended.
    queued: AtomicUsize,
    wake: WakeSender<()>,
    wake_rx: WakeReceiver<()>,
}

impl Shared {
    pub fn read(&self) -> RwLockReadGuard<'_, LogBuffer> {
        self.buffer.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// Asks the view for a frame. A full channel already has a wake-up
    /// pending, so a failed send loses nothing.
    pub fn wake(&self) {
        let _ = self.wake.try_send(());
    }

    pub fn wake_receiver(&self) -> WakeReceiver<()> {
        self.wake_rx.clone()
    }
}

enum Message {
    Data(Vec<u8>),
    Finish,
}

/// A log being written to. Chunks pushed from any thread are parsed on a
/// worker thread and appended to a buffer that [`crate::LogView`]s read.
/// Dropping the feed ends the log: what is left of the last line is kept.
pub struct LogFeed {
    sender: Sender<Message>,
    shared: Arc<Shared>,
}

impl Default for LogFeed {
    fn default() -> Self {
        Self::new()
    }
}

impl LogFeed {
    /// A feed over a buffer with the standard limit of 1,000,000 lines.
    #[must_use]
    pub fn new() -> Self {
        Self::with_buffer(LogBuffer::new())
    }

    /// A feed over `buffer`, which sets the limits.
    #[must_use]
    pub fn with_buffer(buffer: LogBuffer) -> Self {
        let (wake, wake_rx) = async_channel::bounded(1);
        let shared = Arc::new(Shared {
            buffer: RwLock::new(buffer),
            queued: AtomicUsize::new(0),
            wake,
            wake_rx,
        });
        let (sender, receiver) = mpsc::channel();
        let worker = Arc::clone(&shared);
        thread::Builder::new()
            .name("log-parser".into())
            .spawn(move || run(&receiver, &worker))
            .expect("the OS can start a thread");
        Self { sender, shared }
    }

    pub(crate) fn shared(&self) -> Arc<Shared> {
        Arc::clone(&self.shared)
    }

    /// Queues a chunk of the log and returns at once. Chunks may split lines
    /// and escape sequences anywhere.
    pub fn push(&self, bytes: impl Into<Vec<u8>>) {
        self.shared.queued.fetch_add(1, Ordering::AcqRel);
        if self.sender.send(Message::Data(bytes.into())).is_err() {
            self.shared.queued.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// Marks the end of the log: a last line without a newline is kept.
    pub fn finish(&self) {
        self.shared.queued.fetch_add(1, Ordering::AcqRel);
        if self.sender.send(Message::Finish).is_err() {
            self.shared.queued.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// True when every chunk pushed so far is in the buffer.
    #[must_use]
    pub fn is_settled(&self) -> bool {
        self.shared.queued.load(Ordering::Acquire) == 0
    }

    /// Blocks until [`LogFeed::is_settled`] or `timeout`; for tests and
    /// benchmarks, never for a UI thread.
    pub fn wait_settled(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while !self.is_settled() {
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(1));
        }
        true
    }

    /// Reads the buffer. The lock is held for `f`, so keep it short.
    pub fn read<R>(&self, f: impl FnOnce(&LogBuffer) -> R) -> R {
        f(&self.shared.read())
    }
}

fn run(receiver: &Receiver<Message>, shared: &Shared) {
    let mut parser = LogParser::new();
    while let Ok(first) = receiver.recv() {
        let mut taken = 0;
        let mut bytes = 0;
        let mut next = Some(first);
        while let Some(message) = next {
            taken += 1;
            match message {
                Message::Data(data) => {
                    bytes += data.len();
                    parser.feed(&data);
                }
                Message::Finish => parser.finish(),
            }
            next = if bytes < FLUSH_BYTES {
                receiver.try_recv().ok()
            } else {
                None
            };
        }
        append(&mut parser, shared);
        shared.queued.fetch_sub(taken, Ordering::AcqRel);
    }
    parser.finish();
    append(&mut parser, shared);
}

fn append(parser: &mut LogParser, shared: &Shared) {
    let mut batch = parser.take();
    if batch.is_empty() {
        return;
    }
    shared
        .buffer
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .push(&mut batch);
    shared.wake();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_pushed_from_here_arrive_parsed() {
        let feed = LogFeed::new();
        feed.push(&b"first\nsec"[..]);
        feed.push(&b"ond\n\x1b[3"[..]);
        feed.push(&b"1mred\x1b[0m\n"[..]);
        assert!(feed.wait_settled(Duration::from_secs(10)));
        let texts: Vec<String> =
            feed.read(|b| b.lines(0..b.total()).map(|l| l.text.to_owned()).collect());
        assert_eq!(texts, ["first", "second", "red"]);
    }

    #[test]
    fn finish_keeps_the_unterminated_last_line() {
        let feed = LogFeed::new();
        feed.push(&b"a\nb"[..]);
        feed.finish();
        assert!(feed.wait_settled(Duration::from_secs(10)));
        assert_eq!(feed.read(LogBuffer::len), 2);
    }

    #[test]
    fn a_wake_up_is_sent_once_for_many_appends() {
        let feed = LogFeed::new();
        let wake = feed.shared().wake_receiver();
        for _ in 0..50 {
            feed.push(&b"x\n"[..]);
        }
        assert!(feed.wait_settled(Duration::from_secs(10)));
        assert_eq!(wake.len(), 1);
    }
}
