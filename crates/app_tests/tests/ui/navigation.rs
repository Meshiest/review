//! Opening and navigating pull requests: several open at once, each with
//! its own panels; the preview tab a single click opens and the next one
//! reuses; the second click that opens the diff; opens that bring their tab
//! forward over Settings; and closing tabs, Settings included.

use app::appearance::{self, Appearance};
use app::model::PrKey;
use app::panels::{DiffPanel, LogPanel, PanelKind, close_selector, tab_selector};
use app::session::OpenMode;
use app::settings_panel::SettingsPanel;
use forge::model::CiState;
use gpui::{Modifiers, TestAppContext};
use gpui_kit::component::dock::PanelId;
use store::DraftAnchor;
use theme::ModeSetting;

use crate::harness::{Options, ShellWindow, open_shell};

/// Where a click on an inbox row lands: its title, clear of the links in
/// its second line.
fn row(number: u64) -> String {
    format!("inbox-title-{number}")
}

fn tab(shell: &mut ShellWindow, number: u64, kind: PanelKind) -> String {
    let key = shell.key(number);
    tab_selector(&key, kind)
}

fn is_preview(shell: &mut ShellWindow, number: u64) -> bool {
    let key = shell.key(number);
    let session = shell.session();
    shell.update(|_, cx| session.read(cx).is_preview(&key))
}

fn shown(shell: &mut ShellWindow, number: u64, kind: PanelKind) -> bool {
    let id = shell
        .panel_of(number, kind)
        .unwrap_or_else(|| panic!("#{number} has no {kind:?} panel"));
    shell.is_shown(id)
}

fn settings_id(shell: &mut ShellWindow) -> Option<PanelId> {
    let dock = shell.dock();
    shell.update(|_, cx| {
        let panel = SettingsPanel::existing(cx)?;
        let id = PanelId::from(panel.entity_id());
        dock.read(cx).panel(id).is_some().then_some(id)
    })
}

#[gpui::test]
fn pull_requests_open_side_by_side_with_their_own_panels(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    shell.open_pr(41);
    shell.open_pr(42);
    assert_eq!(
        shell.open_numbers(),
        [41, 42],
        "the second open adds a pull request"
    );
    assert_eq!(shell.focused(), Some(42));
    let (first, second) = (
        shell.panel_of(41, PanelKind::Diff).expect("#41 has a diff"),
        shell.panel_of(42, PanelKind::Diff).expect("#42 has a diff"),
    );
    assert_ne!(first, second, "each pull request has its own Diff panel");
    let dock = shell.dock();
    let drawn: Vec<Option<PrKey>> = shell.update(|_, cx| {
        [first, second]
            .iter()
            .map(|id| {
                let panel = dock
                    .read(cx)
                    .panel(*id)?
                    .view()
                    .downcast::<DiffPanel>()
                    .ok()?;
                panel.read(cx).pr(cx)
            })
            .collect()
    });
    assert_eq!(drawn, [Some(shell.key(41)), Some(shell.key(42))]);
    for number in [41, 42] {
        let title = tab(&mut shell, number, PanelKind::Diff);
        assert!(
            shell.drawn(&title),
            "the tab {title} names its pull request"
        );
    }
    let key = shell.key(41);
    assert_eq!(tab_selector(&key, PanelKind::Diff), "tab-review-41-Diff");

    // Beside: a new tab group to the right.
    shell.show(1288, PanelKind::Diff, OpenMode::Beside);
    assert_eq!(
        shell.groups().len(),
        2,
        "Ctrl+Enter opens in a group of its own"
    );

    // Closing a pull request's last panel, by its tab's close button,
    // releases what was loaded for it.
    shell.click_named(&close_selector(Some(&key), PanelKind::Diff));
    assert!(shell.dock_lacks(first));
    assert_eq!(shell.open_numbers(), [42, 1288]);
    let session = shell.session();
    let gone = shell.key(41);
    assert!(shell.update(|_, cx| session.read(cx).writing_for(&gone).is_none()));
}

#[gpui::test]
fn the_log_follows_the_focused_pull_request(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    shell.open_pr(42);
    shell.open_pr(1288);
    let session = shell.session();
    let job = shell.update(|_, cx| {
        let open = session.read(cx).open_pr().expect("#1288 is focused");
        open.ci
            .runs
            .iter()
            .flat_map(|run| &run.jobs)
            .find(|job| job.state == CiState::Fail && job.started_at.is_some())
            .map(|job| job.id.clone())
            .expect("#1288 has a failed job")
    });
    shell.update(|_, cx| session.update(cx, |session, cx| session.open_log(&job, None, cx)));
    shell.settle();
    let log: gpui::Entity<LogPanel> = shell.log_panel();
    let has_log = |shell: &mut ShellWindow| shell.update(|_, cx| log.read(cx).feed().is_some());
    assert!(has_log(&mut shell), "the Log shows #1288's job");

    let other = shell.key(42);
    shell.update(|_, cx| session.update(cx, |session, cx| session.focus(&other, cx)));
    shell.settle();
    assert!(!has_log(&mut shell), "#42 has no job picked");

    let back = shell.key(1288);
    shell.update(|_, cx| session.update(cx, |session, cx| session.focus(&back, cx)));
    shell.settle();
    assert!(has_log(&mut shell), "#1288's job comes back with its focus");
}

#[gpui::test]
fn a_preview_tab_is_reused_until_it_is_kept(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    // Folded, so "Review requested" keeps room for its rows once a pull
    // request is open.
    shell.click("inbox-section-1");
    shell.click("inbox-section-2");

    shell.click_named(&row(42));
    assert_eq!(shell.focused(), Some(42));
    assert!(is_preview(&mut shell, 42), "a single click opens a preview");
    assert!(shown(&mut shell, 42, PanelKind::Conversation));
    assert_eq!(shell.panel_of(42, PanelKind::Diff), None, "the diff waits");

    shell.click_named(&row(1288));
    assert_eq!(shell.open_numbers(), [1288], "the preview was reused");
    assert!(is_preview(&mut shell, 1288));
    assert_eq!(shell.groups(), [vec!["Conversation"]]);

    shell.click_named(&row(1288));
    assert!(!is_preview(&mut shell, 1288), "a second click keeps it");
    assert!(
        shown(&mut shell, 1288, PanelKind::Diff),
        "and opens its diff"
    );

    shell.click_named(&row(412));
    assert_eq!(shell.open_numbers(), [1288, 412], "a kept tab stays");
    assert!(is_preview(&mut shell, 412));
    let title = tab(&mut shell, 412, PanelKind::Conversation);
    shell.double_click(&title);
    assert!(
        !is_preview(&mut shell, 412),
        "a double click on the tab keeps it"
    );

    shell.click("inbox-section-2");
    shell.click_named(&row(38));
    assert!(is_preview(&mut shell, 38));
    shell.click("keep-tab");
    assert!(!is_preview(&mut shell, 38), "the pin keeps it");

    shell.click("inbox-section-2");
    shell.click("inbox-section-1");
    shell.click_named(&row(39));
    assert!(is_preview(&mut shell, 39));
    let writing = shell.writing();
    shell.update(|_, cx| {
        writing.update(cx, |writing, cx| {
            writing.open(DraftAnchor::Conversation, None, cx)
        });
    });
    shell.settle();
    assert!(!is_preview(&mut shell, 39), "starting a comment keeps it");
    assert_eq!(shell.open_numbers(), [1288, 412, 38, 39]);
}

#[gpui::test]
fn opening_brings_the_tab_forward_over_settings(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    // Folded, so "Review requested" keeps room for its rows once a pull
    // request is open.
    shell.click("inbox-section-1");
    shell.keys("ctrl-,");
    let settings = settings_id(&mut shell).expect("Settings opened");
    assert!(shell.is_shown(settings));

    shell.click_named(&row(42));
    assert!(
        shown(&mut shell, 42, PanelKind::Conversation),
        "the conversation came forward over Settings"
    );
    assert!(!shell.is_shown(settings));

    // The click left the keys with the inbox: Enter opens the diff.
    shell.keys("enter");
    assert!(
        shown(&mut shell, 42, PanelKind::Diff),
        "Enter on the selected row opens its diff in front"
    );

    shell.keys("ctrl-,");
    assert!(shell.is_shown(settings));
    shell.click_named(&row(42));
    assert!(
        shown(&mut shell, 42, PanelKind::Diff),
        "a click on an open pull request brings its tab back"
    );
    assert!(!shell.is_shown(settings));

    shell.keys("ctrl-,");
    shell.click_named(&row(1288));
    assert!(shown(&mut shell, 1288, PanelKind::Conversation));
    assert!(!shell.is_shown(settings));
}

#[gpui::test]
fn settings_close_and_come_back_with_nothing_lost(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    shell.keys("ctrl-,");
    let settings = settings_id(&mut shell).expect("Settings opened");
    shell.update(|_, cx| appearance::update(cx, |s| s.mode = ModeSetting::Light));
    shell.settle();

    shell.keys("ctrl-w");
    assert_eq!(settings_id(&mut shell), None, "Ctrl+W closed it");
    assert!(shell.groups().is_empty(), "the dock is empty again");

    shell.keys("ctrl-,");
    assert_eq!(
        settings_id(&mut shell),
        Some(settings),
        "the same panel came back"
    );
    let mode = shell.update(|_, cx| cx.global::<Appearance>().settings.mode);
    assert_eq!(
        mode,
        ModeSetting::Light,
        "the change made before closing held"
    );

    shell.click("close-Settings");
    assert_eq!(
        settings_id(&mut shell),
        None,
        "its tab's close button closes it"
    );

    shell.open_pr(42);
    shell.keys("ctrl-w");
    assert!(
        shell.open_numbers().is_empty(),
        "Ctrl+W closed the pull request's tab"
    );
}

#[gpui::test]
fn ctrl_click_opens_a_pull_request_beside(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    shell.open_pr(41);
    shell.click_with(&row(42), Modifiers::control());
    assert_eq!(shell.groups().len(), 2);
    assert!(shown(&mut shell, 42, PanelKind::Diff));
    assert!(!is_preview(&mut shell, 42));
}
