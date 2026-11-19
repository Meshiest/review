//! Files in the diff: a click in the tree shows that file alone and in full
//! (the "All changes" bar goes back), the right-click menu's actions, the
//! file header's controls, and collapsing files (`z`, `Shift+Z`, viewed
//! marks and the setting that ties them).

use app::appearance::{self, Appearance};
use app::diff_panel::{MenuAction, MenuEntry};
use app::fixtures::Fixtures;
use diff::{ContextMode, FileIx, Row, Side};
use diff_view::ContextTarget;
use forge::model::ForgeKind;
use gpui::{
    Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, Point, SharedString, TestAppContext,
    point, px,
};
use store::DraftAnchor;

use crate::harness::{Options, ShellWindow, open_shell};

/// #41 is on Forgejo, which has no file-level comments.
const FORGEJO_PR: u64 = 41;

fn shell_on(cx: &mut TestAppContext, number: u64) -> ShellWindow {
    let mut shell = open_shell(
        cx,
        Options {
            open_pr: Some(number),
            ..Options::default()
        },
    );
    shell.resize(1440.0, 1600.0);
    shell
}

/// A fixture pull request on GitHub, which takes comments on a whole file.
fn github_pr() -> u64 {
    Fixtures::load()
        .inbox
        .prs
        .iter()
        .find(|row| row.forge == ForgeKind::GitHub)
        .expect("a GitHub fixture pull request")
        .key
        .number
        .0
}

fn clipboard(shell: &mut ShellWindow) -> Option<String> {
    shell.cx.read_from_clipboard().and_then(|item| item.text())
}

fn shown_file(shell: &mut ShellWindow) -> Option<FileIx> {
    let view = shell.diff_view();
    shell.update(|_, cx| view.read(cx).shown_file())
}

fn context(shell: &mut ShellWindow, file: FileIx) -> ContextMode {
    let view = shell.diff_view();
    shell.update(|_, cx| view.read(cx).context(file))
}

fn collapsed(shell: &mut ShellWindow) -> Vec<bool> {
    let view = shell.diff_view();
    shell.update(|_, cx| view.read(cx).collapsed_files().to_vec())
}

fn path_of(shell: &mut ShellWindow, file: FileIx) -> String {
    let view = shell.diff_view();
    shell.update(|_, cx| view.read(cx).list().diff().file(file).path.clone())
}

fn selected_file(shell: &mut ShellWindow) -> Option<SharedString> {
    let session = shell.session();
    shell.update(|_, cx| session.read(cx).selected_file().cloned())
}

/// Clicks a line of the diff, which gives it keyboard focus.
fn focus_diff(shell: &mut ShellWindow) {
    let (position, _) = shell.drawn_lines()[1];
    shell.click_at(position);
}

/// The first drawn code line with a line on `side`: its window position
/// and the menu target a right-click on it names.
fn line_on(shell: &mut ShellWindow, side: Side) -> (Point<gpui::Pixels>, ContextTarget) {
    let view = shell.diff_view();
    let lines = shell.drawn_lines();
    shell.update(|_, cx| {
        let view = view.read(cx);
        lines
            .iter()
            .find_map(|&(position, (row, hit_side, line))| {
                let Row::Line { file, .. } = view.list().rows()[row] else {
                    return None;
                };
                (hit_side == side).then(|| {
                    let path = view.list().diff().file(file).path.clone();
                    (
                        position,
                        ContextTarget::Line {
                            file,
                            side,
                            line,
                            path,
                        },
                    )
                })
            })
            .expect("a drawn line on the side")
    })
}

fn run(shell: &mut ShellWindow, action: MenuAction, target: &ContextTarget) -> bool {
    let panel = shell.diff_panel();
    let done = shell
        .update(|_, cx| panel.update(cx, |panel, cx| panel.run_menu_action(action, target, cx)));
    shell.settle();
    done
}

fn entry_disabled(
    shell: &mut ShellWindow,
    target: &ContextTarget,
    wanted: MenuAction,
) -> Option<SharedString> {
    let panel = shell.diff_panel();
    shell.update(|_, cx| {
        panel
            .read(cx)
            .menu_entries(target, cx)
            .into_iter()
            .find_map(|entry| match entry {
                MenuEntry::Item {
                    action, disabled, ..
                } if action == wanted => Some(disabled),
                _ => None,
            })
            .expect("the menu has the entry")
    })
}

#[gpui::test]
fn a_tree_click_shows_the_file_alone_in_full(cx: &mut TestAppContext) {
    let mut shell = shell_on(cx, FORGEJO_PR);
    assert_eq!(shown_file(&mut shell), None, "every file at first");
    assert!(shell.drawn("tree-all-changes"), "the bar tops the tree");

    // A file row of the tree, right of its checkbox.
    let checkbox = (0..64)
        .find_map(|ix| shell.element((SharedString::from("file-viewed"), ix)))
        .expect("a file row in the tree");
    shell.click_at(checkbox.bounds().center() + point(px(80.0), px(0.0)));
    let path = selected_file(&mut shell).expect("the click picked a file");
    let file = shown_file(&mut shell).expect("the diff shows one file");
    assert_eq!(path_of(&mut shell, file), path.as_ref());
    assert_eq!(context(&mut shell, file), ContextMode::Full);
    let view = shell.diff_view();
    let (rows, counts) = shell.update(|_, cx| {
        let view = view.read(cx);
        let list = view.list();
        let lines = |side| {
            list.rows()
                .iter()
                .filter(|row| match (**row, side) {
                    (Row::Line { old, .. }, Side::Old) => old.is_some(),
                    (Row::Line { new, .. }, Side::New) => new.is_some(),
                    (Row::Pair { left, .. }, Side::Old) => left.is_some(),
                    (Row::Pair { right, .. }, Side::New) => right.is_some(),
                    _ => false,
                })
                .count()
        };
        let text = &list.diff().file(file);
        (
            (lines(Side::Old), lines(Side::New)),
            (
                text.old.line_count() as usize,
                text.new.line_count() as usize,
            ),
        )
    });
    assert_eq!(rows, counts, "every line of both sides, changes inline");
    assert!(
        shell.update(|_, cx| view.read(cx).list().file_rows().len()) == 1,
        "no other file"
    );

    // The header switches between the full file and its changes.
    shell.click("whole-file-changes");
    assert_eq!(context(&mut shell, file), ContextMode::Hunks);
    shell.click("whole-file-full");
    assert_eq!(context(&mut shell, file), ContextMode::Full);

    // "All changes" goes back to every file, the file as it was.
    shell.click("tree-all-changes");
    assert_eq!(shown_file(&mut shell), None);
    assert_eq!(selected_file(&mut shell), None);
    assert_eq!(context(&mut shell, file), ContextMode::Hunks);
    let files = shell.update(|_, cx| view.read(cx).list().diff().files.len());
    assert_eq!(
        shell.update(|_, cx| view.read(cx).list().file_rows().len()),
        files
    );
}

#[gpui::test]
fn the_line_menu_copies_links_and_lines_and_comments(cx: &mut TestAppContext) {
    let mut shell = shell_on(cx, FORGEJO_PR);
    let (position, target) = line_on(&mut shell, Side::New);
    let ContextTarget::Line {
        file, line, path, ..
    } = target.clone()
    else {
        unreachable!("line_on names a line")
    };

    // A right-click on the line opens the menu on it.
    shell.cx.simulate_event(MouseDownEvent {
        button: MouseButton::Right,
        position,
        modifiers: Modifiers::none(),
        click_count: 1,
        first_mouse: false,
    });
    shell.cx.simulate_event(MouseUpEvent {
        button: MouseButton::Right,
        position,
        modifiers: Modifiers::none(),
        click_count: 1,
    });
    shell.settle();
    let panel = shell.diff_panel();
    let opened = shell.update(|_, cx| panel.read(cx).menu().map(|menu| menu.target.clone()));
    assert_eq!(opened, Some(target.clone()));
    shell.click("diff-menu-copy-lines");
    let view = shell.diff_view();
    let text = shell.update(|_, cx| view.read(cx).line_text(file, Side::New, line));
    assert_eq!(clipboard(&mut shell), text, "the line's own text");
    assert!(
        shell.update(|_, cx| panel.read(cx).menu().is_none()),
        "a pick closes the menu"
    );

    // The link is pinned to the head commit.
    let sha = shell.update(|_, cx| {
        panel
            .read(cx)
            .links(cx)
            .and_then(|links| links.head)
            .expect("the diff's head")
    });
    assert!(run(&mut shell, MenuAction::CopyLink, &target));
    let url = format!(
        "https://forgejo.example.com/cora/review/src/commit/{sha}/{path}#L{}",
        line + 1
    );
    assert_eq!(clipboard(&mut shell), Some(url.clone()));
    assert!(run(&mut shell, MenuAction::OpenOnForge, &target));
    assert_eq!(shell.cx.opened_url(), Some(url));

    // An old line links to the base commit, or says it is not known.
    let (_, old) = line_on(&mut shell, Side::Old);
    let base = shell.update(|_, cx| panel.read(cx).links(cx).and_then(|links| links.base));
    match base {
        Some(base) => {
            assert!(run(&mut shell, MenuAction::CopyLink, &old));
            let link = clipboard(&mut shell).unwrap_or_default();
            assert!(link.contains(&format!("/src/commit/{base}/")), "{link}");
        }
        None => {
            assert!(!run(&mut shell, MenuAction::CopyLink, &old));
            assert!(entry_disabled(&mut shell, &old, MenuAction::CopyLink).is_some());
        }
    }

    // Comment on line opens a composer on it.
    assert!(run(&mut shell, MenuAction::Comment, &target));
    let writing = shell.writing();
    let open_on_line = shell.update(|_, cx| {
        writing.read(cx).slots().iter().any(|slot| {
            slot.open
                && matches!(&slot.anchor, DraftAnchor::Line { path: p, line: l, .. }
                    if *p == path && l.line == line + 1)
        })
    });
    assert!(open_on_line, "a composer on the line");

    // A selection is a range: its lines, joined.
    let range = ContextTarget::Range {
        file,
        side: Side::New,
        start: line,
        end: line + 1,
        path: path.clone(),
    };
    assert!(run(&mut shell, MenuAction::CopyLines, &range));
    let both = shell.update(|_, cx| {
        let view = view.read(cx);
        [line, line + 1]
            .map(|l| view.line_text(file, Side::New, l).unwrap_or_default())
            .join("\n")
    });
    assert_eq!(clipboard(&mut shell), Some(both));

    // Open file in full shows it alone, every line.
    assert!(run(&mut shell, MenuAction::OpenFull, &target));
    assert_eq!(shown_file(&mut shell), Some(file));
    assert_eq!(context(&mut shell, file), ContextMode::Full);
}

#[gpui::test]
fn file_header_controls_copy_expand_and_comment(cx: &mut TestAppContext) {
    let mut shell = shell_on(cx, github_pr());
    let file = FileIx(0);
    let path = path_of(&mut shell, file);

    shell.click("copy-name-0");
    assert_eq!(clipboard(&mut shell), Some(path.clone()));

    shell.click("all-lines-0");
    assert_eq!(
        context(&mut shell, file),
        ContextMode::Full,
        "every hunk unfolded"
    );
    shell.click("all-lines-0");
    assert_eq!(context(&mut shell, file), ContextMode::Hunks);

    shell.click("comment-file-0");
    let writing = shell.writing();
    let drafted = shell.update(|_, cx| {
        writing
            .read(cx)
            .slots()
            .iter()
            .any(|slot| slot.open && slot.anchor == DraftAnchor::File { path: path.clone() })
    });
    assert!(drafted, "a file-level draft opened");

    // The `...` button opens the file's menu.
    shell.click("file-menu-0");
    let panel = shell.diff_panel();
    let target = shell.update(|_, cx| panel.read(cx).menu().map(|menu| menu.target.clone()));
    assert_eq!(target, Some(ContextTarget::FileHeader { file, path }));
}

#[gpui::test]
fn forgejo_has_no_file_comments_and_says_why(cx: &mut TestAppContext) {
    let mut shell = shell_on(cx, FORGEJO_PR);
    let file = FileIx(0);
    let path = path_of(&mut shell, file);
    let header = ContextTarget::FileHeader {
        file,
        path: path.clone(),
    };
    let reason = entry_disabled(&mut shell, &header, MenuAction::CommentOnFile);
    assert!(
        reason.is_some_and(|reason| reason.contains("Forgejo")),
        "disabled, naming the forge"
    );
    assert!(!run(&mut shell, MenuAction::CommentOnFile, &header));
    shell.click("comment-file-0");
    let writing = shell.writing();
    let drafted = shell.update(|_, cx| {
        writing
            .read(cx)
            .slots()
            .iter()
            .any(|slot| matches!(slot.anchor, DraftAnchor::File { .. }))
    });
    assert!(!drafted, "nothing opened");
    // The file menu's own actions work on the header.
    assert!(run(&mut shell, MenuAction::CopyFileName, &header));
    assert_eq!(clipboard(&mut shell), Some(path));
    assert!(run(&mut shell, MenuAction::CollapseAll, &header));
    assert!(collapsed(&mut shell).iter().all(|&c| c));
    assert!(run(&mut shell, MenuAction::ExpandAll, &header));
    assert!(collapsed(&mut shell).iter().all(|&c| !c));
}

#[gpui::test]
fn z_and_shift_z_collapse_files(cx: &mut TestAppContext) {
    let mut shell = shell_on(cx, FORGEJO_PR);
    focus_diff(&mut shell);
    let view = shell.diff_view();
    let current = shell
        .update(|_, cx| view.read(cx).current_file())
        .expect("a file at the top");
    let ix = current.0 as usize;
    let start = collapsed(&mut shell)[ix];
    shell.keys("z");
    assert_eq!(collapsed(&mut shell)[ix], !start, "z folds or opens it");
    shell.keys("z");
    assert_eq!(collapsed(&mut shell)[ix], start, "and back");

    shell.keys("shift-z");
    assert!(collapsed(&mut shell).iter().all(|&c| c), "every file");
    shell.keys("shift-z");
    assert!(collapsed(&mut shell).iter().all(|&c| !c), "every file open");

    // Remembered for the pull request: a rebuilt diff folds the same files.
    shell.keys("z");
    let before = collapsed(&mut shell);
    assert!(before.iter().any(|&c| c));
    let session = shell.session();
    let key = shell.key(FORGEJO_PR);
    shell.update(|_, cx| session.update(cx, |session, cx| session.reload(&key, cx)));
    let panel = shell.diff_panel();
    shell.settle_until("the diff to be rebuilt", |cx| {
        panel.read(cx).view().is_some_and(|built| *built != view)
    });
    shell.redraw();
    assert_eq!(collapsed(&mut shell), before);
}

#[gpui::test]
fn viewed_collapses_unviewed_expands_unless_turned_off(cx: &mut TestAppContext) {
    let mut shell = shell_on(cx, FORGEJO_PR);
    focus_diff(&mut shell);
    let view = shell.diff_view();
    let current = shell
        .update(|_, cx| view.read(cx).current_file())
        .expect("a file at the top");
    let ix = current.0 as usize;
    let viewed = |shell: &mut ShellWindow| {
        let view = shell.diff_view();
        shell.update(|_, cx| view.read(cx).is_viewed(current))
    };
    let start = viewed(&mut shell);
    shell.keys("v");
    assert_eq!(viewed(&mut shell), !start);
    assert_eq!(
        collapsed(&mut shell)[ix],
        !start,
        "viewed folds, unviewed opens"
    );
    shell.keys("v");
    assert_eq!(viewed(&mut shell), start);
    assert_eq!(collapsed(&mut shell)[ix], start);

    // The setting is on by default and shown in the settings panel.
    assert!(shell.update(|_, cx| cx.global::<Appearance>().settings.diff.collapse_viewed));
    shell.update(|_, cx| appearance::update(cx, |s| s.diff.collapse_viewed = false));
    shell.settle();
    if start {
        shell.keys("v");
    }
    if collapsed(&mut shell)[ix] {
        shell.keys("z");
    }
    assert!(!viewed(&mut shell) && !collapsed(&mut shell)[ix]);
    shell.keys("v");
    assert!(viewed(&mut shell));
    assert!(!collapsed(&mut shell)[ix], "off: a viewed file stays open");
}

#[gpui::test]
fn the_settings_panel_offers_the_collapse_setting(cx: &mut TestAppContext) {
    let mut shell = shell_on(cx, FORGEJO_PR);
    shell.keys("ctrl-,");
    let element = shell
        .element("diff-collapse-viewed")
        .expect("the settings panel shows the switch");
    assert_eq!(element.checked(), Some(true));
    shell.click_element("diff-collapse-viewed");
    assert!(!shell.update(|_, cx| cx.global::<Appearance>().settings.diff.collapse_viewed));
}
