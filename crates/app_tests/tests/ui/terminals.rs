//! Terminals as dock panels: the machine picker (`Ctrl+\``), placement in a
//! split or beside the CI log, closing a tab ends its program, keys a
//! program uses reach it, the theme follows, a restored layout holds a
//! placeholder, and "Open on machine" makes the worktree first, then starts
//! Claude without the pull request's agent config unless the user trusts it.
//!
//! The programs started here are local `cmd` sessions (`sh` off Windows);
//! nothing connects to another machine (a fixtures run lists invented ones).

use std::time::Duration;

use app::agent::{ConfigCheck, SAFE_CLAUDE_FLAGS};
use app::machines::{self, Machine, MachineEntry};
use app::model::ForgeKind;
use app::panels::PanelKind;
use app::session::{ForgeState, ForgeStatus, OpenMode};
use app::terminal_panel::{RunState, TerminalPanel};
use app::terminals::{TerminalSpec, Terminals, Where, WorktreeState};
use gpui::{Entity, SharedString, TestAppContext};
use gpui_kit::component::dock::{DockPlacement, PanelId};
use term::ExitState;

use crate::harness::{Options, Scratch, ShellWindow, StartLayout, open_shell};

fn terminals(shell: &mut ShellWindow) -> Entity<Terminals> {
    shell.update(|_, cx| Terminals::global(cx).expect("terminals are installed"))
}

/// The terminal panels in the main dock.
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

/// The terminal panels whose program started: not a launch tab still
/// showing its steps.
fn programs(shell: &mut ShellWindow) -> Vec<(PanelId, Entity<TerminalPanel>)> {
    let panels = terminal_panels(shell);
    shell.update(|_, cx| {
        panels
            .into_iter()
            .filter(|(_, panel)| panel.read(cx).checklist().is_none())
            .collect()
    })
}

/// A shell that is quick to start and to end.
const SHELL: &str = if cfg!(windows) { "cmd" } else { "sh" };

/// A command that is not Claude, so it has no safe launch.
const ECHO_READY: &str = if cfg!(windows) {
    "cmd /c echo ready"
} else {
    "echo ready"
};

/// A [`SHELL`] on this PC.
fn shell_spec() -> TerminalSpec {
    TerminalSpec {
        machine: machines::THIS_PC.to_owned(),
        shell: Some(SHELL.to_owned()),
        ..TerminalSpec::default()
    }
}

fn add_terminal(shell: &mut ShellWindow, spec: TerminalSpec, at: Where) {
    let entity = shell.shell.clone();
    shell.update(|window, cx| {
        entity.update(cx, |shell, cx| shell.add_terminal(spec, at, window, cx));
    });
    shell.settle();
}

#[gpui::test]
fn ctrl_backtick_picks_a_machine_and_opens_a_split(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    shell.open_pr(41);
    let terminals = terminals(&mut shell);

    shell.keys("ctrl-`");
    assert!(shell.update(|_, cx| terminals.read(cx).is_picking()));
    assert!(shell.drawn("machine-picker"));
    let names: Vec<String> = shell.update(|_, cx| {
        terminals
            .read(cx)
            .listed()
            .iter()
            .map(|machine| machine.name.clone())
            .collect()
    });
    assert_eq!(names[0], machines::THIS_PC, "this PC first");
    assert!(names.len() > 1, "and the machines");
    shell.keys("escape");
    assert!(!shell.update(|_, cx| terminals.read(cx).is_picking()));
    assert!(terminal_panels(&mut shell).is_empty(), "Esc opens nothing");

    // A split beside the diff, as picking does.
    add_terminal(&mut shell, shell_spec(), Where::Split);
    let groups = shell.groups();
    assert!(
        groups.iter().any(|group| group == &["Terminal"]),
        "a group of its own: {groups:?}"
    );
    assert!(
        groups
            .iter()
            .any(|group| group.contains(&"Diff".to_owned())),
        "beside the diff: {groups:?}"
    );
    let (id, panel) = terminal_panels(&mut shell).remove(0);
    assert_eq!(
        shell.update(|_, cx| panel.read(cx).state().clone()),
        RunState::Running
    );

    // Keys a program uses go to it, not to the window.
    shell.keys("ctrl-w ctrl-b");
    assert!(
        !shell.dock_lacks(id),
        "Ctrl+W reached the shell, not the tab"
    );
    assert!(
        shell.drawn("section-Inbox"),
        "Ctrl+B did not hide the sidebar"
    );
    let goes_back = shell.update(|window, _| {
        let alt_left = gpui::Keystroke::parse("alt-left").expect("a keystroke");
        window
            .possible_bindings_for_input(&[alt_left])
            .iter()
            .any(|binding| binding.action().partial_eq(&app::shell::GoBack))
    });
    assert!(!goes_back, "Alt+Left stays with the program, not Back");

    // Closing the tab ends the program.
    let view = shell.update(|_, cx| panel.read(cx).view().cloned().expect("running"));
    shell.click_named(&format!("close-terminal-{}", panel.entity_id().as_u64()));
    assert!(shell.dock_lacks(id), "the tab is gone");
    shell.settle_until("the program to end", |cx| {
        view.read(cx).session().exit_state() != ExitState::Running
    });
}

#[gpui::test]
fn the_right_pane_is_the_logs_group(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    shell.open_pr(41);
    let session = shell.session();
    shell.update(|_, cx| session.update(cx, |s, cx| s.show_panel(PanelKind::Log, cx)));
    shell.settle();
    add_terminal(&mut shell, shell_spec(), Where::RightPane);
    let groups = shell.groups();
    assert!(
        groups.iter().any(
            |group| group.contains(&"Log".to_owned()) && group.contains(&"Terminal".to_owned())
        ),
        "a tab beside the log: {groups:?}"
    );
    for (_, panel) in terminal_panels(&mut shell) {
        let view = shell.update(|_, cx| panel.read(cx).view().cloned().expect("running"));
        shell.update(|_, cx| view.update(cx, |view, cx| view.close(cx)));
    }
}

#[gpui::test]
fn the_style_follows_the_theme(cx: &mut TestAppContext) {
    let mut shell = open_shell(
        cx,
        Options {
            theme: crate::harness::flat(),
            ..Options::default()
        },
    );
    add_terminal(&mut shell, shell_spec(), Where::Tab);
    let (_, panel) = terminal_panels(&mut shell).remove(0);
    let view = shell.update(|_, cx| panel.read(cx).view().cloned().expect("running"));
    let style = |shell: &mut ShellWindow| shell.update(|_, cx| view.read(cx).style().clone());
    let before = style(&mut shell);
    shell.update(|_, cx| {
        app::appearance::update(cx, |settings| {
            settings.mode = theme::ModeSetting::Light;
            settings.buffer_font.size = Some(19.0);
        });
    });
    shell.settle();
    let after = style(&mut shell);
    assert_ne!(
        before.background, after.background,
        "a light theme, a light terminal"
    );
    assert_ne!(
        before.font_size, after.font_size,
        "and the buffer font's size"
    );
    shell.update(|_, cx| view.update(cx, |view, cx| view.close(cx)));
}

#[gpui::test]
fn a_restored_terminal_waits_to_be_reopened(first: &mut TestAppContext, next: &mut TestAppContext) {
    let scratch = Scratch::new("restored-terminal");
    let mut shell = open_shell(first, Options::default());
    let spec = TerminalSpec {
        cwd: Some(scratch.join("").to_string_lossy().into_owned()),
        ..shell_spec()
    };
    add_terminal(&mut shell, spec.clone(), Where::Tab);
    let dock = shell.dock();
    let saved = shell.update(|_, cx| dock.read(cx).dump(cx));
    for (_, panel) in terminal_panels(&mut shell) {
        let view = shell.update(|_, cx| panel.read(cx).view().cloned().expect("running"));
        shell.update(|_, cx| view.update(cx, |view, cx| view.close(cx)));
    }

    let mut restored = open_shell(
        next,
        Options {
            layout: StartLayout::Saved(Box::new(saved)),
            ..Options::default()
        },
    );
    let panels = terminal_panels(&mut restored);
    assert_eq!(panels.len(), 1, "the layout keeps its place");
    let panel = &panels[0].1;
    let (state, kept) = restored.update(|_, cx| {
        let panel = panel.read(cx);
        (panel.state().clone(), panel.spec().clone())
    });
    assert_eq!(state, RunState::NotStarted, "nothing runs after a restart");
    assert_eq!(kept, spec, "it remembers what it ran");
    assert!(
        restored.element("terminal-reopen").is_some(),
        "and offers to reopen"
    );
}

#[gpui::test]
fn open_on_machine_makes_the_worktree_then_starts_the_command(cx: &mut TestAppContext) {
    let scratch = Scratch::new("open-on");
    let mut shell = open_shell(cx, Options::default());
    shell.show(41, PanelKind::Conversation, OpenMode::Permanent);
    let key = shell.key(41);
    let terminals = terminals(&mut shell);
    let root = scratch.join("worktrees");
    shell.update(|_, cx| terminals.update(cx, |t, _| t.set_fixture_root(root.clone())));

    // The header's button opens the picker for this pull request.
    shell.click_element(SharedString::from("open-on-machine"));
    assert!(shell.update(|_, cx| terminals.read(cx).is_picking()));
    // An invented machine is never connected to.
    shell.keys("down enter");
    let state = shell.update(|_, cx| terminals.read(cx).worktree(&key).cloned());
    assert!(
        matches!(&state, Some(WorktreeState::Failed { error, .. }) if error.contains("invented")),
        "{state:?}"
    );
    assert!(programs(&mut shell).is_empty());
    let tabs = terminal_panels(&mut shell);
    assert_eq!(tabs.len(), 1, "its tab shows the failed step");
    let failed = shell.update(|_, cx| {
        tabs[0]
            .1
            .read(cx)
            .checklist()
            .and_then(|list| list.error().map(str::to_owned))
    });
    assert!(failed.is_some_and(|error| error.contains("invented")));
    assert!(shell.element("launch-retry").is_some(), "Retry");

    // This PC, with a stand-in Claude: the pull request changes no agent
    // config, so it starts at once, with the safe flags.
    let claude = fake_claude(&scratch);
    let machine = this_pc(&claude);
    shell.update(|_, cx| {
        terminals.update(cx, |t, cx| {
            t.open_on(key.clone(), machine, Where::Split, cx)
        });
    });
    shell.settle_until("the worktree", |cx| {
        matches!(
            terminals.read(cx).worktree(&key),
            Some(WorktreeState::Ready(_))
        )
    });
    shell.advance(Duration::from_millis(50));
    let Some(WorktreeState::Ready(info)) =
        shell.update(|_, cx| terminals.read(cx).worktree(&key).cloned())
    else {
        panic!("the worktree was ready a moment ago");
    };
    assert!(info.path.ends_with("pr-41"), "{}", info.path);
    assert!(std::path::Path::new(&info.path).is_dir());
    assert!(
        !shell.drawn("worktree-state"),
        "no status line in the header"
    );
    assert!(!shell.drawn("agent-config-dialog"), "nothing to ask");

    let panels = terminal_panels(&mut shell);
    assert_eq!(panels.len(), 1, "a terminal opened");
    let spec = shell.update(|_, cx| panels[0].1.read(cx).spec().clone());
    assert_eq!(
        spec.command,
        Some(format!("{claude} {}", SAFE_CLAUDE_FLAGS.join(" "))),
        "Claude with the safe flags"
    );
    assert_eq!(spec.cwd.as_deref(), Some(info.path.as_str()));
    assert_absolute_programs(&spec);
    close_terminals(&mut shell);
}

#[gpui::test]
fn another_agent_never_starts_by_itself(cx: &mut TestAppContext) {
    let scratch = Scratch::new("open-on-other");
    let mut shell = open_shell(cx, Options::default());
    shell.show(41, PanelKind::Conversation, OpenMode::Permanent);
    let key = shell.key(41);
    let terminals = terminals(&mut shell);
    let root = scratch.join("worktrees");
    shell.update(|_, cx| terminals.update(cx, |t, _| t.set_fixture_root(root.clone())));
    let open = |shell: &mut ShellWindow| {
        let machine = this_pc(ECHO_READY);
        shell.update(|_, cx| {
            terminals.update(cx, |t, cx| {
                t.open_on(key.clone(), machine, Where::Split, cx)
            });
        });
        shell.settle_until("the choice", |cx| {
            matches!(
                terminals.read(cx).worktree(&key),
                Some(WorktreeState::Choosing(_))
            )
        });
    };

    // No agent config changed, but the command has no safe launch: it asks.
    open(&mut shell);
    let check = shell.update(|_, cx| {
        terminals
            .read(cx)
            .pending()
            .map(|pending| pending.prepared.check.clone())
    });
    assert_eq!(check, Some(ConfigCheck::Clean));
    assert!(shell.drawn("agent-config-dialog"));
    assert!(programs(&mut shell).is_empty(), "nothing started yet");

    // The default opens a shell there, without the command.
    shell.click_element("agent-start-safe");
    shell.advance(Duration::from_millis(50));
    let panels = terminal_panels(&mut shell);
    assert_eq!(panels.len(), 1, "a shell opened");
    let spec = shell.update(|_, cx| panels[0].1.read(cx).spec().clone());
    assert_eq!(spec.command, None, "only a shell");
    assert!(
        spec.cwd
            .as_deref()
            .is_some_and(|cwd| cwd.ends_with("pr-41"))
    );
    close_terminals(&mut shell);

    // Running it takes the confirmation, which names it and the worktree.
    open(&mut shell);
    shell.click_element("agent-trust");
    assert!(shell.drawn("agent-trust-question"));
    assert_eq!(programs(&mut shell).len(), 1, "nothing new yet");
    shell.click_element("agent-trust-confirm");
    shell.advance(Duration::from_millis(50));
    let panels = terminal_panels(&mut shell);
    assert_eq!(panels.len(), 2, "the command's terminal opened");
    let specs: Vec<TerminalSpec> = shell.update(|_, cx| {
        panels
            .iter()
            .map(|(_, panel)| panel.read(cx).spec().clone())
            .collect()
    });
    let spec = specs
        .into_iter()
        .find(|spec| spec.command.is_some())
        .expect("one runs the command");
    assert_eq!(spec.command.as_deref(), Some(ECHO_READY));
    assert_absolute_programs(&spec);
    close_terminals(&mut shell);
}

/// A stand-in `claude` that prints its arguments and ends, at an absolute
/// path, so a test never starts the real one.
fn fake_claude(scratch: &Scratch) -> String {
    let dir = scratch.join("bin");
    std::fs::create_dir_all(&dir).expect("create the bin dir");
    let program = if cfg!(windows) {
        let program = dir.join("claude.cmd");
        std::fs::write(&program, "@echo claude %*\r\n").expect("write the stand-in");
        program
    } else {
        let program = dir.join("claude");
        std::fs::write(&program, "#!/bin/sh\necho claude \"$@\"\n").expect("write the stand-in");
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

/// This PC running `command` in [`SHELL`].
fn this_pc(command: &str) -> Machine {
    Machine {
        extras: MachineEntry {
            host: machines::LOCAL_HOST.to_owned(),
            command: Some(command.to_owned()),
            shell: Some(SHELL.to_owned()),
            ..MachineEntry::default()
        },
        ..Machine::this_pc(None)
    }
}

/// Opens fixture pull request 43, which changes `.claude/settings.json` and
/// adds `.mcp.json`, on this PC, and waits for the agent config dialog.
fn open_config_changing_pr(
    shell: &mut ShellWindow,
    scratch: &Scratch,
    claude: &str,
) -> Entity<Terminals> {
    shell.show(43, PanelKind::Conversation, OpenMode::Permanent);
    let key = shell.key(43);
    let terminals = terminals(shell);
    let root = scratch.join("worktrees");
    shell.update(|_, cx| terminals.update(cx, |t, _| t.set_fixture_root(root.clone())));
    let machine = this_pc(claude);
    shell.update(|_, cx| {
        terminals.update(cx, |t, cx| {
            t.open_on(key.clone(), machine, Where::Split, cx)
        });
    });
    shell.settle_until("the worktree", |cx| {
        matches!(
            terminals.read(cx).worktree(&key),
            Some(WorktreeState::Choosing(_))
        )
    });
    terminals
}

/// The programs the panel starts for `spec` are absolute paths: the shell,
/// and the command's program inside the shell's quotes (double for `cmd`,
/// single or none for a POSIX shell).
fn assert_absolute_programs(spec: &TerminalSpec) {
    let config = spec.session_config().expect("the programs are on the PATH");
    assert!(
        std::path::Path::new(&config.program).is_absolute(),
        "{}",
        config.program
    );
    let line = config.args.last().cloned().unwrap_or_default();
    let program = match line.chars().next() {
        // cmd's whole line is quoted around the program's own quotes.
        Some(quote @ ('"' | '\'')) => line.trim_start_matches(quote).split(quote).next(),
        _ => line.split([' ', ';']).next(),
    }
    .unwrap_or_default();
    assert!(
        std::path::Path::new(program).is_absolute(),
        "the command's program: {line}"
    );
}

fn close_terminals(shell: &mut ShellWindow) {
    for (_, panel) in programs(shell) {
        shell.settle_until("the program to start", |cx| panel.read(cx).view().is_some());
        let view = shell.update(|_, cx| panel.read(cx).view().cloned().expect("running"));
        shell.update(|_, cx| view.update(cx, |view, cx| view.close(cx)));
    }
}

#[gpui::test]
fn a_pr_that_changes_agent_config_starts_without_it_by_default(cx: &mut TestAppContext) {
    let scratch = Scratch::new("agent-config-safe");
    let claude = fake_claude(&scratch);
    let mut shell = open_shell(cx, Options::default());
    let terminals = open_config_changing_pr(&mut shell, &scratch, &claude);

    assert!(shell.drawn("agent-config-dialog"), "the warning shows");
    assert!(programs(&mut shell).is_empty(), "nothing started yet");
    let check = shell.update(|_, cx| {
        terminals
            .read(cx)
            .pending()
            .map(|pending| pending.prepared.check.clone())
    });
    let Some(ConfigCheck::Changed(changes)) = check else {
        panic!("a change to the agent config: {check:?}");
    };
    let paths: Vec<&str> = changes.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(paths, [".claude/settings.json", ".mcp.json"]);
    assert!(
        shell.drawn("agent-worktree-place"),
        "it says where the worktree is"
    );
    assert!(
        shell.element("agent-trust").is_some(),
        "trusting is offered"
    );

    // "View diff" shows the file in the diff and leaves the dialog up.
    shell.click_element(("agent-config-diff", 0usize));
    let session = shell.session();
    let selected = shell.update(|_, cx| session.read(cx).selected_file().cloned());
    assert_eq!(selected.as_deref(), Some(".claude/settings.json"));
    assert!(shell.drawn("agent-config-dialog"));

    shell.click_element("agent-start-safe");
    assert!(!shell.drawn("agent-config-dialog"), "answered");
    shell.advance(Duration::from_millis(50));
    let panels = terminal_panels(&mut shell);
    assert_eq!(panels.len(), 1, "a terminal opened");
    let spec = shell.update(|_, cx| panels[0].1.read(cx).spec().clone());
    let command = spec.command.clone().expect("claude runs");
    assert!(command.starts_with(&claude), "{command}");
    assert!(
        command.contains(&SAFE_CLAUDE_FLAGS.join(" ")),
        "with the safe flags: {command}"
    );
    assert_absolute_programs(&spec);
    let key = shell.key(43);
    assert!(matches!(
        shell.update(|_, cx| terminals.read(cx).worktree(&key).cloned()),
        Some(WorktreeState::Ready(_))
    ));
    close_terminals(&mut shell);
}

#[gpui::test]
fn trusting_a_prs_agent_config_takes_a_confirmation(cx: &mut TestAppContext) {
    let scratch = Scratch::new("agent-config-trust");
    let claude = fake_claude(&scratch);
    let mut shell = open_shell(cx, Options::default());
    let terminals = open_config_changing_pr(&mut shell, &scratch, &claude);

    shell.click_element("agent-trust");
    assert!(shell.drawn("agent-trust-question"), "it asks again");
    assert!(programs(&mut shell).is_empty(), "nothing started yet");
    assert!(shell.element("agent-trust-confirm").is_some());
    let asked = shell.update(|_, cx| {
        terminals
            .read(cx)
            .pending()
            .is_some_and(|pending| pending.confirming)
    });
    assert!(asked);

    // Back returns to the choice, which starts nothing by itself.
    shell.click_element("agent-trust-back");
    assert!(!shell.drawn("agent-trust-question"));
    assert!(shell.element("agent-start-safe").is_some());
    assert!(programs(&mut shell).is_empty());

    shell.click_element("agent-trust");
    shell.click_element("agent-trust-confirm");
    shell.advance(Duration::from_millis(50));
    let panels = terminal_panels(&mut shell);
    assert_eq!(panels.len(), 1, "a terminal opened");
    let spec = shell.update(|_, cx| panels[0].1.read(cx).spec().clone());
    assert_eq!(
        spec.command.as_deref(),
        Some(claude.as_str()),
        "the command as configured, without the safe flags"
    );
    assert_absolute_programs(&spec);
    close_terminals(&mut shell);
}

/// Not clicked: it would start the real `glab auth login`.
#[gpui::test]
fn a_forge_with_an_auth_error_offers_to_sign_in(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    let session = shell.session();
    shell.update(|_, cx| {
        session.update(cx, |session, cx| {
            session.forges = vec![
                ForgeStatus {
                    host: "gitlab.com".into(),
                    kind: ForgeKind::GitLab,
                    state: ForgeState::AuthError("invalid_grant".into()),
                    account: None,
                },
                ForgeStatus {
                    host: "github.com".into(),
                    kind: ForgeKind::GitHub,
                    state: ForgeState::Ok,
                    account: None,
                },
            ];
            cx.notify();
        });
    });
    shell.redraw();
    assert!(shell.drawn("sign-in-gitlab.com"));
    assert!(!shell.drawn("sign-in-github.com"), "signed in already");
}
