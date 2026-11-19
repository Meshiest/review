use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoopSender, Msg};

use crate::ExitState;

pub(crate) type OnChange = Arc<dyn Fn() + Send + Sync>;

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
            Event::MouseCursorDirty
            | Event::ClipboardStore(..)
            | Event::ClipboardLoad(..)
            | Event::ColorRequest(..) => {}
        }
    }
}
