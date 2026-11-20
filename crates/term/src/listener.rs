use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoopSender, Msg};
use alacritty_terminal::term::ClipboardType;

use crate::ExitState;

pub(crate) type OnChange = Arc<dyn Fn() + Send + Sync>;

/// The most text a program may put on the clipboard at once (OSC 52); more
/// is dropped.
const CLIPBOARD_MAX: usize = 1 << 20;

/// State shared between the PTY thread (through [`Listener`]) and the UI.
pub(crate) struct Shared {
    /// True from a wakeup until the UI next reads the grid; further wakeups in
    /// that window are dropped so a flood of output wakes the UI once.
    dirty: AtomicBool,
    /// Counts every change notification, coalesced or not, so a painter can
    /// tell whether the screen may differ from the last frame.
    generation: AtomicU64,
    exit: Mutex<ExitState>,
    title: Mutex<Option<String>>,
    /// Text a program asked to put on the system clipboard (OSC 52), until
    /// the UI takes it.
    clipboard: Mutex<Option<String>>,
    on_change: Option<OnChange>,
    sender: OnceLock<EventLoopSender>,
    size: Mutex<WindowSize>,
}

impl Shared {
    pub(crate) fn new(on_change: Option<OnChange>, size: WindowSize) -> Self {
        Self {
            dirty: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            exit: Mutex::new(ExitState::Running),
            title: Mutex::new(None),
            clipboard: Mutex::new(None),
            on_change,
            sender: OnceLock::new(),
            size: Mutex::new(size),
        }
    }

    pub(crate) fn set_sender(&self, sender: EventLoopSender) {
        // Set once, right after the event loop is created.
        let _ = self.sender.set(sender);
    }

    pub(crate) fn sender(&self) -> Option<&EventLoopSender> {
        self.sender.get()
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    pub(crate) fn clear_dirty(&self) {
        self.dirty.store(false, Ordering::Release);
    }

    pub(crate) fn exit_state(&self) -> ExitState {
        *lock(&self.exit)
    }

    pub(crate) fn title(&self) -> Option<String> {
        lock(&self.title).clone()
    }

    pub(crate) fn take_clipboard(&self) -> Option<String> {
        lock(&self.clipboard).take()
    }

    /// Keeps `text` for the UI to put on the clipboard and always notifies,
    /// as [`Self::finish_and_wake`] does: a copy must not wait for output.
    fn store_clipboard(&self, text: String) {
        if text.len() > CLIPBOARD_MAX {
            return;
        }
        *lock(&self.clipboard) = Some(text);
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.dirty.store(true, Ordering::Release);
        if let Some(on_change) = &self.on_change {
            on_change();
        }
    }

    pub(crate) fn set_size(&self, size: WindowSize) {
        *lock(&self.size) = size;
    }

    fn wake(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        if !self.dirty.swap(true, Ordering::AcqRel)
            && let Some(on_change) = &self.on_change
        {
            on_change();
        }
    }

    fn write(&self, bytes: Vec<u8>) {
        if let Some(sender) = self.sender.get() {
            let _ = sender.send(Msg::Input(bytes.into()));
        }
    }

    /// Records the exit and always notifies, unlike [`Self::wake`]: a wakeup
    /// pending from the last output may already have been handled (its
    /// reader saw the program running) with the paint that clears `dirty`
    /// still to come, and a dropped notice here would leave the exit unseen.
    pub(crate) fn finish_and_wake(&self, status: Option<ExitStatus>) {
        self.finish(status);
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.dirty.store(true, Ordering::Release);
        if let Some(on_change) = &self.on_change {
            on_change();
        }
    }

    fn finish(&self, status: Option<ExitStatus>) {
        let mut exit = lock(&self.exit);
        match (*exit, status) {
            (ExitState::Running, status) => *exit = ExitState::Exited(status),
            (ExitState::Exited(None), Some(status)) => *exit = ExitState::Exited(Some(status)),
            (ExitState::Exited(_), _) => {}
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Receives the emulator's events on the PTY thread.
#[derive(Clone)]
pub struct Listener(pub(crate) Arc<Shared>);

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        let shared = &self.0;
        match event {
            Event::Wakeup | Event::Bell | Event::CursorBlinkingChange => shared.wake(),
            // Replies to queries (cursor position, device attributes). The
            // shell waits for these before it draws its prompt.
            Event::PtyWrite(text) => shared.write(text.into_bytes()),
            Event::TextAreaSizeRequest(format) => {
                let size = *lock(&shared.size);
                shared.write(format(size).into_bytes());
            }
            Event::Title(title) => {
                *lock(&shared.title) = Some(title);
                shared.wake();
            }
            Event::ResetTitle => {
                *lock(&shared.title) = None;
                shared.wake();
            }
            Event::ChildExit(status) => shared.finish_and_wake(Some(status)),
            Event::Exit => shared.finish_and_wake(None),
            // A program's copy (OSC 52), as Claude Code makes when text is
            // selected in it.
            Event::ClipboardStore(ClipboardType::Clipboard, text) => shared.store_clipboard(text),
            // The X11 primary selection, and reading the clipboard, which
            // would hand a program whatever the user copied elsewhere.
            Event::MouseCursorDirty
            | Event::ClipboardStore(ClipboardType::Selection, _)
            | Event::ClipboardLoad(..)
            | Event::ColorRequest(..) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    fn listener() -> (Listener, Arc<AtomicUsize>) {
        let woken = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&woken);
        let on_change: OnChange = Arc::new(move || {
            count.fetch_add(1, Ordering::SeqCst);
        });
        let shared = Shared::new(
            Some(on_change),
            WindowSize {
                num_lines: 24,
                num_cols: 80,
                cell_width: 8,
                cell_height: 16,
            },
        );
        (Listener(Arc::new(shared)), woken)
    }

    #[test]
    fn a_programs_copy_waits_for_the_ui_and_wakes_it_every_time() {
        let (listener, woken) = listener();
        let copy = |text: &str| Event::ClipboardStore(ClipboardType::Clipboard, text.to_owned());
        listener.send_event(copy("first"));
        listener.send_event(copy("second"));
        assert_eq!(woken.load(Ordering::SeqCst), 2, "no copy waits on output");
        assert_eq!(listener.0.take_clipboard().as_deref(), Some("second"));
        assert_eq!(listener.0.take_clipboard(), None, "taken once");
    }

    /// The bytes Claude Code prints to copy: OSC 52, base64, through the
    /// emulator's own parser.
    #[test]
    fn an_osc52_copy_reaches_the_clipboard_and_a_paste_request_is_ignored() {
        use alacritty_terminal::term::test::TermSize;
        use alacritty_terminal::term::{Config, Term};
        use alacritty_terminal::vte::ansi::Processor;

        let (listener, _) = listener();
        let shared = Arc::clone(&listener.0);
        let mut term = Term::new(Config::default(), &TermSize::new(80, 24), listener);
        let mut parser: Processor = Processor::new();
        // "hello" in base64, ended by BEL; then a request to read it back.
        parser.advance(&mut term, b"]52;c;aGVsbG8=]52;c;?");
        assert_eq!(shared.take_clipboard().as_deref(), Some("hello"));
        assert_eq!(
            shared.take_clipboard(),
            None,
            "the read request stores nothing"
        );
    }

    #[test]
    fn the_selection_and_huge_copies_are_dropped() {
        let (listener, _) = listener();
        listener.send_event(Event::ClipboardStore(ClipboardType::Selection, "x".into()));
        listener.send_event(Event::ClipboardStore(
            ClipboardType::Clipboard,
            "y".repeat(CLIPBOARD_MAX + 1),
        ));
        assert_eq!(listener.0.take_clipboard(), None);
    }
}
