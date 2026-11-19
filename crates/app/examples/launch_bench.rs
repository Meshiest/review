//! Launches the release `review` binary again and again against a warm cache
//! and times each launch to the first inbox painted from the cache; prints a
//! `PERF` line and exits.
//!
//! cargo run -p app --release --example launch_bench -- [--runs 12]
//!
//! The binary is built first (`cargo build --release -p app --bin review`).
//! One untimed run fills the cache, then each timed run starts a fresh
//! process with a fresh window and `--report-launch`: the app prints the
//! wall-clock end of the first frame that painted cached inbox rows and quits
//! on its own (see `app::launch`). A sample runs from just before the spawn
//! call to that frame's end, so it covers process creation too.
//! `launch_to_inbox`: p95 against 350 ms.
//!
//! It reads the user's forge config and talks to the live forges, but keeps
//! its cache, avatars, settings and layout under `.scratch/launch-bench`,
//! never the user's own; the untimed fill run fills that cache. It opens
//! real windows.

use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use app::launch::{Cache, LaunchReport};

const BUDGET_MS: f64 = 350.0;
/// Longest wait for one run's report, cold runs included.
const RUN_TIMEOUT: Duration = Duration::from_secs(90);
/// Longest wait for a run to exit after its report.
const EXIT_TIMEOUT: Duration = Duration::from_secs(15);

fn runs_arg() -> usize {
    let mut runs = 12;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--runs" => {
                runs = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .filter(|&n| n > 0)
                    .expect("--runs takes a positive number");
            }
            other => panic!("unknown argument {other}; expected --runs N"),
        }
    }
    runs
}

/// `target/release/review(.exe)`, next to this example's directory.
fn review_binary() -> PathBuf {
    let exe = std::env::current_exe().expect("own path");
    let release = exe
        .parent()
        .and_then(Path::parent)
        .expect("examples sit in the profile directory");
    release.join(format!("review{}", std::env::consts::EXE_SUFFIX))
}

fn build_review() {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .args(["build", "--release", "-p", "app", "--bin", "review"])
        .status()
        .expect("run cargo");
    assert!(status.success(), "building review failed");
}

fn unix_us(at: SystemTime) -> u128 {
    at.duration_since(UNIX_EPOCH)
        .expect("after the epoch")
        .as_micros()
}

/// Waits for the child to quit, killing it after [`EXIT_TIMEOUT`].
fn reap(child: &mut Child) -> bool {
    let deadline = std::time::Instant::now() + EXIT_TIMEOUT;
    while std::time::Instant::now() < deadline {
        if let Ok(Some(_)) = child.try_wait() {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
    false
}

/// The bench's own data dir, settings and layout.
fn scratch_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.scratch/launch-bench")
}

/// One launch: the report and the time from spawn to the painted frame.
fn launch(binary: &Path) -> Result<(LaunchReport, Duration), String> {
    let spawned_at = SystemTime::now();
    let dir = scratch_dir();
    let mut child = Command::new(binary)
        .args(["--report-launch", "--unthrottled"])
        .arg("--data-dir")
        .arg(dir.join("data"))
        .arg("--settings")
        .arg(dir.join("settings.toml"))
        .arg("--layout-file")
        .arg(dir.join("layout.json"))
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn {}: {e}", binary.display()))?;
    let stdout = child.stdout.take().expect("piped stdout");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some(report) = LaunchReport::parse(&line) {
                let _ = tx.send(report);
            }
        }
    });
    let report = rx.recv_timeout(RUN_TIMEOUT);
    let exited = reap(&mut child);
    let report = report.map_err(|_| format!("no report within {RUN_TIMEOUT:?}"))?;
    if !exited {
        return Err("the app did not quit after its report".to_owned());
    }
    let micros = report.painted_unix_us.saturating_sub(unix_us(spawned_at));
    let elapsed = Duration::from_micros(u64::try_from(micros).unwrap_or(u64::MAX));
    Ok((report, elapsed))
}

fn main() {
    let runs = runs_arg();
    build_review();
    let binary = review_binary();

    match launch(&binary) {
        Ok((report, _)) => eprintln!("fill run: cache {:?}, {} rows", report.cache, report.rows),
        Err(error) => eprintln!("fill run: {error}"),
    }

    let mut samples = perf::Samples::with_capacity(runs);
    let mut main_ms = perf::Samples::with_capacity(runs);
    let mut failures = Vec::new();
    let mut rows = 0;
    for run in 0..runs {
        match launch(&binary) {
            Ok((report, elapsed)) if report.cache == Cache::Warm => {
                eprintln!(
                    "run {run}: {:.1} ms from spawn, {:.1} ms from main, {} rows",
                    elapsed.as_secs_f64() * 1000.0,
                    report.main_to_paint_ms,
                    report.rows
                );
                rows = report.rows;
                samples.push(elapsed);
                main_ms.push(Duration::from_secs_f64(report.main_to_paint_ms / 1000.0));
            }
            Ok(_) => failures.push(format!("run {run}: the cache was cold")),
            Err(error) => failures.push(format!("run {run}: {error}")),
        }
    }
    for failure in &failures {
        eprintln!("{failure}");
    }
    let main_report = main_ms.report("main_to_paint", 95.0, BUDGET_MS);
    let mut report = samples
        .report("launch_to_inbox", 95.0, BUDGET_MS)
        .with("runs", runs)
        .with("failed_runs", failures.len())
        .with("rows", rows)
        .with("main_to_paint_p50_ms", main_report.p50_ms)
        .with("main_to_paint_p95_ms", main_report.p95_ms);
    report.pass &= failures.is_empty();
    report.print();
}
