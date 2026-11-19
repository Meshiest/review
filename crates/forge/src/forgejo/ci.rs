//! Actions runs, jobs, steps and logs [F6][F7], gated on the version: v16
//! has job lists and logs, v17 steps (one request per job) and re-runs.
//!
//! The job id is `ActionRunJob.id`, never the task id: `jobs/{task}/logs`
//! is a 404 (or another job's log).

use super::{Api, ApiJob, ApiRun, ApiRunList, Forgejo, absolute, bounded, real, status_state};
use crate::ci_log;
use crate::error::{Error, Result};
use crate::forge::Forge;
use crate::model::{
    CheckStep, CiDetail, CiJob, CiRun, ForgeKind, JobId, LogChunk, PrNumber, PullRequestPulse,
    RepoRef, RunId, Sha,
};

impl Forgejo {
    fn require(&self, have: bool, feature: &'static str) -> Result<()> {
        have.then_some(()).ok_or(Error::Unsupported {
            kind: ForgeKind::Forgejo,
            feature,
        })
    }

    async fn jobs_of(&self, repo: &RepoRef, run: u64, detail: CiDetail) -> Result<Vec<CiJob>> {
        let caps = self.capabilities();
        if !caps.job_logs {
            return Ok(Vec::new());
        }
        let run = run.to_string();
        let mut jobs: Vec<ApiJob> = self
            .api
            .get(
                &[
                    "repos",
                    &repo.owner,
                    &repo.name,
                    "actions",
                    "runs",
                    &run,
                    "jobs",
                ],
                &[],
            )
            .await?;
        jobs.sort_by_key(|job| job.id);
        if detail == CiDetail::Steps && caps.job_steps {
            let api = self.api.clone();
            let repo = repo.clone();
            let ids: Vec<u64> = jobs.iter().map(|job| job.id).collect();
            jobs = bounded(ids, move |id| {
                let (api, repo) = (api.clone(), repo.clone());
                async move { job_detail(&api, &repo, &id.to_string()).await }
            })
            .await?;
        }
        Ok(jobs
            .into_iter()
            .map(|job| job.into_job(&self.api.host))
            .collect())
    }

    pub(super) async fn ci_runs(&self, repo: &RepoRef, sha: &Sha) -> Result<Vec<CiRun>> {
        let runs = match self
            .api
            .paged::<ApiRun, ApiRunList>(
                &["repos", &repo.owner, &repo.name, "actions", "runs"],
                &[("head_sha", sha.as_str())],
                usize::MAX,
                |list| list.workflow_runs,
            )
            .await
        {
            Ok(runs) => runs,
            Err(Error::NotFound { .. }) => Vec::new(),
            Err(err) => return Err(err),
        };
        let mut out = Vec::with_capacity(runs.len());
        for run in runs {
            let jobs = self.jobs_of(repo, run.id, CiDetail::Steps).await?;
            out.push(run.into_run(&self.api.host, jobs));
        }
        out.sort_by_key(|run| std::cmp::Reverse(run.id.as_str().parse::<u64>().unwrap_or(0)));
        Ok(out)
    }

    pub(super) async fn ci_run(
        &self,
        repo: &RepoRef,
        run: &RunId,
        detail: CiDetail,
    ) -> Result<CiRun> {
        let api_run: ApiRun = self
            .api
            .get(
                &[
                    "repos",
                    &repo.owner,
                    &repo.name,
                    "actions",
                    "runs",
                    run.as_str(),
                ],
                &[],
            )
            .await?;
        let jobs = self.jobs_of(repo, api_run.id, detail).await?;
        Ok(api_run.into_run(&self.api.host, jobs))
    }

    pub(super) async fn ci_job(&self, repo: &RepoRef, job: &JobId) -> Result<CiJob> {
        self.require(self.capabilities().job_logs, "CI jobs")?;
        Ok(job_detail(&self.api, repo, job.as_str())
            .await?
            .into_job(&self.api.host))
    }

    pub(super) async fn job_log(&self, repo: &RepoRef, job: &JobId, from: u64) -> Result<LogChunk> {
        self.require(self.capabilities().job_logs, "job logs")?;
        let finished = self.ci_job(repo, job).await?.is_finished();
        let segments = [
            "repos",
            repo.owner.as_str(),
            repo.name.as_str(),
            "actions",
            "jobs",
            job.as_str(),
            "logs",
        ];
        let url = self.api.url(&segments, &[])?;
        ci_log::fetch(&self.api.http, from, finished, |from| {
            Ok(ci_log::request(url.clone(), from))
        })
        .await
    }

    pub(super) async fn pull_request_pulse(
        &self,
        repo: &RepoRef,
        number: PrNumber,
    ) -> Result<PullRequestPulse> {
        let pull =
            self.api
                .pull(repo, number)
                .await?
                .into_pull_request(&self.api.host, repo, &[], None);
        let ci = self
            .api
            .combined_state(repo, pull.head.sha.as_str())
            .await?;
        Ok(PullRequestPulse {
            state: pull.state,
            head: pull.head.sha,
            updated_at: pull.updated_at,
            ci,
        })
    }
}

/// `GET .../actions/jobs/{id}`: the job with its steps (v17).
async fn job_detail(api: &Api, repo: &RepoRef, job: &str) -> Result<ApiJob> {
    api.get(
        &["repos", &repo.owner, &repo.name, "actions", "jobs", job],
        &[],
    )
    .await
}

impl ApiRun {
    fn into_run(self, host: &str, jobs: Vec<CiJob>) -> CiRun {
        let state = status_state(&self.status);
        CiRun {
            id: RunId::new(self.id.to_string()),
            name: self.workflow_id,
            event: Some(self.event).filter(|event| !event.is_empty()),
            state,
            number: Some(self.index_in_repo).filter(|n| *n > 0),
            head_sha: Sha::new(self.commit_sha),
            branch: Some(self.prettyref).filter(|branch| !branch.is_empty()),
            started_at: real(self.started),
            completed_at: (!state.is_active()).then(|| real(self.stopped)).flatten(),
            url: absolute(host, &self.html_url),
            jobs,
        }
    }
}

impl ApiJob {
    fn into_job(self, host: &str) -> CiJob {
        let steps: Vec<CheckStep> = self
            .steps
            .into_iter()
            .map(|step| CheckStep {
                number: step.number,
                name: step.name,
                state: status_state(&step.status),
                started_at: real(step.started),
                completed_at: real(step.stopped),
            })
            .collect();
        let state = status_state(&self.status);
        CiJob {
            id: JobId::new(self.id.to_string()),
            name: self.name,
            stage: None,
            state,
            attempt: Some(self.attempt).filter(|attempt| *attempt > 0),
            // The job itself has no times; its steps do.
            started_at: steps.iter().filter_map(|step| step.started_at).min(),
            completed_at: (!state.is_active())
                .then(|| steps.iter().filter_map(|step| step.completed_at).max())
                .flatten(),
            url: absolute(host, &self.html_url),
            steps,
        }
    }
}
