//! Watching in the running app: [`Watching`] against the forge hub's watch
//! engine (which polls on the forge runtime), with its effects applied to
//! the session, toasts handed to the platform, and the user's choices kept
//! in the store's subscriptions table.
//!
//! The panels and the sidebar read badges, the activity list and the Watch
//! toggles from here. In a fixtures run there is no engine; the toggles
//! still work, in memory.

use std::sync::Arc;
use std::time::Instant;

use forge::model::{CiState, RunId};
use forge::watch::{Change, SubscriptionId, WatchEvent, WatchTarget};
use futures::StreamExt as _;
use gpui::{App, AppContext as _, Context, Entity, Global, Task};
use store::{PrKey, SubscriptionTarget};

use crate::appearance::Appearance;
use crate::ci;
use crate::model::PullRequest;
use crate::notify::{self, Toaster};
use crate::panels::PanelKind;
use crate::session::{OpenMode, Session};
use crate::watching::{
    ActivityEntry, Effect, Engine, OpenInterest, Persist, Place, PrInfo, Toast, Watching,
    run_target,
};

/// Hands out ids and polls nothing: fixtures, and the time before the hub
/// is up.
#[derive(Default)]
struct NullEngine {
    next: u64,
}

impl Engine for NullEngine {
    fn subscribe(&mut self, _: WatchTarget) -> SubscriptionId {
        self.next += 1;
        SubscriptionId(u64::MAX / 2 + self.next)
    }

    fn unsubscribe(&mut self, _: SubscriptionId) {}
}

/// What the wanted subscriptions depend on; a session change that leaves
/// it alone does not recompute them.
#[derive(Clone, PartialEq, Eq)]
struct Inputs {
    inbox: usize,
    open: Option<PrKey>,
    active_runs: Vec<RunId>,
    accounts: Vec<(String, String)>,
    revision: u64,
}

pub struct Subscriptions {
    core: Watching,
    engine: Box<dyn Engine>,
    live: bool,
    session: Entity<Session>,
    toaster: Arc<dyn Toaster>,
    store: Option<store::Store>,
    inputs: Option<Inputs>,
    /// Bumped by a toggle, a settings change or the store's rows arriving.
    revision: u64,
    open: Option<PrKey>,
    flush: Option<Task<()>>,
    _tasks: Vec<Task<()>>,
}

struct Handle(Entity<Subscriptions>);

impl Global for Handle {}

impl Subscriptions {
    /// Starts watching for `session`, and attaches to the forges once its
    /// services are up.
    pub fn install(session: Entity<Session>, cx: &mut App) -> Entity<Self> {
        let settings = cx.global::<Appearance>().settings.notifications;
        let entity = cx.new(|cx| {
            cx.observe(&session, |this: &mut Self, _, cx| this.session_changed(cx))
                .detach();
            cx.observe_global::<Appearance>(|this: &mut Self, cx| {
                let settings = cx.global::<Appearance>().settings.notifications;
                if *this.core.settings() != settings {
                    this.core.set_settings(settings);
                    this.revision += 1;
                    this.resync(cx);
                }
            })
            .detach();
            let mut this = Self {
                core: Watching::new(settings),
                engine: Box::new(NullEngine::default()),
                live: false,
                session: session.clone(),
                toaster: notify::system(),
                store: None,
                inputs: None,
                revision: 0,
                open: None,
                flush: None,
                _tasks: Vec::new(),
            };
            this._tasks.push(Self::follow_clicks(cx));
            #[cfg(not(windows))]
            this._tasks.push(Self::post_queued(cx));
            this.session_changed(cx);
            this
        });
        cx.set_global(Handle(entity.clone()));
        entity
    }

    #[must_use]
    pub fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<Handle>().map(|handle| handle.0.clone())
    }

    /// Uses another toaster, for benchmarks.
    pub fn set_toaster(&mut self, toaster: Arc<dyn Toaster>) {
        self.toaster = toaster;
    }

    fn session_changed(&mut self, cx: &mut Context<Self>) {
        if !self.live
            && let Some(services) = self.session.read(cx).services().cloned()
        {
            self.attach(services, cx);
        }
        let open = self.session.read(cx).open_key().cloned();
        if open != self.open {
            if let Some(key) = &open {
                self.core.clear_badge(key);
            }
            self.core.clear_checks_badge();
            self.open = open;
        }
        self.resync(cx);
    }

    /// The forges are up: the hub's watcher replaces the null engine and
    /// the saved subscriptions are read.
    fn attach(&mut self, services: crate::services::Services, cx: &mut Context<Self>) {
        self.live = true;
        self.store = services.store.clone();
        if let Some(hub) = &services.hub {
            let (handle, mut changes) = hub.watch();
            self.engine = Box::new(handle);
            // Subscriptions of the null engine are not the hub's.
            self.core.forget_subscriptions();
            self.inputs = None;
            self._tasks.push(cx.spawn(async move |this, cx| {
                while let Some(change) = changes.recv().await {
                    if this
                        .update(cx, |this, cx| this.on_change(change, cx))
                        .is_err()
                    {
                        return;
                    }
                }
            }));
        }
        if let Some(store) = self.store.clone() {
            let rows = store.subscriptions();
            self._tasks.push(cx.spawn(async move |this, cx| {
                let Ok(rows) = rows.await else {
                    return;
                };
                this.update(cx, |this, cx| {
                    this.core.load(rows);
                    this.revision += 1;
                    this.resync(cx);
                })
                .ok();
            }));
        }
    }

    /// Recomputes the wanted subscriptions when what they depend on moved.
    fn resync(&mut self, cx: &mut Context<Self>) {
        let session = self.session.read(cx);
        let accounts: Vec<(String, String)> = session
            .forges
            .iter()
            .filter_map(|f| Some((f.host.to_string(), f.account.as_ref()?.login.clone())))
            .collect();
        let open = session.open_pr();
        let inputs = Inputs {
            inbox: Arc::as_ptr(&session.inbox) as usize,
            open: open.map(|open| open.key.clone()),
            active_runs: open
                .map(|open| ci::active_runs(&open.ci.runs))
                .unwrap_or_default(),
            accounts,
            revision: self.revision,
        };
        if self.inputs.as_ref() == Some(&inputs) {
            return;
        }
        for (host, login) in &inputs.accounts {
            self.core.set_me(host, login);
        }
        let rows: Vec<PrInfo> = session.inbox.prs.iter().map(|pr| self.info(pr)).collect();
        let open = open.map(|open| OpenInterest {
            info: self.info(&open.pr),
            active_runs: inputs.active_runs.clone(),
        });
        let wanted = self.core.plan(rows, open);
        self.core.sync(wanted, self.engine.as_mut());
        self.inputs = Some(inputs);
        cx.notify();
    }

    fn info(&self, pr: &PullRequest) -> PrInfo {
        let me = self.core.me(&pr.key.repo.host);
        let authored = me.is_some_and(|me| pr.author == me);
        PrInfo {
            key: pr.key.clone(),
            label: pr.ref_label(),
            title: pr.title.to_string(),
            authored,
            mine: authored || me.is_some_and(|me| pr.reviewers.iter().any(|r| r == me)),
        }
    }

    fn on_change(&mut self, change: Change, cx: &mut Context<Self>) {
        let effects = self.core.handle(change, Instant::now());
        self.apply(effects, cx);
    }

    fn apply(&mut self, effects: Vec<Effect>, cx: &mut Context<Self>) {
        for effect in effects {
            match effect {
                Effect::RowCi { key, ci } => self
                    .session
                    .update(cx, |session, cx| session.patch_row_ci(&key, ci, cx)),
                Effect::RefreshCi(key) => self
                    .session
                    .update(cx, |session, cx| session.refresh_ci(&key, cx)),
                Effect::RefreshPr(key) => self
                    .session
                    .update(cx, |session, cx| session.refresh_pr(&key, cx)),
                Effect::HeadMoved(key) => self
                    .session
                    .update(cx, |session, cx| session.head_moved(&key, cx)),
                Effect::Toast(toast) => self.toaster.show(&toast),
                Effect::PollFailed { key, error } => {
                    eprintln!(
                        "review: watching {}#{}: {error}",
                        key.repo.path(),
                        key.number
                    );
                }
            }
        }
        self.schedule_flush(cx);
        cx.notify();
    }

    /// Shows the toasts coalescing held back once their window ends.
    fn schedule_flush(&mut self, cx: &mut Context<Self>) {
        let Some(at) = self.core.next_flush() else {
            self.flush = None;
            return;
        };
        let wait = at.saturating_duration_since(Instant::now());
        self.flush = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(wait).await;
            this.update(cx, |this, cx| {
                for toast in this.core.flush(Instant::now()) {
                    this.toaster.show(&toast);
                }
                this.schedule_flush(cx);
            })
            .ok();
        }));
    }

    /// A toast was clicked: the window comes forward with the pull request
    /// open where the toast pointed.
    fn follow_clicks(cx: &mut Context<Self>) -> Task<()> {
        let mut clicks = notify::clicks();
        cx.spawn(async move |this, cx| {
            while let Some(toast) = clicks.next().await {
                if this
                    .update(cx, |this, cx| this.open_at(&toast, cx))
                    .is_err()
                {
                    return;
                }
            }
        })
    }

    /// Opens a pull request where a toast or an activity entry points.
    pub fn open_at(&mut self, toast: &Toast, cx: &mut Context<Self>) {
        cx.activate(true);
        for window in cx.windows() {
            let _ = window.update(cx, |_, window, _| window.activate_window());
        }
        self.open_place(&toast.key, &toast.place, cx);
    }

    pub fn open_place(&mut self, key: &PrKey, place: &Place, cx: &mut Context<Self>) {
        self.session.update(cx, |session, cx| {
            let panel = match place {
                Place::Checks => PanelKind::Checks,
                Place::Conversation(_) | Place::Overview => PanelKind::Conversation,
            };
            session.show(key, panel, OpenMode::Permanent, cx);
            if let Place::Conversation(Some(thread)) = place
                && session
                    .open_pr()
                    .is_some_and(|open| open.threads.iter().any(|t| &t.id == thread))
            {
                session.jump_to_thread(thread.clone(), true, cx);
            }
        });
    }

    #[cfg(not(windows))]
    fn post_queued(cx: &mut Context<Self>) -> Task<()> {
        use std::cell::RefCell;
        use std::collections::HashMap;
        use std::rc::Rc;

        let shown: Rc<RefCell<HashMap<String, Toast>>> = Rc::default();
        let responses = shown.clone();
        cx.on_system_notification_response(move |response, _| {
            if let Some(toast) = responses.borrow().get(response.tag.as_ref()) {
                notify::gpui_clicked(toast.clone());
            }
        });
        let mut queue = notify::gpui_queue();
        cx.spawn(async move |_, cx| {
            while let Some(toast) = queue.next().await {
                shown.borrow_mut().insert(toast.tag(), toast.clone());
                cx.update(|cx| notify::post_with_gpui(&toast, cx));
            }
        })
    }

    // ---- what the views read and toggle

    fn mine(&self, key: &PrKey, cx: &App) -> bool {
        let session = self.session.read(cx);
        let row = session
            .inbox
            .get(key)
            .or_else(|| session.open_pr().filter(|o| &o.key == key).map(|o| &o.pr));
        row.is_some_and(|pr| self.info(pr).mine)
    }

    #[must_use]
    pub fn is_watched(&self, key: &PrKey, cx: &App) -> bool {
        self.core
            .is_watched(&SubscriptionTarget::pull_request(key), self.mine(key, cx))
    }

    pub fn toggle(&mut self, key: &PrKey, cx: &mut Context<Self>) {
        let mine = self.mine(key, cx);
        let persist = self
            .core
            .toggle(&SubscriptionTarget::pull_request(key), mine, None);
        self.persist(persist, cx);
    }

    #[must_use]
    pub fn is_run_watched(&self, key: &PrKey, run: &RunId) -> bool {
        self.core.is_watched(&run_target(key, run), false)
    }

    pub fn toggle_run(&mut self, key: &PrKey, run: &RunId, cx: &mut Context<Self>) {
        let persist = self
            .core
            .toggle(&run_target(key, run), false, Some(key.number.0));
        self.persist(persist, cx);
    }

    fn persist(&mut self, persist: Persist, cx: &mut Context<Self>) {
        if let Some(store) = &self.store {
            match &persist {
                Persist::Put(target, state) => store.put_subscription(target, Some(state)).detach(),
                Persist::Delete(target) => store.delete_subscription(target).detach(),
            }
        }
        self.revision += 1;
        self.resync(cx);
    }

    #[must_use]
    pub fn has_badge(&self, key: &PrKey) -> bool {
        self.core.has_badge(key)
    }

    #[must_use]
    pub fn checks_badge(&self) -> bool {
        self.core.checks_badge()
    }

    pub fn clear_checks_badge(&mut self, cx: &mut Context<Self>) {
        if self.core.clear_checks_badge() {
            cx.notify();
        }
    }

    pub fn activity(&self) -> impl Iterator<Item = &ActivityEntry> {
        self.core.activity()
    }

    #[must_use]
    pub fn unread(&self) -> usize {
        self.core.unread()
    }

    pub fn mark_read(&mut self, cx: &mut Context<Self>) {
        self.core.mark_read();
        cx.notify();
    }

    /// Plays a failed check and a comment on the first watched pull
    /// request through the real path, for fixture screenshots.
    pub fn demo(&mut self, cx: &mut Context<Self>) {
        let Some(key) = self
            .session
            .read(cx)
            .inbox
            .prs
            .iter()
            .map(|pr| pr.key.clone())
            .find(|key| self.is_watched(key, cx))
        else {
            return;
        };
        let Some(id) = self.engine_id(&key) else {
            return;
        };
        let target = crate::watching::watch_target(&SubscriptionTarget::pull_request(&key));
        let change = |event| Change {
            subscription: id,
            target: target.clone(),
            event,
        };
        let failed = change(WatchEvent::CiStateChanged {
            from: CiState::Running,
            to: CiState::Fail,
        });
        let effects = self.core.handle(failed, Instant::now());
        self.apply(effects, cx);
    }

    fn engine_id(&self, key: &PrKey) -> Option<SubscriptionId> {
        self.core
            .subscription_id(&SubscriptionTarget::pull_request(key))
    }
}
