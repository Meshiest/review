//! Feeds recorded byte streams straight into a `Term` (no PTY, no threads) and
//! checks the resulting grid cell by cell against a stored, reviewed dump.
//!
//! `tests/fixtures/NAME.bin` is a recording (`cargo run -p term --example
//! record`); `NAME.grid` is its expected grid. After an intended emulation
//! change, regenerate the dumps with `TERM_BLESS=1 cargo test -p term --test
//! replay` and review the diff.

use std::fs;
use std::path::{Path, PathBuf};

use alacritty_terminal::Term;
use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};
use term::{Recording, dump_grid, replay, visible_text};

fn cell(term: &Term<VoidListener>, line: i32, col: usize) -> &alacritty_terminal::term::cell::Cell {
    &term.grid()[Point::new(Line(line), Column(col))]
}

const COLS: usize = 24;
const ROWS: usize = 12;

/// Colors, a screen clear, a wide character, line wrap and cursor moves.
const FIXTURE: &[u8] = b"junk that gets cleared\r\n\
\x1b[2J\x1b[H\
\x1b[31mred\x1b[0m \x1b[1;32mbold green\x1b[0m\r\n\
\x1b[38;5;208morange\x1b[0m \x1b[38;2;1;2;3mtruecolor\x1b[0m \x1b[7minv\x1b[0m\r\n\
\xe6\x97\xa5\xe6\x9c\xac!\r\n\
0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ\r\n\
\x1b[10;3Hmoved\x1b[1A\x1b[2Dup";

fn fixture() -> Term<VoidListener> {
    replay(COLS as u16, ROWS as u16, FIXTURE)
}

#[test]
fn text_survives_clear_wrap_and_cursor_moves() {
    let expected = [
        "red bold green",
        "orange truecolor inv",
        "日本!",
        "0123456789ABCDEFGHIJKLMN",
        "OPQRSTUVWXYZ",
        "",
        "",
        "",
        "     up",
        "  moved",
        "",
        "",
    ];
    assert_eq!(visible_text(&fixture()), expected.map(String::from));
}

#[test]
fn colors_and_attributes() {
    let term = fixture();
    assert_eq!(cell(&term, 0, 0).fg, Color::Named(NamedColor::Red));
    assert_eq!(cell(&term, 0, 3).fg, Color::Named(NamedColor::Foreground));
    let bold_green = cell(&term, 0, 4);
    assert_eq!(bold_green.fg, Color::Named(NamedColor::Green));
    assert!(bold_green.flags.contains(Flags::BOLD));
    assert_eq!(cell(&term, 1, 0).fg, Color::Indexed(208));
    assert_eq!(cell(&term, 1, 7).fg, Color::Spec(Rgb { r: 1, g: 2, b: 3 }));
    assert!(cell(&term, 1, 17).flags.contains(Flags::INVERSE));
    assert!(!cell(&term, 1, 16).flags.contains(Flags::INVERSE));
}

#[test]
fn wide_characters_take_two_cells() {
    let term = fixture();
    assert_eq!(cell(&term, 2, 0).c, '日');
    assert!(cell(&term, 2, 0).flags.contains(Flags::WIDE_CHAR));
    assert!(cell(&term, 2, 1).flags.contains(Flags::WIDE_CHAR_SPACER));
    assert_eq!(cell(&term, 2, 2).c, '本');
    assert_eq!(cell(&term, 2, 4).c, '!');
}

#[test]
fn long_lines_wrap_and_mark_the_wrapped_row() {
    let term = fixture();
    assert!(cell(&term, 3, COLS - 1).flags.contains(Flags::WRAPLINE));
    assert!(!cell(&term, 2, 4).flags.contains(Flags::WRAPLINE));
    assert_eq!(cell(&term, 4, 0).c, 'O');
}

#[test]
fn cursor_ends_after_the_last_write() {
    let term = fixture();
    let cursor = term.grid().cursor.point;
    assert_eq!((cursor.line, cursor.column), (Line(8), Column(7)));
}

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Replays `tests/fixtures/{name}.bin` and compares its grid with
/// `{name}.grid`. Returns the number of rows that differ.
fn differences(name: &str) -> usize {
    let dir = fixture_dir();
    let data = fs::read(dir.join(format!("{name}.bin"))).expect("recording");
    let recording = Recording::decode(&data).expect("valid recording");
    let actual = dump_grid(&recording.replay());
    // Replaying twice must give the same grid.
    assert_eq!(
        actual,
        dump_grid(&recording.replay()),
        "{name}: not deterministic"
    );

    let expected_path = dir.join(format!("{name}.grid"));
    if std::env::var_os("TERM_BLESS").is_some() {
        fs::write(&expected_path, &actual).expect("write expected grid");
        return 0;
    }
    let expected =
        fs::read_to_string(&expected_path).expect("expected grid (generate it with TERM_BLESS=1)");
    let (expected, actual): (Vec<_>, Vec<_>) =
        (expected.lines().collect(), actual.lines().collect());
    let mut differing = 0;
    for i in 0..expected.len().max(actual.len()) {
        let (want, got) = (expected.get(i), actual.get(i));
        if want != got {
            differing += 1;
            if differing <= 10 {
                eprintln!("{name} line {}: expected {want:?}, got {got:?}", i + 1);
            }
        }
    }
    differing
}

#[test]
fn pwsh_session_replays_to_the_expected_grid() {
    assert_eq!(differences("pwsh"), 0);
}

#[test]
fn claude_version_replays_to_the_expected_grid() {
    assert_eq!(differences("claude-version"), 0);
}

#[test]
fn claude_tui_start_up_replays_to_the_expected_grid() {
    assert_eq!(differences("claude-tui-running"), 0);
}

#[test]
fn claude_tui_after_quitting_replays_to_the_expected_grid() {
    assert_eq!(differences("claude-tui"), 0);
}

#[test]
fn the_pwsh_recording_keeps_its_scrollback_and_colors() {
    let data = fs::read(fixture_dir().join("pwsh.bin")).expect("recording");
    let term = Recording::decode(&data).expect("valid recording").replay();
    let text = visible_text(&term);
    assert!(text.iter().any(|row| row.contains("wide ok")));
    assert!(
        term.grid().history_size() > 0,
        "output scrolled off the top"
    );
    let dump = dump_grid(&term);
    assert!(dump.contains("fg=i10"), "the green Write-Host output");
    assert!(dump.contains("WRAPLINE"), "the long line wrapped");
    assert!(dump.contains("WIDE_CHAR_SPACER"));
}
