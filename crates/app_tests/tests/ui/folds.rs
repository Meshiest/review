//! Rows that fold or unfold take a click anywhere on them, not only on a
//! chevron; the controls inside a row keep their own action.

use app::fixtures::Fixtures;
use forge::model::ForgeKind;
use gpui::TestAppContext;

use crate::harness::{Options, ShellWindow, open_shell};

/// The number of a fixture pull request on `forge`.
fn number_on(forge: ForgeKind) -> u64 {
    Fixtures::load()
        .inbox
        .prs
        .iter()
        .find(|row| row.forge == forge)
        .expect("a fixture pull request on the forge")
        .key
        .number
        .0
}

fn diff_shell(cx: &mut TestAppContext, number: u64) -> ShellWindow {
    let mut shell = open_shell(
        cx,
        Options {
            open_pr: Some(number),
            diff_rows: Some(4000),
            ..Options::default()
        },
    );
    // A click on a line gives the diff the keys.
    let (position, _) = shell.drawn_lines()[1];
    shell.click_at(position);
    shell
}

fn is_collapsed(shell: &mut ShellWindow, ix: usize) -> bool {
    let diff = shell.diff_panel();
    let cards = shell.update(|_, cx| diff.read(cx).cards().clone());
    shell.update(|_, cx| cards.read(cx).is_collapsed(ix, cx))
}

/// Steps through the diff's threads with `]t` until one with a card drawn
/// as `selector_of(ix)` is in view, folded or not as asked.
fn reach_thread(
    shell: &mut ShellWindow,
    folded: bool,
    selector_of: impl Fn(usize) -> String,
) -> usize {
    let diff = shell.diff_panel();
    let cards = shell.update(|_, cx| diff.read(cx).cards().clone());
    // Forward from where the click left the view, then back.
    for step in 0..48 {
        shell.keys(if step < 24 { "] t" } else { "[ t" });
        let Some(ix) = shell.update(|_, cx| cards.read(cx).current()) else {
            continue;
        };
        if is_collapsed(shell, ix) == folded && shell.drawn(&selector_of(ix)) {
            return ix;
        }
    }
    panic!("no thread with folded = {folded} came into view");
}

#[gpui::test]
fn a_click_in_the_middle_of_a_thread_row_folds_and_unfolds_it(cx: &mut TestAppContext) {
    let mut shell = diff_shell(cx, number_on(ForgeKind::GitHub));
    let ix = reach_thread(&mut shell, true, |ix| format!("card-row-{ix}"));
    shell.click_named(&format!("card-row-{ix}"));
    assert!(!is_collapsed(&mut shell, ix), "the row click unfolded it");

    shell.click_named(&format!("card-head-{ix}"));
    assert!(is_collapsed(&mut shell, ix), "the header click folded it");

    // The click left the focus on the row: Enter and Space fold it too.
    shell.keys("enter");
    assert!(
        !is_collapsed(&mut shell, ix),
        "Enter unfolded the focused row"
    );
    shell.keys("space");
    assert!(is_collapsed(&mut shell, ix), "and Space folded it");
}

#[gpui::test]
fn resolve_resolves_without_folding_the_card(cx: &mut TestAppContext) {
    let mut shell = diff_shell(cx, number_on(ForgeKind::GitHub));
    let ix = reach_thread(&mut shell, false, |ix| format!("card-head-{ix}"));
    let session = shell.session();
    let thread =
        shell.update(|_, cx| session.read(cx).open_pr().expect("open").threads[ix].clone());
    assert!(!thread.is_resolved(), "an open thread");
    shell.click_named(&format!("resolve-{}", app::threads::ix_of(&thread)));
    shell.settle();
    assert!(
        !is_collapsed(&mut shell, ix),
        "the click did not also fold the card"
    );
}

#[gpui::test]
fn a_click_near_a_tree_checkbox_toggles_it(cx: &mut TestAppContext) {
    let mut shell = open_shell(
        cx,
        Options {
            open_pr: Some(41),
            ..Options::default()
        },
    );
    // Tall enough that the tree's first file row clears the status bar.
    shell.resize(1440.0, 1600.0);
    let ix = (0..64usize)
        .find(|ix| {
            shell
                .element((gpui::SharedString::from("file-viewed"), *ix))
                .is_some()
        })
        .expect("a file in the tree");
    let id = || (gpui::SharedString::from("file-viewed"), ix);
    let before = shell.element(id()).expect("a file row").checked();
    let bounds = shell.element(id()).expect("a file row").bounds();
    assert!(
        bounds.size.width >= gpui::px(22.0) && bounds.size.height >= gpui::px(22.0),
        "the hit area is at least a row high: {bounds:?}"
    );
    // The corner of the hit area: outside the 14 px box in its middle.
    shell.click_at(bounds.origin + gpui::point(gpui::px(1.5), gpui::px(1.5)));
    let after = shell.element(id()).expect("a file row").checked();
    assert_ne!(
        after, before,
        "a click at the edge of the hit area toggled it"
    );
}

fn conversation_of(cx: &mut TestAppContext, number: u64) -> ShellWindow {
    let mut shell = open_shell(cx, Options::default());
    shell.show(
        number,
        app::panels::PanelKind::Conversation,
        app::session::OpenMode::Permanent,
    );
    shell
}

#[gpui::test]
fn the_ci_box_unfolds_in_place_and_a_failing_job_shows_its_log_tail(cx: &mut TestAppContext) {
    let mut shell = conversation_of(cx, 1288);
    let boxes = shell.update(|_, cx| app::conversation::ci_box::CiBoxes::global(cx));
    let expanded = |shell: &mut ShellWindow| {
        let session = shell.session();
        shell.update(|_, cx| {
            let key = session.read(cx).open_pr().expect("open").key.clone();
            boxes.read(cx).is_expanded(&key)
        })
    };
    let tabs = shell.shown_tabs();
    assert!(!expanded(&mut shell));

    // Empty space in the box: not a control, so the click is the box's.
    shell.click("ci-space");
    assert!(expanded(&mut shell), "a click in the box expanded it");
    assert_eq!(shell.shown_tabs(), tabs, "and changed no tab");
    shell.click("ci-space");
    assert!(!expanded(&mut shell), "a second click folded it");
    assert_eq!(shell.shown_tabs(), tabs);

    shell.click("ci-failing-0");
    shell.settle_until("the log tail", |cx| boxes.read(cx).tails_ready() > 0);
    assert!(
        shell.drawn("ci-tail-0"),
        "the tail is under the failing job"
    );
    assert!(!expanded(&mut shell), "the job click is not the box's");
    assert_eq!(shell.shown_tabs(), tabs, "no Log tab for a tail");
    shell.click("ci-failing-0");
    assert!(!shell.drawn("ci-tail-0"), "a second click closed it");

    shell.click_element(gpui::SharedString::from("open-checks"));
    assert_ne!(
        shell.shown_tabs(),
        tabs,
        "Open checks opened the Checks tab"
    );
    assert!(!expanded(&mut shell), "without folding the box");
}

#[gpui::test]
fn a_click_anywhere_on_a_checks_run_row_folds_it_but_not_on_its_link(cx: &mut TestAppContext) {
    let fixtures = Fixtures::load();
    let (row, run) = fixtures
        .inbox
        .prs
        .iter()
        .find_map(|row| {
            let run = fixtures
                .detail(&row.key)
                .runs
                .into_iter()
                .find(|run| run.url.is_some())?;
            Some((row.clone(), run))
        })
        .expect("a fixture run with a web page");
    let mut shell = open_shell(
        cx,
        Options {
            open_pr: Some(row.key.number.0),
            ..Options::default()
        },
    );
    let session = shell.session();
    shell.update(|_, cx| {
        session.update(cx, |session, cx| {
            session.show_panel(app::panels::PanelKind::Checks, cx)
        })
    });
    shell.settle();
    let folded = |shell: &mut ShellWindow| {
        shell.update(|_, cx| {
            session
                .read(cx)
                .open_pr()
                .is_some_and(|open| open.ci.folded.contains(&run.id))
        })
    };
    let selector = format!("run-{}", run.id);
    let header = shell.bounds_of(String::leak(selector.clone()));
    // The middle of the row, far from any control.
    shell.click_at(gpui::point(
        header.left() + header.size.width * 0.5,
        header.center().y,
    ));
    assert!(
        folded(&mut shell),
        "a click in the middle of the row folded it"
    );

    let link = format!(
        "web-link-{}",
        gpui::ElementId::from(("run-web", app::checks_panel::ix_of(&run)))
    );
    shell.click_at(gpui::point(
        header.left() + header.size.width * 0.5,
        header.center().y,
    ));
    assert!(!folded(&mut shell), "and a second click unfolded it");
    shell.click_named(&link);
    assert_eq!(
        shell.cx.opened_url().as_deref(),
        run.url.as_deref(),
        "the link opened the run's page"
    );
    assert!(!folded(&mut shell), "without folding the row");
}

#[gpui::test]
fn the_header_number_opens_the_pull_requests_web_page(cx: &mut TestAppContext) {
    let mut shell = conversation_of(cx, 1288);
    let session = shell.session();
    let url = shell.update(|_, cx| {
        let open = session.read(cx).open_pr().expect("open");
        let pr = open.forge_pr().expect("a forge record");
        Some(pr.web_url.clone())
            .filter(|url| !url.is_empty())
            .unwrap_or_else(|| app::references::pr_url(open.pr.forge, &pr.repo, pr.number))
    });
    shell.click_named(&format!("web-link:{url}"));
    assert_eq!(
        shell.cx.opened_url().as_deref(),
        Some(url.as_str()),
        "the number opened the page on the forge, not the tab already shown"
    );
}

#[gpui::test]
fn a_comments_menu_copies_its_link(cx: &mut TestAppContext) {
    let mut shell = diff_shell(cx, number_on(ForgeKind::GitHub));
    let ix = reach_thread(&mut shell, false, |ix| format!("card-head-{ix}"));
    let session = shell.session();
    let (key, forge, mut thread) = shell.update(|_, cx| {
        let open = session.read(cx).open_pr().expect("open");
        (open.key.clone(), open.pr.forge, open.threads[ix].clone())
    });
    thread.comments[0].id = forge::model::CommentId::new("42");
    thread.comments[0].web_url = None;
    let link = app::threads::comment_menu::comment_url(
        forge,
        &key.repo,
        key.number,
        &thread.comments[0],
        true,
    );
    let writing = shell.writing();
    let menu = app::threads::comment_menu::CommentMenu {
        link: link.clone(),
        text: thread.comments[0].body.clone(),
        thread: thread.clone(),
        writing,
        reply: true,
        resolve: true,
    };
    let url = link.expect("a numeric id has a permalink");
    assert!(shell.update(|_, cx| menu.copy_link(cx)));
    assert_eq!(
        shell.cx.read_from_clipboard().and_then(|item| item.text()),
        Some(url)
    );
    shell.update(|_, cx| menu.copy_text(cx));
    assert_eq!(
        shell.cx.read_from_clipboard().and_then(|item| item.text()),
        Some(thread.comments[0].body.clone())
    );
}
