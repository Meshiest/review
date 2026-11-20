//! The Conversation page: inline threads expanded with the code they are on
//! (or compact, as the user chooses), the jump to the diff, suggestions
//! drawn as suggestions, the side column with reviewers, assignees, labels
//! and the milestone, and links from references, on fixtures and on a fake
//! forge.

use std::sync::Arc;
use std::time::Duration;

use app::appearance::Appearance;
use app::conversation::ConversationPanel;
use app::fixtures::Fixtures;
use app::lifecycle::Load;
use app::model::PrKey;
use app::panels::PanelKind;
use app::people::Field;
use app::session::OpenMode;
use app::settings::ThreadStyle;
use app::side_column::SEARCH_DEBOUNCE;
use app::thread_code::LineKind;
use forge::model::{ForgeKind, PrState, ReviewState, Thread};
use gpui::{Entity, Modifiers, MouseButton, SharedString, TestAppContext, point, px};

use crate::harness::{
    FakeForge, Options, Scratch, ShellWindow, fake_services, flat, go_live, open_shell,
};

pub(crate) fn conversation(shell: &mut ShellWindow) -> Entity<ConversationPanel> {
    shell.panel(app::panels::CONVERSATION)
}

pub(crate) fn open(number: u64, cx: &mut TestAppContext) -> ShellWindow {
    let mut shell = open_shell(cx, Options::default());
    shell.show(number, PanelKind::Conversation, OpenMode::Permanent);
    shell
}

pub(crate) fn threads(shell: &mut ShellWindow) -> Arc<[Thread]> {
    let session = shell.session();
    shell.update(|_, cx| session.read(cx).open_pr().expect("open").threads.clone())
}

/// Scrolls the conversation until the element with `selector` is inside it.
pub(crate) fn reveal(shell: &mut ShellWindow, selector: &str) {
    let selector = String::leak(selector.to_owned());
    for _ in 0..60 {
        let body = shell.bounds_of("conversation-body");
        let element = shell.bounds_of(selector);
        if element.top() >= body.top() && element.bottom() <= body.bottom() {
            return;
        }
        let dy = if element.top() < body.top() {
            -200.0
        } else {
            200.0
        };
        shell.scroll_at(body.center(), dy);
    }
    panic!("{selector} never came into view");
}

/// Settles until the Conversation has read the code of its threads.
pub(crate) fn code_ready(shell: &mut ShellWindow) {
    let panel = conversation(shell);
    shell.settle_until("the threads' code", |cx| {
        panel
            .read(cx)
            .excerpt(&forge::model::ThreadId::new("none"))
            .is_some()
    });
}

fn forge_pr(shell: &mut ShellWindow, key: &PrKey) -> forge::model::PullRequest {
    let session = shell.session();
    shell.update(|_, cx| {
        session
            .read(cx)
            .pr(key)
            .and_then(|open| open.forge_pr().cloned())
            .expect("a forge record")
    })
}

#[gpui::test]
fn inline_threads_show_the_code_they_are_on(cx: &mut TestAppContext) {
    let mut shell = open(41, cx);
    code_ready(&mut shell);
    let threads = threads(&mut shell);
    let panel = conversation(&mut shell);
    let current = threads
        .iter()
        .find(|t| !t.outdated && t.anchor.as_ref().is_some_and(|a| a.position.is_some()))
        .expect("a current inline thread");
    let id = current.id.to_string();
    assert!(
        shell.drawn(&format!("inline-thread-{id}")),
        "expanded by default"
    );
    assert!(shell.drawn(&format!("inline-code-{id}")), "with its code");
    let excerpt = shell
        .update(|_, cx| panel.read(cx).excerpt(&current.id))
        .flatten()
        .expect("the code was read");
    let position = current
        .anchor
        .as_ref()
        .and_then(|a| a.position.as_ref())
        .expect("a position");
    let last = excerpt.lines.last().expect("lines");
    assert_eq!(
        last.number, position.line.line,
        "it ends on the commented line"
    );
    assert!(last.marked, "the commented line is marked");
    assert!(
        excerpt.lines.len() > 1 && !excerpt.lines[0].marked,
        "lines above it give context: {:?}",
        excerpt.lines
    );
    let side = match position.line.side {
        forge::model::Side::Old => diff::Side::Old,
        forge::model::Side::New => diff::Side::New,
    };
    assert_eq!(excerpt.side, side);
    assert!(
        excerpt.lines.iter().any(|l| l.kind != LineKind::Context),
        "changed lines are told apart"
    );

    // An old-side thread shows the removed lines.
    let old = threads.iter().find(|t| {
        t.anchor
            .as_ref()
            .and_then(|a| a.position.as_ref())
            .is_some_and(|p| p.line.side == forge::model::Side::Old)
    });
    if let Some(old) = old {
        let excerpt = shell
            .update(|_, cx| panel.read(cx).excerpt(&old.id))
            .flatten()
            .expect("an old-side excerpt");
        assert_eq!(excerpt.side, diff::Side::Old);
    }
}

#[gpui::test]
fn outdated_threads_keep_their_lines_and_say_so(cx: &mut TestAppContext) {
    let mut shell = open(41, cx);
    code_ready(&mut shell);
    let threads = threads(&mut shell);
    let panel = conversation(&mut shell);
    let outdated: Vec<&Thread> = threads.iter().filter(|t| t.outdated).collect();
    assert!(!outdated.is_empty(), "the fixtures have outdated threads");
    let mut shown = 0;
    for thread in outdated {
        let id = thread.id.to_string();
        assert!(shell.drawn(&format!("inline-thread-{id}")));
        let excerpt = shell
            .update(|_, cx| panel.read(cx).excerpt(&thread.id))
            .flatten();
        if let Some(excerpt) = excerpt {
            shown += 1;
            assert!(excerpt.lines.last().is_some_and(|l| l.marked));
            assert!(shell.drawn(&format!("inline-code-{id}")));
        }
    }
    assert!(
        shown > 0,
        "an outdated thread whose lines are known shows them"
    );
}

#[gpui::test]
fn jump_to_line_opens_the_diff_at_the_thread(cx: &mut TestAppContext) {
    let mut shell = open(41, cx);
    code_ready(&mut shell);
    let threads = threads(&mut shell);
    let (index, thread) = threads
        .iter()
        .enumerate()
        .filter(|(_, t)| !t.outdated && t.anchor.is_some())
        .nth(2)
        .expect("a third inline thread");
    reveal(&mut shell, &format!("jump-{}", thread.id));
    shell.click_named(&format!("jump-{}", thread.id));
    let diff = shell
        .panel_of(41, PanelKind::Diff)
        .expect("the Diff tab opened");
    assert!(shell.is_shown(diff), "and came forward");
    let panel = shell.diff_panel();
    let view = shell.diff_view();
    let (current, block_row, rows) = shell.update(|_, cx| {
        let cards = panel.read(cx).cards().read(cx);
        let block = cards.block_of(index).expect("the thread has a block");
        let view = view.read(cx);
        (
            cards.current(),
            view.list().block_row(block),
            view.rows_in_view().collect::<Vec<_>>(),
        )
    });
    assert_eq!(current, Some(index), "the thread is the current one");
    assert!(
        rows.contains(&block_row),
        "its card is in view: row {block_row} of {rows:?}"
    );
}

#[gpui::test]
fn the_compact_toggle_keeps_one_row_per_thread_and_is_saved(cx: &mut TestAppContext) {
    let mut shell = open(41, cx);
    let threads = threads(&mut shell);
    let thread = threads
        .iter()
        .find(|t| t.anchor.is_some())
        .expect("an inline thread");
    let id = thread.id.to_string();
    // The shared toggle group: one strip of joined choices, the diff's
    // Unified / Split height.
    let (expanded, compact) = (
        shell.bounds_of("thread-style-0"),
        shell.bounds_of("thread-style-1"),
    );
    assert_eq!(expanded.right(), compact.left(), "the choices are joined");
    assert_eq!(
        f32::from(compact.size.height),
        app::ui::TOGGLE_HEIGHT - 2.0,
        "inside the group's border"
    );
    shell.click("thread-style-1");
    let style = |shell: &mut ShellWindow| {
        shell.update(|_, cx| cx.global::<Appearance>().settings.conversation.threads)
    };
    assert_eq!(style(&mut shell), ThreadStyle::Compact, "saved in settings");
    assert!(shell.drawn(&format!("inline-row-{id}")));
    assert!(!shell.drawn(&format!("inline-thread-{id}")));
    shell.click_named(&format!("inline-row-{id}"));
    assert!(
        shell.panel_of(41, PanelKind::Diff).is_some(),
        "a compact row still jumps to the diff"
    );

    shell.show(41, PanelKind::Conversation, OpenMode::Permanent);
    shell.click("thread-style-0");
    assert_eq!(style(&mut shell), ThreadStyle::Expanded);
    assert!(shell.drawn(&format!("inline-thread-{id}")));
}

/// The element number the app gives a comment's suggestion buttons.
fn hash_ix(text: &str) -> usize {
    text.bytes().fold(0usize, |h, b| {
        h.wrapping_mul(31).wrapping_add(usize::from(b))
    })
}

#[gpui::test]
fn suggestions_are_drawn_as_changes_with_apply(cx: &mut TestAppContext) {
    let mut shell = open(41, cx);
    code_ready(&mut shell);
    let threads = threads(&mut shell);
    let (thread, comment) = threads
        .iter()
        .filter(|t| !t.outdated)
        .find_map(|t| {
            let first = t.comments.first()?;
            first.body.contains("```suggestion").then_some((t, first))
        })
        .expect("a thread whose first comment suggests a change");
    assert!(
        shell.drawn(&format!("suggestion-{}", comment.id)),
        "the block is a suggestion, not a code block"
    );
    assert!(shell.drawn(&format!("inline-thread-{}", thread.id)));
    reveal(&mut shell, &format!("suggestion-{}", comment.id));
    let body = shell.bounds_of("conversation-body").center();
    shell.scroll_at(body, 120.0);
    let apply = SharedString::from("apply-suggestion");
    shell.click_element((apply, hash_ix(comment.id.as_str())));
    let session = shell.session();
    let confirming = shell.update(|_, cx| {
        session
            .read(cx)
            .open_pr()
            .is_some_and(|open| open.confirm.is_some())
    });
    assert!(confirming, "Apply asks to confirm, as from the diff");
}

#[gpui::test]
fn the_side_column_lists_reviewers_assignees_labels_and_the_milestone(cx: &mut TestAppContext) {
    let mut shell = open(1297, cx);
    let key = shell.key(1297);
    let pr = forge_pr(&mut shell, &key);
    assert!(shell.drawn("people-column"));
    for reviewer in &pr.reviewers {
        assert!(shell.drawn(&format!("reviewer-{}", reviewer.user.login)));
        assert_eq!(reviewer.state, ReviewState::Commented, "the row's verdict");
    }
    assert!(shell.drawn(&format!("assignee-{}", pr.author.login)));
    assert!(!pr.labels.is_empty());
    for label in &pr.labels {
        assert!(shell.drawn(&format!("label-{}", label.name)));
    }
    assert!(shell.drawn("side-milestone"));
}

#[gpui::test]
fn the_side_column_sits_beside_or_under_the_header_by_width(cx: &mut TestAppContext) {
    let mut shell = open(1297, cx);
    shell.resize(1900.0, 1000.0);
    shell.redraw();
    let column = shell.bounds_of("people-column");
    let description = shell.bounds_of("pr-description");
    assert!(
        column.left() >= description.right(),
        "beside: {column:?} / {description:?}"
    );
    let panel = conversation(&mut shell);
    assert!(shell.update(|_, cx| panel.read(cx).is_wide()));

    shell.resize(640.0, 1000.0);
    shell.redraw();
    let column = shell.bounds_of("people-column");
    let description = shell.bounds_of("pr-description");
    assert!(
        column.bottom() <= description.top(),
        "stacked above the description: {column:?} / {description:?}"
    );
    assert!(!shell.update(|_, cx| panel.read(cx).is_wide()));
}

#[gpui::test]
fn reviewers_are_requested_and_removed_from_the_picker(cx: &mut TestAppContext) {
    let mut shell = open(1297, cx);
    let key = shell.key(1297);
    shell.click("picker-reviewers");
    let panel = conversation(&mut shell);
    let side = shell.update(|_, cx| panel.read(cx).side_column().clone());
    assert_eq!(
        shell.update(|_, cx| side.read(cx).picker()),
        Some(Field::Reviewers)
    );
    shell.type_text("we");
    shell.advance(SEARCH_DEBOUNCE);
    assert!(shell.drawn("pick-reviewers-wen"));
    assert!(!shell.drawn("pick-reviewers-mira"), "filtered by the query");
    shell.click_named("pick-reviewers-wen");
    let pr = forge_pr(&mut shell, &key);
    let wen = pr
        .reviewers
        .iter()
        .find(|r| r.user.login == "wen")
        .expect("wen was asked");
    assert_eq!(wen.state, ReviewState::Requested);
    assert!(shell.drawn("reviewer-wen"));

    shell.click("side-reviewers");
    shell.click_named("remove-reviewer-wen");
    let pr = forge_pr(&mut shell, &key);
    assert!(
        pr.reviewers.iter().all(|r| r.user.login != "wen"),
        "removed"
    );
}

#[gpui::test]
fn asking_the_author_to_review_is_disabled_with_the_reason(cx: &mut TestAppContext) {
    // #1297 is the viewer's own GitHub pull request.
    let mut shell = open(1297, cx);
    let key = shell.key(1297);
    let before = forge_pr(&mut shell, &key).reviewers;
    shell.click("picker-reviewers");
    assert!(shell.drawn("pick-reviewers-cora"));
    shell.click_named("pick-reviewers-cora");
    assert_eq!(forge_pr(&mut shell, &key).reviewers, before, "not sent");
    let session = shell.session();
    let error = shell.update(|_, cx| {
        session
            .read(cx)
            .pr(&key)
            .expect("open")
            .people
            .error
            .clone()
    });
    assert!(error.is_none(), "a disabled row does not act");
}

#[gpui::test]
fn assignees_and_labels_toggle_from_their_pickers(cx: &mut TestAppContext) {
    let mut shell = open(1297, cx);
    let key = shell.key(1297);
    shell.click("picker-assignees");
    shell.click_named("pick-assignees-mira");
    let logins = |shell: &mut ShellWindow| -> Vec<String> {
        forge_pr(shell, &key)
            .assignees
            .iter()
            .map(|u| u.login.clone())
            .collect()
    };
    assert!(logins(&mut shell).contains(&"mira".to_owned()));
    shell.click_named("pick-assignees-mira");
    assert!(
        !logins(&mut shell).contains(&"mira".to_owned()),
        "and off again"
    );

    shell.click("picker-labels");
    assert!(shell.drawn("pick-label-bug"));
    shell.type_text("doc");
    assert!(shell.drawn("pick-label-documentation"));
    assert!(!shell.drawn("pick-label-bug"), "the search filters");
    let had = forge_pr(&mut shell, &key)
        .labels
        .iter()
        .any(|l| l.name == "documentation");
    shell.click_named("pick-label-documentation");
    let has = forge_pr(&mut shell, &key)
        .labels
        .iter()
        .any(|l| l.name == "documentation");
    assert_ne!(had, has, "toggled");
    assert_eq!(has, shell.drawn("label-documentation"));
}

/// The first open GitHub fixture as a fake forge's pull request, open in
/// the Conversation.
fn live(cx: &mut TestAppContext, scratch: &Scratch) -> (ShellWindow, Arc<FakeForge>, PrKey) {
    let fixtures = Fixtures::load();
    let source = fixtures
        .inbox
        .prs
        .iter()
        .find(|pr| pr.forge == ForgeKind::GitHub && pr.state == PrState::Open)
        .map(|pr| pr.key.clone())
        .expect("an open GitHub fixture");
    let pr = fixtures.forge_pr(&source).expect("its forge record");
    let fake = FakeForge::with_pull_request(
        ForgeKind::GitHub,
        &pr.repo.host,
        Vec::new(),
        Some(pr.clone()),
    );
    let services = fake_services(
        scratch,
        vec![fake.clone() as Arc<dyn forge::Forge>],
        std::slice::from_ref(&pr.repo),
    );
    let mut shell = open_shell(
        cx,
        Options {
            theme: flat(),
            ..Options::default()
        },
    );
    let key = PrKey::of(&pr);
    go_live(&mut shell, services, vec![pr], ForgeKind::GitHub);
    let session = shell.session();
    let show = key.clone();
    shell.update(|_, cx| {
        session.update(cx, |session, cx| {
            session.show(&show, PanelKind::Conversation, OpenMode::Permanent, cx);
        });
    });
    shell.settle();
    (shell, fake, key)
}

#[gpui::test]
fn picker_writes_go_through_the_forge_and_patch_the_pull_request(cx: &mut TestAppContext) {
    let scratch = Scratch::new("people-live");
    let (mut shell, fake, key) = live(cx, &scratch);
    shell.click("picker-reviewers");
    shell.settle_until("the collaborators", |_| fake.queries() == [""]);
    shell.type_text("p");
    shell.type_text("r");
    shell.advance(SEARCH_DEBOUNCE / 2);
    shell.type_text("a");
    shell.advance(SEARCH_DEBOUNCE);
    shell.settle_until("the search", |_| fake.queries().len() == 2);
    assert_eq!(fake.queries(), ["", "pra"], "one search after the pause");
    let panel = conversation(&mut shell);
    let side = shell.update(|_, cx| panel.read(cx).side_column().clone());
    let lists = |login: &'static str| {
        let side = side.clone();
        move |cx: &mut gpui::App| matches!(side.read(cx).people(), Load::Ready(users) if users.iter().any(|u| u.login == login))
    };
    shell.settle_until("the people", lists("prakash"));
    assert!(shell.drawn("pick-reviewers-prakash"));
    assert!(!shell.drawn("pick-reviewers-dara"));
    shell.click_named("pick-reviewers-prakash");
    let session = shell.session();
    let asked = key.clone();
    shell.settle_until("the review request", |cx| {
        session
            .read(cx)
            .pr(&asked)
            .and_then(|open| open.forge_pr())
            .is_some_and(|pr| pr.reviewers.iter().any(|r| r.user.login == "prakash"))
    });
    assert!(
        fake.retries()
            .iter()
            .any(|sent| sent == &format!("request {} prakash", key.number)),
        "{:?}",
        fake.retries()
    );
    assert!(shell.drawn("reviewer-prakash"));

    // A refusal shows under its section.
    shell.click("picker-reviewers");
    shell.click("picker-reviewers");
    shell.type_text("gh");
    shell.advance(SEARCH_DEBOUNCE);
    shell.settle_until("ghost listed", lists("ghost"));
    shell.click_named("pick-reviewers-ghost");
    let failed = key.clone();
    shell.settle_until("the refusal", |cx| {
        session
            .read(cx)
            .pr(&failed)
            .is_some_and(|open| open.people.error.is_some())
    });
    assert!(shell.drawn("side-error-reviewers"));

    shell.click("picker-labels");
    let labels = side.clone();
    shell.settle_until("the labels", move |cx| {
        matches!(labels.read(cx).labels(), Load::Ready(_))
    });
    shell.click_named("pick-label-docs");
    let labeled = key.clone();
    shell.settle_until("the label write", |cx| {
        session
            .read(cx)
            .pr(&labeled)
            .and_then(|open| open.forge_pr())
            .is_some_and(|pr| pr.labels.iter().any(|l| l.name == "docs"))
    });
    assert!(shell.drawn("label-docs"));
}

#[gpui::test]
fn header_links_show_their_target_and_open_on_the_web(cx: &mut TestAppContext) {
    let mut shell = open(42, cx);
    let base = "https://forgejo.example.com/cora/review/src/branch/main";
    let link = format!("web-link:{base}");
    assert!(shell.drawn(&link), "the base branch is a link");
    let bounds = shell.bounds_of(String::leak(link.clone()));
    shell
        .cx
        .simulate_mouse_move(bounds.center(), None, Modifiers::none());
    shell.advance(Duration::from_secs(2));
    assert!(
        shell.drawn(&format!("link-tooltip:{base}")),
        "hovering shows where it goes"
    );
    shell.click_named(&link);
    assert_eq!(shell.cx.opened_url().as_deref(), Some(base));
    for url in [
        "https://forgejo.example.com/cora/review",
        "https://forgejo.example.com/cora/review/pulls/42",
        "https://forgejo.example.com/mira",
    ] {
        assert!(shell.drawn(&format!("web-link:{url}")), "{url} is a link");
    }
}

#[gpui::test]
fn references_to_known_pull_requests_open_in_the_app(cx: &mut TestAppContext) {
    let mut shell = open(39, cx);
    let target = shell.key(38);
    shell.update(|_, cx| {
        app::references::open_link("https://forgejo.example.com/cora/review/pulls/38", cx);
    });
    shell.settle();
    assert_eq!(shell.focused(), Some(38), "#38 opened here");
    let session = shell.session();
    assert!(shell.update(|_, cx| session.read(cx).is_preview(&target)));
    assert_eq!(shell.cx.opened_url(), None, "not in the browser");

    shell.update(|_, cx| app::references::open_link("file:///C:/Windows/win.ini", cx));
    assert_eq!(shell.cx.opened_url(), None, "never a file");
    shell.update(|_, cx| {
        app::references::open_link("https://evil.example/cora/review/pulls/38", cx);
    });
    assert_eq!(
        shell.cx.opened_url().as_deref(),
        Some("https://evil.example/cora/review/pulls/38"),
        "another host is only a web page"
    );
}

#[gpui::test]
fn an_issue_reference_opens_here_when_the_forge_says_it_is_a_pull_request(cx: &mut TestAppContext) {
    let scratch = Scratch::new("refs-live");
    let (mut shell, fake, key) = live(cx, &scratch);
    let mut other = forge_pr(&mut shell, &key);
    other.number = forge::model::PrNumber(4242);
    other.title = "Another one".to_owned();
    fake.add_pull_request(other);
    let base = format!("https://{}/{}", key.repo.host, key.repo.path());
    shell.update(|_, cx| app::references::open_link(&format!("{base}/issues/4242"), cx));
    shell.settle_until("the forge's answer", |cx| {
        app::session::Session::global(cx)
            .read(cx)
            .open_key()
            .is_some_and(|open| open.number.0 == 4242)
    });
    assert_eq!(shell.cx.opened_url(), None);

    shell.update(|_, cx| app::references::open_link(&format!("{base}/issues/7"), cx));
    let url = format!("{base}/issues/7");
    shell.settle_until("the browser", |_| true);
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while shell.cx.opened_url().as_deref() != Some(url.as_str()) {
        assert!(
            std::time::Instant::now() < deadline,
            "an issue opens on the web"
        );
        shell.settle();
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[gpui::test]
fn inbox_links_open_on_the_web_without_opening_the_row(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    let url = "https://forgejo.example.com/cora/review";
    assert!(
        shell.drawn(&format!("web-link:{url}")),
        "the repository is a link"
    );
    shell.click_named(&format!("web-link:{url}"));
    assert_eq!(shell.cx.opened_url().as_deref(), Some(url));
    assert!(shell.open_numbers().is_empty(), "the row did not open");
}

#[gpui::test]
fn teams_are_asked_where_the_forge_takes_team_requests(cx: &mut TestAppContext) {
    // GitHub takes team review requests.
    let mut shell = open(1297, cx);
    let key = shell.key(1297);
    shell.click("picker-reviewers");
    shell.type_text("acme/core");
    shell.advance(SEARCH_DEBOUNCE);
    shell.click("pick-team");
    let pr = forge_pr(&mut shell, &key);
    let team = pr
        .reviewers
        .iter()
        .find(|r| r.user.login == "acme/core")
        .expect("the team was asked");
    assert!(team.is_team());
    assert!(shell.drawn("reviewer-acme/core"));

    // GitLab has none: the row is there, disabled, and sends nothing.
    shell.show(412, PanelKind::Conversation, OpenMode::Permanent);
    let gitlab = shell.key(412);
    let before = forge_pr(&mut shell, &gitlab).reviewers;
    shell.click("picker-reviewers");
    shell.type_text("acme/core");
    shell.advance(SEARCH_DEBOUNCE);
    assert!(shell.drawn("pick-team"));
    shell.click("pick-team");
    assert_eq!(forge_pr(&mut shell, &gitlab).reviewers, before);
}

/// Draws frames over `by` of real time: the autoscroll gesture runs on the
/// wall clock, not the test clock.
fn frames_over(shell: &mut ShellWindow, by: Duration) {
    for _ in 0..4 {
        std::thread::sleep(by / 4);
        shell.redraw();
    }
}

#[gpui::test]
fn middle_press_and_drag_autoscrolls_the_conversation_until_escape(cx: &mut TestAppContext) {
    let mut shell = open(41, cx);
    let panel = conversation(&mut shell);
    let body = shell.bounds_of("conversation-body");
    let scrolled = |shell: &mut ShellWindow| shell.update(|_, cx| panel.read(cx).scrolled());
    let running = |shell: &mut ShellWindow| shell.update(|_, cx| panel.read(cx).is_autoscrolling());
    assert_eq!(scrolled(&mut shell), 0.0);

    let anchor = point(body.left() + body.size.width * 0.4, body.top() + px(60.0));
    let below = point(anchor.x, anchor.y + px(150.0));
    let none = Modifiers::none();
    shell.cx.simulate_mouse_move(anchor, None, none);
    shell
        .cx
        .simulate_mouse_down(anchor, MouseButton::Middle, none);
    shell.settle();
    assert!(running(&mut shell), "the press starts autoscroll");
    shell
        .cx
        .simulate_mouse_move(below, MouseButton::Middle, none);
    frames_over(&mut shell, Duration::from_millis(80));
    let first = scrolled(&mut shell);
    assert!(first > 0.0, "dragging down scrolls down, at {first}");
    frames_over(&mut shell, Duration::from_millis(80));
    assert!(
        scrolled(&mut shell) > first,
        "it keeps scrolling while held"
    );

    // Escape stops it where it is.
    shell.keys("escape");
    assert!(!running(&mut shell));
    let stopped = scrolled(&mut shell);
    frames_over(&mut shell, Duration::from_millis(80));
    assert_eq!(scrolled(&mut shell), stopped);

    // Letting go of a held press ends it as well.
    shell.cx.simulate_mouse_up(below, MouseButton::Middle, none);
    shell.settle();
    assert!(!running(&mut shell));
}
