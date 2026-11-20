//! Running CI icons spin in a layer the shell paints over the window, at
//! the places the rows recorded. A spinner must never outlive its row: a
//! folded pipelines card, a pull request without running checks or a hidden
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

/// The id of a running job of the focused pull request.
fn running_job(shell: &mut ShellWindow) -> String {
    let session = shell.session();
    shell.update(|_, cx| {
        let open = session.read(cx).open_pr().expect("open");
        open.ci
            .runs
            .iter()
            .flat_map(|run| &run.jobs)
            .find(|job| job.state == forge::model::CiState::Running)
            .map(|job| job.id.to_string())
            .expect("a running job")
    })
}

/// Shows `number`'s Conversation with its pipelines card open, scrolled to
/// the card's job list.
fn pipelines(shell: &mut ShellWindow, number: u64) {
    let key = shell.key(number);
    let session = shell.session();
    shell.update(|_, cx| {
        session.update(cx, |session, cx| {
            session.show_pipelines(&key, app::session::OpenMode::Permanent, cx);
        });
    });
    shell.settle();
    for _ in 0..80 {
        let body = shell.bounds_of("conversation-body");
        let card = shell.bounds_of("merge-box-checks");
        if card.top() >= body.top() && card.top() < body.center().y {
            return;
        }
        shell.scroll_at(
            body.center(),
            if card.top() < body.top() {
                -100.0
            } else {
                100.0
            },
        );
    }
    panic!("the pipelines card never came into view");
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
    pipelines(&mut shell, 41);
    two_frames(&mut shell);
    let job = running_job(&mut shell);
    let row = shell.bounds_of(String::leak(format!("ci-job-{job}")));
    let running = all_spinning(&mut shell);
    assert!(
        running.iter().any(|(slot, _)| row.contains(&slot.center())),
        "the running job spins in its row: {running:?}"
    );
    for (slot, mask) in &running {
        assert!(
            slot.intersects(mask),
            "not clipped away: {slot:?} / {mask:?}"
        );
    }

    // Folded, the card's rows are gone and so are their spinners.
    shell.click("merge-box-checks");
    two_frames(&mut shell);
    let body = shell.bounds_of("conversation-body");
    assert!(
        all_spinning(&mut shell)
            .iter()
            .all(|(slot, _)| !body.contains(&slot.center())),
        "folded"
    );
    shell.click("merge-box-checks");
    two_frames(&mut shell);
    let row = shell.bounds_of(String::leak(format!("ci-job-{job}")));
    assert!(
        all_spinning(&mut shell)
            .iter()
            .any(|(slot, _)| row.contains(&slot.center())),
        "back"
    );

    // A pull request whose checks all passed spins nothing in its card or
    // the sections that show it.
    pipelines(&mut shell, 1297);
    two_frames(&mut shell);
    for section in [Section::Stack, Section::Threads, Section::Files] {
        assert_eq!(spinning(&mut shell, section), [], "{section:?}");
    }
    let body = shell.bounds_of("conversation-body");
    assert!(
        all_spinning(&mut shell)
            .iter()
            .all(|(slot, _)| !body.contains(&slot.center())),
        "nothing left over the Conversation"
    );
}

#[gpui::test]
fn a_hidden_sidebar_leaves_no_spinner(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    // #41 is stacked on #40, whose checks run.
    shell.open_pr(41);
    two_frames(&mut shell);
    assert!(!spinning(&mut shell, Section::Stack).is_empty());
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
/// behind the preview of #1297 (all passed). Nothing may spin in the main
/// area: not for the hidden tab.
#[gpui::test]
fn a_tab_behind_a_preview_leaves_no_spinner(cx: &mut TestAppContext) {
    use app::panels::PanelKind;
    use app::session::OpenMode;

    let mut shell = open_shell(cx, Options::default());
    pipelines(&mut shell, 41);
    two_frames(&mut shell);
    let body = shell.bounds_of("conversation-body");
    let in_body = |shell: &mut ShellWindow| {
        all_spinning(shell)
            .into_iter()
            .filter(|(slot, _)| body.contains(&slot.center()))
            .count()
    };
    assert!(in_body(&mut shell) > 0, "#41 spins while shown");
    shell.show(1297, PanelKind::Conversation, OpenMode::Preview);
    two_frames(&mut shell);
    assert_eq!(in_body(&mut shell), 0, "nothing spins for #1297");
}
