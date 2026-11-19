//! The line composer through the keyboard: `c` opens it on the line under
//! the pointer, typed text lands in its draft, `Ctrl+Enter` queues it as a
//! pending comment card, and `Esc` closes it and keeps the draft.

use app::writing::Writing;
use diff::Side;
use forge::model::LineRef;
use gpui::{App, Entity, TestAppContext};
use store::DraftAnchor;

use crate::harness::{Options, ShellWindow, open_shell};

/// The anchor a composer opened on `line` of the new side has.
fn on_new_line(path: &str, line: u32) -> DraftAnchor {
    DraftAnchor::Line {
        path: path.to_owned(),
        line: LineRef {
            side: forge::model::Side::New,
            line,
        },
        start: None,
    }
}

/// Points at the `nth` drawn line of the new side, clicks it so the diff has
/// focus, and returns its anchor.
fn point_at_new_line(shell: &mut ShellWindow, nth: usize) -> DraftAnchor {
    let lines = shell.drawn_lines();
    let (position, (row, _, line)) = lines
        .iter()
        .copied()
        .filter(|(_, (_, side, _))| *side == Side::New)
        .nth(nth)
        .expect("enough new-side lines on screen");
    let view = shell.diff_view();
    let path = shell.update(|_, cx| {
        let list = view.read(cx).list();
        let file = app::gutter::file_of(list, row).expect("a code row is in a file");
        list.diff().file(file).path.to_string()
    });
    shell.click_at(position);
    // The view counts lines from 0, comments from 1.
    on_new_line(&path, line + 1)
}

fn open_slots(writing: &Entity<Writing>, cx: &App) -> Vec<DraftAnchor> {
    writing
        .read(cx)
        .slots()
        .iter()
        .filter(|slot| slot.open)
        .map(|slot| slot.anchor.clone())
        .collect()
}

/// A composer opened on a line of a viewed, so collapsed, file opens the
/// file and takes the focus, as the scripted `--compose` run needs.
#[gpui::test]
fn a_composer_in_a_collapsed_file_opens_it(cx: &mut TestAppContext) {
    let mut shell = open_shell(
        cx,
        Options {
            open_pr: Some(41),
            ..Options::default()
        },
    );
    let writing = shell.writing();
    let view = shell.diff_view();
    let (path, line) = shell.update(|_, cx| app::scenario::first_line(cx).expect("a diff line"));
    let file = shell.update(|_, cx| {
        let files = &view.read(cx).list().diff().files;
        diff::FileIx(files.iter().position(|f| f.path == path).unwrap() as u32)
    });
    assert!(
        shell.update(|_, cx| view.read(cx).is_file_collapsed(file)),
        "the fixture's first file is viewed and collapsed"
    );

    let anchor = DraftAnchor::Line {
        path,
        line,
        start: None,
    };
    shell.update(|_, cx| writing.update(cx, |w, cx| w.open(anchor.clone(), None, cx)));
    shell.settle();
    assert!(
        !shell.update(|_, cx| view.read(cx).is_file_collapsed(file)),
        "the file opened"
    );
    assert!(
        shell
            .update(|_, cx| writing.read(cx).slot(&anchor)?.composer.clone())
            .is_some(),
        "the composer was drawn"
    );
}

#[gpui::test]
fn keys_open_type_queue_and_keep_a_comment(cx: &mut TestAppContext) {
    let mut shell = open_shell(
        cx,
        Options {
            open_pr: Some(41),
            ..Options::default()
        },
    );
    let writing = shell.writing();
    let view = shell.diff_view();
    let blocks_before = shell.update(|_, cx| view.read(cx).list().diff().blocks.len());

    let anchor = point_at_new_line(&mut shell, 2);
    shell.keys("c");
    assert_eq!(
        shell.update(|_, cx| open_slots(&writing, cx)),
        std::slice::from_ref(&anchor),
        "c opened a composer on the line under the pointer"
    );

    shell.type_text("Off by one?");
    let body = shell.update(|_, cx| writing.read(cx).slot(&anchor).map(|s| s.body.clone()));
    assert_eq!(
        body.as_deref(),
        Some("Off by one?"),
        "the keys reached its text box"
    );

    shell.keys("ctrl-enter");
    let queued: Vec<(DraftAnchor, String)> = shell.update(|_, cx| {
        writing
            .read(cx)
            .queued()
            .iter()
            .map(|q| (q.anchor.clone(), q.body.clone()))
            .collect()
    });
    assert_eq!(queued, [(anchor.clone(), "Off by one?".to_owned())]);
    assert!(shell.update(|_, cx| open_slots(&writing, cx)).is_empty());
    let view = shell.diff_view();
    let blocks_after = shell.update(|_, cx| view.read(cx).list().diff().blocks.len());
    assert_eq!(
        blocks_after,
        blocks_before + 1,
        "the pending comment shows as a card in the diff"
    );

    let other = point_at_new_line(&mut shell, 5);
    assert_ne!(other, anchor);
    shell.keys("c");
    shell.type_text("Keep me");
    shell.keys("escape");
    let kept = shell.update(|_, cx| {
        let writing = writing.read(cx);
        writing
            .slot(&other)
            .map(|slot| (slot.open, slot.body.clone(), writing.draft_count()))
    });
    assert_eq!(
        kept,
        Some((false, "Keep me".to_owned(), 1)),
        "Esc closed the composer and kept its draft"
    );

    point_at_new_line(&mut shell, 5);
    shell.keys("c");
    let reopened = shell.update(|_, cx| {
        let slot = writing.read(cx).slot(&other)?;
        Some((slot.open, slot.composer.as_ref()?.read(cx).text(cx)))
    });
    assert_eq!(
        reopened,
        Some((true, "Keep me".to_owned())),
        "c on the same line brings the draft back"
    );
}
