//! Notification latency: from the moment the forge
//! reports a CI state change to the toast going to the system.
//!
//! On the GitHub sandbox (`REVIEW_SANDBOX_GITHUB`, see `docs/sandboxes.md`), pushes a scratch branch that adds
//! `ci/fail`, opens a pull request and watches it with the app's watch
//! wiring ([`app::watching::Watching`]) over the forge hub's watch engine,
//! the way the running app does. Two transitions are timed: the checks
//! failing (against the `test` job's `completed_at`) and, after a commit
//! that removes `ci/fail`, the checks passing again (against the new run's
//! last `completed_at`). Two CI runs in all. The pull request is closed
//! and the branch deleted at the end, also on failure.
//!
//! `cargo run -p app --release --example notify_bench`
//!
//! Prints a `PERF` line for `change_to_toast` (p95 against 15 s) and exits
//! non-zero when a step fails. The toasts are real: they show on screen.

#[path = "../../forge/examples/sandbox/mod.rs"]
#[allow(dead_code, reason = "the forge examples use the rest")]
mod sandbox;

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use app::notify::{self, Recording, Toaster};
use app::settings::NotificationSettings;
use app::watching::{Effect, Engine, NoticeKind, PrInfo, Toast, Watching};
use chrono::{DateTime, Utc};
use forge::model::{CiRun, CiState, ForgeKind, PrNumber, Sha, Timestamp};
use forge::watch::Change;
use forge::{ForgeConfig, ForgeHub};
use perf::Samples;
use sandbox::SandboxCli as _;
use store::{PrKey, SubscriptionTarget};

const BRANCH_PREFIX: &str = "notify-bench/";
const FAIL_FILE: &str = "ci/fail";
const BUDGET_MS: f64 = 15_000.0;
/// Longest the whole bench waits for CI.
const DEADLINE: Duration = Duration::from_secs(15 * 60);

fn head(dir: &Path) -> Result<Sha, String> {
    Ok(Sha::new(sandbox::git(dir, &["rev-parse", "HEAD"])?.trim()))
}

fn push_failing(sandbox: &sandbox::Sandbox, dir: &Path, branch: &str) -> Result<Sha, String> {
    sandbox::fresh_branch(sandbox, dir, branch)?;
    let path = dir.join(FAIL_FILE);
    std::fs::create_dir_all(path.parent().ok_or("no parent")?).map_err(|e| e.to_string())?;
    std::fs::write(&path, "Makes the sandbox's test job fail.\n").map_err(|e| e.to_string())?;
    sandbox::git(dir, &["add", FAIL_FILE])?;
    sandbox::git(
        dir,
        &["commit", "--quiet", "-m", "Notify bench: add ci/fail"],
    )?;
    sandbox::push(dir, branch)?;
    head(dir)
}

fn push_fix(dir: &Path, branch: &str) -> Result<Sha, String> {
    sandbox::git(dir, &["rm", "--quiet", FAIL_FILE])?;
    sandbox::git(
        dir,
        &["commit", "--quiet", "-m", "Notify bench: remove ci/fail"],
    )?;
    sandbox::push(dir, branch)?;
    head(dir)
}

/// What a toast is timed against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Moment {
    /// The first failed job of the head's runs completed.
    FirstFailure,
    /// The last job of the head's runs completed.
    AllDone,
}

struct Bench {
    hub: ForgeHub,
    key: PrKey,
    watching: Watching,
    engine: forge::WatchHandle,
    toaster: Arc<Recording>,
    samples: Samples,
    timed: Vec<String>,
}

impl Bench {
    fn runs(&self, sha: &Sha) -> Result<Vec<CiRun>, String> {
        let (repo, sha) = (self.key.repo.clone(), sha.clone());
        let pending = self
            .hub
            .spawn(&self.key.repo.host, move |forge| async move {
                forge.ci_runs(&repo, &sha).await
            });
        futures::executor::block_on(pending).map_err(|e| e.to_string())
    }

    fn info(&self) -> PrInfo {
        PrInfo {
            key: self.key.clone(),
            label: format!("{}#{}", self.key.repo.path(), self.key.number),
            title: "notify bench".to_owned(),
            authored: true,
            mine: false,
        }
    }

    fn resync(&mut self) {
        let wanted = self.watching.plan([self.info()], None);
        self.watching
            .sync(wanted, &mut self.engine as &mut dyn Engine);
    }

    fn handle(&mut self, change: Change) {
        for effect in self.watching.handle(change, Instant::now()) {
            if let Effect::Toast(toast) = effect {
                self.toaster.show(&toast);
            }
        }
    }

    fn flush(&mut self) {
        for toast in self.watching.flush(Instant::now()) {
            self.toaster.show(&toast);
        }
    }

    /// Times a toast against the forge's own completion time.
    fn time(&mut self, at: SystemTime, toast: &Toast, moment: Moment, runs: &[CiRun]) {
        let jobs = runs.iter().flat_map(|run| &run.jobs);
        let reference: Option<Timestamp> = match moment {
            Moment::FirstFailure => jobs
                .filter(|job| job.state == CiState::Fail)
                .filter_map(|job| job.completed_at)
                .min(),
            Moment::AllDone => jobs.filter_map(|job| job.completed_at).max(),
        };
        let Some(reference) = reference else {
            println!("{:?}: no completion time to time against", toast.kind);
            return;
        };
        let shown: DateTime<Utc> = at.into();
        let ms = (shown - reference).num_milliseconds();
        println!(
            "toast {:?} \"{}\": {ms} ms after the forge's completed_at {reference}",
            toast.kind, toast.title
        );
        self.samples
            .push(Duration::from_millis(u64::try_from(ms.max(0)).unwrap_or(0)));
        self.timed.push(format!("{:?}", toast.kind));
    }
}

fn main() -> ExitCode {
    let sandbox = match sandbox::Sandbox::from_env(ForgeKind::GitHub) {
        Ok(sandbox) => sandbox,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let sandbox = &sandbox;
    let repo = sandbox.repo();
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let branch = format!("{BRANCH_PREFIX}{stamp}");
    let dir = sandbox::scratch_dir("notify-bench", sandbox.kind);
    let head = match push_failing(sandbox, &dir, &branch) {
        Ok(head) => head,
        Err(error) => {
            eprintln!("FAIL setup: {error}");
            let _ = sandbox::delete_branch(&dir, &branch, BRANCH_PREFIX);
            return ExitCode::FAILURE;
        }
    };
    let body = "Scratch pull request of the notification bench; closed when it ends.";
    let number =
        match sandbox.open_pull_request(&dir, &branch, &format!("Notify bench {branch}"), body) {
            Ok(number) => number,
            Err(error) => {
                eprintln!("FAIL setup: {error}");
                let _ = sandbox::delete_branch(&dir, &branch, BRANCH_PREFIX);
                return ExitCode::FAILURE;
            }
        };
    println!("watching {repo}#{number} on {branch} at {}", head.short());
    let outcome = run(&repo, number, head, &dir, &branch);
    let mut cleanup = true;
    if let Err(error) = sandbox.close_pull_request(&dir, number) {
        eprintln!("cleanup: {error}");
        cleanup = false;
    }
    if let Err(error) = sandbox::delete_branch(&dir, &branch, BRANCH_PREFIX) {
        eprintln!("cleanup: {error}");
        cleanup = false;
    }
    match outcome {
        Ok(()) if cleanup => ExitCode::SUCCESS,
        Ok(()) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("FAIL {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(
    repo: &forge::model::RepoRef,
    number: PrNumber,
    first_head: Sha,
    dir: &Path,
    branch: &str,
) -> Result<(), String> {
    let hub = ForgeHub::start(vec![ForgeConfig {
        kind: ForgeKind::GitHub,
        host: repo.host.clone(),
    }])
    .map_err(|e| e.to_string())?;
    let (engine, changes) = hub.watch();
    // The engine's receiver is runtime-agnostic; a thread hands its changes
    // over with a timeout the main loop can wait on.
    let (tx, rx) = mpsc::channel::<Change>();
    std::thread::spawn(move || {
        let mut changes = changes;
        while let Some(change) = changes.blocking_recv() {
            if tx.send(change).is_err() {
                return;
            }
        }
    });
    let key = PrKey::new(repo.clone(), number);
    let mut bench = Bench {
        hub,
        key: key.clone(),
        watching: Watching::new(NotificationSettings::default()),
        engine,
        toaster: Arc::new(Recording::new(notify::system())),
        samples: Samples::with_capacity(4),
        timed: Vec::new(),
    };
    let started = Instant::now();
    let mut head = first_head.clone();
    let mut watching = false;
    let mut pushed_fix = false;
    let mut recovered = false;
    while !recovered {
        if started.elapsed() > DEADLINE {
            return Err(format!("timed out; timed so far: {:?}", bench.timed));
        }
        // The user turns watching on once the first run is going, so the
        // watch starts on its fast interval: a pull request with no CI yet
        // is polled every five minutes.
        if !watching {
            let runs = bench.runs(&first_head)?;
            if runs.iter().any(|run| run.state.is_active()) {
                println!("watching; run {} is {:?}", runs[0].id, runs[0].state);
                bench
                    .watching
                    .toggle(&SubscriptionTarget::pull_request(&key), false, None);
                bench.resync();
                watching = true;
            } else {
                std::thread::sleep(Duration::from_secs(2));
                continue;
            }
        }
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(change) => {
                println!("change: {:?}", change.event);
                bench.handle(change);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return Err("the watcher stopped".into()),
        }
        bench.flush();
        for (at, toast) in bench.toaster.take() {
            match toast.kind {
                NoticeKind::CiFailed if !pushed_fix => {
                    let runs = bench.runs(&first_head)?;
                    bench.time(at, &toast, Moment::FirstFailure, &runs);
                    head = push_fix(dir, branch)?;
                    pushed_fix = true;
                    println!("pushed the fix {}", head.short());
                }
                NoticeKind::CiRecovered | NoticeKind::CiFinished if pushed_fix => {
                    let runs = bench.runs(&head)?;
                    bench.time(at, &toast, Moment::AllDone, &runs);
                    recovered = true;
                }
                kind => println!("toast {kind:?} \"{}\" not timed", toast.title),
            }
        }
    }
    bench
        .samples
        .report("change_to_toast", 95.0, BUDGET_MS)
        .with("transitions", bench.timed.len())
        .with("forge", "github")
        .print();
    if bench.timed.len() < 2 {
        return Err(format!("only {:?} were timed", bench.timed));
    }
    Ok(())
}
