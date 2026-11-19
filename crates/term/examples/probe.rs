//! Runs a program in a PTY, waits for it to exit and prints the final screen.
//! With `PROBE_TYPE` set it types that text (`\n` becomes Enter) after a
//! moment and prints the screen instead.
//!
//! `cargo run -p term --example probe -- pwsh -NoLogo -NoProfile -Command "Write-Host hi"`

use std::time::{Duration, Instant};

use term::{ExitState, Session, SessionConfig, visible_text};

fn main() {
    let mut args = std::env::args().skip(1);
    let mut config = SessionConfig::default();
    if let Some(program) = args.next() {
        config.program = program;
        config.args = args.collect();
    }
    let session = Session::spawn(config).expect("spawn");

    if let Ok(text) = std::env::var("PROBE_TYPE") {
        std::thread::sleep(Duration::from_secs(3));
        session.write(text.replace('\n', "\r").into_bytes());
        std::thread::sleep(Duration::from_secs(2));
    } else {
        let deadline = Instant::now() + Duration::from_secs(20);
        while session.exit_state() == ExitState::Running && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    for line in visible_text(&session.lock()) {
        println!("{line}");
    }
    println!("exit: {:?}", session.exit_state());
}
