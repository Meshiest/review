//! The forge side of the live CI check: on a scratch
//! pull request whose branch adds `ci/fail`, watch the sandbox workflow
//! run, tail the `slow` job, retry `test` unchanged, then push the removal
//! and watch the new run pass. Shared by `examples/ci_live` (live) and the
//! CI replay tests, so a replay sends the requests that were recorded.
//!
//! Every wait is a loop of reads with a pause between them; a replay passes
//! no pause, and since the replay serves each request's answers in recorded
//! order, its loops end where the live ones did.

use std::fmt;
use std::time::Duration;

use forge::model::{CiDetail, CiJob, CiRun, CiState, JobId, RepoRef, RunId, Sha};
use forge::{Capabilities, Forge};

/// The sandbox workflow's jobs ([`docs/sandboxes.md`](../../../../docs/sandboxes.md)).
pub const JOBS: [&str; 3] = ["build", "test", "slow"];
/// The build job's own steps, between the forge's setup and teardown steps.
pub const BUILD_STEPS: [&str; 3] = ["Configure", "Compile", "Package"];
/// Lines the `slow` job prints, `line 0001` to `line 2000`.
pub const SLOW_LINES: u32 = 2000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Pass(String),
    Fail(String),
    /// The capability the forge lacks.
    Skip(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub step: &'static str,
    pub status: Status,
}

impl Outcome {
    #[must_use]
    pub fn failed(&self) -> bool {
        matches!(self.status, Status::Fail(_))
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.status {
            Status::Pass(detail) => write!(f, "PASS {}: {detail}", self.step),
            Status::Fail(reason) => write!(f, "FAIL {}: {reason}", self.step),
            Status::Skip(capability) => write!(f, "SKIP {}: {capability}", self.step),
        }
    }
}

/// How long the loops wait: between reads of a run, between log reads, and
/// in all for one wait.
#[derive(Clone, Copy, Debug)]
pub struct Pace {
    pub poll: Duration,
    pub tail: Duration,
    pub timeout: Duration,
}

impl Pace {
    pub const LIVE: Self = Self {
        poll: Duration::from_secs(5),
        tail: Duration::from_secs(2),
        timeout: Duration::from_secs(15 * 60),
    };

    fn polls(self, every: Duration) -> usize {
        let every = every.max(Duration::from_secs(1));
        usize::try_from(self.timeout.as_secs() / every.as_secs()).unwrap_or(usize::MAX)
    }
}

/// One log read of the tail, as the replay tests check it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TailRead {
    pub asked: u64,
    pub offset: u64,
    pub len: usize,
    pub next_offset: u64,
    pub complete: bool,
}

/// What a run saw, for the replay tests.
#[derive(Clone, Debug, Default)]
pub struct Seen {
    pub run: Option<CiRun>,
    /// The run with steps, once finished.
    pub finished: Option<CiRun>,
    pub tail: Vec<TailRead>,
    pub log: Vec<u8>,
    pub retried: Option<CiJob>,
    pub fixed: Option<CiRun>,
}

type Step = Result<String, String>;

fn ensure(ok: bool, message: impl FnOnce() -> String) -> Result<(), String> {
    if ok { Ok(()) } else { Err(message()) }
}

fn err(error: forge::Error) -> String {
    error.to_string()
}

async fn pause(every: Duration) {
    if !every.is_zero() {
        tokio::time::sleep(every).await;
    }
}

fn is_pull_request_event(run: &CiRun) -> bool {
    matches!(
        run.event.as_deref(),
        Some("pull_request" | "merge_request_event" | "pull_request_target")
    )
}

/// The run to follow among a commit's runs: the pull request one when there
/// is one (a push and a pull request event both start the workflow), else
/// the newest. It must list the three jobs.
fn choose(runs: &[CiRun]) -> Option<&CiRun> {
    let complete = |run: &&CiRun| JOBS.iter().all(|name| run.job(name).is_some());
    runs.iter()
        .filter(complete)
        .find(|run| is_pull_request_event(run))
        .or_else(|| runs.iter().find(complete))
}

fn job_states(run: &CiRun) -> String {
    run.jobs
        .iter()
        .map(|job| format!("{} {:?}", job.name, job.state))
        .collect::<Vec<_>>()
        .join(", ")
}

struct Live<'a> {
    forge: &'a dyn Forge,
    repo: &'a RepoRef,
    caps: Capabilities,
    pace: Pace,
}

impl Live<'_> {
    /// Reads the commit's runs until one has the three jobs.
    async fn first_run(&self, sha: &Sha) -> Result<CiRun, String> {
        for _ in 0..self.pace.polls(self.pace.poll) {
            let runs = self.forge.ci_runs(self.repo, sha).await.map_err(err)?;
            if let Some(run) = choose(&runs) {
                return Ok(run.clone());
            }
            pause(self.pace.poll).await;
        }
        Err(format!(
            "no run with jobs {JOBS:?} for {} in time",
            sha.short()
        ))
    }

    /// Reads the run (jobs only) until `done` says so.
    async fn wait_run(
        &self,
        run: &RunId,
        what: &str,
        done: impl Fn(&CiRun) -> bool,
    ) -> Result<CiRun, String> {
        for _ in 0..self.pace.polls(self.pace.poll) {
            let read = self
                .forge
                .ci_run(self.repo, run, CiDetail::Jobs)
                .await
                .map_err(err)?;
            if done(&read) {
                return Ok(read);
            }
            pause(self.pace.poll).await;
        }
        Err(format!("run {run}: {what} did not happen in time"))
    }

    async fn finished(&self, run: &RunId) -> Result<CiRun, String> {
        self.wait_run(run, "finishing", |read| {
            !read.state.is_active() && read.jobs.iter().all(CiJob::is_finished)
        })
        .await?;
        self.forge
            .ci_run(self.repo, run, CiDetail::Steps)
            .await
            .map_err(err)
    }

    /// Follows the `slow` job's log from its start until it is complete.
    async fn tail(&self, run: &RunId, seen: &mut Seen) -> Step {
        let started = self
            .wait_run(run, "slow starting", |read| {
                read.job("slow")
                    .is_some_and(|job| job.state != CiState::Queued)
            })
            .await?;
        let job: JobId = started
            .job("slow")
            .map(|job| job.id.clone())
            .ok_or("no slow job")?;
        let mut offset = 0;
        let mut live_reads = 0;
        for _ in 0..self.pace.polls(self.pace.tail) {
            let chunk = self
                .forge
                .job_log(self.repo, &job, offset)
                .await
                .map_err(err)?;
            seen.tail.push(TailRead {
                asked: offset,
                offset: chunk.offset,
                len: chunk.bytes.len(),
                next_offset: chunk.next_offset,
                complete: chunk.complete,
            });
            if chunk.offset != offset {
                ensure(chunk.offset == 0, || {
                    format!("asked for {offset}, got a chunk at {}", chunk.offset)
                })?;
                seen.log.clear();
            }
            if !chunk.complete && !chunk.bytes.is_empty() {
                live_reads += 1;
            }
            seen.log.extend_from_slice(&chunk.bytes);
            offset = chunk.next_offset;
            if chunk.complete {
                let lines = check_slow_log(&seen.log)?;
                let while_running = if live_reads == 0 {
                    "the forge served the log only once the job finished".to_owned()
                } else {
                    format!("{live_reads} with new bytes while running")
                };
                return Ok(format!(
                    "{lines} lines in order with red, green and bold, {} bytes in {} reads ({while_running})",
                    seen.log.len(),
                    seen.tail.len()
                ));
            }
            pause(self.pace.tail).await;
        }
        Err("the log did not complete in time".to_owned())
    }

    async fn retry(&self, run: &CiRun, seen: &mut Seen) -> Step {
        let before = run.job("test").ok_or("no test job")?;
        let mark = (before.id.clone(), before.attempt);
        self.forge
            .retry_job(self.repo, &before.id)
            .await
            .map_err(err)?;
        let after = self
            .wait_run(&run.id, "the test retry finishing", |read| {
                read.job("test")
                    .is_some_and(|job| (job.id.clone(), job.attempt) != mark && job.is_finished())
            })
            .await?;
        let test = after
            .job("test")
            .ok_or("no test job after the retry")?
            .clone();
        ensure(test.state == CiState::Fail, || {
            format!("the retried test is {:?}, not Fail", test.state)
        })?;
        let detail = format!(
            "test job {} attempt {:?} -> job {} attempt {:?}: Fail again",
            mark.0, mark.1, test.id, test.attempt
        );
        seen.retried = Some(test);
        Ok(detail)
    }

    async fn fix(&self, sha: &Sha, seen: &mut Seen) -> Step {
        let run = self.first_run(sha).await?;
        let done = self.finished(&run.id).await?;
        for name in JOBS {
            let state = done.job(name).map(|job| job.state);
            ensure(state == Some(CiState::Pass), || {
                format!("run {} after the fix: {}", done.id, job_states(&done))
            })?;
        }
        let detail = format!("run {} on {}: {}", done.id, sha.short(), job_states(&done));
        seen.fixed = Some(done);
        Ok(detail)
    }
}

/// The numbered lines of the `slow` log, checked: `line 0001` to
/// `line 2000` once each, in order, and 200 each of the red, green and bold
/// lines with their escape codes.
pub fn check_slow_log(log: &[u8]) -> Result<u32, String> {
    let text = String::from_utf8_lossy(log);
    let mut expected = 1;
    for row in text.lines() {
        let Some(at) = row.find("line ") else {
            continue;
        };
        let digits = row.get(at + 5..at + 9).unwrap_or_default();
        let numbered = digits.len() == 4
            && digits.bytes().all(|b| b.is_ascii_digit())
            && row[at + 9..].starts_with(' ');
        if !numbered {
            continue;
        }
        let n: u32 = digits
            .parse()
            .map_err(|_| format!("bad number in {row:?}"))?;
        ensure(n == expected, || {
            format!("expected line {expected:04}, found line {n:04}")
        })?;
        expected += 1;
    }
    ensure(expected == SLOW_LINES + 1, || {
        format!("{} of {SLOW_LINES} lines", expected - 1)
    })?;
    for (code, color) in [
        ("\x1b[31m", "red"),
        ("\x1b[32m", "green"),
        ("\x1b[1m", "bold"),
    ] {
        let count = text.matches(&format!("{code}line ")).count();
        ensure(count == 200, || {
            format!("{count} {color} lines, expected 200")
        })?;
    }
    Ok(SLOW_LINES)
}

/// Runs the steps on the pull request whose head is `head` (with
/// `ci/fail`). `push_fix` removes `ci/fail` with a new commit and returns
/// its id. `report` is called after each step.
pub async fn run(
    forge: &dyn Forge,
    repo: &RepoRef,
    head: &Sha,
    pace: Pace,
    push_fix: impl FnOnce() -> Result<Sha, String>,
    mut report: impl FnMut(&Outcome),
) -> (Vec<Outcome>, Seen) {
    let live = Live {
        forge,
        repo,
        caps: forge.capabilities(),
        pace,
    };
    let mut seen = Seen::default();
    let mut outcomes = Vec::new();
    let mut push = |step: &'static str, status: Status| {
        let outcome = Outcome { step, status };
        report(&outcome);
        outcomes.push(outcome);
    };
    let status = |result: Step| match result {
        Ok(detail) => Status::Pass(detail),
        Err(reason) => Status::Fail(reason),
    };

    let run = match live.first_run(head).await {
        Ok(run) => run,
        Err(reason) => {
            push("run", Status::Fail(reason));
            return (outcomes, seen);
        }
    };
    push(
        "run",
        Status::Pass(format!(
            "run {} ({}), jobs {}",
            run.id,
            run.event.as_deref().unwrap_or("?"),
            JOBS.join(", ")
        )),
    );
    seen.run = Some(run.clone());

    if live.caps.job_logs {
        let result = live.tail(&run.id, &mut seen).await;
        push("tail", status(result));
    } else {
        push("tail", Status::Skip("job logs".to_owned()));
    }

    let finished = match live.finished(&run.id).await {
        Ok(finished) => finished,
        Err(reason) => {
            push("states", Status::Fail(reason));
            return (outcomes, seen);
        }
    };
    let expected = [
        ("build", CiState::Pass),
        ("test", CiState::Fail),
        ("slow", CiState::Pass),
    ];
    let states_ok = expected
        .iter()
        .all(|(name, state)| finished.job(name).map(|job| job.state) == Some(*state));
    push(
        "states",
        if states_ok {
            Status::Pass(job_states(&finished))
        } else {
            Status::Fail(format!(
                "expected build Pass, test Fail, slow Pass; got {}",
                job_states(&finished)
            ))
        },
    );

    if live.caps.job_steps {
        push("steps", status(check_steps(&finished)));
    } else {
        push("steps", Status::Skip("job steps".to_owned()));
    }
    seen.finished = Some(finished.clone());

    if live.caps.retry_job {
        let result = live.retry(&finished, &mut seen).await;
        push("retry", status(result));
    } else {
        push("retry", Status::Skip("re-running jobs".to_owned()));
    }

    let result = match push_fix() {
        Ok(sha) => live.fix(&sha, &mut seen).await,
        Err(reason) => Err(format!("pushing the fix: {reason}")),
    };
    push("fix", status(result));
    (outcomes, seen)
}

/// `build` has the three steps, passed, and `test` has a failed one.
fn check_steps(run: &CiRun) -> Step {
    let build = run.job("build").ok_or("no build job")?;
    for name in BUILD_STEPS {
        let step = build.steps.iter().find(|step| step.name.ends_with(name));
        ensure(step.is_some_and(|step| step.state == CiState::Pass), || {
            format!("build step {name}: {:?}", step.map(|s| s.state))
        })?;
    }
    let test = run.job("test").ok_or("no test job")?;
    ensure(
        test.steps.iter().any(|step| step.state == CiState::Fail),
        || "no failed step in test".to_owned(),
    )?;
    Ok(format!(
        "build: {}",
        build
            .steps
            .iter()
            .map(|step| step.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    ))
}
