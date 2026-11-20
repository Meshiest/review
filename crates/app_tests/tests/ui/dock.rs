//! A dock layout changed in the window is saved by the shell and restored,
//! by the next start, into a real `DockArea` with the same groups, shown
//! tabs and measured split sizes, and with the pull request each tab showed
//! open again.

use std::time::Duration;

use app::layout;
use app::panels::{CONVERSATION, DIFF, LOG, PanelKind};
use gpui::TestAppContext;
use gpui_kit::component::dock::{DockPlacement, InsertTarget};

use crate::harness::{Options, Scratch, StartLayout, open_shell};

/// The panel names under `state`, in layout order.
fn sections(state: &gpui_kit::component::dock::PanelState) -> Vec<String> {
    if state.children.is_empty() {
        return vec![state.panel_name.clone()];
    }
    state.children.iter().flat_map(sections).collect()
}

#[gpui::test]
fn a_changed_layout_is_restored_into_a_real_dock(
    first: &mut TestAppContext,
    next: &mut TestAppContext,
) {
    let scratch = Scratch::new("dock-restore");
    let file = scratch.join("layout.json");
    let mut shell = open_shell(
        first,
        Options {
            layout_file: Some(file.clone()),
            open_pr: Some(41),
            ..Options::default()
        },
    );
    let session = shell.session();
    shell.update(|_, cx| {
        session.update(cx, |session, cx| {
            session.show_panel(PanelKind::Conversation, cx);
            session.show_panel(PanelKind::Log, cx);
        });
    });
    shell.settle();
    assert_eq!(
        shell.groups(),
        [vec![DIFF, CONVERSATION], vec![LOG]],
        "the Conversation tab joins its pull request's group, the Log splits off"
    );

    // What dropping the Conversation tab onto the Log tab bar does.
    let (conversation, log) = (shell.panel_id(CONVERSATION), shell.panel_id(LOG));
    let dock = shell.dock();
    shell.update(|window, cx| {
        dock.update(cx, |dock, cx| {
            let node = dock
                .layout(DockPlacement::Center)
                .and_then(|tree| tree.find_panel_node(log))
                .expect("the Log group");
            let target = InsertTarget::Tabs {
                node,
                ix: None,
                activate: true,
            };
            dock.move_panel(conversation, target, window, cx);
        });
    });
    shell.settle();
    assert_eq!(shell.shown_tabs(), [DIFF, CONVERSATION]);

    shell.advance(Duration::from_secs(1));
    let saved = layout::load_from(&file).expect("the shell saved a layout startup can read");
    let live = shell.update(|_, cx| dock.read(cx).dump(cx));
    assert_eq!(saved, live, "the file is the dock as drawn");
    let log_group = &saved.center.children[1];
    let names: Vec<&str> = log_group
        .children
        .iter()
        .map(|panel| panel.panel_name.as_str())
        .collect();
    assert_eq!(names, [LOG, CONVERSATION]);

    let mut restored = open_shell(
        next,
        Options {
            layout: StartLayout::Saved(Box::new(saved.clone())),
            ..Options::default()
        },
    );
    assert_eq!(restored.shown_tabs(), [DIFF, CONVERSATION]);
    assert_eq!(
        restored.open_numbers(),
        [41],
        "the saved tabs opened their pull request again"
    );
    let dock = restored.dock();
    let again = restored.update(|_, cx| dock.read(cx).dump(cx));
    assert_eq!(
        again.center, saved.center,
        "groups, shown tabs and split sizes come back"
    );
    let (sidebar, sidebar_again) = (
        saved.left_dock.as_ref().expect("the sidebar is saved"),
        again.left_dock.as_ref().expect("and restored"),
    );
    assert_eq!(
        sections(sidebar.panel()),
        sections(sidebar_again.panel()),
        "the sidebar holds the same sections in the same order"
    );
    assert_eq!(sidebar.size(), sidebar_again.size());
    assert!(
        restored.panel_of(41, PanelKind::Conversation).is_some(),
        "the Conversation panel was rebuilt by the registry, bound to its pull request"
    );
}

/// Every panel name under `state`, containers included.
fn names(state: &gpui_kit::component::dock::PanelState, out: &mut Vec<String>) {
    out.push(state.panel_name.clone());
    for child in &state.children {
        names(child, out);
    }
}

#[gpui::test]
fn a_layout_saved_with_a_checks_tab_and_section_loads_without_them(
    first: &mut TestAppContext,
    next: &mut TestAppContext,
) {
    use gpui_kit::component::dock::{DockState, PanelInfo, PanelState};

    let scratch = Scratch::new("dock-old-checks");
    let file = scratch.join("layout.json");
    let mut shell = open_shell(
        first,
        Options {
            layout_file: Some(file.clone()),
            open_pr: Some(41),
            ..Options::default()
        },
    );
    shell.advance(Duration::from_secs(1));
    let dock = shell.dock();
    let mut old = shell.update(|_, cx| dock.read(cx).dump(cx));

    // What an older build saved: a Checks tab beside #41's Diff, shown, and
    // the sidebar's checks section ("Rollup") in a group of its own.
    let key = shell.key(41);
    let checks = PanelState {
        panel_name: "Checks".to_owned(),
        children: Vec::new(),
        info: PanelInfo::panel(serde_json::json!({ "repo": key.repo, "number": 41 })),
    };
    fn first_tabs(state: &mut PanelState) -> Option<&mut PanelState> {
        if let PanelInfo::Tabs { .. } = state.info {
            return Some(state);
        }
        state.children.iter_mut().find_map(first_tabs)
    }
    let group = first_tabs(&mut old.center).expect("a tab group");
    group.children.push(checks);
    group.info = PanelInfo::tabs(group.children.len() - 1);
    let left = old.left_dock.take().expect("a sidebar");
    let mut sections = left.panel().clone();
    sections.children.push(PanelState {
        panel_name: "TabPanel".to_owned(),
        children: vec![PanelState::new("Rollup")],
        info: PanelInfo::tabs(0),
    });
    if let PanelInfo::Stack { sizes, .. } = &mut sections.info {
        sizes.push(gpui::px(64.0));
    }
    old.left_dock = Some(DockState::new(
        sections,
        left.placement(),
        left.size(),
        left.open(),
    ));
    layout::write_to(&file, &old).expect("the old layout is written");

    let saved = layout::load_from(&file).expect("the old layout reads");
    let mut restored = open_shell(
        next,
        Options {
            layout: StartLayout::Saved(Box::new(saved)),
            ..Options::default()
        },
    );
    assert_eq!(
        restored.shown_tabs(),
        [DIFF],
        "the Diff took the Checks tab's place"
    );
    assert_eq!(restored.open_numbers(), [41]);
    let dock = restored.dock();
    let state = restored.update(|_, cx| dock.read(cx).dump(cx));
    let mut all = Vec::new();
    names(&state.center, &mut all);
    if let Some(left) = &state.left_dock {
        names(left.panel(), &mut all);
    }
    for gone in ["Checks", "Rollup", "InvalidPanel"] {
        assert!(!all.iter().any(|name| name == gone), "{gone} in {all:?}");
    }
    assert!(
        !restored.drawn("section-Rollup"),
        "no checks section in the sidebar"
    );
    assert!(restored.drawn("section-Inbox"), "the sidebar came back");
}
