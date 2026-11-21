//! Headless UI tests of the real shell on gpui's test platform, in one
//! binary: every module links all of gpui, so one binary links it once.
//!
//! See `docs/testing.md` for how to write one.

mod harness;

mod agent_review;
mod apply;
mod back_forward;
mod changes_requested;
mod ci_settle;
mod commits;
mod composer;
mod conversation;
mod diff_expand;
mod diff_files;
mod diff_keys;
mod dock;
mod docking;
mod drafts;
mod folds;
mod footer;
mod header;
mod inbox;
mod inbox_actions;
mod inbox_remove_repo;
mod isolation;
mod job_menus;
mod navigation;
mod palette;
mod pipelines;
mod review_marks;
mod sections;
mod settings;
mod sidebar;
mod spinners;
mod tab_keys;
mod tabs;
mod terminals;
mod thread_menus;
mod threads_chip;
mod viewed;
mod widths;
mod workspace_dirs;
mod workspace_machine;
