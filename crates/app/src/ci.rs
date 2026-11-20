//! The open pull request's CI as the Conversation's pipelines card and the
//! Log panel show it, without gpui: runs, jobs and steps, what a re-run would
//! touch, and how a job's log is followed while it runs.

use std::time::Duration;

use chrono::Utc;
use forge::Capabilities;
use forge::model::{
    CheckStep, CiJob, CiRun, CiState, ForgeKind, JobId, LogChunk, RunId, Timestamp,
};
use gpui::SharedString;
use log_view::{LineKind, LogBuffer};

use crate::model::{Check, duration_label};

/// Where reading the runs stands.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum CiLoad {
    /// Nothing asked yet (the head is not known).
    #[default]
    Idle,
    Loading,
    Ready,
    /// The forge has no run list (Forgejo before v16): the checks rollup
    /// is all there is.
    Unsupported,
    Failed(SharedString),
}

/// A re-run the user asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Retry {
    Job { run: RunId, job: JobId },
    FailedJobs { run: RunId },
}

impl Retry {
    #[must_use]
    pub fn run(&self) -> &RunId {
        match self {
            Self::Job { run, .. } | Self::FailedJobs { run } => run,
        }
    }
}

/// A re-run of more than one job waits for this.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetryConfirm {
    pub retry: Retry,
    pub run_name: SharedString,
    pub jobs: Vec<SharedString>,
}

/// The jobs a re-run would start again, by name.
#[must_use]
pub fn retry_jobs(run: &CiRun, retry: &Retry) -> Vec<SharedString> {
    match retry {
        Retry::Job { job, .. } => run
            .jobs
            .iter()
            .filter(|j| &j.id == job)
            .map(|j| SharedString::from(j.name.clone()))
            .collect(),
        Retry::FailedJobs { .. } => failed_jobs(run)
            .map(|j| SharedString::from(j.name.clone()))
            .collect(),
    }
}

pub fn failed_jobs(run: &CiRun) -> impl Iterator<Item = &CiJob> {
    run.jobs.iter().filter(|job| job.state == CiState::Fail)
}

/// What the forge lets the app do with CI; the rest becomes a web link.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CiActions {
    pub retry: bool,
    pub logs: bool,
    pub steps: bool,
}

impl CiActions {
    #[must_use]
    pub fn of(caps: &Capabilities) -> Self {
        Self {
            retry: caps.retry_job,
            logs: caps.job_logs,
            steps: caps.job_steps,
        }
    }
}

/// `3m 10s`, or the time so far while it runs; `None` before it starts.
#[must_use]
pub fn duration(started: Option<Timestamp>, completed: Option<Timestamp>) -> Option<String> {
    let start = started?;
    let end = completed.unwrap_or_else(Utc::now);
    Some(duration_label((end - start).num_seconds()))
}

/// Jobs grouped by consecutive stage (GitLab); one unnamed group elsewhere.
#[must_use]
pub fn stages(run: &CiRun) -> Vec<(Option<&str>, Vec<&CiJob>)> {
    let mut groups: Vec<(Option<&str>, Vec<&CiJob>)> = Vec::new();
    for job in &run.jobs {
        let stage = job.stage.as_deref();
        match groups.last_mut() {
            Some((last, jobs)) if *last == stage => jobs.push(job),
            _ => groups.push((stage, vec![job])),
        }
    }
    groups
}

/// The rollup rows the sidebar counts, from the latest runs: one per job.
#[must_use]
pub fn checks_of(runs: &[CiRun]) -> Vec<Check> {
    runs.iter()
        .flat_map(|run| &run.jobs)
        .map(|job| Check {
            name: match &job.stage {
                Some(stage) => format!("{stage} / {}", job.name).into(),
                None => job.name.clone().into(),
            },
            state: job.state,
            detail: duration(job.started_at, job.completed_at)
                .unwrap_or_else(|| crate::ui::ci_label(job.state).to_owned())
                .into(),
        })
        .collect()
}

/// Runs still going: followed by the watch engine while the pull request
/// is open.
#[must_use]
pub fn active_runs(runs: &[CiRun]) -> Vec<RunId> {
    runs.iter()
        .filter(|run| run.state.is_active() || run.jobs.iter().any(|job| job.state.is_active()))
        .map(|run| run.id.clone())
        .collect()
}

// ---- log tailing

/// How a forge's log can be followed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tail {
    /// The log grows while the job runs (GitLab in bursts, Forgejo every
    /// second or so): read from the last offset.
    Stream,
    /// The log exists only once the job has ended (GitHub): wait for the
    /// job, then read it.
    AfterFinish,
}

impl Tail {
    #[must_use]
    pub fn of(kind: ForgeKind) -> Self {
        match kind {
            ForgeKind::GitHub => Self::AfterFinish,
            ForgeKind::GitLab | ForgeKind::Forgejo => Self::Stream,
        }
    }
}

/// Between reads of a running job's log.
pub const TAIL_POLL: Duration = Duration::from_secs(2);
/// Right after new bytes: more tends to follow.
pub const TAIL_SOON: Duration = Duration::from_millis(500);
/// Between looks at a job whose log only exists at the end.
pub const FINISH_POLL: Duration = Duration::from_secs(5);
/// After a failed read, doubling up to [`RETRY_MAX`].
pub const RETRY_MIN: Duration = Duration::from_secs(2);
pub const RETRY_MAX: Duration = Duration::from_secs(30);

#[must_use]
pub fn tail_delay(got_bytes: bool) -> Duration {
    if got_bytes { TAIL_SOON } else { TAIL_POLL }
}

/// What a chunk read from `asked` does to the log on screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Piece {
    Append(Vec<u8>),
    /// The forge started the log over (a re-run under the same job id):
    /// the old lines go.
    Restart(Vec<u8>),
}

#[must_use]
pub fn piece(asked: u64, chunk: LogChunk) -> Piece {
    if asked > 0 && chunk.offset == 0 {
        Piece::Restart(chunk.bytes)
    } else {
        Piece::Append(chunk.bytes)
    }
}

/// The log line a step starts at: Forgejo's runner prints `⭐ Run <step>`;
/// elsewhere the first line stamped at or after the step's start,
/// preferring a group header in the second the step started (step times
/// are whole seconds).
#[must_use]
pub fn step_line(buffer: &LogBuffer, step: &CheckStep) -> Option<u64> {
    let all = buffer.dropped()..buffer.total();
    let marker = format!(" {}", step.name);
    if let Some(line) = buffer.lines(all.clone()).find(|line| {
        line.text.contains("Run ") && line.text.ends_with(&marker) && line.text.contains('⭐')
    }) {
        return Some(line.id);
    }
    let start = step.started_at?.timestamp_millis();
    let first = buffer
        .lines(all)
        .find(|line| line.time_ms.is_some_and(|t| t >= start))?;
    let window = start + 1000;
    let header = buffer
        .lines(first.id..first.id + 64)
        .take_while(|line| line.time_ms.is_none_or(|t| t < window))
        .find(|line| line.kind == LineKind::GroupStart);
    Some(header.map_or(first.id, |line| line.id))
}

/// The row a line sits on with `collapsed` group headers folded, counting
/// a leading "earlier lines dropped" row when there is one.
#[must_use]
pub fn row_of_line(buffer: &LogBuffer, line: u64, collapsed: impl Fn(u64) -> bool) -> u64 {
    let total = buffer.total();
    let hidden: u64 = buffer
        .groups()
        .iter()
        .filter(|group| group.start < line && collapsed(group.start))
        .map(|group| {
            let body = group.body(total);
            body.end.min(line) - body.start.min(line)
        })
        .sum();
    let lead = u64::from(buffer.dropped() > 0);
    (line - buffer.dropped()).saturating_sub(hidden) + lead
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use forge::model::Sha;
    use log_view::LogParser;

    use super::*;

    fn job(name: &str, state: CiState, stage: Option<&str>) -> CiJob {
        CiJob {
            id: JobId::new(name),
            name: name.to_owned(),
            stage: stage.map(str::to_owned),
            state,
            attempt: Some(1),
            started_at: None,
            completed_at: None,
            url: None,
            steps: Vec::new(),
        }
    }

    fn run(jobs: Vec<CiJob>) -> CiRun {
        CiRun {
            id: RunId::new("7"),
            name: "ci".to_owned(),
            event: None,
            state: CiState::Fail,
            number: Some(3),
            head_sha: Sha::new("a".repeat(40)),
            branch: None,
            started_at: None,
            completed_at: None,
            url: None,
            jobs,
        }
    }

    #[test]
    fn a_rerun_of_failed_jobs_lists_each() {
        let run = run(vec![
            job("build", CiState::Pass, None),
            job("test", CiState::Fail, None),
            job("lint", CiState::Fail, None),
        ]);
        let all = retry_jobs(
            &run,
            &Retry::FailedJobs {
                run: run.id.clone(),
            },
        );
        assert_eq!(all, ["test", "lint"]);
        let one = retry_jobs(
            &run,
            &Retry::Job {
                run: run.id.clone(),
                job: JobId::new("lint"),
            },
        );
        assert_eq!(one, ["lint"]);
    }

    #[test]
    fn stages_group_consecutive_jobs() {
        let run = run(vec![
            job("a", CiState::Pass, Some("build")),
            job("b", CiState::Pass, Some("build")),
            job("c", CiState::Fail, Some("test")),
        ]);
        let groups: Vec<(Option<&str>, usize)> = stages(&run)
            .into_iter()
            .map(|(stage, jobs)| (stage, jobs.len()))
            .collect();
        assert_eq!(groups, [(Some("build"), 2), (Some("test"), 1)]);
    }

    #[test]
    fn a_shorter_log_than_asked_starts_over() {
        let chunk = |offset| LogChunk {
            offset,
            bytes: b"x".to_vec(),
            next_offset: offset + 1,
            complete: false,
        };
        assert_eq!(piece(10, chunk(10)), Piece::Append(b"x".to_vec()));
        assert_eq!(piece(10, chunk(0)), Piece::Restart(b"x".to_vec()));
        assert_eq!(piece(0, chunk(0)), Piece::Append(b"x".to_vec()));
        assert!(tail_delay(true) < tail_delay(false));
        assert_eq!(Tail::of(ForgeKind::GitHub), Tail::AfterFinish);
    }

    fn buffer(text: &str) -> LogBuffer {
        let mut buffer = LogBuffer::new();
        let mut parser = LogParser::new();
        buffer.append(&mut parser, text.as_bytes());
        parser.finish();
        buffer.push(&mut parser.take());
        buffer
    }

    fn step(name: &str, second: u32) -> CheckStep {
        CheckStep {
            number: 1,
            name: name.to_owned(),
            state: CiState::Pass,
            started_at: Some(Utc.with_ymd_and_hms(2026, 11, 17, 14, 0, second).unwrap()),
            completed_at: None,
        }
    }

    #[test]
    fn steps_find_their_lines_by_time_or_runner_marker() {
        let log = "2026-11-17T14:00:00.1000000Z Set up job\n\
                   2026-11-17T14:00:01.0500000Z noise\n\
                   2026-11-17T14:00:02.0100000Z ##[group]Run make\n\
                   2026-11-17T14:00:02.0200000Z make: ok\n\
                   2026-11-17T14:00:02.0300000Z ##[endgroup]\n\
                   2026-11-17T14:00:03.0000000Z ⭐ Run Main Package\n";
        let buffer = buffer(log);
        assert_eq!(step_line(&buffer, &step("Compile", 2)), Some(2));
        assert_eq!(step_line(&buffer, &step("Configure", 1)), Some(1));
        let package = step_line(&buffer, &step("Package", 0)).unwrap();
        assert!(buffer.line(package).unwrap().text.ends_with("Package"));
    }

    #[test]
    fn collapsed_groups_above_a_line_hide_their_rows() {
        let buffer = buffer("a\n##[group]g\nb\nc\n##[endgroup]\nd\n");
        let header = buffer.groups()[0].start;
        let last = buffer.total() - 1;
        let open = row_of_line(&buffer, last, |_| false);
        let folded = row_of_line(&buffer, last, |h| h == header);
        assert_eq!(
            open - folded,
            buffer.groups()[0].body(buffer.total()).count() as u64
        );
    }
}
