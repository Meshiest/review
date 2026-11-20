//! A fixtures run, and every benchmark or screenshot run built on one, keeps
//! its state in memory or in the files its flags name: the user's layout,
//! sidebar, settings, cache and avatars are neither read nor written.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use app::appearance;
use app::config::WorkspaceEntry;
use app::startup::{RunFiles, RunFlags, UserDirs};
use app::workspaces::{SaveState, Storage, Workspaces};
use gpui::{Entity, TestAppContext};

use crate::harness::{Options, Scratch, ShellWindow, open_shell};

/// Every file under `root` with its bytes.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)
            .expect("read the directory")
            .flatten()
        {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                files.insert(path.clone(), std::fs::read(&path).expect("read the file"));
            }
        }
    }
    files
}

/// User directories holding a file of each kind the app keeps.
fn user_files(scratch: &Scratch) -> UserDirs {
    let dirs = UserDirs::under(&scratch.join("user"));
    let config = dirs.config.clone().expect("config dir");
    let data = dirs.data.clone().expect("data dir");
    let local = dirs.data_local.clone().expect("local dir");
    for (path, text) in [
        (config.join("settings.toml"), "theme = \"vercel\"\n"),
        (config.join("themes").join("mine.toml"), "not a theme"),
        (data.join("layout.json"), "{\"version\": 2}"),
        (data.join("sidebar.json"), "{\"version\": 1}"),
        (local.join("cache.sqlite"), "not a database"),
        (local.join("avatars").join("a.png"), "not an image"),
    ] {
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("create the dir");
        std::fs::write(path, text).expect("write the file");
    }
    dirs
}

#[gpui::test]
fn a_fixtures_run_leaves_the_user_files_alone(cx: &mut TestAppContext) {
    let scratch = Scratch::new("isolation");
    let user = user_files(&scratch);
    let root = scratch.join("user");
    let before = snapshot(&root);

    // As `review --fixtures --layout-file .. --settings ..` plans it.
    let run = scratch.join("run");
    let files = RunFiles::plan(
        &RunFlags {
            fixtures: true,
            layout_file: Some(run.join("layout.json")),
            settings: Some(run.join("settings.toml")),
            ..RunFlags::default()
        },
        &user,
    );

    let mut shell = open_shell(
        cx,
        Options {
            layout_file: files.layout.clone(),
            settings: files.settings.clone(),
            ..Options::default()
        },
    );
    // Things that save: a layout change, a settings change, a draft.
    shell.open_pr(41);
    shell.keys("ctrl-b ctrl-b");
    shell.update(|_, cx| appearance::update(cx, |settings| settings.load_gravatar = true));
    shell.advance(Duration::from_secs(2));
    shell.settle_until("the layout is saved", |_| run.join("layout.json").is_file());
    shell.settle_until("the settings are saved", |_| {
        run.join("settings.toml").is_file()
    });

    assert_eq!(snapshot(&root), before, "the user's files are as they were");
}

fn workspace() -> WorkspaceEntry {
    WorkspaceEntry {
        repo: "octo/widgets".into(),
        machine: "local".into(),
        path: "C:/src/widgets".into(),
        ..WorkspaceEntry::default()
    }
}

fn save_workspace(shell: &mut ShellWindow) -> Entity<Workspaces> {
    let workspaces =
        shell.update(|_, cx| Workspaces::global(cx).expect("workspaces are installed"));
    shell.update(|_, cx| {
        workspaces.update(cx, |workspaces, cx| {
            workspaces.save_entry(None, workspace(), cx);
        });
    });
    shell.settle_until("the workspace is saved", |cx| {
        *workspaces.read(cx).state() == SaveState::Saved
    });
    workspaces
}

#[gpui::test]
fn a_fixtures_run_with_a_settings_file_saves_workspaces_beside_it(cx: &mut TestAppContext) {
    let scratch = Scratch::new("workspaces-file");
    let config = scratch.join("config.toml");
    let mut shell = open_shell(
        cx,
        Options {
            settings: Some(scratch.join("settings.toml")),
            ..Options::default()
        },
    );
    let workspaces = save_workspace(&mut shell);
    let storage = shell.update(|_, cx| workspaces.read(cx).storage().clone());
    assert_eq!(storage, Storage::File(config.clone()));
    let text = std::fs::read_to_string(&config).expect("written beside the settings");
    assert!(text.contains("octo/widgets"), "{text}");
}

/// The names in the directory a run starts from, and in its `.scratch`.
fn start_dir_names() -> Vec<PathBuf> {
    let dir = std::env::current_dir().expect("a current directory");
    [dir.clone(), dir.join(".scratch")]
        .iter()
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flat_map(|entries| entries.flatten().map(|entry| entry.path()))
        .collect()
}

#[gpui::test]
fn a_fixtures_run_without_a_settings_file_keeps_workspaces_in_memory(cx: &mut TestAppContext) {
    let before = start_dir_names();
    let mut shell = open_shell(cx, Options::default());
    let workspaces = save_workspace(&mut shell);
    shell.update(|_, cx| {
        let workspaces = workspaces.read(cx);
        assert_eq!(*workspaces.storage(), Storage::Memory);
        assert_eq!(
            workspaces.entries().cloned().collect::<Vec<_>>(),
            [workspace()],
            "saved, and used"
        );
    });
    assert_eq!(start_dir_names(), before, "no file or folder is created");
}
