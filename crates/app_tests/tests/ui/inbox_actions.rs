//! An inbox row's own actions: its right-click menu (reviews, merge, close,
//! reopen, draft, re-review, copying, the local marks) with the reasons a
//! forbidden entry is disabled, the line those actions confirm in, the
//! viewer's review label, refetching the inbox by hand, and a repeated click
//! toggling between the diff and the conversation.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use app::inbox::row_menu::{self, RowAction, RowEntry};
use app::model::{Inbox, PrKey, ReviewState, ReviewStatus, RowReview};
use app::panels::PanelKind;
use app::session::OpenMode;
use app::sink::Verdict;
use forge::model::{ForgeKind, MergeMethod, PrState};
use gpui::{Modifiers, MouseButton, SharedString, Task, TestAppContext};

use crate::harness::{
    FakeForge, Options, Scratch, ShellWindow, fake_services, go_live, open_shell,
};

fn entries(shell: &mut ShellWindow, number: u64) -> Vec<RowEntry> {
    let key = shell.key(number);
    shell.update(|_, cx| row_menu::entries(&key, cx).into_iter().flatten().collect())
}

fn entry(shell: &mut ShellWindow, number: u64, action: &RowAction) -> RowEntry {
    entries(shell, number)
        .into_iter()
        .find(|entry| &entry.action == action)
        .unwrap_or_else(|| panic!("#{number} has no {action:?} entry"))
}

fn run(shell: &mut ShellWindow, number: u64, action: RowAction) {
    let key = shell.key(number);
    shell.update(|window, cx| row_menu::run(action, &key, window, cx));
    shell.settle();
}

fn row(shell: &mut ShellWindow, key: &PrKey) -> app::model::PullRequest {
    let session = shell.session();
    shell.update(|_, cx| {
        session
            .read(cx)
            .inbox
            .get(key)
            .cloned()
            .expect("the inbox lists it")
    })
}

fn label(shell: &mut ShellWindow, key: &PrKey) -> RowReview {
    let session = shell.session();
    let row = row(shell, key);
    shell.update(|_, cx| row.row_review(session.read(cx).account(&key.repo.host)))
}

fn acting(shell: &mut ShellWindow) -> Option<u64> {
    let sidebar = shell.sidebar();
    shell.update(|_, cx| sidebar.read(cx).row_action_key().map(|key| key.number.0))
}

fn clipboard(shell: &mut ShellWindow) -> Option<String> {
    shell.cx.read_from_clipboard().and_then(|item| item.text())
}

#[gpui::test]
fn forbidden_entries_are_disabled_with_the_reason(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    // #41 is the fixture user's own draft on Forgejo, with failing checks.
    let approve = entry(&mut shell, 41, &RowAction::Verdict(Verdict::Approve));
    assert_eq!(
        approve.blocked.as_deref(),
        Some("Forgejo does not let you approve your own pull request")
    );
    let merges: Vec<RowEntry> = entries(&mut shell, 41)
        .into_iter()
        .filter(|entry| matches!(entry.action, RowAction::Merge(_)))
        .collect();
    assert!(!merges.is_empty());
    for merge in &merges {
        let why = merge.blocked.as_deref().expect("a draft cannot merge");
        assert!(
            why.starts_with("Cannot merge:") && why.contains("draft"),
            "{why}"
        );
    }
    assert!(
        entries(&mut shell, 41)
            .iter()
            .any(|entry| entry.action == RowAction::SetDraft(false) && entry.label == "Mark ready"),
        "a draft offers Mark ready"
    );
    let rereview = entry(&mut shell, 41, &RowAction::RequestReReview);
    assert_eq!(
        rereview.blocked.as_deref(),
        Some("No one has reviewed it yet")
    );

    // #42 asks the fixture user to review: everything goes, a merge by any
    // method the repository allows.
    let open: Vec<RowEntry> = entries(&mut shell, 42);
    for verdict in [Verdict::Approve, Verdict::RequestChanges, Verdict::Comment] {
        let found = open
            .iter()
            .find(|entry| entry.action == RowAction::Verdict(verdict))
            .expect("every verdict is listed");
        assert_eq!(found.blocked, None, "{verdict:?}");
    }
    assert!(open.iter().any(
        |entry| entry.action == RowAction::Merge(Some(MergeMethod::Squash))
            && entry.blocked.is_none()
    ));
    assert!(open.iter().any(|entry| entry.action == RowAction::Close));
    assert!(
        open.iter()
            .any(|entry| entry.action == RowAction::SetDraft(true))
    );
}

#[gpui::test]
fn a_right_click_opens_the_rows_menu(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    let at = shell.bounds_of("inbox-title-42").center();
    let none = Modifiers::none();
    shell.cx.simulate_mouse_move(at, None, none);
    shell.cx.simulate_mouse_down(at, MouseButton::Right, none);
    shell.cx.simulate_mouse_up(at, MouseButton::Right, none);
    shell.settle();
    assert!(
        shell.element(SharedString::from("popup-menu")).is_some(),
        "the row's menu is open"
    );
    assert!(
        shell.open_numbers().is_empty(),
        "a right click opens nothing"
    );
}

#[gpui::test]
fn copy_and_open_entries_use_the_pull_request(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    run(&mut shell, 42, RowAction::CopyReference);
    assert_eq!(clipboard(&mut shell).as_deref(), Some("cora/review#42"));
    run(&mut shell, 42, RowAction::CopyLink);
    let link = "https://forgejo.example.com/cora/review/pulls/42";
    assert_eq!(clipboard(&mut shell).as_deref(), Some(link));
    let key = shell.key(42);
    let head = row(&mut shell, &key).head.to_string();
    run(&mut shell, 42, RowAction::CopyBranch);
    assert_eq!(clipboard(&mut shell), Some(head));
    run(&mut shell, 42, RowAction::OpenOnForge);
    assert_eq!(shell.cx.opened_url().as_deref(), Some(link));
}

#[gpui::test]
fn approve_from_the_menu_sends_at_once_and_patches_the_row(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    let key = shell.key(42);
    assert_eq!(
        label(&mut shell, &key),
        RowReview::Mine(ReviewStatus::ReviewRequested)
    );
    run(&mut shell, 42, RowAction::Verdict(Verdict::Approve));
    assert_eq!(
        label(&mut shell, &key),
        RowReview::Overall(ReviewState::Approved),
        "the row says it was approved"
    );
    assert_eq!(acting(&mut shell), None, "no line to confirm or dismiss");
    assert!(!shell.drawn("inbox-action"));
    assert!(
        shell.open_numbers().is_empty(),
        "loaded for the action only, then released"
    );
}

#[gpui::test]
fn request_changes_needs_a_summary(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    let key = shell.key(1288);
    run(
        &mut shell,
        1288,
        RowAction::Verdict(Verdict::RequestChanges),
    );
    shell.click_element(SharedString::from("inbox-action-confirm"));
    assert!(
        shell.open_numbers().is_empty(),
        "an empty summary sends nothing"
    );
    shell.type_text("Please add a test");
    shell.click_element(SharedString::from("inbox-action-confirm"));
    assert_eq!(
        label(&mut shell, &key),
        RowReview::Overall(ReviewState::ChangesRequested)
    );
}

#[gpui::test]
fn close_asks_first_and_reopen_too(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    let key = shell.key(42);
    run(&mut shell, 42, RowAction::Close);
    assert!(shell.drawn("inbox-action"));
    assert_eq!(
        row(&mut shell, &key).state,
        PrState::Open,
        "not before the confirm"
    );
    shell.click_element(SharedString::from("inbox-action-confirm"));
    assert_eq!(row(&mut shell, &key).state, PrState::Closed);
    shell.click_element(SharedString::from("inbox-action-cancel"));
    assert!(shell.open_numbers().is_empty());

    assert!(
        entries(&mut shell, 42)
            .iter()
            .any(|entry| entry.action == RowAction::Reopen && entry.blocked.is_none())
    );
    run(&mut shell, 42, RowAction::Reopen);
    assert_eq!(row(&mut shell, &key).state, PrState::Closed, "asks first");
    shell.click_element(SharedString::from("inbox-action-confirm"));
    assert_eq!(row(&mut shell, &key).state, PrState::Open);

    run(&mut shell, 42, RowAction::SetDraft(true));
    assert_eq!(
        row(&mut shell, &key).state,
        PrState::Draft,
        "no confirmation for drafts"
    );
}

#[gpui::test]
fn re_review_asks_whoever_reviewed_again(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    // #38 is the fixture user's own, approved by mira.
    let key = shell.key(38);
    assert_eq!(
        entry(&mut shell, 38, &RowAction::RequestReReview).blocked,
        None
    );
    run(&mut shell, 38, RowAction::RequestReReview);
    let session = shell.session();
    let mira = shell.update(|_, cx| {
        let record = session.read(cx).forge_record(&key).expect("its record");
        record
            .reviewers
            .iter()
            .find(|r| r.user.login == "mira")
            .map(|r| (r.state, r.requested_again))
    });
    assert_eq!(
        mira,
        Some((forge::model::ReviewState::Requested, true)),
        "asked again"
    );
}

/// A GitHub fixture on a [`FakeForge`], live, its row in the inbox.
fn live(
    cx: &mut TestAppContext,
    scratch: &Scratch,
) -> (
    ShellWindow,
    Arc<FakeForge>,
    PrKey,
    forge::model::PullRequest,
) {
    let fixtures = app::fixtures::Fixtures::load();
    let key = fixtures
        .inbox
        .prs
        .iter()
        .find(|pr| pr.forge == ForgeKind::GitHub && pr.state == PrState::Open)
        .map(|pr| pr.key.clone())
        .expect("an open GitHub fixture");
    let pr = fixtures.forge_pr(&key).expect("its forge record");
    let fake = FakeForge::with_pull_request(
        ForgeKind::GitHub,
        &pr.repo.host,
        Vec::new(),
        Some(pr.clone()),
    );
    let services = fake_services(
        scratch,
        vec![fake.clone() as Arc<dyn forge::Forge>],
        std::slice::from_ref(&pr.repo),
    );
    let mut shell = open_shell(cx, Options::default());
    go_live(&mut shell, services, vec![pr.clone()], ForgeKind::GitHub);
    (shell, fake, key, pr)
}

#[gpui::test]
fn a_merge_from_the_row_confirms_at_the_head_it_recorded(cx: &mut TestAppContext) {
    let scratch = Scratch::new("row-merge");
    let (mut shell, fake, key, pr) = live(cx, &scratch);
    let number = key.number.0;
    run(
        &mut shell,
        number,
        RowAction::Merge(Some(MergeMethod::Squash)),
    );
    let session = shell.session();
    let waiting = key.clone();
    shell.settle_until("the merge to confirm", |cx| {
        session
            .read(cx)
            .pr(&waiting)
            .is_some_and(|open| open.lifecycle.confirm.is_some())
    });
    assert!(fake.retries().is_empty(), "nothing sent before the confirm");
    assert_eq!(shell.focused(), None, "loaded without taking the focus");
    let asked = shell.update(|_, cx| {
        session
            .read(cx)
            .pr(&key)
            .and_then(|open| open.lifecycle.confirm.clone())
            .expect("a merge to confirm")
    });
    assert_eq!(asked.head(), Some(&pr.head.sha), "the head on screen");
    assert!(shell.drawn("inbox-action"));
    shell.click_element(SharedString::from("inbox-action-confirm"));
    let merged = key.clone();
    shell.settle_until("the merge at the forge", |cx| {
        session
            .read(cx)
            .inbox
            .get(&merged)
            .is_some_and(|row| row.state == PrState::Merged)
    });
    let sent = fake.retries();
    assert_eq!(sent.len(), 1);
    assert!(
        sent[0].starts_with(&format!("merge {number} Squash"))
            && sent[0].ends_with(pr.head.sha.as_str()),
        "{sent:?}"
    );
    shell.click_element(SharedString::from("inbox-action-cancel"));
    assert!(shell.open_numbers().is_empty(), "released once dismissed");
}

#[gpui::test]
fn the_row_reads_re_review_requested_when_asked_again(cx: &mut TestAppContext) {
    use forge::model::{ReviewDecision, ReviewState as Forge, Reviewer, User};

    let scratch = Scratch::new("rereview-label");
    let fixtures = app::fixtures::Fixtures::load();
    // GitLab: someone approved the head; the viewer approved earlier and was
    // asked again. The decision alone says approved.
    let key = shell_key(&fixtures, ForgeKind::GitLab);
    let mut pr = fixtures.forge_pr(&key).expect("its record");
    let mut me = Reviewer::new(User::new("cora"), Forge::Requested);
    me.requested_again = true;
    pr.reviewers = vec![Reviewer::new(User::new("mira"), Forge::Approved), me];
    pr.review_decision = ReviewDecision::from_reviewers(&pr.reviewers);
    assert_eq!(pr.review_decision, ReviewDecision::Approved);
    let fake = FakeForge::with_pull_request(ForgeKind::GitLab, &pr.repo.host, Vec::new(), None);
    let services = fake_services(
        &scratch,
        vec![fake as Arc<dyn forge::Forge>],
        std::slice::from_ref(&pr.repo),
    );
    let mut shell = open_shell(cx, Options::default());
    go_live(&mut shell, services, vec![pr], ForgeKind::GitLab);
    assert!(shell.drawn(&format!("inbox-review-{}", key.number.0)));
    assert_eq!(
        label(&mut shell, &key),
        RowReview::Mine(ReviewStatus::ReReviewRequested)
    );
    assert_eq!(label(&mut shell, &key).label(), "re-review requested");
}

fn shell_key(fixtures: &app::fixtures::Fixtures, forge: ForgeKind) -> PrKey {
    fixtures
        .inbox
        .prs
        .iter()
        .find(|pr| pr.forge == forge && pr.state == PrState::Open)
        .map(|pr| pr.key.clone())
        .expect("an open fixture on that forge")
}

fn offset(shell: &mut ShellWindow, section: usize) -> f64 {
    let sidebar = shell.sidebar();
    shell.update(|_, cx| {
        sidebar.update(cx, |sidebar, cx| sidebar.scroll_section(section, 0.0, cx).0)
    })
}

#[gpui::test]
fn refresh_refetches_in_place_and_keeps_the_selection(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    shell.resize(1440.0, 520.0);
    let session = shell.session();
    let calls = Rc::new(Cell::new(0));
    let (finish, finished) = futures::channel::oneshot::channel::<()>();
    let finished = Rc::new(Cell::new(Some(finished)));
    let counted = calls.clone();
    let patched = session.downgrade();
    shell.update(|_, cx| {
        session.update(cx, |session, _| {
            session.set_inbox_refresher(Rc::new(move |cx: &mut gpui::App| {
                counted.set(counted.get() + 1);
                // The forge answers with a new title for #42.
                if let Some(session) = patched.upgrade() {
                    session.update(cx, |session, cx| {
                        let mut inbox = Inbox::clone(&session.inbox);
                        if let Some(pr) = inbox.prs.iter_mut().find(|pr| pr.number() == 42) {
                            pr.title = format!("{} (edited)", pr.title).into();
                        }
                        session.set_inbox(inbox, cx);
                    });
                }
                match finished.take() {
                    Some(wait) => cx.background_executor().spawn(async move {
                        let _ = wait.await;
                    }),
                    None => Task::ready(()),
                }
            }));
        });
    });
    shell.keys("j j");
    let sidebar = shell.sidebar();
    let selected = shell.update(|_, cx| sidebar.read(cx).cursor_key().cloned());
    assert!(selected.is_some());
    shell.update(|_, cx| sidebar.update(cx, |sidebar, cx| sidebar.scroll_section(0, 46.0, cx)));
    shell.settle();
    let scrolled = offset(&mut shell, 0);

    shell.click("inbox-refresh");
    assert_eq!(calls.get(), 1);
    assert!(
        shell.update(|_, cx| session.read(cx).inbox_refresh.running),
        "the icon turns while the forges answer"
    );
    shell.click("inbox-refresh");
    assert_eq!(calls.get(), 1, "one at a time");
    let _ = finish.send(());
    shell.settle_until("the refresh to finish", |cx| {
        !session.read(cx).inbox_refresh.running
    });
    let refreshed = shell.update(|_, cx| session.read(cx).inbox_refresh.last.len());
    let forges = shell.update(|_, cx| session.read(cx).forges.len());
    assert_eq!(refreshed, forges, "the tooltip has each forge's time");
    let key = shell.key(42);
    assert!(
        row(&mut shell, &key).title.ends_with("(edited)"),
        "patched in place"
    );
    assert_eq!(
        shell.update(|_, cx| sidebar.read(cx).cursor_key().cloned()),
        selected,
        "the selection stays"
    );
    assert_eq!(offset(&mut shell, 0), scrolled, "and the scroll");

    // F5 and Ctrl+R while the inbox has the keys: a row click leaves them
    // there.
    shell.click("inbox-title-1288");
    shell.keys("f5");
    assert_eq!(calls.get(), 2);
    shell.keys("ctrl-r");
    assert_eq!(calls.get(), 3);
}

#[gpui::test]
fn further_clicks_toggle_between_diff_and_conversation(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    let shown = |shell: &mut ShellWindow, kind: PanelKind| {
        let id = shell.panel_of(42, kind).expect("the panel");
        shell.is_shown(id)
    };
    shell.click("inbox-title-42");
    assert!(
        shown(&mut shell, PanelKind::Conversation),
        "the first click"
    );
    shell.click("inbox-title-42");
    assert!(shown(&mut shell, PanelKind::Diff), "the second");
    shell.click("inbox-title-42");
    assert!(shown(&mut shell, PanelKind::Conversation), "the third");
    shell.keys("enter");
    assert!(shown(&mut shell, PanelKind::Diff), "Enter toggles too");
    shell.keys("enter");
    assert!(shown(&mut shell, PanelKind::Conversation));
}

#[gpui::test]
fn the_more_button_opens_the_same_menu_as_a_right_click(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    assert!(
        shell.element(SharedString::from("popup-menu")).is_none(),
        "no menu before the click"
    );
    shell.click("inbox-more-42");
    assert!(
        shell.element(SharedString::from("popup-menu")).is_some(),
        "the button opens the row's menu"
    );
    assert!(
        shell.open_numbers().is_empty(),
        "the button does not also open the row"
    );
}

#[gpui::test]
fn the_menu_has_the_new_sections(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    let key = shell.key(42);
    let groups = shell.update(|_, cx| row_menu::entries(&key, cx));
    assert_eq!(groups.len(), 6, "{groups:#?}");
    let flat = entries(&mut shell, 42);
    for (action, label) in [
        (
            RowAction::Open(PanelKind::Diff, OpenMode::Permanent),
            "Open diff",
        ),
        (RowAction::OpenOnMachine, "Open on\u{2026}"),
        (RowAction::ReviewWithAgent, "Review with agent"),
        (RowAction::CopyNumber, "Copy number"),
        (RowAction::HideRepo, "Hide this repository"),
    ] {
        assert_eq!(entry(&mut shell, 42, &action).label, label);
    }
    assert!(flat.iter().all(|entry| !entry.label.is_empty()));
    assert!(
        !flat.iter().any(|entry| entry.label == "Open checks"),
        "the checks are in the Conversation, not a tab of their own"
    );
}

#[gpui::test]
fn open_in_a_new_tab_and_open_diff_bring_those_panels_forward(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    run(
        &mut shell,
        42,
        RowAction::Open(PanelKind::Conversation, OpenMode::Permanent),
    );
    let conversation = shell
        .panel_of(42, PanelKind::Conversation)
        .expect("a Conversation panel for #42");
    assert!(shell.is_shown(conversation), "the Conversation is in front");
    run(
        &mut shell,
        42,
        RowAction::Open(PanelKind::Diff, OpenMode::Permanent),
    );
    let diff = shell
        .panel_of(42, PanelKind::Diff)
        .expect("a Diff panel for #42");
    assert!(shell.is_shown(diff), "then the Diff");
}

#[gpui::test]
fn copy_number_and_hide_repo_act_on_the_row(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    run(&mut shell, 42, RowAction::CopyNumber);
    assert_eq!(clipboard(&mut shell).as_deref(), Some("42"));

    assert!(shell.drawn("inbox-row-42"));
    run(&mut shell, 42, RowAction::HideRepo);
    assert!(!shell.drawn("inbox-row-42"), "its repository is hidden");
}

#[gpui::test]
fn open_on_asks_for_a_machine_and_opens_the_pull_request(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    run(&mut shell, 42, RowAction::OpenOnMachine);
    assert_eq!(shell.open_numbers(), [42]);
    assert!(shell.drawn("machine-picker"));
}
