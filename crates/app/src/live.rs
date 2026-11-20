//! Startup against the real forges: read the config, open the cache, read
//! the cached inbox and start the forge hub off the main thread, paint the
//! inbox from the cache, then refresh every section through the hub and
//! patch the list in place.
//!
//! [`EarlyBoot`] starts that work on its own thread before gpui starts, so
//! it overlaps the window system's start and the first frame can already
//! show the cached inbox.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use forge::model::{ForgeKind, InboxSection, PullRequest as ForgePr, RepoRef};
use forge::{ForgeConfig, ForgeHub};
use futures::StreamExt as _;
use futures::channel::{mpsc, oneshot};
use gpui::{App, AppContext as _, AsyncApp, Context, Entity, Global, SharedString, Task};

use crate::appearance;
use crate::avatars::{self, Avatars, Preloaded};
use crate::config::{Config, SectionKind};
use crate::model::{Inbox, PrKey, PullRequest, RemovableSection, Section, SectionState};
use crate::services::{Services, Spawn};
use crate::session::{ForgeState, ForgeStatus, Session};
use crate::settings::Startup;

/// How often every section is asked again.
const REFRESH: Duration = Duration::from_secs(180);

/// One inbox section and the forge queries that feed it.
#[derive(Clone, Debug)]
struct SectionSpec {
    title: SharedString,
    sources: Vec<(String, InboxSection)>,
    expanded_by_default: bool,
    /// The repository of a `[[repos]]` section.
    repo: Option<RepoRef>,
    /// The built-in section it is.
    kind: Option<SectionKind>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Freshness {
    Cached,
    Fresh,
}

/// A forge and one of its inbox queries.
type Source = (String, InboxSection);

#[derive(Default)]
struct Results {
    lists: HashMap<Source, (Vec<Arc<ForgePr>>, Freshness)>,
    failed: HashMap<Source, SharedString>,
}

/// Owns the inbox's sources and keeps the session's inbox up to date.
pub struct Live {
    session: Entity<Session>,
    services: Services,
    specs: Vec<SectionSpec>,
    kinds: HashMap<String, ForgeKind>,
    results: Results,
    /// Repositories whose object stores were opened ahead of use.
    warmed: HashSet<RepoRef>,
    _tasks: Vec<Task<()>>,
}

struct LiveHandle(#[expect(dead_code, reason = "keeps the entity alive")] Entity<Live>);

impl Global for LiveHandle {}

/// What the background start produced.
struct Boot {
    config: Arc<Config>,
    store: Option<store::Store>,
    hub: Option<Arc<ForgeHub>>,
    notice: Option<String>,
    /// The cached list of each source that has one.
    cached: Vec<(Source, store::CachedInbox)>,
    /// The user's settings and themes, read beside the config.
    startup: Startup,
    /// Cached avatars, read here so rows can show them on the first frame.
    avatars: Preloaded,
}

/// The background start, begun before the app runs; see [`EarlyBoot::start`].
pub struct EarlyBoot(RefCell<Option<oneshot::Receiver<Boot>>>);

impl EarlyBoot {
    /// Starts reading the config and the cache on a thread of its own. Call
    /// it first thing in `main`; [`start`] picks the result up.
    pub fn start() -> Self {
        Self::start_with(BootPaths::user())
    }

    /// [`Self::start`] with the settings, themes and data dir in `paths`.
    pub fn start_with(paths: BootPaths) -> Self {
        let (tx, rx) = oneshot::channel();
        let spawned = thread::Builder::new()
            .name("review-boot".to_owned())
            .spawn(move || {
                let _ = tx.send(boot(&paths));
            });
        Self(RefCell::new(spawned.is_ok().then_some(rx)))
    }

    fn take(&self) -> Option<oneshot::Receiver<Boot>> {
        self.0.borrow_mut().take()
    }
}

/// Where a live run's settings, themes and cache are.
#[derive(Clone, Debug, Default)]
pub struct BootPaths {
    pub settings: Option<PathBuf>,
    pub themes: Option<PathBuf>,
    /// Overrides the config's `data_dir`.
    pub data_dir: Option<PathBuf>,
}

impl BootPaths {
    /// The user's own settings and themes, and the config's data dir.
    #[must_use]
    pub fn user() -> Self {
        Self {
            settings: crate::settings::SettingsFile::default_path(),
            themes: crate::settings::SettingsFile::themes_dir(),
            data_dir: None,
        }
    }
}

fn boot(paths: &BootPaths) -> Boot {
    let startup = Startup::read(paths.settings.clone(), paths.themes.as_deref());
    let mut notices = Vec::new();
    let config = match Config::default_path() {
        Some(path) => match Config::load_or_create(&path) {
            Ok((config, created)) => {
                if created {
                    eprintln!("review: wrote {}", path.display());
                }
                config
            }
            Err(error) => {
                notices.push(error.to_string());
                Config::default()
            }
        },
        None => Config::default(),
    };
    let mut config = config;
    if let Some(dir) = &paths.data_dir {
        config.data_dir = Some(dir.clone());
    }
    let store = match store::Store::open(cache_path(&config)) {
        Ok(store) => Some(store),
        Err(error) => {
            notices.push(format!("cache: {error}"));
            None
        }
    };
    let forges = config
        .forges
        .iter()
        .map(|forge| ForgeConfig {
            kind: forge.kind,
            host: forge.host.clone(),
        })
        .collect();
    let hub = match ForgeHub::start(forges) {
        Ok(hub) => Some(Arc::new(hub)),
        Err(error) => {
            notices.push(format!("forges: {error}"));
            None
        }
    };
    let cached = store
        .as_ref()
        .map(|store| read_cached(store, &section_specs(&config)))
        .unwrap_or_default();
    let avatars = avatars::preload(&avatars::cache_dir(&config.data_dir()));
    Boot {
        config: Arc::new(config),
        store,
        hub,
        notice: (!notices.is_empty()).then(|| notices.join("; ")),
        cached,
        startup,
        avatars,
    }
}

/// Every source's cached list. Blocks the boot thread, never the UI.
fn read_cached(store: &store::Store, specs: &[SectionSpec]) -> Vec<(Source, store::CachedInbox)> {
    let reads = sources(specs).into_iter().map(|(host, section)| {
        let pending = store.inbox(&host, &section);
        async move { ((host, section), pending.await) }
    });
    futures::executor::block_on(futures::future::join_all(reads))
        .into_iter()
        .filter_map(|(source, result)| Some((source, result.ok()??)))
        .collect()
}

/// Each section's sources once, in section order.
fn sources(specs: &[SectionSpec]) -> Vec<Source> {
    let mut sources: Vec<Source> = Vec::new();
    for source in specs.iter().flat_map(|spec| spec.sources.iter()) {
        if !sources.contains(source) {
            sources.push(source.clone());
        }
    }
    sources
}

fn cache_path(config: &Config) -> PathBuf {
    config.data_dir().join("cache.sqlite")
}

fn section_specs(config: &Config) -> Vec<SectionSpec> {
    let mut specs: Vec<SectionSpec> = SectionKind::ALL
        .into_iter()
        .map(|kind| SectionSpec {
            title: kind.title().into(),
            sources: config
                .forges
                .iter()
                .filter(|forge| forge.sections.contains(&kind))
                .map(|forge| (forge.host.clone(), kind.query()))
                .collect(),
            expanded_by_default: true,
            repo: None,
            kind: Some(kind),
        })
        .filter(|spec| !spec.sources.is_empty())
        .collect();
    specs.extend(config.repos.iter().filter_map(|entry| {
        let repo = entry.repo_ref()?;
        Some(SectionSpec {
            title: format!("{}: open", repo.path()).into(),
            sources: vec![(repo.host.clone(), InboxSection::OpenInRepo(repo.clone()))],
            expanded_by_default: false,
            repo: Some(repo),
            kind: None,
        })
    }));
    specs
}

/// Starts everything in the background; the inbox fills in as it arrives.
/// With an [`EarlyBoot`] that has finished, the cached inbox is in the
/// session before this returns, so the window's first frame shows it.
pub fn start(session: Entity<Session>, started: Instant, early: Option<&EarlyBoot>, cx: &mut App) {
    let mut pending = early.and_then(EarlyBoot::take);
    if let Some(boot) = pending.as_mut().and_then(|rx| rx.try_recv().ok().flatten()) {
        install(session, started, boot, cx);
        return;
    }
    cx.spawn(async move |cx| {
        let early = match pending {
            Some(rx) => rx.await.ok(),
            None => None,
        };
        let boot = match early {
            Some(boot) => boot,
            None => {
                cx.background_executor()
                    .spawn(async { boot(&BootPaths::user()) })
                    .await
            }
        };
        cx.update(|cx| install(session, started, boot, cx));
    })
    .detach();
}

fn install(session: Entity<Session>, started: Instant, boot: Boot, cx: &mut App) {
    let executor = cx.background_executor().clone();
    let spawn: Spawn = Arc::new(move |future| executor.spawn(future).detach());
    if let Some(avatars) = cx.try_global::<Avatars>().cloned() {
        let dir = avatars::cache_dir(&boot.config.data_dir());
        avatars.attach(dir, boot.hub.clone(), boot.avatars);
    }
    if let Some(review) = crate::claude_review::AgentReview::global(cx) {
        let dir = boot.config.data_dir().join("mcp");
        review.update(cx, |review, cx| review.set_config_dir(dir, cx));
    }
    if let Some(workspaces) = crate::workspaces::Workspaces::global(cx) {
        let entries = boot.config.workspaces.clone();
        workspaces.update(cx, |workspaces, cx| workspaces.loaded(entries, cx));
    }
    let services = Services::new(boot.config.clone(), boot.store, boot.hub, spawn);
    let notices = [boot.notice, appearance::loaded(cx, boot.startup)];
    let notice = notices
        .into_iter()
        .flatten()
        .reduce(|a, b| format!("{a}; {b}"));
    let cached = boot.cached;
    add_new_logins(session.clone(), cx);
    let live = cx.new(|cx| Live::new(session, services, notice, cached, started, cx));
    cx.set_global(LiveHandle(live));
}

/// Asks the forge CLIs, off the start's path, for hosts they were not logged
/// in to before, and adds them to the config. The forges are fixed for this
/// run, so the notice asks for a restart.
fn add_new_logins(session: Entity<Session>, cx: &mut App) {
    let Some(path) = Config::default_path() else {
        return;
    };
    let read = cx
        .background_executor()
        .spawn(async move { crate::config::add_new_logins(&path) });
    cx.spawn(async move |cx| {
        let added = match read.await {
            Ok(added) if !added.is_empty() => added,
            Ok(_) => return,
            Err(error) => {
                eprintln!("review: checking for new logins: {error}");
                return;
            }
        };
        let names: Vec<String> = added
            .iter()
            .map(|forge| format!("{} ({})", forge.host, forge.kind.label()))
            .collect();
        let text = format!(
            "New login found: {}. Added to config.toml; restart review to load it.",
            names.join(", ")
        );
        add_notice(&session, text, cx);
    })
    .detach();
}

/// Adds `text` to the status bar's notice, after what it already says.
fn add_notice(session: &Entity<Session>, text: String, cx: &mut AsyncApp) {
    session.update(cx, |session, cx| {
        session.notice = Some(match session.notice.take() {
            Some(old) => format!("{old}; {text}").into(),
            None => text.into(),
        });
        cx.notify();
    });
}

impl Live {
    fn new(
        session: Entity<Session>,
        services: Services,
        notice: Option<String>,
        cached: Vec<(Source, store::CachedInbox)>,
        started: Instant,
        cx: &mut Context<Self>,
    ) -> Self {
        let config = services.config.clone();
        let kinds: HashMap<String, ForgeKind> = config
            .forges
            .iter()
            .map(|forge| (forge.host.clone(), forge.kind))
            .collect();
        let forges = config
            .forges
            .iter()
            .map(|forge| ForgeStatus {
                host: forge.host.clone().into(),
                kind: forge.kind,
                state: ForgeState::Connecting,
                account: None,
            })
            .collect();
        session.update(cx, |session, cx| {
            session.forges = forges;
            session.notice = notice.map(Into::into);
            session.set_services(services.clone(), cx);
        });
        let mut live = Self {
            session,
            services,
            specs: section_specs(&config),
            kinds,
            results: Results::default(),
            warmed: HashSet::new(),
            _tasks: Vec::new(),
        };
        for (source, inbox) in cached {
            let prs = inbox.prs.into_iter().map(Arc::new).collect();
            live.results.lists.insert(source, (prs, Freshness::Cached));
        }
        live.publish(cx);
        if !live.results.lists.is_empty() {
            eprintln!(
                "review: inbox from cache {:.0} ms after launch",
                started.elapsed().as_secs_f64() * 1000.0
            );
        }
        live._tasks.push(live.watch_activity(cx));
        live._tasks.push(live.refresh_loop(cx));
        // The inbox's refresh button and F5 refetch through this.
        let weak = cx.weak_entity();
        let weak_remover = weak.clone();
        live.session.update(cx, |session, _| {
            session.set_inbox_refresher(std::rc::Rc::new(move |cx: &mut App| {
                weak.update(cx, |live, cx| live.refresh(cx))
                    .unwrap_or_else(|_| Task::ready(()))
            }));
            session.set_section_remover(std::rc::Rc::new(
                move |section: &RemovableSection, cx: &mut App| {
                    let _ = weak_remover.update(cx, |live, cx| live.remove_section(section, cx));
                },
            ));
        });
        live
    }

    /// Stops listing `section`: it leaves the specs, so no refresh brings it
    /// back, and the config file: a repository's `[[repos]]` table, or the
    /// built-in section from each forge's `sections`.
    fn remove_section(&mut self, section: &RemovableSection, cx: &mut Context<Self>) {
        let (gone, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.specs)
            .into_iter()
            .partition(|spec| section.covers(spec.repo.as_ref(), spec.kind));
        self.specs = kept;
        let still_used = sources(&self.specs);
        for source in gone.iter().flat_map(|spec| &spec.sources) {
            if !still_used.contains(source) {
                self.results.lists.remove(source);
                self.results.failed.remove(source);
            }
        }
        self.publish(cx);
        let Some(path) = Config::default_path() else {
            return;
        };
        let section = section.clone();
        let write = cx.background_executor().spawn(async move {
            match &section {
                RemovableSection::Repo(repo) => crate::config::remove_repo(&path, repo),
                RemovableSection::Builtin(kind) => crate::config::remove_section(&path, *kind),
            }
        });
        let session = self.session.clone();
        cx.spawn(async move |_, cx| {
            if let Err(error) = write.await {
                add_notice(
                    &session,
                    format!("Could not remove the section from config.toml: {error}"),
                    cx,
                );
            }
        })
        .detach();
    }

    fn sources(&self) -> Vec<Source> {
        sources(&self.specs)
    }

    /// Mirrors the fetch queue and request counts into the status bar.
    fn watch_activity(&self, cx: &mut Context<Self>) -> Task<()> {
        let (tx, mut rx) = mpsc::unbounded::<()>();
        self.services.activity.set_on_change(move || {
            let _ = tx.unbounded_send(());
        });
        let activity = self.services.activity.clone();
        let session = self.session.clone();
        cx.spawn(async move |_, cx| {
            while rx.next().await.is_some() {
                // Coalesce a burst of changes into one repaint.
                while rx.try_recv().is_ok() {}
                let counts = activity.counts();
                cx.update(|cx| {
                    session.update(cx, |session, cx| {
                        if session.activity != counts {
                            session.activity = counts;
                            cx.notify();
                        }
                    });
                });
            }
        })
    }

    fn refresh_loop(&self, cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                let Ok(round) = this.update(cx, |live, cx| live.refresh(cx)) else {
                    return;
                };
                round.await;
                cx.background_executor().timer(REFRESH).await;
            }
        })
    }

    /// Asks each forge who is signed in and each section's list, patching
    /// the inbox as each answer lands. Resolves when all have.
    fn refresh(&mut self, cx: &mut Context<Self>) -> Task<()> {
        let hosts: Vec<String> = self.kinds.keys().cloned().collect();
        let whoami = hosts.into_iter().map(|host| {
            let call = self.services.forge(&host, |forge| async move {
                let user = forge.whoami().await?;
                Ok((user, forge.capabilities()))
            });
            let this = cx.entity().downgrade();
            let services = self.services.clone();
            cx.spawn(async move |_, cx| {
                let state = match call.await {
                    Ok((user, capabilities)) => {
                        services.set_capabilities(&host, capabilities);
                        let _ = this.update(cx, |live, cx| live.set_account(&host, user, cx));
                        ForgeState::Ok
                    }
                    Err(error) => ForgeState::of(&error),
                };
                let _ = this.update(cx, |live, cx| live.set_forge_state(&host, state, cx));
            })
        });
        let lists = self.sources().into_iter().map(|(host, section)| {
            let query = section.clone();
            let call = self
                .services
                .forge(&host, move |forge| async move { forge.inbox(&query).await });
            let store = self.services.store.clone();
            let this = cx.entity().downgrade();
            cx.spawn(async move |_, cx| {
                let result = call.await;
                if let (Ok(prs), Some(store)) = (&result, &store) {
                    store.put_inbox(&host, &section, prs.clone()).detach();
                }
                let _ = this.update(cx, |live, cx| live.patch(host, section, result, cx));
            })
        });
        let tasks: Vec<Task<()>> = whoami.chain(lists).collect();
        cx.background_executor().spawn(async move {
            futures::future::join_all(tasks).await;
        })
    }

    fn set_account(&mut self, host: &str, user: forge::model::User, cx: &mut Context<Self>) {
        self.session.update(cx, |session, cx| {
            if let Some(forge) = session.forges.iter_mut().find(|f| f.host == host)
                && forge.account.as_ref() != Some(&user)
            {
                forge.account = Some(user);
                cx.notify();
            }
        });
    }

    fn set_forge_state(&mut self, host: &str, state: ForgeState, cx: &mut Context<Self>) {
        self.session.update(cx, |session, cx| {
            if let Some(forge) = session.forges.iter_mut().find(|f| f.host == host)
                && forge.state != state
            {
                forge.state = state;
                cx.notify();
            }
        });
    }

    fn patch(
        &mut self,
        host: String,
        section: InboxSection,
        result: forge::Result<Vec<ForgePr>>,
        cx: &mut Context<Self>,
    ) {
        let source = (host.clone(), section);
        match result {
            Ok(prs) => {
                self.results.failed.remove(&source);
                let prs = prs.into_iter().map(Arc::new).collect();
                self.results.lists.insert(source, (prs, Freshness::Fresh));
                let connecting = self
                    .session
                    .read(cx)
                    .forges
                    .iter()
                    .any(|f| f.host == host.as_str() && f.state == ForgeState::Connecting);
                if connecting {
                    self.set_forge_state(&host, ForgeState::Ok, cx);
                }
            }
            Err(error) => {
                self.results.failed.insert(source, error.to_string().into());
                self.set_forge_state(&host, ForgeState::of(&error), cx);
            }
        }
        self.publish(cx);
    }

    /// Rebuilds the inbox from every source's latest list.
    fn publish(&mut self, cx: &mut Context<Self>) {
        let inbox = build_inbox(&self.specs, &self.results, &self.kinds);
        let fresh: Vec<RepoRef> = inbox
            .prs
            .iter()
            .map(|pr| pr.key.repo.clone())
            .filter(|repo| self.warmed.insert(repo.clone()))
            .collect();
        if !fresh.is_empty() {
            let objects = self.services.objects.clone();
            drop(self.services.run_blocking(move || {
                for repo in &fresh {
                    objects.warm(repo);
                }
            }));
        }
        self.session
            .update(cx, |session, cx| session.set_inbox(inbox, cx));
    }
}

fn build_inbox(
    specs: &[SectionSpec],
    results: &Results,
    kinds: &HashMap<String, ForgeKind>,
) -> Inbox {
    let mut inbox = Inbox::default();
    let mut index: HashMap<PrKey, usize> = HashMap::new();
    // What "Changes requested" lists; "Created by me", after it, leaves
    // those out.
    let mut needs_work: HashSet<PrKey> = HashSet::new();
    for spec in specs {
        let mut rows: Vec<&Arc<ForgePr>> = Vec::new();
        let mut state = SectionState::Fresh;
        let mut any = false;
        for source in &spec.sources {
            if let Some(error) = results.failed.get(source) {
                state = SectionState::Failed(error.clone());
            }
            match results.lists.get(source) {
                Some((prs, freshness)) => {
                    any = true;
                    rows.extend(prs);
                    if *freshness == Freshness::Cached && state == SectionState::Fresh {
                        state = SectionState::Cached;
                    }
                }
                None if state == SectionState::Fresh => state = SectionState::Loading,
                None => {}
            }
        }
        if !any && state == SectionState::Cached {
            state = SectionState::Loading;
        }
        rows.sort_by_key(|pr| std::cmp::Reverse(pr.updated_at));
        let mut items = Vec::with_capacity(rows.len());
        for pr in rows {
            let key = PrKey::of(pr);
            let known = index.get(&key).copied();
            let row = match known {
                Some(ix) => Cow::Borrowed(&inbox.prs[ix]),
                None => {
                    let kind = kinds
                        .get(&pr.repo.host)
                        .copied()
                        .unwrap_or(ForgeKind::GitHub);
                    Cow::Owned(PullRequest::from_forge(pr.clone(), kind))
                }
            };
            let listed = match spec.kind {
                Some(SectionKind::ChangesRequested) => row.needs_work().is_some(),
                Some(SectionKind::AuthoredByMe) => !needs_work.contains(&key),
                Some(SectionKind::ReviewRequested) | None => true,
            };
            if !listed {
                continue;
            }
            if spec.kind == Some(SectionKind::ChangesRequested) {
                needs_work.insert(key.clone());
            }
            let ix = match known {
                Some(ix) => ix,
                None => {
                    inbox.prs.push(row.into_owned());
                    index.insert(key, inbox.prs.len() - 1);
                    inbox.prs.len() - 1
                }
            };
            if !items.contains(&ix) {
                items.push(ix);
            }
        }
        inbox.sections.push(Section {
            title: spec.title.clone(),
            items,
            expanded_by_default: spec.expanded_by_default,
            state,
            repo: spec.repo.clone(),
            kind: spec.kind,
        });
    }
    inbox
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge::model::{PrNumber, ReviewDecision};

    fn authored(number: u64, decision: ReviewDecision, unresolved: Option<u32>) -> Arc<ForgePr> {
        let fixtures = crate::fixtures::Fixtures::load();
        let mut record = fixtures
            .forge_pr(&fixtures.inbox.prs[0].key)
            .expect("a record");
        record.number = PrNumber(number);
        record.review_decision = decision;
        record.unresolved_threads = unresolved;
        // Newest first: in number order.
        record.updated_at = chrono::DateTime::UNIX_EPOCH
            + chrono::Duration::minutes(100 - i64::try_from(number).expect("small"));
        Arc::new(record)
    }

    fn titles_and_numbers(inbox: &Inbox) -> Vec<(String, Vec<u64>)> {
        inbox
            .sections
            .iter()
            .map(|section| {
                let numbers = section
                    .items
                    .iter()
                    .map(|&ix| inbox.prs[ix].number())
                    .collect();
                (section.title.to_string(), numbers)
            })
            .collect()
    }

    #[test]
    fn changes_requested_is_picked_out_of_created_by_me() {
        let mut config = Config::parse(
            "[[forges]]\nkind = \"github\"\nhost = \"github.com\"\nsections = [\"authored_by_me\"]\n",
        )
        .expect("parses");
        let source: Source = ("github.com".to_owned(), InboxSection::AuthoredByMe);
        let mut results = Results::default();
        let prs = vec![
            authored(1, ReviewDecision::ChangesRequested, Some(0)),
            authored(2, ReviewDecision::Approved, Some(2)),
            authored(3, ReviewDecision::Approved, Some(0)),
            authored(4, ReviewDecision::Commented, None),
        ];
        results
            .lists
            .insert(source.clone(), (prs, Freshness::Fresh));
        let kinds = HashMap::from([("github.com".to_owned(), ForgeKind::GitHub)]);

        let specs = section_specs(&config);
        assert_eq!(
            sources(&specs),
            std::slice::from_ref(&source),
            "one query feeds both"
        );
        let inbox = build_inbox(&specs, &results, &kinds);
        assert_eq!(
            titles_and_numbers(&inbox),
            [
                ("Changes requested".to_owned(), vec![1, 2]),
                ("Created by me".to_owned(), vec![3, 4]),
            ]
        );
        assert_eq!(inbox.prs.len(), 4, "each pull request once");

        // Without the section, "Created by me" lists them all.
        config.forges[0].sections = vec![SectionKind::AuthoredByMe];
        let inbox = build_inbox(&section_specs(&config), &results, &kinds);
        assert_eq!(
            titles_and_numbers(&inbox),
            [("Created by me".to_owned(), vec![1, 2, 3, 4])]
        );

        // Alone, it lists only what waits on the author.
        config.forges[0].sections = vec![SectionKind::ChangesRequested];
        let inbox = build_inbox(&section_specs(&config), &results, &kinds);
        assert_eq!(
            titles_and_numbers(&inbox),
            [("Changes requested".to_owned(), vec![1, 2])]
        );
        assert_eq!(inbox.prs.len(), 2, "no row nothing lists");
    }
}
