//! A dock layout changed in the window is saved by the shell and restored,
//! by the next start, into a real `DockArea` with the same groups, shown
//! tabs and measured split sizes, and with the pull request each tab showed
//! open again.

use std::time::Duration;

use app::layout;
use app::panels::{CHECKS, DIFF, LOG, PanelKind};
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
            session.show_panel(PanelKind::Checks, cx);
            session.show_panel(PanelKind::Log, cx);
        });
    });
    shell.settle();
    assert_eq!(
        shell.groups(),
        [vec![DIFF, CHECKS], vec![LOG]],
        "the Checks tab joins its pull request's group, the Log splits off"
    );

    // What dropping the Checks tab onto the Log tab bar does.
    let (checks, log) = (shell.panel_id(CHECKS), shell.panel_id(LOG));
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
            dock.move_panel(checks, target, window, cx);
        });
    });
    shell.settle();
    assert_eq!(shell.shown_tabs(), [DIFF, CHECKS]);

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
    assert_eq!(names, [LOG, CHECKS]);

    let mut restored = open_shell(
        next,
        Options {
            layout: StartLayout::Saved(Box::new(saved.clone())),
            ..Options::default()
        },
    );
    assert_eq!(restored.shown_tabs(), [DIFF, CHECKS]);
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
        restored.panel_of(41, PanelKind::Checks).is_some(),
        "the Checks panel was rebuilt by the registry, bound to its pull request"
    );
}
