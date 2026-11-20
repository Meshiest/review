//! A dock layout the user changed survives a restart: it is written through
//! the path the shell saves with and read back through the one startup
//! loads with, into a fresh state equal to the saved one.
//!
//! The layout model is plain data (`DockAreaState`), so no window is needed.
//! The file lives under `.scratch/layout-restore/`, never in the user's data
//! directory.

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use app::layout::{self, VERSION};
use app::panels::{CONVERSATION, DIFF, LOG};
use gpui::{Axis, px};
use gpui_kit::component::dock::{DockAreaState, DockPlacement, DockState, PanelInfo, PanelState};

fn panel(name: &str) -> PanelState {
    PanelState::new(name)
}

fn tabs(active: usize, panels: &[&str]) -> PanelState {
    PanelState {
        panel_name: "TabPanel".to_owned(),
        children: panels.iter().map(|name| panel(name)).collect(),
        info: PanelInfo::tabs(active),
    }
}

fn stack(axis: Axis, sizes: &[f32], children: Vec<PanelState>) -> PanelState {
    PanelState {
        panel_name: "StackPanel".to_owned(),
        children,
        info: PanelInfo::stack(sizes.iter().map(|&s| px(s)).collect(), axis),
    }
}

/// The default layout: Diff and Conversation tabbed, Log beside.
fn default_layout() -> DockAreaState {
    DockAreaState {
        version: Some(VERSION),
        center: stack(
            Axis::Horizontal,
            &[820.0, 340.0],
            vec![tabs(0, &[DIFF, CONVERSATION]), tabs(0, &[LOG])],
        ),
        ..DockAreaState::default()
    }
}

/// The user split the main group vertically and moved Conversation out of it
/// into the Log group, where it is the shown tab; the splits were dragged.
fn changed_layout() -> DockAreaState {
    DockAreaState {
        version: Some(VERSION),
        center: stack(
            Axis::Horizontal,
            &[903.5, 256.5],
            vec![
                stack(
                    Axis::Vertical,
                    &[512.0, 248.0],
                    vec![tabs(0, &[DIFF]), tabs(0, &[DIFF])],
                ),
                tabs(1, &[LOG, CONVERSATION]),
            ],
        ),
        ..DockAreaState::default()
    }
}

/// A fresh directory for this run under the workspace's `.scratch`.
fn scratch_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("after the epoch")
        .as_nanos();
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../.scratch/layout-restore")
        .join(format!("{}-{nanos}", std::process::id()))
}

#[test]
fn a_changed_layout_is_restored_equal() {
    let dir = scratch_dir();
    let path = dir.join("layout.json");
    let saved = changed_layout();
    assert_ne!(
        saved,
        default_layout(),
        "the saved layout is not the default"
    );

    layout::write_to(&path, &saved).expect("the layout is written");
    // Written through a temp file that is renamed into place.
    assert!(!path.with_extension("json.tmp").exists());

    let restored = layout::load_from(&path).expect("the saved layout loads");
    assert_eq!(restored, saved);

    // Saving again over it replaces it whole.
    layout::write_to(&path, &default_layout()).expect("the layout is rewritten");
    assert_eq!(layout::load_from(&path), Some(default_layout()));

    fs::remove_dir_all(&dir).expect("remove the scratch directory");
}

#[test]
fn an_older_or_broken_layout_starts_from_the_default() {
    let dir = scratch_dir();
    let path = dir.join("layout.json");

    let older = DockAreaState {
        version: Some(VERSION - 1),
        ..changed_layout()
    };
    layout::write_to(&path, &older).expect("the layout is written");
    assert_eq!(
        layout::load_from(&path),
        None,
        "an older version is ignored"
    );

    fs::write(&path, "{ not json").expect("write a broken file");
    assert_eq!(layout::load_from(&path), None, "a broken file is ignored");

    assert_eq!(layout::load_from(&dir.join("missing.json")), None);
    fs::remove_dir_all(&dir).expect("remove the scratch directory");
}

#[test]
fn a_layout_with_the_removed_checks_panels_loads_without_them() {
    let dir = scratch_dir();
    let path = dir.join("layout.json");
    let sidebar = |sections: &[&str]| {
        DockState::new(
            stack(
                Axis::Vertical,
                &[420.0, 64.0, 100.0],
                sections.iter().map(|s| tabs(0, &[*s])).collect(),
            ),
            DockPlacement::Left,
            layout::SIDEBAR_WIDTH,
            true,
        )
    };
    let old = DockAreaState {
        version: Some(VERSION),
        center: stack(
            Axis::Horizontal,
            &[600.0, 300.0, 260.0],
            vec![
                tabs(2, &[DIFF, CONVERSATION, "Checks"]),
                tabs(0, &["Checks"]),
                tabs(1, &["Checks", LOG]),
            ],
        ),
        left_dock: Some(sidebar(&["Inbox", "Rollup", "Threads"])),
        ..DockAreaState::default()
    };
    layout::write_to(&path, &old).expect("the layout is written");

    let restored = layout::load_from(&path).expect("the old layout loads");
    let without = DockAreaState {
        version: Some(VERSION),
        center: stack(
            Axis::Horizontal,
            &[600.0, 260.0],
            vec![tabs(1, &[DIFF, CONVERSATION]), tabs(0, &[LOG])],
        ),
        left_dock: Some(DockState::new(
            stack(
                Axis::Vertical,
                &[420.0, 100.0],
                vec![tabs(0, &["Inbox"]), tabs(0, &["Threads"])],
            ),
            DockPlacement::Left,
            layout::SIDEBAR_WIDTH,
            true,
        )),
        ..DockAreaState::default()
    };
    assert_eq!(
        restored, without,
        "the Checks tabs and the checks section are gone, the rest as it was"
    );
    fs::remove_dir_all(&dir).expect("remove the scratch directory");
}
