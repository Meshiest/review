//! Widths: tab titles as wide as their text up to a cap; thread rows,
//! inline-thread rows and the checks tree across the whole panel; the
//! description wrapping at the panel's width.

use std::sync::Arc;

use app::fixtures::Fixtures;
use app::panels::{PanelKind, TAB_TITLE_MAX, tab_selector};
use app::session::OpenMode;
use forge::model::{ForgeKind, PrState};
use gpui::{TestAppContext, px};

use crate::harness::{
    FakeForge, Options, Scratch, ShellWindow, fake_services, flat, go_live, open_shell,
};

/// A GitHub fixture on a fake forge, renamed and described as `edit` says,
/// with its Conversation shown.
fn live(
    cx: &mut TestAppContext,
    scratch: &Scratch,
    edit: impl FnOnce(&mut forge::model::PullRequest),
) -> (ShellWindow, app::model::PrKey) {
    let fixtures = Fixtures::load();
    let source = fixtures
        .inbox
        .prs
        .iter()
        .find(|pr| pr.forge == ForgeKind::GitHub && pr.state == PrState::Open)
        .map(|pr| pr.key.clone())
        .expect("an open GitHub fixture");
    let mut pr = fixtures.forge_pr(&source).expect("its forge record");
    edit(&mut pr);
    let fake = FakeForge::new(ForgeKind::GitHub, &pr.repo.host, Vec::new());
    let services = fake_services(
        scratch,
        vec![fake as Arc<dyn forge::Forge>],
        std::slice::from_ref(&pr.repo),
    );
    let mut shell = open_shell(
        cx,
        Options {
            theme: flat(),
            ..Options::default()
        },
    );
    let key = app::model::PrKey::of(&pr);
    go_live(&mut shell, services, vec![pr], ForgeKind::GitHub);
    let session = shell.session();
    let show = key.clone();
    shell.update(|_, cx| {
        session.update(cx, |session, cx| {
            session.show(&show, PanelKind::Conversation, OpenMode::Permanent, cx);
        });
    });
    shell.settle();
    (shell, key)
}

#[gpui::test]
fn a_short_tab_title_is_as_wide_as_its_text(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    shell.open_pr(41);
    let key = shell.key(41);
    let title = shell.bounds_of(String::leak(tab_selector(&key, PanelKind::Diff)));
    assert!(
        title.size.width < px(TAB_TITLE_MAX) && title.size.width > px(40.0),
        "sized to its text: {title:?}"
    );
}

#[gpui::test]
fn a_long_tab_title_stops_at_the_cap(cx: &mut TestAppContext) {
    let scratch = Scratch::new("long-tab");
    let (mut shell, key) = live(cx, &scratch, |pr| {
        pr.repo.name = "a-repository-whose-name-runs-well-past-what-a-tab-shows".into();
    });
    let title = shell.bounds_of(String::leak(tab_selector(&key, PanelKind::Conversation)));
    assert_eq!(
        title.size.width,
        px(TAB_TITLE_MAX),
        "capped, then ellipsized"
    );
}

#[gpui::test]
fn thread_rows_and_the_checks_tree_use_the_whole_width(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    shell.open_pr(41);
    let threads = shell.bounds_of("section-Threads");
    let row = shell.bounds_of("thread-row-0");
    assert!(
        row.size.width >= threads.size.width - px(12.0),
        "a sidebar thread row spans its panel: {row:?} in {threads:?}"
    );

    shell.show(41, PanelKind::Conversation, OpenMode::Permanent);
    let session = shell.session();
    let thread = shell.update(|_, cx| {
        let open = session.read(cx).open_pr().expect("open");
        open.threads
            .iter()
            .find(|t| t.anchor.is_some())
            .map(|t| t.id.to_string())
            .expect("an inline thread")
    });
    let description = shell.bounds_of("pr-description");
    let inline = shell.bounds_of(String::leak(format!("inline-thread-{thread}")));
    assert_eq!(
        inline.size.width, description.size.width,
        "an expanded inline thread spans the conversation"
    );
    shell.click("thread-style-1");
    let inline = shell.bounds_of(String::leak(format!("inline-row-{thread}")));
    assert_eq!(
        inline.size.width, description.size.width,
        "an inline thread row spans the conversation"
    );
    shell.click("thread-style-0");

    shell.show(41, PanelKind::Checks, OpenMode::Permanent);
    let tree = shell.bounds_of("checks-scroll");
    let job = shell.update(|_, cx| {
        let open = session.read(cx).open_pr().expect("open");
        open.ci
            .runs
            .iter()
            .flat_map(|run| &run.jobs)
            .next()
            .map(|job| job.id.to_string())
            .expect("a job")
    });
    let job = shell.bounds_of(String::leak(format!("job-{job}")));
    assert_eq!(job.size.width, tree.size.width, "a job row spans the tree");
}

#[gpui::test]
fn the_description_wraps_at_the_panel_width(cx: &mut TestAppContext) {
    let scratch = Scratch::new("wrap");
    let (mut shell, _) = live(cx, &scratch, |pr| {
        pr.body = "wrapping words ".repeat(120);
    });
    let description = shell.bounds_of("pr-description");
    let scroll = shell.bounds_of("conversation-body");
    assert!(
        description.right() <= scroll.right(),
        "inside the panel: {description:?} in {scroll:?}"
    );
    assert!(
        description.size.height > px(60.0),
        "on several lines: {description:?}"
    );
}
