//! The Workspaces section's directory inputs: a folder button on the left
//! of each, which opens the system's folder picker for this PC and a
//! modal folder list for an SSH machine, and the line saying that an empty
//! directory is cloned in a terminal running the command shown.
//!
//! No `ssh` runs: the fixtures' SSH machines are invented, and a listing is
//! put in the folder list directly.

use app::config::WorkspaceEntry;
use app::dir_browser::Listing;
use app::settings_panel::{DirField, SettingsPanel};
use app::workspaces::Workspaces;
use gpui::{Entity, SharedString, TestAppContext};

use crate::harness::{Options, ShellWindow, open_shell};

fn panel(shell: &mut ShellWindow) -> Entity<SettingsPanel> {
    shell
        .update(|_, cx| SettingsPanel::existing(cx))
        .expect("the settings are open")
}

/// The settings open with `entries` as the saved workspaces.
fn settings_with(shell: &mut ShellWindow, entries: Vec<WorkspaceEntry>) -> Entity<SettingsPanel> {
    shell.update(|_, cx| {
        let workspaces = Workspaces::global(cx).expect("workspaces are installed");
        workspaces.update(cx, |workspaces, cx| workspaces.loaded(entries, cx));
    });
    shell.keys("ctrl-,");
    let panel = panel(shell);
    shell.update(|window, cx| panel.update(cx, |panel, cx| panel.reveal_workspaces(window, cx)));
    shell.settle();
    panel
}

fn entry(machine: &str, path: &str) -> WorkspaceEntry {
    WorkspaceEntry {
        host: Some("github.com".into()),
        repo: "octo/widgets".into(),
        machine: machine.into(),
        path: path.into(),
        ..WorkspaceEntry::default()
    }
}

fn path_of(shell: &mut ShellWindow, panel: &Entity<SettingsPanel>, ix: usize) -> String {
    shell.update(|_, cx| panel.read(cx).workspace_entries(cx)[ix].path.clone())
}

#[gpui::test]
fn each_directory_has_its_folder_button_on_the_left(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    settings_with(&mut shell, vec![entry("local", "")]);
    for field in [DirField::Repository, DirField::Workdir] {
        let id = field.id();
        let button = shell.bounds_of(String::leak(format!("{id}-browse-0")));
        let input = shell.bounds_of(String::leak(format!("{id}-input-0")));
        assert!(
            button.right() <= input.left(),
            "{id}: the button {button:?} is left of the input {input:?}"
        );
        assert!(
            button.top() < input.bottom() && input.top() < button.bottom(),
            "{id}: on the same line"
        );
    }
}

#[gpui::test]
fn on_this_pc_the_folder_picker_fills_the_input(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    let panel = settings_with(&mut shell, vec![entry("local", "")]);
    shell.click("workspace-path-browse-0");
    assert!(
        shell.cx.did_prompt_for_paths(),
        "the system's picker opened"
    );
    assert!(
        !shell.drawn("workspace-browser"),
        "no in-app list for this PC"
    );
    let chosen = std::path::PathBuf::from(if cfg!(windows) {
        "C:\\src\\widgets"
    } else {
        "/src/widgets"
    });
    let answer = chosen.clone();
    shell.cx.simulate_path_prompt_response(move |options| {
        assert!(options.directories && !options.files && !options.multiple);
        Some(vec![answer])
    });
    shell.settle();
    assert_eq!(path_of(&mut shell, &panel, 0), chosen.to_string_lossy());
}

#[gpui::test]
fn on_an_ssh_machine_a_folder_list_opens_and_says_what_failed(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    let panel = settings_with(&mut shell, vec![entry("buildbox", "~/src")]);
    shell.click("workspace-workdir-browse-0");
    shell.settle();
    assert!(shell.drawn("workspace-browser"));
    assert!(!shell.cx.did_prompt_for_paths());
    let error = shell.update(|_, cx| panel.read(cx).browser_error());
    assert!(
        error
            .flatten()
            .is_some_and(|error| error.contains("invented machine")),
        "the error line says why"
    );
    assert!(shell.drawn("workspace-browser-error"));

    // A listing as the machine would send it.
    shell.update(|_, cx| {
        panel.update(cx, |panel, cx| {
            panel.show_listing(
                Listing {
                    home: "/home/u".into(),
                    dir: "/home/u/src".into(),
                    folders: vec!["app".into(), "it's".into()],
                },
                cx,
            );
        });
    });
    assert!(shell.drawn("workspace-browser-up"));
    assert!(shell.drawn("workspace-browser-folder-1"));
    shell.click_element(SharedString::from("workspace-browser-use"));
    assert!(!shell.drawn("workspace-browser"), "the list closes");
    let workdir = shell.update(|_, cx| panel.read(cx).workspace_entries(cx)[0].workdir.clone());
    assert_eq!(workdir.as_deref(), Some("~/src"));
    assert_eq!(
        path_of(&mut shell, &panel, 0),
        "~/src",
        "the other field kept"
    );

    // Entering a folder lists it; the invented machine answers with an error.
    shell.click("workspace-path-browse-0");
    shell.update(|_, cx| {
        panel.update(cx, |panel, cx| {
            panel.show_listing(
                Listing {
                    home: "/home/u".into(),
                    dir: "/home/u".into(),
                    folders: vec!["src".into()],
                },
                cx,
            );
        });
    });
    shell.click("workspace-browser-folder-0");
    let error = shell.update(|_, cx| panel.read(cx).browser_error());
    assert!(error.flatten().is_some_and(|e| e.contains("buildbox")));
    shell.click_element(SharedString::from("workspace-browser-cancel"));
    assert!(!shell.drawn("workspace-browser"));
    assert_eq!(
        path_of(&mut shell, &panel, 0),
        "~/src",
        "cancel changes nothing"
    );
}

#[gpui::test]
fn the_folder_list_is_a_modal_over_the_window(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    settings_with(&mut shell, vec![entry("buildbox", "~/src")]);
    shell.click("workspace-workdir-browse-0");
    let modal = shell.bounds_of("workspace-browser");
    let viewport = shell.update(|window, _| window.viewport_size());
    let input = shell.bounds_of("workspace-workdir-input-0");
    assert!(
        (f32::from(modal.center().x) - f32::from(viewport.width) / 2.0).abs() < 2.0
            && (f32::from(modal.center().y) - f32::from(viewport.height) / 2.0).abs() < 2.0,
        "centered in the window: {modal:?} in {viewport:?}"
    );
    assert!(
        !(modal.top() >= input.bottom() && modal.left() == input.left()),
        "not inline under the input"
    );

    shell.keys("escape");
    assert!(!shell.drawn("workspace-browser"), "Escape closes it");

    shell.click("workspace-workdir-browse-0");
    assert!(shell.drawn("workspace-browser"));
    shell.click_at(gpui::point(gpui::px(4.0), gpui::px(4.0)));
    assert!(
        !shell.drawn("workspace-browser"),
        "a click on the backdrop closes it"
    );
}

#[gpui::test]
fn the_wizard_says_a_terminal_runs_the_clone(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    let local_dir = if cfg!(windows) {
        "C:/src/widgets"
    } else {
        "/src/widgets"
    };
    let panel = settings_with(
        &mut shell,
        vec![
            entry("local", local_dir),
            entry("buildbox", "~/src/widgets"),
        ],
    );
    assert!(shell.drawn("workspace-clone-note-0"));
    assert!(shell.drawn("workspace-clone-note-1"));
    let notes: Vec<String> = shell.update(|_, cx| {
        let panel = panel.read(cx);
        (0..2).filter_map(|ix| panel.clone_note(ix, cx)).collect()
    });
    let local = &notes[0];
    assert!(
        local.starts_with("When the directory is empty, Review with agent opens a terminal on This PC that runs `git clone "),
        "{local}"
    );
    assert!(local.contains("git@github.com:octo/widgets.git"), "{local}");
    assert!(
        local.contains("src") && local.contains("widgets"),
        "{local}"
    );
    assert!(local.ends_with("; you can keep typing in it."), "{local}");
    assert_eq!(
        notes[1],
        "When the directory is empty, Review with agent opens a terminal on buildbox over SSH \
         that runs `git clone git@github.com:octo/widgets.git src/widgets`; you can keep typing \
         in it."
    );

    // Without a forge the URL is unknown, and the line says so less exactly.
    shell.update(|_, cx| {
        let workspaces = Workspaces::global(cx).expect("workspaces");
        workspaces.update(cx, |workspaces, cx| {
            workspaces.loaded(
                vec![WorkspaceEntry {
                    host: None,
                    ..entry("buildbox", "~/w")
                }],
                cx,
            );
        });
    });
    shell.settle();
    let note = shell.update(|_, cx| panel.read(cx).clone_note(0, cx));
    assert_eq!(
        note.as_deref(),
        Some(
            "When the directory is empty, Review with agent opens a terminal on buildbox over \
             SSH that runs `git clone`; you can keep typing in it."
        )
    );
}
