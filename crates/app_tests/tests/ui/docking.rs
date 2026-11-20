//! The one dock: the sidebar's sections in its left region, the pull
//! requests' tabs in its center. Files drags into the main area beside the
//! tabs and back, and the saved layout keeps it wherever it is; a layout
//! from before, with its sidebar in a file of its own, still loads. A tab
//! group fills the dock and gives it back (the panel menu's Zoom in and
//! Zoom out, `Shift+Esc`, `Esc`), and a middle click closes a tab while one
//! inside a panel still scrolls.

use std::time::Duration;

use app::layout;
use app::panels::{DIFF, PanelKind, tab_selector};
use app::session::OpenMode;
use app::sidebar_panels::{Section, SectionPanel};
use app::tab_menu;
use gpui::{
    Axis, Entity, Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, Pixels, Point,
    SharedString, TestAppContext, px,
};
use gpui_kit::component::dock::{DockAreaState, DockPlacement, PanelId, PanelInfo, PanelState};

use crate::harness::{Options, Scratch, ShellWindow, StartLayout, open_shell};

/// The dock region holding `panel`.
fn region_of(shell: &mut ShellWindow, panel: PanelId) -> Option<DockPlacement> {
    let dock = shell.dock();
    shell.update(|_, cx| {
        let dock = dock.read(cx);
        [DockPlacement::Center, DockPlacement::Left]
            .into_iter()
            .find(|region| {
                dock.layout(*region)
                    .is_some_and(|tree| tree.find_panel_node(panel).is_some())
            })
    })
}

fn section_panel(shell: &mut ShellWindow, section: Section) -> Entity<SectionPanel> {
    let id = shell.section(section);
    let dock = shell.dock();
    shell.update(|_, cx| {
        dock.read(cx)
            .panel(id)
            .and_then(|panel| panel.view().downcast::<SectionPanel>().ok())
            .expect("a section panel")
    })
}

/// The middle of `panel`'s tab.
fn tab_center(shell: &mut ShellWindow, panel: PanelId) -> Point<Pixels> {
    shell
        .bounds_of(String::leak(tab_menu::tab_selector(panel)))
        .center()
}

/// Whether the saved panel tree `state` holds a panel named `name`.
fn holds(state: &PanelState, name: &str) -> bool {
    state.panel_name == name || state.children.iter().any(|child| holds(child, name))
}

#[gpui::test]
fn files_drags_into_the_main_area_and_back_and_the_layout_keeps_it(
    first: &mut TestAppContext,
    next: &mut TestAppContext,
) {
    let scratch = Scratch::new("files-in-main");
    let file = scratch.join("layout.json");
    let mut shell = open_shell(
        first,
        Options {
            layout_file: Some(file.clone()),
            open_pr: Some(41),
            ..Options::default()
        },
    );
    let files = shell.section(Section::Files);
    assert_eq!(region_of(&mut shell, files), Some(DockPlacement::Left));

    // Its tab onto the Diff tab: it joins the Diff's group.
    let from = tab_center(&mut shell, files);
    let key = shell.key(41);
    let to = shell
        .bounds_of(String::leak(tab_selector(&key, PanelKind::Diff)))
        .center();
    shell.drag(from, to);
    assert_eq!(
        region_of(&mut shell, files),
        Some(DockPlacement::Center),
        "dropped into the main area"
    );
    assert!(
        shell.groups().iter().any(|group| {
            group.contains(&DIFF.to_owned()) && group.contains(&"Files".to_owned())
        }),
        "beside the Diff tab: {:?}",
        shell.groups()
    );
    let panel = section_panel(&mut shell, Section::Files);
    assert!(
        shell.update(|_, cx| !panel.read(cx).in_sidebar()),
        "it knows it left the sidebar"
    );
    shell.update(|window, cx| {
        let dock = shell_dock(cx);
        dock.update(cx, |dock, cx| dock.select_panel(files, window, cx));
    });
    shell.settle();
    assert!(shell.drawn("section-Files"), "and draws in the main area");

    shell.advance(Duration::from_secs(1));
    let saved = layout::load_from(&file).expect("the shell saved the layout");
    assert!(holds(&saved.center, "Files"), "saved in the main area");
    let sidebar = saved.left_dock.as_ref().expect("with the sidebar");
    assert!(!holds(sidebar.panel(), "Files"));

    let mut restored = open_shell(
        next,
        Options {
            layout: StartLayout::Saved(Box::new(saved)),
            ..Options::default()
        },
    );
    let files = restored.section(Section::Files);
    assert_eq!(
        region_of(&mut restored, files),
        Some(DockPlacement::Center),
        "restored where it was"
    );
    let all: Vec<PanelId> = Section::ALL
        .iter()
        .map(|&section| restored.section(section))
        .collect();
    assert_eq!(all.len(), 5, "one of each section, none added twice");

    // And back: onto the Inbox's tab.
    let inbox = restored.section(Section::Inbox);
    let from = tab_center(&mut restored, files);
    let to = tab_center(&mut restored, inbox);
    restored.drag(from, to);
    assert_eq!(
        region_of(&mut restored, files),
        Some(DockPlacement::Left),
        "back in the sidebar"
    );
}

/// The dock of the window's shell.
fn shell_dock(cx: &mut gpui::App) -> Entity<gpui_kit::component::dock::DockArea> {
    app::shell::Shell::global(cx)
        .expect("the shell")
        .read(cx)
        .dock()
        .clone()
}

fn tabs(panels: &[(&str, serde_json::Value)]) -> PanelState {
    PanelState {
        panel_name: "TabPanel".to_owned(),
        children: panels
            .iter()
            .map(|(name, info)| PanelState {
                panel_name: (*name).to_owned(),
                children: Vec::new(),
                info: PanelInfo::panel(info.clone()),
            })
            .collect(),
        info: PanelInfo::tabs(0),
    }
}

#[gpui::test]
fn an_older_layout_with_a_separate_sidebar_still_loads(cx: &mut TestAppContext) {
    let scratch = Scratch::new("older-layout");
    let file = scratch.join("layout.json");
    let diff = serde_json::json!({
        "repo": { "host": "forgejo.example.com", "owner": "cora", "name": "review" },
        "number": 41,
    });
    let main = DockAreaState {
        version: Some(layout::VERSION),
        center: PanelState {
            panel_name: "StackPanel".to_owned(),
            children: vec![tabs(&[(DIFF, diff)])],
            info: PanelInfo::stack(vec![px(1100.0)], Axis::Horizontal),
        },
        ..DockAreaState::default()
    };
    let folded = serde_json::json!({ "collapsed": true, "restore": 300.0 });
    let open = serde_json::json!({ "collapsed": false });
    let sidebar = DockAreaState {
        version: Some(layout::SIDEBAR_VERSION),
        center: PanelState {
            panel_name: "StackPanel".to_owned(),
            children: vec![
                tabs(&[("Inbox", open.clone())]),
                tabs(&[("Stack", open.clone())]),
                tabs(&[("Rollup", open.clone())]),
                tabs(&[("Threads", folded)]),
                tabs(&[("Files", open)]),
            ],
            info: PanelInfo::stack(
                [400.0, 100.0, 64.0, 100.0, 200.0].map(px).to_vec(),
                Axis::Vertical,
            ),
        },
        ..DockAreaState::default()
    };
    layout::write_to(&file, &main).expect("the main layout is written");
    layout::write_to(&layout::sidebar_path(&file), &sidebar).expect("and the sidebar");

    let state = layout::load_from(&file).expect("the older layout loads");
    let left = state
        .left_dock
        .as_ref()
        .expect("its sidebar read into the left");
    assert!(holds(left.panel(), "Files") && !holds(left.panel(), "Repos"));
    let mut shell = open_shell(
        cx,
        Options {
            layout: StartLayout::Saved(Box::new(state)),
            ..Options::default()
        },
    );
    assert_eq!(
        shell.open_numbers(),
        [41],
        "its tab opened its pull request"
    );
    for section in Section::ALL {
        let id = shell.section(section);
        assert_eq!(
            region_of(&mut shell, id),
            Some(DockPlacement::Left),
            "{section:?} in the sidebar"
        );
    }
    let threads = section_panel(&mut shell, Section::Threads);
    assert!(
        shell.update(|_, cx| threads.read(cx).is_collapsed()),
        "the fold it had"
    );
    let repos = section_panel(&mut shell, Section::Repos);
    assert!(
        shell.update(|_, cx| repos.read(cx).is_collapsed()),
        "Repos, newer than the layout, joins folded"
    );
    assert!(shell.drawn("section-toggle-Repos"));
}

fn zoomed(shell: &mut ShellWindow) -> bool {
    let dock = shell.dock();
    shell.update(|_, cx| dock.read(cx).is_zoomed())
}

#[gpui::test]
fn a_tab_group_zooms_from_its_menu_and_the_keys_and_esc_puts_it_back(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    shell.open_pr(41);
    assert!(shell.drawn("section-Inbox"));

    let key = shell.key(41);
    let diff_tab = String::leak(tab_selector(&key, PanelKind::Diff));

    // The keys reach the inbox: its group fills the window.
    shell.keys("shift-escape");
    assert!(zoomed(&mut shell), "Shift+Esc zooms the group with focus");
    assert!(shell.drawn("section-Inbox"));
    assert!(!shell.drawn(diff_tab), "over everything else");
    shell.keys("escape");
    assert!(!zoomed(&mut shell), "Esc puts it back");
    assert!(shell.drawn(diff_tab));

    // With the diff focused, the Diff's group.
    let (line, _) = shell.drawn_lines()[1];
    shell.click_at(line);
    shell.keys("shift-escape");
    assert!(zoomed(&mut shell));
    assert!(
        !shell.drawn("section-Inbox"),
        "the Diff's group fills the dock, over the sidebar"
    );
    let diff = shell.panel_id(DIFF);
    assert!(shell.is_shown(diff));
    shell.keys("escape");
    assert!(!zoomed(&mut shell), "Esc puts it back");
    assert!(shell.drawn("section-Inbox"));

    // The panel menu of the Diff's group: its ellipsis, then Zoom in.
    let menu = diff_menu_button(&mut shell);
    shell.click_at(menu);
    assert!(
        shell.element(SharedString::from("popup-menu")).is_some(),
        "the panel menu is open"
    );
    shell.keys("down enter");
    assert!(zoomed(&mut shell), "Zoom in fills the dock");
    let dock = shell.dock();
    let group = shell.update(|_, cx| dock.read(cx).zoomed_group());
    let diff_group = shell.update(|_, cx| {
        dock.read(cx)
            .layout(DockPlacement::Center)
            .and_then(|tree| tree.find_panel_node(diff))
    });
    assert_eq!(group, diff_group, "the menu's own group");

    let menu = diff_menu_button(&mut shell);
    shell.click_at(menu);
    shell.keys("down enter");
    assert!(!zoomed(&mut shell), "Zoom out gives the dock back");

    shell.keys("shift-escape");
    assert!(zoomed(&mut shell));
    shell.keys("shift-escape");
    assert!(!zoomed(&mut shell), "the shortcut toggles");

    // The menu's other entry, Close, closes the shown tab (a lone tab has
    // only its own close button).
    shell.show(41, PanelKind::Conversation, OpenMode::Permanent);
    let conversation = shell.panel_of(41, PanelKind::Conversation).expect("open");
    let menu = diff_menu_button(&mut shell);
    shell.click_at(menu);
    shell.keys("down down enter");
    assert!(
        shell.dock_lacks(conversation),
        "Close closed the Conversation tab"
    );
    assert!(!shell.dock_lacks(diff));
}

#[gpui::test]
fn ctrl_b_hides_and_shows_the_sidebar(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    let open = |shell: &mut ShellWindow| shell.read_shell(|shell, cx| shell.is_sidebar_open(cx));
    assert!(open(&mut shell));
    shell.keys("ctrl-b");
    assert!(!open(&mut shell));
    assert!(!shell.drawn("section-Inbox"));
    shell.keys("ctrl-b");
    assert!(open(&mut shell));
    assert!(shell.drawn("section-Inbox"));
}

/// The ellipsis of the tab bar the Diff tab is in: right of its tabs, at
/// the right edge of the main area.
fn diff_menu_button(shell: &mut ShellWindow) -> Point<Pixels> {
    let key = shell.key(41);
    let tab = shell.bounds_of(String::leak(tab_selector(&key, PanelKind::Diff)));
    let window = shell.update(|window, _| window.bounds().size);
    gpui::point(window.width - px(18.0), tab.center().y)
}

fn middle_click(shell: &mut ShellWindow, position: Point<Pixels>) {
    let none = Modifiers::none();
    shell.cx.simulate_mouse_move(position, None, none);
    shell.cx.simulate_event(MouseDownEvent {
        button: MouseButton::Middle,
        position,
        modifiers: none,
        click_count: 1,
        first_mouse: false,
    });
    shell.cx.simulate_event(MouseUpEvent {
        button: MouseButton::Middle,
        position,
        modifiers: none,
        click_count: 1,
    });
    shell.settle();
}

#[gpui::test]
fn a_middle_click_closes_a_tab_and_still_scrolls_inside_a_panel(cx: &mut TestAppContext) {
    let mut shell = open_shell(
        cx,
        Options {
            open_pr: Some(41),
            diff_rows: Some(4000),
            ..Options::default()
        },
    );

    // Inside the diff: autoscroll, and the tab stays.
    let (line, _) = shell.drawn_lines()[2];
    middle_click(&mut shell, line);
    let view = shell.diff_view();
    assert!(
        shell.update(|_, cx| view.read(cx).is_autoscrolling()),
        "a middle click in the diff starts autoscroll"
    );
    shell.keys("escape");
    assert!(shell.panel_of(41, PanelKind::Diff).is_some());

    // On a tab: it closes, as its close button closes it.
    shell.show(41, PanelKind::Conversation, OpenMode::Permanent);
    let conversation = shell.panel_of(41, PanelKind::Conversation).expect("open");
    let key = shell.key(41);
    let tab = shell
        .bounds_of(String::leak(tab_selector(&key, PanelKind::Conversation)))
        .center();
    middle_click(&mut shell, tab);
    assert!(
        shell.dock_lacks(conversation),
        "the Conversation tab closed"
    );
    assert!(shell.panel_of(41, PanelKind::Diff).is_some(), "only it");

    // A section does not close.
    let files = shell.section(Section::Files);
    let at = tab_center(&mut shell, files);
    middle_click(&mut shell, at);
    assert!(!shell.dock_lacks(files));
}
