//! Actions over REST [G6][G7]: runs, jobs with steps, logs and the pull
//! request pulse. REST rather than GraphQL because it sends ETags, so a
//! poll that finds nothing new is a free `304`.

use std::collections::HashMap;
use std::fmt::Write as _;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::{
    Conclusion, Connection, GitHub, HeadCommit, RunStatus, StatusState, rollup_state, run_state,
    status_state,
};
use crate::ci_log;
use crate::error::{Error, Result};
use crate::forge::ci_states_by_pulse;
use crate::http::{HttpRequest, RedirectPolicy, Url};
use crate::model::{
    CheckStep, CiDetail, CiJob, CiRun, CiState, JobId, LogChunk, PrNumber, PrState,
    PullRequestPulse, RepoRef, RunId, Sha, Timestamp,
};

/// GitHub's largest REST page.
const PER_PAGE: &str = "100";
/// A list that never ends is a server bug; stop instead of looping.
const MAX_PAGES: usize = 50;

/// Hosts a job log redirect may go to: the Actions results blob store
/// (`productionresultssa*.blob.core.windows.net`) and the older
/// `pipelines.actions.githubusercontent.com`. HTTPS only; the token is not
/// sent there (the transport drops it across origins), the URL is presigned.
fn log_redirects() -> RedirectPolicy {
    RedirectPolicy::allow(|to| {
        to.scheme() == "https"
            && to.host_str().is_some_and(|host| {
                host.ends_with(".blob.core.windows.net")
                    || host.ends_with(".actions.githubusercontent.com")
            })
    })
}

/// REST spells the GraphQL enums in lowercase (`in_progress`).
fn wire<T: DeserializeOwned>(text: Option<&str>) -> Option<T> {
    serde_json::from_value(Value::String(text?.to_ascii_uppercase())).ok()
}

fn rest_state(status: Option<&str>, conclusion: Option<&str>) -> CiState {
    run_state(wire::<RunStatus>(status), wire::<Conclusion>(conclusion))
}

impl GitHub {
    async fn rest_pages<T, P: DeserializeOwned>(
        &self,
        first: Url,
        rows: impl Fn(P) -> Vec<T>,
    ) -> Result<Vec<T>> {
        let mut all = Vec::new();
        let mut next = Some(first);
        for _ in 0..MAX_PAGES {
            let Some(url) = next.take() else { break };
            let request = HttpRequest::get(url);
            let describe = request.describe();
            let response = self.http.send(request).await?;
            all.extend(rows(response.json(&describe)?));
            next = response.next_page();
        }
        Ok(all)
    }

    fn rest_list_url(
        &self,
        repo: &RepoRef,
        segments: &[&str],
        query: &[(&str, &str)],
    ) -> Result<Url> {
        let mut url = self.rest_url(repo, segments)?;
        url.query_pairs_mut()
            .extend_pairs(query)
            .append_pair("per_page", PER_PAGE);
        Ok(url)
    }

    async fn run_jobs(&self, repo: &RepoRef, run: &str) -> Result<Vec<CiJob>> {
        let url = self.rest_list_url(repo, &["actions", "runs", run, "jobs"], &[])?;
        let jobs = self.rest_pages(url, |page: RestJobs| page.jobs).await?;
        Ok(jobs.into_iter().map(RestJob::into_job).collect())
    }

    pub(super) async fn ci_runs(&self, repo: &RepoRef, sha: &Sha) -> Result<Vec<CiRun>> {
        let url = self.rest_list_url(repo, &["actions", "runs"], &[("head_sha", sha.as_str())])?;
        let runs = self
            .rest_pages(url, |page: RestRuns| page.workflow_runs)
            .await?;
        let mut out = Vec::with_capacity(runs.len());
        for run in runs {
            let jobs = self.run_jobs(repo, &run.id.to_string()).await?;
            out.push(run.into_run(jobs));
        }
        out.sort_by_key(|run| std::cmp::Reverse(run.id.as_str().parse::<u64>().unwrap_or(0)));
        Ok(out)
    }

    pub(super) async fn ci_run(&self, repo: &RepoRef, run: &RunId, _: CiDetail) -> Result<CiRun> {
        let url = self.rest_url(repo, &["actions", "runs", run.as_str()])?;
        let detail: RestRun = self.http.get_json(url).await?;
        let jobs = self.run_jobs(repo, run.as_str()).await?;
        Ok(detail.into_run(jobs))
    }

    pub(super) async fn ci_job(&self, repo: &RepoRef, job: &JobId) -> Result<CiJob> {
        let url = self.rest_url(repo, &["actions", "jobs", job.as_str()])?;
        let job: RestJob = self.http.get_json(url).await?;
        Ok(job.into_job())
    }

    pub(super) async fn job_log(&self, repo: &RepoRef, job: &JobId, from: u64) -> Result<LogChunk> {
        let finished = self.ci_job(repo, job).await?.is_finished();
        let url = self.rest_url(repo, &["actions", "jobs", job.as_str(), "logs"])?;
        ci_log::fetch(&self.http, from, finished, |from| {
            Ok(ci_log::request(url.clone(), from).redirects(log_redirects()))
        })
        .await
    }

    /// Batched with aliases; a missing pull request fails the whole query
    /// with `NOT_FOUND`, so that falls back to one pulse each.
    pub(super) async fn ci_states(
        &self,
        repo: &RepoRef,
        numbers: &[PrNumber],
    ) -> Result<Vec<CiState>> {
        let mut states = Vec::with_capacity(numbers.len());
        for chunk in numbers.chunks(CI_STATES_PAGE) {
            match self.ci_states_page(repo, chunk).await {
                Ok(page) => states.extend(page),
                Err(Error::NotFound { .. }) => {
                    states.extend(ci_states_by_pulse(self, repo, chunk).await?);
                }
                Err(error) => return Err(error),
            }
        }
        Ok(states)
    }

    async fn ci_states_page(&self, repo: &RepoRef, numbers: &[PrNumber]) -> Result<Vec<CiState>> {
        let mut fields = String::new();
        for (ix, number) in numbers.iter().enumerate() {
            let _ = write!(
                fields,
                "pr{ix}: pullRequest(number: {}) {{                  commits(last:1){{nodes{{commit{{statusCheckRollup{{state}}}}}}}} }} ",
                number.0
            );
        }
        let query = format!(
            "query CiStates($owner: String!, $name: String!) {{              repository(owner: $owner, name: $name) {{ {fields}}} }}"
        );
        let variables = json!({"owner": repo.owner, "name": repo.name});
        let data: CiStatesData = self.query("CiStates", &query, variables).await?;
        let mut found = data.repository.ok_or_else(|| Error::NotFound {
            request: format!("CiStates {}", repo.path()),
        })?;
        Ok((0..numbers.len())
            .map(|ix| {
                found
                    .remove(&format!("pr{ix}"))
                    .flatten()
                    .and_then(|pr| pr.commits.into_parts().0.pop())
                    .and_then(|commit| commit.commit.status_check_rollup)
                    .and_then(|rollup| rollup.state)
                    .map_or(CiState::None, rollup_state)
            })
            .collect())
    }

    pub(super) async fn pull_request_pulse(
        &self,
        repo: &RepoRef,
        number: PrNumber,
    ) -> Result<PullRequestPulse> {
        let url = self.rest_url(repo, &["pulls", &number.to_string()])?;
        let pull: RestPull = self.http.get_json(url).await?;
        let sha = pull.head.sha.as_str();
        // One page of each: a pulse is a change detector, not the list.
        let checks: RestCheckRuns = self
            .http
            .get_json(self.rest_list_url(repo, &["commits", sha, "check-runs"], &[])?)
            .await?;
        let statuses: RestCombinedStatus = self
            .http
            .get_json(self.rest_list_url(repo, &["commits", sha, "status"], &[])?)
            .await?;
        let ci = CiState::rollup(
            checks
                .check_runs
                .iter()
                .map(|run| rest_state(run.status.as_deref(), run.conclusion.as_deref()))
                .chain(statuses.statuses.iter().map(|status| {
                    wire::<StatusState>(Some(&status.state)).map_or(CiState::None, status_state)
                })),
        );
        let state = match (pull.state.as_str(), pull.merged_at.is_some()) {
            (_, true) => PrState::Merged,
            ("closed", false) => PrState::Closed,
            _ if pull.draft => PrState::Draft,
            _ => PrState::Open,
        };
        Ok(PullRequestPulse {
            state,
            head: Sha::new(pull.head.sha),
            updated_at: pull.updated_at,
            ci,
        })
    }
}

/// Pull requests per [`GitHub::ci_states`] query; GitHub caps a query's
/// cost, not its aliases, and each alias is a small one.
const CI_STATES_PAGE: usize = 50;

#[derive(Deserialize)]
struct CiStatesData {
    repository: Option<HashMap<String, Option<CiStateNode>>>,
}

#[derive(Deserialize)]
struct CiStateNode {
    commits: Connection<HeadCommit>,
}

#[derive(Deserialize)]
struct RestRuns {
    #[serde(default)]
    workflow_runs: Vec<RestRun>,
}

#[derive(Deserialize)]
struct RestRun {
    id: u64,
    name: Option<String>,
    event: Option<String>,
    status: Option<String>,
    conclusion: Option<String>,
    run_number: Option<u64>,
    head_sha: String,
    head_branch: Option<String>,
    run_started_at: Option<Timestamp>,
    updated_at: Option<Timestamp>,
    html_url: Option<String>,
}

impl RestRun {
    fn into_run(self, jobs: Vec<CiJob>) -> CiRun {
        let state = rest_state(self.status.as_deref(), self.conclusion.as_deref());
        CiRun {
            id: RunId::new(self.id.to_string()),
            name: self.name.unwrap_or_default(),
            event: self.event,
            state,
            number: self.run_number,
            head_sha: Sha::new(self.head_sha),
            branch: self.head_branch,
            started_at: self.run_started_at,
            completed_at: (!state.is_active()).then_some(self.updated_at).flatten(),
            url: self.html_url,
            jobs,
        }
    }
}

#[derive(Deserialize)]
struct RestJobs {
    #[serde(default)]
    jobs: Vec<RestJob>,
}

#[derive(Deserialize)]
struct RestJob {
    id: u64,
    name: String,
    status: Option<String>,
    conclusion: Option<String>,
    run_attempt: Option<u32>,
    started_at: Option<Timestamp>,
    completed_at: Option<Timestamp>,
    html_url: Option<String>,
    #[serde(default)]
    steps: Option<Vec<RestStep>>,
}

impl RestJob {
    fn into_job(self) -> CiJob {
        CiJob {
            id: JobId::new(self.id.to_string()),
            name: self.name,
            stage: None,
            state: rest_state(self.status.as_deref(), self.conclusion.as_deref()),
            attempt: self.run_attempt,
            started_at: self.started_at,
            completed_at: self.completed_at,
            url: self.html_url,
            steps: self
                .steps
                .unwrap_or_default()
                .into_iter()
                .map(|step| CheckStep {
                    number: step.number,
                    state: rest_state(step.status.as_deref(), step.conclusion.as_deref()),
                    name: step.name,
                    started_at: step.started_at,
                    completed_at: step.completed_at,
                })
                .collect(),
        }
    }
}

#[derive(Deserialize)]
struct RestStep {
    number: u32,
    name: String,
    status: Option<String>,
    conclusion: Option<String>,
    started_at: Option<Timestamp>,
    completed_at: Option<Timestamp>,
}

#[derive(Deserialize)]
struct RestPull {
    state: String,
    #[serde(default)]
    draft: bool,
    merged_at: Option<Timestamp>,
    updated_at: Timestamp,
    head: RestHead,
}

#[derive(Deserialize)]
struct RestHead {
    sha: String,
}

#[derive(Deserialize)]
struct RestCheckRuns {
    #[serde(default)]
    check_runs: Vec<RestCheckRun>,
}

#[derive(Deserialize)]
struct RestCheckRun {
    status: Option<String>,
    conclusion: Option<String>,
}

#[derive(Deserialize)]
struct RestCombinedStatus {
    #[serde(default)]
    statuses: Vec<RestStatus>,
}

#[derive(Deserialize)]
struct RestStatus {
    state: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rest_states_follow_the_model_table() {
        assert_eq!(rest_state(Some("in_progress"), None), CiState::Running);
        assert_eq!(rest_state(Some("queued"), None), CiState::Queued);
        assert_eq!(
            rest_state(Some("completed"), Some("failure")),
            CiState::Fail
        );
        assert_eq!(
            rest_state(Some("completed"), Some("success")),
            CiState::Pass
        );
        assert_eq!(
            rest_state(Some("completed"), Some("cancelled")),
            CiState::Canceled
        );
        assert_eq!(rest_state(None, None), CiState::Queued);
    }

    #[test]
    fn logs_follow_only_blob_storage_redirects() {
        let policy = log_redirects();
        let from = Url::parse("https://api.github.com/repos/o/r/actions/jobs/1/logs").unwrap();
        let blob = "https://productionresultssa19.blob.core.windows.net/actions-results/x?sig=1";
        assert!(policy.next(&from, blob, 0).unwrap().is_some());
        for refused in [
            "https://evil.example.org/x",
            "https://blob.core.windows.net.evil.example/x",
            "http://productionresultssa19.blob.core.windows.net/x",
        ] {
            assert!(policy.next(&from, refused, 0).is_err(), "{refused}");
        }
    }
}
