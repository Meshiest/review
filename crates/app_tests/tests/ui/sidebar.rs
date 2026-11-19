//! The sidebar from the keyboard and its file tree: `j`, `k` and Enter open
//! pull requests in inbox order, and toggling a file viewed (its checkbox,
//! or `v` on the selected file) rolls up into its directory's checkbox.

use gpui::{SharedString, TestAppContext, point, px};

use crate::harness::{Options, ShellWindow, open_shell};

/// The pull requests in the order the inbox lists them.
fn inbox_order(shell: &mut ShellWindow) -> Vec<u64> {
    let session = shell.session();
    shell.update(|_, cx| {
        let inbox = &session.read(cx).inbox;
        inbox
            .sections
            .iter()
            .filter(|section| section.expanded_by_default)
            .flat_map(|section| &section.items)
            .map(|&ix| inbox.prs[ix].number())
            .collect()
    })
}

fn open_number(shell: &mut ShellWindow) -> Option<u64> {
    let session = shell.session();
    shell.update(|_, cx| session.read(cx).open_key().map(|key| key.number.0))
}

#[gpui::test]
fn j_k_and_enter_open_pull_requests_in_inbox_order(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    let order = inbox_order(&mut shell);
    assert!(
        order.len() >= 3,
        "the fixture inbox lists several: {order:?}"
    );
    assert_eq!(open_number(&mut shell), None);

    shell.keys("j j enter");
    assert_eq!(open_number(&mut shell), Some(order[1]));
    shell.keys("k enter");
    assert_eq!(open_number(&mut shell), Some(order[0]));
    shell.keys("j j j enter");
    assert_eq!(
        open_number(&mut shell),
        Some(order[3]),
        "the list keeps its focus while pull requests open"
    );
}

/// A row of the file tree as drawn: its checkbox and how it is checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RollUp {
    None,
    Partial,
    Full,
}

struct TreeRow {
    ix: usize,
    dir: bool,
    /// The checkbox's left edge, which is the row's indent.
    left: f32,
}

fn checkbox_id(dir: bool, ix: usize) -> (SharedString, usize) {
    let name = if dir { "dir-viewed" } else { "file-viewed" };
    (name.into(), ix)
}

fn state(shell: &mut ShellWindow, dir: bool, ix: usize) -> RollUp {
    let (name, ix) = checkbox_id(dir, ix);
    let element = shell
        .element((name.clone(), ix))
        .unwrap_or_else(|| panic!("no {name} {ix} drawn"));
    match (element.checked(), element.indeterminate()) {
        (_, Some(true)) => RollUp::Partial,
        (Some(true), _) => RollUp::Full,
        _ => RollUp::None,
    }
}

fn click_checkbox(shell: &mut ShellWindow, dir: bool, ix: usize) {
    let (name, ix) = checkbox_id(dir, ix);
    shell.click_element((name, ix));
}

/// The drawn tree, row by row, read from the checkboxes it registered.
fn tree(shell: &mut ShellWindow) -> Vec<TreeRow> {
    let mut rows = Vec::new();
    for ix in 0.. {
        let (dir, element) = match (
            shell.element(checkbox_id(true, ix)),
            shell.element(checkbox_id(false, ix)),
        ) {
            (Some(element), _) => (true, element),
            (None, Some(element)) => (false, element),
            (None, None) => break,
        };
        rows.push(TreeRow {
            ix,
            dir,
            left: f32::from(element.bounds().origin.x),
        });
    }
    rows
}

#[gpui::test]
fn viewed_toggles_roll_up_into_the_directory(cx: &mut TestAppContext) {
    let mut shell = open_shell(
        cx,
        Options {
            open_pr: Some(41),
            ..Options::default()
        },
    );
    // Tall enough that the whole file tree is on screen, below the detail.
    shell.resize(1440.0, 3000.0);
    let rows = tree(&mut shell);
    // A directory with at least two files right under it: the rows after it
    // that are indented further, up to the first that is not.
    let (dir, files) = rows
        .iter()
        .filter(|row| row.dir)
        .find_map(|dir| {
            let under: Vec<usize> = rows[dir.ix + 1..]
                .iter()
                .take_while(|row| row.left > dir.left)
                .filter(|row| !row.dir)
                .map(|row| row.ix)
                .collect();
            (under.len() >= 2).then_some((dir.ix, under))
        })
        .expect("a directory with two files in the fixture tree");
    let file = files[0];

    // Clear the directory: a partial or full one goes to empty in at most two.
    for _ in 0..2 {
        if state(&mut shell, true, dir) != RollUp::None {
            click_checkbox(&mut shell, true, dir);
        }
    }
    assert_eq!(state(&mut shell, true, dir), RollUp::None);

    click_checkbox(&mut shell, false, file);
    assert_eq!(state(&mut shell, false, file), RollUp::Full);
    assert_eq!(
        state(&mut shell, true, dir),
        RollUp::Partial,
        "one file of several viewed is a partial roll-up"
    );

    // Select the file by clicking its name, then `v` toggles it back.
    let checkbox = shell
        .element(checkbox_id(false, file))
        .expect("the file's checkbox")
        .bounds();
    shell.click_at(point(checkbox.right() + px(60.0), checkbox.center().y));
    shell.keys("v");
    assert_eq!(state(&mut shell, false, file), RollUp::None);
    assert_eq!(state(&mut shell, true, dir), RollUp::None);
    shell.keys("v");
    assert_eq!(state(&mut shell, true, dir), RollUp::Partial);

    click_checkbox(&mut shell, true, dir);
    assert_eq!(state(&mut shell, true, dir), RollUp::Full);
    for &file in &files {
        assert_eq!(
            state(&mut shell, false, file),
            RollUp::Full,
            "the directory's box marks every file under it"
        );
    }
}

/// A fixture pull request with a failed job, and that job's name.
fn with_a_failed_job() -> (u64, String) {
    let fixtures = app::fixtures::Fixtures::load();
    fixtures
        .inbox
        .prs
        .iter()
        .find_map(|row| {
            let job = fixtures
                .detail(&row.key)
                .runs
                .into_iter()
                .flat_map(|run| run.jobs)
                .find(|job| job.state == forge::model::CiState::Fail)?;
            Some((row.number(), job.name))
        })
        .expect("a fixture pull request with a failed job")
}

#[gpui::test]
fn the_checks_section_lists_failing_jobs_and_opens_their_log(cx: &mut TestAppContext) {
    let (number, name) = with_a_failed_job();
    let mut shell = open_shell(cx, Options::default());
    shell.open_pr(number);

    let session = shell.session();
    let rows = shell.update(|_, cx| {
        app::sidebar::check_rows(
            session
                .read(cx)
                .open_pr()
                .expect("the pull request is open"),
        )
    });
    let first = rows
        .iter()
        .find(|row| row.is_listed())
        .expect("a job with a row of its own")
        .clone();
    assert_eq!(first.state, forge::model::CiState::Fail, "failing first");
    assert_eq!(first.name, name);
    assert!(
        !first.duration_label().is_empty() && first.duration_label() != "failing",
        "a duration: {:?}",
        first.duration_label()
    );
    assert!(shell.drawn("sidebar-check-0"), "its row is drawn");

    shell.click("sidebar-check-0");
    let log = shell.update(|_, cx| {
        session
            .read(cx)
            .open_pr()
            .and_then(|open| open.ci.log.as_ref().map(|log| log.job.id.clone()))
    });
    assert_eq!(
        log.as_ref().map(|id| id.as_str().to_owned()),
        first.job.as_ref().map(|id| id.as_str().to_owned()),
        "its log is in the Log panel"
    );
    assert!(shell.shown_tabs().iter().any(|tab| tab == app::panels::LOG));
    assert!(
        shell
            .shown_tabs()
            .iter()
            .any(|tab| tab == app::panels::CHECKS)
    );
}
