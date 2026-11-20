//! The sidebar's sections: Inbox, Stack, Threads, Files and Repos
//! are dock panels in one column that can split into two, follow the
//! focused pull request, keep a fold in the saved layout, and pin the pull
//! request's title above them.

use std::time::Duration;

use app::layout;
use app::sidebar_panels::{Section, SectionPanel, beside_inbox};
use gpui::{Bounds, Entity, Modifiers, MouseButton, Pixels, TestAppContext, px};
use gpui_kit::component::dock::{DockPlacement, DockState, InsertTarget, PanelInfo, PanelState};

use crate::harness::{Options, Scratch, ShellWindow, StartLayout, open_shell};

fn bounds(shell: &mut ShellWindow, section: Section) -> Bounds<Pixels> {
    shell.bounds_of(String::leak(format!("section-{}", section.name())))
}

fn panel(shell: &mut ShellWindow, section: Section) -> Entity<SectionPanel> {
    let id = shell.section(section);
    let dock = shell.sidebar_dock();
    shell.update(|_, cx| {
        dock.read(cx)
            .panel(id)
            .and_then(|panel| panel.view().downcast::<SectionPanel>().ok())
            .expect("a section panel")
    })
}

/// The names of the sidebar's panels, column by column.
fn columns(shell: &mut ShellWindow) -> Vec<Vec<String>> {
    fn names(state: &gpui_kit::component::dock::PanelState, out: &mut Vec<String>) {
        match state.info {
            PanelInfo::Tabs { .. } => {
                out.extend(state.children.iter().map(|p| p.panel_name.clone()));
            }
            _ => state.children.iter().for_each(|child| names(child, out)),
        }
    }
    let layout = shell.sidebar_layout();
    let root = &layout;
    match root.info.axis() {
        Some(gpui::Axis::Horizontal) => root
            .children
            .iter()
            .map(|column| {
                let mut out = Vec::new();
                names(column, &mut out);
                out
            })
            .collect(),
        _ => {
            let mut out = Vec::new();
            names(root, &mut out);
            vec![out]
        }
    }
}

#[gpui::test]
fn the_sections_start_in_one_column_and_split_into_two(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    shell.open_pr(41);
    assert_eq!(
        columns(&mut shell),
        [vec!["Inbox", "Stack", "Threads", "Files", "Repos"]],
        "one column, the inbox on top"
    );
    let (inbox, threads) = (
        bounds(&mut shell, Section::Inbox),
        bounds(&mut shell, Section::Threads),
    );
    assert!(
        threads.top() >= inbox.bottom(),
        "the threads sit under the inbox"
    );

    // What dropping the Threads tab right of the inbox does.
    let dock = shell.sidebar_dock();
    shell.update(|window, cx| beside_inbox(&dock, Section::Threads, window, cx));
    shell.settle();
    let layout = columns(&mut shell);
    assert_eq!(layout.len(), 2, "two columns: {layout:?}");
    assert!(layout[1].contains(&"Threads".to_owned()));
    let (inbox, threads) = (
        bounds(&mut shell, Section::Inbox),
        bounds(&mut shell, Section::Threads),
    );
    assert!(
        threads.left() >= inbox.right() - gpui::px(1.0),
        "the threads show beside the inbox: {inbox:?} {threads:?}"
    );
    assert!(
        threads.top() < inbox.bottom() && inbox.top() < threads.bottom(),
        "side by side, not stacked"
    );
    assert!(
        shell.drawn("pinned-title-41"),
        "the threads' column pins the title too"
    );
}

#[gpui::test]
fn the_sections_follow_the_focused_pull_request(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    assert!(
        !shell.drawn("section-Files"),
        "with nothing open, only the inbox takes space"
    );
    shell.open_pr(41);
    assert!(shell.drawn("pinned-title-41"));
    assert!(shell.drawn("section-Files"));
    shell.open_pr(42);
    assert!(shell.drawn("pinned-title-42"), "the new focus");
    assert!(!shell.drawn("pinned-title-41"));
    let key = shell.key(41);
    let session = shell.session();
    shell.update(|_, cx| session.update(cx, |session, cx| session.focus(&key, cx)));
    shell.settle();
    assert!(shell.drawn("pinned-title-41"), "and back");
}

#[gpui::test]
fn the_pinned_title_stays_above_the_sections(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    shell.open_pr(41);
    let title = shell.bounds_of("pinned-title-41");
    let first = bounds(&mut shell, Section::Stack);
    assert_eq!(
        title.top(),
        first.top(),
        "pinned at the top of the first section"
    );
    let row = shell.bounds_of("thread-row-0");
    shell.scroll_at(row.center(), 200.0);
    assert_eq!(
        shell.bounds_of("pinned-title-41"),
        title,
        "scrolling a section never moves it"
    );
    assert!(
        bounds(&mut shell, Section::Inbox).bottom() <= title.top(),
        "the inbox's edge never meets scrolled rows"
    );
}

#[gpui::test]
fn a_folded_section_keeps_its_fold_in_the_layout(
    first: &mut TestAppContext,
    next: &mut TestAppContext,
) {
    let scratch = Scratch::new("section-fold");
    let file = scratch.join("layout.json");
    let mut shell = open_shell(
        first,
        Options {
            layout_file: Some(file.clone()),
            open_pr: Some(41),
            ..Options::default()
        },
    );
    let open = bounds(&mut shell, Section::Inbox);
    let others_before: f32 = heights(&mut shell)[1..].iter().sum();
    let fold = shell.bounds_of("section-toggle-Inbox");
    let tab = shell.bounds_of("section-title-Inbox");
    assert!(
        fold.right() <= tab.left() && fold.left() >= open.left(),
        "the fold button leads the tabs: {fold:?}, {tab:?}"
    );
    shell.click("section-toggle-Inbox");
    shell.redraw();
    let inbox = panel(&mut shell, Section::Inbox);
    assert!(shell.update(|_, cx| inbox.read(cx).is_collapsed()));
    assert!(
        !shell.drawn("section-Inbox"),
        "a folded section leaves the dock's slots"
    );
    assert!(
        shell.drawn("section-toggle-Inbox"),
        "its header stays under the dock"
    );
    let others: f32 = heights(&mut shell)[1..].iter().sum();
    assert!(others > others_before + 50.0, "the room went to the others");

    shell.advance(Duration::from_secs(1));
    let saved = layout::load_from(&file).expect("the shell saved the layout");
    assert!(saved.left_dock.is_some(), "with the sidebar in it");
    let mut restored = open_shell(
        next,
        Options {
            layout: StartLayout::Saved(Box::new(saved)),
            ..Options::default()
        },
    );
    let inbox = panel(&mut restored, Section::Inbox);
    assert!(
        restored.update(|_, cx| inbox.read(cx).is_collapsed()),
        "the fold came back"
    );
    restored.click("section-toggle-Inbox");
    restored.redraw();
    let unfolded = bounds(&mut restored, Section::Inbox);
    assert!(
        (unfolded.size.height - open.size.height).abs() < gpui::px(20.0),
        "unfolded to about the height it had (no pull request is open here to share the column): {unfolded:?} vs {open:?}"
    );
}

fn heights(shell: &mut ShellWindow) -> Vec<f32> {
    Section::ALL
        .iter()
        .map(|&section| {
            let selector = format!("section-{}", section.name());
            if shell.drawn(&selector) {
                f32::from(bounds(shell, section).size.height)
            } else {
                0.0
            }
        })
        .collect()
}

/// Drags the divider under `section` by `dy`.
fn drag_divider(shell: &mut ShellWindow, section: Section, dy: f32) {
    let b = bounds(shell, section);
    let from = gpui::point(b.center().x, b.bottom());
    shell.drag(from, gpui::point(from.x, from.y + gpui::px(dy)));
}

#[gpui::test]
fn a_folded_section_gives_its_space_to_the_open_ones(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    shell.resize(1440.0, 1000.0);
    shell.open_pr(41);
    // The user gives Files most of the column, squeezing the others.
    drag_divider(&mut shell, Section::Threads, -150.0);
    drag_divider(&mut shell, Section::Inbox, -150.0);
    let big = heights(&mut shell);
    eprintln!("BIG {big:?}");
    shell.click("section-toggle-Files");
    let folded = heights(&mut shell);
    eprintln!("FOLDED {folded:?}");
    assert!(!shell.drawn("section-Files"), "Files left the dock's slots");
    assert!(
        shell.drawn("section-toggle-Files"),
        "its header is under the dock"
    );
    assert!(
        folded[0] > big[0] + 100.0,
        "the inbox got the room: {big:?} -> {folded:?}"
    );
    let column: f32 = big.iter().sum();

    // The inbox can grow back over everything else, whatever is folded.
    drag_divider(&mut shell, Section::Inbox, 2000.0);
    let grown = heights(&mut shell);
    eprintln!("GROWN {grown:?}");
    assert!(
        grown[0] > folded[0] + 20.0 || grown[0] > column - 400.0,
        "{folded:?} -> {grown:?}"
    );

    // Unfolded, Files takes its size back and the others share as before.
    shell.click("section-toggle-Files");
    let back = heights(&mut shell);
    eprintln!("BACK {back:?}");
    assert!(back[3] > 150.0, "{back:?}");
}

/// Puts the Threads section in the Files section's tab group, Threads
/// shown: what dropping its tab on Files' tab bar does.
fn threads_with_files(shell: &mut ShellWindow) {
    let (files, threads) = (
        shell.section(Section::Files),
        shell.section(Section::Threads),
    );
    let dock = shell.sidebar_dock();
    shell.update(|window, cx| {
        dock.update(cx, |dock, cx| {
            let node = dock
                .layout(DockPlacement::Left)
                .and_then(|tree| tree.find_panel_node(files))
                .expect("the Files group");
            let target = InsertTarget::Tabs {
                node,
                ix: None,
                activate: true,
            };
            dock.move_panel(threads, target, window, cx);
        });
    });
    shell.settle();
}

fn folded(shell: &mut ShellWindow, section: Section) -> bool {
    let panel = panel(shell, section);
    shell.update(|_, cx| panel.read(cx).is_collapsed())
}

#[gpui::test]
fn folding_one_tab_of_a_group_folds_the_whole_group(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    shell.open_pr(41);
    threads_with_files(&mut shell);
    assert!(shell.drawn("section-Threads") && !shell.drawn("section-Files"));

    // Files is the shown tab; the group's fold button folds both.
    shell.click("section-title-Files");
    assert!(shell.drawn("section-Files"));
    assert!(
        !shell.drawn("section-toggle-Threads"),
        "one fold button per group, the shown tab's"
    );
    shell.click("section-toggle-Files");
    assert!(folded(&mut shell, Section::Files) && folded(&mut shell, Section::Threads));
    assert!(!shell.drawn("section-Files") && !shell.drawn("section-Threads"));
    assert_eq!(
        columns(&mut shell),
        [vec!["Inbox", "Stack", "Files", "Threads", "Repos"]],
        "the group keeps its tabs"
    );

    // One strip row holds both names.
    let (files, threads) = (
        shell.bounds_of("section-toggle-Files"),
        shell.bounds_of("section-toggle-Threads"),
    );
    assert_eq!(files.top(), threads.top(), "both names in one row");
    assert!(files.right() <= threads.left(), "in tab order");
    let row = shell.bounds_of("section-folded-group-Files");
    assert!(row.contains(&files.center()) && row.contains(&threads.center()));
    assert!(
        !shell.drawn("section-folded-group-Threads"),
        "no second row for Threads"
    );

    // The second name unfolds the group with that tab shown.
    shell.click("section-toggle-Threads");
    assert!(!folded(&mut shell, Section::Files) && !folded(&mut shell, Section::Threads));
    assert!(shell.drawn("section-Threads"), "Threads is the shown tab");
    assert!(!shell.drawn("section-Files"));
    assert!(!shell.drawn("section-folded-group-Files"));

    // Folded again and unfolded by the first name: Files is shown.
    shell.click("section-toggle-Threads");
    assert!(folded(&mut shell, Section::Threads) && folded(&mut shell, Section::Files));
    shell.click("section-toggle-Files");
    assert!(shell.drawn("section-Files") && !shell.drawn("section-Threads"));
}

#[gpui::test]
fn a_tab_click_only_selects_and_the_fold_button_leads_the_tabs(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    shell.open_pr(41);
    threads_with_files(&mut shell);

    shell.click("section-title-Files");
    assert!(shell.drawn("section-Files") && !shell.drawn("section-Threads"));
    shell.click("section-title-Files");
    assert!(
        !folded(&mut shell, Section::Files) && !folded(&mut shell, Section::Threads),
        "a click on the shown tab does not fold it either"
    );
    assert!(shell.drawn("section-Files"));

    // The button leads the tab bar at the sidebar's edge, not in a tab; the
    // tabs scroll beside it when they do not fit.
    let button = shell.bounds_of("section-toggle-Files");
    let (files, threads) = (
        shell.bounds_of("section-title-Files"),
        shell.bounds_of("section-title-Threads"),
    );
    assert!(
        button.left() < px(12.0) && button.right() <= threads.left(),
        "{button:?} before {files:?} and {threads:?}"
    );
    assert!(button.top() < files.bottom() && files.top() < button.bottom());
    shell.click("section-toggle-Files");
    assert!(
        folded(&mut shell, Section::Files) && folded(&mut shell, Section::Threads),
        "the button folds the group"
    );
}

fn set_folded(state: &mut PanelState, name: &str) {
    if state.panel_name == name {
        state.info = PanelInfo::panel(serde_json::json!({ "collapsed": true, "restore": 200.0 }));
    }
    state.children.iter_mut().for_each(|c| set_folded(c, name));
}

#[gpui::test]
fn a_saved_group_with_mixed_folds_opens_unfolded(
    first: &mut TestAppContext,
    next: &mut TestAppContext,
) {
    let mut first = open_shell(first, Options::default());
    first.open_pr(41);
    threads_with_files(&mut first);
    let dock = first.sidebar_dock();
    let mut state = first.update(|_, cx| dock.read(cx).dump(cx));
    let left = state.left_dock.take().expect("a sidebar");
    let mut panel_state = left.panel().clone();
    set_folded(&mut panel_state, "Files");
    state.left_dock = Some(DockState::new(
        panel_state,
        left.placement(),
        left.size(),
        left.open(),
    ));
    let mut restored = open_shell(
        next,
        Options {
            layout: StartLayout::Saved(Box::new(state)),
            ..Options::default()
        },
    );
    assert!(!folded(&mut restored, Section::Files));
    assert!(!folded(&mut restored, Section::Threads));
}

/// The open sections of the sidebar's one column, top to bottom, checked to
/// fill it: each starts one tab bar under the one before, and the last ends
/// on the border over the folded groups' rows.
fn open_column(shell: &mut ShellWindow) -> Vec<(Section, Bounds<Pixels>)> {
    let mut open = Vec::new();
    for section in Section::ALL {
        if shell.drawn(&format!("section-{}", section.name())) {
            open.push((section, bounds(shell, section)));
        }
    }
    open.sort_by(|a, b| f32::from(a.1.top()).total_cmp(&f32::from(b.1.top())));
    if let [first, second, ..] = open[..] {
        let bar = second.1.top() - first.1.bottom();
        for pair in open.windows(2) {
            let gap = pair[1].1.top() - pair[0].1.bottom();
            assert!(
                (gap - bar).abs() < px(0.5),
                "{:?} starts a tab bar under {:?}: {open:?}",
                pair[1].0,
                pair[0].0
            );
        }
    }
    let mut first_row: Option<Pixels> = None;
    for section in Section::ALL {
        let row = format!("section-folded-group-{}", section.name());
        if shell.drawn(&row) {
            let top = shell.bounds_of(String::leak(row)).top();
            first_row = Some(first_row.map_or(top, |row| row.min(top)));
        }
    }
    let first_row = first_row.expect("Repos is folded under the column");
    let last = open.last().expect("an open section").1;
    assert!(
        (first_row - last.bottom() - px(1.0)).abs() < px(0.5),
        "the last open section reaches the folded rows: {last:?}, rows at {first_row:?}"
    );
    open
}

fn sections(open: &[(Section, Bounds<Pixels>)]) -> Vec<Section> {
    open.iter().map(|(section, _)| *section).collect()
}

fn height(open: &[(Section, Bounds<Pixels>)], section: Section) -> f32 {
    open.iter()
        .find(|(s, _)| *s == section)
        .map(|(_, b)| f32::from(b.size.height))
        .unwrap_or_else(|| panic!("{section:?} is not open: {open:?}"))
}

/// Asserts each section's height changed by its `by`, in pixels.
fn assert_moved(
    before: &[(Section, Bounds<Pixels>)],
    after: &[(Section, Bounds<Pixels>)],
    by: &[(Section, f32)],
) {
    for &(section, by) in by {
        let (was, is) = (height(before, section), height(after, section));
        assert!(
            (is - was - by).abs() < 1.0,
            "{section:?}: {was} -> {is}, expected {by:+}"
        );
    }
}

/// Drags the boundary under `section` by `dy`.
fn drag_edge(shell: &mut ShellWindow, section: Section, dy: f32) {
    let b = bounds(shell, section);
    let from = gpui::point(b.center().x, b.bottom());
    shell.drag(from, gpui::point(from.x, from.y + px(dy)));
}

#[gpui::test]
fn every_boundary_between_open_groups_drags(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    shell.resize(1440.0, 1000.0);
    // Stack folded hides the slot under the inbox; Threads folded hides the
    // slot over Files too.
    shell.open_pr(42);
    shell.click("section-toggle-Stack");
    shell.click("section-toggle-Threads");
    let start = open_column(&mut shell);
    assert_eq!(sections(&start), [Section::Inbox, Section::Files]);

    drag_edge(&mut shell, Section::Inbox, -80.0);
    let up = open_column(&mut shell);
    assert_moved(
        &start,
        &up,
        &[(Section::Inbox, -80.0), (Section::Files, 80.0)],
    );
    drag_edge(&mut shell, Section::Inbox, 120.0);
    let down = open_column(&mut shell);
    assert_moved(
        &up,
        &down,
        &[(Section::Inbox, 120.0), (Section::Files, -120.0)],
    );

    // Threads open again between them, the Stack slot still folded.
    shell.click("section-toggle-Threads");
    let start = open_column(&mut shell);
    assert_eq!(
        sections(&start),
        [Section::Inbox, Section::Threads, Section::Files]
    );
    drag_edge(&mut shell, Section::Inbox, -60.0);
    let up = open_column(&mut shell);
    assert_moved(
        &start,
        &up,
        &[
            (Section::Inbox, -60.0),
            (Section::Threads, 60.0),
            (Section::Files, 0.0),
        ],
    );
    // And the boundary under Threads, with no hidden slot at it.
    drag_edge(&mut shell, Section::Threads, 40.0);
    let down = open_column(&mut shell);
    assert_moved(
        &up,
        &down,
        &[
            (Section::Inbox, 0.0),
            (Section::Threads, 40.0),
            (Section::Files, -40.0),
        ],
    );
}

#[gpui::test]
fn the_open_sections_fill_the_column_folded_or_not(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    shell.resize(1440.0, 1000.0);
    assert_eq!(
        sections(&open_column(&mut shell)),
        [Section::Inbox],
        "with nothing open the inbox reaches Repos' row"
    );
    shell.open_pr(41);
    open_column(&mut shell);
    shell.click("section-toggle-Threads");
    shell.click("section-toggle-Files");
    let open = sections(&open_column(&mut shell));
    assert_eq!(open, [Section::Inbox, Section::Stack]);
    assert!(
        shell.drawn("section-folded-group-Threads") && shell.drawn("section-folded-group-Files"),
        "a header row each"
    );
    shell.click("section-toggle-Threads");
    shell.click("section-toggle-Files");
    assert_eq!(
        sections(&open_column(&mut shell)),
        [
            Section::Inbox,
            Section::Stack,
            Section::Threads,
            Section::Files
        ]
    );
    assert!(!shell.drawn("section-folded-group-Threads"));
}

/// The inbox slot's size in the layout saved to `file`, if one was saved.
fn saved_inbox_size(file: &std::path::Path) -> Option<f32> {
    let saved = layout::load_from(file)?;
    let sizes = saved.left_dock?.panel().info.sizes()?.clone();
    sizes.first().map(|&size| f32::from(size))
}

#[gpui::test]
fn a_boundary_drag_saves_the_layout_when_it_ends(cx: &mut TestAppContext) {
    let scratch = Scratch::new("boundary-drag");
    let file = scratch.join("layout.json");
    let mut shell = open_shell(
        cx,
        Options {
            layout_file: Some(file.clone()),
            open_pr: Some(42),
            ..Options::default()
        },
    );
    shell.resize(1440.0, 1000.0);
    shell.advance(Duration::from_secs(1));
    let before = saved_inbox_size(&file);
    let start = open_column(&mut shell);

    // Pressed past the hidden Stack slot and moved, not yet let go.
    let inbox = bounds(&mut shell, Section::Inbox);
    let edge = gpui::point(inbox.center().x, inbox.bottom());
    let none = Modifiers::none();
    shell.cx.simulate_mouse_move(edge, None, none);
    shell.cx.simulate_mouse_down(edge, MouseButton::Left, none);
    for step in 1..=4u8 {
        let at = gpui::point(edge.x, edge.y - px(20.0 * f32::from(step)));
        shell.cx.simulate_mouse_move(at, MouseButton::Left, none);
    }
    shell.advance(Duration::from_secs(1));
    let moving = open_column(&mut shell);
    assert_moved(&start, &moving, &[(Section::Inbox, -80.0)]);
    assert_eq!(
        saved_inbox_size(&file),
        before,
        "nothing is saved while the boundary moves"
    );

    shell.cx.simulate_mouse_up(
        gpui::point(edge.x, edge.y - px(80.0)),
        MouseButton::Left,
        none,
    );
    shell.advance(Duration::from_secs(1));
    let saved = saved_inbox_size(&file).expect("the layout is saved once let go");
    let drawn = height(&open_column(&mut shell), Section::Inbox);
    let tab_bar = f32::from(start[1].1.top() - start[0].1.bottom());
    assert!(
        (saved - (drawn + tab_bar)).abs() < 1.0,
        "the saved inbox slot is the one drawn: {saved} vs {drawn} + {tab_bar}"
    );
}
