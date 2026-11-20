//! Local review tracking on the inbox rows: the viewed progress of the last
//! open, the unread dot after something new, the reviewed mark (by hand or
//! once every file is viewed), unread rows first, and the marks surviving a
//! store reopen.

use app::appearance;
use app::inbox::marks::{Marks, ReviewedMark, RowMarks};
use app::inbox::row_menu::{self, RowAction};
use app::model::{Inbox, PrKey};
use gpui::TestAppContext;

use crate::harness::{Options, Scratch, ShellWindow, open_shell};

fn marks_of(shell: &mut ShellWindow, key: &PrKey) -> RowMarks {
    let session = shell.session();
    shell.update(|_, cx| {
        let row = session.read(cx).inbox.get(key).cloned().expect("listed");
        Marks::global(cx).read(cx).row(&row)
    })
}

fn run(shell: &mut ShellWindow, number: u64, action: RowAction) {
    let key = shell.key(number);
    shell.update(|window, cx| row_menu::run(action, &key, window, cx));
    shell.settle();
}

/// A new comment on `number`, as a refresh brings it.
fn comment_on(shell: &mut ShellWindow, number: u64) {
    let session = shell.session();
    shell.update(|_, cx| {
        session.update(cx, |session, cx| {
            let mut inbox = Inbox::clone(&session.inbox);
            if let Some(pr) = inbox.prs.iter_mut().find(|pr| pr.number() == number) {
                pr.comments = Some(pr.comments.unwrap_or(0) + 1);
            }
            session.set_inbox(inbox, cx);
        });
    });
    shell.settle();
}

/// Scrolls "Created by me" until `number`'s row is drawn.
fn reveal(shell: &mut ShellWindow, number: u64) {
    let sidebar = shell.sidebar();
    let row = format!("inbox-row-{number}");
    for _ in 0..12 {
        if shell.drawn(&row) {
            return;
        }
        shell.update(|_, cx| sidebar.update(cx, |sidebar, cx| sidebar.scroll_section(2, 46.0, cx)));
        shell.settle();
    }
    panic!("#{number} never came into view");
}

#[gpui::test]
fn viewing_every_file_records_progress_and_the_reviewed_mark(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    shell.resize(1440.0, 1400.0);
    let key = shell.key(41);
    assert_eq!(marks_of(&mut shell, &key).progress, None, "never opened");
    shell.open_pr(41);
    let progress = marks_of(&mut shell, &key)
        .progress
        .expect("the open diff's progress");
    assert!(progress.current && progress.total > progress.viewed && progress.viewed > 0);
    reveal(&mut shell, 41);
    assert!(shell.drawn("inbox-progress-41"), "drawn on the row");
    assert_eq!(marks_of(&mut shell, &key).reviewed, None);

    let session = shell.session();
    shell.update(|_, cx| {
        session.update(cx, |session, cx| {
            let files = session.pr(&key).expect("open").files.clone();
            session.toggle_viewed_all(&files, cx);
        });
    });
    shell.settle();
    let marks = marks_of(&mut shell, &key);
    let progress = marks.progress.expect("progress");
    assert_eq!(progress.viewed, progress.total);
    assert_eq!(
        marks.reviewed,
        Some(ReviewedMark::Current),
        "every file viewed marks it reviewed"
    );
    assert!(shell.drawn("inbox-reviewed-41"));
}

#[gpui::test]
fn a_new_comment_after_opening_shows_the_unread_dot(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    // Folded, so "Review requested" keeps room for its rows once a pull
    // request is open.
    shell.click("inbox-section-1");
    let key = shell.key(42);
    shell.click("inbox-title-42");
    assert!(!marks_of(&mut shell, &key).unread, "seen when opened");
    shell.click("inbox-title-1288");
    comment_on(&mut shell, 42);
    assert!(marks_of(&mut shell, &key).unread);
    assert!(shell.drawn("inbox-unread-42"), "the dot is drawn");

    shell.click("inbox-title-42");
    assert!(!marks_of(&mut shell, &key).unread, "opening it reads it");
    assert!(!shell.drawn("inbox-unread-42"));
}

#[gpui::test]
fn marked_unread_it_stays_unread_until_opened_again(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    let key = shell.key(42);
    shell.click("inbox-title-42");
    run(&mut shell, 42, RowAction::MarkUnread);
    shell.redraw();
    assert!(
        marks_of(&mut shell, &key).unread,
        "the focus alone does not read it again"
    );
    shell.click("inbox-title-42");
    assert!(!marks_of(&mut shell, &key).unread, "opening it does");
}

#[gpui::test]
fn the_row_menu_marks_by_hand_and_unread_rows_come_first(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    let key = shell.key(412);
    run(&mut shell, 412, RowAction::MarkReviewed(true));
    assert_eq!(
        marks_of(&mut shell, &key).reviewed,
        Some(ReviewedMark::Current)
    );
    assert!(shell.drawn("inbox-reviewed-412"));
    run(&mut shell, 412, RowAction::MarkReviewed(false));
    assert_eq!(marks_of(&mut shell, &key).reviewed, None);

    let first = shell.bounds_of("inbox-row-42").top();
    assert!(shell.bounds_of("inbox-row-412").top() > first);
    run(&mut shell, 412, RowAction::MarkUnread);
    assert!(marks_of(&mut shell, &key).unread);
    shell.update(|_, cx| appearance::update(cx, |s| s.inbox.unread_first = true));
    shell.settle();
    assert!(
        shell.bounds_of("inbox-row-412").top() <= first,
        "the unread row leads its section"
    );
    run(&mut shell, 412, RowAction::MarkRead);
    assert!(!marks_of(&mut shell, &key).unread);
}

#[gpui::test]
fn marks_survive_a_store_reopen(cx: &mut TestAppContext) {
    let scratch = Scratch::new("marks-store");
    let path = scratch.join("cache.sqlite");
    let mut shell = open_shell(
        cx,
        Options {
            store: Some(path.clone()),
            ..Options::default()
        },
    );
    let key = shell.key(42);
    shell.click("inbox-title-42");
    run(&mut shell, 42, RowAction::MarkReviewed(true));

    // The same file opened again, as the next start does.
    let reopened = store::Store::open(&path).expect("the store opens again");
    let mut stored = None;
    shell.settle_until("the marks in the file", |_| {
        stored = reopened
            .marks()
            .wait()
            .expect("the marks read")
            .into_iter()
            .find(|(at, _)| *at == key)
            .map(|(_, mark)| mark);
        stored
            .as_ref()
            .is_some_and(|mark| mark.seen.is_some() && mark.reviewed.is_some())
    });
    let mark = stored.expect("stored");
    let row = shell.update(|_, cx| {
        app::session::Session::global(cx)
            .read(cx)
            .inbox
            .get(&key)
            .cloned()
            .expect("listed")
    });
    assert_eq!(mark.reviewed.map(|r| r.head), Some(row.sha.to_string()));
}

#[gpui::test]
fn marks_from_an_earlier_run_show_on_start(cx: &mut TestAppContext) {
    let scratch = Scratch::new("marks-start");
    let path = scratch.join("cache.sqlite");
    let fixtures = app::fixtures::Fixtures::load();
    let row = fixtures
        .inbox
        .prs
        .iter()
        .find(|pr| pr.number() == 1288)
        .expect("#1288")
        .clone();
    {
        let store = store::Store::open(&path).expect("a fresh store");
        let mark = store::ReviewMark {
            reviewed: Some(store::Reviewed {
                head: row.sha.to_string(),
                at: row.updated_at,
            }),
            unread: true,
            ..store::ReviewMark::default()
        };
        store.put_mark(&row.key, mark).wait().expect("written");
    }
    let mut shell = open_shell(
        cx,
        Options {
            store: Some(path),
            ..Options::default()
        },
    );
    let key = row.key.clone();
    shell.settle_until("the stored marks", |cx| {
        Marks::global(cx).read(cx).get(&key).is_some()
    });
    assert!(shell.drawn("inbox-unread-1288"));
    assert!(shell.drawn("inbox-reviewed-1288"));
}
