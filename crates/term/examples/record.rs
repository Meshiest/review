//! Records what a program prints into a PTY, for the replay test.
//!
//! ```text
//! cargo run -p term --example record -- OUT.bin [--size 100x30] [--secs 8]
//!     [--cwd DIR] [--cut MS] [--send MS:TEXT]... -- PROGRAM ARGS...
//! ```
//!
//! The recorder owns the PTY: every `--send` types TEXT into the program MS
//! milliseconds after the start, through the PTY, never through the operating
//! system's input. TEXT understands `\r \n \t \e \\` and `\xNN` (`\x03` is
//! Ctrl+C). The recording ends when the program exits or after `--secs`,
//! whichever is first; then the program is closed. `--cut MS` also keeps what
//! had been printed MS milliseconds in, as OUT-running.bin, for programs whose
//! exit erases their screen. The raw bytes plus the size go to OUT.bin, and the
//! final screen is printed so it can be checked before it is kept.
//!
//! The fixtures in `tests/fixtures` were made with (`-NoProfile`, so a prompt
//! theme does not put this machine's host name into the recording):
//!
//! ```text
//! record pwsh.bin --size 100x24 --secs 25 --send '2500:Clear-Host\r'
//!     --send '4500:Write-Host "build ok" -ForegroundColor Green; Write-Host "warning: red on blue" -ForegroundColor Red -BackgroundColor Blue\r'
//!     --send '6500:"0123456789" * 24\r' --send '8000:Write-Output "日本語 wide ok"\r'
//!     --send '9500:1..14 | ForEach-Object { "line $_" }\r' --send '11500:exit\r'
//!     -- pwsh -NoLogo -NoProfile
//! record claude-version.bin --size 100x24 --secs 15 -- claude --version
//! record claude-tui.bin --size 120x36 --secs 14 --cut 7000 --cwd ~/Downloads/temp
//!     --send '9000:\x03' --send '9800:\x03' -- claude
//! ```
//!
//! `claude` shows a start-up screen that changes between runs (the placeholder
//! in its prompt is random), so each fixture is recorded once and its expected
//! grid reviewed by hand; the test never runs the program.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use term::{ExitState, GridSize, Recording, Session, SessionConfig, dump_grid, replay};

/// Where the emulator drops the raw output (`SessionConfig::record_output`).
const RAW_FILE: &str = "alacritty.recording";

struct Args {
    out: PathBuf,
    cols: u16,
    rows: u16,
    secs: u64,
    cwd: Option<PathBuf>,
    cut: Option<u64>,
    sends: Vec<(u64, Vec<u8>)>,
    program: Vec<String>,
}

fn main() {
    let args = parse().unwrap_or_else(|message| {
        eprintln!("{message}");
        std::process::exit(2);
    });
    let out = std::path::absolute(&args.out).expect("output path");
    let cwd = std::env::current_dir().expect("working directory");
    // The emulator writes its raw capture into the working directory.
    let scratch = cwd.join(".scratch").join("record");
    fs::create_dir_all(&scratch).expect("scratch directory");
    std::env::set_current_dir(&scratch).expect("enter scratch directory");
    let _ = fs::remove_file(RAW_FILE);

    let mut config = SessionConfig {
        cwd: Some(args.cwd.clone().unwrap_or(cwd)),
        record_output: true,
        size: GridSize {
            cols: args.cols,
            rows: args.rows,
            ..GridSize::default()
        },
        ..SessionConfig::default()
    };
    let mut program = args.program.iter().cloned();
    if let Some(name) = program.next() {
        config.program = name;
        config.args = program.collect();
    }
    let session = Session::spawn(config).expect("start the program");

    let start = Instant::now();
    let mut sends = args.sends.clone();
    sends.sort_by_key(|(at, _)| *at);
    let mut sends = sends.into_iter().peekable();
    let deadline = Duration::from_secs(args.secs);
    let mut cut_len = None;
    while start.elapsed() < deadline && session.exit_state() == ExitState::Running {
        if cut_len.is_none()
            && args
                .cut
                .is_some_and(|at| Duration::from_millis(at) <= start.elapsed())
        {
            cut_len = fs::metadata(RAW_FILE).ok().map(|meta| meta.len() as usize);
        }
        while let Some((_, text)) =
            sends.next_if(|(at, _)| Duration::from_millis(*at) <= start.elapsed())
        {
            session.write(text);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(300));
    let natural = session.exit_state();
    let bytes = fs::read(RAW_FILE).expect("raw recording");
    session.close();
    let settle = Instant::now();
    while session.exit_state() == ExitState::Running && settle.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(20));
    }

    if let Some(dir) = out.parent() {
        fs::create_dir_all(dir).expect("output directory");
    }
    if let Some(len) = cut_len {
        let running = Recording {
            cols: args.cols,
            rows: args.rows,
            bytes: bytes[..len.min(bytes.len())].to_vec(),
        };
        let name = format!(
            "{}-running.bin",
            out.file_stem().unwrap_or_default().to_string_lossy()
        );
        fs::write(out.with_file_name(name), running.encode()).expect("write the cut recording");
        println!("{}", dump_grid(&running.replay()));
    }
    let recording = Recording {
        cols: args.cols,
        rows: args.rows,
        bytes,
    };
    fs::write(&out, recording.encode()).expect("write the recording");
    println!(
        "{}",
        dump_grid(&replay(args.cols, args.rows, &recording.bytes))
    );
    println!(
        "recorded {} bytes to {}; exit before close: {natural:?}; after close: {:?}",
        recording.bytes.len(),
        out.display(),
        session.exit_state()
    );
}

fn parse() -> Result<Args, String> {
    let mut it = std::env::args().skip(1);
    let out = PathBuf::from(
        it.next()
            .ok_or("usage: record OUT.bin [options] -- PROGRAM ARGS")?,
    );
    let mut args = Args {
        out,
        cols: 100,
        rows: 30,
        secs: 8,
        cwd: None,
        cut: None,
        sends: Vec::new(),
        program: Vec::new(),
    };
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--size" => {
                let value = it.next().ok_or("--size needs COLSxROWS")?;
                let (cols, rows) = value.split_once('x').ok_or("--size needs COLSxROWS")?;
                args.cols = cols.parse().map_err(|_| "bad columns")?;
                args.rows = rows.parse().map_err(|_| "bad rows")?;
            }
            "--secs" => {
                args.secs = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .ok_or("--secs needs a number")?;
            }
            "--cut" => {
                args.cut = Some(
                    it.next()
                        .and_then(|v| v.parse().ok())
                        .ok_or("--cut needs milliseconds")?,
                );
            }
            "--cwd" => args.cwd = Some(it.next().ok_or("--cwd needs a directory")?.into()),
            "--send" => {
                let value = it.next().ok_or("--send needs MS:TEXT")?;
                let (at, text) = value.split_once(':').ok_or("--send needs MS:TEXT")?;
                args.sends
                    .push((at.parse().map_err(|_| "bad send time")?, unescape(text)?));
            }
            "--" => {
                args.program = it.by_ref().collect();
            }
            other => return Err(format!("unknown option {other}")),
        }
    }
    Ok(args)
}

fn unescape(text: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            let mut buf = [0; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        match chars.next() {
            Some('r') => out.push(b'\r'),
            Some('n') => out.push(b'\n'),
            Some('t') => out.push(b'\t'),
            Some('e') => out.push(0x1b),
            Some('\\') => out.push(b'\\'),
            Some('x') => {
                let hex: String = chars.by_ref().take(2).collect();
                out.push(u8::from_str_radix(&hex, 16).map_err(|_| "bad \\x escape")?);
            }
            _ => return Err("bad escape in --send".to_owned()),
        }
    }
    Ok(out)
}
