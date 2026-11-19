//! Forgejo (self-hosted; Gitea-compatible REST at `https://{host}/api/v1/`,
//! OpenAPI at `/swagger.v1.json`). No GraphQL. Capabilities depend on the
//! server version, probed once in [`Forgejo::connect`].
//!
//! Endpoints (footnotes in `docs/design.md`):
//! - `whoami`: `GET /user`.
//! - `inbox` [F9]: `GET /repos/issues/search?type=pulls&state=open` with
//!   `review_requested=true` or `created=true`; rows are issues, so fetch
//!   `GET /repos/{o}/{r}/pulls/{i}` for each (bounded concurrency). A repo:
//!   `GET /repos/{o}/{r}/pulls?state=open`. Lists send no ETag; they paginate
//!   with `page` and `limit` and report `X-Total-Count`.
//! - `pull_request`: `GET /repos/{o}/{r}/pulls/{i}` (`draft`, `merged`,
//!   `merge_base`, `requested_reviewers`), `GET .../reviews` for reviewer
//!   states, `GET .../commits/{sha}/status` for `ci` [F6].
//! - `threads` [F1][F2][F3]: `GET .../pulls/{i}/reviews`, then `GET
//!   .../reviews/{id}/comments` per review (no flat list). There is no thread
//!   object: comments of one path, side and line are one thread, and the
//!   oldest comment's id is the thread id. `resolver` non-null on any
//!   comment means resolved. `position` is the new-side line and
//!   `original_position` the old-side one (0 = absent); `extra_lines_count`
//!   (v17) extends the range after the commented line. A comment is outdated
//!   when it was written on another commit than the head and the line it
//!   sits on no longer reads the same in the head's file (`GET .../raw/{path}`
//!   against the last line of the comment's `diff_hunk`). Conversation:
//!   `GET /repos/{o}/{r}/issues/{i}/comments` and review summaries.
//! - `checks` [F6][F7]: `GET .../commits/{sha}/statuses` (context
//!   `workflow / job (event)`, `target_url` `/{o}/{r}/actions/runs/{run}/jobs/{n}`,
//!   where `{run}` is `index_in_repo`) and `GET .../actions/runs?head_sha=`.
//!   v16+: `GET .../actions/runs/{run_id}/jobs`.
//! - `stack`: no native API; base-branch rule over the open pull requests.
//! - `viewed_files`: none [F5]; the trait default (Unsupported) stands.
//! - Web routes (`/{o}/{r}/...` without `/api`) need a login session, not a
//!   token: never use them.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;

use async_trait::async_trait;
use chrono::{DateTime, Datelike, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};
use tokio::task::JoinSet;

use crate::capabilities::{Capabilities, ForgejoVersion};
use crate::error::{Error, Result};
use crate::forge::{Forge, INBOX_LIMIT};
use crate::http::{HttpClient, HttpRequest, Url};
use crate::model::{
    AccountKind, Anchor, BranchRef, CheckRun, CheckRunId, CiState, Comment, CommentId, DiffStat,
    ForgeKind, InboxSection, JobId, Label, LineRef, Milestone, Position, PrNumber, PrState,
    PullRequest, PullRequestId, RepoRef, Resolution, ReviewDecision, ReviewId, ReviewState,
    Reviewer, RunId, Sha, Side, Stack, Thread, ThreadId, Timestamp, User, UserId,
};
use crate::model::{
    CiDetail, CiJob, CiRun, LogChunk, MergeOptions, MergeSettings, Mergeability, NewComment,
    PendingComment, PendingReview, PullRequestPulse, Review, Verdict,
};
use crate::people::PickerCache;
use crate::stack::stack_by_base_branch;

mod ci;
mod lifecycle;
mod people;
mod writes;

/// Rows per page; the server's own cap is 50.
const PAGE_SIZE: usize = 50;
/// Requests in flight at once for per-item fan-out (pull requests, review
/// comments, files). Well inside the rate limiter's burst.
const CONCURRENCY: usize = 4;

pub struct Forgejo {
    api: Api,
    version: ForgejoVersion,
    /// The collaborator and label lists behind the pickers.
    picker: PickerCache,
}

impl Forgejo {
    /// Probes `GET /api/v1/version` for the capabilities.
    ///
    /// # Errors
    /// [`Error::Config`] for a bad host, [`Error::Decode`] for an
    /// unreadable version, or the request's error.
    pub async fn connect(http: HttpClient) -> Result<Self> {
        #[derive(Deserialize)]
        struct VersionBody {
            version: String,
        }
        let host = http.host().to_owned();
        let base = Url::parse(&format!("https://{host}/api/v1/")).map_err(|err| Error::Config {
            host: host.clone(),
            reason: err.to_string(),
        })?;
        let api = Api { http, base, host };
        let body: VersionBody = api.get(&["version"], &[]).await?;
        let version =
            body.version
                .parse()
                .map_err(
                    |err: crate::capabilities::ParseVersionError| Error::Decode {
                        request: "GET /api/v1/version".to_owned(),
                        reason: err.to_string(),
                    },
                )?;
        Ok(Self {
            api,
            version,
            picker: PickerCache::default(),
        })
    }

    #[must_use]
    pub fn version(&self) -> ForgejoVersion {
        self.version
    }
}

#[async_trait]
impl Forge for Forgejo {
    fn kind(&self) -> ForgeKind {
        ForgeKind::Forgejo
    }

    fn host(&self) -> &str {
        self.api.http.host()
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::forgejo(self.version)
    }

    async fn whoami(&self) -> Result<User> {
        let me: ApiUser = self.api.get(&["user"], &[]).await?;
        Ok(me.into_user())
    }

    async fn inbox(&self, section: &InboxSection) -> Result<Vec<PullRequest>> {
        let mut pulls = match section {
            InboxSection::ReviewRequested => {
                self.search_inbox(&[("state", "open"), ("review_requested", "true")])
                    .await?
            }
            InboxSection::AuthoredByMe => {
                self.search_inbox(&[("state", "open"), ("created", "true")])
                    .await?
            }
            InboxSection::Query(query) => {
                let pairs = parse_query(query)?;
                let pairs: Vec<(&str, &str)> = pairs
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.as_str()))
                    .collect();
                self.search_inbox(&pairs).await?
            }
            InboxSection::OpenInRepo(repo) => {
                let rows: Vec<ApiPull> = self
                    .api
                    .paged(
                        &["repos", &repo.owner, &repo.name, "pulls"],
                        &[("state", "open"), ("sort", "recentupdate")],
                        INBOX_LIMIT,
                        |rows| rows,
                    )
                    .await?;
                let api = self.api.clone();
                let repo = repo.clone();
                bounded(rows, move |row| {
                    let api = api.clone();
                    let repo = repo.clone();
                    async move {
                        let number = PrNumber(row.number);
                        let reviews = api.reviews(&repo, number).await?;
                        Ok(row.into_pull_request(&api.host, &repo, &reviews, None))
                    }
                })
                .await?
            }
        };
        pulls.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then(a.number.cmp(&b.number))
        });
        pulls.truncate(INBOX_LIMIT);
        Ok(pulls)
    }

    async fn pull_request(&self, repo: &RepoRef, number: PrNumber) -> Result<PullRequest> {
        self.api.pull_request(repo, number, true).await
    }

    async fn threads(&self, repo: &RepoRef, number: PrNumber) -> Result<Vec<Thread>> {
        let index = number.to_string();
        let comments_path = [
            "repos",
            &repo.owner,
            &repo.name,
            "issues",
            &index,
            "comments",
        ];
        let (pull, reviews, issue_comments) = tokio::try_join!(
            self.api.pull(repo, number),
            self.api.reviews(repo, number),
            self.api
                .paged::<ApiIssueComment, _>(&comments_path, &[], usize::MAX, |rows| rows),
        )?;
        let head = pull.head.sha.clone();

        let with_comments: Vec<ApiReview> = reviews
            .iter()
            .filter(|review| review.has_inline_comments())
            .cloned()
            .collect();
        let api = self.api.clone();
        let repo_for_comments = repo.clone();
        let per_review = bounded(with_comments, move |review| {
            let api = api.clone();
            let repo = repo_for_comments.clone();
            async move {
                let comments: Vec<ApiReviewComment> = api
                    .paged(
                        &[
                            "repos",
                            &repo.owner,
                            &repo.name,
                            "pulls",
                            &number.to_string(),
                            "reviews",
                            &review.id.to_string(),
                            "comments",
                        ],
                        &[],
                        usize::MAX,
                        |rows| rows,
                    )
                    .await?;
                Ok(comments)
            }
        })
        .await?;
        let inline: Vec<ApiReviewComment> = per_review.into_iter().flatten().collect();

        // Only comments written on an older commit need the head's file to
        // tell whether their line still reads the same.
        let stale_paths: Vec<String> = inline
            .iter()
            .filter(|comment| comment.is_behind(&head))
            .map(|comment| comment.path.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let api = self.api.clone();
        let repo_for_files = repo.clone();
        let head_for_files = head.clone();
        let files = bounded(stale_paths.clone(), move |path| {
            let api = api.clone();
            let repo = repo_for_files.clone();
            let head = head_for_files.clone();
            async move { api.file_at(&repo, &head, &path).await }
        })
        .await?;
        let head_files: HashMap<String, Option<Vec<String>>> = stale_paths
            .into_iter()
            .zip(files)
            .map(|(path, text)| {
                let lines = text.map(|text| text.lines().map(str::to_owned).collect());
                (path, lines)
            })
            .collect();

        let mut threads = inline_threads(&inline, &head, &head_files, &reviews);
        threads.extend(review_summary_threads(&reviews));
        threads.extend(issue_comments.into_iter().map(ApiIssueComment::into_thread));
        // Stable: threads written in the same second keep comment-id order.
        threads.sort_by_key(|thread| thread.comments.first().map(|comment| comment.created_at));
        Ok(threads)
    }

    async fn checks(&self, repo: &RepoRef, number: PrNumber) -> Result<Vec<CheckRun>> {
        let pull = self.api.pull(repo, number).await?;
        let sha = pull.head.sha.as_str();
        let statuses: Vec<ApiStatus> = self
            .api
            .paged(
                &["repos", &repo.owner, &repo.name, "commits", sha, "statuses"],
                &[],
                usize::MAX,
                |rows| rows,
            )
            .await?;
        let runs = match self
            .api
            .paged::<ApiRun, ApiRunList>(
                &["repos", &repo.owner, &repo.name, "actions", "runs"],
                &[("head_sha", sha)],
                usize::MAX,
                |list| list.workflow_runs,
            )
            .await
        {
            Ok(runs) => runs,
            // Actions switched off for the repository.
            Err(Error::NotFound { .. }) => Vec::new(),
            Err(err) => return Err(err),
        };
        let mut jobs: HashMap<u64, Vec<ApiJob>> = HashMap::new();
        if self.capabilities().job_logs && !runs.is_empty() {
            let api = self.api.clone();
            let repo = repo.clone();
            let ids: Vec<u64> = runs.iter().map(|run| run.id).collect();
            let listed = bounded(ids.clone(), move |run_id| {
                let api = api.clone();
                let repo = repo.clone();
                async move {
                    let mut list: Vec<ApiJob> = api
                        .get(
                            &[
                                "repos",
                                &repo.owner,
                                &repo.name,
                                "actions",
                                "runs",
                                &run_id.to_string(),
                                "jobs",
                            ],
                            &[],
                        )
                        .await?;
                    list.sort_by_key(|job| job.id);
                    Ok(list)
                }
            })
            .await?;
            jobs.extend(ids.into_iter().zip(listed));
        }
        Ok(build_checks(&self.api.host, &statuses, &runs, &jobs))
    }

    async fn stack(&self, repo: &RepoRef, number: PrNumber) -> Result<Stack> {
        let open: Vec<ApiPull> = self
            .api
            .paged(
                &["repos", &repo.owner, &repo.name, "pulls"],
                &[("state", "open")],
                usize::MAX,
                |rows| rows,
            )
            .await?;
        let host = &self.api.host;
        let candidates: Vec<PullRequest> = open
            .into_iter()
            .map(|row| row.into_pull_request(host, repo, &[], None))
            .collect();
        let target = match candidates.iter().find(|pr| pr.number == number) {
            Some(pr) => pr.clone(),
            None => self
                .api
                .pull(repo, number)
                .await?
                .into_pull_request(host, repo, &[], None),
        };
        Ok(stack_by_base_branch(&target, &candidates))
    }

    // ---- writes, in `writes.rs` ----

    async fn start_review(
        &self,
        repo: &RepoRef,
        number: PrNumber,
        commit: &Sha,
    ) -> Result<PendingReview> {
        writes::start_review(self, repo, number, commit).await
    }

    async fn pending_review(
        &self,
        repo: &RepoRef,
        number: PrNumber,
    ) -> Result<Option<PendingReview>> {
        writes::pending_review(self, repo, number).await
    }

    async fn add_review_comment(
        &self,
        repo: &RepoRef,
        number: PrNumber,
        review: &ReviewId,
        comment: &NewComment,
    ) -> Result<PendingComment> {
        writes::add_review_comment(self, repo, number, review, comment).await
    }

    async fn delete_review_comment(
        &self,
        repo: &RepoRef,
        number: PrNumber,
        comment: &CommentId,
    ) -> Result<()> {
        writes::delete_review_comment(self, repo, number, comment).await
    }

    async fn submit_review(
        &self,
        repo: &RepoRef,
        number: PrNumber,
        review: &ReviewId,
        verdict: Verdict,
        body: &str,
    ) -> Result<Review> {
        writes::submit_review(self, repo, number, review, verdict, body).await
    }

    async fn discard_review(
        &self,
        repo: &RepoRef,
        number: PrNumber,
        review: &ReviewId,
    ) -> Result<()> {
        writes::discard_review(self, repo, number, review).await
    }

    async fn reply(
        &self,
        repo: &RepoRef,
        number: PrNumber,
        thread: &ThreadId,
        body: &str,
    ) -> Result<Comment> {
        writes::reply(self, repo, number, thread, body).await
    }

    async fn add_comment(&self, repo: &RepoRef, number: PrNumber, body: &str) -> Result<Comment> {
        writes::add_comment(self, repo, number, body).await
    }

    async fn retry_job(&self, repo: &RepoRef, job: &JobId) -> Result<()> {
        writes::retry_job(self, repo, job).await
    }

    async fn retry_failed_jobs(&self, repo: &RepoRef, run: &RunId) -> Result<()> {
        writes::retry_failed_jobs(self, repo, run).await
    }

    async fn mergeability(&self, repo: &RepoRef, number: PrNumber) -> Result<Mergeability> {
        lifecycle::mergeability(self, repo, number).await
    }

    async fn merge_methods(&self, repo: &RepoRef) -> Result<MergeSettings> {
        lifecycle::merge_methods(self, repo).await
    }

    async fn merge(
        &self,
        repo: &RepoRef,
        number: PrNumber,
        options: &MergeOptions,
    ) -> Result<PullRequest> {
        lifecycle::merge(self, repo, number, options).await
    }

    async fn close(&self, repo: &RepoRef, number: PrNumber) -> Result<PullRequest> {
        lifecycle::set_state(self, repo, number, "closed").await
    }

    async fn reopen(&self, repo: &RepoRef, number: PrNumber) -> Result<PullRequest> {
        lifecycle::set_state(self, repo, number, "open").await
    }

    async fn set_draft(
        &self,
        repo: &RepoRef,
        number: PrNumber,
        draft: bool,
    ) -> Result<PullRequest> {
        lifecycle::set_draft(self, repo, number, draft).await
    }

    async fn collaborators(&self, repo: &RepoRef, query: &str) -> Result<Vec<User>> {
        people::collaborators(self, repo, query).await
    }

    async fn labels(&self, repo: &RepoRef) -> Result<Vec<Label>> {
        people::labels(self, repo).await
    }

    async fn request_reviewers(
        &self,
        repo: &RepoRef,
        number: PrNumber,
        users: &[User],
        teams: &[User],
    ) -> Result<PullRequest> {
        people::request_reviewers(self, repo, number, users, teams).await
    }

    async fn remove_reviewers(
        &self,
        repo: &RepoRef,
        number: PrNumber,
        users: &[User],
        teams: &[User],
    ) -> Result<PullRequest> {
        people::remove_reviewers(self, repo, number, users, teams).await
    }

    async fn set_assignees(
        &self,
        repo: &RepoRef,
        number: PrNumber,
        users: &[User],
    ) -> Result<PullRequest> {
        people::set_assignees(self, repo, number, users).await
    }

    async fn set_labels(
        &self,
        repo: &RepoRef,
        number: PrNumber,
        labels: &[String],
    ) -> Result<PullRequest> {
        people::set_labels(self, repo, number, labels).await
    }

    async fn ci_runs(&self, repo: &RepoRef, sha: &Sha) -> Result<Vec<CiRun>> {
        Forgejo::ci_runs(self, repo, sha).await
    }

    async fn ci_run(&self, repo: &RepoRef, run: &RunId, detail: CiDetail) -> Result<CiRun> {
        Forgejo::ci_run(self, repo, run, detail).await
    }

    async fn ci_job(&self, repo: &RepoRef, job: &JobId) -> Result<CiJob> {
        Forgejo::ci_job(self, repo, job).await
    }

    async fn job_log(&self, repo: &RepoRef, job: &JobId, from: u64) -> Result<LogChunk> {
        Forgejo::job_log(self, repo, job, from).await
    }

    async fn pull_request_pulse(
        &self,
        repo: &RepoRef,
        number: PrNumber,
    ) -> Result<PullRequestPulse> {
        Forgejo::pull_request_pulse(self, repo, number).await
    }

    // Kept defaults: `set_resolved` (web route only [F3]; the v17.0.1 spec
    // has no resolve endpoint), `set_viewed` (web route only [F5]) and
    // `apply_suggestions` (`ApplyLocally` [F4]).
}

impl Forgejo {
    /// An issue search for pull requests, each row completed with its pull
    /// request (search rows have no refs) and reviews.
    async fn search_inbox(&self, params: &[(&str, &str)]) -> Result<Vec<PullRequest>> {
        let mut query: Vec<(&str, &str)> = params
            .iter()
            .copied()
            .filter(|(key, _)| !matches!(*key, "type" | "page" | "limit"))
            .collect();
        query.push(("type", "pulls"));
        if !query.iter().any(|(key, _)| *key == "sort") {
            query.push(("sort", "recentupdate"));
        }
        let rows: Vec<ApiIssueRow> = self
            .api
            .paged(
                &["repos", "issues", "search"],
                &query,
                INBOX_LIMIT,
                |rows| rows,
            )
            .await?;
        let api = self.api.clone();
        let host = self.api.host.clone();
        bounded(rows, move |row| {
            let api = api.clone();
            let host = host.clone();
            async move {
                let repo = row.repo_ref(&host)?;
                api.pull_request(&repo, PrNumber(row.number), false).await
            }
        })
        .await
    }
}

fn parse_query(query: &str) -> Result<Vec<(String, String)>> {
    let url = Url::parse(&format!(
        "https://query.invalid/?{}",
        query.trim_start_matches('?')
    ))
    .map_err(|err| Error::Config {
        host: "query".to_owned(),
        reason: err.to_string(),
    })?;
    Ok(url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect())
}

/// Runs `f` over `items` with at most [`CONCURRENCY`] in flight; results
/// keep the order of `items`. The first error wins and aborts the rest.
async fn bounded<T, R, Fut>(items: Vec<T>, f: impl Fn(T) -> Fut) -> Result<Vec<R>>
where
    Fut: Future<Output = Result<R>> + Send + 'static,
    R: Send + 'static,
{
    let mut slots: Vec<Option<R>> = items.iter().map(|_| None).collect();
    let mut pending = items.into_iter().enumerate();
    let mut running = JoinSet::new();
    loop {
        while running.len() < CONCURRENCY
            && let Some((index, item)) = pending.next()
        {
            let work = f(item);
            running.spawn(async move { (index, work.await) });
        }
        let Some(joined) = running.join_next().await else {
            break;
        };
        let (index, result) = joined.map_err(|_| Error::Shutdown)?;
        slots[index] = Some(result?);
    }
    Ok(slots.into_iter().flatten().collect())
}

/// The REST plumbing every method shares. Cheap to clone, so fan-out tasks
/// can own one.
#[derive(Clone)]
struct Api {
    http: HttpClient,
    base: Url,
    host: String,
}

impl Api {
    fn url(&self, segments: &[&str], query: &[(&str, &str)]) -> Result<Url> {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .map_err(|()| Error::Config {
                host: self.host.clone(),
                reason: "API URL cannot take a path".to_owned(),
            })?
            .pop_if_empty()
            .extend(segments);
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        Ok(url)
    }

    async fn get<T: DeserializeOwned>(
        &self,
        segments: &[&str],
        query: &[(&str, &str)],
    ) -> Result<T> {
        self.http.get_json(self.url(segments, query)?).await
    }

    /// Every row of a list endpoint, at most `max`. Lists take `page` and
    /// `limit`, and report `X-Total-Count` (no ETag, no `Link` needed).
    async fn paged<T, P: DeserializeOwned>(
        &self,
        segments: &[&str],
        query: &[(&str, &str)],
        max: usize,
        rows: impl Fn(P) -> Vec<T>,
    ) -> Result<Vec<T>> {
        let limit = PAGE_SIZE.to_string();
        let mut all = Vec::new();
        for page in 1_u32.. {
            let page = page.to_string();
            let mut full: Vec<(&str, &str)> = query.to_vec();
            full.push(("limit", &limit));
            full.push(("page", &page));
            let request = HttpRequest::get(self.url(segments, &full)?);
            let describe = request.describe();
            let response = self.http.send(request).await?;
            let total: Option<usize> = response
                .header("x-total-count")
                .and_then(|value| value.trim().parse().ok());
            let batch = rows(response.json::<P>(&describe)?);
            let received = batch.len();
            all.extend(batch);
            let done = match total {
                Some(total) => all.len() >= total || received == 0,
                None => received < PAGE_SIZE,
            };
            if done || all.len() >= max {
                break;
            }
        }
        all.truncate(max);
        Ok(all)
    }

    async fn pull(&self, repo: &RepoRef, number: PrNumber) -> Result<ApiPull> {
        self.get(
            &[
                "repos",
                &repo.owner,
                &repo.name,
                "pulls",
                &number.to_string(),
            ],
            &[],
        )
        .await
    }

    async fn reviews(&self, repo: &RepoRef, number: PrNumber) -> Result<Vec<ApiReview>> {
        self.paged(
            &[
                "repos",
                &repo.owner,
                &repo.name,
                "pulls",
                &number.to_string(),
                "reviews",
            ],
            &[],
            usize::MAX,
            |rows| rows,
        )
        .await
    }

    /// The pull request with reviewers; `with_ci` adds the combined commit
    /// status of the head.
    async fn pull_request(
        &self,
        repo: &RepoRef,
        number: PrNumber,
        with_ci: bool,
    ) -> Result<PullRequest> {
        let (pull, reviews) =
            tokio::try_join!(self.pull(repo, number), self.reviews(repo, number))?;
        let ci = if with_ci {
            Some(self.combined_state(repo, &pull.head.sha).await?)
        } else {
            None
        };
        Ok(pull.into_pull_request(&self.host, repo, &reviews, ci))
    }

    async fn combined_state(&self, repo: &RepoRef, sha: &str) -> Result<CiState> {
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Combined {
            #[serde(deserialize_with = "null_default")]
            state: String,
            total_count: u64,
        }
        match self
            .get::<Combined>(
                &["repos", &repo.owner, &repo.name, "commits", sha, "status"],
                &[],
            )
            .await
        {
            Ok(combined) if combined.total_count > 0 => Ok(status_state(&combined.state)),
            Ok(_) | Err(Error::NotFound { .. }) => Ok(CiState::None),
            Err(err) => Err(err),
        }
    }

    /// A file's text at a commit; `None` when it no longer exists there.
    async fn file_at(&self, repo: &RepoRef, sha: &str, path: &str) -> Result<Option<String>> {
        let mut segments = vec!["repos", repo.owner.as_str(), repo.name.as_str(), "raw"];
        segments.extend(path.split('/'));
        let request = HttpRequest::get(self.url(&segments, &[("ref", sha)])?);
        match self.http.send(request).await {
            Ok(response) => Ok(Some(response.text())),
            Err(Error::NotFound { .. }) => Ok(None),
            Err(err) => Err(err),
        }
    }
}

fn null_default<'de, D, T>(deserializer: D) -> std::result::Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// The API sends `0001-01-01T00:00:00Z` for "never".
fn real(time: Option<Timestamp>) -> Option<Timestamp> {
    time.filter(|time| time.year() > 1)
}

fn epoch() -> Timestamp {
    DateTime::<Utc>::UNIX_EPOCH
}

#[derive(Deserialize, Default, Clone)]
#[serde(default)]
struct ApiUser {
    /// Negative for the placeholder account of a migrated author.
    id: i64,
    #[serde(deserialize_with = "null_default")]
    login: String,
    #[serde(deserialize_with = "null_default")]
    full_name: String,
    #[serde(deserialize_with = "null_default")]
    avatar_url: String,
}

impl ApiUser {
    fn into_user(self) -> User {
        User {
            id: (self.id > 0).then(|| UserId::new(self.id.to_string())),
            login: self.login,
            name: Some(self.full_name).filter(|name| !name.is_empty()),
            avatar_url: Some(self.avatar_url).filter(|url| !url.is_empty()),
            kind: AccountKind::User,
        }
    }
}

#[derive(Deserialize, Default, Clone)]
#[serde(default)]
struct ApiTeam {
    #[serde(deserialize_with = "null_default")]
    name: String,
    organization: Option<ApiOrg>,
}

#[derive(Deserialize, Default, Clone)]
#[serde(default)]
struct ApiOrg {
    #[serde(deserialize_with = "null_default")]
    username: String,
}

impl ApiTeam {
    fn into_user(self) -> User {
        let login = match self.organization {
            Some(org) if !org.username.is_empty() => format!("{}/{}", org.username, self.name),
            _ => self.name,
        };
        User {
            kind: AccountKind::Team,
            ..User::new(login)
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ApiRepo {
    #[serde(deserialize_with = "null_default")]
    name: String,
    owner: ApiUser,
    /// The viewer's rights on the repository; absent without a token.
    permissions: Option<ApiPermissions>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ApiPermissions {
    push: bool,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ApiBranch {
    #[serde(rename = "ref", deserialize_with = "null_default")]
    name: String,
    #[serde(deserialize_with = "null_default")]
    sha: String,
    repo: Option<ApiRepo>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ApiLabel {
    id: u64,
    #[serde(deserialize_with = "null_default")]
    name: String,
    #[serde(deserialize_with = "null_default")]
    color: String,
    #[serde(deserialize_with = "null_default")]
    description: String,
}

impl ApiLabel {
    fn into_label(self) -> Label {
        Label {
            name: self.name,
            color: Some(self.color.trim_start_matches('#').to_owned())
                .filter(|color| !color.is_empty()),
            description: Some(self.description).filter(|text| !text.is_empty()),
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ApiMilestone {
    #[serde(deserialize_with = "null_default")]
    title: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ApiPull {
    id: u64,
    number: u64,
    #[serde(deserialize_with = "null_default")]
    title: String,
    #[serde(deserialize_with = "null_default")]
    body: String,
    user: Option<ApiUser>,
    #[serde(deserialize_with = "null_default")]
    state: String,
    draft: bool,
    merged: bool,
    base: ApiBranch,
    head: ApiBranch,
    merge_base: Option<String>,
    created_at: Option<Timestamp>,
    updated_at: Option<Timestamp>,
    #[serde(deserialize_with = "null_default")]
    labels: Vec<ApiLabel>,
    #[serde(deserialize_with = "null_default")]
    assignees: Vec<ApiUser>,
    milestone: Option<ApiMilestone>,
    additions: u32,
    deletions: u32,
    changed_files: u32,
    comments: Option<u32>,
    review_comments: Option<u32>,
    #[serde(deserialize_with = "null_default")]
    html_url: String,
    #[serde(deserialize_with = "null_default")]
    requested_reviewers: Vec<ApiUser>,
    #[serde(deserialize_with = "null_default")]
    requested_reviewers_teams: Vec<ApiTeam>,
}

impl ApiPull {
    fn into_pull_request(
        self,
        host: &str,
        repo: &RepoRef,
        reviews: &[ApiReview],
        ci: Option<CiState>,
    ) -> PullRequest {
        let state = if self.merged {
            PrState::Merged
        } else if self.state == "closed" {
            PrState::Closed
        } else if self.draft {
            PrState::Draft
        } else {
            PrState::Open
        };
        let head_writable = self
            .head
            .repo
            .as_ref()
            .and_then(|repo| repo.permissions.as_ref())
            .is_some_and(|permissions| permissions.push);
        let branch = |branch: ApiBranch| BranchRef {
            repo: branch
                .repo
                .map(|r| RepoRef::new(host, r.owner.login, r.name)),
            name: branch.name,
            sha: Sha::new(branch.sha),
        };
        let reviewers = reviewers(
            &self.requested_reviewers,
            &self.requested_reviewers_teams,
            reviews,
        );
        let review_decision = ReviewDecision::from_reviewers(&reviewers);
        PullRequest {
            id: PullRequestId::new(self.id.to_string()),
            repo: repo.clone(),
            number: PrNumber(self.number),
            title: self.title,
            body: self.body,
            author: self.user.unwrap_or_default().into_user(),
            state,
            base: branch(self.base),
            head: branch(self.head),
            merge_base: self.merge_base.filter(|sha| !sha.is_empty()).map(Sha::new),
            created_at: real(self.created_at).unwrap_or_else(epoch),
            updated_at: real(self.updated_at).unwrap_or_else(epoch),
            review_decision,
            reviewers,
            ci,
            labels: self.labels.into_iter().map(ApiLabel::into_label).collect(),
            assignees: self.assignees.into_iter().map(ApiUser::into_user).collect(),
            milestone: self
                .milestone
                .filter(|milestone| !milestone.title.is_empty())
                .map(|milestone| Milestone {
                    title: milestone.title,
                }),
            diff_stat: Some(DiffStat {
                additions: self.additions,
                deletions: self.deletions,
                changed_files: self.changed_files,
            }),
            comment_count: self
                .comments
                .zip(self.review_comments)
                .map(|(comments, review)| comments.saturating_add(review)),
            unresolved_threads: None,
            web_url: self.html_url,
            head_writable,
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ApiIssueRow {
    number: u64,
    repository: ApiRowRepo,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ApiRowRepo {
    #[serde(deserialize_with = "null_default")]
    full_name: String,
}

impl ApiIssueRow {
    fn repo_ref(&self, host: &str) -> Result<RepoRef> {
        let (owner, name) =
            self.repository
                .full_name
                .split_once('/')
                .ok_or_else(|| Error::Decode {
                    request: "GET /api/v1/repos/issues/search".to_owned(),
                    reason: format!("row without a repository: {:?}", self.repository.full_name),
                })?;
        Ok(RepoRef::new(host, owner, name))
    }
}

#[derive(Deserialize, Default, Clone)]
#[serde(default)]
struct ApiReview {
    id: u64,
    user: Option<ApiUser>,
    team: Option<ApiTeam>,
    #[serde(deserialize_with = "null_default")]
    state: String,
    #[serde(deserialize_with = "null_default")]
    body: String,
    dismissed: bool,
    /// The head moved since the review (`stale` in the API).
    stale: bool,
    comments_count: u64,
    submitted_at: Option<Timestamp>,
    updated_at: Option<Timestamp>,
    #[serde(deserialize_with = "null_default")]
    commit_id: String,
    #[serde(deserialize_with = "null_default")]
    html_url: String,
}

impl ApiReview {
    fn review_state(&self) -> ReviewState {
        if self.dismissed {
            return ReviewState::Dismissed;
        }
        match self.state.as_str() {
            "APPROVED" => ReviewState::Approved,
            "REQUEST_CHANGES" => ReviewState::ChangesRequested,
            "PENDING" => ReviewState::Pending,
            "REQUEST_REVIEW" => ReviewState::Requested,
            _ => ReviewState::Commented,
        }
    }

    /// Submitted by someone: not a pending draft, not a review request.
    fn is_submitted(&self) -> bool {
        !matches!(self.state.as_str(), "PENDING" | "REQUEST_REVIEW")
    }

    fn has_inline_comments(&self) -> bool {
        self.is_submitted() && self.comments_count > 0
    }

    fn author(&self) -> User {
        match (&self.user, &self.team) {
            (Some(user), _) => user.clone().into_user(),
            (None, Some(team)) => team.clone().into_user(),
            (None, None) => User::new("ghost"),
        }
    }
}

/// One row per person: the latest review row (highest id), plus anyone still
/// only requested.
///
/// A review request is a row of its own, so one made after a review is the
/// latest row: the person is asked again and the earlier review no longer
/// counts. An approval is stale when the head moved after it.
fn reviewers(users: &[ApiUser], teams: &[ApiTeam], reviews: &[ApiReview]) -> Vec<Reviewer> {
    let mut latest: BTreeMap<String, &ApiReview> = BTreeMap::new();
    let mut reviewed: HashSet<&str> = HashSet::new();
    for review in reviews {
        let Some(user) = &review.user else { continue };
        if review.is_submitted() {
            reviewed.insert(user.login.as_str());
        }
        let slot = latest.entry(user.login.clone()).or_insert(review);
        if review.id >= slot.id {
            *slot = review;
        }
    }
    let mut rows: Vec<Reviewer> = latest
        .iter()
        .map(|(login, review)| {
            let state = review.review_state();
            Reviewer {
                user: review.author(),
                state,
                requested_again: state == ReviewState::Requested
                    && reviewed.contains(login.as_str()),
                stale: state == ReviewState::Approved && review.stale,
            }
        })
        .collect();
    for user in users {
        match rows.iter_mut().find(|row| row.user.login == user.login) {
            Some(row) if matches!(row.state, ReviewState::Requested | ReviewState::Pending) => {}
            Some(row) => {
                row.state = ReviewState::Requested;
                row.requested_again = true;
                row.stale = false;
            }
            None => rows.push(Reviewer::new(
                user.clone().into_user(),
                ReviewState::Requested,
            )),
        }
    }
    for team in teams {
        let user = team.clone().into_user();
        if !rows.iter().any(|row| row.user.login == user.login) {
            rows.push(Reviewer::new(user, ReviewState::Requested));
        }
    }
    rows
}

#[derive(Deserialize, Default, Clone)]
#[serde(default)]
struct ApiReviewComment {
    id: u64,
    #[serde(deserialize_with = "null_default")]
    body: String,
    user: Option<ApiUser>,
    resolver: Option<ApiUser>,
    pull_request_review_id: u64,
    created_at: Option<Timestamp>,
    updated_at: Option<Timestamp>,
    #[serde(deserialize_with = "null_default")]
    path: String,
    #[serde(deserialize_with = "null_default")]
    commit_id: String,
    #[serde(deserialize_with = "null_default")]
    diff_hunk: String,
    /// New-side line, 0 when the comment is on the old side.
    position: u32,
    /// Old-side line, 0 when the comment is on the new side.
    original_position: u32,
    extra_lines_count: u32,
    #[serde(deserialize_with = "null_default")]
    html_url: String,
}

impl ApiReviewComment {
    fn side(&self) -> Option<LineRef> {
        if self.position > 0 {
            Some(LineRef {
                side: Side::New,
                line: self.position,
            })
        } else if self.original_position > 0 {
            Some(LineRef {
                side: Side::Old,
                line: self.original_position,
            })
        } else {
            None
        }
    }

    /// Written on the new side against a commit other than the head.
    fn is_behind(&self, head: &str) -> bool {
        self.position > 0 && !self.commit_id.is_empty() && self.commit_id != head
    }

    /// The new-side line this comment is on still reads as it did.
    fn still_current(&self, head_file: Option<&Vec<String>>) -> bool {
        let Some(lines) = head_file else { return false };
        let Some(written) = self.diff_hunk.lines().next_back() else {
            return false;
        };
        let written = written.get(1..).unwrap_or_default();
        let index = self.position as usize - 1;
        lines
            .get(index)
            .is_some_and(|now| now.trim_end() == written.trim_end())
    }

    fn created(&self) -> Timestamp {
        real(self.created_at).unwrap_or_else(epoch)
    }

    fn into_comment(self) -> Comment {
        let created_at = self.created();
        let edited_at = real(self.updated_at).filter(|updated| *updated != created_at);
        Comment {
            id: CommentId::new(self.id.to_string()),
            author: self.user.unwrap_or_default().into_user(),
            body: self.body,
            created_at,
            edited_at,
            web_url: Some(self.html_url).filter(|url| !url.is_empty()),
            review: (self.pull_request_review_id != 0)
                .then(|| ReviewId::new(self.pull_request_review_id.to_string())),
            verdict: None,
            suggestions: Vec::new(),
        }
    }
}

/// Groups review comments by path, side and line; the oldest comment of a
/// group is the thread's first.
fn inline_threads(
    comments: &[ApiReviewComment],
    head: &str,
    head_files: &HashMap<String, Option<Vec<String>>>,
    reviews: &[ApiReview],
) -> Vec<Thread> {
    let visible: HashSet<u64> = reviews
        .iter()
        .filter(|review| review.is_submitted())
        .map(|review| review.id)
        .collect();
    let mut groups: Vec<Vec<&ApiReviewComment>> = Vec::new();
    // The server groups by path and first line only; a range is its own thread
    // here so its anchor is not lost behind a single-line comment.
    let mut index: HashMap<(&str, Option<LineRef>, u32), usize> = HashMap::new();
    let mut ordered: Vec<&ApiReviewComment> = comments
        .iter()
        .filter(|comment| visible.contains(&comment.pull_request_review_id))
        .collect();
    ordered.sort_by_key(|comment| (comment.created(), comment.id));
    for comment in ordered {
        let key = (
            comment.path.as_str(),
            comment.side(),
            comment.extra_lines_count,
        );
        let slot = *index.entry(key).or_insert_with(|| {
            groups.push(Vec::new());
            groups.len() - 1
        });
        groups[slot].push(comment);
    }
    groups
        .into_iter()
        .map(|group| {
            let root = group[0];
            let resolver = group.iter().find_map(|comment| comment.resolver.clone());
            let outdated = root.is_behind(head)
                && !root.still_current(head_files.get(&root.path).and_then(Option::as_ref));
            Thread {
                id: ThreadId::new(root.id.to_string()),
                anchor: Some(anchor(root, head, outdated)),
                resolution: match resolver {
                    Some(user) => Resolution::Resolved {
                        by: Some(user.into_user()),
                    },
                    None => Resolution::Unresolved,
                },
                outdated,
                comments: group
                    .into_iter()
                    .map(|comment| comment.clone().into_comment())
                    .collect(),
            }
        })
        .collect()
}

fn anchor(root: &ApiReviewComment, head: &str, outdated: bool) -> Anchor {
    let at_written = |commit: Option<&str>| {
        root.side().map(|line| {
            // `extra_lines_count` lines follow the commented line.
            let (start, line) = if root.extra_lines_count > 0 {
                (
                    Some(line),
                    LineRef {
                        side: line.side,
                        line: line.line + root.extra_lines_count,
                    },
                )
            } else {
                (None, line)
            };
            Position {
                line,
                start,
                commit: commit.filter(|sha| !sha.is_empty()).map(Sha::new),
            }
        })
    };
    let (position, original) = if outdated {
        (None, at_written(Some(&root.commit_id)))
    } else {
        // New-side comments that still read the same are relative to the
        // head; old-side ones stay on the commit they were written on.
        let commit = if root.position > 0 {
            head
        } else {
            &root.commit_id
        };
        (at_written(Some(commit)), None)
    };
    Anchor {
        path: root.path.clone(),
        old_path: None,
        position,
        original,
    }
}

/// A review with a body is a conversation entry carrying its verdict.
fn review_summary_threads(reviews: &[ApiReview]) -> Vec<Thread> {
    reviews
        .iter()
        .filter(|review| review.is_submitted() && !writes::is_blank(&review.body))
        .map(|review| {
            let created_at = real(review.submitted_at.or(review.updated_at)).unwrap_or_else(epoch);
            Thread {
                id: ThreadId::new(format!("review-{}", review.id)),
                anchor: None,
                resolution: Resolution::NotResolvable,
                outdated: false,
                comments: vec![Comment {
                    id: CommentId::new(format!("review-{}", review.id)),
                    author: review.author(),
                    body: review.body.clone(),
                    created_at,
                    edited_at: None,
                    web_url: Some(review.html_url.clone()).filter(|url| !url.is_empty()),
                    review: Some(ReviewId::new(review.id.to_string())),
                    verdict: Some(review.review_state()),
                    suggestions: Vec::new(),
                }],
            }
        })
        .collect()
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ApiIssueComment {
    id: u64,
    #[serde(deserialize_with = "null_default")]
    body: String,
    user: Option<ApiUser>,
    created_at: Option<Timestamp>,
    updated_at: Option<Timestamp>,
    #[serde(deserialize_with = "null_default")]
    html_url: String,
}

impl ApiIssueComment {
    fn into_comment(self) -> Comment {
        let created_at = real(self.created_at).unwrap_or_else(epoch);
        Comment {
            id: CommentId::new(self.id.to_string()),
            author: self.user.unwrap_or_default().into_user(),
            body: self.body,
            created_at,
            edited_at: real(self.updated_at).filter(|updated| *updated != created_at),
            web_url: Some(self.html_url).filter(|url| !url.is_empty()),
            review: None,
            verdict: None,
            suggestions: Vec::new(),
        }
    }

    fn into_thread(self) -> Thread {
        Thread {
            id: ThreadId::new(self.id.to_string()),
            anchor: None,
            resolution: Resolution::NotResolvable,
            outdated: false,
            comments: vec![self.into_comment()],
        }
    }
}

#[derive(Deserialize, Default, Clone)]
#[serde(default)]
struct ApiStatus {
    id: u64,
    #[serde(deserialize_with = "null_default")]
    status: String,
    #[serde(deserialize_with = "null_default")]
    target_url: String,
    #[serde(deserialize_with = "null_default")]
    context: String,
    created_at: Option<Timestamp>,
    updated_at: Option<Timestamp>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ApiRunList {
    #[serde(deserialize_with = "null_default")]
    workflow_runs: Vec<ApiRun>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ApiRun {
    id: u64,
    index_in_repo: u64,
    #[serde(deserialize_with = "null_default")]
    status: String,
    #[serde(deserialize_with = "null_default")]
    title: String,
    #[serde(deserialize_with = "null_default")]
    workflow_id: String,
    #[serde(deserialize_with = "null_default")]
    html_url: String,
    started: Option<Timestamp>,
    stopped: Option<Timestamp>,
    #[serde(deserialize_with = "null_default")]
    event: String,
    #[serde(deserialize_with = "null_default")]
    commit_sha: String,
    #[serde(deserialize_with = "null_default")]
    prettyref: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ApiJob {
    id: u64,
    #[serde(deserialize_with = "null_default")]
    name: String,
    #[serde(deserialize_with = "null_default")]
    status: String,
    #[serde(deserialize_with = "null_default")]
    html_url: String,
    attempt: u32,
    /// Only from `GET .../actions/jobs/{id}` (v17).
    #[serde(deserialize_with = "null_default")]
    steps: Vec<ApiStep>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ApiStep {
    number: u32,
    #[serde(deserialize_with = "null_default")]
    name: String,
    #[serde(deserialize_with = "null_default")]
    status: String,
    started: Option<Timestamp>,
    stopped: Option<Timestamp>,
}

/// Commit status states, and the run and job states Actions reports.
fn status_state(state: &str) -> CiState {
    match state {
        "success" | "warning" => CiState::Pass,
        "failure" | "error" => CiState::Fail,
        "pending" | "waiting" | "blocked" => CiState::Queued,
        "running" => CiState::Running,
        "cancelled" | "canceled" => CiState::Canceled,
        "skipped" => CiState::Skipped,
        "" => CiState::None,
        _ => CiState::Queued,
    }
}

/// `(run index_in_repo, job index)` of an Actions status's `target_url`.
fn actions_target(url: &str) -> Option<(u64, usize)> {
    let rest = url.split("/actions/runs/").nth(1)?;
    let mut parts = rest.split('/');
    let run = parts.next()?.parse().ok()?;
    if parts.next()? != "jobs" {
        return None;
    }
    let job = parts.next()?.split(['?', '#']).next()?.parse().ok()?;
    Some((run, job))
}

fn absolute(host: &str, url: &str) -> Option<String> {
    match url {
        "" => None,
        url if url.starts_with('/') => Some(format!("https://{host}{url}")),
        url => Some(url.to_owned()),
    }
}

/// Latest status per context, joined to the Actions run and job it came
/// from; jobs and runs without a status of their own get a row too.
fn build_checks(
    host: &str,
    statuses: &[ApiStatus],
    runs: &[ApiRun],
    jobs: &HashMap<u64, Vec<ApiJob>>,
) -> Vec<CheckRun> {
    let mut latest: BTreeMap<&str, &ApiStatus> = BTreeMap::new();
    for status in statuses {
        let slot = latest.entry(status.context.as_str()).or_insert(status);
        if (status.updated_at, status.id) >= (slot.updated_at, slot.id) {
            *slot = status;
        }
    }
    let run_by_index: HashMap<u64, &ApiRun> =
        runs.iter().map(|run| (run.index_in_repo, run)).collect();
    let mut seen_jobs: HashSet<u64> = HashSet::new();
    let mut seen_runs: HashSet<u64> = HashSet::new();
    let mut checks = Vec::new();

    for status in latest.values() {
        let target = actions_target(&status.target_url);
        let run = target.and_then(|(index, _)| run_by_index.get(&index).copied());
        let job = target
            .zip(run)
            .and_then(|((_, job_index), run)| jobs.get(&run.id)?.get(job_index));
        if let Some(run) = run {
            seen_runs.insert(run.id);
        }
        if let Some(job) = job {
            seen_jobs.insert(job.id);
        }
        let mut state = status_state(&status.status);
        if state == CiState::Queued {
            // The status only says "pending"; the job or run knows more.
            let live = job
                .map(|job| status_state(&job.status))
                .or_else(|| run.map(|run| status_state(&run.status)));
            if live == Some(CiState::Running) {
                state = CiState::Running;
            }
        }
        let (group, name) = match status.context.split_once(" / ") {
            Some((group, name)) => (Some(group.to_owned()), name.to_owned()),
            None => (None, status.context.clone()),
        };
        let started_at = real(status.created_at);
        checks.push(CheckRun {
            id: CheckRunId::new(format!("status-{}", status.id)),
            name,
            group,
            state,
            url: absolute(host, &status.target_url),
            job: job.map(|job| JobId::new(job.id.to_string())),
            run: run.map(|run| RunId::new(run.id.to_string())),
            started_at,
            completed_at: matches!(
                state,
                CiState::Pass | CiState::Fail | CiState::Canceled | CiState::Skipped
            )
            .then(|| real(status.updated_at))
            .flatten(),
            steps: Vec::new(),
        });
    }

    for run in runs {
        let listed = jobs.get(&run.id);
        for job in listed.into_iter().flatten() {
            if !seen_jobs.insert(job.id) {
                continue;
            }
            seen_runs.insert(run.id);
            checks.push(CheckRun {
                id: CheckRunId::new(format!("job-{}", job.id)),
                name: job.name.clone(),
                group: Some(run.workflow_id.clone()).filter(|group| !group.is_empty()),
                state: status_state(&job.status),
                url: absolute(host, &job.html_url),
                job: Some(JobId::new(job.id.to_string())),
                run: Some(RunId::new(run.id.to_string())),
                started_at: real(run.started),
                completed_at: real(run.stopped),
                steps: Vec::new(),
            });
        }
        if seen_runs.insert(run.id) {
            checks.push(CheckRun {
                id: CheckRunId::new(format!("run-{}", run.id)),
                name: if run.title.is_empty() {
                    run.workflow_id.clone()
                } else {
                    run.title.clone()
                },
                group: Some(run.workflow_id.clone()).filter(|group| !group.is_empty()),
                state: status_state(&run.status),
                url: absolute(host, &run.html_url),
                job: None,
                run: Some(RunId::new(run.id.to_string())),
                started_at: real(run.started),
                completed_at: real(run.stopped),
                steps: Vec::new(),
            });
        }
    }
    checks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_actions_targets() {
        assert_eq!(
            actions_target("/cora/review-sandbox/actions/runs/7/jobs/2"),
            Some((7, 2))
        );
        assert_eq!(
            actions_target("https://forge.test/o/r/actions/runs/12/jobs/0?x=1"),
            Some((12, 0))
        );
        assert_eq!(actions_target("https://ci.example/build/1"), None);
        assert_eq!(actions_target("/o/r/actions/runs/7"), None);
    }

    #[test]
    fn maps_states() {
        assert_eq!(status_state("success"), CiState::Pass);
        assert_eq!(status_state("warning"), CiState::Pass);
        assert_eq!(status_state("error"), CiState::Fail);
        assert_eq!(status_state("pending"), CiState::Queued);
        assert_eq!(status_state("running"), CiState::Running);
        assert_eq!(status_state("cancelled"), CiState::Canceled);
        assert_eq!(status_state("skipped"), CiState::Skipped);
        assert_eq!(status_state(""), CiState::None);
    }

    #[test]
    fn query_pairs_are_decoded() {
        let pairs = parse_query("state=open&labels=a%2Cb&q=two+words").unwrap();
        assert_eq!(
            pairs,
            vec![
                ("state".to_owned(), "open".to_owned()),
                ("labels".to_owned(), "a,b".to_owned()),
                ("q".to_owned(), "two words".to_owned()),
            ]
        );
    }
}
