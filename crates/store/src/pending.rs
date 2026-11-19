use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::sync::oneshot;

use crate::error::{Error, Result};

/// A result that will arrive from a store thread. Poll it with
/// [`try_take`](Self::try_take) each frame, or await it in `cx.spawn`; the
/// UI thread never blocks on it.
///
/// A write queued before the `Pending` is dropped still happens; a queued
/// read that nobody waits for any more is skipped.
#[must_use = "a dropped read is skipped; call detach() on a write you do not await"]
pub struct Pending<T> {
    rx: oneshot::Receiver<Result<T>>,
}

impl<T> Pending<T> {
    pub(crate) fn channel() -> (oneshot::Sender<Result<T>>, Self) {
        let (tx, rx) = oneshot::channel();
        (tx, Self { rx })
    }

    /// The result if it has arrived, without waiting.
    pub fn try_take(&mut self) -> Option<Result<T>> {
        match self.rx.try_recv() {
            Ok(result) => Some(result),
            Err(oneshot::error::TryRecvError::Empty) => None,
            Err(oneshot::error::TryRecvError::Closed) => Some(Err(Error::Shutdown)),
        }
    }

    /// Blocks the calling thread until the result arrives. For tests,
    /// startup code and worker threads; never the UI thread, and not from
    /// inside an async task.
    pub fn wait(self) -> Result<T> {
        self.rx.blocking_recv().unwrap_or(Err(Error::Shutdown))
    }

    /// Declares that the result is not needed (a fire-and-forget write).
    pub fn detach(self) {}
}

impl<T> Future for Pending<T> {
    type Output = Result<T>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.rx)
            .poll(cx)
            .map(|received| received.unwrap_or(Err(Error::Shutdown)))
    }
}
