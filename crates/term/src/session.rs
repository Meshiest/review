use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use alacritty_terminal::Term;
use alacritty_terminal::event::WindowSize;
use alacritty_terminal::event_loop::{EventLoop, Msg};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Config as TermConfig;
use alacritty_terminal::tty::{self, Shell};
use thiserror::Error;

use crate::listener::{Listener, Shared};
use crate::reap::{self, Tree};

/// Size of the terminal in cells, plus the cell size in pixels (which
/// programs can query).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GridSize {
    pub cols: u16,
    pub rows: u16,
    pub cell_width: u16,
    pub cell_height: u16,
}

impl Default for GridSize {
    fn default() -> Self {
        Self {
            cols: 80,
            rows: 24,
            cell_width: 8,
            cell_height: 16,
        }
    }
}

impl GridSize {
    fn clamped(self) -> Self {
        Self {
            cols: self.cols.max(2),
            rows: self.rows.max(1),
            ..self
        }
    }

    fn window_size(self) -> WindowSize {
        WindowSize {
            num_lines: self.rows,
            num_cols: self.cols,
            cell_width: self.cell_width,
            cell_height: self.cell_height,
        }
    }
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.rows.into()
    }

    fn screen_lines(&self) -> usize {
        self.rows.into()
    }

    fn columns(&self) -> usize {
        self.cols.into()
    }
}

/// What to run in the PTY: a shell, or `ssh` with the host in `args`.
#[derive(Clone)]
pub struct SessionConfig {
    /// Should be an absolute path (see [`crate::program::resolve`]): Windows looks
    /// for a bare name in the app's own and current directories first.
    pub program: String,
    pub args: Vec<String>,
    /// Windows: the args are joined with spaces as they are, not quoted, for
    /// `cmd`, whose `/K` line does not follow the usual quoting rules.
    pub raw_args: bool,
    pub cwd: Option<PathBuf>,
    /// Added to the inherited environment.
    pub env: HashMap<String, String>,
    pub size: GridSize,
    /// Lines kept above the screen, at most [`MAX_SCROLLBACK_LINES`].
    pub scrollback_lines: usize,
    /// Writes every byte the program prints to `alacritty.recording` in the
    /// process's working directory, for recorded-replay tests.
    pub record_output: bool,
    /// Called from the PTY thread when the screen changed. Coalesced: after
    /// one call there are no more until the UI reads the grid with
    /// [`Session::lock_for_paint`]. Keep it cheap (wake a channel).
    pub on_change: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            program: default_program(),
            args: Vec::new(),
            raw_args: false,
            cwd: None,
            env: HashMap::new(),
            size: GridSize::default(),
            scrollback_lines: DEFAULT_SCROLLBACK_LINES,
            record_output: false,
            on_change: None,
        }
    }
}

/// Lines of history kept by default.
pub const DEFAULT_SCROLLBACK_LINES: usize = 10_000;

/// The most lines of history a session keeps, whatever the config asks for.
pub const MAX_SCROLLBACK_LINES: usize = 100_000;

/// The program as the first word of a Windows command line, which the PTY
/// does not quote: a path with a space is quoted here, or
/// `C:\Program Files\x.exe` would start `C:\Program.exe`.
fn command_word(program: &str) -> String {
    if cfg!(windows) && program.contains(char::is_whitespace) && !program.starts_with('"') {
        format!("\"{program}\"")
    } else {
        program.to_owned()
    }
}

fn default_program() -> String {
    if cfg!(windows) {
        "pwsh".to_owned()
    } else {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_owned())
    }
}

#[derive(Debug, Error)]
pub enum SpawnError {
    #[error("failed to start `{program}` in a PTY")]
    Pty {
        program: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to start the PTY event loop")]
    EventLoop(#[source] io::Error),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitState {
    Running,
    /// The status is `None` when the PTY closed without reporting one.
    Exited(Option<ExitStatus>),
}

/// A program running in a PTY, with its emulated screen.
pub struct Session {
    term: Arc<FairMutex<Term<Listener>>>,
    shared: Arc<Shared>,
    size: Mutex<GridSize>,
    /// The program's process id, when the system reported one.
    pid: Option<u32>,
    /// The child's process tree, until the session closes.
    tree: Mutex<Option<Tree>>,
    closed: AtomicBool,
}

/// Guard over the emulator; hold it only while reading.
pub type TermGuard<'a> = parking_lot::MutexGuard<'a, Term<Listener>>;

impl Session {
    /// Starts `config.program` in a new PTY and the thread that reads it.
    ///
    /// # Errors
    /// When the program cannot be started.
    pub fn spawn(config: SessionConfig) -> Result<Self, SpawnError> {
        let size = config.size.clamped();
        let shared = Arc::new(Shared::new(config.on_change, size.window_size()));
        let listener = Listener(Arc::clone(&shared));

        let mut env = HashMap::from([
            ("TERM".to_owned(), "xterm-256color".to_owned()),
            ("COLORTERM".to_owned(), "truecolor".to_owned()),
        ]);
        env.extend(config.env);
        // A Windows child that runs a bare command name (`cmd` does) must not
        // look in its working directory first: that may be a checkout of
        // someone else's code.
        #[cfg(windows)]
        env.insert(
            "NoDefaultCurrentDirectoryInExePath".to_owned(),
            "1".to_owned(),
        );

        let options = tty::Options {
            shell: Some(Shell::new(command_word(&config.program), config.args)),
            working_directory: config.cwd,
            drain_on_exit: true,
            env,
            #[cfg(windows)]
            escape_args: !config.raw_args,
        };

        let term_config = TermConfig {
            scrolling_history: config.scrollback_lines.min(MAX_SCROLLBACK_LINES),
            ..TermConfig::default()
        };
        let term = Arc::new(FairMutex::new(Term::new(
            term_config,
            &size,
            listener.clone(),
        )));
        let pty = tty::new(&options, size.window_size(), 0).map_err(|source| SpawnError::Pty {
            program: config.program,
            source,
        })?;
        let pid = child_id(&pty);
        let tree = pid.and_then(Tree::adopt);
        let event_loop =
            EventLoop::new(Arc::clone(&term), listener, pty, true, config.record_output)
                .map_err(SpawnError::EventLoop)?;
        shared.set_sender(event_loop.channel());
        // The thread ends on child exit or `Msg::Shutdown`. Nothing joins it:
        // dropping a ConPTY can block until its output is drained.
        drop(event_loop.spawn());

        Ok(Self {
            term,
            shared,
            size: Mutex::new(size),
            pid,
            tree: Mutex::new(tree),
            closed: AtomicBool::new(false),
        })
    }

    /// Counts the screen changes so far. A painter that saw the same number
    /// last frame, with the same size and scroll position, has nothing new to
    /// draw. Read it before locking the grid.
    pub fn generation(&self) -> u64 {
        self.shared.generation()
    }

    /// Locks the emulator. The PTY thread waits while it is held.
    pub fn lock(&self) -> TermGuard<'_> {
        self.term.lock()
    }

    /// Like [`lock`](Self::lock), and re-arms the change notification: call
    /// it when painting, so output that arrives meanwhile wakes the UI again.
    pub fn lock_for_paint(&self) -> TermGuard<'_> {
        self.shared.clear_dirty();
        self.term.lock()
    }

    /// Sends bytes to the program's stdin.
    pub fn write(&self, bytes: impl Into<Vec<u8>>) {
        let bytes: Vec<u8> = bytes.into();
        if bytes.is_empty() {
            return;
        }
        if let Some(sender) = self.shared.sender() {
            let _ = sender.send(Msg::Input(bytes.into()));
        }
    }

    /// Resizes the emulator and the PTY. Does nothing when the size is unchanged.
    pub fn resize(&self, size: GridSize) {
        let size = size.clamped();
        {
            let mut current = self.size.lock().unwrap_or_else(PoisonError::into_inner);
            if *current == size {
                return;
            }
            *current = size;
        }
        self.shared.set_size(size.window_size());
        self.term.lock().resize(size);
        if let Some(sender) = self.shared.sender() {
            let _ = sender.send(Msg::Resize(size.window_size()));
        }
    }

    pub fn size(&self) -> GridSize {
        *self.size.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The process id of the program (the shell, or `ssh`), if the system reported one.
    pub fn process_id(&self) -> Option<u32> {
        self.pid
    }

    pub fn exit_state(&self) -> ExitState {
        self.shared.exit_state()
    }

    /// The exit status, once the program has exited and the system reported one.
    pub fn exit_status(&self) -> Option<ExitStatus> {
        match self.exit_state() {
            ExitState::Exited(status) => status,
            ExitState::Running => None,
        }
    }

    /// The title the program set with an escape sequence.
    pub fn title(&self) -> Option<String> {
        self.shared.title()
    }
}

impl Session {
    /// Ends the program and everything it started. Never waits: the PTY is
    /// closed, and a thread in the background kills what is left after a
    /// short grace period. [`exit_state`](Self::exit_state) then reports the
    /// exit status and the change notification fires. Does nothing if called
    /// before.
    pub fn close(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        let tree = self
            .tree
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        reap::spawn_reaper(tree, Arc::clone(&self.shared));
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(windows)]
fn child_id(pty: &tty::Pty) -> Option<u32> {
    pty.child_watcher().pid().map(std::num::NonZeroU32::get)
}

#[cfg(unix)]
fn child_id(pty: &tty::Pty) -> Option<u32> {
    Some(pty.child().id())
}
