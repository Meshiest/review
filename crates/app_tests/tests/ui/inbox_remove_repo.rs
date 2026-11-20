//! "Remove from inbox" on a repository's section header: the section leaves
//! the session and the panel, a refresh does not bring it back, and only
//! repository sections and "Changes requested" offer it.

use app::inbox::{SectionMenuItem, remove_from_inbox};
use app::model::{RemovableSection, RepoRef};
use gpui::TestAppContext;

use crate::harness::{Options, ShellWindow, open_shell};

fn titles(shell: &mut ShellWindow) -> Vec<String> {
    let session = shell.session();
    shell.update(|_, cx| {
        session
            .read(cx)
            .inbox
            .sections
            .iter()
            .map(|section| section.title.to_string())
            .collect()
    })
}

fn menu(shell: &mut ShellWindow, ix: usize) -> Vec<SectionMenuItem> {
    let sidebar = shell.sidebar();
    shell.update(|_, cx| sidebar.read(cx).section_menu(ix, cx))
}

fn collapsed(shell: &mut ShellWindow, ix: usize) -> Option<bool> {
    let sidebar = shell.sidebar();
    shell.update(|_, cx| sidebar.read(cx).section_collapsed(ix))
}

#[gpui::test]
fn only_repository_sections_and_changes_requested_offer_remove_from_inbox(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    assert_eq!(
        menu(&mut shell, 0),
        [SectionMenuItem::Fold, SectionMenuItem::HideDrafts]
    );
    assert_eq!(
        menu(&mut shell, 1),
        [
            SectionMenuItem::Fold,
            SectionMenuItem::HideDrafts,
            SectionMenuItem::RemoveFromInbox
        ]
    );
    assert_eq!(menu(&mut shell, 2), [SectionMenuItem::Fold]);
    for ix in 3..6 {
        assert_eq!(
            menu(&mut shell, ix),
            [SectionMenuItem::Unfold, SectionMenuItem::RemoveFromInbox],
            "section {ix} is folded and a repository's"
        );
    }
    assert_eq!(
        SectionMenuItem::RemoveFromInbox.label(),
        "Remove from inbox"
    );
}

#[gpui::test]
fn remove_from_inbox_drops_the_section_and_a_refresh_keeps_it_gone(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    let before = titles(&mut shell);
    assert_eq!(before.len(), 6);
    // The section after the one that goes is unfolded, so its state can be
    // seen moving up.
    let sidebar = shell.sidebar();
    shell.update(|_, cx| sidebar.update(cx, |s, cx| s.toggle_section(4, cx)));
    shell.settle();
    assert_eq!(collapsed(&mut shell, 4), Some(false));
    assert!(shell.drawn("inbox-section-5"));

    // What the menu item runs, for cora/review (section 3).
    let repo = RepoRef::new("forgejo.example.com", "cora", "review");
    shell.update(|_, cx| remove_from_inbox(&RemovableSection::Repo(repo), cx));
    shell.settle();

    let after = titles(&mut shell);
    assert_eq!(after.len(), 5);
    assert!(
        !after.contains(&"cora/review: open".to_owned()),
        "{after:?}"
    );
    let others: Vec<_> = before
        .iter()
        .filter(|t| *t != "cora/review: open")
        .collect();
    assert_eq!(
        after.iter().collect::<Vec<_>>(),
        others,
        "the rest in order"
    );
    assert!(!shell.drawn("inbox-section-5"), "one header fewer");
    assert_eq!(
        collapsed(&mut shell, 3),
        Some(false),
        "acme/api: open kept its unfolded state in its new place"
    );
    assert_eq!(collapsed(&mut shell, 0), Some(false), "others untouched");
    let saved = shell.update(|_, cx| sidebar.read(cx).saved_inbox());
    assert!(
        saved
            .sections
            .iter()
            .all(|s| s.title != "cora/review: open"),
        "its saved state is forgotten"
    );

    // A refresh sets the inbox again as the session has it.
    let session = shell.session();
    shell.update(|_, cx| session.update(cx, |session, cx| session.refresh_inbox(cx)));
    shell.settle();
    assert_eq!(titles(&mut shell), after);
}
