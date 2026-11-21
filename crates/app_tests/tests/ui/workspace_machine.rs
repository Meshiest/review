//! The Workspaces section's Machine field: a dropdown with a search box,
//! as wide as its field however many machines there are.
//!
//! The machines are invented hosts of a fixtures run; none is connected to.

use app::config::WorkspaceEntry;
use app::settings_panel::SettingsPanel;
use app::terminals::Terminals;
use app::workspaces::Workspaces;
use gpui::{Entity, TestAppContext, point, px};

use crate::harness::{Options, ShellWindow, open_shell};

/// Invented hosts listed after the fixtures' three machines, so option `n`
/// of the dropdown is `build-host-{n - 3}`.
const HOSTS: usize = 40;
const FIXTURE_MACHINES: usize = 3;

/// The settings open on one workspace on this PC, with [`HOSTS`] more
/// machines to pick from.
fn settings_with_many_machines(shell: &mut ShellWindow) -> Entity<SettingsPanel> {
    shell.update(|_, cx| {
        let terminals = Terminals::global(cx).expect("terminals are installed");
        terminals.update(cx, |terminals, _| {
            terminals.add_fixture_hosts((0..HOSTS).map(|n| format!("build-host-{n:02}")));
        });
        let workspaces = Workspaces::global(cx).expect("workspaces are installed");
        workspaces.update(cx, |workspaces, cx| {
            workspaces.loaded(
                vec![WorkspaceEntry {
                    host: Some("github.com".into()),
                    repo: "octo/widgets".into(),
                    machine: "local".into(),
                    path: "~/src/widgets".into(),
                    ..WorkspaceEntry::default()
                }],
                cx,
            );
        });
    });
    shell.keys("ctrl-,");
    let panel = shell
        .update(|_, cx| SettingsPanel::existing(cx))
        .expect("the settings are open");
    shell.update(|window, cx| panel.update(cx, |panel, cx| panel.reveal_workspaces(window, cx)));
    shell.settle();
    panel
}

fn machine_of(shell: &mut ShellWindow, panel: &Entity<SettingsPanel>) -> String {
    shell.update(|_, cx| panel.read(cx).workspace_entries(cx)[0].machine.clone())
}

fn option(n: usize) -> String {
    format!("workspace-0-machine-{n}")
}

#[gpui::test]
fn many_machines_fit_in_a_dropdown_that_searches(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    let panel = settings_with_many_machines(&mut shell);

    let control = shell.bounds_of("workspace-0-machine");
    let row = shell.bounds_of("workspace-row-0");
    assert!(
        control.left() >= row.left() && control.right() <= row.right(),
        "the control {control:?} fits in its row {row:?}"
    );
    assert!(
        f32::from(control.size.height) <= app::ui::CONTROL_HEIGHT,
        "one control tall: {control:?}"
    );
    assert!(!shell.drawn("workspace-0-machine-list"), "closed at first");

    shell.click("workspace-0-machine");
    assert!(shell.drawn("workspace-0-machine-list"));
    assert!(shell.drawn("workspace-0-machine-search"));
    let list = shell.bounds_of("workspace-0-machine-list");
    let last = shell.bounds_of(String::leak(option(FIXTURE_MACHINES + HOSTS - 1)));
    assert!(
        last.top() > list.bottom(),
        "the rows {last:?} scroll inside the list {list:?}"
    );
    let probe = String::leak(option(10));
    let before = shell.bounds_of(probe);
    shell.scroll_at(list.center(), 120.0);
    let after = shell.bounds_of(probe);
    assert!(
        after.top() < before.top(),
        "the wheel scrolls the rows: {before:?} then {after:?}"
    );

    // The search box has the keyboard; case does not matter.
    shell.type_text("HOST-1");
    let first = FIXTURE_MACHINES + 10;
    assert!(shell.drawn(&option(first)));
    assert!(shell.drawn(&option(first + 9)));
    assert!(!shell.drawn(&option(0)), "This PC does not match");
    assert!(
        !shell.drawn(&option(FIXTURE_MACHINES)),
        "build-host-00 does not"
    );
    assert!(!shell.drawn(&option(first + 10)), "build-host-20 does not");
    shell.keys("down enter");
    assert!(!shell.drawn("workspace-0-machine-list"), "a pick closes it");
    assert_eq!(machine_of(&mut shell, &panel), "build-host-11");

    // It opens again with the search cleared; a click on a row picks it.
    shell.click("workspace-0-machine");
    assert!(shell.drawn(&option(0)), "every machine is listed again");
    shell.type_text("buildbox");
    shell.click_named(&option(1));
    assert_eq!(machine_of(&mut shell, &panel), "buildbox");

    // Esc closes it and picks nothing.
    shell.click("workspace-0-machine");
    shell.keys("down");
    shell.keys("escape");
    assert!(!shell.drawn("workspace-0-machine-list"), "Esc closes it");
    assert_eq!(machine_of(&mut shell, &panel), "buildbox");

    // So does a click outside.
    shell.click("workspace-0-machine");
    assert!(shell.drawn("workspace-0-machine-list"));
    // On the field's label, left of the control.
    shell.click_at(point(control.left() - px(20.0), control.center().y));
    assert!(
        !shell.drawn("workspace-0-machine-list"),
        "a click outside closes it"
    );
    assert_eq!(machine_of(&mut shell, &panel), "buildbox");
}
