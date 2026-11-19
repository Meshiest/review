//! A pull request's commits: the Conversation's "N commits" chip opens the
//! list, a commit opens its own diff in the Diff panel under a banner,
//! read-only, "Show all changes" returns to the whole diff, and Back and
//! Forward retrace the jump with the commit it showed.

use app::commits::PrCommit;
use app::panels::PanelKind;
use gpui::SharedString;

use crate::conversation::open;
use crate::harness::ShellWindow;

const NUMBER: u64 = 41;

fn commits(shell: &mut ShellWindow) -> Vec<PrCommit> {
    let session = shell.session();
    shell.update(|_, cx| {
        session
            .read(cx)
            .open_pr()
            .and_then(|open| open.commits.as_deref().map(<[PrCommit]>::to_vec))
            .expect("the fixture's commits")
    })
}

fn shown_commit(shell: &mut ShellWindow) -> Option<SharedString> {
    let session = shell.session();
    shell.update(|_, cx| session.read(cx).open_pr()?.shown_commit().cloned())
}

fn diff_paths(shell: &mut ShellWindow) -> Vec<String> {
    let view = shell.diff_view();
    shell.update(|_, cx| {
        view.read(cx)
            .list()
            .diff()
            .files
            .iter()
            .map(|file| file.path.clone())
            .collect()
    })
}

fn row(commit: &PrCommit) -> String {
    format!("commit-{}", commit.short())
}

/// Opens the list and clicks the `n`th commit, newest first.
fn open_commit(shell: &mut ShellWindow, n: usize) -> PrCommit {
    let commit = commits(shell)[n].clone();
    if !shell.drawn(&row(&commit)) {
        shell.click("commits-chip");
    }
    shell.click_named(&row(&commit));
    commit
}

#[gpui::test]
fn the_chip_counts_the_commits_and_opens_their_list(cx: &mut gpui::TestAppContext) {
    let mut shell = open(NUMBER, cx);
    let commits = commits(&mut shell);
    let pr = {
        let session = shell.session();
        shell.update(|_, cx| session.read(cx).open_pr().expect("open").pr.clone())
    };
    assert_eq!(
        commits,
        app::fixtures::commits(&pr),
        "the fixture's commits"
    );
    assert_eq!(commits.len(), 3);
    assert_eq!(
        commits[0].short(),
        pr.sha.as_ref(),
        "the newest is the head"
    );
    assert!(commits.iter().any(|c| c.author != commits[0].author));

    assert!(shell.drawn("commits-chip"));
    assert!(!shell.drawn("commit-list"), "the list starts closed");
    shell.click("commits-chip");
    assert!(shell.drawn("commit-list"));
    for commit in &commits {
        assert!(shell.drawn(&row(commit)), "{} is listed", commit.short());
    }
    let list = shell.bounds_of("commit-list");
    let rows: Vec<f32> = commits
        .iter()
        .map(|commit| f32::from(shell.bounds_of(String::leak(row(commit))).top()))
        .collect();
    assert!(rows.is_sorted(), "newest first, top to bottom: {rows:?}");
    assert!(f32::from(list.top()) <= rows[0]);

    shell.click("commits-chip");
    assert!(!shell.drawn("commit-list"), "a second click closes it");
}

#[gpui::test]
fn a_commit_shows_its_own_diff_read_only_under_a_banner(cx: &mut gpui::TestAppContext) {
    let mut shell = open(NUMBER, cx);
    let commit = open_commit(&mut shell, 1);
    let diff = shell
        .panel_of(NUMBER, PanelKind::Diff)
        .expect("the Diff opened");
    assert!(shell.is_shown(diff), "the click shows the diff");
    assert_eq!(shown_commit(&mut shell), Some(commit.sha.clone()));
    assert!(shell.drawn("commit-banner"));

    let files = {
        let session = shell.session();
        shell.update(|_, cx| session.read(cx).open_pr().expect("open").files.clone())
    };
    let expected: Vec<String> = app::fixtures::commit_diff(NUMBER, &commit, &files)
        .files
        .into_iter()
        .map(|file| file.path)
        .collect();
    assert_eq!(diff_paths(&mut shell), expected, "the commit's files alone");
    assert!(expected.len() < files.len(), "fewer files than all changes");

    let panel = shell.diff_panel();
    let (read_only, cards) = shell.update(|_, cx| {
        let panel = panel.read(cx);
        (panel.read_only(), panel.cards().read(cx).placements().len())
    });
    assert!(read_only, "a commit's diff takes no comments");
    assert_eq!(cards, 0, "no thread is placed on a commit's lines");
}

#[gpui::test]
fn show_all_changes_returns_to_the_whole_diff(cx: &mut gpui::TestAppContext) {
    let mut shell = open(NUMBER, cx);
    open_commit(&mut shell, 0);
    assert!(shell.drawn("commit-banner"));

    shell.click_element("show-all-changes");
    assert_eq!(shown_commit(&mut shell), None);
    assert!(!shell.drawn("commit-banner"));
    let files: Vec<String> = {
        let session = shell.session();
        shell.update(|_, cx| {
            let open = session.read(cx).open_pr().expect("open");
            open.files.iter().map(|f| f.path.to_string()).collect()
        })
    };
    assert_eq!(diff_paths(&mut shell), files, "every file again");
    let panel = shell.diff_panel();
    assert!(!shell.update(|_, cx| panel.read(cx).read_only()));
}

#[gpui::test]
fn back_returns_to_the_conversation_and_forward_to_the_commit(cx: &mut gpui::TestAppContext) {
    let mut shell = open(NUMBER, cx);
    let conversation_tab = shell
        .panel_of(NUMBER, PanelKind::Conversation)
        .expect("the Conversation tab");
    let commit = open_commit(&mut shell, 2);
    let diff = shell
        .panel_of(NUMBER, PanelKind::Diff)
        .expect("the Diff opened");

    assert!(shell.is_shown(diff));
    assert!(
        shell.read_shell(|s, _| s.can_go_back()),
        "the jump is recorded"
    );
    shell.keys("alt-left");
    assert!(
        shell.is_shown(conversation_tab),
        "Back shows the conversation"
    );
    assert!(!shell.is_shown(diff));

    // Elsewhere the diff went back to all changes; Forward brings the commit.
    shell.update(|_, cx| {
        let session = app::session::Session::global(cx);
        session.update(cx, |session, cx| {
            let key = session.open_key().cloned().expect("open");
            session.set_shown_commit(&key, None, cx);
        });
    });
    shell.keys("alt-right");
    assert!(shell.is_shown(diff), "Forward shows the diff again");
    assert_eq!(shown_commit(&mut shell), Some(commit.sha));
    assert!(shell.drawn("commit-banner"));
}

#[gpui::test]
fn enter_on_a_listed_commit_opens_it(cx: &mut gpui::TestAppContext) {
    let mut shell = open(NUMBER, cx);
    let commits = commits(&mut shell);
    shell.click("commits-chip");
    shell.keys("tab enter");
    assert_eq!(
        shown_commit(&mut shell),
        Some(commits[0].sha.clone()),
        "Tab moves from the chip to the newest commit and Enter opens it"
    );
    assert!(shell.drawn("commit-banner"));
}
