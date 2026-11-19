//! Terminal sessions: emulation and PTYs for a local shell or ssh.
//!
//! `alacritty_terminal` owns the emulator and the PTY (ConPTY on Windows); a
//! background thread reads the PTY and feeds the emulator. The UI never waits
//! on it: it takes a short lock to read the grid and gets a coalesced wakeup
//! when something changed.

mod listener;
pub mod process;
pub mod program;
mod reap;
mod replay;
mod session;
mod text;

pub use alacritty_terminal;
pub use listener::Listener;
pub use replay::{Recording, RecordingError, dump_grid, replay};
pub use session::{
    DEFAULT_SCROLLBACK_LINES, ExitState, GridSize, MAX_SCROLLBACK_LINES, Session, SessionConfig,
    SpawnError, TermGuard,
};
pub use text::visible_text;
