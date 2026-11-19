//! Running CI icons spin in a layer the shell paints over the window, at
//! the places the rows recorded. A spinner must never outlive its row: a
//! folded section, a pull request without running checks or a hidden
//! sidebar leaves none behind, and each one stays inside the clip of the
//! row that drew it.

use app::sidebar_panels::{Section, SectionPanel};
use app::ui::spinner::DockSpinners;
use gpui::{Bounds, Entity, Pixels, TestAppContext};

use crate::harness::{Options, ShellWindow, open_shell};

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

/// The spinners `section` would paint now.
fn spinning(shell: &mut ShellWindow, section: Section) -> Vec<(Bounds<Pixels>, Bounds<Pixels>)> {
    let panel = panel(shell, section);
    shell.update(|_, cx| panel.read(cx).spinners().visible())
}

/// Every spinner the layer would paint now.
fn all_spinning(shell: &mut ShellWindow) -> Vec<(Bounds<Pixels>, Bounds<Pixels>)> {
    shell.update(|_, cx| {
        DockSpinners::shown(cx)
            .iter()
            .flat_map(app::ui::spinner::SpinnerSlots::visible)
            .collect()
    })
}

fn checks_bounds(shell: &mut ShellWindow) -> Bounds<Pixels> {
    shell.bounds_of(String::leak(format!("section-{}", Section::Checks.name())))
}

/// Scrolls the Checks section down to its running job, as the wheel does:
/// the failing jobs come first and the section is short.
fn open_up_checks(shell: &mut ShellWindow) {
    let checks = checks_bounds(shell);
    shell.scroll_at(checks.center(), 60.0);
}

/// Draws twice: a slot whose row left the frame is forgotten after it.
fn two_frames(shell: &mut ShellWindow) {
    shell.redraw();
    shell.redraw();
}

#[gpui::test]
fn spinners_stay_with_their_rows(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    // #41's checks have a running job.
    shell.open_pr(41);
    two_frames(&mut shell);
    open_up_checks(&mut shell);
    two_frames(&mut shell);
    let checks = checks_bounds(&mut shell);
    let running = spinning(&mut shell, Section::Checks);
    assert!(!running.is_empty(), "a running job spins in Checks");
    for (slot, mask) in &running {
        assert!(
            checks.contains(&slot.center()),
            "inside its section: {slot:?}"
        );
        assert!(
            slot.intersects(mask),
            "not clipped away: {slot:?} / {mask:?}"
        );
    }

    // Folded, the section's rows are gone and so are their spinners.
    shell.click_named(&format!("section-toggle-{}", Section::Checks.name()));
    two_frames(&mut shell);
    assert_eq!(spinning(&mut shell, Section::Checks), [], "folded");
    shell.click_named(&format!("section-toggle-{}", Section::Checks.name()));
    two_frames(&mut shell);
    assert!(!spinning(&mut shell, Section::Checks).is_empty(), "back");

    // A pull request whose checks all passed spins nothing in the sections
    // that show it.
    shell.open_pr(1297);
    two_frames(&mut shell);
    for section in [
        Section::Stack,
        Section::Checks,
        Section::Threads,
        Section::Files,
    ] {
        assert_eq!(spinning(&mut shell, section), [], "{section:?}");
    }
    let checks = checks_bounds(&mut shell);
    assert!(
        all_spinning(&mut shell)
            .iter()
            .all(|(slot, _)| !checks.contains(&slot.center())),
        "nothing left over the Checks section"
    );
}

#[gpui::test]
fn a_hidden_sidebar_leaves_no_spinner(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    shell.open_pr(41);
    two_frames(&mut shell);
    open_up_checks(&mut shell);
    two_frames(&mut shell);
    assert!(!spinning(&mut shell, Section::Checks).is_empty());
    shell.keys("ctrl-b");
    two_frames(&mut shell);
    for section in Section::ALL {
        assert_eq!(
            spinning(&mut shell, section),
            [],
            "{section:?} is not drawn, so it spins nothing"
        );
    }
}

/// The state a stray spinner was once seen in: #41 (a running job) in a tab
/// behind the preview of #1297 (all passed). Nothing may spin anywhere: not
/// under the Checks rollup, not for the hidden tab.
#[gpui::test]
fn a_tab_behind_a_preview_leaves_no_spinner(cx: &mut TestAppContext) {
    use app::panels::PanelKind;
    use app::session::OpenMode;

    let mut shell = open_shell(cx, Options::default());
    shell.show(41, PanelKind::Conversation, OpenMode::Permanent);
    two_frames(&mut shell);
    assert!(
        !all_spinning(&mut shell).is_empty(),
        "#41 spins while shown"
    );
    shell.show(1297, PanelKind::Conversation, OpenMode::Preview);
    two_frames(&mut shell);
    assert_eq!(all_spinning(&mut shell), [], "nothing spins for #1297");
    let checks = checks_bounds(&mut shell);
    let threads = shell.bounds_of(String::leak(format!("section-{}", Section::Threads.name())));
    assert!(
        checks.bottom() <= threads.top() + gpui::px(1.0),
        "Checks ends where Threads starts"
    );
}
