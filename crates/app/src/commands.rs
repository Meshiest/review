//! Every key and command of the app in one table: its group, its keys, what
//! the `?` overlay says about it, and its title and action in the command
//! palette. Key bindings live in [`crate::bind_keys`]; this table is what
//! the overlay and the palette read, so a command is described once.

use gpui::Action;

use crate::composer::{SubmitNow, SubmitPrimary};
use crate::diff_panel::{
    NextFile, NextHunk, NextInStack, NextThread, OpenRangeComposer, PrevFile, PrevHunk,
    PrevInStack, PrevThread, ToggleAllFilesCollapsed, ToggleFileCollapsed, ToggleFileViewed,
    ToggleThread,
};
use crate::shell::{
    CloseTab, GoBack, GoForward, NextTab, OpenCommandPalette, OpenSettings, PrevTab, QuickOpen,
    ToggleDiffLayout, ToggleKeyHelp, ToggleSidebar,
};
use crate::sidebar::{OpenBeside, OpenSelected, SelectNext, SelectPrev, ToggleViewed};
use crate::terminals::NewTerminal;
use gpui_kit::component::dock::ToggleZoom;

/// Where a key works, as the `?` overlay groups them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    Inbox,
    Diff,
    CommentBox,
    Window,
}

impl Group {
    pub const ALL: [Self; 4] = [Self::Inbox, Self::Diff, Self::CommentBox, Self::Window];

    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            Self::Inbox => "Inbox",
            Self::Diff => "Diff",
            Self::CommentBox => "Comment box",
            Self::Window => "Window",
        }
    }

    /// Why one of the group's commands cannot run where focus is.
    fn needs(self) -> &'static str {
        match self {
            Self::Inbox => "Focus the inbox first",
            Self::Diff => "Focus a diff first",
            Self::CommentBox => "Focus a comment box first",
            Self::Window => "Not available here",
        }
    }
}

/// What a row of the table does.
#[derive(Clone, Copy)]
pub enum Run {
    /// Dispatches this action where focus is.
    Action(fn() -> Box<dyn Action>),
    /// A key the overlay lists that is no command: a click, Esc in a picker.
    Key,
}

/// One key or command.
#[derive(Clone, Copy)]
pub struct Command {
    pub group: Group,
    /// As the overlay shows them; alternatives separated by two spaces.
    pub keys: &'static str,
    /// What the key does, for the overlay.
    pub help: &'static str,
    /// Its title in the palette; empty for one the palette does not offer.
    pub title: &'static str,
    pub run: Run,
    /// Why it cannot run where focus is, when its group's reason is wrong.
    needs: Option<&'static str>,
}

fn boxed<A: Action + Default>() -> Box<dyn Action> {
    Box::new(A::default())
}

const fn command(
    group: Group,
    keys: &'static str,
    help: &'static str,
    title: &'static str,
    action: fn() -> Box<dyn Action>,
) -> Command {
    Command {
        group,
        keys,
        help,
        title,
        run: Run::Action(action),
        needs: None,
    }
}

const fn key(group: Group, keys: &'static str, help: &'static str) -> Command {
    Command {
        group,
        keys,
        help,
        title: "",
        run: Run::Key,
        needs: None,
    }
}

const STACK: &str = "Focus the inbox or a diff first";

use Group::{CommentBox, Diff, Inbox, Window};

/// Every key and command, grouped, in the order the overlay lists them.
pub const COMMANDS: &[Command] = &[
    command(
        Inbox,
        "j  Down",
        "next pull request",
        "Next pull request",
        boxed::<SelectNext>,
    ),
    command(
        Inbox,
        "k  Up",
        "previous pull request",
        "Previous pull request",
        boxed::<SelectPrev>,
    ),
    key(
        Inbox,
        "click",
        "preview its conversation; again for the diff",
    ),
    command(
        Inbox,
        "Enter",
        "open its diff, or show the selected thread",
        "Open the selected pull request",
        boxed::<OpenSelected>,
    ),
    command(
        Inbox,
        "Ctrl+Enter",
        "open its diff beside the others",
        "Open the selected pull request beside the others",
        boxed::<OpenBeside>,
    ),
    command(
        Inbox,
        "v",
        "mark the selected file viewed",
        "Mark the selected file viewed",
        boxed::<ToggleViewed>,
    ),
    command(
        Diff,
        "] f  Alt+Down",
        "next file",
        "Next file",
        boxed::<NextFile>,
    ),
    command(
        Diff,
        "[ f  Alt+Up",
        "previous file",
        "Previous file",
        boxed::<PrevFile>,
    ),
    command(Diff, "n", "next hunk", "Next hunk", boxed::<NextHunk>),
    command(
        Diff,
        "p",
        "previous hunk",
        "Previous hunk",
        boxed::<PrevHunk>,
    ),
    command(
        Diff,
        "] t",
        "next thread",
        "Next thread",
        boxed::<NextThread>,
    ),
    command(
        Diff,
        "[ t",
        "previous thread",
        "Previous thread",
        boxed::<PrevThread>,
    ),
    Command {
        needs: Some(STACK),
        ..command(
            Diff,
            "] s",
            "next pull request down the stack",
            "Next pull request down the stack",
            boxed::<NextInStack>,
        )
    },
    Command {
        needs: Some(STACK),
        ..command(
            Diff,
            "[ s",
            "next pull request up the stack",
            "Next pull request up the stack",
            boxed::<PrevInStack>,
        )
    },
    command(
        Diff,
        "x",
        "fold or unfold the thread",
        "Fold or unfold the thread",
        boxed::<ToggleThread>,
    ),
    command(
        Diff,
        "v",
        "mark the file at the top viewed",
        "Mark the file at the top viewed",
        boxed::<ToggleFileViewed>,
    ),
    command(
        Diff,
        "z",
        "fold or unfold the file at the top",
        "Fold or unfold the file at the top",
        boxed::<ToggleFileCollapsed>,
    ),
    command(
        Diff,
        "Shift+Z",
        "fold every file, or unfold them all",
        "Fold every file, or unfold them all",
        boxed::<ToggleAllFilesCollapsed>,
    ),
    command(
        Diff,
        "s",
        "unified or split",
        "Unified or split diff",
        boxed::<ToggleDiffLayout>,
    ),
    key(Diff, "c", "comment on the line under the pointer"),
    command(
        Diff,
        "Shift+C",
        "comment on the lines last dragged over",
        "Comment on the lines last dragged over",
        boxed::<OpenRangeComposer>,
    ),
    command(
        CommentBox,
        "Ctrl+Enter",
        "add to the review",
        "Add the comment to the review",
        boxed::<SubmitPrimary>,
    ),
    command(
        CommentBox,
        "Ctrl+Shift+Enter",
        "send as a single comment",
        "Send the comment on its own",
        boxed::<SubmitNow>,
    ),
    key(CommentBox, "Esc", "close it; the draft is kept"),
    command(
        Window,
        "Ctrl+Shift+P",
        "every command",
        "",
        boxed::<OpenCommandPalette>,
    ),
    command(
        Window,
        "Ctrl+P",
        "go to a pull request or a file by name",
        "Go to a pull request or a file",
        boxed::<QuickOpen>,
    ),
    command(
        Window,
        "Alt+Left  mouse back",
        "back to where the last jump left from",
        "Go back",
        boxed::<GoBack>,
    ),
    command(
        Window,
        "Alt+Right  mouse forward",
        "forward again, after going back",
        "Go forward",
        boxed::<GoForward>,
    ),
    command(
        Window,
        "Ctrl+B",
        "show or hide the sidebar",
        "Show or hide the sidebar",
        boxed::<ToggleSidebar>,
    ),
    command(
        Window,
        "Ctrl+,",
        "settings: theme and fonts",
        "Open the settings",
        boxed::<OpenSettings>,
    ),
    command(
        Window,
        "Ctrl+W",
        "close the tab",
        "Close the tab",
        boxed::<CloseTab>,
    ),
    key(Window, "middle click", "close the tab clicked"),
    command(
        Window,
        "Ctrl+Tab",
        "the tab to the right in the group, wrapping",
        "Next tab",
        boxed::<NextTab>,
    ),
    command(
        Window,
        "Ctrl+Shift+Tab",
        "the tab to the left in the group, wrapping",
        "Previous tab",
        boxed::<PrevTab>,
    ),
    command(
        Window,
        "Shift+Esc",
        "the tab group fills the window, or not",
        "Fill the window with the tab group, or not",
        boxed::<ToggleZoom>,
    ),
    key(Window, "Esc", "while it fills the window: back"),
    command(
        Window,
        "Ctrl+`",
        "new terminal, here or on a machine",
        "New terminal",
        boxed::<NewTerminal>,
    ),
    command(
        Window,
        "?",
        "this list",
        "Show every key",
        boxed::<ToggleKeyHelp>,
    ),
    key(Window, "Esc", "close it"),
];

impl Command {
    /// The palette's id for it, stable across openings.
    #[must_use]
    pub fn id(&self) -> String {
        format!("command:{}", self.title)
    }

    /// The action it runs, when it is one the palette offers.
    #[must_use]
    pub fn action(&self) -> Option<Box<dyn Action>> {
        match self.run {
            Run::Action(action) if !self.title.is_empty() => Some(action()),
            Run::Action(_) | Run::Key => None,
        }
    }

    /// Why it cannot run where focus is.
    #[must_use]
    pub fn needs(&self) -> &'static str {
        self.needs.unwrap_or_else(|| self.group.needs())
    }
}

/// The `?` overlay's rows: each group's keys, in table order.
pub fn key_help() -> impl Iterator<Item = (Group, Vec<&'static Command>)> {
    Group::ALL.into_iter().map(|group| {
        let rows = COMMANDS
            .iter()
            .filter(|command| command.group == group)
            .collect();
        (group, rows)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_group_has_keys_and_every_palette_title_is_unique() {
        for (group, rows) in key_help() {
            assert!(!rows.is_empty(), "{} has no keys", group.title());
            assert!(
                rows.iter()
                    .all(|row| !row.keys.is_empty() && !row.help.is_empty())
            );
        }
        let mut titles: Vec<&str> = COMMANDS
            .iter()
            .filter(|command| command.action().is_some())
            .map(|command| command.title)
            .collect();
        let count = titles.len();
        titles.sort_unstable();
        titles.dedup();
        assert_eq!(titles.len(), count, "two commands share a title");
    }

    #[test]
    fn keys_and_click_hints_are_not_palette_commands() {
        let offered: Vec<&str> = COMMANDS
            .iter()
            .filter(|command| command.action().is_some())
            .map(|command| command.keys)
            .collect();
        assert!(!offered.contains(&"click"));
        assert!(
            !offered.contains(&"Ctrl+Shift+P"),
            "the palette does not list itself"
        );
        assert!(offered.contains(&"Ctrl+B"));
    }
}
