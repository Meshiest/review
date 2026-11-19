//! Pipelines and jobs over REST [L5][L6][L7]: GraphQL has no ETags and no
//! raw trace. GitLab has no steps, so a job is one log.

use serde::Deserialize;

use super::{GitLab, PAGE, RestJob, ci_state, pr_state, project_segment};
use crate::ci_log;
use crate::error::Result;
use crate::model::{
    CiDetail, CiJob, CiRun, CiState, JobId, LogChunk, PrNumber, PullRequestPulse, RepoRef, RunId,
    Sha, Timestamp,
};

impl GitLab {
    fn project_path(repo: &RepoRef, rest: &str) -> String {
        format!("projects/{}/{rest}", project_segment(repo))
    }

    async fn pipeline_job_list(&self, repo: &RepoRef, pipeline: &str) -> Result<Vec<CiJob>> {
        let path = Self::project_path(repo, &format!("pipelines/{pipeline}/jobs"));
        let url = self.rest_url(&path, &[("per_page", &PAGE.to_string())])?;
        let jobs: Vec<RestJob> = self.rest_pages(url, usize::MAX).await?;
        Ok(jobs.into_iter().map(RestJob::into_job).collect())
    }

    pub(super) async fn ci_runs(&self, repo: &RepoRef, sha: &Sha) -> Result<Vec<CiRun>> {
        let path = Self::project_path(repo, "pipelines");
        let url = self.rest_url(
            &path,
            &[("sha", sha.as_str()), ("per_page", &PAGE.to_string())],
        )?;
        let pipelines: Vec<RestPipeline> = self.rest_pages(url, usize::MAX).await?;
        let mut runs = Vec::with_capacity(pipelines.len());
        for pipeline in pipelines {
            let jobs = self
                .pipeline_job_list(repo, &pipeline.id.to_string())
                .await?;
            runs.push(pipeline.into_run(jobs));
        }
        Ok(runs)
    }

    pub(super) async fn ci_run(&self, repo: &RepoRef, run: &RunId, _: CiDetail) -> Result<CiRun> {
        let path = Self::project_path(repo, &format!("pipelines/{run}"));
        let pipeline: RestPipeline = self.http.get_json(self.rest_url(&path, &[])?).await?;
        let jobs = self.pipeline_job_list(repo, run.as_str()).await?;
        Ok(pipeline.into_run(jobs))
    }

    pub(super) async fn ci_job(&self, repo: &RepoRef, job: &JobId) -> Result<CiJob> {
        let path = Self::project_path(repo, &format!("jobs/{job}"));
        let job: RestJob = self.http.get_json(self.rest_url(&path, &[])?).await?;
        Ok(job.into_job())
    }

    pub(super) async fn job_log(&self, repo: &RepoRef, job: &JobId, from: u64) -> Result<LogChunk> {
        let finished = self.ci_job(repo, job).await?.is_finished();
        let url = self.rest_url(&Self::project_path(repo, &format!("jobs/{job}/trace")), &[])?;
        ci_log::fetch(&self.http, from, finished, |from| {
            Ok(ci_log::request(url.clone(), from))
        })
        .await
    }

    pub(super) async fn pull_request_pulse(
        &self,
        repo: &RepoRef,
        number: PrNumber,
    ) -> Result<PullRequestPulse> {
        let path = Self::project_path(repo, &format!("merge_requests/{number}"));
        let mr: RestMergeRequest = self.http.get_json(self.rest_url(&path, &[])?).await?;
        Ok(PullRequestPulse {
            state: pr_state(&mr.state, mr.draft),
            head: Sha::new(mr.sha.unwrap_or_default()),
            updated_at: mr.updated_at,
            ci: mr
                .head_pipeline
                .map_or(CiState::None, |pipeline| ci_state(&pipeline.status)),
        })
    }
}

impl RestJob {
    fn into_job(self) -> CiJob {
        CiJob {
            id: JobId::new(self.id.to_string()),
            name: self.name,
            stage: self.stage,
            state: ci_state(&self.status),
            attempt: None,
            started_at: self.started_at,
            completed_at: self.finished_at,
            url: self.web_url,
            steps: Vec::new(),
        }
    }
}

#[derive(Deserialize)]
struct RestPipeline {
    id: u64,
    iid: Option<u64>,
    status: String,
    source: Option<String>,
    #[serde(rename = "ref")]
    git_ref: Option<String>,
    sha: String,
    web_url: Option<String>,
    name: Option<String>,
    /// Only on a single pipeline; a list row has neither time.
    started_at: Option<Timestamp>,
    finished_at: Option<Timestamp>,
}

impl RestPipeline {
    fn into_run(self, jobs: Vec<CiJob>) -> CiRun {
        let state = ci_state(&self.status);
        let started_at = self
            .started_at
            .or_else(|| jobs.iter().filter_map(|job| job.started_at).min());
        let completed_at = (!state.is_active())
            .then(|| {
                self.finished_at
                    .or_else(|| jobs.iter().filter_map(|job| job.completed_at).max())
            })
            .flatten();
        CiRun {
            id: RunId::new(self.id.to_string()),
            name: self
                .name
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| "pipeline".to_owned()),
            event: self.source,
            state,
            number: self.iid,
            head_sha: Sha::new(self.sha),
            branch: self.git_ref,
            started_at,
            completed_at,
            url: self.web_url,
            jobs,
        }
    }
}

#[derive(Deserialize)]
struct RestMergeRequest {
    state: String,
    #[serde(default)]
    draft: bool,
    sha: Option<String>,
    updated_at: Timestamp,
    head_pipeline: Option<RestPipelineStatus>,
}

#[derive(Deserialize)]
struct RestPipelineStatus {
    status: String,
}
