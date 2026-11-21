//! Find (`Ctrl+F`) in the diff and the conversation: the bar opens with the
//! cursor in it, typing counts the matches, `Enter` and `Shift+Enter` step
//! through them with wrapping and scroll each into view, and `Esc` closes the
//! bar, clears the highlights and gives the keys back. A comment box keeps
//! its own `Ctrl+F`.

use app::find::{Count, FindBar};
use app::panels::PanelKind;
use app::session::OpenMode;
use diff::{FileIx, Row, Side, SideLine};
use gpui::{Entity, TestAppContext};

use crate::composer::point_at_new_line;
use crate::conversation::{conversation, threads};
use crate::harness::{Options, ShellWindow, open_shell};

/// The find key: Cmd+F on macOS.
const FIND: &str = if cfg!(target_os = "macos") {
    "cmd-f"
} else {
    "ctrl-f"
};

/// What the synthetic diff's code says often.
const WORD: &str = "cursor";

fn count(shell: &mut ShellWindow, bar: &Entity<FindBar>) -> Count {
    shell.update(|_, cx| bar.read(cx).count())
}

fn label(shell: &mut ShellWindow, bar: &Entity<FindBar>) -> String {
    shell.update(|_, cx| bar.read(cx).label(cx))
}

/// `needle`'s matches in `hay`, ignoring case, counted with `str` alone.
fn occurrences(hay: &str, needle: &str) -> usize {
    hay.to_lowercase().matches(&needle.to_lowercase()).count()
}

/// The matches the diff should find: in each shown file's path and in the
/// lines its rows show when it is open, both halves of a split row.
fn expected_in_diff(shell: &mut ShellWindow, needle: &str) -> usize {
    let view = shell.diff_view();
    shell.update(|_, cx| {
        let list = view.read(cx).list();
        let diff = list.diff();
        let line = |file: FileIx, side: Side, line: u32| {
            diff.line_text(file, side, line)
                .map_or(0, |text| occurrences(&text, needle))
        };
        (0..diff.files.len() as u32)
            .map(FileIx)
            .filter(|&file| list.file_row(file).is_some())
            .map(|file| {
                let rows: usize = list
                    .open_rows(file)
                    .into_iter()
                    .map(|row| match row {
                        Row::Line { new: Some(at), .. } => line(file, Side::New, at),
                        Row::Line { old: Some(at), .. } => line(file, Side::Old, at),
                        Row::Pair { left, right, .. } => {
                            left.map_or(0, |SideLine { line: at, .. }| line(file, Side::Old, at))
                                + right.map_or(0, |SideLine { line: at, .. }| {
                                    line(file, Side::New, at)
                                })
                        }
                        _ => 0,
                    })
                    .sum();
                occurrences(&diff.file(file).path, needle) + rows
            })
            .sum()
    })
}

/// Whether the current match's row is wholly inside the view.
fn current_in_view(shell: &mut ShellWindow) -> bool {
    let view = shell.diff_view();
    shell.update(|_, cx| {
        let view = view.read(cx);
        let Some(current) = view.current_find() else {
            return false;
        };
        let row = view
            .find_row(&view.find_matches()[current])
            .expect("the current match is on a row");
        let shown = view.rows_in_view();
        shown.start < row && row + 1 < shown.end
    })
}

fn scroll_y(shell: &mut ShellWindow) -> f64 {
    let view = shell.diff_view();
    shell.update(|_, cx| view.read(cx).scroll_y())
}

#[gpui::test]
fn ctrl_f_finds_in_the_diff_and_steps_through_the_matches(cx: &mut TestAppContext) {
    let mut shell = open_shell(
        cx,
        Options {
            open_pr: Some(41),
            diff_rows: Some(4000),
            ..Options::default()
        },
    );
    let (position, _) = shell.drawn_lines()[1];
    shell.click_at(position);
    let diff = shell.diff_panel();
    shell.keys(FIND);
    assert!(shell.update(|_, cx| diff.read(cx).is_find_open()));
    assert!(shell.drawn("find-bar"), "the bar is drawn");
    let bar = shell
        .update(|_, cx| diff.read(cx).find_bar().cloned())
        .expect("a find bar");

    shell.type_text(WORD);
    assert_eq!(
        shell.update(|_, cx| bar.read(cx).text(cx).to_string()),
        WORD,
        "the keys reached the bar"
    );
    let expected = expected_in_diff(&mut shell, WORD);
    assert!(expected > 10, "the synthetic diff says {WORD:?} often");
    let found = count(&mut shell, &bar);
    assert_eq!(found.total, expected);
    assert!(!found.capped);
    let view = shell.diff_view();
    let first = found.current.expect("a current match");
    assert_eq!(
        label(&mut shell, &bar),
        format!("{} of {expected}", first + 1)
    );
    assert!(current_in_view(&mut shell), "the first match is in view");

    shell.keys("enter");
    assert_eq!(count(&mut shell, &bar).current, Some(first + 1));
    assert!(
        current_in_view(&mut shell),
        "Enter scrolled to the next one"
    );

    // Back from the first match wraps to the last, far down the diff.
    let at = scroll_y(&mut shell);
    let steps = first + 2;
    for _ in 0..steps {
        shell.keys("shift-enter");
    }
    assert_eq!(count(&mut shell, &bar).current, Some(expected - 1));
    assert!(scroll_y(&mut shell) > at, "the view scrolled down to it");
    assert!(current_in_view(&mut shell), "and shows it");
    shell.keys("enter");
    assert_eq!(count(&mut shell, &bar).current, Some(0), "and on wraps");
    shell.keys("f3");
    assert_eq!(count(&mut shell, &bar).current, Some(1), "F3 steps too");

    shell.update(|window, cx| bar.update(cx, |bar, cx| bar.focus(None, window, cx)));
    shell.type_text("zzqqx");
    assert_eq!(count(&mut shell, &bar).total, 0);
    assert_eq!(label(&mut shell, &bar), "No results");

    shell.keys("escape");
    assert!(!shell.update(|_, cx| diff.read(cx).is_find_open()));
    assert!(!shell.drawn("find-bar"), "the bar closed");
    assert!(
        shell.update(|_, cx| view.read(cx).find_matches().is_empty()),
        "and its highlights are gone"
    );
    let at = scroll_y(&mut shell);
    shell.keys("n");
    assert!(
        scroll_y(&mut shell) > at,
        "Esc gave the keys back to the diff"
    );
}

#[gpui::test]
fn ctrl_f_again_keeps_the_query_and_a_collapsed_file_opens_at_its_match(cx: &mut TestAppContext) {
    let mut shell = open_shell(
        cx,
        Options {
            open_pr: Some(41),
            ..Options::default()
        },
    );
    let view = shell.diff_view();
    let file = FileIx(0);
    assert!(
        shell.update(|_, cx| view.read(cx).is_file_collapsed(file)),
        "the fixture's first file is viewed and collapsed"
    );
    // A word from a line of that file.
    let word = shell.update(|_, cx| {
        let list = view.read(cx).list();
        list.open_rows(file)
            .into_iter()
            .find_map(|row| match row {
                Row::Line { new: Some(at), .. } => list.diff().line_text(file, Side::New, at),
                _ => None,
            })
            .and_then(|text| {
                text.split(|c: char| !c.is_alphanumeric() && c != '_')
                    .find(|word| word.len() > 4)
                    .map(str::to_owned)
            })
            .expect("a word on a line of the first file")
    });
    let (position, _) = shell.drawn_lines()[0];
    shell.click_at(position);
    shell.keys(FIND);
    shell.type_text(&word);
    let diff = shell.diff_panel();
    let bar = shell
        .update(|_, cx| diff.read(cx).find_bar().cloned())
        .expect("a find bar");
    assert!(count(&mut shell, &bar).total > 0);
    let in_file = shell.update(|_, cx| {
        view.read(cx)
            .find_matches()
            .iter()
            .position(|m| m.file == file && m.at != diff_view::FindAt::Path)
            .expect("a match on a line of the collapsed file")
    });
    let total = count(&mut shell, &bar).total;
    while count(&mut shell, &bar).current != Some(in_file) {
        shell.keys("enter");
        assert!(count(&mut shell, &bar).total == total);
    }
    assert!(
        !shell.update(|_, cx| view.read(cx).is_file_collapsed(file)),
        "the file opened to show the match"
    );
    assert!(current_in_view(&mut shell));

    // Ctrl+F with the bar open selects the query: typing replaces it.
    shell.keys(FIND);
    shell.type_text("x");
    assert_eq!(shell.update(|_, cx| bar.read(cx).text(cx).to_string()), "x");
}

#[gpui::test]
fn ctrl_f_in_a_comment_box_stays_there(cx: &mut TestAppContext) {
    let mut shell = open_shell(
        cx,
        Options {
            open_pr: Some(41),
            ..Options::default()
        },
    );
    point_at_new_line(&mut shell, 2);
    shell.keys("c");
    shell.type_text("Off");
    shell.keys(FIND);
    let diff = shell.diff_panel();
    assert!(
        !shell.update(|_, cx| diff.read(cx).is_find_open()),
        "the composer kept Ctrl+F"
    );
    shell.type_text(" by one");
    let writing = shell.writing();
    let typed = shell.update(|_, cx| {
        writing
            .read(cx)
            .slots()
            .iter()
            .find(|slot| slot.open)
            .map(|slot| slot.body.clone())
    });
    assert_eq!(typed.as_deref(), Some("Off by one"), "and the typing");
}

#[gpui::test]
fn ctrl_f_finds_in_the_conversation_and_folded_threads(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    shell.show(41, PanelKind::Conversation, OpenMode::Permanent);
    let panel = conversation(&mut shell);
    // A remark on threads and in the conversation; the description does
    // not say it.
    let needle = "next push";
    let threads = threads(&mut shell);
    let in_comments: usize = threads
        .iter()
        .flat_map(|thread| &thread.comments)
        .map(|comment| occurrences(&comment.body, needle))
        .sum();
    assert!(
        in_comments > 1,
        "the fixture says {needle:?} more than once"
    );

    let holding: Vec<String> = threads
        .iter()
        .filter(|thread| thread.anchor.is_some())
        .filter(|thread| {
            thread
                .comments
                .iter()
                .any(|c| occurrences(&c.body, needle) > 0)
        })
        .map(|thread| thread.id.to_string())
        .collect();
    let folded = |shell: &mut ShellWindow| {
        holding
            .iter()
            .filter(|id| !shell.drawn(&format!("inline-thread-{id}")))
            .count()
    };
    assert!(!holding.is_empty(), "threads say it");
    // Compact: every thread folded to one row.
    shell.click("thread-style-1");
    assert_eq!(folded(&mut shell), holding.len(), "compact folds them");

    shell.click("pr-description");
    shell.keys(FIND);
    assert!(shell.update(|_, cx| panel.read(cx).is_find_open()));
    let bar = shell
        .update(|_, cx| panel.read(cx).find_bar().cloned())
        .expect("a find bar");
    shell.type_text(needle);
    let found = count(&mut shell, &bar);
    assert_eq!(found.total, in_comments);
    assert_eq!(found.current, Some(0));
    assert_eq!(label(&mut shell, &bar), format!("1 of {in_comments}"));

    let top = shell.update(|_, cx| panel.read(cx).scrolled());
    shell.keys("shift-enter");
    assert_eq!(count(&mut shell, &bar).current, Some(in_comments - 1));
    shell.redraw();
    assert!(
        shell.update(|_, cx| panel.read(cx).scrolled()) > top,
        "the last match is scrolled to"
    );
    shell.keys("enter");
    assert_eq!(count(&mut shell, &bar).current, Some(0), "Enter wraps");
    shell.keys("enter");
    assert_eq!(count(&mut shell, &bar).current, Some(1));
    for _ in 1..in_comments {
        shell.keys("enter");
    }
    assert_eq!(
        folded(&mut shell),
        0,
        "each thread opened as its match came up"
    );

    shell.keys("escape");
    assert!(!shell.update(|_, cx| panel.read(cx).is_find_open()));
    assert!(!shell.drawn("find-bar"));
    assert_eq!(count(&mut shell, &bar), Count::default(), "cleared");
}
