//! A session keeps the configured number of history lines, and the emulator
//! scrolls into them: this runs a real program in a PTY.

use std::time::{Duration, Instant};

use term::alacritty_terminal::Term;
use term::alacritty_terminal::grid::{Dimensions, Scroll};
use term::alacritty_terminal::index::{Column, Line, Point};
use term::{
    DEFAULT_SCROLLBACK_LINES, ExitState, GridSize, Listener, Session, SessionConfig, visible_text,
};

const LINES: usize = 300;

/// A program that prints `line 1` to `line 300` and exits.
fn counter() -> (String, Vec<String>) {
    if cfg!(windows) {
        let script = format!("for /l %i in (1,1,{LINES}) do @echo line %i");
        ("cmd".to_owned(), vec!["/c".to_owned(), script])
    } else {
        let script = format!("i=1; while [ $i -le {LINES} ]; do echo line $i; i=$((i+1)); done");
        ("sh".to_owned(), vec!["-c".to_owned(), script])
    }
}

fn run_to_the_end(scrollback_lines: usize) -> Session {
    let (program, args) = counter();
    let session = Session::spawn(SessionConfig {
        program,
        args,
        scrollback_lines,
        size: GridSize {
            cols: 40,
            rows: 10,
            ..GridSize::default()
        },
        ..SessionConfig::default()
    })
    .expect("start the program");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let done = session.exit_state() != ExitState::Running
            && visible_text(&session.lock())
                .iter()
                .any(|row| row == &format!("line {LINES}"));
        if done {
            return session;
        }
        assert!(Instant::now() < deadline, "the program did not finish");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn history_stops_at_the_configured_limit() {
    let session = run_to_the_end(50);
    assert_eq!(session.lock().grid().history_size(), 50);
}

/// The text of grid line `line` (history is negative), trailing blanks trimmed.
fn grid_row(term: &Term<Listener>, line: i32) -> String {
    let text: String = (0..term.columns())
        .map(|col| term.grid()[Point::new(Line(line), Column(col))].c)
        .collect();
    text.trim_end().to_owned()
}

#[test]
fn the_history_holds_the_lines_just_above_the_screen() {
    let session = run_to_the_end(50);
    let term = session.lock();
    let top: usize = visible_text(&term)[0]
        .strip_prefix("line ")
        .and_then(|n| n.parse().ok())
        .expect("a numbered line on the first row");
    assert_eq!(grid_row(&term, -1), format!("line {}", top - 1));
    assert_eq!(grid_row(&term, -50), format!("line {}", top - 50));
}

#[test]
fn scrolling_back_moves_the_display_and_the_end_returns() {
    let session = run_to_the_end(50);
    let mut term = session.lock();
    term.scroll_display(Scroll::Top);
    assert_eq!(term.grid().display_offset(), 50);
    term.scroll_display(Scroll::PageDown);
    assert_eq!(term.grid().display_offset(), 50 - term.screen_lines());
    term.scroll_display(Scroll::Bottom);
    assert_eq!(term.grid().display_offset(), 0);
}

#[test]
fn the_default_keeps_ten_thousand_lines() {
    assert_eq!(DEFAULT_SCROLLBACK_LINES, 10_000);
    assert_eq!(
        SessionConfig::default().scrollback_lines,
        DEFAULT_SCROLLBACK_LINES
    );
    let session = run_to_the_end(DEFAULT_SCROLLBACK_LINES);
    assert!(session.lock().grid().history_size() >= LINES - 10 - 1);
}
