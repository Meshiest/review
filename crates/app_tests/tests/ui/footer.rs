//! The end of the Conversation: the merge box (the pipelines card, whether
//! it can merge, Merge with its method) and the comment box (Close pull
//! request, Comment) scroll with the timeline. Comment posts through the
//! composer; Merge and Close confirm in the merge box.

use app::appearance::ThemeFlags;
use app::panels::PanelKind;
use app::session::OpenMode;
use forge::model::{MergeMethod, PrState};
use gpui::{SharedString, TestAppContext};

use crate::harness::{Options, ShellWindow, flat, open_shell};

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

/// Scrolls the conversation until the element with `selector` is inside it.
fn reveal(shell: &mut ShellWindow, selector: &'static str) {
    for _ in 0..80 {
        let body = shell.bounds_of("conversation-body");
        let element = shell.bounds_of(selector);
        if element.top() >= body.top() && element.bottom() <= body.bottom() {
            return;
        }
        let dy = if element.top() < body.top() {
            -200.0
        } else {
            200.0
        };
        shell.scroll_at(body.center(), dy);
    }
    panic!("{selector} never came into view");
}

fn state(shell: &mut ShellWindow) -> PrState {
    let session = shell.session();
    shell.update(|_, cx| session.read(cx).open_pr().expect("open").pr.state)
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

#[gpui::test]
fn comment_posts_through_the_composer(cx: &mut TestAppContext) {
    let theme = flat();
    let mut shell = conversation(theme, 42, cx);
    let body = shell.bounds_of("conversation-body");
    let merge = shell.bounds_of("merge-box");
    let comment = shell.bounds_of("comment-box");
    assert!(
        merge.top() > body.top() && comment.top() > merge.bottom(),
        "the merge box, then the comment box, after the timeline"
    );
    reveal(&mut shell, "comment-box");
    assert!(
        shell
            .element(SharedString::from("composer-close-pr"))
            .is_some(),
        "Close pull request beside Comment"
    );
    shell.click("composer-input");
    shell.type_text("Ship it");
    shell.click_element(SharedString::from("composer-send-now"));
    let writing = shell.writing();
    shell.settle_until("the comment to be posted", |cx| {
        writing
            .read(cx)
            .sent()
            .iter()
            .any(|comment| comment.body.contains("Ship it"))
    });
}

#[gpui::test]
fn merge_in_the_merge_box_picks_its_method_and_asks_first(cx: &mut TestAppContext) {
    let theme = flat();
    let mut shell = conversation(theme, 42, cx);
    reveal(&mut shell, "merge-box");
    assert!(shell.drawn("merge-box-checks-summary"));
    assert!(shell.drawn("merge-box-status"));

    // The pipelines row lists the jobs; a second click folds them again.
    reveal(&mut shell, "merge-box-checks");
    shell.click("merge-box-checks");
    assert!(shell.drawn("merge-box-jobs"));
    reveal(&mut shell, "merge-box-checks");
    shell.click("merge-box-checks");
    assert!(!shell.drawn("merge-box-jobs"));

    let session = shell.session();
    let allowed = shell.update(|_, cx| {
        let open = session.read(cx).open_pr().expect("open");
        open.lifecycle
            .settings
            .ready()
            .map_or(0, |settings| settings.allowed.len())
    });
    if allowed > 1 {
        reveal(&mut shell, "merge-box");
        shell.click("merge-box-method");
        reveal(&mut shell, "merge-box-methods");
        shell.click("merge-box-method-merge");
        let method = shell.update(|_, cx| {
            session
                .read(cx)
                .open_pr()
                .and_then(|open| open.lifecycle.chosen_method())
        });
        assert_eq!(method, Some(MergeMethod::Merge));
        assert!(
            !shell.drawn("merge-box-methods"),
            "the choice closes the list"
        );
    }

    reveal(&mut shell, "merge-box");
    shell.click_element(SharedString::from("merge-box-merge"));
    assert!(confirming(&mut shell), "a merge asks first");
    assert_eq!(state(&mut shell), PrState::Open);
    assert!(shell.drawn("merge-box-confirm-card"));
    assert!(
        !shell.drawn("action-confirm-card"),
        "one confirmation, where it was asked"
    );
    reveal(&mut shell, "merge-box-confirm-card");
    shell.click_element(SharedString::from("merge-box-confirm"));
    assert_eq!(state(&mut shell), PrState::Merged);
    assert!(shell.drawn("merge-box-notice"));
}

#[gpui::test]
fn close_pull_request_beside_comment_asks_first(cx: &mut TestAppContext) {
    let mut shell = conversation(flat(), 42, cx);
    reveal(&mut shell, "comment-box");
    shell.click_element(SharedString::from("composer-close-pr"));
    assert!(confirming(&mut shell), "a close asks first");
    reveal(&mut shell, "merge-box-confirm-card");
    shell.click_element(SharedString::from("merge-box-cancel"));
    assert!(!confirming(&mut shell));
    assert_eq!(state(&mut shell), PrState::Open);

    reveal(&mut shell, "comment-box");
    shell.click_element(SharedString::from("composer-close-pr"));
    reveal(&mut shell, "merge-box-confirm-card");
    shell.click_element(SharedString::from("merge-box-confirm"));
    assert_eq!(state(&mut shell), PrState::Closed);
    reveal(&mut shell, "comment-box");
    assert!(
        shell
            .element(SharedString::from("composer-reopen-pr"))
            .is_some(),
        "a closed pull request offers Reopen"
    );
}
