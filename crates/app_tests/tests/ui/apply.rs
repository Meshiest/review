//! Applying suggestions with several pull requests open: every step acts on
//! the pull request whose bar asked, never on the focused one.

use app::model::PrKey;
use app::session::{ApplyState, Session};
use app::suggestions::{Offer, SuggestionKey};
use forge::model::{CommentId, ThreadId};
use gpui::{App, TestAppContext};

use crate::harness::{Options, ShellWindow, open_shell};

/// The first suggestion `key` can apply.
fn applicable(session: &Session, key: &PrKey) -> Option<SuggestionKey> {
    let open = session.pr(key)?;
    open.threads
        .iter()
        .flat_map(|thread| {
            thread.comments.iter().map(|comment| SuggestionKey {
                thread: thread.id.clone(),
                comment: comment.id.clone(),
            })
        })
        .find(|suggestion| matches!(session.suggestion(key, suggestion), Some(Offer::Apply(_))))
}

/// The apply state and whether a confirmation is up, for `key`.
fn apply_of(shell: &mut ShellWindow, key: &PrKey) -> (Option<ApplyState>, bool) {
    let session = shell.session();
    let key = key.clone();
    shell.update(move |_, cx: &mut App| {
        let open = session.read(cx).pr(&key).expect("open");
        (open.apply.clone(), open.confirm.is_some())
    })
}

#[gpui::test]
fn apply_acts_on_the_bars_pull_request_not_the_focused_one(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    shell.open_pr(42);
    shell.open_pr(1288);
    let (a, b) = (shell.key(42), shell.key(1288));
    assert_eq!(shell.focused(), Some(1288), "the second one has focus");

    let session = shell.session();
    let suggestion = shell
        .update(|_, cx| applicable(session.read(cx), &a))
        .expect("#42 has a suggestion to apply");
    let (request_key, request) = (a.clone(), suggestion.clone());
    shell.update(|_, cx| {
        session.update(cx, |session, cx| {
            session.request_apply(&request_key, &[request], cx);
        });
    });
    shell.settle();
    assert_eq!(
        apply_of(&mut shell, &a),
        (None, true),
        "#42 asks to confirm"
    );
    assert_eq!(
        apply_of(&mut shell, &b),
        (None, false),
        "#1288 is untouched"
    );

    let confirm_key = a.clone();
    shell.update(|_, cx| {
        session.update(cx, |session, cx| session.confirm_apply(&confirm_key, cx));
    });
    shell.settle();
    let (applied, still_confirming) = apply_of(&mut shell, &a);
    assert!(
        matches!(applied, Some(ApplyState::Failed(_))) && !still_confirming,
        "#42's confirmation was acted on (fixtures have no forge): {applied:?}"
    );
    assert_eq!(
        apply_of(&mut shell, &b),
        (None, false),
        "#1288 is untouched"
    );
    assert_eq!(shell.focused(), Some(1288), "focus did not move");
}

#[gpui::test]
fn a_suggestion_from_another_pull_request_is_refused(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    shell.open_pr(42);
    let a = shell.key(42);
    let session = shell.session();
    let stranger = SuggestionKey {
        thread: ThreadId::new("thread-on-another-pull-request"),
        comment: CommentId::new("c"),
    };
    let key = a.clone();
    shell.update(|_, cx| {
        session.update(cx, |session, cx| {
            session.request_apply(&key, &[stranger], cx)
        });
    });
    shell.settle();
    let (state, confirming) = apply_of(&mut shell, &a);
    assert!(matches!(state, Some(ApplyState::Failed(_))), "{state:?}");
    assert!(!confirming, "nothing to confirm");
}
