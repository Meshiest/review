//! The pull request header: where the pull request lives and the actions on
//! it (review verdicts, merge, draft, close), on fixtures and on a fake forge.
//! The action tests run under the Vercel theme.

use std::sync::Arc;

use app::appearance::ThemeFlags;
use app::fixtures::Fixtures;
use app::lifecycle::{Action, Load};
use app::panels::PanelKind;
use app::pr_header::{Menu, PrActions};
use app::session::OpenMode;
use app::sink::Verdict;
use forge::model::{ForgeKind, MergeMethod, PrState};
use gpui::{Entity, SharedString, TestAppContext};

use crate::harness::{
    FakeForge, Options, Scratch, ShellWindow, fake_services, flat, go_live, open_shell,
};

fn state(shell: &mut ShellWindow, number: u64) -> PrState {
    let key = shell.key(number);
    let session = shell.session();
    shell.update(|_, cx| {
        session
            .read(cx)
            .pr(&key)
            .expect("the pull request is open")
            .pr
            .state
    })
}

fn row_state(shell: &mut ShellWindow, number: u64) -> PrState {
    let key = shell.key(number);
    let session = shell.session();
    shell.update(|_, cx| {
        session
            .read(cx)
            .inbox
            .get(&key)
            .expect("the inbox lists it")
            .state
    })
}

fn confirming(shell: &mut ShellWindow) -> bool {
    let session = shell.session();
    shell.update(|_, cx| {
        session
            .read(cx)
            .open_pr()
            .is_some_and(|open| open.lifecycle.confirm.is_some())
    })
}

/// The action bar of the focused pull request's Conversation.
fn actions(shell: &mut ShellWindow) -> Entity<PrActions> {
    let id = shell.panel_id(app::panels::CONVERSATION);
    let dock = shell.dock();
    shell.update(|_, cx| {
        let view = dock.read(cx).panel(id).expect("the conversation").view();
        let panel = view
            .downcast::<app::panels::ConversationPanel>()
            .expect("a Conversation panel");
        panel.read(cx).actions().clone()
    })
}

fn conversation(theme: ThemeFlags, number: u64, cx: &mut TestAppContext) -> ShellWindow {
    let mut shell = open_shell(
        cx,
        Options {
            theme,
            ..Options::default()
        },
    );
    shell.show(number, PanelKind::Conversation, OpenMode::Permanent);
    shell
}

#[gpui::test]
fn the_header_names_the_pull_request_without_a_ci_box(cx: &mut TestAppContext) {
    let mut shell = conversation(flat(), 1288, cx);
    assert!(
        shell.drawn("pr-meta"),
        "host, repo, number, branches and author"
    );
    assert!(shell.drawn("my-review"));
    assert!(shell.drawn("pr-actions"));
    assert!(
        !shell.drawn("ci-summary"),
        "the checks are in the pipelines card at the end"
    );
}

#[gpui::test]
fn the_diff_has_the_compact_action_bar(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    shell.open_pr(42);
    assert!(shell.drawn("pr-actions-compact"));
}

#[gpui::test]
fn the_diff_review_button_opens_a_popover_that_submits_a_review(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    shell.open_pr(42);
    let writing = shell.writing();
    shell.click("review-button");
    assert!(
        shell.drawn("action-popover"),
        "the button only opens the popover"
    );
    shell.type_text("Looks good");
    assert_eq!(
        shell.update(|_, cx| writing.read(cx).review_body().to_owned()),
        "Looks good",
        "the comment box takes the keys as the popover opens"
    );
    shell.click("verdict-approve");
    assert_eq!(
        shell.update(|_, cx| writing.read(cx).verdict()),
        Verdict::Approve
    );
    let epoch = shell.update(|_, cx| writing.read(cx).review_epoch());
    shell.click_element(SharedString::from("review-menu-submit"));
    let (error, after, body) = shell.update(|_, cx| {
        let writing = writing.read(cx);
        (
            writing.review_error().cloned(),
            writing.review_epoch(),
            writing.review_body().to_owned(),
        )
    });
    assert_eq!(error, None);
    assert!(after > epoch, "the review was submitted");
    assert_eq!(body, "", "the comment went with it");
    assert!(!shell.drawn("action-popover"), "the popover closes");
}

#[gpui::test]
fn a_review_is_submitted_with_its_verdict(cx: &mut TestAppContext) {
    let theme = flat();
    let mut shell = conversation(theme, 42, cx);
    let bar = actions(&mut shell);
    let writing = shell.writing();
    shell.click("review-button");
    assert_eq!(
        shell.update(|_, cx| bar.read(cx).menu()),
        Some(Menu::Review)
    );
    shell.click("verdict-approve");
    assert_eq!(
        shell.update(|_, cx| writing.read(cx).verdict()),
        Verdict::Approve
    );
    let epoch = shell.update(|_, cx| writing.read(cx).review_epoch());
    shell.click_element(SharedString::from("review-menu-submit"));
    let (verdict, error, after) = shell.update(|_, cx| {
        let writing = writing.read(cx);
        (
            writing.verdict(),
            writing.review_error().cloned(),
            writing.review_epoch(),
        )
    });
    assert_eq!(error, None);
    assert!(after > epoch, "the verdict alone was submitted");
    assert_eq!(verdict, Verdict::Comment, "the bar starts over");
}

#[gpui::test]
fn verdicts_on_your_own_pull_request_are_disabled_with_the_reason(cx: &mut TestAppContext) {
    // #41 is the fixture user's own, on Forgejo, which refuses both.
    let theme = flat();
    let mut shell = conversation(theme, 41, cx);
    let writing = shell.writing();
    shell.click("review-button");
    assert!(
        shell.drawn("verdict-approve"),
        "Approve is listed, not hidden"
    );
    shell.click("verdict-approve");
    shell.click("verdict-request-changes");
    assert_eq!(
        shell.update(|_, cx| writing.read(cx).verdict()),
        Verdict::Comment,
        "a disabled verdict cannot be picked"
    );
}

#[gpui::test]
fn a_merge_asks_first_then_patches_the_state(cx: &mut TestAppContext) {
    let theme = flat();
    let mut shell = conversation(theme, 42, cx);
    shell.click("merge-split-menu");
    shell.click("merge-method-merge");
    let session = shell.session();
    let method = shell.update(|_, cx| {
        session
            .read(cx)
            .open_pr()
            .and_then(|open| open.lifecycle.chosen_method())
    });
    assert_eq!(method, Some(MergeMethod::Merge));

    shell.click_element(SharedString::from("merge-split"));
    assert!(confirming(&mut shell), "a merge asks first");
    assert!(shell.drawn("action-confirm-card"));
    assert_eq!(state(&mut shell, 42), PrState::Open);
    shell.click_element(SharedString::from("action-cancel"));
    assert!(!confirming(&mut shell));

    shell.click_element(SharedString::from("merge-split"));
    shell.click_element(SharedString::from("action-confirm"));
    assert_eq!(state(&mut shell, 42), PrState::Merged);
    assert_eq!(
        row_state(&mut shell, 42),
        PrState::Merged,
        "the inbox row too"
    );
    assert!(shell.drawn("action-notice"));
}

#[gpui::test]
fn a_blocked_merge_says_why(cx: &mut TestAppContext) {
    // #41 is a draft with failing checks.
    let theme = flat();
    let mut shell = conversation(theme, 41, cx);
    assert!(shell.drawn("merge-blocked"));
    shell.click_element(SharedString::from("merge-split"));
    assert!(!confirming(&mut shell), "a disabled merge does nothing");
}

#[gpui::test]
fn the_more_menu_drafts_closes_and_reopens(cx: &mut TestAppContext) {
    let theme = flat();
    let mut shell = conversation(theme, 42, cx);
    shell.click("more-menu");
    shell.click("more-draft");
    assert_eq!(
        state(&mut shell, 42),
        PrState::Draft,
        "no confirmation for drafts"
    );
    shell.click("more-menu");
    shell.click("more-ready");
    assert_eq!(state(&mut shell, 42), PrState::Open);

    shell.click("more-menu");
    shell.click("more-close");
    assert!(confirming(&mut shell), "a close asks first");
    shell.click_element(SharedString::from("action-confirm"));
    assert_eq!(state(&mut shell, 42), PrState::Closed);
    assert_eq!(row_state(&mut shell, 42), PrState::Closed);

    shell.click("more-menu");
    shell.click("more-reopen");
    assert_eq!(state(&mut shell, 42), PrState::Open);
}

/// A GitHub fixture on a [`FakeForge`], its Conversation shown, once the
/// forge's merge settings are in.
fn live_merge(
    cx: &mut TestAppContext,
    scratch: &Scratch,
) -> (
    ShellWindow,
    Arc<FakeForge>,
    app::model::PrKey,
    forge::model::PullRequest,
) {
    let fixtures = Fixtures::load();
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
    let mut shell = open_shell(
        cx,
        Options {
            theme: flat(),
            ..Options::default()
        },
    );
    go_live(&mut shell, services, vec![pr.clone()], ForgeKind::GitHub);
    let session = shell.session();
    shell.update(|_, cx| {
        session.update(cx, |session, cx| {
            session.show(&key, PanelKind::Conversation, OpenMode::Permanent, cx);
        });
    });
    let ready = key.clone();
    shell.settle_until("the forge's merge settings", |cx| {
        session.read(cx).pr(&ready).is_some_and(|open| {
            matches!(open.lifecycle.mergeability, Load::Ready(_))
                && open.lifecycle.settings.ready().is_some()
        })
    });
    (shell, fake, key, pr)
}

/// The merge waiting for its confirmation, if any.
fn pending(shell: &mut ShellWindow, key: &app::model::PrKey) -> Option<Action> {
    let session = shell.session();
    let key = key.clone();
    shell.update(move |_, cx| session.read(cx).pr(&key)?.lifecycle.confirm.clone())
}

#[gpui::test]
fn a_confirmed_merge_reaches_the_forge_at_the_head_it_named(cx: &mut TestAppContext) {
    let scratch = Scratch::new("merge-live");
    let (mut shell, fake, key, pr) = live_merge(cx, &scratch);
    shell.click_element(SharedString::from("merge-split"));
    assert!(
        fake.retries().is_empty(),
        "nothing is sent before the confirm"
    );
    let asked = pending(&mut shell, &key).expect("a merge to confirm");
    let head = asked.head().expect("a merge records its head").clone();
    assert_eq!(head, pr.head.sha, "the head on screen");
    assert!(
        asked
            .question(&key, ForgeKind::GitHub)
            .contains(&format!("at {}", head.short())),
        "the confirmation names the commit"
    );
    shell.click_element(SharedString::from("action-confirm"));
    let session = shell.session();
    shell.settle_until("the merge at the forge", |cx| {
        session
            .read(cx)
            .pr(&key)
            .is_some_and(|open| open.pr.state == PrState::Merged)
    });
    let sent = fake.retries();
    assert_eq!(sent.len(), 1);
    assert!(
        sent[0].starts_with(&format!("merge {} Squash", key.number)),
        "the forge's default method: {sent:?}"
    );
    assert!(
        sent[0].ends_with(head.as_str()),
        "exactly the recorded head: {sent:?}"
    );
}

#[gpui::test]
fn a_head_that_moves_cancels_the_merge_confirmation(cx: &mut TestAppContext) {
    let scratch = Scratch::new("merge-moved");
    let (mut shell, fake, key, _) = live_merge(cx, &scratch);
    shell.click_element(SharedString::from("merge-split"));
    assert!(pending(&mut shell, &key).is_some(), "a merge to confirm");

    fake.move_head("feedfacefeedfacefeedfacefeedfacefeedface");
    let session = shell.session();
    let refresh = key.clone();
    shell.update(|_, cx| session.update(cx, |session, cx| session.refresh_pr(&refresh, cx)));
    let moved = key.clone();
    shell.settle_until("the new head", |cx| {
        session
            .read(cx)
            .pr(&moved)
            .and_then(|open| open.forge_pr())
            .is_some_and(|pr| pr.head.sha.as_str().starts_with("feedface"))
    });
    assert_eq!(pending(&mut shell, &key), None, "the confirmation is gone");
    assert!(shell.drawn("action-notice"), "and the header says why");
    assert!(
        !shell.drawn("action-confirm-card"),
        "nothing left to confirm"
    );

    // A merge asked at the old head is refused before it reaches the forge.
    let stale = Action::Merge {
        method: MergeMethod::Squash,
        delete_branch: false,
        head: forge::model::Sha::new("0000000000000000000000000000000000000000"),
    };
    let request = key.clone();
    shell.update(|_, cx| {
        session.update(cx, |session, cx| {
            session.request_action(&request, stale, cx)
        });
    });
    shell.settle();
    assert_eq!(pending(&mut shell, &key), None);
    assert!(
        fake.retries().is_empty(),
        "nothing was sent: {:?}",
        fake.retries()
    );
}
