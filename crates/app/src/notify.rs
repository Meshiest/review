//! System toasts. gpui-pre has no notification support on Windows, so
//! there the WinRT toast API is used (through `tauri-winrt-notification`,
//! which keeps the `unsafe` out of this crate); elsewhere gpui's own
//! [`gpui::App::show_system_notification`].
//!
//! [`Toaster::show`] may be called from any thread and never blocks the
//! caller on the platform: the Windows call runs on a thread of its own.
//! A click on a toast arrives on the receiver [`clicks`] hands out.

use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::SystemTime;

use futures::channel::mpsc;

use crate::watching::Toast;

/// Something that shows a toast.
pub trait Toaster: Send + Sync {
    fn show(&self, toast: &Toast);
}

/// A clicked toast: the pull request and where to open it.
pub type Click = Toast;

type ClickSender = mpsc::UnboundedSender<Click>;

static CLICKS: OnceLock<Mutex<Option<ClickSender>>> = OnceLock::new();

fn click_slot() -> &'static Mutex<Option<ClickSender>> {
    CLICKS.get_or_init(|| Mutex::new(None))
}

/// The receiver of toast clicks; a second call replaces the first.
pub fn clicks() -> mpsc::UnboundedReceiver<Click> {
    let (tx, rx) = mpsc::unbounded();
    *click_slot().lock().unwrap_or_else(PoisonError::into_inner) = Some(tx);
    rx
}

fn clicked(toast: Click) {
    if let Some(tx) = click_slot()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
    {
        let _ = tx.unbounded_send(toast);
    }
}

/// The platform's toaster.
#[must_use]
pub fn system() -> Arc<dyn Toaster> {
    Arc::new(platform::System)
}

/// Wraps a toaster and records when each toast went to it, for
/// `notify_bench`.
pub struct Recording {
    inner: Arc<dyn Toaster>,
    shown: Mutex<Vec<(SystemTime, Toast)>>,
}

impl Recording {
    #[must_use]
    pub fn new(inner: Arc<dyn Toaster>) -> Self {
        Self {
            inner,
            shown: Mutex::default(),
        }
    }

    /// Everything shown so far, oldest first, and forgets it.
    pub fn take(&self) -> Vec<(SystemTime, Toast)> {
        std::mem::take(&mut *self.shown.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

impl Toaster for Recording {
    fn show(&self, toast: &Toast) {
        self.shown
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((SystemTime::now(), toast.clone()));
        self.inner.show(toast);
    }
}

#[cfg(windows)]
mod platform {
    use tauri_winrt_notification::{Duration, Toast as WinToast};

    use super::{Toaster, clicked};
    use crate::watching::Toast;

    /// The toasts go out under PowerShell's AppUserModelID: an unpackaged
    /// app's own id only works once registered, which an installer does.
    #[derive(Default)]
    pub struct System;

    impl Toaster for System {
        fn show(&self, toast: &Toast) {
            let toast = toast.clone();
            // The WinRT calls take a few ms and pause 10 ms after showing.
            let _ = std::thread::Builder::new()
                .name("toast".to_owned())
                .spawn(move || {
                    let click = toast.clone();
                    let shown = WinToast::new(WinToast::POWERSHELL_APP_ID)
                        .title(&toast.title)
                        .text1(&toast.text())
                        .duration(Duration::Short)
                        .on_activated(move |_| {
                            clicked(click.clone());
                            Ok(())
                        })
                        .show();
                    if let Err(error) = shown {
                        eprintln!("review: toast not shown: {error}");
                    }
                });
        }
    }
}

#[cfg(not(windows))]
mod platform {
    use std::sync::{Mutex, OnceLock, PoisonError};

    use futures::channel::mpsc;

    use super::{Toaster, clicked};
    use crate::watching::Toast;

    type Queue = Mutex<Option<mpsc::UnboundedSender<Toast>>>;

    static QUEUE: OnceLock<Queue> = OnceLock::new();

    /// gpui posts the notification and needs the app for it, so toasts go
    /// to the main thread through [`queue`].
    #[derive(Default)]
    pub struct System;

    impl Toaster for System {
        fn show(&self, toast: &Toast) {
            if let Some(tx) = QUEUE
                .get_or_init(Mutex::default)
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_ref()
            {
                let _ = tx.unbounded_send(toast.clone());
            }
        }
    }

    /// Toasts for the main thread to post with
    /// [`gpui::App::show_system_notification`].
    pub fn queue() -> mpsc::UnboundedReceiver<Toast> {
        let (tx, rx) = mpsc::unbounded();
        *QUEUE
            .get_or_init(Mutex::default)
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(tx);
        rx
    }

    pub fn post(toast: &Toast, cx: &gpui::App) {
        cx.show_system_notification(gpui::SystemNotification {
            tag: toast.tag().into(),
            title: toast.title.clone().into(),
            body: toast.text().into(),
            actions: Vec::new(),
        });
    }

    pub fn respond(toast: Toast) {
        clicked(toast);
    }
}

#[cfg(not(windows))]
pub use platform::{post as post_with_gpui, queue as gpui_queue, respond as gpui_clicked};
