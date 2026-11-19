//! GitLab (gitlab.com or self-managed). Hybrid: GraphQL for merge requests,
//! pipelines, stacks and inbox lists (`https://{host}/api/graphql`); REST
//! (`https://{host}/api/v4/`) where GraphQL lacks the data (a note's
//! `line_range`, the free inbox query) and for later writes. Merge requests
//! are addressed by `iid`; a project by its URL-encoded full path
//! (`projects/group%2Fsub%2Frepo`).
//!
//! Which call serves which method (footnotes in `docs/design.md`):
//! - `whoami`: GraphQL `currentUser`.
//! - `inbox` [L8]: GraphQL `currentUser.reviewRequestedMergeRequests` /
//!   `authoredMergeRequests` and `project.mergeRequests`, state `opened`,
//!   newest update first. [`InboxSection::Query`] is REST `GET
//!   /merge_requests?<query>`, whose rows are then loaded with GraphQL
//!   `project.mergeRequests(iids)` so every row has refs and CI.
//! - `pull_request`: GraphQL `project.mergeRequest(iid)`. `merge_base` is
//!   `diffRefs.baseSha`; `base.sha` is `diffRefs.startSha` (the target branch
//!   tip the diff was made against), `head.sha` is `diffRefs.headSha`.
//! - `threads` [L3]: REST `GET .../merge_requests/:iid/discussions`, because
//!   GraphQL `DiffPosition` has no `lineRange` and a multi-line comment
//!   would lose its start line. The current head comes from GraphQL.
//! - `checks` [L5]: GraphQL `headPipeline.stages.jobs`, no steps [L6]. A
//!   pipeline that does not fit one page is read from REST
//!   `GET /projects/:id/pipelines/:pid/jobs` instead.
//! - `stack` [L9]: GraphQL `MergeRequest.stack`; empty means not stacked and
//!   the base-branch rule runs over the project's open merge requests.
//! - `snapshot`: one GraphQL query for the merge request and its pipeline,
//!   plus the REST discussions, concurrently.
//! - `viewed_files`: none [L4]; the trait default (Unsupported) stands.
//!
//! How a thread is classified:
//! - GitLab keeps a diff note's `position` current: when a push leaves the
//!   line in place, the position moves to the new head. A position whose
//!   `head_sha` is not the merge request's current head is therefore one the
//!   forge could not map forward, and the thread is outdated; GitLab's own
//!   "changed this line in version N" system note appears at the same time.
//! - System notes are left out, except the three that are verdicts
//!   (`approved this merge request`, `unapproved this merge request`,
//!   `requested changes`), which become conversation threads with a verdict
//!   and an empty body. When a plain note by the same author sits just before
//!   a verdict note (`left review comments` counts), it is the review's
//!   summary: it takes the verdict and the system note is dropped.
//!
//! Writes are in `writes`: a pending review is the viewer's draft notes.
//!
//! Ids: a merge request's [`PullRequestId`] is its global id
//! (`gid://gitlab/MergeRequest/<id>`, what GraphQL writes take); a thread id
//! is the discussion's hex id (`gid://gitlab/Discussion/<id>` for GraphQL); a
//! comment id is the numeric note id; a check's id is the job's global id,
//! its [`JobId`] the numeric job id and its [`RunId`] the numeric pipeline id.
//!
//! REST sends ETags; GraphQL does not.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::OnceCell;

use crate::capabilities::Capabilities;
use crate::error::{Error, Result};
use crate::forge::{Forge, INBOX_LIMIT};
use crate::http::{HttpClient, HttpRequest, Url};
use crate::model::{
    AccountKind, Anchor, BranchRef, CheckRun, CheckRunId, CiState, Comment, CommentId, DiffStat,
    ForgeKind, InboxSection, JobId, Label, LineRef, Milestone, Position, PrNumber, PrState,
    PullRequest, PullRequestId, RepoRef, Resolution, ReviewDecision, ReviewState, Reviewer, RunId,
    Sha, Side, Snapshot, Stack, StackLink, StackSource, Thread, ThreadId, Timestamp, User, UserId,
};
use crate::model::{
    CiDetail, CiJob, CiRun, LogChunk, MergeOptions, MergeSettings, Mergeability, NewComment,
    PendingComment, PendingReview, PullRequestPulse, Review, ReviewId, Suggestion, SuggestionId,
    Verdict,
};
use crate::people::PickerCache;
use crate::stack::stack_by_base_branch;

mod ci;
mod lifecycle;
mod people;
mod writes;

/// GitLab's largest page, for GraphQL connections and REST `per_page`.
const PAGE: usize = 100;

pub struct GitLab {
    http: HttpClient,
    rest: Url,
    graphql: Url,
    /// The signed-in account, asked for once; review writes name it.
    viewer: OnceCell<User>,
    /// How long an approval waits between looks for its system note.
    settle: Duration,
    /// The collaborator and label lists behind the pickers.
    picker: PickerCache,
}

impl GitLab {
    /// # Errors
    /// [`Error::Config`] when the host does not make an API URL.
    pub async fn connect(http: HttpClient) -> Result<Self> {
        let host = http.host().to_owned();
        let parse = |text: String| {
            Url::parse(&text).map_err(|err| Error::Config {
                host: host.clone(),
                reason: err.to_string(),
            })
        };
        let rest = parse(format!("https://{host}/api/v4/"))?;
        let graphql = parse(format!("https://{host}/api/graphql"))?;
        Ok(Self {
            http,
            rest,
            graphql,
            viewer: OnceCell::new(),
            settle: Duration::from_secs(1),
            picker: PickerCache::default(),
        })
    }

    /// Sets the wait between the looks `submit_review` takes for the system
    /// note of an approval; tests replaying a recording make it zero.
    #[must_use]
    pub fn with_settle_interval(mut self, interval: Duration) -> Self {
        self.settle = interval;
        self
    }

    async fn query<T: DeserializeOwned>(
        &self,
        operation: &str,
        query: &str,
        variables: Value,
    ) -> Result<T> {
        self.http
            .graphql(self.graphql.clone(), operation, query, variables)
            .await
    }

    fn rest_url(&self, path: &str, query: &[(&str, &str)]) -> Result<Url> {
        let mut url = self.rest.join(path).map_err(|err| Error::Config {
            host: self.http.host().to_owned(),
            reason: err.to_string(),
        })?;
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        Ok(url)
    }

    /// Every page of a REST list (or the first `limit` rows), following
    /// `X-Next-Page`, or the `Link` header where GitLab offers only that
    /// (keyset pagination).
    async fn rest_pages<T: DeserializeOwned>(&self, first: Url, limit: usize) -> Result<Vec<T>> {
        let mut rows: Vec<T> = Vec::new();
        let mut url = first;
        loop {
            let request = HttpRequest::get(url.clone());
            let describe = request.describe();
            let response = self.http.send(request).await?;
            rows.extend(response.json::<Vec<T>>(&describe)?);
            if rows.len() >= limit {
                break;
            }
            let next = match response
                .header("x-next-page")
                .map(str::trim)
                .filter(|page| !page.is_empty())
            {
                Some(page) => Some(with_page(&url, page)),
                None => response.next_page(),
            };
            let Some(next) = next else { break };
            url = next;
        }
        rows.truncate(limit);
        Ok(rows)
    }

    fn pull_request_variables(repo: &RepoRef, number: PrNumber) -> Value {
        json!({"path": repo.path(), "iid": number.to_string()})
    }

    async fn merge_request<T: DeserializeOwned>(
        &self,
        operation: &str,
        query: &str,
        repo: &RepoRef,
        number: PrNumber,
    ) -> Result<T> {
        let data: ProjectData<MergeRequestHolder<T>> = self
            .query(operation, query, Self::pull_request_variables(repo, number))
            .await?;
        data.project
            .and_then(|project| project.merge_request)
            .ok_or_else(|| Error::NotFound {
                request: format!("merge request {repo}!{number}"),
            })
    }

    async fn discussions(&self, repo: &RepoRef, number: PrNumber) -> Result<Vec<RestDiscussion>> {
        let path = format!(
            "projects/{}/merge_requests/{number}/discussions",
            project_segment(repo)
        );
        let url = self.rest_url(&path, &[("per_page", &PAGE.to_string())])?;
        self.rest_pages(url, usize::MAX).await
    }

    /// The jobs of a pipeline from REST, for a pipeline GraphQL cut short.
    async fn pipeline_jobs(&self, repo: &RepoRef, pipeline: &str) -> Result<Vec<RestJob>> {
        let path = format!(
            "projects/{}/pipelines/{}/jobs",
            project_segment(repo),
            gid_number(pipeline)
        );
        let url = self.rest_url(&path, &[("per_page", &PAGE.to_string())])?;
        self.rest_pages(url, usize::MAX).await
    }

    async fn checks_of(
        &self,
        repo: &RepoRef,
        pipeline: Option<&GqlPipeline>,
    ) -> Result<Vec<CheckRun>> {
        let Some(pipeline) = pipeline else {
            return Ok(Vec::new());
        };
        if pipeline.is_truncated() {
            let jobs = self.pipeline_jobs(repo, &pipeline.id).await?;
            return Ok(checks_from_rest(&pipeline.id, jobs));
        }
        Ok(checks_from_graphql(self.http.host(), pipeline))
    }

    /// One connection of merge requests, newest update first, up to
    /// [`INBOX_LIMIT`]. `pointer` locates the connection in `data`.
    async fn inbox_connection(
        &self,
        operation: &str,
        selection: &str,
        pointer: &str,
        variables: Value,
    ) -> Result<Vec<PullRequest>> {
        let query = format!(
            "query {operation}($first: Int!, $after: String{extra}) {{ {selection} }}
             {MR_FIELDS}",
            extra = if variables.get("path").is_some() {
                ", $path: ID!"
            } else {
                ""
            },
        );
        let mut found: Vec<PullRequest> = Vec::new();
        let mut after = Value::Null;
        loop {
            let mut vars = variables.clone();
            vars["first"] = json!((INBOX_LIMIT - found.len()).min(PAGE));
            vars["after"] = after;
            let mut data: Value = self.query(operation, &query, vars).await?;
            let connection = data.pointer_mut(pointer).map(Value::take);
            let Some(connection) = connection.filter(|value| !value.is_null()) else {
                return Err(Error::NotFound {
                    request: format!("{operation} {variables}"),
                });
            };
            let connection: Connection<GqlMr> =
                serde_json::from_value(connection).map_err(|err| Error::Decode {
                    request: operation.to_owned(),
                    reason: err.to_string(),
                })?;
            for mr in &connection.nodes {
                found.push(to_pull_request(self.http.host(), mr)?);
            }
            let page = connection.page_info.unwrap_or_default();
            match page.end_cursor {
                Some(cursor) if page.has_next_page && found.len() < INBOX_LIMIT => {
                    after = Value::String(cursor);
                }
                _ => break,
            }
        }
        found.truncate(INBOX_LIMIT);
        Ok(found)
    }

    /// The free query: REST finds the merge requests, GraphQL loads them.
    async fn inbox_query(&self, query: &str) -> Result<Vec<PullRequest>> {
        let mut url = self.rest_url("merge_requests", &[])?;
        url.set_query(Some(query.trim_start_matches('?')));
        let has = |url: &Url, key: &str| url.query_pairs().any(|(name, _)| name == key);
        if !has(&url, "per_page") {
            url.query_pairs_mut()
                .append_pair("per_page", &PAGE.to_string());
        }
        if !has(&url, "order_by") && !has(&url, "sort") {
            url.query_pairs_mut()
                .append_pair("order_by", "updated_at")
                .append_pair("sort", "desc");
        }
        let rows: Vec<RestMergeRequestRow> = self.rest_pages(url, INBOX_LIMIT).await?;

        let mut by_project: Vec<(String, Vec<u64>)> = Vec::new();
        let mut order: Vec<(String, u64)> = Vec::new();
        for row in &rows {
            let Some((project, _)) = row.references.full.rsplit_once('!') else {
                continue;
            };
            order.push((project.to_owned(), row.iid));
            match by_project.iter_mut().find(|(path, _)| path == project) {
                Some((_, iids)) => iids.push(row.iid),
                None => by_project.push((project.to_owned(), vec![row.iid])),
            }
        }

        let mut loaded: HashMap<(String, u64), PullRequest> = HashMap::new();
        for (project, iids) in by_project {
            for chunk in iids.chunks(PAGE) {
                let iids: Vec<String> = chunk.iter().map(u64::to_string).collect();
                let query = format!(
                    "query InboxLoad($path: ID!, $iids: [String!], $first: Int!) {{
                       project(fullPath: $path) {{
                         mergeRequests(iids: $iids, state: all, first: $first) {{
                           nodes {{ ...MrFields }}
                         }}
                       }}
                     }}
                     {MR_FIELDS}"
                );
                let data: ProjectData<MergeRequestsHolder> = self
                    .query(
                        "InboxLoad",
                        &query,
                        json!({"path": project, "iids": iids, "first": chunk.len()}),
                    )
                    .await?;
                let nodes = data.project.map(|p| p.merge_requests.nodes);
                for mr in nodes.iter().flatten() {
                    let pr = to_pull_request(self.http.host(), mr)?;
                    loaded.insert((project.clone(), pr.number.0), pr);
                }
            }
        }
        Ok(order
            .into_iter()
            .filter_map(|key| loaded.remove(&key))
            .collect())
    }
}

#[async_trait]
impl Forge for GitLab {
    fn kind(&self) -> ForgeKind {
        ForgeKind::GitLab
    }

    fn host(&self) -> &str {
        self.http.host()
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::GITLAB
    }

    async fn whoami(&self) -> Result<User> {
        let query = "query Whoami { currentUser { id username name avatarUrl bot } }";
        let data: CurrentUserData = self.query("Whoami", query, json!({})).await?;
        let user = data.current_user.ok_or_else(|| Error::Auth {
            host: self.http.host().to_owned(),
            reason: "GitLab reports no signed-in user".to_owned(),
        })?;
        Ok(user.to_user(self.http.host()))
    }

    async fn inbox(&self, section: &InboxSection) -> Result<Vec<PullRequest>> {
        let user_list = |field: &str| {
            format!(
                "currentUser {{ {field}(state: opened, sort: UPDATED_DESC, first: $first, after: $after) {{ {CONNECTION} }} }}"
            )
        };
        match section {
            InboxSection::ReviewRequested => {
                self.inbox_connection(
                    "InboxReviewRequested",
                    &user_list("reviewRequestedMergeRequests"),
                    "/currentUser/reviewRequestedMergeRequests",
                    json!({}),
                )
                .await
            }
            InboxSection::AuthoredByMe => {
                self.inbox_connection(
                    "InboxAuthored",
                    &user_list("authoredMergeRequests"),
                    "/currentUser/authoredMergeRequests",
                    json!({}),
                )
                .await
            }
            InboxSection::OpenInRepo(repo) => {
                let selection = format!(
                    "project(fullPath: $path) {{ mergeRequests(state: opened, sort: UPDATED_DESC, first: $first, after: $after) {{ {CONNECTION} }} }}"
                );
                self.inbox_connection(
                    "InboxProject",
                    &selection,
                    "/project/mergeRequests",
                    json!({"path": repo.path()}),
                )
                .await
            }
            InboxSection::Query(query) => self.inbox_query(query).await,
        }
    }

    async fn pull_request(&self, repo: &RepoRef, number: PrNumber) -> Result<PullRequest> {
        let query = format!(
            "query PullRequest($path: ID!, $iid: String!) {{
               project(fullPath: $path) {{ mergeRequest(iid: $iid) {{ ...MrFields }} }}
             }}
             {MR_FIELDS}"
        );
        let mr: GqlMr = self
            .merge_request("PullRequest", &query, repo, number)
            .await?;
        to_pull_request(self.http.host(), &mr)
    }

    async fn threads(&self, repo: &RepoRef, number: PrNumber) -> Result<Vec<Thread>> {
        let query = "query ThreadsHead($path: ID!, $iid: String!) {
               project(fullPath: $path) { mergeRequest(iid: $iid) { webUrl diffHeadSha diffRefs { headSha } } }
             }";
        let head = self.merge_request::<GqlHead>("ThreadsHead", query, repo, number);
        let (head, discussions) = tokio::try_join!(head, self.discussions(repo, number))?;
        let context = ThreadContext {
            host: self.http.host(),
            head: head.head_sha(),
            web_url: &head.web_url,
        };
        Ok(build_threads(&discussions, &context))
    }

    async fn checks(&self, repo: &RepoRef, number: PrNumber) -> Result<Vec<CheckRun>> {
        let query = format!(
            "query Checks($path: ID!, $iid: String!) {{
               project(fullPath: $path) {{ mergeRequest(iid: $iid) {{ headPipeline {{ {PIPELINE_JOBS} }} }} }}
             }}"
        );
        let mr: GqlPipelineOnly = self.merge_request("Checks", &query, repo, number).await?;
        self.checks_of(repo, mr.head_pipeline.as_ref()).await
    }

    async fn stack(&self, repo: &RepoRef, number: PrNumber) -> Result<Stack> {
        let query = format!(
            "query Stack($path: ID!, $iid: String!) {{
               project(fullPath: $path) {{ mergeRequest(iid: $iid) {{
                 ...MrFields
                 stack {{ iid title state draft sourceBranch targetBranch }}
               }} }}
             }}
             {MR_FIELDS}"
        );
        let mr: GqlStacked = self.merge_request("Stack", &query, repo, number).await?;
        let native: Vec<StackLink> = mr
            .stack
            .iter()
            .map(GqlStackEntry::link)
            .collect::<Result<_>>()?;
        if !native.is_empty() {
            return Ok(Stack {
                source: StackSource::Native,
                links: order_stack(native),
            });
        }
        let target = to_pull_request(self.http.host(), &mr.mr)?;
        let open = self.inbox(&InboxSection::OpenInRepo(repo.clone())).await?;
        Ok(stack_by_base_branch(&target, &open))
    }

    async fn snapshot(&self, repo: &RepoRef, number: PrNumber) -> Result<Snapshot> {
        let query = format!(
            "query Snapshot($path: ID!, $iid: String!) {{
               project(fullPath: $path) {{ mergeRequest(iid: $iid) {{
                 ...MrFields
                 headPipeline {{ {PIPELINE_JOBS} }}
               }} }}
             }}
             {MR_FIELDS}"
        );
        let mr = self.merge_request::<GqlMr>("Snapshot", &query, repo, number);
        let (mr, discussions) = tokio::try_join!(mr, self.discussions(repo, number))?;
        let context = ThreadContext {
            host: self.http.host(),
            head: mr.diff_refs.as_ref().map_or_else(
                || mr.diff_head_sha.as_deref().unwrap_or_default(),
                |refs| refs.head_sha.as_str(),
            ),
            web_url: &mr.web_url,
        };
        let threads = build_threads(&discussions, &context);
        let checks = self.checks_of(repo, mr.head_pipeline.as_ref()).await?;
        Ok(Snapshot {
            pull_request: to_pull_request(self.http.host(), &mr)?,
            threads,
            checks,
        })
    }

    // ---- writes, in `writes` ----

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
        _review: &ReviewId,
        comment: &NewComment,
    ) -> Result<PendingComment> {
        writes::add_review_comment(self, repo, number, comment).await
    }

    async fn delete_review_comment(
        &self,
        repo: &RepoRef,
        number: PrNumber,
        comment: &CommentId,
    ) -> Result<()> {
        writes::delete_review_comment(self, repo, number, comment).await
    }

    async fn edit_review_comment(
        &self,
        repo: &RepoRef,
        number: PrNumber,
        comment: &PendingComment,
        body: &str,
    ) -> Result<PendingComment> {
        writes::edit_review_comment(self, repo, number, comment, body).await
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
        _review: &ReviewId,
    ) -> Result<()> {
        writes::discard_review(self, repo, number).await
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

    async fn set_resolved(
        &self,
        repo: &RepoRef,
        number: PrNumber,
        thread: &ThreadId,
        resolved: bool,
    ) -> Result<Thread> {
        writes::set_resolved(self, repo, number, thread, resolved).await
    }

    async fn add_comment(&self, repo: &RepoRef, number: PrNumber, body: &str) -> Result<Comment> {
        writes::add_comment(self, repo, number, body).await
    }

    async fn apply_suggestions(
        &self,
        _repo: &RepoRef,
        _number: PrNumber,
        suggestions: &[SuggestionId],
    ) -> Result<()> {
        writes::apply_suggestions(self, suggestions).await
    }

    async fn retry_job(&self, repo: &RepoRef, job: &JobId) -> Result<()> {
        writes::retry_job(self, repo, job).await
    }

    async fn retry_failed_jobs(&self, repo: &RepoRef, run: &RunId) -> Result<()> {
        writes::retry_failed_jobs(self, repo, run).await
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
        GitLab::ci_runs(self, repo, sha).await
    }

    async fn ci_run(&self, repo: &RepoRef, run: &RunId, detail: CiDetail) -> Result<CiRun> {
        GitLab::ci_run(self, repo, run, detail).await
    }

    async fn ci_job(&self, repo: &RepoRef, job: &JobId) -> Result<CiJob> {
        GitLab::ci_job(self, repo, job).await
    }

    async fn job_log(&self, repo: &RepoRef, job: &JobId, from: u64) -> Result<LogChunk> {
        GitLab::job_log(self, repo, job, from).await
    }

    async fn pull_request_pulse(
        &self,
        repo: &RepoRef,
        number: PrNumber,
    ) -> Result<PullRequestPulse> {
        GitLab::pull_request_pulse(self, repo, number).await
    }

    async fn ci_states(&self, repo: &RepoRef, numbers: &[PrNumber]) -> Result<Vec<CiState>> {
        GitLab::ci_states(self, repo, numbers).await
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

    async fn set_auto_merge(
        &self,
        repo: &RepoRef,
        number: PrNumber,
        options: Option<&MergeOptions>,
    ) -> Result<PullRequest> {
        lifecycle::set_auto_merge(self, repo, number, options).await
    }

    async fn close(&self, repo: &RepoRef, number: PrNumber) -> Result<PullRequest> {
        lifecycle::set_state(self, repo, number, "close").await
    }

    async fn reopen(&self, repo: &RepoRef, number: PrNumber) -> Result<PullRequest> {
        lifecycle::set_state(self, repo, number, "reopen").await
    }

    async fn set_draft(
        &self,
        repo: &RepoRef,
        number: PrNumber,
        draft: bool,
    ) -> Result<PullRequest> {
        lifecycle::set_draft(self, repo, number, draft).await
    }

    // Kept default: `set_viewed` (no API [L4], viewed state is local).
}

// ---------------------------------------------------------------------------
// GraphQL documents

/// The selection `inbox` and `pull_request` share. Jobs are not in it:
/// `PIPELINE_JOBS` is added only where checks are wanted.
const MR_FIELDS: &str = "fragment MrFields on MergeRequest {
  id iid title description draft state sourceBranch targetBranch diffHeadSha
  diffRefs { baseSha startSha headSha }
  sourceProject { fullPath }
  targetProject { fullPath }
  createdAt updatedAt webUrl
  author { id username name avatarUrl bot }
  reviewers(first: 100) { nodes { id username name avatarUrl bot mergeRequestInteraction { reviewState approved approvedAt } } }
  approvedBy(first: 100) { nodes { id username name avatarUrl bot } }
  mergeRequestDiffs(first: 1) { nodes { createdAt } }
  labels(first: 100) { nodes { title color description } }
  assignees(first: 100) { nodes { id username name avatarUrl bot } }
  milestone { title }
  diffStatsSummary { additions deletions fileCount }
  userNotesCount resolvableDiscussionsCount resolvedDiscussionsCount
  headPipeline { id status }
  userPermissions { pushToSourceBranch }
}";

const CONNECTION: &str = "nodes { ...MrFields } pageInfo { hasNextPage endCursor }";

/// A pipeline with its stages and jobs, 100 of each; `pageInfo` says when
/// that was not all of them.
const PIPELINE_JOBS: &str = "id status
  stages(first: 100) {
    pageInfo { hasNextPage }
    nodes { name jobs(first: 100) {
      pageInfo { hasNextPage }
      nodes { id name status webPath startedAt finishedAt }
    } }
  }";

// ---------------------------------------------------------------------------
// GraphQL response shapes

#[derive(Deserialize)]
struct ProjectData<T> {
    project: Option<T>,
}

#[derive(Deserialize)]
struct MergeRequestHolder<T> {
    #[serde(rename = "mergeRequest")]
    merge_request: Option<T>,
}

#[derive(Deserialize)]
struct MergeRequestsHolder {
    #[serde(rename = "mergeRequests")]
    merge_requests: Connection<GqlMr>,
}

#[derive(Deserialize)]
struct CurrentUserData {
    #[serde(rename = "currentUser")]
    current_user: Option<GqlUser>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Connection<T> {
    #[serde(default = "Vec::new")]
    nodes: Vec<T>,
    page_info: Option<PageInfo>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    #[serde(default)]
    has_next_page: bool,
    end_cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlUser {
    id: String,
    username: String,
    name: Option<String>,
    avatar_url: Option<String>,
    #[serde(default)]
    bot: bool,
}

impl GqlUser {
    fn to_user(&self, host: &str) -> User {
        User {
            id: Some(UserId::new(&self.id)),
            login: self.username.clone(),
            name: self.name.clone(),
            avatar_url: self.avatar_url.as_deref().map(|url| absolute(host, url)),
            kind: if self.bot {
                AccountKind::Bot
            } else {
                AccountKind::User
            },
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlReviewer {
    #[serde(flatten)]
    user: GqlUser,
    merge_request_interaction: Option<GqlInteraction>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlInteraction {
    review_state: Option<String>,
    #[serde(default)]
    approved: bool,
    approved_at: Option<Timestamp>,
}

/// One version of the diff; a push or a rebase makes a new one.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlDiffVersion {
    created_at: Timestamp,
}

#[derive(Deserialize)]
struct GqlLabel {
    title: String,
    color: Option<String>,
    description: Option<String>,
}

#[derive(Deserialize)]
struct GqlMilestone {
    title: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlDiffRefs {
    base_sha: Option<String>,
    start_sha: String,
    head_sha: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlStats {
    additions: u32,
    deletions: u32,
    file_count: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlProject {
    full_path: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlMr {
    id: String,
    iid: String,
    title: String,
    description: Option<String>,
    #[serde(default)]
    draft: bool,
    state: String,
    source_branch: String,
    target_branch: String,
    diff_head_sha: Option<String>,
    diff_refs: Option<GqlDiffRefs>,
    source_project: Option<GqlProject>,
    target_project: GqlProject,
    created_at: Timestamp,
    updated_at: Timestamp,
    web_url: String,
    author: Option<GqlUser>,
    reviewers: Option<Connection<GqlReviewer>>,
    approved_by: Option<Connection<GqlUser>>,
    merge_request_diffs: Option<Connection<GqlDiffVersion>>,
    labels: Option<Connection<GqlLabel>>,
    assignees: Option<Connection<GqlUser>>,
    milestone: Option<GqlMilestone>,
    diff_stats_summary: Option<GqlStats>,
    user_notes_count: Option<u32>,
    resolvable_discussions_count: Option<u32>,
    resolved_discussions_count: Option<u32>,
    head_pipeline: Option<GqlPipeline>,
    user_permissions: Option<GqlMrPermissions>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlMrPermissions {
    #[serde(default)]
    push_to_source_branch: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlPipeline {
    id: String,
    status: String,
    stages: Option<Connection<GqlStage>>,
}

impl GqlPipeline {
    /// GraphQL stopped at a page, so the job list is not complete.
    fn is_truncated(&self) -> bool {
        let more = |page: &Option<PageInfo>| page.as_ref().is_some_and(|p| p.has_next_page);
        self.stages.as_ref().is_some_and(|stages| {
            more(&stages.page_info) || stages.nodes.iter().any(|s| more(&s.jobs.page_info))
        })
    }
}

#[derive(Deserialize)]
struct GqlStage {
    name: String,
    jobs: Connection<GqlJob>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlJob {
    id: String,
    name: String,
    status: Option<String>,
    web_path: Option<String>,
    started_at: Option<Timestamp>,
    finished_at: Option<Timestamp>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlPipelineOnly {
    head_pipeline: Option<GqlPipeline>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlHead {
    web_url: String,
    diff_head_sha: Option<String>,
    diff_refs: Option<GqlHeadRefs>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlHeadRefs {
    head_sha: String,
}

impl GqlHead {
    fn head_sha(&self) -> &str {
        self.diff_refs
            .as_ref()
            .map(|refs| refs.head_sha.as_str())
            .or(self.diff_head_sha.as_deref())
            .unwrap_or_default()
    }
}

#[derive(Deserialize)]
struct GqlStacked {
    #[serde(flatten)]
    mr: GqlMr,
    #[serde(default)]
    stack: Vec<GqlStackEntry>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlStackEntry {
    iid: String,
    title: String,
    state: String,
    #[serde(default)]
    draft: bool,
    source_branch: String,
    target_branch: String,
}

impl GqlStackEntry {
    fn link(&self) -> Result<StackLink> {
        Ok(StackLink {
            number: parse_iid(&self.iid)?,
            title: self.title.clone(),
            state: pr_state(&self.state, self.draft),
            head: self.source_branch.clone(),
            base: self.target_branch.clone(),
        })
    }
}

// ---------------------------------------------------------------------------
// REST response shapes

#[derive(Deserialize)]
struct RestMergeRequestRow {
    iid: u64,
    references: RestReferences,
}

#[derive(Deserialize)]
struct RestReferences {
    /// `group/project!iid`.
    full: String,
}

#[derive(Deserialize)]
struct RestDiscussion {
    id: String,
    #[serde(default)]
    notes: Vec<RestNote>,
}

#[derive(Deserialize)]
struct RestNote {
    id: u64,
    body: String,
    author: Option<RestUser>,
    created_at: Timestamp,
    #[serde(default)]
    system: bool,
    #[serde(default)]
    resolvable: bool,
    #[serde(default)]
    resolved: bool,
    resolved_by: Option<RestUser>,
    position: Option<RestPosition>,
    #[serde(default)]
    suggestions: Vec<RestSuggestion>,
}

/// One of a note's `suggestions` [L4].
#[derive(Deserialize)]
struct RestSuggestion {
    id: u64,
    #[serde(default)]
    applied: bool,
    #[serde(default)]
    appliable: bool,
    from_line: Option<u32>,
    to_line: Option<u32>,
    from_content: Option<String>,
    to_content: Option<String>,
    #[serde(default)]
    lines_above: u32,
    #[serde(default)]
    lines_below: u32,
}

#[derive(Deserialize)]
struct RestUser {
    id: u64,
    username: String,
    name: Option<String>,
    avatar_url: Option<String>,
    #[serde(default)]
    bot: bool,
}

impl RestUser {
    fn to_user(&self, host: &str) -> User {
        User {
            id: Some(UserId::new(format!("gid://gitlab/User/{}", self.id))),
            login: self.username.clone(),
            name: self.name.clone(),
            avatar_url: self.avatar_url.as_deref().map(|url| absolute(host, url)),
            kind: if self.bot {
                AccountKind::Bot
            } else {
                AccountKind::User
            },
        }
    }
}

#[derive(Deserialize)]
struct RestPosition {
    /// Null on the position of a draft reply.
    #[serde(default, deserialize_with = "null_as_default")]
    head_sha: String,
    position_type: Option<String>,
    old_path: Option<String>,
    new_path: Option<String>,
    old_line: Option<u32>,
    new_line: Option<u32>,
    line_range: Option<RestLineRange>,
}

fn null_as_default<'de, D, T>(deserializer: D) -> std::result::Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Option::<T>::deserialize(deserializer).map(Option::unwrap_or_default)
}

#[derive(Deserialize)]
struct RestLineRange {
    start: RestRangeEnd,
}

#[derive(Deserialize)]
struct RestRangeEnd {
    #[serde(rename = "type")]
    side: Option<String>,
    old_line: Option<u32>,
    new_line: Option<u32>,
}

#[derive(Deserialize)]
struct RestJob {
    id: u64,
    name: String,
    status: String,
    stage: Option<String>,
    web_url: Option<String>,
    started_at: Option<Timestamp>,
    finished_at: Option<Timestamp>,
}

// ---------------------------------------------------------------------------
// Mapping

/// `Project/Sub/Repo` -> `Project%2FSub%2FRepo`, GitLab's `:id`.
fn project_segment(repo: &RepoRef) -> String {
    repo.path().replace('/', "%2F")
}

fn repo_ref(host: &str, full_path: &str) -> RepoRef {
    let (owner, name) = full_path.rsplit_once('/').unwrap_or(("", full_path));
    RepoRef::new(host, owner, name)
}

/// `gid://gitlab/Ci::Build/123` -> `123`.
fn gid_number(gid: &str) -> &str {
    gid.rsplit('/').next().unwrap_or(gid)
}

/// GraphQL avatars of self-managed instances can be paths on the host.
fn absolute(host: &str, url: &str) -> String {
    if url.starts_with('/') {
        format!("https://{host}{url}")
    } else {
        url.to_owned()
    }
}

fn with_page(url: &Url, page: &str) -> Url {
    let kept: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(name, _)| name != "page")
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect();
    let mut next = url.clone();
    next.query_pairs_mut()
        .clear()
        .extend_pairs(kept)
        .append_pair("page", page);
    next
}

fn parse_iid(iid: &str) -> Result<PrNumber> {
    iid.parse().map(PrNumber).map_err(|err| Error::Decode {
        request: "merge request".to_owned(),
        reason: format!("iid {iid:?}: {err}"),
    })
}

/// GitLab `state` is `opened`, `closed`, `merged` or `locked` (a merge in
/// progress, still open for review).
fn pr_state(state: &str, draft: bool) -> PrState {
    match state {
        "merged" => PrState::Merged,
        "closed" => PrState::Closed,
        _ if draft => PrState::Draft,
        _ => PrState::Open,
    }
}

fn ci_state(status: &str) -> CiState {
    match status.to_ascii_lowercase().as_str() {
        "created"
        | "waiting_for_resource"
        | "preparing"
        | "pending"
        | "scheduled"
        | "waiting_for_callback" => CiState::Queued,
        "running" => CiState::Running,
        "success" => CiState::Pass,
        "failed" => CiState::Fail,
        "canceled" | "cancelled" | "canceling" => CiState::Canceled,
        "skipped" | "manual" => CiState::Skipped,
        _ => CiState::None,
    }
}

fn review_state(state: Option<&str>) -> ReviewState {
    match state {
        Some("REVIEWED") => ReviewState::Commented,
        Some("REQUESTED_CHANGES") => ReviewState::ChangesRequested,
        Some("APPROVED") => ReviewState::Approved,
        Some("UNAPPROVED") => ReviewState::Dismissed,
        Some("REVIEW_STARTED") => ReviewState::Pending,
        _ => ReviewState::Requested,
    }
}

/// The reviewers GitLab lists, plus approvers who were never asked.
///
/// GitLab keeps an approval after a push and after a re-request, so
/// `approvedBy` alone would say "approved". Two things correct it:
/// - `UNREVIEWED` on someone with an earlier approval is a re-request: the
///   approval is still on record (`approvedAt`, `approved`, `approvedBy`),
///   but the person was asked again.
/// - an approval made before the newest diff version (the last push) is
///   stale. An approver who is not a reviewer has no timestamp in GraphQL,
///   so such an approval is never marked stale.
fn gitlab_reviewers(host: &str, mr: &GqlMr) -> Vec<Reviewer> {
    let last_push = mr
        .merge_request_diffs
        .iter()
        .flat_map(|list| &list.nodes)
        .map(|diff| diff.created_at)
        .max();
    let approvers: Vec<&str> = mr
        .approved_by
        .iter()
        .flat_map(|list| &list.nodes)
        .map(|approver| approver.username.as_str())
        .collect();

    let mut reviewers: Vec<Reviewer> = mr
        .reviewers
        .iter()
        .flat_map(|list| &list.nodes)
        .map(|node| {
            let interaction = node.merge_request_interaction.as_ref();
            let state = review_state(interaction.and_then(|i| i.review_state.as_deref()));
            let approved_at = interaction.and_then(|i| i.approved_at);
            let approved_before = approved_at.is_some()
                || interaction.is_some_and(|i| i.approved)
                || approvers.contains(&node.user.username.as_str());
            Reviewer {
                user: node.user.to_user(host),
                state,
                requested_again: state == ReviewState::Requested && approved_before,
                stale: state == ReviewState::Approved
                    && approved_at
                        .zip(last_push)
                        .is_some_and(|(approved, push)| approved < push),
            }
        })
        .collect();
    for approver in mr.approved_by.iter().flat_map(|list| &list.nodes) {
        if reviewers.iter().all(|r| r.user.login != approver.username) {
            reviewers.push(Reviewer::new(approver.to_user(host), ReviewState::Approved));
        }
    }
    reviewers
}

/// A merge request with reviewers and nobody decided yet needs review.
fn review_decision(reviewers: &[Reviewer]) -> ReviewDecision {
    match ReviewDecision::from_reviewers(reviewers) {
        ReviewDecision::None if !reviewers.is_empty() => ReviewDecision::ReviewRequired,
        decision => decision,
    }
}

fn to_pull_request(host: &str, mr: &GqlMr) -> Result<PullRequest> {
    let number = parse_iid(&mr.iid)?;
    let target = repo_ref(host, &mr.target_project.full_path);
    let refs = mr.diff_refs.as_ref();
    let head_sha = refs
        .map(|refs| refs.head_sha.as_str())
        .or(mr.diff_head_sha.as_deref())
        .unwrap_or_default();
    let base_sha = refs.map_or("", |refs| refs.start_sha.as_str());
    let head_repo = mr
        .source_project
        .as_ref()
        .map(|project| repo_ref(host, &project.full_path));
    let reviewers = gitlab_reviewers(host, mr);

    Ok(PullRequest {
        id: PullRequestId::new(&mr.id),
        repo: target,
        number,
        title: mr.title.clone(),
        body: mr.description.clone().unwrap_or_default(),
        author: mr
            .author
            .as_ref()
            .map_or_else(|| User::new("ghost"), |author| author.to_user(host)),
        state: pr_state(&mr.state, mr.draft),
        base: BranchRef {
            repo: Some(repo_ref(host, &mr.target_project.full_path)),
            name: mr.target_branch.clone(),
            sha: Sha::new(base_sha),
        },
        head: BranchRef {
            repo: head_repo,
            name: mr.source_branch.clone(),
            sha: Sha::new(head_sha),
        },
        merge_base: refs.and_then(|refs| refs.base_sha.as_deref()).map(Sha::new),
        created_at: mr.created_at,
        updated_at: mr.updated_at,
        review_decision: review_decision(&reviewers),
        reviewers,
        ci: Some(
            mr.head_pipeline
                .as_ref()
                .map_or(CiState::None, |pipeline| ci_state(&pipeline.status)),
        ),
        labels: mr
            .labels
            .iter()
            .flat_map(|list| &list.nodes)
            .map(|label| Label {
                name: label.title.clone(),
                color: label
                    .color
                    .as_deref()
                    .map(|color| color.trim_start_matches('#').to_owned()),
                description: label.description.clone().filter(|text| !text.is_empty()),
            })
            .collect(),
        assignees: mr
            .assignees
            .iter()
            .flat_map(|list| &list.nodes)
            .map(|node| node.to_user(host))
            .collect(),
        milestone: mr.milestone.as_ref().map(|milestone| Milestone {
            title: milestone.title.clone(),
        }),
        diff_stat: mr.diff_stats_summary.as_ref().map(|stats| DiffStat {
            additions: stats.additions,
            deletions: stats.deletions,
            changed_files: stats.file_count,
        }),
        comment_count: mr.user_notes_count,
        unresolved_threads: mr
            .resolvable_discussions_count
            .zip(mr.resolved_discussions_count)
            .map(|(resolvable, resolved)| resolvable.saturating_sub(resolved)),
        web_url: mr.web_url.clone(),
        head_writable: mr
            .user_permissions
            .as_ref()
            .is_some_and(|permissions| permissions.push_to_source_branch),
    })
}

/// Bottom first. GitLab lists a stack in order; this only checks the chain
/// (each merge request's base is the head branch of the one below) and keeps
/// the listed order when the chain does not account for every entry.
fn order_stack(links: Vec<StackLink>) -> Vec<StackLink> {
    let mut order: Vec<usize> = Vec::with_capacity(links.len());
    let mut current = links
        .iter()
        .position(|link| links.iter().all(|other| other.head != link.base));
    while let Some(index) = current {
        order.push(index);
        current = links
            .iter()
            .enumerate()
            .find(|(next, link)| link.base == links[index].head && !order.contains(next))
            .map(|(next, _)| next);
    }
    if order.len() != links.len() {
        return links;
    }
    let mut slots: Vec<Option<StackLink>> = links.into_iter().map(Some).collect();
    order
        .into_iter()
        .filter_map(|index| slots[index].take())
        .collect()
}

fn checks_from_graphql(host: &str, pipeline: &GqlPipeline) -> Vec<CheckRun> {
    let run = RunId::new(gid_number(&pipeline.id));
    pipeline
        .stages
        .iter()
        .flat_map(|stages| &stages.nodes)
        .flat_map(|stage| stage.jobs.nodes.iter().map(move |job| (stage, job)))
        .map(|(stage, job)| CheckRun {
            id: CheckRunId::new(&job.id),
            name: job.name.clone(),
            group: Some(stage.name.clone()),
            state: job.status.as_deref().map_or(CiState::None, ci_state),
            url: job
                .web_path
                .as_ref()
                .map(|path| format!("https://{host}{path}")),
            job: Some(JobId::new(gid_number(&job.id))),
            run: Some(run.clone()),
            started_at: job.started_at,
            completed_at: job.finished_at,
            steps: Vec::new(),
        })
        .collect()
}

fn checks_from_rest(pipeline: &str, jobs: Vec<RestJob>) -> Vec<CheckRun> {
    let run = RunId::new(gid_number(pipeline));
    jobs.into_iter()
        .map(|job| CheckRun {
            id: CheckRunId::new(format!("gid://gitlab/Ci::Build/{}", job.id)),
            name: job.name,
            group: job.stage,
            state: ci_state(&job.status),
            url: job.web_url,
            job: Some(JobId::new(job.id.to_string())),
            run: Some(run.clone()),
            started_at: job.started_at,
            completed_at: job.finished_at,
            steps: Vec::new(),
        })
        .collect()
}

struct ThreadContext<'a> {
    host: &'a str,
    /// The merge request's current head commit.
    head: &'a str,
    /// For note links.
    web_url: &'a str,
}

/// The longest gap between a review's summary note and the system note
/// GitLab adds for its verdict. An approval's note comes from a background
/// job, several seconds after the request.
const SUMMARY_WINDOW_SECONDS: i64 = 15;

/// Oldest first.
fn build_threads(discussions: &[RestDiscussion], context: &ThreadContext<'_>) -> Vec<Thread> {
    let (summaries, absorbed) = pair_summaries(discussions);
    let mut threads: Vec<Thread> = discussions
        .iter()
        .filter(|discussion| !absorbed.contains(discussion.id.as_str()))
        .filter_map(|discussion| {
            let mut thread = thread_of(discussion, context)?;
            if let (Some(verdict), Some(first)) = (
                summaries.get(discussion.id.as_str()),
                thread.comments.first_mut(),
            ) {
                first.verdict = Some(*verdict);
            }
            Some(thread)
        })
        .collect();
    threads.sort_by_key(|thread| thread.comments.first().map(|comment| comment.created_at));
    threads
}

/// GitLab has no review object: publishing drafts with a summary leaves the
/// summary as a plain note and the verdict as a system note beside it
/// (`left review comments`, `requested changes`, `approved this merge
/// request`). For each such system note, the latest plain note by the same
/// author within [`SUMMARY_WINDOW_SECONDS`] before it that no other verdict
/// has claimed is the review's summary and takes its verdict; the system note
/// is then left out. Returns the verdict by summary discussion id, and the
/// system notes used.
fn pair_summaries(discussions: &[RestDiscussion]) -> (HashMap<&str, ReviewState>, HashSet<&str>) {
    let mut plain: Vec<(Timestamp, u64, &RestDiscussion)> = Vec::new();
    let mut verdicts: Vec<(Timestamp, u64, ReviewState, &RestDiscussion)> = Vec::new();
    for discussion in discussions {
        let [note] = discussion.notes.as_slice() else {
            continue;
        };
        let Some(author) = note.author.as_ref().map(|user| user.id) else {
            continue;
        };
        if !note.system {
            if note.position.is_none() {
                plain.push((note.created_at, author, discussion));
            }
        } else if let Some(verdict) = system_verdict(&note.body) {
            verdicts.push((note.created_at, author, verdict, discussion));
        }
    }
    plain.sort_by_key(|(at, ..)| *at);
    verdicts.sort_by_key(|(at, ..)| *at);

    let mut summaries = HashMap::new();
    let mut absorbed = HashSet::new();
    for (at, author, verdict, system) in verdicts {
        let summary = plain.iter().rev().find(|(before, by, discussion)| {
            *by == author
                && !summaries.contains_key(discussion.id.as_str())
                && (0..=SUMMARY_WINDOW_SECONDS).contains(&(at - *before).num_seconds())
        });
        if let Some((_, _, summary)) = summary {
            summaries.insert(summary.id.as_str(), verdict);
            absorbed.insert(system.id.as_str());
        }
    }
    (summaries, absorbed)
}

/// The verdict a system note announces.
fn system_verdict(body: &str) -> Option<ReviewState> {
    match body {
        "approved this merge request" => Some(ReviewState::Approved),
        "unapproved this merge request" => Some(ReviewState::Dismissed),
        "requested changes" => Some(ReviewState::ChangesRequested),
        "left review comments" => Some(ReviewState::Commented),
        _ => None,
    }
}

fn thread_of(discussion: &RestDiscussion, context: &ThreadContext<'_>) -> Option<Thread> {
    let notes: Vec<&RestNote> = discussion.notes.iter().filter(|n| !n.system).collect();
    let Some(first) = notes.first() else {
        return verdict_thread(discussion, context);
    };

    let (anchor, outdated) = match first
        .position
        .as_ref()
        .and_then(|p| anchor_of(p, context.head))
    {
        Some((anchor, outdated)) => (Some(anchor), outdated),
        None => (None, false),
    };
    let resolvable: Vec<&&RestNote> = notes.iter().filter(|n| n.resolvable).collect();
    let resolution = if resolvable.is_empty() {
        Resolution::NotResolvable
    } else if resolvable.iter().all(|n| n.resolved) {
        let by = resolvable
            .iter()
            .rev()
            .find_map(|n| n.resolved_by.as_ref())
            .map(|user| user.to_user(context.host));
        Resolution::Resolved { by }
    } else {
        Resolution::Unresolved
    };

    Some(Thread {
        id: ThreadId::new(&discussion.id),
        anchor,
        resolution,
        outdated,
        comments: notes
            .iter()
            .map(|note| comment_of(note, context, None))
            .collect(),
    })
}

/// `approved this merge request` and friends: a conversation entry whose
/// only content is its verdict.
fn verdict_thread(discussion: &RestDiscussion, context: &ThreadContext<'_>) -> Option<Thread> {
    let note = discussion.notes.first()?;
    let verdict =
        system_verdict(&note.body).filter(|verdict| *verdict != ReviewState::Commented)?;
    let mut comment = comment_of(note, context, Some(verdict));
    comment.body.clear();
    Some(Thread {
        id: ThreadId::new(&discussion.id),
        anchor: None,
        resolution: Resolution::NotResolvable,
        outdated: false,
        comments: vec![comment],
    })
}

fn comment_of(
    note: &RestNote,
    context: &ThreadContext<'_>,
    verdict: Option<ReviewState>,
) -> Comment {
    Comment {
        id: CommentId::new(note.id.to_string()),
        author: note
            .author
            .as_ref()
            .map_or_else(|| User::new("ghost"), |user| user.to_user(context.host)),
        body: note.body.clone(),
        created_at: note.created_at,
        // REST has no `last_edited_at`, and `updated_at` also moves when a
        // thread is resolved.
        edited_at: None,
        web_url: Some(format!("{}#note_{}", context.web_url, note.id)),
        review: None,
        verdict,
        suggestions: note
            .suggestions
            .iter()
            .map(|suggestion| Suggestion {
                id: SuggestionId::new(suggestion.id.to_string()),
                applied: suggestion.applied,
                appliable: suggestion.appliable,
                from_line: suggestion.from_line,
                to_line: suggestion.to_line,
                from_content: suggestion.from_content.clone(),
                to_content: suggestion.to_content.clone(),
                lines_above: suggestion.lines_above,
                lines_below: suggestion.lines_below,
            })
            .collect(),
    }
}

/// The commented line, and the first line of a range when the position has
/// one. `None` for a file comment or an image pin.
fn position_lines(position: &RestPosition) -> Option<(LineRef, Option<LineRef>)> {
    let line = match (position.new_line, position.old_line) {
        (Some(line), _) => LineRef {
            side: Side::New,
            line,
        },
        (None, Some(line)) => LineRef {
            side: Side::Old,
            line,
        },
        (None, None) => return None,
    };
    let start = position.line_range.as_ref().and_then(|range| {
        let end = &range.start;
        let start = match (end.side.as_deref(), end.new_line, end.old_line) {
            (Some("old"), _, Some(line)) => LineRef {
                side: Side::Old,
                line,
            },
            (_, Some(line), _) => LineRef {
                side: Side::New,
                line,
            },
            (_, None, Some(line)) => LineRef {
                side: Side::Old,
                line,
            },
            _ => return None,
        };
        (start != line).then_some(start)
    });
    Some((line, start))
}

/// The path on `side` of a renamed file, and the other path when it differs.
fn position_paths(position: &RestPosition, side: Side) -> Option<(String, Option<String>)> {
    let (new_path, old_path) = (position.new_path.as_deref(), position.old_path.as_deref());
    let (path, other) = match side {
        Side::New => (new_path.or(old_path)?, old_path),
        Side::Old => (old_path.or(new_path)?, new_path),
    };
    Some((
        path.to_owned(),
        other.filter(|other| *other != path).map(str::to_owned),
    ))
}

/// The anchor and whether it is outdated. A position with no line (a file
/// comment, or an image pin) is anchored to the file only.
fn anchor_of(position: &RestPosition, current_head: &str) -> Option<(Anchor, bool)> {
    let Some((line, start)) = position_lines(position) else {
        let (path, _) = position_paths(position, Side::New)?;
        let anchor = Anchor {
            old_path: position.old_path.clone().filter(|old| *old != path),
            path,
            position: None,
            original: None,
        };
        return Some((anchor, false));
    };
    let found = Position {
        line,
        start,
        commit: (!position.head_sha.is_empty()).then(|| Sha::new(&position.head_sha)),
    };
    let outdated = !current_head.is_empty()
        && !position.head_sha.is_empty()
        && position.head_sha != current_head;

    let (path, old_path) = position_paths(position, line.side)?;
    let anchor = Anchor {
        old_path,
        path,
        position: (!outdated).then(|| found.clone()),
        original: outdated.then_some(found),
    };
    Some((anchor, outdated))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(body: &str, system: bool) -> RestNote {
        serde_json::from_value(json!({
            "id": 7, "body": body, "system": system,
            "created_at": "2026-11-17T04:45:48.419Z",
            "author": {"id": 1, "username": "u"},
        }))
        .unwrap()
    }

    fn context() -> ThreadContext<'static> {
        ThreadContext {
            host: "gitlab.com",
            head: "bbb",
            web_url: "https://gitlab.com/o/r/-/merge_requests/1",
        }
    }

    fn discussion(notes: Vec<RestNote>) -> RestDiscussion {
        RestDiscussion {
            id: "d1".to_owned(),
            notes,
        }
    }

    #[test]
    fn maps_pipeline_statuses() {
        let cases = [
            ("CREATED", CiState::Queued),
            ("WAITING_FOR_RESOURCE", CiState::Queued),
            ("PREPARING", CiState::Queued),
            ("PENDING", CiState::Queued),
            ("SCHEDULED", CiState::Queued),
            ("WAITING_FOR_CALLBACK", CiState::Queued),
            ("running", CiState::Running),
            ("SUCCESS", CiState::Pass),
            ("FAILED", CiState::Fail),
            ("CANCELING", CiState::Canceled),
            ("MANUAL", CiState::Skipped),
            ("SKIPPED", CiState::Skipped),
            ("CANCELED", CiState::Canceled),
            ("something_new", CiState::None),
        ];
        for (status, expected) in cases {
            assert_eq!(ci_state(status), expected, "{status}");
        }
    }

    #[test]
    fn verdict_notes_become_threads_and_other_system_notes_vanish() {
        let approved = discussion(vec![note("approved this merge request", true)]);
        let thread = thread_of(&approved, &context()).unwrap();
        assert_eq!(thread.anchor, None);
        assert_eq!(thread.resolution, Resolution::NotResolvable);
        assert_eq!(thread.comments[0].verdict, Some(ReviewState::Approved));
        assert_eq!(thread.comments[0].body, "");

        let noise = discussion(vec![note("added 1 commit", true)]);
        assert!(thread_of(&noise, &context()).is_none());
    }

    #[test]
    fn a_note_s_suggestions_keep_gitlab_s_own_range_and_text() {
        let mut note = note(
            "```suggestion
b
```",
            false,
        );
        note.suggestions = serde_json::from_value(json!([{
            "id": 5, "from_line": 3, "to_line": 4, "appliable": true, "applied": false,
            "from_content": "  a
  b
", "to_content": "b
", "lines_above": 0,
        }, {"id": 6}]))
        .unwrap();
        let comment = comment_of(&note, &context(), None);
        let first = &comment.suggestions[0];
        assert_eq!((first.from_line, first.to_line), (Some(3), Some(4)));
        assert_eq!(
            first.from_content.as_deref(),
            Some(
                "  a
  b
"
            )
        );
        assert_eq!(
            first.to_content.as_deref(),
            Some(
                "b
"
            )
        );
        assert_eq!((first.lines_above, first.lines_below), (0, 0));
        assert!(first.appliable && !first.applied);
        let bare = &comment.suggestions[1];
        assert_eq!((bare.from_line, bare.to_content.as_ref()), (None, None));
    }

    #[test]
    fn a_position_on_an_old_head_is_outdated_and_keeps_only_the_original() {
        let position: RestPosition = serde_json::from_value(json!({
            "head_sha": "aaa", "old_path": "a.rs", "new_path": "a.rs", "new_line": 24,
        }))
        .unwrap();
        let (anchor, outdated) = anchor_of(&position, "bbb").unwrap();
        assert!(outdated);
        assert_eq!(anchor.position, None);
        assert_eq!(anchor.original.unwrap().line.line, 24);

        let (anchor, outdated) = anchor_of(&position, "aaa").unwrap();
        assert!(!outdated);
        assert!(anchor.position.is_some() && anchor.original.is_none());
    }

    #[test]
    fn multi_line_ranges_and_old_side_lines() {
        let range: RestPosition = serde_json::from_value(json!({
            "head_sha": "bbb", "old_path": "a.rs", "new_path": "a.rs", "new_line": 31,
            "line_range": {"start": {"type": "new", "new_line": 29}, "end": {"type": "new", "new_line": 31}},
        }))
        .unwrap();
        let position = anchor_of(&range, "bbb").unwrap().0.position.unwrap();
        assert_eq!(position.start.unwrap().line, 29);
        assert_eq!(position.line.line, 31);

        let removed: RestPosition = serde_json::from_value(json!({
            "head_sha": "bbb", "old_path": "a.rs", "new_path": "b.rs", "old_line": 8,
        }))
        .unwrap();
        let anchor = anchor_of(&removed, "bbb").unwrap().0;
        assert_eq!(anchor.path, "a.rs");
        assert_eq!(anchor.old_path.as_deref(), Some("b.rs"));
        assert_eq!(anchor.position.unwrap().line.side, Side::Old);
    }

    #[test]
    fn a_position_without_lines_is_a_file_comment() {
        let file: RestPosition = serde_json::from_value(json!({
            "head_sha": "aaa", "old_path": "a.rs", "new_path": "a.rs", "position_type": "file",
        }))
        .unwrap();
        let (anchor, outdated) = anchor_of(&file, "bbb").unwrap();
        assert!(!outdated);
        assert!(anchor.position.is_none() && anchor.original.is_none());
    }

    #[test]
    fn stack_order_follows_the_branch_chain() {
        let link = |number, head: &str, base: &str| StackLink {
            number: PrNumber(number),
            title: String::new(),
            state: PrState::Open,
            head: head.to_owned(),
            base: base.to_owned(),
        };
        let ordered = order_stack(vec![
            link(3, "c", "b"),
            link(1, "a", "main"),
            link(2, "b", "a"),
        ]);
        let numbers: Vec<u64> = ordered.iter().map(|l| l.number.0).collect();
        assert_eq!(numbers, [1, 2, 3]);

        let broken = vec![link(2, "b", "a"), link(1, "x", "main")];
        let kept = order_stack(broken.clone());
        assert_eq!(kept, broken);
    }

    #[test]
    fn gid_and_path_helpers() {
        assert_eq!(gid_number("gid://gitlab/Ci::Build/42"), "42");
        let repo = RepoRef::new("gitlab.com", "g/sub", "r");
        assert_eq!(project_segment(&repo), "g%2Fsub%2Fr");
        assert_eq!(repo_ref("gitlab.com", "g/sub/r"), repo);
        let url = Url::parse("https://x.test/a?per_page=100&page=2").unwrap();
        assert_eq!(with_page(&url, "3").query(), Some("per_page=100&page=3"));
    }

    fn plain(id: &str, body: &str, at: &str, author: u64) -> RestDiscussion {
        let mut note = note(body, false);
        note.created_at = at.parse().unwrap();
        note.author = Some(RestUser {
            id: author,
            username: "u".to_owned(),
            name: None,
            avatar_url: None,
            bot: false,
        });
        RestDiscussion {
            id: id.to_owned(),
            notes: vec![note],
        }
    }

    fn system(id: &str, body: &str, at: &str, author: u64) -> RestDiscussion {
        let mut discussion = plain(id, body, at, author);
        discussion.notes[0].system = true;
        discussion
    }

    #[test]
    fn a_summary_takes_the_verdict_of_the_system_note_after_it() {
        let discussions = vec![
            plain("a", "Looks good.", "2026-11-17T12:00:00Z", 1),
            plain("other", "Unrelated.", "2026-11-17T12:00:01Z", 2),
            system("a-left", "left review comments", "2026-11-17T12:00:02Z", 1),
            plain("b", "Approving.", "2026-11-17T12:00:10Z", 1),
            plain("c", "Needs work.", "2026-11-17T12:00:11Z", 1),
            system("c-rc", "requested changes", "2026-11-17T12:00:12Z", 1),
            system(
                "b-ok",
                "approved this merge request",
                "2026-11-17T12:00:16Z",
                1,
            ),
            plain("late", "Much later.", "2026-11-17T12:05:00Z", 1),
            system(
                "late-ok",
                "approved this merge request",
                "2026-11-17T12:09:00Z",
                1,
            ),
        ];
        let threads = build_threads(&discussions, &context());
        let verdicts: Vec<(&str, Option<ReviewState>)> = threads
            .iter()
            .map(|t| (t.id.as_str(), t.comments[0].verdict))
            .collect();
        assert_eq!(
            verdicts,
            [
                ("a", Some(ReviewState::Commented)),
                ("other", None),
                ("b", Some(ReviewState::Approved)),
                ("c", Some(ReviewState::ChangesRequested)),
                ("late", None),
                ("late-ok", Some(ReviewState::Approved)),
            ]
        );
        assert_eq!(threads[2].comments[0].body, "Approving.");
        assert_eq!(threads[5].comments[0].body, "");
    }
}
