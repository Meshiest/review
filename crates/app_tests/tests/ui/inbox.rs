//! The inbox panel: sections the user resizes and folds, each scrolling on
//! its own, saved with the layout; grouping by repository with a menu per
//! repository; the Repos panel that hides repositories from view only, with
//! a search box of its own; and the counts on each row.

use std::time::Duration;

use app::appearance::Appearance;
use app::inbox::{SavedInbox, repo_id};
use app::layout;
use app::sidebar_panels::Section;
use gpui::{SharedString, TestAppContext, point, px};
use gpui_kit::component::dock::PanelInfo;

use crate::harness::{Options, Scratch, ShellWindow, StartLayout, open_shell};

fn block(shell: &mut ShellWindow, ix: usize) -> gpui::Bounds<gpui::Pixels> {
    shell.bounds_of(String::leak(format!("inbox-section-block-{ix}")))
}

fn hidden(shell: &mut ShellWindow) -> Vec<String> {
    shell.update(|_, cx| {
        cx.global::<Appearance>()
            .settings
            .inbox
            .hidden_repos
            .clone()
    })
}

fn repos_shown(shell: &mut ShellWindow) -> (usize, usize) {
    let sidebar = shell.sidebar();
    shell.update(|_, cx| sidebar.read(cx).repos_shown(cx))
}

/// The panel named `name` anywhere under `state`.
fn find_panel<'a>(
    state: &'a gpui_kit::component::dock::PanelState,
    name: &str,
) -> Option<&'a gpui_kit::component::dock::PanelState> {
    if state.panel_name == name {
        return Some(state);
    }
    state
        .children
        .iter()
        .find_map(|child| find_panel(child, name))
}

/// The inbox's part of a saved layout, from its sidebar.
fn saved_inbox(state: &gpui_kit::component::dock::DockAreaState) -> SavedInbox {
    let sidebar = state.left_dock.as_ref().expect("the sidebar is saved");
    let inbox = find_panel(sidebar.panel(), Section::Inbox.name()).expect("the inbox panel");
    let PanelInfo::Panel(value) = &inbox.info else {
        panic!("the inbox saves its state");
    };
    serde_json::from_value(value["inbox"].clone()).expect("the inbox's state")
}

#[gpui::test]
fn inbox_sections_resize_fold_and_keep_it(first: &mut TestAppContext, next: &mut TestAppContext) {
    let scratch = Scratch::new("inbox-sections");
    let file = scratch.join("layout.json");
    let mut shell = open_shell(
        first,
        Options {
            layout_file: Some(file.clone()),
            ..Options::default()
        },
    );
    let before = block(&mut shell, 0);
    // The handle sits on the first section's bottom edge.
    let grip = point(before.center().x, before.bottom() - px(2.0));
    shell.drag(grip, point(grip.x, grip.y - px(40.0)));
    let after = block(&mut shell, 0);
    assert!(
        (after.size.height - (before.size.height - px(40.0))).abs() < px(3.0),
        "dragged 40px shorter: {before:?} -> {after:?}"
    );
    let sidebar = shell.sidebar();
    let size = shell.update(|_, cx| sidebar.read(cx).section_size(0));
    assert!(size.is_some(), "the drag is the section's size now");

    shell.click("inbox-section-1");
    let folded = block(&mut shell, 1);
    assert_eq!(folded.size.height, px(28.0), "folded to its header");
    assert!(!shell.drawn("inbox-row-38"), "its rows are gone");

    shell.advance(Duration::from_secs(1));
    let saved = layout::load_from(&file).expect("the shell saved the layout");
    let inbox = saved_inbox(&saved);
    assert_eq!(inbox.sections[0].share, size);
    assert!(inbox.sections[1].collapsed);

    let mut restored = open_shell(
        next,
        Options {
            layout: StartLayout::Saved(Box::new(saved)),
            ..Options::default()
        },
    );
    let sidebar = restored.sidebar();
    let (size_again, folded_again) = restored.update(|_, cx| {
        let sidebar = sidebar.read(cx);
        (sidebar.section_size(0), sidebar.section_collapsed(1))
    });
    assert_eq!(size_again, size);
    assert_eq!(folded_again, Some(true));
    assert_eq!(block(&mut restored, 1).size.height, px(28.0));
}

#[gpui::test]
fn each_inbox_section_scrolls_on_its_own(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    // Short enough that "Created by me" holds more rows than it has room for.
    shell.resize(1440.0, 600.0);
    let first_row = shell.bounds_of("inbox-row-42");
    let row = shell.bounds_of("inbox-row-38");
    // Less than a row, so the first row stays partly in view.
    shell.scroll_at(row.center(), 30.0);
    let scrolled = shell.bounds_of("inbox-row-38");
    assert!(
        scrolled.top() < row.top(),
        "its rows moved up: {row:?} -> {scrolled:?}"
    );
    assert_eq!(
        shell.bounds_of("inbox-row-42"),
        first_row,
        "the section above did not move"
    );
}

#[gpui::test]
fn grouping_by_repo_folds_and_hides_repositories(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    let sidebar = shell.sidebar();
    let subscriptions = shell
        .update(|_, cx| app::subscriptions::Subscriptions::global(cx))
        .expect("watching");
    let key = shell.key(1288);
    let watched = shell.update(|_, cx| subscriptions.read(cx).is_watched(&key, cx));

    shell.click("group-by-repo");
    assert!(shell.update(|_, cx| sidebar.read(cx).grouped_by_repo()));
    assert!(shell.drawn("repo-group-0-api"), "a group per repository");
    assert!(shell.drawn("inbox-row-1288"));

    shell.click("repo-group-0-api");
    assert!(
        !shell.drawn("inbox-row-1288"),
        "a folded group hides its rows"
    );
    assert!(shell.drawn("repo-group-0-api"), "and keeps its header");
    shell.click("repo-group-0-api");
    assert!(shell.drawn("inbox-row-1288"));

    shell.click("repo-menu-api");
    assert!(shell.drawn("repo-menu-hide") && shell.drawn("repo-menu-open"));
    shell.click("repo-menu-hide");
    assert_eq!(hidden(&mut shell), [repo_id(&key.repo)]);
    assert!(!shell.drawn("inbox-row-1288"), "hidden from every section");
    let session = shell.session();
    assert!(
        shell.update(|_, cx| session.read(cx).inbox.get(&key).is_some()),
        "still in the inbox: hiding is display only"
    );
    assert_eq!(
        shell.update(|_, cx| subscriptions.read(cx).is_watched(&key, cx)),
        watched,
        "and still watched as before"
    );
}

#[gpui::test]
fn the_repos_panel_shows_and_hides_every_repository(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    let (shown, total) = repos_shown(&mut shell);
    assert!(total >= 3 && shown == total, "{shown} of {total}");
    assert!(
        shell.drawn("section-toggle-Repos"),
        "it starts folded under the sidebar"
    );
    let sidebar = shell.sidebar();
    assert_eq!(
        shell.update(|_, cx| sidebar.read(cx).repos_note(cx)),
        format!("{total} of {total} shown"),
        "what its header says after its name"
    );

    shell.click("section-toggle-Repos");
    assert!(shell.drawn("section-Repos"), "unfolded in the sidebar");
    shell.click_element(("repo-shown", 0usize));
    assert_eq!(repos_shown(&mut shell), (total - 1, total));
    assert_eq!(hidden(&mut shell).len(), 1, "saved in the settings");

    shell.click_element(SharedString::from("repos-hide-all"));
    assert_eq!(repos_shown(&mut shell), (0, total));
    assert!(!shell.drawn("inbox-row-42"), "nothing listed");

    shell.click_element(SharedString::from("repos-show-all"));
    assert_eq!(repos_shown(&mut shell), (total, total));
    assert!(hidden(&mut shell).is_empty());
    assert!(shell.drawn("inbox-row-42"));
}

fn repos_search(shell: &mut ShellWindow) -> String {
    let sidebar = shell.sidebar();
    shell.update(|_, cx| sidebar.read(cx).repos_search_text().to_owned())
}

/// Whether the last frame drew the Repos panel's row of `repo`, read
/// without forcing a full redraw, as the next frame after typing draws it.
fn repo_row_drawn(shell: &mut ShellWindow, repo: &str) -> bool {
    shell.settle();
    shell
        .cx
        .debug_bounds(String::leak(format!("repo-row-{repo}")))
        .is_some()
}

#[gpui::test]
fn the_repos_panel_search_filters_its_list_as_typed(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    shell.click("section-toggle-Repos");
    let sidebar = shell.sidebar();
    let repos = shell.update(|_, cx| sidebar.read(cx).repos(cx));
    let api = repos
        .iter()
        .find(|repo| repo.ends_with("/api"))
        .expect("a repository named api")
        .clone();
    let other = repos
        .iter()
        .find(|repo| !repo.contains("api"))
        .expect("another repository")
        .clone();
    shell.redraw();
    assert!(repo_row_drawn(&mut shell, &api) && repo_row_drawn(&mut shell, &other));

    shell.click("repos-search");
    shell.type_text("API");
    assert_eq!(
        repos_search(&mut shell),
        "api",
        "typed into its box, any case"
    );
    assert!(repo_row_drawn(&mut shell, &api), "the matching one stays");
    assert!(
        !repo_row_drawn(&mut shell, &other),
        "the others go at once, without anything else redrawing"
    );
    assert_eq!(
        shell.update(|_, cx| sidebar.read(cx).listed_repos(cx)),
        std::slice::from_ref(&api)
    );
    assert_eq!(filter(&mut shell), "", "the inbox's own filter is separate");
    assert!(shell.drawn("inbox-row-42"), "and the inbox rows stay");

    shell.click("repos-search");
    shell.keys("end x");
    assert!(!repo_row_drawn(&mut shell, &api), "nothing matches now");
    assert!(shell.drawn("repos-no-match"));
    shell.keys("escape");
    assert_eq!(repos_search(&mut shell), "", "Esc empties it");
    assert!(repo_row_drawn(&mut shell, &api) && repo_row_drawn(&mut shell, &other));
}

#[gpui::test]
fn inbox_rows_show_their_counts_beside_the_title(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    let row = shell.bounds_of("inbox-row-42");
    let counts = shell.bounds_of("inbox-counts-42");
    assert!(counts.size.width > px(0.0), "comments and lines are drawn");
    assert!(
        counts.right() <= row.right() && counts.top() >= row.center().y - px(4.0),
        "on the author line, inside the row: {counts:?} in {row:?}"
    );
    assert!(
        counts.size.width < row.size.width / 2.0,
        "they leave the title its room"
    );
}

fn filter(shell: &mut ShellWindow) -> String {
    let sidebar = shell.sidebar();
    shell.update(|_, cx| sidebar.read(cx).filter_text().to_owned())
}

fn cursor(shell: &mut ShellWindow) -> Option<u64> {
    let sidebar = shell.sidebar();
    shell.update(|_, cx| sidebar.read(cx).cursor_key().map(|key| key.number.0))
}

#[gpui::test]
fn the_filter_box_takes_typing_and_filters_rows_live(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    assert!(
        shell.drawn("repo-filter"),
        "at the top of the inbox, Repos folded"
    );
    shell.keys("j");
    let selected = cursor(&mut shell);
    assert!(selected.is_some(), "j moves the cursor in the list");

    shell.click("repo-filter");
    shell.keys("A p i");
    assert_eq!(filter(&mut shell), "api", "typed into the box, any case");
    assert!(shell.drawn("inbox-row-1288"), "acme/api rows stay");
    assert!(!shell.drawn("inbox-row-42"), "cora/review rows go");
    assert!(!shell.drawn("inbox-row-412"), "acme/web rows go");

    shell.click("repo-filter");
    shell.keys("end j");
    assert_eq!(filter(&mut shell), "apij", "j is text in the box");
    assert_eq!(cursor(&mut shell), selected, "and did not move the cursor");
    assert!(!shell.drawn("inbox-row-1288"), "nothing matches now");

    shell.keys("backspace");
    assert!(shell.drawn("inbox-row-1288"));
    shell.keys("escape");
    assert_eq!(filter(&mut shell), "", "Esc empties it");
    assert!(shell.drawn("inbox-row-42") && shell.drawn("inbox-row-1288"));
}

fn heights(shell: &mut ShellWindow) -> Vec<f32> {
    shell.redraw();
    let sidebar = shell.sidebar();
    shell.update(|_, cx| sidebar.read(cx).section_heights().to_vec())
}

fn near(a: &[f32], b: &[f32]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(a, b)| (a - b).abs() < 2.0)
}

#[gpui::test]
fn inner_sections_keep_their_shares_through_outer_resizes(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    // Three-line rows: the Inbox panel needs room for its sections to
    // have something to give.
    shell.resize(1440.0, 1200.0);
    shell.open_pr(41);
    let inbox = shell.bounds_of(String::leak(format!("section-{}", Section::Inbox.name())));
    let edge = point(inbox.center().x, inbox.bottom());
    shell.drag(edge, point(edge.x, edge.y + px(200.0)));
    // A drag of the first inner handle gives every section a share.
    let first = block(&mut shell, 0);
    let grip = point(first.center().x, first.bottom() - px(2.0));
    shell.drag(grip, point(grip.x, grip.y - px(30.0)));
    let before = heights(&mut shell);
    let open: f32 = before.iter().sum();
    let shares: Vec<f32> = before.iter().map(|h| h / open).collect();

    // Shrink the Inbox panel through the sidebar's own split, then grow it
    // back: every section returns to its share.
    let inbox = shell.bounds_of(String::leak(format!("section-{}", Section::Inbox.name())));
    let edge = point(inbox.center().x, inbox.bottom());
    shell.drag(edge, point(edge.x, edge.y - px(120.0)));
    let shrunk = heights(&mut shell);
    assert!(
        shrunk.iter().sum::<f32>() < open - 60.0,
        "the panel shrank: {before:?} -> {shrunk:?}"
    );
    let inbox = shell.bounds_of(String::leak(format!("section-{}", Section::Inbox.name())));
    let edge = point(inbox.center().x, inbox.bottom());
    shell.drag(edge, point(edge.x, edge.y + px(120.0)));
    let grown = heights(&mut shell);
    let total: f32 = grown.iter().sum();
    let back: Vec<f32> = grown.iter().map(|h| h / total).collect();
    assert!(
        near(
            &back.iter().map(|s| s * 1000.0).collect::<Vec<_>>(),
            &shares.iter().map(|s| s * 1000.0).collect::<Vec<_>>()
        ),
        "shares came back: {shares:?} -> {back:?}"
    );
    assert!(
        near(&grown, &before),
        "and the heights: {before:?} -> {grown:?}"
    );

    // The inner handle moves the boundary: what the first gives, the next
    // open one takes, and the sections still fill the panel.
    let first = block(&mut shell, 0);
    let grip = point(first.center().x, first.bottom() - px(2.0));
    shell.drag(grip, point(grip.x, grip.y - px(10.0)));
    let up = heights(&mut shell);
    assert!(up[0] < grown[0] - 5.0, "{grown:?} -> {up:?}");
    let next = (1..up.len())
        .find(|&ix| grown[ix] > 28.0)
        .expect("an open section under the first");
    assert!(
        (up[next] - grown[next] - (grown[0] - up[0])).abs() < 2.0,
        "the next takes it: {grown:?} -> {up:?}"
    );
    assert!((up.iter().sum::<f32>() - total).abs() < 2.0);
    let first = block(&mut shell, 0);
    let grip = point(first.center().x, first.bottom() - px(2.0));
    shell.drag(grip, point(grip.x, grip.y + px(20.0)));
    let down = heights(&mut shell);
    assert!(down[0] > up[0] + 5.0, "{up:?} -> {down:?}");
    assert!(down[0] >= app::inbox::DRAG_MIN - 1.0, "not under two rows");
}

/// Opens exactly the sections in `open`, folds the rest.
fn open_only(shell: &mut ShellWindow, open: &[usize]) {
    let sidebar = shell.sidebar();
    shell.update(|_, cx| {
        sidebar.update(cx, |sidebar, cx| {
            for ix in 0..5 {
                if sidebar.section_collapsed(ix) == Some(open.contains(&ix)) {
                    sidebar.toggle_section(ix, cx);
                }
            }
        });
    });
    shell.settle();
}

/// Drags the handle at the bottom of section `ix` by `dy` (up is negative).
fn drag_handle(shell: &mut ShellWindow, ix: usize, dy: f32) {
    let bounds = block(shell, ix);
    let grip = point(bounds.center().x, bounds.bottom() - px(2.0));
    shell.drag(grip, point(grip.x, grip.y + px(dy)));
}

const LEAST: f32 = app::inbox::DRAG_MIN;

#[gpui::test]
fn the_boundary_between_open_sections_moves_beside_folded_ones(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    shell.resize(1440.0, 1100.0);
    // The user's shape: an open section, two folded, then an open one.
    open_only(&mut shell, &[0, 3]);
    let start = heights(&mut shell);
    let total: f32 = start.iter().sum();
    assert!(
        start[0] > LEAST + 40.0 && start[3] > LEAST + 40.0,
        "both open sections have room to shrink: {start:?}"
    );
    assert_eq!(start[1], 28.0);
    assert_eq!(start[2], 28.0);
    let filled = |heights: &[f32]| (heights.iter().sum::<f32>() - total).abs() < 2.0;

    // The first shrinks from its handle and the last open one takes the
    // space: nothing is left empty.
    drag_handle(&mut shell, 0, -40.0);
    let first = heights(&mut shell);
    assert!(first[0] < start[0] - 30.0, "{start:?} -> {first:?}");
    assert!(first[3] > start[3] + 30.0 && filled(&first), "{first:?}");

    // The last open one fills the rest and has no handle.
    drag_handle(&mut shell, 3, -40.0);
    assert!(near(&heights(&mut shell), &first), "nothing to drag");

    // Neither goes under about two rows, however far it is dragged.
    drag_handle(&mut shell, 0, -2000.0);
    let least = heights(&mut shell);
    assert!(
        (least[0] - LEAST).abs() < 2.0 && filled(&least),
        "{least:?}"
    );
    drag_handle(&mut shell, 0, 2000.0);
    let most = heights(&mut shell);
    assert!((most[3] - LEAST).abs() < 2.0 && filled(&most), "{most:?}");

    // And back: growing the last takes from the first.
    drag_handle(&mut shell, 0, -40.0);
    let back = heights(&mut shell);
    assert!(
        back[3] > most[3] + 30.0 && filled(&back),
        "{most:?} -> {back:?}"
    );
}

#[gpui::test]
fn shares_survive_a_shrunk_then_regrown_panel(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    shell.resize(1440.0, 1100.0);
    open_only(&mut shell, &[0, 3]);
    drag_handle(&mut shell, 0, -40.0);
    let before = heights(&mut shell);
    shell.resize(1440.0, 700.0);
    let small = heights(&mut shell);
    assert!(small.iter().sum::<f32>() < before.iter().sum::<f32>() - 20.0);
    shell.resize(1440.0, 1100.0);
    let back = heights(&mut shell);
    assert!(near(&back, &before), "{before:?} -> {small:?} -> {back:?}");
}

#[gpui::test]
fn a_click_anywhere_on_a_header_folds_it_and_enter_or_space_does_too(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    let sidebar = shell.sidebar();
    let folded = |shell: &mut ShellWindow, ix: usize| {
        shell.update(|_, cx| sidebar.read(cx).section_collapsed(ix))
    };

    // The middle of the row, far from the chevron.
    let header = shell.bounds_of("inbox-section-0");
    shell.click_at(point(header.right() - px(80.0), header.center().y));
    assert_eq!(folded(&mut shell, 0), Some(true));
    shell.keys("enter");
    assert_eq!(
        folded(&mut shell, 0),
        Some(false),
        "Enter on the focused header"
    );
    shell.keys("space");
    assert_eq!(folded(&mut shell, 0), Some(true), "and Space");
    shell.keys("space");

    // The Repos panel, folded under the sidebar, and its fold button once
    // open.
    shell.click("section-toggle-Repos");
    assert!(shell.element(("repo-shown", 0usize)).is_some());
    shell.click("section-toggle-Repos");
    assert!(
        shell.element(("repo-shown", 0usize)).is_none(),
        "folded again"
    );

    // The fold button in the sidebar section's tab bar folds the whole
    // section.
    shell.click("section-toggle-Inbox");
    let inbox = shell.update(|_, cx| sidebar.read(cx).section_collapsed(0));
    assert!(inbox.is_some());
    assert!(!shell.drawn("inbox-section-0"), "the inbox panel is folded");
    assert!(
        shell.drawn("section-toggle-Inbox"),
        "its header is stacked below"
    );
    shell.click("section-toggle-Inbox");
    assert!(shell.drawn("inbox-section-0"), "and unfolds again");
}

#[gpui::test]
fn a_wheel_step_over_the_inbox_list_reaches_the_screen_without_other_redraws(
    cx: &mut TestAppContext,
) {
    let mut shell = open_shell(cx, Options::default());
    shell.resize(1440.0, 600.0);
    let row = shell.bounds_of("inbox-row-38");
    // The wheel only; no forced redraw between it and the read, as a user's
    // wheel gives. A list that does not repaint keeps its row where it was.
    shell.scroll_at(row.center(), 30.0);
    let scrolled = shell
        .cx
        .debug_bounds("inbox-row-38")
        .expect("the row is still drawn");
    assert!(
        scrolled.top() < row.top(),
        "the wheel moved the drawn rows: {row:?} -> {scrolled:?}"
    );
}

/// Lists fixture draft #44 first in the review requested section.
fn draft_in_review_requested(shell: &mut ShellWindow) {
    let key = shell.key(44);
    let session = shell.session();
    shell.update(|_, cx| {
        session.update(cx, |session, cx| {
            let mut inbox = (*session.inbox).clone();
            let ix = inbox.index_of(&key).expect("the draft is in the inbox");
            inbox.sections[0].items.retain(|&item| item != ix);
            inbox.sections[0].items.insert(0, ix);
            session.set_inbox(inbox, cx);
        });
    });
    shell.settle();
}

fn section_note(shell: &mut ShellWindow, ix: usize) -> String {
    let sidebar = shell.sidebar();
    shell.update(|_, cx| sidebar.read(cx).section_note(ix, cx))
}

#[gpui::test]
fn review_requested_can_leave_its_drafts_out_and_remembers_it(
    first: &mut TestAppContext,
    next: &mut TestAppContext,
) {
    let scratch = Scratch::new("hide-drafts");
    let file = scratch.join("layout.json");
    let mut shell = open_shell(
        first,
        Options {
            layout_file: Some(file.clone()),
            ..Options::default()
        },
    );
    draft_in_review_requested(&mut shell);
    let total = section_note(&mut shell, 0);
    assert!(shell.drawn("inbox-row-44"), "the draft is listed");

    // The header's menu opens on a right click.
    let at = shell.bounds_of("inbox-section-0").center();
    let none = gpui::Modifiers::none();
    shell.cx.simulate_mouse_move(at, None, none);
    shell
        .cx
        .simulate_mouse_down(at, gpui::MouseButton::Right, none);
    shell
        .cx
        .simulate_mouse_up(at, gpui::MouseButton::Right, none);
    shell.settle();
    assert!(
        shell.element(SharedString::from("popup-menu")).is_some(),
        "the section header's menu is open"
    );
    shell.keys("escape");

    // What its "Hide drafts" entry does.
    let sidebar = shell.sidebar();
    shell.update(|_, cx| sidebar.update(cx, |s, cx| s.set_section_hides_drafts(0, true, cx)));
    shell.settle();
    assert!(!shell.drawn("inbox-row-44"), "the draft is left out");
    assert!(
        section_note(&mut shell, 0).ends_with(", 1 draft hidden"),
        "the count says so: {}",
        section_note(&mut shell, 0)
    );
    assert!(
        shell.drawn("inbox-row-42"),
        "the other pull requests stay listed"
    );
    let key = shell.key(44);
    let session = shell.session();
    assert!(
        shell.update(|_, cx| session.read(cx).inbox.get(&key).is_some()),
        "display only: it is still in the inbox"
    );
    let other = section_note(&mut shell, 1);
    assert!(!other.contains("draft"), "other sections keep their drafts");
    shell.update(|_, cx| sidebar.update(cx, |s, cx| s.set_section_hides_drafts(1, true, cx)));
    assert!(
        !shell.update(|_, cx| sidebar.read(cx).section_hides_drafts(1)),
        "only review requested offers it"
    );

    // It is in the saved layout and comes back with it.
    shell.advance(Duration::from_secs(1));
    let saved = layout::load_from(&file).expect("the shell saved the layout");
    let mut restored = open_shell(
        next,
        Options {
            layout: StartLayout::Saved(Box::new(saved)),
            ..Options::default()
        },
    );
    draft_in_review_requested(&mut restored);
    assert!(!restored.drawn("inbox-row-44"), "still left out");
    let sidebar = restored.sidebar();
    assert!(restored.update(|_, cx| sidebar.read(cx).section_hides_drafts(0)));

    // Off again: back in the list and the count.
    restored.update(|_, cx| sidebar.update(cx, |s, cx| s.set_section_hides_drafts(0, false, cx)));
    restored.settle();
    assert!(restored.drawn("inbox-row-44"));
    assert_eq!(section_note(&mut restored, 0), total);
}

#[gpui::test]
fn a_pull_request_row_has_three_lines_in_the_height_the_list_counts(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    let row = shell.bounds_of("inbox-row-42");
    let title = shell.bounds_of("inbox-title-42");
    let more = shell.bounds_of("inbox-more-42");
    let review = shell.bounds_of("inbox-review-42");
    let counts = shell.bounds_of("inbox-counts-42");
    assert_eq!(
        row.size.height,
        px(app::inbox::PR_ROW),
        "the list's row height"
    );
    assert!(
        (more.center().y - title.center().y).abs() < px(2.0),
        "the ... button sits on the title line"
    );
    assert!(
        review.top() >= title.bottom() + px(15.0),
        "the status line is under the reference line, which is under the title"
    );
    assert!(
        (counts.center().y - review.center().y).abs() < px(2.0),
        "chips and review state share the status line"
    );
    assert!(
        review.bottom() <= row.bottom() && counts.right() <= row.right(),
        "nothing spills out of the row"
    );
}
