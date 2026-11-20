//! The diff header's "N drafts" chip opens a menu of the drafts not yet in
//! the review; picking one shows it where it is written (the diff, or the
//! conversation's comment box) and Back returns. Also: the composers draw the
//! app's shared controls, as tall as the header's.

use app::panels::PanelKind;
use app::session::OpenMode;
use app::writing::Writing;
use forge::model::LineRef;
use gpui::{App, Bounds, Entity, Pixels, SharedString, TestAppContext};
use store::DraftAnchor;

use crate::composer::point_at_new_line;
use crate::conversation::{conversation, open, reveal, threads};
use crate::harness::{Options, ShellWindow, open_shell};

/// Writes `text` in a new composer on the `nth` drawn new-side line and
/// closes it with Esc, so it is kept as a draft.
fn line_draft(shell: &mut ShellWindow, nth: usize, text: &str) -> DraftAnchor {
    let anchor = point_at_new_line(shell, nth);
    shell.keys("c");
    shell.type_text(text);
    shell.keys("escape");
    anchor
}

fn is_open(writing: &Entity<Writing>, anchor: &DraftAnchor, cx: &App) -> bool {
    writing.read(cx).is_open(anchor)
}

fn diff_scrolled(shell: &mut ShellWindow) -> f64 {
    let view = shell.diff_view();
    shell.update(|_, cx| view.read(cx).scroll_y())
}

fn menu_open(shell: &mut ShellWindow) -> bool {
    shell.menu_items().is_some()
}

fn inside(inner: Bounds<Pixels>, outer: Bounds<Pixels>) -> bool {
    inner.top() >= outer.top() && inner.bottom() <= outer.bottom()
}

#[gpui::test]
fn the_drafts_chip_lists_each_draft_and_jumps_to_one_in_the_diff(cx: &mut TestAppContext) {
    let mut shell = open_shell(
        cx,
        Options {
            open_pr: Some(41),
            ..Options::default()
        },
    );
    let writing = shell.writing();
    let first = line_draft(&mut shell, 2, "Off by one?\nThe loop starts at 1.");
    line_draft(&mut shell, 5, "Rename this");
    assert_eq!(shell.update(|_, cx| writing.read(cx).draft_count()), 2);

    let body = shell.bounds_of("diff-body");
    shell.scroll_at(body.center(), 4000.0);
    let away = diff_scrolled(&mut shell);
    assert!(away > 0.0, "scrolled away from the drafts");

    shell.click("drafts-chip");
    assert!(menu_open(&mut shell), "the chip opens the drafts menu");
    assert!(shell.drawn("draft-entry-0") && shell.drawn("draft-entry-1"));
    assert!(!shell.drawn("draft-entry-2"), "one entry per draft");

    shell.click("draft-entry-0");
    assert!(!menu_open(&mut shell), "picking one closes the menu");
    assert!(
        shell.update(|_, cx| is_open(&writing, &first, cx)),
        "the draft's composer is open again"
    );
    let at = diff_scrolled(&mut shell);
    assert!(
        at < away,
        "the diff scrolled back up to it: {at} from {away}"
    );
    let composer = shell.bounds_of("composer-input");
    let body = shell.bounds_of("diff-body");
    assert!(inside(composer, body), "its composer is in view");
    let focused = shell.update(|window, cx| {
        let slot = writing.read(cx).slot(&first)?;
        let composer = slot.composer.as_ref()?.read(cx);
        Some(composer.input_focus(cx).is_focused(window))
    });
    assert_eq!(focused, Some(true), "the cursor is in it");

    shell.keys("alt-left");
    assert_eq!(diff_scrolled(&mut shell), away, "Back returns where it was");
}

#[gpui::test]
fn enter_or_space_on_the_focused_chip_opens_the_menu(cx: &mut TestAppContext) {
    let mut shell = open_shell(
        cx,
        Options {
            open_pr: Some(41),
            ..Options::default()
        },
    );
    let writing = shell.writing();
    let draft = line_draft(&mut shell, 3, "Keep me");
    let panel = shell.diff_panel();
    let focus_chip = |shell: &mut ShellWindow| {
        shell.update(|window, cx| {
            let focus = panel.read(cx).drafts_focus().clone();
            window.focus(&focus, cx);
        });
        shell.settle();
    };

    focus_chip(&mut shell);
    shell.keys("enter");
    assert!(menu_open(&mut shell), "Enter opens the menu");
    shell.keys("escape");
    assert!(!menu_open(&mut shell), "Esc closes it");

    focus_chip(&mut shell);
    shell.keys("space");
    assert!(menu_open(&mut shell), "Space opens the menu");
    // Past "Show all drafts", "Add all" and the separator to the draft.
    shell.keys("down down down enter");
    assert!(
        shell.update(|_, cx| is_open(&writing, &draft, cx)),
        "the keys pick the draft"
    );
}

#[gpui::test]
fn show_all_drafts_reopens_every_draft(cx: &mut TestAppContext) {
    let mut shell = open_shell(
        cx,
        Options {
            open_pr: Some(41),
            ..Options::default()
        },
    );
    let writing = shell.writing();
    let drafts = [
        line_draft(&mut shell, 2, "Off by one?"),
        line_draft(&mut shell, 5, "Rename this"),
    ];
    let open = |shell: &mut ShellWindow| {
        shell.update(|_, cx| drafts.each_ref().map(|draft| is_open(&writing, draft, cx)))
    };
    assert_eq!(open(&mut shell), [false, false], "Esc kept them closed");

    shell.click("drafts-chip");
    assert!(shell.drawn("drafts-show-all") && shell.drawn("draft-entry-1"));
    let all = shell.bounds_of("drafts-show-all");
    let first = shell.bounds_of("draft-entry-0");
    assert!(all.bottom() < first.top(), "it is the menu's first entry");
    shell.click("drafts-show-all");
    assert!(!menu_open(&mut shell), "picking it closes the menu");
    assert_eq!(open(&mut shell), [true, true], "every draft is open again");
}

#[gpui::test]
fn add_all_queues_every_line_draft(cx: &mut TestAppContext) {
    let mut shell = open_shell(
        cx,
        Options {
            open_pr: Some(41),
            ..Options::default()
        },
    );
    let writing = shell.writing();
    line_draft(&mut shell, 2, "Off by one?");
    line_draft(&mut shell, 5, "Rename this");
    let queued = |shell: &mut ShellWindow| shell.update(|_, cx| writing.read(cx).queued().len());
    let before = queued(&mut shell);

    shell.click("drafts-chip");
    let add = shell.bounds_of("drafts-add-all");
    assert!(
        shell.bounds_of("drafts-show-all").bottom() <= add.top()
            && add.bottom() < shell.bounds_of("draft-entry-0").top(),
        "after Show all, before the drafts"
    );
    shell.click("drafts-add-all");
    assert!(!menu_open(&mut shell), "picking it closes the menu");
    assert_eq!(
        queued(&mut shell),
        before + 2,
        "both drafts are in the review"
    );
    assert!(
        shell.update(|_, cx| writing.read(cx).queueable().is_empty()),
        "nothing left to add"
    );
}

#[gpui::test]
fn a_draft_on_the_pull_request_jumps_to_the_comment_box(cx: &mut TestAppContext) {
    let mut shell = open(41, cx);
    reveal(&mut shell, "comment-box");
    shell.click("composer-input");
    shell.type_text("Ship it");
    let panel = conversation(&mut shell);
    shell.update(|_, cx| panel.update(cx, |panel, cx| panel.scroll_to(0.0, cx)));
    shell.show(41, PanelKind::Diff, OpenMode::Permanent);
    let diff = shell.panel_of(41, PanelKind::Diff).expect("the Diff tab");
    let talk = shell
        .panel_of(41, PanelKind::Conversation)
        .expect("the Conversation tab");
    assert!(shell.is_shown(diff));

    shell.click("drafts-chip");
    assert!(shell.drawn("draft-entry-0") && !shell.drawn("draft-entry-1"));
    shell.click("draft-entry-0");
    assert!(shell.is_shown(talk), "the jump shows the conversation");
    let scrolled = shell.update(|_, cx| panel.read(cx).scrolled());
    assert!(scrolled > 0.0, "scrolled down to the comment box");
    let comment = shell.bounds_of("comment-box");
    let body = shell.bounds_of("conversation-body");
    assert!(comment.top() < body.bottom(), "the comment box is in view");

    shell.keys("alt-left");
    assert!(shell.is_shown(diff), "Back returns to the diff");
}

/// The heights the composer's controls must share with the header's.
struct Heights {
    button: Pixels,
    toggle: Pixels,
}

fn reference_heights(shell: &mut ShellWindow) -> Heights {
    Heights {
        button: shell.bounds_of("review-button").size.height,
        toggle: shell.bounds_of("layout-unified").size.height,
    }
}

fn height_of(shell: &mut ShellWindow, id: &'static str) -> Pixels {
    shell
        .element(SharedString::from(id))
        .unwrap_or_else(|| panic!("no {id} drawn"))
        .bounds()
        .size
        .height
}

/// Checks the open composer's controls against `want`; `queue` is whether
/// it is a new comment on code, whose send button is the split button.
fn check_composer(shell: &mut ShellWindow, want: &Heights, queue: bool, what: &str) {
    assert_eq!(
        shell.bounds_of("composer-write").size.height,
        want.toggle,
        "{what}: Write / Preview is the diff's toggle"
    );
    assert_eq!(
        height_of(shell, "composer-discard"),
        want.button,
        "{what}: Discard is a toolbar button"
    );
    if queue {
        assert_eq!(
            shell.bounds_of("composer-queue-unit").size.height,
            want.button,
            "{what}: Add to review is the header's split button"
        );
        shell.click("composer-queue-menu");
        assert!(menu_open(shell), "{what}: the chevron opens its menu");
        shell.keys("escape");
    } else {
        assert_eq!(
            height_of(shell, "composer-send-now"),
            want.button,
            "{what}: the send button is a toolbar button"
        );
        assert!(!shell.drawn("composer-queue-unit"));
    }
}

fn open_composer(shell: &mut ShellWindow, writing: &Entity<Writing>, anchor: &DraftAnchor) {
    let anchor = anchor.clone();
    shell.update(|_, cx| writing.update(cx, |w, cx| w.open(anchor, None, cx)));
    shell.settle();
}

fn close_composer(shell: &mut ShellWindow, writing: &Entity<Writing>, anchor: &DraftAnchor) {
    shell.update(|_, cx| writing.update(cx, |w, cx| w.close(anchor, cx)));
    shell.settle();
}

#[gpui::test]
fn every_composer_draws_the_shared_controls(cx: &mut TestAppContext) {
    let mut shell = open_shell(
        cx,
        Options {
            open_pr: Some(41),
            ..Options::default()
        },
    );
    let writing = shell.writing();
    let want = reference_heights(&mut shell);
    assert_eq!(f32::from(want.button), app::ui::CONTROL_HEIGHT);
    assert_eq!(app::ui::TOGGLE_HEIGHT, app::ui::CONTROL_HEIGHT);
    // A toggle's choices sit inside its 1px border, so the group is as tall
    // as the toolbar buttons beside it.
    assert_eq!(want.toggle + gpui::px(2.0), want.button);

    let (path, line) = shell.update(|_, cx| app::scenario::first_line(cx).expect("a diff line"));
    let comment = DraftAnchor::Line {
        path: path.clone(),
        line,
        start: None,
    };
    open_composer(&mut shell, &writing, &comment);
    check_composer(&mut shell, &want, true, "a line comment");
    close_composer(&mut shell, &writing, &comment);

    let range = DraftAnchor::Line {
        path,
        line: LineRef {
            line: line.line + 1,
            ..line
        },
        start: Some(line),
    };
    open_composer(&mut shell, &writing, &range);
    check_composer(&mut shell, &want, true, "a range comment");
    shell.type_text("Queue me");
    shell.keys("ctrl-enter");
    let queued = shell.update(|_, cx| writing.read(cx).queued().first().map(|q| q.id));
    let queued = queued.expect("the range comment was queued");

    let thread = threads(&mut shell)
        .iter()
        .find(|t| !t.outdated && !t.is_resolved() && t.anchor.is_some())
        .map(|t| t.id.clone())
        .expect("a current inline thread");
    let reply = DraftAnchor::Reply { thread };
    open_composer(&mut shell, &writing, &reply);
    check_composer(&mut shell, &want, false, "a reply");
    close_composer(&mut shell, &writing, &reply);

    shell.update(|_, cx| writing.update(cx, |w, cx| w.edit_queued(queued, cx)));
    shell.settle();
    let edit = DraftAnchor::Edit { queued };
    assert!(shell.update(|_, cx| is_open(&writing, &edit, cx)));
    check_composer(&mut shell, &want, false, "an edit");
    close_composer(&mut shell, &writing, &edit);

    shell.show(41, PanelKind::Conversation, OpenMode::Permanent);
    let want = Heights {
        button: shell.bounds_of("review-button").size.height,
        toggle: want.toggle,
    };
    reveal(&mut shell, "comment-box");
    check_composer(&mut shell, &want, false, "a conversation comment");
}
