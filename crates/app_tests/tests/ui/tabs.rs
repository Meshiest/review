//! A dock tab's right-click menu: the pull request's reference, link,
//! branch and title to the clipboard; the close entries; Keep open on a
//! preview; Split right and down; moving a tab between the sidebar and the
//! main area; Open on forge. A tab that is not a pull request's gets only
//! the close and placement entries.

use app::panels::{CONVERSATION, DIFF, PanelKind, tab_selector};
use app::session::OpenMode;
use app::sidebar_panels::Section;
use app::tab_menu::{self, TabAction};
use gpui::{Modifiers, MouseButton, SharedString, TestAppContext};
use gpui_kit::component::dock::{DockPlacement, PanelId};

use crate::harness::{Options, ShellWindow, open_shell};

fn run(shell: &mut ShellWindow, action: TabAction, panel: PanelId) {
    shell.update(|window, cx| tab_menu::run(action, panel, window, cx));
    shell.settle();
}

fn clipboard(shell: &mut ShellWindow) -> Option<String> {
    shell.cx.read_from_clipboard().and_then(|item| item.text())
}

fn actions(shell: &mut ShellWindow, panel: PanelId) -> Vec<TabAction> {
    shell.update(|_, cx| {
        tab_menu::entries(panel, cx)
            .into_iter()
            .flatten()
            .map(|entry| entry.action)
            .collect()
    })
}

fn region_of(shell: &mut ShellWindow, panel: PanelId) -> Option<DockPlacement> {
    let dock = shell.dock();
    shell.update(|_, cx| tab_menu::place_of(dock.read(cx), panel).map(|place| place.region))
}

/// #41's Diff and Conversation and #42's Diff tabbed in one group, #41
/// focused.
fn three_tabs(cx: &mut TestAppContext) -> ShellWindow {
    let mut shell = open_shell(
        cx,
        Options {
            open_pr: Some(41),
            ..Options::default()
        },
    );
    shell.show(41, PanelKind::Conversation, OpenMode::Permanent);
    shell.show(42, PanelKind::Diff, OpenMode::Permanent);
    assert_eq!(shell.groups(), [vec![DIFF, CONVERSATION, DIFF]]);
    shell.show(41, PanelKind::Conversation, OpenMode::Permanent);
    shell
}

/// #42's Diff, the tab right of #41's.
fn other_diff(shell: &mut ShellWindow) -> PanelId {
    shell.panel_of(42, PanelKind::Diff).expect("#42's Diff")
}

#[gpui::test]
fn the_copy_entries_put_the_pull_requests_text_on_the_clipboard(cx: &mut TestAppContext) {
    let mut shell = three_tabs(cx);
    let diff = shell.panel_id(DIFF);
    let session = shell.session();
    let key = shell.key(41);
    let (title, head) = shell.update(|_, cx| {
        let pr = &session.read(cx).pr(&key).expect("open").pr;
        (pr.title.to_string(), pr.head.to_string())
    });

    run(&mut shell, TabAction::CopyReference, diff);
    assert_eq!(clipboard(&mut shell).as_deref(), Some("cora/review#41"));
    run(&mut shell, TabAction::CopyLink, diff);
    let link = "https://forgejo.example.com/cora/review/pulls/41";
    assert_eq!(clipboard(&mut shell).as_deref(), Some(link));
    run(&mut shell, TabAction::CopyBranch, diff);
    assert_eq!(clipboard(&mut shell), Some(head));
    run(&mut shell, TabAction::CopyTitle, diff);
    assert_eq!(
        clipboard(&mut shell),
        Some(format!("{title} (cora/review#41)"))
    );
    run(&mut shell, TabAction::OpenOnForge, diff);
    assert_eq!(shell.cx.opened_url().as_deref(), Some(link));

    // GitLab writes merge requests with `!`.
    shell.open_pr(412);
    let gitlab = shell.panel_of(412, PanelKind::Diff).expect("open");
    run(&mut shell, TabAction::CopyReference, gitlab);
    assert_eq!(clipboard(&mut shell).as_deref(), Some("acme/web!412"));
}

#[gpui::test]
fn a_right_click_opens_the_menu_and_its_entries_act(cx: &mut TestAppContext) {
    let mut shell = three_tabs(cx);
    let key = shell.key(41);
    let tab = shell
        .bounds_of(String::leak(tab_selector(&key, PanelKind::Diff)))
        .center();
    let none = Modifiers::none();
    shell.cx.simulate_mouse_move(tab, None, none);
    shell.cx.simulate_mouse_down(tab, MouseButton::Right, none);
    shell.cx.simulate_mouse_up(tab, MouseButton::Right, none);
    shell.settle();
    assert!(
        shell.element(SharedString::from("popup-menu")).is_some(),
        "the tab's menu is open"
    );
    shell.keys("down enter");
    assert_eq!(
        clipboard(&mut shell).as_deref(),
        Some("cora/review#41"),
        "its first entry copied the reference"
    );
}

#[gpui::test]
fn the_close_entries_close_what_they_say(cx: &mut TestAppContext) {
    let mut shell = three_tabs(cx);
    let (diff, conversation, other) = (
        shell.panel_id(DIFF),
        shell.panel_id(CONVERSATION),
        other_diff(&mut shell),
    );
    run(&mut shell, TabAction::CloseToRight, conversation);
    assert!(shell.dock_lacks(other), "#42's Diff was right of it");
    assert!(!shell.dock_lacks(diff) && !shell.dock_lacks(conversation));

    shell.show(42, PanelKind::Diff, OpenMode::Permanent);
    shell.show(41, PanelKind::Conversation, OpenMode::Permanent);
    run(&mut shell, TabAction::CloseOthers, conversation);
    assert_eq!(shell.groups(), [vec![CONVERSATION]]);

    shell.show(41, PanelKind::Diff, OpenMode::Permanent);
    let diff = shell.panel_id(DIFF);
    run(&mut shell, TabAction::Close, diff);
    assert!(shell.dock_lacks(diff), "Close closes that tab");
    assert_eq!(
        shell.open_numbers(),
        [41],
        "its pull request still has a tab"
    );

    shell.show(41, PanelKind::Diff, OpenMode::Permanent);
    let diff = shell.panel_id(DIFF);
    run(&mut shell, TabAction::CloseAll, diff);
    assert!(shell.groups().iter().all(Vec::is_empty) || shell.groups().is_empty());
    assert!(
        shell.open_numbers().is_empty(),
        "nothing of it is left open"
    );

    // Another pull request's tabs stay when this one's close.
    shell.open_pr(41);
    shell.show(41, PanelKind::Conversation, OpenMode::Permanent);
    shell.open_pr(42);
    let diff = shell.panel_of(41, PanelKind::Diff).expect("open");
    run(&mut shell, TabAction::ClosePr, diff);
    assert_eq!(shell.open_numbers(), [42]);
    assert!(shell.panel_of(41, PanelKind::Conversation).is_none());
}

#[gpui::test]
fn keep_open_makes_the_preview_permanent(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    shell.show(42, PanelKind::Conversation, OpenMode::Preview);
    let tab = shell.panel_of(42, PanelKind::Conversation).expect("open");
    let key = shell.key(42);
    let session = shell.session();
    assert!(shell.update(|_, cx| session.read(cx).is_preview(&key)));
    assert!(actions(&mut shell, tab).contains(&TabAction::KeepOpen));
    run(&mut shell, TabAction::KeepOpen, tab);
    assert!(!shell.update(|_, cx| session.read(cx).is_preview(&key)));
    assert!(
        !actions(&mut shell, tab).contains(&TabAction::KeepOpen),
        "a permanent tab has nothing to keep"
    );
}

#[gpui::test]
fn the_placement_entries_split_and_move_tabs(cx: &mut TestAppContext) {
    let mut shell = three_tabs(cx);
    let (conversation, other) = (shell.panel_id(CONVERSATION), other_diff(&mut shell));
    run(&mut shell, TabAction::SplitRight, other);
    assert_eq!(shell.groups(), [vec![DIFF, CONVERSATION], vec![DIFF]]);
    let dock = shell.dock();
    let axis = shell.update(|_, cx| dock.read(cx).dump(cx).center.info.axis());
    assert_eq!(axis, Some(gpui::Axis::Horizontal), "beside it");

    run(&mut shell, TabAction::SplitDown, conversation);
    assert_eq!(shell.groups().len(), 3);
    assert!(shell.is_shown(conversation));

    // A pull request's tab into the sidebar and back.
    let diff = shell.panel_id(DIFF);
    run(&mut shell, TabAction::MoveToOtherArea, diff);
    assert_eq!(region_of(&mut shell, diff), Some(DockPlacement::Left));
    run(&mut shell, TabAction::MoveToOtherArea, diff);
    assert_eq!(region_of(&mut shell, diff), Some(DockPlacement::Center));

    // Files into the main area, beside the pull request's tabs, and back.
    let files = shell.section(Section::Files);
    run(&mut shell, TabAction::MoveToOtherArea, files);
    assert_eq!(region_of(&mut shell, files), Some(DockPlacement::Center));
    assert!(shell.drawn("section-Files"), "drawn there");
    run(&mut shell, TabAction::MoveToOtherArea, files);
    assert_eq!(region_of(&mut shell, files), Some(DockPlacement::Left));
}

#[gpui::test]
fn a_tab_that_is_not_a_pull_requests_gets_the_close_and_placement_entries(cx: &mut TestAppContext) {
    let mut shell = three_tabs(cx);
    shell.keys("ctrl-,");
    let settings = shell.update(|_, cx| {
        app::settings_panel::SettingsPanel::existing(cx)
            .map(|panel| PanelId::from(panel.entity_id()))
            .expect("the settings are open")
    });
    let only_pr = [
        TabAction::CopyReference,
        TabAction::CopyLink,
        TabAction::CopyBranch,
        TabAction::CopyTitle,
        TabAction::ClosePr,
        TabAction::KeepOpen,
        TabAction::OpenOnForge,
    ];
    let listed = actions(&mut shell, settings);
    assert!(listed.contains(&TabAction::Close));
    assert!(listed.contains(&TabAction::MoveToOtherArea));
    assert!(
        listed.iter().all(|action| !only_pr.contains(action)),
        "{listed:?}"
    );

    let files = shell.section(Section::Files);
    let listed = actions(&mut shell, files);
    assert_eq!(
        listed,
        [
            TabAction::SplitRight,
            TabAction::SplitDown,
            TabAction::MoveToOtherArea
        ],
        "a section does not close: placement only"
    );

    let diff = shell.panel_id(DIFF);
    let listed = actions(&mut shell, diff);
    assert!(only_pr[..5].iter().all(|action| listed.contains(action)));
}
