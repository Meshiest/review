//! `Ctrl+Tab` and `Ctrl+Shift+Tab` show the next and previous tab of the
//! focused group, wrapping, and focus follows, also from a terminal.

use app::panels::PanelKind;
use app::tab_menu;
use app::terminals::{TerminalSpec, Where};
use gpui::TestAppContext;
use gpui_kit::component::dock::PanelId;

use crate::harness::{Options, ShellWindow, open_shell};

/// The panel of pull request `number`'s diff tab.
fn diff_tab(shell: &mut ShellWindow, number: u64) -> PanelId {
    shell
        .panel_of(number, PanelKind::Diff)
        .unwrap_or_else(|| panic!("no diff tab for #{number}"))
}

/// The tabs of the group holding `panel`, left to right.
fn order_of(shell: &mut ShellWindow, panel: PanelId) -> Vec<PanelId> {
    let dock = shell.dock();
    shell.update(|_, cx| {
        tab_menu::place_of(dock.read(cx), panel)
            .expect("the panel is docked")
            .panels
    })
}

/// Moves focus into `panel`, which opening a tab from code does not.
fn focus(shell: &mut ShellWindow, panel: PanelId) {
    let dock = shell.dock();
    shell.update(|window, cx| {
        let view = dock.read(cx).panel(panel).expect("the panel is docked");
        window.focus(&view.focus_handle(cx), cx);
    });
    shell.settle();
}

/// Whether focus is inside `panel`.
fn has_focus(shell: &mut ShellWindow, panel: PanelId) -> bool {
    let dock = shell.dock();
    shell.update(|window, cx| {
        dock.read(cx)
            .panel(panel)
            .is_some_and(|view| view.focus_handle(cx).contains_focused(window, cx))
    })
}

/// Asserts `tabs[shown]` is the shown tab and holds focus.
fn assert_shown(shell: &mut ShellWindow, tabs: &[PanelId], shown: usize, after: &str) {
    for (ix, tab) in tabs.iter().enumerate() {
        assert_eq!(shell.is_shown(*tab), ix == shown, "{after}: tab {ix}");
    }
    assert!(has_focus(shell, tabs[shown]), "{after}: focus is in it");
}

#[gpui::test]
fn ctrl_tab_walks_the_tabs_and_wraps(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    for number in [41, 42, 43] {
        shell.open_pr(number);
    }
    assert_eq!(shell.groups().len(), 1, "one group: {:?}", shell.groups());
    let newest = diff_tab(&mut shell, 43);
    assert!(shell.is_shown(newest), "the last one opened is shown");
    let tabs: [PanelId; 3] = order_of(&mut shell, newest).try_into().expect("three tabs");
    let last = tabs.iter().position(|tab| *tab == newest).unwrap();
    focus(&mut shell, newest);

    // Three steps each way from wherever the tabs opened pass the end of the
    // row once, so each wrap is checked.
    let mut at = last;
    let mut wraps = [false; 2];
    for _ in 0..3 {
        shell.keys("ctrl-tab");
        at = (at + 1) % 3;
        wraps[0] |= at == 0;
        assert_shown(&mut shell, &tabs, at, "right");
    }
    for _ in 0..3 {
        shell.keys("ctrl-shift-tab");
        at = (at + 2) % 3;
        wraps[1] |= at == 2;
        assert_shown(&mut shell, &tabs, at, "left");
    }
    assert_eq!(wraps, [true; 2], "went round the end both ways");
    assert_eq!(at, last, "back where it started");
}

#[gpui::test]
fn ctrl_tab_works_while_a_terminal_has_focus(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    for number in [41, 42] {
        shell.open_pr(number);
    }
    let diffs = [41, 42].map(|number| diff_tab(&mut shell, number));
    let spec = TerminalSpec {
        machine: app::machines::THIS_PC.to_owned(),
        shell: Some(if cfg!(windows) { "cmd" } else { "sh" }.to_owned()),
        ..TerminalSpec::default()
    };
    let entity = shell.shell.clone();
    shell.update(|window, cx| {
        entity.update(cx, |shell, cx| {
            shell.add_terminal(spec, Where::Tab, window, cx);
        });
    });
    shell.settle();
    let terminal = shell.update(|_, cx| {
        let dock = entity.read(cx).dock().read(cx);
        dock.layout(gpui_kit::component::dock::DockPlacement::Center)
            .and_then(|tree| tree.panels().find(|id| !diffs.contains(id)))
            .expect("the terminal's tab")
    });
    let tabs = [diffs[0], diffs[1], terminal];
    assert_shown(&mut shell, &tabs, 2, "the terminal opens focused");

    shell.keys("ctrl-tab");
    assert_shown(&mut shell, &tabs, 0, "Ctrl+Tab leaves the terminal");
    shell.keys("ctrl-shift-tab");
    assert_shown(&mut shell, &tabs, 2, "Ctrl+Shift+Tab comes back to it");
    shell.keys("ctrl-shift-tab");
    assert_shown(&mut shell, &tabs, 1, "left of the terminal");
}
