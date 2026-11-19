//! Dock layouts: the built-in presets, and saving and restoring the user's.
//!
//! The dock serializes itself (`DockArea::dump` / `load`); this module only
//! decides where the JSON lives. Each pull request panel saves the pull
//! request it shows, so a restored layout opens them again. The sidebar's
//! sections are the dock's left region, saved in the same file; a layout from
//! before that keeps them in `sidebar.json` beside it, which is read once
//! into the left region.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use gpui::{Pixels, px};
use gpui_kit::component::dock::{DockAreaState, DockPlacement, DockState};

/// Bump when a panel's persisted shape changes; an older file is then ignored.
pub const VERSION: usize = 2;
/// The version of the separate sidebar file older layouts kept.
pub const SIDEBAR_VERSION: usize = 1;
/// How wide the sidebar starts.
pub const SIDEBAR_WIDTH: Pixels = px(320.0);

pub fn layout_path() -> Option<PathBuf> {
    dirs::data_dir().map(|dir| dir.join("review").join("layout.json"))
}

/// Where an older layout kept the sidebar, beside the main layout at `main`.
#[must_use]
pub fn sidebar_path(main: &Path) -> PathBuf {
    main.with_file_name("sidebar.json")
}

/// Reads the saved layout. A missing, unreadable or older-version file means
/// "start from the default", never an error.
pub fn load_saved() -> Option<DockAreaState> {
    load_from(&layout_path()?)
}

/// [`load_saved`] from `path`. A layout without a sidebar region takes the
/// one an older build saved beside it.
pub fn load_from(path: &Path) -> Option<DockAreaState> {
    let mut state = read(path, VERSION)?;
    if state.left_dock.is_none()
        && let Some(sidebar) = load_sidebar_from(&sidebar_path(path))
    {
        merge_sidebar(&mut state, sidebar);
    }
    Some(state)
}

/// The separate sidebar file an older build saved beside its main layout.
pub fn load_saved_sidebar() -> Option<DockAreaState> {
    load_sidebar_from(&sidebar_path(&layout_path()?))
}

/// [`load_saved_sidebar`] from `path`.
pub fn load_sidebar_from(path: &Path) -> Option<DockAreaState> {
    read(path, SIDEBAR_VERSION)
}

/// Puts an older build's sidebar (its own dock, sections in the center) in
/// `main`'s left region, open at the default width.
pub fn merge_sidebar(main: &mut DockAreaState, sidebar: DockAreaState) {
    main.left_dock = Some(DockState::new(
        sidebar.center,
        DockPlacement::Left,
        SIDEBAR_WIDTH,
        true,
    ));
}

fn read(path: &Path, version: usize) -> Option<DockAreaState> {
    let text = fs::read_to_string(path).ok()?;
    let state: DockAreaState = serde_json::from_str(&text).ok()?;
    (state.version == Some(version)).then_some(state)
}

/// Saves the layout where [`load_saved`] reads it.
pub fn write(state: &DockAreaState) -> io::Result<()> {
    let path =
        layout_path().ok_or_else(|| io::Error::other("no data directory on this platform"))?;
    write_to(&path, state)
}

/// Writes through a temp file and a rename, so a crash cannot leave half a file.
pub fn write_to(path: &Path, state: &DockAreaState) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(state).map_err(io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, json)?;
    fs::rename(&tmp, path)
}

/// `--layout` names: a fresh, empty layout that the run neither reads from
/// nor saves to the user's file. Pull requests open into it as they would
/// into a saved one. `columns` puts the sidebar's Checks section in a second
/// column beside the inbox.
pub const PRESETS: &[&str] = &["default", "columns"];
