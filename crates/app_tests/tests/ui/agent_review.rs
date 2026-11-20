//! "Review with agent": without a workspace for the repository the button
//! opens the settings on a new row for it; with one, it makes the worktree,
//! writes the context file and starts Claude in a terminal whose command
//! line holds the safe flags, the review's MCP server and the prompt (never
//! the pull request's text); a second click brings that terminal forward.
//! A tool call through the MCP server becomes a draft from Claude, which the
//! button's menu lists.
//!
//! The button's menu lists the repository's workspaces and saves the one
//! chosen as its preferred; the launch opens its tab at once with numbered
//! steps that turn into checks, then the terminal in their place; a failed
//! step shows its error and Retry; the editor says whether each workspace
//! is saved, never keeping one for a run only.
//!
//! No real `claude` runs: the workspace's command is a stand-in at an
//! absolute path that waits and ends, and worktree jobs are stand-ins too
//! where a test steps through them.

use std::io::{Read as _, Write as _};
use std::net::{Ipv4Addr, TcpStream};
use std::path::PathBuf;
use std::time::Duration;

use app::agent::{ConfigCheck, SAFE_CLAUDE_FLAGS};
use app::checklist::StepState;
use app::claude_review::AgentReview;
use app::claude_review::command::{ADDITIONAL_CLAUDE_MD, CONTEXT_FILE};
use app::claude_review::mcp::TOOL_RULE;
use app::config::{Config, WorkspaceEntry};
use app::model::PrKey;
use app::panels::PanelKind;
use app::session::OpenMode;
use app::settings_panel::{DirField, RowState, SettingsPanel, row_status_text};
use app::terminal_panel::TerminalPanel;
use app::terminals::{Outcome, Prepared, TerminalSpec, Terminals, WorktreeJob, WorktreeState};
use app::workspaces::{SaveState, Workspaces};
use app::worktrees::{Step, WorktreeInfo};
use app::writing::FROM_CLAUDE;
use gpui::{Entity, SharedString, TestAppContext};
use gpui_kit::component::dock::{DockPlacement, PanelId};

use crate::harness::{Options, Scratch, ShellWindow, open_shell};

/// Opens `number`'s diff and waits for it.
fn open_diff(shell: &mut ShellWindow, number: u64) -> PrKey {
    shell.show(number, PanelKind::Diff, OpenMode::Permanent);
    let key = shell.key(number);
    let session = shell.session();
    shell.settle_until("the diff", |cx| {
        session
            .read(cx)
            .pr(&key)
            .is_some_and(|open| open.diff.is_some())
    });
    key
}

fn terminal_panels(shell: &mut ShellWindow) -> Vec<(PanelId, Entity<TerminalPanel>)> {
    let dock = shell.dock();
    shell.update(|_, cx| {
        let dock = dock.read(cx);
        dock.layout(DockPlacement::Center)
            .map(|tree| {
                tree.panels()
                    .filter_map(|id| {
                        let panel = dock.panel(id)?.view().downcast::<TerminalPanel>().ok()?;
                        Some((id, panel))
                    })
                    .collect()
            })
            .unwrap_or_default()
    })
}

fn close_terminals(shell: &mut ShellWindow) {
    for (_, panel) in terminal_panels(shell) {
        if let Some(view) = shell.update(|_, cx| panel.read(cx).view().cloned()) {
            shell.update(|_, cx| view.update(cx, |view, cx| view.close(cx)));
        }
    }
}

/// A stand-in `claude` at an absolute path: it waits a while, then ends.
fn fake_claude(scratch: &Scratch) -> String {
    let dir = scratch.join("bin");
    std::fs::create_dir_all(&dir).expect("create the bin dir");
    let program = if cfg!(windows) {
        let program = dir.join("claude.cmd");
        std::fs::write(&program, "@ping -n 30 127.0.0.1 >nul\r\n").expect("write the stand-in");
        program
    } else {
        let program = dir.join("claude");
        std::fs::write(&program, "#!/bin/sh\nsleep 30\n").expect("write the stand-in");
        make_executable(&program);
        program
    };
    std::path::absolute(program)
        .expect("an absolute path")
        .to_string_lossy()
        .into_owned()
}

#[cfg(unix)]
fn make_executable(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .expect("mark the stand-in executable");
}

#[cfg(not(unix))]
fn make_executable(_: &std::path::Path) {}

/// A workspace for `key`'s repository on this PC, running `command`, with
/// worktree folders under `scratch`.
fn install_workspace(shell: &mut ShellWindow, key: &PrKey, scratch: &Scratch, command: &str) {
    let entry = WorkspaceEntry {
        repo: key.repo.path(),
        machine: "local".into(),
        path: scratch.join("clone").to_string_lossy().into_owned(),
        command: Some(command.to_owned()),
        ..WorkspaceEntry::default()
    };
    let root = scratch.join("worktrees");
    let file = scratch.join("config.toml");
    shell.update(|_, cx| {
        let workspaces = Workspaces::global(cx).expect("workspaces are installed");
        workspaces.update(cx, |workspaces, cx| {
            workspaces.set_file(Ok(file), cx);
            workspaces.loaded(vec![entry], cx);
        });
        let terminals = Terminals::global(cx).expect("terminals are installed");
        terminals.update(cx, |terminals, _| terminals.set_fixture_root(root));
    });
}

/// Starts the review of `key` and waits for its terminal.
fn start_review(shell: &mut ShellWindow, key: &PrKey) -> (Entity<TerminalPanel>, TerminalSpec) {
    shell.click_element(SharedString::from("review-with-agent"));
    let terminals = shell.update(|_, cx| Terminals::global(cx).expect("terminals"));
    shell.settle_until("the worktree", |cx| {
        matches!(
            terminals.read(cx).worktree(key),
            Some(WorktreeState::Ready(_) | WorktreeState::Failed { .. })
        )
    });
    let state = shell.update(|_, cx| terminals.read(cx).worktree(key).cloned());
    assert!(matches!(state, Some(WorktreeState::Ready(_))), "{state:?}");
    shell.advance(Duration::from_millis(50));
    let panels = terminal_panels(shell);
    assert_eq!(panels.len(), 1, "one terminal");
    let panel = panels[0].1.clone();
    let spec = shell.update(|_, cx| panel.read(cx).spec().clone());
    (panel, spec)
}

/// Sends one JSON-RPC message to the review's MCP server from another
/// thread, as Claude does, and returns the reply.
fn call_tool(
    shell: &mut ShellWindow,
    port: u16,
    token: &str,
    arguments: serde_json::Value,
) -> serde_json::Value {
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": "add_review_comment", "arguments": arguments }
    })
    .to_string();
    let request = format!(
        "POST /mcp/{token} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).expect("connect");
        stream.write_all(request.as_bytes()).expect("send");
        let mut text = String::new();
        stream.read_to_string(&mut text).expect("read");
        let _ = tx.send(text);
    });
    let mut reply = None;
    shell.settle_until("the tool call", |_| {
        reply = reply.take().or_else(|| rx.try_recv().ok());
        reply.is_some()
    });
    let text = reply.expect("a reply");
    let (_, body) = text.split_once("\r\n\r\n").expect("an HTTP response");
    serde_json::from_str(body).expect("JSON")
}

#[gpui::test]
fn without_a_workspace_the_settings_open_on_a_new_row(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    let key = open_diff(&mut shell, 41);

    shell.click_element(SharedString::from("review-with-agent"));
    shell.settle();
    assert!(terminal_panels(&mut shell).is_empty(), "nothing started");
    assert!(shell.drawn("workspaces-section"), "the settings show it");
    let rows = shell.update(|_, cx| {
        SettingsPanel::existing(cx)
            .expect("the settings panel")
            .read(cx)
            .workspace_entries(cx)
    });
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].repo, key.repo.path());
    assert_eq!(rows[0].host.as_deref(), Some(key.repo.host.as_str()));
    assert_eq!(rows[0].machine, "local");
    assert!(shell.drawn("workspace-row-0"));

    // An empty directory cannot be saved, and the row says why.
    let states = shell.update(|_, cx| {
        SettingsPanel::existing(cx)
            .expect("the settings panel")
            .read(cx)
            .workspace_states(cx)
    });
    assert!(
        matches!(&states[0], RowState::Invalid(why) if why.contains("directory")),
        "{states:?}"
    );
    shell.click_element(("workspace-save", 0usize));
    assert!(shell.update(|_, cx| {
        Workspaces::global(cx)
            .expect("workspaces")
            .read(cx)
            .saved()
            .is_empty()
    }));
}

#[gpui::test]
fn with_a_workspace_claude_starts_in_a_terminal_and_a_second_click_focuses_it(
    cx: &mut TestAppContext,
) {
    let scratch = Scratch::new("agent-review");
    let claude = fake_claude(&scratch);
    let mut shell = open_shell(cx, Options::default());
    let key = open_diff(&mut shell, 41);
    install_workspace(&mut shell, &key, &scratch, &claude);

    let (panel, spec) = start_review(&mut shell, &key);
    let agent = spec.agent.clone().expect("an agent launch");
    let argv = &agent.argv;
    assert_eq!(argv[0], claude, "the workspace's command");
    for flag in SAFE_CLAUDE_FLAGS {
        assert!(argv.iter().any(|arg| arg == flag), "{flag} in {argv:?}");
    }
    let config = argv
        .windows(2)
        .find(|w| w[0] == "--mcp-config")
        .map(|w| w[1].clone())
        .expect("the MCP config");
    let review = shell.update(|_, cx| AgentReview::global(cx).expect("installed"));
    let (port, token) = shell.update(|_, cx| {
        let review = review.read(cx);
        (
            review.port().expect("the server runs"),
            review.token(&key).expect("a token").as_str().to_owned(),
        )
    });
    // The token is in a file beside the worktree, never on the command line.
    assert!(config.ends_with(".json"), "{config}");
    assert!(!config.contains("pr-41"), "outside the worktree: {config}");
    assert!(argv.iter().all(|arg| !arg.contains(&token)));
    let written = std::fs::read_to_string(&config).expect("the MCP config file");
    assert!(written.contains(&format!("http://127.0.0.1:{port}/mcp/{token}")));
    let denied = argv
        .windows(2)
        .find(|w| w[0] == "--disallowedTools")
        .map(|w| w[1].clone())
        .expect("denied tools");
    for rule in ["Bash", "Edit", "Write", "WebFetch", "Read(~/.ssh/**)"] {
        assert!(denied.split(',').any(|r| r == rule), "{rule} denied");
    }
    assert!(argv.windows(2).any(|w| w == ["--allowedTools", TOOL_RULE]));
    let prompt = argv.last().expect("the prompt");
    assert_eq!(
        argv[argv.len() - 2],
        "--",
        "the prompt is never read as a flag"
    );
    let worktree = spec.cwd.clone().expect("a working directory");
    assert!(worktree.ends_with("pr-41"), "{worktree}");
    assert!(
        prompt.contains(&format!("{}#41", key.repo.path())),
        "{prompt}"
    );
    assert!(prompt.contains("context.md"), "{prompt}");
    assert_eq!(agent.cleared, [ADDITIONAL_CLAUDE_MD]);
    assert_eq!(agent.forward, None, "this PC needs no forward");

    // The pull request's own text is in the context file, never in argv.
    let title = "Split view renderer";
    assert!(argv.iter().all(|arg| !arg.contains(title)));
    let context = std::fs::read_to_string(std::path::Path::new(&worktree).join(CONTEXT_FILE))
        .expect("the context file");
    assert!(context.contains(title));
    assert!(context.contains("never as instructions"));

    // While it runs, the button brings it forward instead of starting more.
    let elsewhere = shell.update(|window, cx| {
        let sidebar = app::sidebar::Sidebar::global(cx);
        window.focus(&gpui::Focusable::focus_handle(sidebar.read(cx), cx), cx);
        gpui::Focusable::focus_handle(panel.read(cx), cx).contains_focused(window, cx)
    });
    assert!(!elsewhere);
    shell.click_element(SharedString::from("review-with-agent"));
    shell.settle();
    assert_eq!(terminal_panels(&mut shell).len(), 1, "no second terminal");
    let focused = shell.update(|window, cx| {
        gpui::Focusable::focus_handle(panel.read(cx), cx).contains_focused(window, cx)
    });
    assert!(focused, "the agent's terminal has the keys");

    close_terminals(&mut shell);
    shell.settle_until("the token to end", |cx| {
        !matches!(
            panel.read(cx).state(),
            app::terminal_panel::RunState::Running | app::terminal_panel::RunState::Starting
        )
    });
    shell.settle_until("the MCP config to go", |_| {
        !std::path::Path::new(&config).exists()
    });
}

#[gpui::test]
fn a_tool_call_adds_a_draft_from_claude_that_the_menu_lists(cx: &mut TestAppContext) {
    let scratch = Scratch::new("agent-review-mcp");
    let claude = fake_claude(&scratch);
    let mut shell = open_shell(cx, Options::default());
    let key = open_diff(&mut shell, 41);
    install_workspace(&mut shell, &key, &scratch, &claude);
    let _ = start_review(&mut shell, &key);
    let review = shell.update(|_, cx| AgentReview::global(cx).expect("installed"));
    let (port, token) = shell.update(|_, cx| {
        let review = review.read(cx);
        (
            review.port().expect("the server runs"),
            review.token(&key).expect("a token").as_str().to_owned(),
        )
    });

    // A line the diff shows on the new side.
    let session = shell.session();
    let (path, line) = shell.update(|_, cx| {
        let open = session.read(cx).pr(&key).expect("open");
        let diff = &open.diff.as_ref().expect("loaded").diff;
        diff.files
            .iter()
            .find_map(|file| {
                (0..file.new.line_count())
                    .find(|ix| file.shows(diff::Side::New, *ix))
                    .map(|ix| (file.path.to_string(), ix + 1))
            })
            .expect("a shown line")
    });

    let reply = call_tool(
        &mut shell,
        port,
        &token,
        serde_json::json!({ "path": path, "line": line, "body": "Check the bounds.\nMore." }),
    );
    assert_eq!(reply["result"]["isError"], false, "{reply}");
    let text = reply["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(text.contains(&format!("{path}:{line}")), "{text}");

    let missing = call_tool(
        &mut shell,
        port,
        &token,
        serde_json::json!({ "path": "not/in/the/diff.rs", "line": 1, "body": "x" }),
    );
    assert_eq!(missing["result"]["isError"], true);

    let writing = shell.writing();
    let drafts = shell.update(|_, cx| {
        let writing = writing.read(cx);
        writing
            .slots()
            .iter()
            .filter(|slot| writing.is_from_claude(&slot.anchor))
            .map(|slot| (slot.body.clone(), writing.place_label(&slot.anchor, None)))
            .collect::<Vec<_>>()
    });
    assert_eq!(drafts.len(), 1);
    assert_eq!(drafts[0].0, "Check the bounds.\nMore.");
    assert!(
        drafts[0]
            .1
            .as_ref()
            .is_some_and(|label| label.contains(FROM_CLAUDE))
    );

    // The button's menu lists it; "Add" queues it. The terminal's tab goes
    // first, so the diff's header has its full width.
    close_terminals(&mut shell);
    for (id, _) in terminal_panels(&mut shell) {
        let entity = shell.shell.clone();
        shell.update(|window, cx| entity.update(cx, |shell, cx| shell.close_panel(id, window, cx)));
    }
    shell.settle();
    assert!(shell.drawn("agent-pending-count"));
    shell.click("review-with-agent-menu");
    assert!(
        shell.drawn("agent-workspace-0"),
        "the repository's workspace"
    );
    assert!(shell.drawn("agent-pending-0"));
    assert!(shell.drawn("agent-workspace-settings"));
    shell.click("agent-add-all");
    shell.settle();
    shell.update(|_, cx| {
        let writing = writing.read(cx);
        assert_eq!(writing.queued().len(), 1, "added to the review");
        assert!(writing.sent().is_empty(), "nothing was posted");
    });
}

/// Workspaces for `key`'s repository on this PC and on the fixtures'
/// `buildbox`, and one for another repository, saved to `scratch`'s config.
fn install_two_workspaces(shell: &mut ShellWindow, key: &PrKey, scratch: &Scratch) -> PathBuf {
    let local = WorkspaceEntry {
        host: Some(key.repo.host.clone()),
        repo: key.repo.path(),
        machine: "local".into(),
        path: scratch.join("clone").to_string_lossy().into_owned(),
        ..WorkspaceEntry::default()
    };
    let remote = WorkspaceEntry {
        machine: "buildbox".into(),
        path: "~/src/widgets".into(),
        ..local.clone()
    };
    let other = WorkspaceEntry {
        repo: "someone/else".into(),
        ..local.clone()
    };
    let file = scratch.join("config.toml");
    let saved = file.clone();
    shell.update(|_, cx| {
        let workspaces = Workspaces::global(cx).expect("workspaces are installed");
        workspaces.update(cx, |workspaces, cx| {
            workspaces.set_file(Ok(saved), cx);
            workspaces.loaded(vec![local, remote, other], cx);
        });
    });
    file
}

fn launch_steps(shell: &mut ShellWindow, panel: &Entity<TerminalPanel>) -> Vec<(Step, StepState)> {
    shell.update(|_, cx| {
        panel
            .read(cx)
            .checklist()
            .map(|list| list.steps().to_vec())
            .unwrap_or_default()
    })
}

#[gpui::test]
fn the_menu_lists_the_repositorys_workspaces_and_saves_the_one_chosen(cx: &mut TestAppContext) {
    let scratch = Scratch::new("agent-review-menu");
    let mut shell = open_shell(cx, Options::default());
    let key = open_diff(&mut shell, 41);
    let file = install_two_workspaces(&mut shell, &key, &scratch);
    shell.settle();

    // Two match and neither is preferred: the button asks in its menu
    // instead of starting one of them.
    shell.click("review-with-agent");
    assert!(terminal_panels(&mut shell).is_empty(), "nothing started");
    assert!(shell.drawn("agent-workspace-0"), "the menu asks");
    assert!(shell.drawn("agent-workspace-1"));
    assert!(!shell.drawn("agent-workspace-2"), "only this repository's");
    assert!(shell.drawn("agent-workspace-settings"));
    assert!(!shell.drawn("worktree-state"), "no status line");

    // Picking one there saves it as the repository's and starts it: on the
    // invented buildbox, whose step fails with Retry.
    shell.click("agent-workspace-1");
    let workspaces = shell.update(|_, cx| Workspaces::global(cx).expect("installed"));
    shell.settle_until("the preference to be written", |cx| {
        *workspaces.read(cx).state() == SaveState::Saved
    });
    let written = Config::parse(&std::fs::read_to_string(&file).expect("the config file"))
        .expect("valid config");
    let preferred: Vec<bool> = written.workspaces.iter().map(|w| w.preferred).collect();
    assert_eq!(preferred, [false, true, false]);
    let panels = terminal_panels(&mut shell);
    assert_eq!(panels.len(), 1, "its tab opened");
    let panel = panels[0].1.clone();
    shell.settle_until("the launch to fail", |cx| {
        panel
            .read(cx)
            .checklist()
            .is_some_and(|list| list.error().is_some())
    });
    let error = shell.update(|_, cx| {
        panel
            .read(cx)
            .checklist()
            .and_then(|list| list.error().map(str::to_owned))
    });
    assert!(
        error.as_deref().is_some_and(|e| e.contains("buildbox")),
        "{error:?}"
    );
    assert!(shell.element("launch-retry").is_some(), "Retry");
    assert!(!shell.drawn("worktree-state"), "no status line");

    // The chevron's menu checks the preferred one; choosing the other only
    // moves the preference.
    let entity = shell.shell.clone();
    let id = panels[0].0;
    shell.update(|window, cx| entity.update(cx, |shell, cx| shell.close_panel(id, window, cx)));
    shell.settle();
    shell.click("review-with-agent-menu");
    assert!(shell.drawn("agent-workspace-0"));
    shell.click("agent-workspace-0");
    shell.settle_until("the preference to be written", |cx| {
        let workspaces = workspaces.read(cx);
        *workspaces.state() == SaveState::Saved
            && workspaces
                .saved()
                .first()
                .is_some_and(|w| w.entry.preferred)
    });
    assert!(terminal_panels(&mut shell).is_empty(), "nothing started");
    let written = Config::parse(&std::fs::read_to_string(&file).expect("the config file"))
        .expect("valid config");
    let preferred: Vec<bool> = written.workspaces.iter().map(|w| w.preferred).collect();
    assert_eq!(preferred, [true, false, false]);
}

#[gpui::test]
fn the_main_button_uses_the_preferred_workspace(cx: &mut TestAppContext) {
    let scratch = Scratch::new("agent-review-preferred");
    let mut shell = open_shell(cx, Options::default());
    let key = open_diff(&mut shell, 41);
    install_two_workspaces(&mut shell, &key, &scratch);
    shell.update(|_, cx| {
        let workspaces = Workspaces::global(cx).expect("installed");
        let id = workspaces.read(cx).saved()[1].id;
        workspaces.update(cx, |workspaces, cx| workspaces.prefer(&key.repo, id, cx));
    });
    let workspaces = shell.update(|_, cx| Workspaces::global(cx).expect("installed"));
    shell.settle_until("the preference", |cx| {
        *workspaces.read(cx).state() == SaveState::Saved
    });

    shell.click_element(SharedString::from("review-with-agent"));
    assert!(!shell.drawn("agent-workspace-0"), "no menu: it knows which");
    let panels = terminal_panels(&mut shell);
    assert_eq!(panels.len(), 1);
    let steps = shell.update(|_, cx| panels[0].1.read(cx).checklist().is_some());
    assert!(steps, "the tab shows the steps");
    let state = shell.update(|_, cx| {
        Terminals::global(cx)
            .expect("terminals")
            .read(cx)
            .worktree(&key)
            .cloned()
    });
    assert!(
        matches!(&state, Some(WorktreeState::Failed { machine, .. }) if machine == "buildbox"),
        "the preferred workspace's machine: {state:?}"
    );
}

/// A worktree job that reports `steps` one at a time, each when `go` says,
/// then ends with `end`.
fn gated_job(
    steps: Vec<Step>,
    go: std::sync::mpsc::Receiver<()>,
    end: Result<Outcome, String>,
) -> WorktreeJob {
    Box::new(move |progress: app::worktrees::Progress<'_>| {
        for step in steps {
            go.recv().map_err(|_| "the test ended".to_owned())?;
            progress(step);
        }
        go.recv().map_err(|_| "the test ended".to_owned())?;
        end
    })
}

/// A prepared worktree under `scratch`, with an MCP config file path.
fn prepared_in(scratch: &Scratch) -> Outcome {
    let path = scratch.join("worktree");
    std::fs::create_dir_all(&path).expect("the worktree folder");
    Prepared {
        info: WorktreeInfo {
            machine: app::machines::THIS_PC.to_owned(),
            local: true,
            path: std::path::absolute(&path)
                .expect("absolute")
                .to_string_lossy()
                .into_owned(),
            head: "abc1234".into(),
            note: None,
        },
        check: ConfigCheck::Clean,
        links: Ok(Vec::new()),
        mcp_file: Some(scratch.join("mcp.json").to_string_lossy().into_owned()),
    }
    .into()
}

#[gpui::test]
fn the_launch_tab_opens_at_once_and_ticks_off_its_numbered_steps(cx: &mut TestAppContext) {
    let scratch = Scratch::new("agent-review-steps");
    let claude = fake_claude(&scratch);
    let mut shell = open_shell(cx, Options::default());
    let key = open_diff(&mut shell, 41);
    install_workspace(&mut shell, &key, &scratch, &claude);
    let (go, gate) = std::sync::mpsc::channel();
    let job = gated_job(
        vec![Step::Fetch, Step::Worktree, Step::Config, Step::Context],
        gate,
        Ok(prepared_in(&scratch)),
    );
    shell.update(|_, cx| {
        let terminals = Terminals::global(cx).expect("terminals");
        terminals.update(cx, |terminals, _| terminals.queue_job(job));
    });

    shell.click_element(SharedString::from("review-with-agent"));
    let panels = terminal_panels(&mut shell);
    assert_eq!(panels.len(), 1, "the tab opened before any work");
    let panel = panels[0].1.clone();
    assert!(shell.drawn("launch-steps"));
    assert!(shell.drawn("launch-step-4"), "five numbered steps");
    assert!(
        !shell.drawn("worktree-state"),
        "no status line in the header"
    );
    let order: Vec<Step> = launch_steps(&mut shell, &panel)
        .into_iter()
        .map(|(step, _)| step)
        .collect();
    assert_eq!(
        order,
        [
            Step::Fetch,
            Step::Worktree,
            Step::Config,
            Step::Context,
            Step::StartAgent
        ]
    );

    for (at, step) in order[..4].iter().enumerate() {
        go.send(()).expect("the job waits");
        shell.settle_until("the next step", |cx| {
            panel
                .read(cx)
                .checklist()
                .is_some_and(|list| list.steps()[at] == (*step, StepState::Running))
        });
        let steps = launch_steps(&mut shell, &panel);
        for (before, (_, state)) in steps[..at].iter().enumerate() {
            assert_eq!(*state, StepState::Done, "step {before} is checked");
        }
        for (_, state) in &steps[at + 1..] {
            assert_eq!(*state, StepState::Waiting);
        }
    }

    // The terminal takes the steps' place in the same tab.
    go.send(()).expect("the job waits");
    shell.settle_until("the agent to start", |cx| {
        panel.read(cx).checklist().is_none()
    });
    assert_eq!(terminal_panels(&mut shell).len(), 1, "the same tab");
    let spec = shell.update(|_, cx| panel.read(cx).spec().clone());
    assert_eq!(
        spec.agent.as_ref().map(|agent| agent.argv[0].clone()),
        Some(claude)
    );
    assert!(!shell.drawn("launch-steps"));
    close_terminals(&mut shell);
}

#[gpui::test]
fn a_failed_step_shows_its_error_and_retry_starts_again_in_the_same_tab(cx: &mut TestAppContext) {
    let scratch = Scratch::new("agent-review-retry");
    let claude = fake_claude(&scratch);
    let mut shell = open_shell(cx, Options::default());
    let key = open_diff(&mut shell, 41);
    install_workspace(&mut shell, &key, &scratch, &claude);
    let (go, gate) = std::sync::mpsc::channel();
    let failing = gated_job(
        vec![Step::Fetch, Step::Worktree],
        gate,
        Err("the disk is full".into()),
    );
    let (again, gate) = std::sync::mpsc::channel();
    let working = gated_job(Vec::new(), gate, Ok(prepared_in(&scratch)));
    shell.update(|_, cx| {
        let terminals = Terminals::global(cx).expect("terminals");
        terminals.update(cx, |terminals, _| {
            terminals.queue_job(failing);
            terminals.queue_job(working);
        });
    });

    shell.click_element(SharedString::from("review-with-agent"));
    let panel = terminal_panels(&mut shell)[0].1.clone();
    for _ in 0..3 {
        go.send(()).expect("the job waits");
    }
    shell.settle_until("the failure", |cx| {
        panel
            .read(cx)
            .checklist()
            .is_some_and(|list| list.error().is_some())
    });
    let steps = launch_steps(&mut shell, &panel);
    assert_eq!(steps[0], (Step::Fetch, StepState::Done));
    assert_eq!(
        steps[1],
        (Step::Worktree, StepState::Failed("the disk is full".into()))
    );
    assert!(shell.element("launch-retry").is_some(), "Retry");
    assert!(
        !shell.drawn("worktree-state"),
        "no status line in the header"
    );

    // A second click on the button while it failed starts it over too; the
    // Retry button does the same, in the same tab.
    shell.click_element("launch-retry");
    assert_eq!(terminal_panels(&mut shell).len(), 1, "the same tab");
    assert!(
        launch_steps(&mut shell, &panel)
            .iter()
            .all(|(_, state)| !matches!(state, StepState::Failed(_))),
        "the steps start over"
    );
    again.send(()).expect("the job waits");
    shell.settle_until("the agent to start", |cx| {
        panel.read(cx).checklist().is_none()
    });
    assert_eq!(terminal_panels(&mut shell).len(), 1);
    close_terminals(&mut shell);
}

#[gpui::test]
fn the_editor_says_each_workspace_is_saved_and_never_keeps_one_for_a_run(cx: &mut TestAppContext) {
    let scratch = Scratch::new("agent-review-editor");
    let mut shell = open_shell(cx, Options::default());
    let key = open_diff(&mut shell, 41);
    install_workspace(&mut shell, &key, &scratch, "claude");
    shell.keys("ctrl-,");
    let panel = shell
        .update(|_, cx| SettingsPanel::existing(cx))
        .expect("the settings are open");
    shell.update(|window, cx| panel.update(cx, |panel, cx| panel.reveal_workspaces(window, cx)));
    shell.settle();
    let lines =
        |shell: &mut ShellWindow| shell.update(|_, cx| panel.read(cx).workspace_status_lines(cx));
    let states = shell.update(|_, cx| panel.read(cx).workspace_states(cx));
    assert_eq!(states, [RowState::Saved]);
    assert!(shell.drawn("workspace-state-0"));
    assert!(shell.drawn("workspace-file"));

    // A new row for another repository: unsaved until its Save.
    shell.update(|_, cx| {
        let workspaces = Workspaces::global(cx).expect("installed");
        workspaces.update(cx, |workspaces, cx| {
            workspaces.request(&app::model::RepoRef::new("github.com", "o", "new"), cx);
        });
    });
    shell.settle();
    shell.click(String::leak(format!(
        "{}-input-1",
        DirField::Repository.id()
    )));
    shell.type_text("C:/src/new");
    let states = shell.update(|_, cx| panel.read(cx).workspace_states(cx));
    assert_eq!(states, [RowState::Saved, RowState::Unsaved]);
    shell.click_element(("workspace-save", 1usize));
    let workspaces = shell.update(|_, cx| Workspaces::global(cx).expect("installed"));
    shell.settle_until("the save", |cx| {
        *workspaces.read(cx).state() == SaveState::Saved
    });
    let states = shell.update(|_, cx| panel.read(cx).workspace_states(cx));
    assert_eq!(states, [RowState::Saved, RowState::Saved]);
    let text = std::fs::read_to_string(scratch.join("config.toml")).expect("written");
    assert!(text.contains("C:/src/new"), "{text}");

    // A file that cannot be written says where and why, and nothing
    // pretends to be saved.
    let blocker = scratch.join("blocker");
    std::fs::write(&blocker, "a file, not a folder").expect("write");
    let unwritable = blocker.join("config.toml");
    shell.update(|_, cx| {
        workspaces.update(cx, |workspaces, cx| workspaces.set_file(Ok(unwritable), cx));
    });
    shell.click(String::leak(format!(
        "{}-input-1",
        DirField::Repository.id()
    )));
    shell.type_text("2");
    shell.click_element(("workspace-save", 1usize));
    shell.settle_until("the failed save", |cx| {
        matches!(workspaces.read(cx).state(), SaveState::Failed(_))
    });
    let states = shell.update(|_, cx| panel.read(cx).workspace_states(cx));
    assert_eq!(states, [RowState::Saved, RowState::Unsaved]);
    let shown = lines(&mut shell);
    assert!(
        shown[1].starts_with("Not saved: could not write") && shown[1].contains("blocker"),
        "{shown:?}"
    );
    let saved = shell.update(|_, cx| workspaces.read(cx).saved()[1].entry.path.clone());
    assert_eq!(saved, "C:/src/new", "the file's version is the one used");

    for state in [
        RowState::Saved,
        RowState::Saving,
        RowState::Unsaved,
        RowState::Invalid("x".into()),
    ] {
        for failure in [None, Some("Not saved: could not write x")] {
            let text = row_status_text(&state, failure);
            assert!(!text.contains("this run"), "{text}");
        }
    }
    assert!(
        lines(&mut shell)
            .iter()
            .all(|line| !line.contains("this run"))
    );
}
