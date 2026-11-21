//! Clicks on the stack section's rows: from another pull request's diff a
//! click shows that member's diff, to step through a stack; otherwise a row
//! acts as an inbox row, the conversation first and then the diff.

use app::panels::PanelKind;
use app::session::OpenMode;
use gpui::{Bounds, Pixels, TestAppContext};

use crate::harness::{Options, ShellWindow, open_shell};

fn shown(shell: &mut ShellWindow, number: u64, kind: PanelKind) -> bool {
    shell
        .panel_of(number, kind)
        .is_some_and(|id| shell.is_shown(id))
}

/// Clicks #`number`'s stack row, first scrolling the short section so
/// that row is its top one. `top` is where the section's first row is
/// drawn when it is not scrolled.
fn click_row(shell: &mut ShellWindow, top: Bounds<Pixels>, number: u64) {
    let selector = String::leak(format!("stack-row-{number}"));
    let first = shell.bounds_of("stack-row-41");
    let row = shell.bounds_of(selector);
    shell.scroll_at(top.center(), -1000.0);
    shell.scroll_at(top.center(), f32::from(row.top() - first.top()));
    shell.click(selector);
}

#[gpui::test]
fn from_a_diff_the_stack_steps_through_diffs(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    shell.show(40, PanelKind::Diff, OpenMode::Permanent);
    let top = shell.bounds_of("stack-row-41");
    assert!(shown(&mut shell, 40, PanelKind::Diff));

    click_row(&mut shell, top, 41);
    assert!(shown(&mut shell, 41, PanelKind::Diff), "#41's diff");
    click_row(&mut shell, top, 40);
    assert!(shown(&mut shell, 40, PanelKind::Diff), "and back to #40's");
}

#[gpui::test]
fn from_a_conversation_a_stack_row_acts_as_an_inbox_row(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    shell.show(41, PanelKind::Conversation, OpenMode::Permanent);
    let top = shell.bounds_of("stack-row-41");

    click_row(&mut shell, top, 39);
    assert!(
        shown(&mut shell, 39, PanelKind::Conversation),
        "the conversation first"
    );
    assert!(!shown(&mut shell, 39, PanelKind::Diff));
    click_row(&mut shell, top, 39);
    assert!(shown(&mut shell, 39, PanelKind::Diff), "then the diff");
    click_row(&mut shell, top, 39);
    assert!(shown(&mut shell, 39, PanelKind::Conversation), "and back");
}
