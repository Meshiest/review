//! A cancel flag for the git work of one thread: every git command started
//! inside [`scope`] (fetches too) is killed, with what it spawned, once the
//! flag is set, and fails with [`crate::Error::GitCanceled`].

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

thread_local! {
    static CURRENT: RefCell<Option<Arc<AtomicBool>>> = const { RefCell::new(None) };
}

/// Runs `work` with `flag` as this thread's cancel flag, then puts the one
/// before back.
pub fn scope<R>(flag: Arc<AtomicBool>, work: impl FnOnce() -> R) -> R {
    struct Restore(Option<Arc<AtomicBool>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let previous = self.0.take();
            CURRENT.with(|current| *current.borrow_mut() = previous);
        }
    }
    let previous = CURRENT.with(|current| current.borrow_mut().replace(flag));
    let _restore = Restore(previous);
    work()
}

/// Whether this thread's work was canceled.
#[must_use]
pub fn canceled() -> bool {
    current().is_some_and(|flag| flag.load(Ordering::Relaxed))
}

/// This thread's cancel flag, inside [`scope`].
pub(crate) fn current() -> Option<Arc<AtomicBool>> {
    CURRENT.with(|current| current.borrow().clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_flag_holds_inside_its_scope_only() {
        assert!(!canceled());
        let flag = Arc::new(AtomicBool::new(false));
        scope(Arc::clone(&flag), || {
            assert!(!canceled());
            flag.store(true, Ordering::Relaxed);
            assert!(canceled());
            let other = std::thread::spawn(canceled).join().unwrap();
            assert!(!other, "another thread has its own");
        });
        assert!(!canceled());
        assert!(current().is_none());
    }
}
