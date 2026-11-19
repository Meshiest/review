//! The command palette (`Ctrl+Shift+P`) and quick open (`Ctrl+P`): typing
//! filters by fuzzy name, Up and Down move, Enter runs, a disabled command
//! says why and does not run, commands run last come first, and Esc or a
//! click outside closes.

use app::palette::{Mode, Palette};
use app::panels::PanelKind;
use app::session::OpenMode;
use app::sink::Verdict;
use diff::Layout;
use gpui::{Entity, TestAppContext, point, px};

use crate::harness::{Options, ShellWindow, open_shell};

fn palette(shell: &mut ShellWindow) -> Option<Entity<Palette>> {
    shell.read_shell(|shell, _| shell.palette().cloned())
}

/// The rows shown, top to bottom: each title and, for a disabled one, why.
fn rows(shell: &mut ShellWindow) -> Vec<(String, Option<String>)> {
    let palette = palette(shell).expect("the palette is open");
    shell.update(|_, cx| {
        palette
            .read(cx)
            .rows()
            .into_iter()
            .map(|(title, why)| (title.to_string(), why.map(|why| why.to_string())))
            .collect()
    })
}

fn titles(shell: &mut ShellWindow) -> Vec<String> {
    rows(shell).into_iter().map(|(title, _)| title).collect()
}

fn layout(shell: &mut ShellWindow) -> Layout {
    let session = shell.session();
    shell.update(|_, cx| session.read(cx).diff_layout())
}

#[gpui::test]
fn ctrl_shift_p_finds_a_command_by_fuzzy_name_and_enter_runs_it(cx: &mut TestAppContext) {
    let mut shell = open_shell(
        cx,
        Options {
            open_pr: Some(42),
            ..Options::default()
        },
    );
    assert_eq!(layout(&mut shell), Layout::Unified);
    shell.keys("ctrl-shift-p");
    let open = palette(&mut shell).expect("Ctrl+Shift+P opens the palette");
    assert_eq!(shell.update(|_, cx| open.read(cx).mode()), Mode::Commands);
    assert!(shell.drawn("palette"));
    let every = titles(&mut shell);
    assert!(
        every
            .iter()
            .any(|title| title == "Show or hide the sidebar")
    );
    assert!(
        every.iter().any(|title| title == "Approve"),
        "the open pull request's commands are listed"
    );

    shell.type_text("splt dif");
    let found = titles(&mut shell);
    assert_eq!(
        found.first().map(String::as_str),
        Some("Unified or split diff")
    );
    assert!(found.len() < every.len(), "the query filters: {found:?}");

    shell.keys("down");
    assert_eq!(
        shell.update(|_, cx| open.read(cx).selected()),
        1.min(found.len() - 1)
    );
    shell.keys("up");
    assert_eq!(shell.update(|_, cx| open.read(cx).selected()), 0);

    shell.keys("enter");
    assert!(palette(&mut shell).is_none(), "running a command closes it");
    assert_eq!(layout(&mut shell), Layout::Split, "the command ran");
}

#[gpui::test]
fn a_disabled_command_says_why_and_does_not_run(cx: &mut TestAppContext) {
    // #41 is the fixture user's own, on Forgejo, which refuses approvals.
    let mut shell = open_shell(cx, Options::default());
    shell.show(41, PanelKind::Conversation, OpenMode::Permanent);
    let writing = shell.writing();
    let epoch = shell.update(|_, cx| writing.read(cx).review_epoch());

    shell.keys("ctrl-shift-p");
    shell.type_text("approve");
    let (title, why) = rows(&mut shell).into_iter().next().expect("a match");
    assert_eq!(title, "Approve");
    let why = why.expect("Approve is disabled");
    assert!(why.contains("own pull request"), "the reason: {why}");

    shell.keys("enter");
    assert!(palette(&mut shell).is_some(), "Enter on it does nothing");
    let (after, verdict) = shell.update(|_, cx| {
        let writing = writing.read(cx);
        (writing.review_epoch(), writing.verdict())
    });
    assert_eq!(after, epoch, "no review was submitted");
    assert_eq!(verdict, Verdict::Comment);

    shell.keys("escape");
    assert!(palette(&mut shell).is_none(), "Esc closes it");
}

#[gpui::test]
fn commands_run_last_come_first(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    let sidebar = |shell: &mut ShellWindow| shell.read_shell(|shell, cx| shell.is_sidebar_open(cx));
    assert!(sidebar(&mut shell));

    shell.keys("ctrl-shift-p");
    shell.type_text("sidebar");
    shell.keys("enter");
    assert!(!sidebar(&mut shell), "the command ran");
    shell.keys("ctrl-shift-p");
    assert_eq!(titles(&mut shell)[0], "Show or hide the sidebar");

    shell.type_text("split");
    shell.keys("enter");
    shell.keys("ctrl-shift-p");
    assert_eq!(
        titles(&mut shell)[..2],
        ["Unified or split diff", "Show or hide the sidebar"],
        "newest first"
    );

    // A click outside the box closes it and runs nothing.
    shell.click_at(point(px(30.0), px(860.0)));
    assert!(palette(&mut shell).is_none());
    assert_eq!(layout(&mut shell), Layout::Split);
}

#[gpui::test]
fn ctrl_p_goes_to_a_pull_request_and_to_a_file(cx: &mut TestAppContext) {
    let mut shell = open_shell(
        cx,
        Options {
            open_pr: Some(42),
            ..Options::default()
        },
    );
    shell.keys("ctrl-p");
    let open = palette(&mut shell).expect("Ctrl+P opens quick open");
    assert_eq!(shell.update(|_, cx| open.read(cx).mode()), Mode::QuickOpen);
    shell.type_text("1297");
    let first = titles(&mut shell).into_iter().next().expect("a match");
    assert!(first.contains("#1297"), "the pull request first: {first}");
    shell.keys("enter");
    assert!(palette(&mut shell).is_none());
    assert_eq!(shell.focused(), Some(1297), "its diff opened");

    let session = shell.session();
    shell.settle_until("the files of #1297", |cx| {
        session
            .read(cx)
            .open_pr()
            .is_some_and(|open| !open.files.is_empty())
    });
    let path = shell.update(|_, cx| {
        let open = session.read(cx).open_pr().expect("#1297 is open");
        open.files.last().expect("a file").path.to_string()
    });
    shell.keys("ctrl-p");
    shell.type_text(&path);
    assert_eq!(titles(&mut shell).first(), Some(&path));
    shell.keys("enter");
    let selected = shell.update(|_, cx| {
        session
            .read(cx)
            .open_pr()
            .and_then(|open| open.selected_file.clone())
    });
    assert_eq!(
        selected.as_deref(),
        Some(path.as_str()),
        "the file is selected"
    );
    assert!(
        shell
            .shown_tabs()
            .iter()
            .any(|tab| tab == app::panels::DIFF),
        "in the diff"
    );
}
