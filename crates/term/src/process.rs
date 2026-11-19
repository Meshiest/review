//! Plain child processes, without a PTY, started in a checkout someone else
//! wrote: how to start one so it cannot pick up a program planted there, and
//! how to end everything it started.

use std::process::Command;

use crate::reap::Tree;

/// Prepares `command` for a child whose working directory may be someone
/// else's checkout. On Windows a bare program name the child runs is not
/// looked up in that directory first, and no console window opens; on Unix
/// the child leads its own process group, so [`ProcessTree::kill`] reaches
/// everything it started.
pub fn isolate(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command
            .env("NoDefaultCurrentDirectoryInExePath", "1")
            .creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
}

/// A started child and whatever it starts in turn: on Windows a kill-on-close
/// job holds them, on Unix the child's process group.
pub struct ProcessTree(Tree);

impl ProcessTree {
    /// Takes hold of the child `pid`, started with [`isolate`]. `None` when it
    /// is gone already.
    #[must_use]
    pub fn adopt(pid: u32) -> Option<Self> {
        Tree::adopt(pid).map(Self)
    }

    /// Ends the child and everything it started, without waiting.
    pub fn kill(&self) {
        #[cfg(windows)]
        self.0.kill();
        #[cfg(unix)]
        self.0.signal("KILL");
    }
}
