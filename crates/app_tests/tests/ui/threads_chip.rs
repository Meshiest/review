//! The diff header's "N open threads" chip opens a menu of the open threads;
//! picking one scrolls the diff to its card, as `]t` does, and Back returns.

use gpui::TestAppContext;

use crate::harness::{Options, ShellWindow, open_shell};

fn diff_scrolled(shell: &mut ShellWindow) -> f64 {
    let view = shell.diff_view();
    shell.update(|_, cx| view.read(cx).scroll_y())
}

#[gpui::test]
fn the_threads_chip_lists_each_open_thread_and_jumps_to_one(cx: &mut TestAppContext) {
    let mut shell = open_shell(
        cx,
        Options {
            open_pr: Some(41),
            ..Options::default()
        },
    );
    let panel = shell.diff_panel();
    let cards = shell.update(|_, cx| panel.read(cx).cards().clone());
    // The open threads, in the menu's order, by their index in the cards.
    let open: Vec<usize> = shell.update(|_, cx| {
        let cards = cards.read(cx);
        cards
            .threads()
            .iter()
            .enumerate()
            .filter(|(_, t)| t.anchor.is_some() && !t.is_resolved())
            .map(|(ix, _)| ix)
            .collect()
    });
    assert!(open.len() >= 2, "the fixture has open threads");
    let thread = open[0];
    assert!(
        shell.update(|_, cx| cards.read(cx).block_of(thread).is_some()),
        "the first one has a card"
    );

    let body = shell.bounds_of("diff-body");
    shell.scroll_at(body.center(), 100_000.0);
    let away = diff_scrolled(&mut shell);
    assert!(away > 0.0, "scrolled to the end");

    shell.click("threads-chip");
    assert!(
        shell.menu_items().is_some(),
        "the chip opens the threads menu"
    );
    let last = open.len() - 1;
    assert!(shell.drawn("thread-entry-0") && shell.drawn(&format!("thread-entry-{last}")));
    assert!(
        !shell.drawn(&format!("thread-entry-{}", open.len())),
        "one entry per open thread"
    );

    shell.click("thread-entry-0");
    assert!(shell.menu_items().is_none(), "picking one closes the menu");
    let at = diff_scrolled(&mut shell);
    assert!(at < away, "the diff scrolled back up: {at} from {away}");
    let view = shell.diff_view();
    let (current, shown) = shell.update(|_, cx| {
        let cards = cards.read(cx);
        let view = view.read(cx);
        let block = cards.block_of(thread).expect("the thread has a card");
        (
            cards.current(),
            view.rows_in_view().contains(&view.list().block_row(block)),
        )
    });
    assert_eq!(current, Some(thread), "the thread is the current one");
    assert!(shown, "its card is in view");

    shell.keys("alt-left");
    assert_eq!(diff_scrolled(&mut shell), away, "Back returns where it was");
}
