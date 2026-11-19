//! Back and forward: a jump from the Conversation to a thread in the Diff
//! is undone by `Alt+Left` or the mouse's back button and redone by
//! `Alt+Right` or its forward button, each tab where it was left.

use app::panels::PanelKind;
use forge::model::Thread;
use gpui::{Modifiers, MouseButton, NavigationDirection};
use gpui_kit::component::dock::PanelId;

use crate::conversation::{code_ready, conversation, open, reveal, threads};
use crate::harness::ShellWindow;

/// The `n`th current, unresolved inline thread of the open pull request:
/// one drawn unfolded.
fn inline_thread(shell: &mut ShellWindow, n: usize) -> Thread {
    threads(shell)
        .iter()
        .filter(|t| !t.outdated && !t.is_resolved() && t.anchor.is_some())
        .nth(n)
        .cloned()
        .expect("enough inline threads")
}

/// Clicks the "Jump to line" of `thread`, scrolled into view first.
fn jump(shell: &mut ShellWindow, thread: &Thread) -> PanelId {
    let selector = format!("jump-{}", thread.id);
    reveal(shell, &selector);
    shell.click_named(&selector);
    let diff = shell
        .panel_of(41, PanelKind::Diff)
        .expect("the Diff opened");
    assert!(shell.is_shown(diff), "the jump shows the diff");
    diff
}

fn scrolled(shell: &mut ShellWindow) -> f32 {
    let panel = conversation(shell);
    shell.update(|_, cx| panel.read(cx).scrolled())
}

fn diff_scrolled(shell: &mut ShellWindow) -> f64 {
    let view = shell.diff_view();
    shell.update(|_, cx| view.read(cx).scroll_y())
}

fn press_navigate(shell: &mut ShellWindow, direction: NavigationDirection) {
    let at = shell.bounds_of("diff-body").center();
    let button = MouseButton::Navigate(direction);
    shell.cx.simulate_mouse_move(at, None, Modifiers::none());
    shell.cx.simulate_mouse_down(at, button, Modifiers::none());
    shell.cx.simulate_mouse_up(at, button, Modifiers::none());
    shell.settle();
}

#[gpui::test]
fn back_returns_to_the_conversation_where_it_was_and_forward_to_the_diff(
    cx: &mut gpui::TestAppContext,
) {
    let mut shell = open(41, cx);
    code_ready(&mut shell);
    let conversation = shell
        .panel_of(41, PanelKind::Conversation)
        .expect("the Conversation tab");
    let thread = inline_thread(&mut shell, 2);
    assert!(!shell.read_shell(|s, _| s.can_go_back()), "nowhere yet");

    let diff = jump(&mut shell, &thread);
    let left_at = scrolled(&mut shell);
    assert!(left_at > 0.0, "the jump was from down the conversation");
    let diff_at = diff_scrolled(&mut shell);
    assert!(diff_at > 0.0, "the diff is at the thread, at {diff_at}");

    shell.keys("alt-left");
    assert!(shell.is_shown(conversation), "Back shows the conversation");
    assert!(!shell.is_shown(diff));
    assert_eq!(scrolled(&mut shell), left_at, "where it was scrolled");
    assert!(shell.read_shell(|s, _| s.can_go_forward()));

    shell.keys("alt-right");
    assert!(shell.is_shown(diff), "Forward shows the diff again");
    assert_eq!(diff_scrolled(&mut shell), diff_at, "at the thread");
    assert!(!shell.read_shell(|s, _| s.can_go_forward()));
}

#[gpui::test]
fn the_mouse_back_and_forward_buttons_go_back_and_forward(cx: &mut gpui::TestAppContext) {
    let mut shell = open(41, cx);
    code_ready(&mut shell);
    let conversation = shell
        .panel_of(41, PanelKind::Conversation)
        .expect("the Conversation tab");
    let thread = inline_thread(&mut shell, 1);
    let diff = jump(&mut shell, &thread);

    press_navigate(&mut shell, NavigationDirection::Back);
    assert!(shell.is_shown(conversation), "the back button goes back");

    let at = shell.bounds_of("conversation-body").center();
    let button = MouseButton::Navigate(NavigationDirection::Forward);
    shell.cx.simulate_mouse_move(at, None, Modifiers::none());
    shell.cx.simulate_mouse_down(at, button, Modifiers::none());
    shell.cx.simulate_mouse_up(at, button, Modifiers::none());
    shell.settle();
    assert!(shell.is_shown(diff), "the forward button goes forward");
}

#[gpui::test]
fn a_new_jump_after_going_back_drops_the_way_forward(cx: &mut gpui::TestAppContext) {
    let mut shell = open(41, cx);
    code_ready(&mut shell);
    let conversation = shell
        .panel_of(41, PanelKind::Conversation)
        .expect("the Conversation tab");
    let first = inline_thread(&mut shell, 1);
    let second = inline_thread(&mut shell, 2);
    jump(&mut shell, &first);
    shell.keys("alt-left");
    assert!(shell.read_shell(|s, _| s.can_go_forward()));

    let diff = jump(&mut shell, &second);
    assert!(
        !shell.read_shell(|s, _| s.can_go_forward()),
        "the jump drops the way forward"
    );
    shell.keys("alt-right");
    assert!(shell.is_shown(diff), "Forward has nowhere to go");
    shell.keys("alt-left");
    assert!(
        shell.is_shown(conversation),
        "Back returns from the new jump"
    );
}

#[gpui::test]
fn back_undoes_a_file_picked_in_the_files_list(cx: &mut gpui::TestAppContext) {
    let mut shell = open(41, cx);
    shell.show(41, PanelKind::Diff, app::session::OpenMode::Permanent);
    let body = shell.bounds_of("diff-body");
    shell.scroll_at(body.center(), 300.0);
    let before = diff_scrolled(&mut shell);
    assert!(before > 0.0, "scrolled down the diff");
    let view = shell.diff_view();
    let path = shell.update(|_, cx| {
        let files = &view.read(cx).list().diff().files;
        gpui::SharedString::from(files.last().expect("files").path.clone())
    });
    let session = shell.session();
    let selected =
        |shell: &mut ShellWindow| shell.update(|_, cx| session.read(cx).selected_file().cloned());
    shell.update(|_, cx| session.update(cx, |s, cx| s.select_file(path.clone(), cx)));
    shell.settle();
    assert_eq!(selected(&mut shell), Some(path.clone()));
    assert!(
        shell.read_shell(|s, _| s.can_go_back()),
        "the pick is recorded"
    );

    shell.keys("alt-left");
    assert_eq!(selected(&mut shell), None, "every file again");
    assert_eq!(diff_scrolled(&mut shell), before, "where the diff was");

    shell.keys("alt-right");
    assert_eq!(selected(&mut shell), Some(path), "the file again");
}

#[gpui::test]
fn back_opens_the_preview_another_pull_request_replaced(cx: &mut gpui::TestAppContext) {
    use app::session::OpenMode;

    let mut shell = crate::harness::open_shell(cx, crate::harness::Options::default());
    shell.show(41, PanelKind::Conversation, OpenMode::Preview);
    shell.show(42, PanelKind::Conversation, OpenMode::Preview);
    assert!(
        shell.panel_of(41, PanelKind::Conversation).is_none(),
        "the second preview replaced the first"
    );

    shell.keys("alt-left");
    let first = shell
        .panel_of(41, PanelKind::Conversation)
        .expect("Back opens the first again");
    assert!(shell.is_shown(first));
    assert_eq!(shell.focused(), Some(41));

    shell.keys("alt-right");
    let second = shell
        .panel_of(42, PanelKind::Conversation)
        .expect("Forward opens the second again");
    assert!(shell.is_shown(second));
}
