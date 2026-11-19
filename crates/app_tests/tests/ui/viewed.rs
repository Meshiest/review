//! The Viewed checkbox in a diff file header marks the file viewed the way
//! the file tree does: the session, the store and the tree follow it, and
//! `v` in the diff toggles the file at the top back.

use diff::FileIx;
use gpui::{TestAppContext, point, px};

use crate::harness::{Options, Scratch, ShellWindow, open_shell};

/// Whether the session holds `path` of #41 viewed.
fn session_viewed(shell: &mut ShellWindow, path: &str) -> bool {
    let session = shell.session();
    shell.update(|_, cx| {
        let session = session.read(cx);
        let open = session.open_pr().expect("#41 is open");
        open.files
            .iter()
            .find(|file| file.path.as_ref() == path)
            .is_some_and(|file| open.viewed.contains(file))
    })
}

/// How many files the tree's checkboxes show viewed.
fn tree_checked(shell: &mut ShellWindow) -> usize {
    (0..64)
        .filter_map(|ix| shell.element((gpui::SharedString::from("file-viewed"), ix)))
        .filter(|element| element.checked() == Some(true))
        .count()
}

#[gpui::test]
fn the_header_checkbox_marks_the_file_viewed(cx: &mut TestAppContext) {
    let scratch = Scratch::new("viewed-header");
    let db = scratch.join("cache.db");
    let mut shell = open_shell(
        cx,
        Options {
            open_pr: Some(41),
            store: Some(db.clone()),
            ..Options::default()
        },
    );
    shell.resize(1440.0, 2400.0);
    let view = shell.diff_view();
    let path = shell.update(|_, cx| view.read(cx).list().diff().files[0].path.clone());
    let key = shell.key(41);
    let before = session_viewed(&mut shell, &path);
    let checked = tree_checked(&mut shell);
    assert_eq!(
        shell.update(|_, cx| view.read(cx).is_viewed(FileIx(0))),
        before,
        "the header shows the session's mark from the start"
    );

    // The first file's header is the top row of the diff; its checkbox is
    // at the header's right end, left of the header's trailing controls.
    let body = shell.bounds_of("diff-body");
    let trailing = app::diff_panel::HEADER_SLOTS.trailing;
    shell.click_at(point(
        body.right() - px(48.0) - trailing,
        body.top() + px(10.0),
    ));
    assert_eq!(
        session_viewed(&mut shell, &path),
        !before,
        "the session flipped"
    );
    assert_eq!(
        shell.update(|_, cx| view.read(cx).is_viewed(FileIx(0))),
        !before,
        "and the header shows it"
    );
    let now = tree_checked(&mut shell);
    assert_eq!(
        now as isize - checked as isize,
        if before { -1 } else { 1 },
        "the tree's checkboxes follow"
    );
    let session = shell.session();
    let blob = shell.update(|_, cx| {
        session
            .read(cx)
            .open_pr()
            .and_then(|open| open.files.iter().find(|file| file.path.as_ref() == path))
            .map(|file| file.blob.to_string())
            .expect("the file")
    });
    shell.settle_until("the store to hold the mark", |_| {
        store::Store::open(&db)
            .ok()
            .and_then(|store| store.viewed_for(&key).wait().ok())
            .is_some_and(|set| set.is_viewed(&path, &blob) == !before)
    });

    // `v` in the diff toggles the file at the top back.
    shell.click_at(point(body.center().x, body.top() + px(120.0)));
    shell.keys("v");
    assert_eq!(
        session_viewed(&mut shell, &path),
        before,
        "v toggled it back"
    );
    assert_eq!(
        shell.update(|_, cx| view.read(cx).is_viewed(FileIx(0))),
        before
    );
    assert_eq!(tree_checked(&mut shell), checked);
}
