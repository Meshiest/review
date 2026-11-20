//! The inbox's "Changes requested" section: the viewer's own pull requests
//! with a change request or unresolved threads, between "Review requested"
//! and "Created by me", which leaves them out. Each row says why; the
//! section folds, hides drafts and can be removed, which gives its rows back
//! to "Created by me".

use app::config::SectionKind;
use app::inbox::{SectionMenuItem, remove_from_inbox};
use app::model::RemovableSection;
use gpui::{SharedString, TestAppContext};

use crate::harness::{Options, ShellWindow, open_shell};

/// Each section's title and the numbers of its pull requests.
fn sections(shell: &mut ShellWindow) -> Vec<(String, Vec<u64>)> {
    let session = shell.session();
    shell.update(|_, cx| {
        let inbox = &session.read(cx).inbox;
        inbox
            .sections
            .iter()
            .map(|section| {
                let numbers = section
                    .items
                    .iter()
                    .map(|&ix| inbox.prs[ix].number())
                    .collect();
                (section.title.to_string(), numbers)
            })
            .collect()
    })
}

/// What the row of `number` in section `section` says on its status line,
/// and in which color.
fn status(shell: &mut ShellWindow, section: usize, number: u64) -> (SharedString, gpui::Hsla) {
    let session = shell.session();
    let sidebar = shell.sidebar();
    shell.update(|_, cx| {
        let pr = session
            .read(cx)
            .inbox
            .prs
            .iter()
            .position(|pr| pr.number() == number)
            .expect("in the inbox");
        sidebar.read(cx).row_status(section, pr, cx).expect("a row")
    })
}

#[gpui::test]
fn changes_requested_lists_what_waits_on_the_author_and_says_why(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    let listed = sections(&mut shell);
    let titles: Vec<&str> = listed.iter().map(|(title, _)| title.as_str()).collect();
    assert_eq!(
        titles[..3],
        ["Review requested", "Changes requested", "Created by me"]
    );
    assert_eq!(listed[1].1, [39, 1297], "a change request and a thread");
    assert!(
        listed[2].1.iter().all(|n| !listed[1].1.contains(n)),
        "Created by me leaves them out: {:?}",
        listed[2].1
    );
    assert_eq!(listed[2].1, [38, 40, 41]);

    assert!(shell.drawn("inbox-row-39") && shell.drawn("inbox-row-1297"));
    let colors = shell.update(|_, cx| app::ui::theme(cx).colors);
    assert_eq!(
        status(&mut shell, 1, 39),
        ("changes requested".into(), colors.danger)
    );
    assert_eq!(
        status(&mut shell, 1, 1297),
        ("1 unresolved".into(), colors.warn)
    );
    assert_eq!(
        status(&mut shell, 2, 38).0.as_ref(),
        "approved",
        "elsewhere the review"
    );

    let sidebar = shell.sidebar();
    let menu = shell.update(|_, cx| sidebar.read(cx).section_menu(1, cx));
    assert_eq!(
        menu,
        [
            SectionMenuItem::Fold,
            SectionMenuItem::HideDrafts,
            SectionMenuItem::RemoveFromInbox
        ]
    );

    shell.click("inbox-section-1");
    assert_eq!(
        shell.update(|_, cx| sidebar.read(cx).section_collapsed(1)),
        Some(true)
    );
    assert!(!shell.drawn("inbox-row-39"), "folded to its header");
    assert!(shell.drawn("inbox-row-38"), "the next section stays open");
    shell.click("inbox-section-1");
    assert!(shell.drawn("inbox-row-39"), "unfolded again");

    shell.update(|_, cx| sidebar.update(cx, |s, cx| s.set_section_hides_drafts(1, true, cx)));
    assert!(shell.update(|_, cx| sidebar.read(cx).section_hides_drafts(1)));
}

#[gpui::test]
fn removing_changes_requested_gives_its_rows_back_to_created_by_me(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    let section = RemovableSection::Builtin(SectionKind::ChangesRequested);
    shell.update(|_, cx| remove_from_inbox(&section, cx));
    shell.settle();

    let listed = sections(&mut shell);
    assert!(
        listed.iter().all(|(title, _)| title != "Changes requested"),
        "{listed:?}"
    );
    assert_eq!(listed[1].0, "Created by me");
    let mut mine = listed[1].1.clone();
    mine.sort_unstable();
    assert_eq!(mine, [38, 39, 40, 41, 1297]);
    assert_eq!(
        status(&mut shell, 1, 39).0.as_ref(),
        "changes requested",
        "the review again"
    );
    assert_eq!(status(&mut shell, 1, 1297).0.as_ref(), "commented");
    let sidebar = shell.sidebar();
    let saved = shell.update(|_, cx| sidebar.read(cx).saved_inbox());
    assert!(
        saved
            .sections
            .iter()
            .all(|s| s.title != "Changes requested"),
        "its saved state is forgotten"
    );
}
