//! Rows whose CI shows queued or running are read again until it settles,
//! watched or not, and a stack's members show what their inbox rows show.

use std::sync::Arc;

use app::ci_settle::SETTLE_POLL;
use app::fixtures::Fixtures;
use app::model::{Inbox, PrKey, PullRequest as Row};
use app::sidebar_panels::{Section, SectionPanel};
use forge::model::{
    CiState, ForgeKind, PrNumber, PrState, PullRequest, PullRequestId, RepoRef, Sha,
};
use gpui::{Entity, TestAppContext};

use crate::harness::{
    FakeForge, Options, Scratch, ShellWindow, fake_services, go_live, open_shell,
};

fn repo() -> RepoRef {
    RepoRef::new("gitlab.com", "rackn", "rackn.com")
}

/// Merge request `number` of branch `head` into `base`, with `ci`.
fn mr(template: &PullRequest, number: u64, base: &str, head: &str, ci: CiState) -> PullRequest {
    let mut pr = template.clone();
    pr.id = PullRequestId::new(format!("mr-{number}"));
    pr.repo = repo();
    pr.number = PrNumber(number);
    pr.title = format!("Change {number}");
    pr.state = PrState::Open;
    pr.base.repo = Some(repo());
    pr.base.name = base.to_owned();
    pr.head.repo = Some(repo());
    pr.head.name = head.to_owned();
    pr.head.sha = Sha::new(format!("{number:040}"));
    pr.ci = Some(ci);
    pr
}

fn key(number: u64) -> PrKey {
    PrKey::new(repo(), PrNumber(number))
}

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

/// How many running CI icons `section` drew, scrolled into view or not.
fn spinning(shell: &mut ShellWindow, section: Section) -> usize {
    shell.redraw();
    shell.redraw();
    let panel = panel(shell, section);
    shell.update(|_, cx| panel.read(cx).spinners().len())
}

/// The CI each row shows, and each member of the open stack, by number.
fn shown(shell: &mut ShellWindow, numbers: &[u64]) -> Vec<CiState> {
    let session = shell.session();
    shell.update(|_, cx| {
        let session = session.read(cx);
        numbers
            .iter()
            .map(|&n| session.shown_ci(&key(n)).expect("a row"))
            .collect()
    })
}

#[gpui::test]
fn running_rows_and_stack_members_settle(cx: &mut TestAppContext) {
    let fixtures = Fixtures::load();
    let template = fixtures
        .forge_pr(&fixtures.inbox.prs[0].key)
        .expect("a fixture pull request");
    // A stack 29 <- 30 <- 31, as on GitLab, and a row that has settled.
    let prs = vec![
        mr(&template, 29, "main", "s29", CiState::Running),
        mr(&template, 30, "s29", "s30", CiState::Queued),
        mr(&template, 31, "s30", "s31", CiState::Running),
        mr(&template, 12, "main", "other", CiState::Pass),
    ];
    let scratch = Scratch::new("ci-settle");
    let fake = FakeForge::new(ForgeKind::GitLab, "gitlab.com", Vec::new());
    let services = fake_services(
        &scratch,
        vec![fake.clone() as Arc<dyn forge::Forge>],
        &[repo()],
    );
    let mut shell = open_shell(cx, Options::default());
    go_live(&mut shell, services, prs, ForgeKind::GitLab);
    shell.open_pr(30);
    let session = shell.session();
    let stack: Vec<u64> = shell.update(|_, cx| {
        let open = session.read(cx).open_pr().expect("#30 is open");
        open.stack.members.iter().map(|m| m.key.number.0).collect()
    });
    assert_eq!(stack, [29, 30, 31]);
    // #29 and #31; #30, queued, does not spin.
    assert_eq!(spinning(&mut shell, Section::Stack), 2);
    assert!(fake.ci_state_reads().is_empty(), "nothing before the poll");

    // The pipelines move on; nobody watches these rows.
    fake.set_ci(29, CiState::Pass);
    fake.set_ci(30, CiState::Running);
    fake.set_ci(31, CiState::Fail);
    shell.advance(SETTLE_POLL);
    shell.settle_until("the first read", |_| fake.ci_state_reads().len() == 1);
    shell.settle_until("the rows to follow it", |cx| {
        session.read(cx).shown_ci(&key(29)) == Some(CiState::Pass)
    });
    assert_eq!(
        fake.ci_state_reads(),
        [vec![29, 30, 31]],
        "one read for the repository, without the settled row"
    );
    assert_eq!(
        shown(&mut shell, &[29, 30, 31, 12]),
        [
            CiState::Pass,
            CiState::Running,
            CiState::Fail,
            CiState::Pass
        ]
    );
    assert_eq!(
        spinning(&mut shell, Section::Stack),
        2,
        "only #30 runs: its stack row and the pinned title above the stack"
    );

    // A manual pipeline is not waited on.
    fake.set_ci(30, CiState::Skipped);
    shell.advance(SETTLE_POLL);
    shell.settle_until("the second read", |_| fake.ci_state_reads().len() == 2);
    shell.settle_until("#30 to settle", |cx| {
        session.read(cx).shown_ci(&key(30)) == Some(CiState::Skipped)
    });
    assert_eq!(fake.ci_state_reads()[1], [30]);
    assert_eq!(spinning(&mut shell, Section::Stack), 0);
    assert_eq!(spinning(&mut shell, Section::Inbox), 0);

    shell.advance(SETTLE_POLL * 3);
    assert_eq!(
        fake.ci_state_reads().len(),
        2,
        "settled rows are not read again"
    );

    // The inbox built again from the same list, as when another section
    // answers or this one's refresh fails, keeps what the rows settled to.
    shell.update(|_, cx| {
        session.update(cx, |session, cx| {
            let mut inbox = Inbox::clone(&session.inbox);
            for row in &mut inbox.prs {
                let source = row.source.clone().expect("a forge row");
                *row = Row::from_forge(source, row.forge);
            }
            session.set_inbox(inbox, cx);
        });
    });
    assert_eq!(
        shown(&mut shell, &[29, 30, 31]),
        [CiState::Pass, CiState::Skipped, CiState::Fail]
    );
    shell.advance(SETTLE_POLL);
    assert_eq!(fake.ci_state_reads().len(), 2);
}
