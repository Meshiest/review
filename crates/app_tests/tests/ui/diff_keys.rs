//! Keyboard navigation in the diff: `n`/`p` step between hunks, `]f`
//! between files and `]t` between threads, each scrolling the view to its
//! row; `x` folds the current thread; `?` opens the key list and Esc gives
//! the keys back to the diff.

use gpui::TestAppContext;

use crate::harness::{Options, ShellWindow, open_shell};

fn scroll_y(shell: &mut ShellWindow) -> f64 {
    let view = shell.diff_view();
    shell.update(|_, cx| view.read(cx).scroll_y())
}

/// The tops of the rows of this kind, as the view lays them out.
fn tops(shell: &mut ShellWindow, files: bool) -> Vec<f64> {
    let view = shell.diff_view();
    shell.update(|_, cx| {
        let list = view.read(cx).list();
        let rows = if files {
            list.file_rows()
        } else {
            list.hunk_rows()
        };
        rows.iter().map(|&row| list.top_of(row as usize)).collect()
    })
}

/// Clicks a line of the diff, which gives it keyboard focus.
fn focus_diff(shell: &mut ShellWindow) {
    let (position, _) = shell.drawn_lines()[1];
    shell.click_at(position);
}

#[gpui::test]
fn keys_move_through_hunks_files_and_threads(cx: &mut TestAppContext) {
    let mut shell = open_shell(
        cx,
        Options {
            open_pr: Some(41),
            diff_rows: Some(4000),
            ..Options::default()
        },
    );
    focus_diff(&mut shell);
    let start = scroll_y(&mut shell);
    let hunks = tops(&mut shell, false);
    let files = tops(&mut shell, true);
    assert!(hunks.len() >= 3 && files.len() >= 2, "a long diff");

    shell.keys("n");
    let first = scroll_y(&mut shell);
    assert!(first > start, "n scrolled down");
    assert!(hunks.contains(&first), "to the top of a hunk: {first}");
    shell.keys("n");
    let second = scroll_y(&mut shell);
    assert!(second > first && hunks.contains(&second));
    shell.keys("p");
    assert_eq!(scroll_y(&mut shell), first, "p went back one hunk");

    shell.keys("] f");
    let file = scroll_y(&mut shell);
    assert!(files.contains(&file), "]f lands on a file header: {file}");
    assert!(file > first);
    shell.keys("[ f");
    assert!(scroll_y(&mut shell) < file, "[f went back a file");

    let diff = shell.diff_panel();
    let cards = shell.update(|_, cx| diff.read(cx).cards().clone());
    assert_eq!(shell.update(|_, cx| cards.read(cx).current()), None);
    shell.keys("] t");
    let thread = shell
        .update(|_, cx| cards.read(cx).current())
        .expect("]t picked a thread");
    let view = shell.diff_view();
    let shown = shell.update(|_, cx| {
        let cards = cards.read(cx);
        let view = view.read(cx);
        let block = cards.block_of(thread).expect("the thread has a card");
        view.rows_in_view().contains(&view.list().block_row(block))
    });
    assert!(shown, "]t scrolled its card into view");

    let folded =
        |shell: &mut ShellWindow| shell.update(|_, cx| cards.read(cx).is_collapsed(thread, cx));
    let before = folded(&mut shell);
    shell.keys("x");
    assert_ne!(folded(&mut shell), before, "x folded or unfolded it");
    shell.keys("x");
    assert_eq!(folded(&mut shell), before);

    shell.keys("?");
    assert!(shell.read_shell(|shell, _| shell.is_key_help_open()));
    let at = scroll_y(&mut shell);
    shell.keys("n");
    assert_eq!(scroll_y(&mut shell), at, "the key list holds the keys");
    shell.keys("escape");
    assert!(!shell.read_shell(|shell, _| shell.is_key_help_open()));
    shell.keys("n");
    assert!(
        scroll_y(&mut shell) > at,
        "Esc gave the keys back to the diff"
    );
}
