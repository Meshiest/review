//! The expand arrows on hunk headers: a click on one reveals 20 context
//! lines above its hunk, and once the gap is small one arrow reveals the
//! rest of it and joins the hunk to the one before.

use diff::{Expand, FileIx, Row};
use gpui::{Pixels, Point, TestAppContext};

use crate::harness::{Options, ShellWindow, open_shell};

/// Scrolls the header of gap `gap` of `file` into view and returns its
/// arrows as drawn: what each reveals and the window position of its middle.
fn arrows_of(shell: &mut ShellWindow, file: FileIx, gap: u32) -> Vec<(Expand, Point<Pixels>)> {
    let view = shell.diff_view();
    shell.update(|_, cx| {
        view.update(cx, |view, _| {
            let row = view
                .list()
                .rows()
                .iter()
                .position(|row| row.gap(view.list().diff()) == Some((file, gap)))
                .expect("the gap has a row");
            view.scroll_to_row(row, 200.0);
        });
    });
    let body = shell.bounds_of("diff-body");
    shell.update(|_, cx| {
        view.read(cx)
            .expand_arrows()
            .into_iter()
            .filter(|(arrow, _)| arrow.file == file && arrow.gap == gap)
            .map(|(arrow, bounds)| (arrow.expand, body.origin + bounds.center()))
            .collect()
    })
}

fn rows(shell: &mut ShellWindow) -> Vec<Row> {
    let view = shell.diff_view();
    shell.update(|_, cx| view.read(cx).list().rows().to_vec())
}

#[gpui::test]
fn arrows_reveal_context_and_join_hunks(cx: &mut TestAppContext) {
    let mut shell = open_shell(
        cx,
        Options {
            open_pr: Some(41),
            ..Options::default()
        },
    );
    shell.resize(1440.0, 1600.0);
    // A gap between two hunks with more than 20 lines hidden: an arrow up
    // and one down on the later hunk's header.
    let view = shell.diff_view();
    let first = shell.update(|_, cx| {
        let list = view.read(cx).list();
        (0..list.diff().files.len() as u32)
            .map(FileIx)
            .flat_map(|file| {
                (1..list.diff().file(file).hunks.len() as u32).map(move |gap| (file, gap))
            })
            .find(|&(file, gap)| list.arrows(file, gap) == [Expand::Down, Expand::Up])
    });
    let (file, gap) = first.expect("a fixture diff with a large gap between hunks");
    let at = shell.update(|_, cx| {
        view.read(cx)
            .list()
            .diff()
            .file(file)
            .gap(gap)
            .expect("the gap")
    });
    let header = Row::HunkHeader { file, hunk: gap };
    let before = rows(&mut shell);

    let arrows = arrows_of(&mut shell, file, gap);
    assert_eq!(
        arrows.iter().map(|(expand, _)| *expand).collect::<Vec<_>>(),
        [Expand::Down, Expand::Up]
    );
    shell.click_at(arrows[1].1);
    let after = rows(&mut shell);
    assert_eq!(after.len(), before.len() + 20, "20 more context rows");
    // The 20 lines just above the hunk, numbered on both sides, under its
    // header.
    let row = after.iter().position(|row| *row == header).expect("header");
    let skip = at.len - 20;
    for k in 0..20 {
        assert_eq!(
            after[row + 1 + k as usize],
            Row::Line {
                file,
                old: Some(at.old + skip + k),
                new: Some(at.new + skip + k),
            }
        );
    }

    // Reveal upward until one arrow is left for the rest of the gap, then
    // take it: the header goes and the hunks are joined.
    loop {
        let arrows = arrows_of(&mut shell, file, gap);
        let [(expand, position)] = arrows[..] else {
            let (_, up) = arrows
                .iter()
                .find(|(expand, _)| *expand == Expand::Up)
                .expect("an arrow up");
            shell.click_at(*up);
            continue;
        };
        assert_eq!(expand, Expand::All);
        shell.click_at(position);
        break;
    }
    let joined = rows(&mut shell);
    assert!(!joined.contains(&header), "the hunks are joined");
    assert_eq!(joined.len(), before.len() + at.len as usize - 1);
    let left = shell.update(|_, cx| view.read(cx).list().arrows(file, gap).len());
    assert_eq!(left, 0);
    let kept = shell.update(|_, cx| {
        let view = view.read(cx);
        let path = &view.list().diff().file(file).path;
        view.reveals().contains_key(path)
    });
    assert!(kept, "the view hands its reveals on by path");
}
