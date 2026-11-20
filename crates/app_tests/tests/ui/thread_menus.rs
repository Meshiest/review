//! Threads from the sidebar, the diff and the Conversation: a row click shows
//! the thread in the Conversation (unfolded, scrolled to, briefly
//! highlighted), a double click or Enter at its line in the diff, and the
//! thread's right-click menu does the same everywhere, with its disabled
//! entries saying why. Resolving goes through the forge on a fake one.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use app::conversation::ConversationPanel;
use app::fixtures::Fixtures;
use app::model::PrKey;
use app::panels::{CONVERSATION, DIFF, PanelKind};
use app::session::{OpenMode, Session};
use app::settings::ThreadStyle;
use app::threads::thread_menu::{
    HIGHLIGHT, Origin, ThreadAction, ThreadEntry, ThreadMenu, ThreadNav,
};
use forge::model::{
    Anchor, Comment, CommentId, ForgeKind, LineRef, Position, PrNumber, RepoRef, Resolution, Side,
    Thread, ThreadId, User,
};
use gpui::{Modifiers, MouseButton, TestAppContext};

use crate::harness::{Options, Scratch, ShellWindow, fake_services, go_live, open_shell};

fn threads(shell: &mut ShellWindow) -> Arc<[Thread]> {
    let session = shell.session();
    shell.update(|_, cx| session.read(cx).open_pr().expect("open").threads.clone())
}

/// The anchored threads in the order the sidebar lists them: by file, then
/// line.
fn sidebar_order(threads: &[Thread]) -> Vec<ThreadId> {
    let mut anchored: Vec<&Thread> = threads.iter().filter(|t| t.anchor.is_some()).collect();
    anchored.sort_by_key(|t| {
        let anchor = t.anchor.as_ref().expect("anchored");
        (
            anchor.path.clone(),
            anchor.best_position().map_or(0, |p| p.line.line),
        )
    });
    anchored.into_iter().map(|t| t.id.clone()).collect()
}

fn nav(shell: &mut ShellWindow) -> gpui::Entity<ThreadNav> {
    shell.update(|_, cx| ThreadNav::global(cx))
}

fn style(shell: &mut ShellWindow) -> ThreadStyle {
    shell.update(|_, cx| ConversationPanel::thread_style(cx))
}

fn folded(shell: &mut ShellWindow, key: &PrKey, id: &ThreadId) -> bool {
    let nav = nav(shell);
    let style = style(shell);
    let session = shell.session();
    shell.update(|_, cx| {
        let resolved = session
            .read(cx)
            .pr(key)
            .is_some_and(|open| open.threads.iter().any(|t| &t.id == id && t.is_resolved()));
        nav.read(cx).is_folded(key, id, resolved, style)
    })
}

fn highlighted(shell: &mut ShellWindow, key: &PrKey, id: &ThreadId) -> bool {
    let nav = nav(shell);
    shell.update(|_, cx| nav.read(cx).is_highlighted(key, id))
}

/// Whether the element is inside the Conversation's scrolled body.
fn in_view(shell: &mut ShellWindow, selector: &str) -> bool {
    let body = shell.bounds_of("conversation-body");
    let element = shell.bounds_of(String::leak(selector.to_owned()));
    element.top() >= body.top() - gpui::px(1.0) && element.top() < body.bottom()
}

fn right_click(shell: &mut ShellWindow, selector: &str) {
    let at = shell.bounds_of(String::leak(selector.to_owned())).center();
    let none = Modifiers::none();
    shell.cx.simulate_mouse_move(at, None, none);
    shell.cx.simulate_mouse_down(at, MouseButton::Right, none);
    shell.cx.simulate_mouse_up(at, MouseButton::Right, none);
    shell.settle();
}

/// The current thread of the diff's cards, as an id.
fn diff_current(shell: &mut ShellWindow) -> Option<ThreadId> {
    let diff = shell.diff_panel();
    shell.update(|_, cx| {
        let cards = diff.read(cx).cards().read(cx);
        Some(cards.threads()[cards.current()?].id.clone())
    })
}

fn entries(shell: &mut ShellWindow, menu: &ThreadMenu) -> Vec<ThreadEntry> {
    shell.update(|_, cx| menu.entries(cx).into_iter().flatten().collect())
}

fn entry(
    shell: &mut ShellWindow,
    menu: &ThreadMenu,
    pick: fn(ThreadAction) -> bool,
) -> ThreadEntry {
    entries(shell, menu)
        .into_iter()
        .find(|entry| pick(entry.action))
        .expect("the menu has the entry")
}

fn run(shell: &mut ShellWindow, menu: &ThreadMenu, action: ThreadAction) {
    shell.update(|_, cx| menu.run(action, cx));
    shell.settle();
}

#[gpui::test]
fn a_thread_row_shows_it_in_the_conversation_and_enter_at_its_line(cx: &mut TestAppContext) {
    let mut shell = open_shell(
        cx,
        Options {
            open_pr: Some(41),
            ..Options::default()
        },
    );
    let key = shell.key(41);
    let order = sidebar_order(&threads(&mut shell));
    // The second row: the Threads section draws it without scrolling.
    let (pos, id) = (1, order[1].clone());
    shell.click_named(&format!("thread-row-{pos}"));
    assert!(
        shell.shown_tabs().iter().any(|tab| tab == CONVERSATION),
        "the Conversation came forward: {:?}",
        shell.shown_tabs()
    );
    assert!(!folded(&mut shell, &key, &id), "the thread is unfolded");
    let card = format!("inline-thread-{id}");
    assert!(shell.drawn(&card), "drawn in full");
    assert!(in_view(&mut shell, &card), "and scrolled into view");
    assert!(highlighted(&mut shell, &key, &id), "briefly highlighted");
    shell.advance(HIGHLIGHT + Duration::from_millis(100));
    assert!(!highlighted(&mut shell, &key, &id), "then not");

    shell.keys("enter");
    assert!(
        shell.shown_tabs().iter().any(|tab| tab == DIFF),
        "Enter brought the Diff forward: {:?}",
        shell.shown_tabs()
    );
    assert_eq!(diff_current(&mut shell), Some(id), "at the thread");
}

#[gpui::test]
fn a_thread_far_down_the_conversation_is_scrolled_to(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    shell.show(41, PanelKind::Conversation, OpenMode::Permanent);
    let key = shell.key(41);
    let order = sidebar_order(&threads(&mut shell));
    let (pos, id) = order
        .iter()
        .enumerate()
        .take(3)
        .find(|(_, id)| !in_view(&mut shell, &format!("inline-thread-{id}")))
        .map(|(pos, id)| (pos, id.clone()))
        .expect("one of the first rows' threads is below the fold");
    shell.show(41, PanelKind::Diff, OpenMode::Permanent);
    shell.click_named(&format!("thread-row-{pos}"));
    assert!(shell.shown_tabs().iter().any(|tab| tab == CONVERSATION));
    assert!(
        in_view(&mut shell, &format!("inline-thread-{id}")),
        "the Conversation scrolled to it"
    );
    assert!(highlighted(&mut shell, &key, &id));
}

#[gpui::test]
fn a_double_click_on_a_thread_row_jumps_to_its_line(cx: &mut TestAppContext) {
    let mut shell = open_shell(
        cx,
        Options {
            open_pr: Some(41),
            ..Options::default()
        },
    );
    let order = sidebar_order(&threads(&mut shell));
    shell.double_click("thread-row-0");
    assert!(shell.shown_tabs().iter().any(|tab| tab == DIFF));
    assert_eq!(diff_current(&mut shell), Some(order[0].clone()));
}

#[gpui::test]
fn compact_threads_unfold_when_shown_and_fold_from_the_menu(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    shell.show(41, PanelKind::Conversation, OpenMode::Permanent);
    shell.click("thread-style-1");
    assert_eq!(style(&mut shell), ThreadStyle::Compact);
    let key = shell.key(41);
    let order = sidebar_order(&threads(&mut shell));
    let (shown, other) = (order[0].clone(), order[1].clone());
    shell.click("thread-row-0");
    assert!(shell.drawn(&format!("inline-thread-{shown}")), "unfolded");
    assert!(
        shell.drawn(&format!("inline-row-{other}")),
        "the others stay rows"
    );

    let menu = ThreadMenu::new(key.clone(), shown.clone(), Origin::Sidebar);
    let fold = entry(&mut shell, &menu, |a| a == ThreadAction::ToggleFolded);
    assert_eq!(fold.label, "Collapse");
    run(&mut shell, &menu, ThreadAction::ToggleFolded);
    assert!(folded(&mut shell, &key, &shown));
    assert!(shell.drawn(&format!("inline-row-{shown}")), "a row again");
    let fold = entry(&mut shell, &menu, |a| a == ThreadAction::ToggleFolded);
    assert_eq!(fold.label, "Expand");
}

#[gpui::test]
fn resolving_on_a_forge_that_cannot_is_disabled_with_the_reason(cx: &mut TestAppContext) {
    let fixtures = Fixtures::load();
    let number = fixtures
        .inbox
        .prs
        .iter()
        .find(|row| row.forge == ForgeKind::Forgejo)
        .expect("a Forgejo fixture")
        .key
        .number
        .0;
    let mut shell = open_shell(
        cx,
        Options {
            open_pr: Some(number),
            ..Options::default()
        },
    );
    let key = shell.key(number);
    let thread = threads(&mut shell)
        .iter()
        .find(|t| t.anchor.is_some())
        .expect("an inline thread")
        .clone();
    let menu = ThreadMenu::new(key, thread.id, Origin::Sidebar);
    let resolve = entry(&mut shell, &menu, |a| {
        matches!(a, ThreadAction::SetResolved(_))
    });
    assert_eq!(
        resolve.blocked.as_deref(),
        Some("Forgejo does not let you resolve threads from here")
    );
    let link = entry(&mut shell, &menu, |a| a == ThreadAction::CopyLink);
    assert_eq!(
        link.blocked.as_deref(),
        Some("The forge gave no address for it"),
        "fixture comments have no permalink"
    );
    let reply = entry(&mut shell, &menu, |a| a == ThreadAction::Reply);
    assert_eq!(reply.blocked, None);
}

#[gpui::test]
fn the_menu_opens_on_diff_cards_and_conversation_cards(cx: &mut TestAppContext) {
    let github = Fixtures::load()
        .inbox
        .prs
        .iter()
        .find(|row| row.forge == ForgeKind::GitHub)
        .expect("a GitHub fixture")
        .key
        .number
        .0;
    let mut shell = open_shell(
        cx,
        Options {
            open_pr: Some(github),
            ..Options::default()
        },
    );
    let key = shell.key(github);
    let order = sidebar_order(&threads(&mut shell));
    let id = order[0].clone();

    // The sidebar row.
    right_click(&mut shell, "thread-row-0");
    assert!(shell.drawn("thread-menu-show-in-conversation"));
    shell.keys("escape");

    // A card in the diff: its entries are the thread's, and Collapse folds it.
    shell.double_click("thread-row-0");
    let diff = shell.diff_panel();
    let cards = shell.update(|_, cx| diff.read(cx).cards().clone());
    let ix = shell.update(|_, cx| {
        cards
            .read(cx)
            .threads()
            .iter()
            .position(|t| t.id == id)
            .expect("the card")
    });
    let open_card = shell.update(|_, cx| !cards.read(cx).is_collapsed(ix, cx));
    let head = if open_card {
        format!("card-head-{ix}")
    } else {
        format!("card-row-{ix}")
    };
    right_click(&mut shell, &head);
    assert!(shell.drawn("thread-menu-jump-to-line"), "the thread's menu");
    shell.click("thread-menu-fold");
    assert_ne!(
        shell.update(|_, cx| !cards.read(cx).is_collapsed(ix, cx)),
        open_card,
        "the menu folded the card"
    );

    // An inline thread in the Conversation, on its head and on a comment.
    shell.show(github, PanelKind::Conversation, OpenMode::Permanent);
    let menu = ThreadMenu::new(key.clone(), id.clone(), Origin::Sidebar);
    run(&mut shell, &menu, ThreadAction::ShowInConversation);
    right_click(&mut shell, &format!("inline-head-{id}"));
    assert!(shell.drawn("thread-menu-reply"));
    shell.keys("escape");
    let first = threads(&mut shell)
        .iter()
        .find(|t| t.id == id)
        .expect("the thread")
        .comments[0]
        .id
        .clone();
    right_click(&mut shell, &format!("inline-comment-{first}"));
    assert!(
        shell.drawn("thread-menu-resolve") || shell.drawn("thread-menu-unresolve"),
        "a comment's menu has the thread's entries"
    );
    shell.click("thread-menu-fold");
    assert!(folded(&mut shell, &key, &id), "folded in the Conversation");
}

/// A GitHub forge that serves one pull request with `threads` and resolves
/// them, recording each change.
struct ThreadForge {
    host: String,
    pull_request: forge::model::PullRequest,
    threads: Vec<Thread>,
    resolved: Mutex<Vec<(String, bool)>>,
}

#[async_trait::async_trait]
impl forge::Forge for ThreadForge {
    fn kind(&self) -> ForgeKind {
        ForgeKind::GitHub
    }

    fn host(&self) -> &str {
        &self.host
    }

    fn capabilities(&self) -> forge::Capabilities {
        forge::Capabilities::baseline(ForgeKind::GitHub)
    }

    async fn whoami(&self) -> forge::Result<User> {
        Ok(User::new("tester"))
    }

    async fn inbox(
        &self,
        _: &forge::model::InboxSection,
    ) -> forge::Result<Vec<forge::model::PullRequest>> {
        Ok(Vec::new())
    }

    async fn pull_request(
        &self,
        _: &RepoRef,
        _: PrNumber,
    ) -> forge::Result<forge::model::PullRequest> {
        Ok(self.pull_request.clone())
    }

    async fn threads(&self, _: &RepoRef, _: PrNumber) -> forge::Result<Vec<Thread>> {
        Ok(self.threads.clone())
    }

    async fn checks(&self, _: &RepoRef, _: PrNumber) -> forge::Result<Vec<forge::model::CheckRun>> {
        Ok(Vec::new())
    }

    async fn stack(&self, _: &RepoRef, _: PrNumber) -> forge::Result<forge::model::Stack> {
        Err(forge::Error::Unsupported {
            kind: ForgeKind::GitHub,
            feature: "stacks",
        })
    }

    async fn set_resolved(
        &self,
        _: &RepoRef,
        _: PrNumber,
        thread: &ThreadId,
        resolved: bool,
    ) -> forge::Result<Thread> {
        self.resolved
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((thread.as_str().to_owned(), resolved));
        let mut found = self
            .threads
            .iter()
            .find(|t| &t.id == thread)
            .cloned()
            .ok_or_else(|| forge::Error::NotFound {
                request: format!("thread {}", thread.as_str()),
            })?;
        found.resolution = if resolved {
            Resolution::Resolved {
                by: Some(User::new("tester")),
            }
        } else {
            Resolution::Unresolved
        };
        Ok(found)
    }
}

fn inline_thread(id: &str, comment: &str, line: u32) -> Thread {
    Thread {
        id: ThreadId::new(id),
        anchor: Some(Anchor {
            path: "src/lib.rs".into(),
            old_path: None,
            position: Some(Position {
                line: LineRef {
                    side: Side::New,
                    line,
                },
                start: None,
                commit: None,
            }),
            original: None,
        }),
        resolution: Resolution::Unresolved,
        outdated: false,
        comments: vec![Comment {
            id: CommentId::new(comment),
            author: User::new("mira"),
            body: "Why this way?".into(),
            created_at: "2026-11-17T12:00:00Z".parse().expect("a time"),
            edited_at: None,
            web_url: None,
            review: None,
            verdict: None,
            suggestions: Vec::new(),
        }],
    }
}

#[gpui::test]
fn the_menu_resolves_copies_and_opens_through_the_forge(cx: &mut TestAppContext) {
    let fixtures = Fixtures::load();
    let row = fixtures
        .inbox
        .prs
        .iter()
        .find(|row| row.forge == ForgeKind::GitHub)
        .expect("a GitHub fixture");
    let pr = fixtures.forge_pr(&row.key).expect("its forge record");
    let scratch = Scratch::new("thread-menus");
    let fake = Arc::new(ThreadForge {
        host: pr.repo.host.clone(),
        pull_request: pr.clone(),
        threads: vec![inline_thread("T1", "42", 3), inline_thread("T2", "43", 9)],
        resolved: Mutex::default(),
    });
    let services = fake_services(
        &scratch,
        vec![fake.clone() as Arc<dyn forge::Forge>],
        std::slice::from_ref(&pr.repo),
    );
    let mut shell = open_shell(cx, Options::default());
    go_live(&mut shell, services, vec![pr.clone()], ForgeKind::GitHub);
    shell.keys("j enter");
    let session = shell.session();
    shell.settle_until("the threads from the fake forge", |cx| {
        session
            .read(cx)
            .open_pr()
            .is_some_and(|open| open.threads.len() == 2)
    });
    let key = shell.update(|_, cx| session.read(cx).open_key().cloned().expect("open"));
    let menu = ThreadMenu::new(key.clone(), ThreadId::new("T1"), Origin::Sidebar);

    let link = format!(
        "{}#discussion_r42",
        app::references::pr_url(ForgeKind::GitHub, &key.repo, key.number)
    );
    run(&mut shell, &menu, ThreadAction::CopyLink);
    assert_eq!(
        shell.cx.read_from_clipboard().and_then(|item| item.text()),
        Some(link.clone())
    );
    run(&mut shell, &menu, ThreadAction::OpenOnForge);
    assert_eq!(shell.cx.opened_url(), Some(link));

    let resolve = entry(&mut shell, &menu, |a| a == ThreadAction::SetResolved(true));
    assert_eq!((resolve.label, resolve.blocked), ("Resolve", None));
    run(&mut shell, &menu, ThreadAction::SetResolved(true));
    shell.settle_until("the resolve at the forge", |cx| {
        session.read(cx).open_pr().is_some_and(|open| {
            open.threads
                .iter()
                .any(|t| t.id.as_str() == "T1" && t.is_resolved())
        })
    });
    assert_eq!(
        *fake.resolved.lock().unwrap_or_else(PoisonError::into_inner),
        [("T1".to_owned(), true)]
    );
    let unresolve = entry(&mut shell, &menu, |a| {
        matches!(a, ThreadAction::SetResolved(_))
    });
    assert_eq!(
        (unresolve.action, unresolve.label),
        (ThreadAction::SetResolved(false), "Unresolve")
    );

    run(&mut shell, &menu, ThreadAction::Reply);
    let writing = shell.writing();
    let replying = shell.update(|_, cx| {
        writing
            .read(cx)
            .is_open(&app::threads::reply_anchor(&inline_thread("T1", "42", 3)))
    });
    assert!(replying, "the reply composer is open");
    assert!(
        shell.panel_of(key.number.0, PanelKind::Diff).is_some(),
        "where it is written: the diff"
    );

    run(&mut shell, &menu, ThreadAction::ShowInConversation);
    assert!(shell.shown_tabs().iter().any(|tab| tab == CONVERSATION));
    assert!(highlighted(&mut shell, &key, &ThreadId::new("T1")));
}

/// A jump to a thread in a viewed, so collapsed, file opens the file, or
/// the thread's card would never be drawn.
#[gpui::test]
fn a_jump_opens_the_collapsed_file_of_the_thread(cx: &mut TestAppContext) {
    let mut shell = open_shell(
        cx,
        Options {
            open_pr: Some(41),
            ..Options::default()
        },
    );
    let view = shell.diff_view();
    let target = shell.update(|_, cx| {
        let view = view.read(cx);
        let files = &view.list().diff().files;
        let open = Session::global(cx).read(cx).open_pr().expect("open");
        open.threads.iter().find_map(|thread| {
            let path = &thread.anchor.as_ref()?.path;
            let ix = files.iter().position(|f| f.path == *path)?;
            let file = diff::FileIx(u32::try_from(ix).ok()?);
            view.is_file_collapsed(file)
                .then(|| (thread.id.clone(), file))
        })
    });
    let (thread, file) = target.expect("the fixture has a thread in a viewed file");

    let session = shell.session();
    shell.update(|_, cx| session.update(cx, |s, cx| s.jump_to_thread(thread, false, cx)));
    shell.settle();
    assert!(
        !shell.update(|_, cx| view.read(cx).is_file_collapsed(file)),
        "the thread's file opened"
    );
}

#[gpui::test]
fn a_resolved_thread_starts_folded_and_its_chevron_toggles_it(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    shell.show(41, PanelKind::Conversation, OpenMode::Permanent);
    let key = shell.key(41);
    let all = threads(&mut shell);
    let resolved = all
        .iter()
        .find(|t| t.anchor.is_some() && t.is_resolved())
        .expect("#41 has a resolved inline thread")
        .id
        .clone();
    let open = all
        .iter()
        .find(|t| t.anchor.is_some() && !t.is_resolved())
        .expect("#41 has an open inline thread")
        .id
        .clone();
    assert!(folded(&mut shell, &key, &resolved), "resolved: one row");
    assert!(!folded(&mut shell, &key, &open), "open: in full");

    let toggle = format!("inline-fold-{resolved}");
    let body = shell.bounds_of("conversation-body").center();
    for _ in 0..40 {
        if in_view(&mut shell, &toggle) {
            break;
        }
        shell.scroll_at(body, 150.0);
    }
    assert!(in_view(&mut shell, &toggle), "scrolled to the folded row");
    let at = |shell: &mut ShellWindow, toggle: &str, frame: String| {
        let chevron = shell.bounds_of(String::leak(toggle.to_owned()));
        let frame = shell.bounds_of(String::leak(frame));
        chevron.origin - frame.origin
    };
    let row = at(&mut shell, &toggle, format!("inline-row-{resolved}"));
    shell.click_named(&toggle);
    assert!(
        !folded(&mut shell, &key, &resolved),
        "the chevron unfolds it"
    );
    assert!(shell.drawn(&format!("inline-head-{resolved}")));
    let card = at(&mut shell, &toggle, format!("inline-thread-{resolved}"));
    assert_eq!(row, card, "the chevron stays where it was");
    shell.click_named(&toggle);
    assert!(folded(&mut shell, &key, &resolved), "and folds it again");
    assert!(shell.drawn(&format!("inline-row-{resolved}")));
}

#[gpui::test]
fn the_conversation_card_resolves_its_thread(cx: &mut TestAppContext) {
    let fixtures = Fixtures::load();
    let row = fixtures
        .inbox
        .prs
        .iter()
        .find(|row| row.forge == ForgeKind::GitHub)
        .expect("a GitHub fixture");
    let pr = fixtures.forge_pr(&row.key).expect("its forge record");
    let scratch = Scratch::new("conversation-resolve");
    let fake = Arc::new(ThreadForge {
        host: pr.repo.host.clone(),
        pull_request: pr.clone(),
        threads: vec![inline_thread("T1", "42", 3)],
        resolved: Mutex::default(),
    });
    let services = fake_services(
        &scratch,
        vec![fake.clone() as Arc<dyn forge::Forge>],
        std::slice::from_ref(&pr.repo),
    );
    let mut shell = open_shell(cx, Options::default());
    go_live(&mut shell, services, vec![pr.clone()], ForgeKind::GitHub);
    shell.keys("j enter");
    let session = shell.session();
    shell.settle_until("the thread from the fake forge", |cx| {
        session
            .read(cx)
            .open_pr()
            .is_some_and(|open| open.threads.len() == 1)
    });
    let thread = shell.update(|_, cx| session.read(cx).open_pr().expect("open").threads[0].clone());
    let button = format!("conversation-resolve-{}", app::threads::ix_of(&thread));
    crate::conversation::reveal(&mut shell, &button);
    shell.click_named(&button);
    shell.settle_until("the resolve at the forge", |cx| {
        session
            .read(cx)
            .open_pr()
            .is_some_and(|open| open.threads.iter().any(|t| t.is_resolved()))
    });
    assert_eq!(
        *fake.resolved.lock().unwrap_or_else(PoisonError::into_inner),
        [("T1".to_owned(), true)]
    );
}
