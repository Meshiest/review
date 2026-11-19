//! GitHub (github.com, and Enterprise Server by host). GraphQL for every read,
//! so a pull request with its threads and checks is one query; REST is kept
//! for logs and re-runs (later milestones).
//!
//! Operations (footnotes in `docs/design.md`); every one is named, because
//! recordings match on the operation name and variables:
//! - `Viewer`: `viewer{...}`.
//! - `Inbox` [G8]: `search(type: ISSUE)` with `is:pr is:open
//!   review-requested:@me`, `author:@me` or `repo:o/r`. `review-requested`,
//!   not `user-review-requested`, which drops team requests.
//! - `PullRequest`: `repository.pullRequest` with refs, `reviewDecision`,
//!   `reviewRequests`, `latestReviews`, labels, stats and the head commit's
//!   rollup state.
//! - `PullRequestThreads` [G3]: issue comments, reviews (verdicts, and
//!   comment-only reviews with a body) and `reviewThreads`, each paged to
//!   the end (`...Page` operations).
//! - `PullRequestChecks` [G5][G6]: the head commit's
//!   `statusCheckRollup.contexts` (check runs with steps, and status contexts).
//! - `PullRequestSnapshot`: the three above in one request.
//! - `PullRequestStack` [G9]: `PullRequest.stack`; without one, the shared
//!   base-branch rule over `OpenPullRequests`.
//! - `PullRequestFiles` [G4]: `files{path viewerViewedState}`.
//! - Writes: the mutations listed in `writes`.
//!
//! Connections are fetched in pages sized so that one request stays well
//! under GitHub's node limit; a connection that has more gets follow-up
//! queries until `hasNextPage` is false. `Inbox` is capped at one page of
//! [`INBOX_LIMIT`], the most GitHub's search returns per request.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::capabilities::Capabilities;
use crate::error::{Error, Result};
use crate::forge::{Forge, INBOX_LIMIT};
use crate::http::{HttpClient, Url};
use crate::model::{
    AccountKind, Anchor, BranchRef, CheckRun, CheckRunId, CheckStep, CiState, Comment, CommentId,
    DiffStat, ForgeKind, InboxSection, JobId, Label, LineRef, Milestone, Position, PrNumber,
    PrState, PullRequest, PullRequestId, RepoRef, Resolution, ReviewDecision, ReviewId,
    ReviewState, Reviewer, RunId, Sha, Side, Snapshot, Stack, StackLink, StackSource, Thread,
    ThreadId, Timestamp, User, UserId, ViewedFile, ViewedState,
};
use crate::model::{
    CiDetail, CiJob, CiRun, LogChunk, MergeOptions, MergeSettings, Mergeability, NewComment,
    PendingComment, PendingReview, PullRequestPulse, Review, Verdict,
};
use crate::people::PickerCache;
use crate::stack::stack_by_base_branch;

/// GraphQL's `first` for search is at most 100.
const _: () = assert!(INBOX_LIMIT <= 100);

/// A connection that never ends is a server bug; stop instead of looping.
const MAX_PAGES: usize = 500;

macro_rules! page_info {
    () => {
        "pageInfo{hasNextPage endCursor}"
    };
}

macro_rules! actor {
    () => {
        "__typename login avatarUrl ... on User{id name} ... on Bot{id} \
         ... on Organization{id name} ... on Mannequin{id}"
    };
}

macro_rules! pull_request_fields {
    () => {
        concat!(
            "id number title body author{",
            actor!(),
            "} state isDraft createdAt updatedAt \
             baseRefName baseRefOid headRefName headRefOid \
             headRepository{nameWithOwner} baseRepository{nameWithOwner} \
             reviewDecision \
             reviewRequests(first:50){nodes{requestedReviewer{__typename \
               ... on User{id login name avatarUrl} \
               ... on Team{id slug name avatarUrl organization{login}} \
               ... on Bot{id login avatarUrl} ... on Mannequin{id login avatarUrl}}}} \
             latestReviews(first:50){nodes{state commit{oid} author{",
            actor!(),
            "}}} \
             labels(first:100){nodes{name color description}} \n             assignees(first:50){nodes{__typename id login name avatarUrl}} milestone{title} \
             additions deletions changedFiles url viewerCanApplySuggestion \
             commentCount:comments{totalCount} \
             threadCount:reviewThreads{totalCount} \
             threadCounts:reviewThreads(first:100){nodes{isResolved comments{totalCount}}} \
             commits(last:1){nodes{commit{oid statusCheckRollup{state}}}}"
        )
    };
}

macro_rules! thread_fields {
    () => {
        concat!(
            "id isResolved isOutdated resolvedBy{__typename login avatarUrl id name} path line startLine originalLine originalStartLine diffSide startDiffSide subjectType \
             comments(first:20){",
            page_info!(),
            " nodes{id author{",
            actor!(),
            "} body createdAt lastEditedAt url originalCommit{oid} commit{oid} \
             pullRequestReview{id state}}}"
        )
    };
}

macro_rules! conversation_fields {
    () => {
        concat!(
            "comments(first:50){",
            page_info!(),
            " nodes{id author{",
            actor!(),
            "} body createdAt lastEditedAt url}} \
             reviews(first:50){",
            page_info!(),
            " nodes{id state body createdAt lastEditedAt url author{",
            actor!(),
            "}}} \
             reviewThreads(first:50){",
            page_info!(),
            " nodes{",
            thread_fields!(),
            "}}"
        )
    };
}

macro_rules! check_fields {
    () => {
        concat!(
            "commits(last:1){nodes{commit{oid statusCheckRollup{state contexts(first:50){",
            page_info!(),
            " nodes{__typename \
             ... on CheckRun{id databaseId name status conclusion detailsUrl startedAt completedAt \
               steps(first:100){",
            page_info!(),
            " nodes{number name status conclusion startedAt completedAt}} \
               checkSuite{workflowRun{databaseId url workflow{name}}}} \
             ... on StatusContext{id context state targetUrl}}}}}}}"
        )
    };
}

macro_rules! stack_fields {
    () => {
        concat!(
            "stack{id entries(first:100){",
            page_info!(),
            " nodes{position pullRequest{number title state isDraft headRefName baseRefName}}}}"
        )
    };
}

macro_rules! on_pull_request {
    ($operation:literal, $selection:expr) => {
        concat!(
            "query ",
            $operation,
            "($owner:String!,$name:String!,$number:Int!){\
             repository(owner:$owner,name:$name){pullRequest(number:$number){",
            $selection,
            "}}}"
        )
    };
}

macro_rules! on_pull_request_page {
    ($operation:literal, $connection:literal, $selection:expr) => {
        concat!(
            "query ",
            $operation,
            "($owner:String!,$name:String!,$number:Int!,$after:String){\
             repository(owner:$owner,name:$name){pullRequest(number:$number){",
            $connection,
            "(first:50,after:$after){",
            page_info!(),
            " nodes{",
            $selection,
            "}}}}}"
        )
    };
}

mod ci;
mod lifecycle;
mod people;
mod writes;

const VIEWER: &str = "query Viewer{viewer{__typename login name avatarUrl id}}";

const INBOX: &str = concat!(
    "query Inbox($query:String!,$first:Int!){search(type:ISSUE,query:$query,first:$first){nodes{\
     ... on PullRequest{",
    pull_request_fields!(),
    "}}}}"
);

const PULL_REQUEST: &str = on_pull_request!("PullRequest", pull_request_fields!());

const THREADS: &str = on_pull_request!("PullRequestThreads", conversation_fields!());

const CHECKS: &str = on_pull_request!("PullRequestChecks", check_fields!());

const SNAPSHOT: &str = on_pull_request!(
    "PullRequestSnapshot",
    concat!(
        pull_request_fields!(),
        " ",
        conversation_fields!(),
        " ",
        check_fields!()
    )
);

const STACK: &str = on_pull_request!(
    "PullRequestStack",
    concat!(pull_request_fields!(), " ", stack_fields!())
);

const FILES: &str = on_pull_request_page!("PullRequestFiles", "files", "path viewerViewedState");

const OPEN_PULL_REQUESTS: &str = concat!(
    "query OpenPullRequests($owner:String!,$name:String!,$after:String){\
     repository(owner:$owner,name:$name){pullRequests(states:OPEN,first:50,after:$after,\
     orderBy:{field:CREATED_AT,direction:ASC}){",
    page_info!(),
    " nodes{",
    pull_request_fields!(),
    "}}}}"
);

const ISSUE_COMMENTS_PAGE: &str = on_pull_request_page!(
    "IssueCommentsPage",
    "comments",
    concat!("id author{", actor!(), "} body createdAt lastEditedAt url")
);

const REVIEWS_PAGE: &str = on_pull_request_page!(
    "ReviewsPage",
    "reviews",
    concat!(
        "id state body createdAt lastEditedAt url author{",
        actor!(),
        "}"
    )
);

const THREADS_PAGE: &str =
    on_pull_request_page!("ReviewThreadsPage", "reviewThreads", thread_fields!());

const CHECKS_PAGE: &str = concat!(
    "query ChecksPage($owner:String!,$name:String!,$number:Int!,$after:String){\
     repository(owner:$owner,name:$name){pullRequest(number:$number){\
     commits(last:1){nodes{commit{statusCheckRollup{contexts(first:50,after:$after){",
    page_info!(),
    " nodes{__typename \
     ... on CheckRun{id databaseId name status conclusion detailsUrl startedAt completedAt \
       steps(first:100){",
    page_info!(),
    " nodes{number name status conclusion startedAt completedAt}} \
       checkSuite{workflowRun{databaseId url workflow{name}}}} \
     ... on StatusContext{id context state targetUrl}}}}}}}}}"
);

const THREAD_COMMENTS_PAGE: &str = concat!(
    "query ThreadCommentsPage($id:ID!,$after:String){node(id:$id){\
     ... on PullRequestReviewThread{comments(first:100,after:$after){",
    page_info!(),
    " nodes{id author{",
    actor!(),
    "} body createdAt lastEditedAt url originalCommit{oid} commit{oid} \
     pullRequestReview{id state}}}}}}"
);

const STEPS_PAGE: &str = concat!(
    "query CheckStepsPage($id:ID!,$after:String){node(id:$id){\
     ... on CheckRun{steps(first:100,after:$after){",
    page_info!(),
    " nodes{number name status conclusion startedAt completedAt}}}}}"
);

const STACK_ENTRIES_PAGE: &str = concat!(
    "query StackEntriesPage($id:ID!,$after:String){node(id:$id){\
     ... on PullRequestStack{entries(first:100,after:$after){",
    page_info!(),
    " nodes{position pullRequest{number title state isDraft headRefName baseRefName}}}}}}"
);

/// The search text of an inbox section: always pull requests, newest update
/// first. Only [`InboxSection::Query`] may list closed ones.
#[must_use]
pub fn inbox_query(section: &InboxSection) -> String {
    let terms = match section {
        InboxSection::ReviewRequested => "is:open review-requested:@me".to_owned(),
        InboxSection::AuthoredByMe => "is:open author:@me".to_owned(),
        InboxSection::OpenInRepo(repo) => format!("is:open repo:{}", repo.path()),
        InboxSection::Query(query) => query.trim().to_owned(),
    };
    if terms.contains("sort:") {
        format!("is:pr {terms}")
    } else {
        format!("is:pr {terms} sort:updated-desc")
    }
}

pub struct GitHub {
    http: HttpClient,
    graphql: Url,
    /// REST root for Actions re-runs [G7].
    rest: Url,
    /// Pull request node ids, which mutations address.
    node_ids: Mutex<HashMap<(RepoRef, PrNumber), PullRequestId>>,
    /// The collaborator and label lists behind the pickers.
    picker: PickerCache,
}

impl GitHub {
    /// # Errors
    /// [`Error::Config`] when the host does not make an API URL.
    pub async fn connect(http: HttpClient) -> Result<Self> {
        let host = http.host().to_owned();
        let (graphql, rest) = if host == "github.com" {
            (
                "https://api.github.com/graphql".to_owned(),
                "https://api.github.com/".to_owned(),
            )
        } else {
            (
                format!("https://{host}/api/graphql"),
                format!("https://{host}/api/v3/"),
            )
        };
        let parse = |url: &str| {
            Url::parse(url).map_err(|err| Error::Config {
                host: host.clone(),
                reason: err.to_string(),
            })
        };
        Ok(Self {
            graphql: parse(&graphql)?,
            rest: parse(&rest)?,
            http,
            node_ids: Mutex::default(),
            picker: PickerCache::default(),
        })
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

    /// Every node of a connection: `initial` is the first page when the
    /// caller already has it, otherwise the first request is made here.
    /// `query` must take `$after` (and whatever `variables` carries).
    async fn paginate<T, E>(
        &self,
        initial: Option<Connection<T>>,
        operation: &str,
        query: &str,
        mut variables: Value,
        pick: impl Fn(E) -> Option<Connection<T>>,
    ) -> Result<Vec<T>>
    where
        E: DeserializeOwned,
    {
        let mut items = Vec::new();
        let mut page = initial;
        let mut cursor = Value::Null;
        for _ in 0..MAX_PAGES {
            let connection = match page.take() {
                Some(connection) => connection,
                None => {
                    variables["after"] = cursor.clone();
                    let envelope: E = self.query(operation, query, variables.clone()).await?;
                    pick(envelope).ok_or_else(|| Error::NotFound {
                        request: operation.to_owned(),
                    })?
                }
            };
            let (nodes, page_info) = connection.into_parts();
            items.extend(nodes);
            if !page_info.has_next_page {
                return Ok(items);
            }
            let Some(next) = page_info.end_cursor else {
                return Err(Error::Decode {
                    request: operation.to_owned(),
                    reason: "hasNextPage without an endCursor".to_owned(),
                });
            };
            cursor = Value::String(next);
        }
        Err(Error::Decode {
            request: operation.to_owned(),
            reason: format!("more than {MAX_PAGES} pages"),
        })
    }

    /// `paginate` for a connection under `repository.pullRequest`.
    async fn paginate_pull_request<T>(
        &self,
        initial: Option<Connection<T>>,
        operation: &str,
        query: &str,
        repo: &RepoRef,
        number: PrNumber,
        pick: impl Fn(Sections) -> Option<Connection<T>>,
    ) -> Result<Vec<T>> {
        self.paginate(
            initial,
            operation,
            query,
            variables(repo, number),
            |envelope: RepoEnvelope<Sections>| pick(envelope.pull_request()?),
        )
        .await
    }

    fn repo_ref(&self, name_with_owner: &str) -> Option<RepoRef> {
        let (owner, name) = name_with_owner.split_once('/')?;
        Some(RepoRef::new(self.http.host(), owner, name))
    }

    fn pull_request_from(
        &self,
        node: PullRequestNode,
        fallback: Option<&RepoRef>,
    ) -> Result<PullRequest> {
        let repo = node
            .base_repository
            .as_ref()
            .and_then(|repo| self.repo_ref(&repo.name_with_owner))
            .or_else(|| fallback.cloned())
            .ok_or_else(|| Error::Decode {
                request: format!("PullRequest {}", node.number),
                reason: "no base repository".to_owned(),
            })?;
        let head_repo = node
            .head_repository
            .as_ref()
            .and_then(|repo| self.repo_ref(&repo.name_with_owner));
        let ci = node
            .commits
            .into_parts()
            .0
            .pop()
            .and_then(|commit| commit.commit.status_check_rollup)
            .and_then(|rollup| rollup.state)
            .map_or(CiState::None, rollup_state);
        let reviewers = reviewers(
            node.review_requests.items(),
            node.latest_reviews.items(),
            &node.head_ref_oid,
        );
        let decision = review_decision(node.review_decision, &reviewers);
        let state = pr_state(node.state, node.is_draft);
        let (comment_count, unresolved_threads) =
            comment_counts(node.comment_count, node.thread_count, node.thread_counts);
        Ok(PullRequest {
            id: PullRequestId::new(node.id),
            number: PrNumber(node.number),
            title: node.title,
            body: node.body,
            author: user(node.author),
            state,
            base: BranchRef {
                repo: Some(repo.clone()),
                name: node.base_ref_name,
                sha: Sha::new(node.base_ref_oid),
            },
            head: BranchRef {
                repo: head_repo,
                name: node.head_ref_name,
                sha: Sha::new(node.head_ref_oid),
            },
            repo,
            merge_base: None,
            created_at: node.created_at,
            updated_at: node.updated_at,
            review_decision: decision,
            reviewers,
            ci: Some(ci),
            labels: node.labels.items().into_iter().map(Label::from).collect(),
            assignees: node
                .assignees
                .into_parts()
                .0
                .into_iter()
                .map(|actor| user(Some(actor)))
                .collect(),
            milestone: node.milestone.map(|milestone| Milestone {
                title: milestone.title,
            }),
            diff_stat: Some(DiffStat {
                additions: node.additions,
                deletions: node.deletions,
                changed_files: node.changed_files,
            }),
            comment_count,
            unresolved_threads,
            web_url: node.url,
            head_writable: node.viewer_can_apply_suggestion,
        })
    }

    async fn threads_from(
        &self,
        sections: Sections,
        repo: &RepoRef,
        number: PrNumber,
    ) -> Result<Vec<Thread>> {
        let comments = self
            .paginate_pull_request(
                sections.comments,
                "IssueCommentsPage",
                ISSUE_COMMENTS_PAGE,
                repo,
                number,
                |sections| sections.comments,
            )
            .await?;
        let reviews = self
            .paginate_pull_request(
                sections.reviews,
                "ReviewsPage",
                REVIEWS_PAGE,
                repo,
                number,
                |sections| sections.reviews,
            )
            .await?;
        let mut threads = self
            .paginate_pull_request(
                sections.review_threads,
                "ReviewThreadsPage",
                THREADS_PAGE,
                repo,
                number,
                |sections| sections.review_threads,
            )
            .await?;
        for thread in &mut threads {
            self.complete_thread(thread).await?;
        }
        Ok(map_threads(comments, reviews, threads))
    }

    /// Moves the thread's first comment page into `comment_nodes` and fetches
    /// the rest.
    async fn complete_thread(&self, thread: &mut ThreadNode) -> Result<()> {
        let first = std::mem::take(&mut thread.comments);
        thread.comment_nodes = self
            .paginate(
                Some(first),
                "ThreadCommentsPage",
                THREAD_COMMENTS_PAGE,
                json!({"id": thread.id}),
                |envelope: NodeEnvelope| envelope.node?.comments,
            )
            .await?;
        Ok(())
    }

    async fn checks_from(
        &self,
        sections: Sections,
        repo: &RepoRef,
        number: PrNumber,
    ) -> Result<Vec<CheckRun>> {
        let Some(rollup) = sections
            .commits
            .and_then(|commits| commits.into_parts().0.pop())
            .and_then(|commit| commit.commit.status_check_rollup)
        else {
            return Ok(Vec::new());
        };
        let mut contexts = self
            .paginate_pull_request(
                rollup.contexts,
                "ChecksPage",
                CHECKS_PAGE,
                repo,
                number,
                |sections| {
                    sections
                        .commits?
                        .into_parts()
                        .0
                        .pop()?
                        .commit
                        .status_check_rollup?
                        .contexts
                },
            )
            .await?;
        for context in &mut contexts {
            let Context::CheckRun(run) = context else {
                continue;
            };
            let Some(first) = run.steps.take() else {
                continue;
            };
            run.step_nodes = self
                .paginate(
                    Some(first),
                    "CheckStepsPage",
                    STEPS_PAGE,
                    json!({"id": run.id}),
                    |envelope: NodeEnvelope| envelope.node?.steps,
                )
                .await?;
        }
        Ok(map_checks(contexts))
    }

    fn open_pull_requests_variables(repo: &RepoRef) -> Value {
        json!({"owner": repo.owner, "name": repo.name})
    }
}

#[async_trait]
impl Forge for GitHub {
    fn kind(&self) -> ForgeKind {
        ForgeKind::GitHub
    }

    fn host(&self) -> &str {
        self.http.host()
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::GITHUB
    }

    async fn whoami(&self) -> Result<User> {
        let envelope: ViewerEnvelope = self.query("Viewer", VIEWER, json!({})).await?;
        Ok(user(Some(envelope.viewer)))
    }

    async fn inbox(&self, section: &InboxSection) -> Result<Vec<PullRequest>> {
        let envelope: SearchEnvelope = self
            .query(
                "Inbox",
                INBOX,
                json!({"query": inbox_query(section), "first": INBOX_LIMIT}),
            )
            .await?;
        let fallback = match section {
            InboxSection::OpenInRepo(repo) => Some(repo),
            _ => None,
        };
        envelope
            .search
            .items()
            .into_iter()
            .take(INBOX_LIMIT)
            .map(|node| self.pull_request_from(node, fallback))
            .collect()
    }

    async fn pull_request(&self, repo: &RepoRef, number: PrNumber) -> Result<PullRequest> {
        let envelope: RepoEnvelope<PullRequestNode> = self
            .query("PullRequest", PULL_REQUEST, variables(repo, number))
            .await?;
        let node = envelope.into_pull_request(repo, number)?;
        self.pull_request_from(node, Some(repo))
    }

    async fn threads(&self, repo: &RepoRef, number: PrNumber) -> Result<Vec<Thread>> {
        let envelope: RepoEnvelope<Sections> = self
            .query("PullRequestThreads", THREADS, variables(repo, number))
            .await?;
        let sections = envelope.into_pull_request(repo, number)?;
        self.threads_from(sections, repo, number).await
    }

    async fn checks(&self, repo: &RepoRef, number: PrNumber) -> Result<Vec<CheckRun>> {
        let envelope: RepoEnvelope<Sections> = self
            .query("PullRequestChecks", CHECKS, variables(repo, number))
            .await?;
        let sections = envelope.into_pull_request(repo, number)?;
        self.checks_from(sections, repo, number).await
    }

    async fn stack(&self, repo: &RepoRef, number: PrNumber) -> Result<Stack> {
        let operation = "PullRequestStack";
        let value: Value = self
            .query(operation, STACK, variables(repo, number))
            .await?;
        let node: RepoEnvelope<PullRequestNode> = decode(&value, operation)?;
        let sections: RepoEnvelope<Sections> = decode(&value, operation)?;
        let target = self.pull_request_from(node.into_pull_request(repo, number)?, Some(repo))?;
        if let Some(native) = sections.into_pull_request(repo, number)?.stack {
            let entries = self
                .paginate(
                    Some(native.entries),
                    "StackEntriesPage",
                    STACK_ENTRIES_PAGE,
                    json!({"id": native.id}),
                    |envelope: NodeEnvelope| envelope.node?.entries,
                )
                .await?;
            let mut entries = entries;
            entries.sort_by_key(|entry| entry.position);
            let links: Vec<StackLink> = entries
                .into_iter()
                .filter_map(|entry| entry.pull_request)
                .map(|pr| StackLink {
                    number: PrNumber(pr.number),
                    title: pr.title,
                    state: pr_state(pr.state, pr.is_draft),
                    head: pr.head_ref_name,
                    base: pr.base_ref_name,
                })
                .collect();
            if !links.is_empty() {
                return Ok(Stack {
                    source: StackSource::Native,
                    links,
                });
            }
        }
        let open = self
            .paginate(
                None,
                "OpenPullRequests",
                OPEN_PULL_REQUESTS,
                Self::open_pull_requests_variables(repo),
                |envelope: RepoEnvelope<PullRequestNode>| envelope.repository?.pull_requests,
            )
            .await?;
        let open = open
            .into_iter()
            .map(|node| self.pull_request_from(node, Some(repo)))
            .collect::<Result<Vec<_>>>()?;
        Ok(stack_by_base_branch(&target, &open))
    }

    async fn viewed_files(&self, repo: &RepoRef, number: PrNumber) -> Result<Vec<ViewedFile>> {
        let files = self
            .paginate_pull_request(None, "PullRequestFiles", FILES, repo, number, |sections| {
                sections.files
            })
            .await?;
        Ok(files
            .into_iter()
            .map(|file| ViewedFile {
                path: file.path,
                state: match file.viewer_viewed_state {
                    ViewedStateWire::Viewed => ViewedState::Viewed,
                    ViewedStateWire::Dismissed => ViewedState::Dismissed,
                    ViewedStateWire::Unviewed | ViewedStateWire::Other => ViewedState::Unviewed,
                },
            })
            .collect())
    }

    async fn snapshot(&self, repo: &RepoRef, number: PrNumber) -> Result<Snapshot> {
        let operation = "PullRequestSnapshot";
        let value: Value = self
            .query(operation, SNAPSHOT, variables(repo, number))
            .await?;
        let node: RepoEnvelope<PullRequestNode> = decode(&value, operation)?;
        let sections: RepoEnvelope<Sections> = decode(&value, operation)?;
        let pull_request =
            self.pull_request_from(node.into_pull_request(repo, number)?, Some(repo))?;
        let sections = sections.into_pull_request(repo, number)?;
        let checks = Sections {
            commits: sections.commits.clone(),
            ..Sections::default()
        };
        let threads = self.threads_from(sections, repo, number).await?;
        let checks = self.checks_from(checks, repo, number).await?;
        Ok(Snapshot {
            pull_request,
            threads,
            checks,
        })
    }

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
        _repo: &RepoRef,
        _number: PrNumber,
        review: &ReviewId,
        comment: &NewComment,
    ) -> Result<PendingComment> {
        writes::add_review_comment(self, review, comment).await
    }

    async fn delete_review_comment(
        &self,
        _repo: &RepoRef,
        _number: PrNumber,
        comment: &CommentId,
    ) -> Result<()> {
        writes::delete_review_comment(self, comment).await
    }

    async fn edit_review_comment(
        &self,
        _repo: &RepoRef,
        _number: PrNumber,
        comment: &PendingComment,
        body: &str,
    ) -> Result<PendingComment> {
        writes::edit_review_comment(self, comment, body).await
    }

    async fn submit_review(
        &self,
        _repo: &RepoRef,
        _number: PrNumber,
        review: &ReviewId,
        verdict: Verdict,
        body: &str,
    ) -> Result<Review> {
        writes::submit_review(self, review, verdict, body).await
    }

    async fn discard_review(
        &self,
        _repo: &RepoRef,
        _number: PrNumber,
        review: &ReviewId,
    ) -> Result<()> {
        writes::discard_review(self, review).await
    }

    async fn reply(
        &self,
        _repo: &RepoRef,
        _number: PrNumber,
        thread: &ThreadId,
        body: &str,
    ) -> Result<Comment> {
        writes::reply(self, thread, body).await
    }

    async fn set_resolved(
        &self,
        _repo: &RepoRef,
        _number: PrNumber,
        thread: &ThreadId,
        resolved: bool,
    ) -> Result<Thread> {
        writes::set_resolved(self, thread, resolved).await
    }

    async fn add_comment(&self, repo: &RepoRef, number: PrNumber, body: &str) -> Result<Comment> {
        writes::add_comment(self, repo, number, body).await
    }

    async fn set_viewed(
        &self,
        repo: &RepoRef,
        number: PrNumber,
        path: &str,
        viewed: bool,
    ) -> Result<ViewedFile> {
        writes::set_viewed(self, repo, number, path, viewed).await
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

    async fn set_auto_merge(
        &self,
        repo: &RepoRef,
        number: PrNumber,
        options: Option<&MergeOptions>,
    ) -> Result<PullRequest> {
        lifecycle::set_auto_merge(self, repo, number, options).await
    }

    async fn close(&self, repo: &RepoRef, number: PrNumber) -> Result<PullRequest> {
        lifecycle::close(self, repo, number).await
    }

    async fn reopen(&self, repo: &RepoRef, number: PrNumber) -> Result<PullRequest> {
        lifecycle::reopen(self, repo, number).await
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
        GitHub::ci_runs(self, repo, sha).await
    }

    async fn ci_run(&self, repo: &RepoRef, run: &RunId, detail: CiDetail) -> Result<CiRun> {
        GitHub::ci_run(self, repo, run, detail).await
    }

    async fn ci_job(&self, repo: &RepoRef, job: &JobId) -> Result<CiJob> {
        GitHub::ci_job(self, repo, job).await
    }

    async fn job_log(&self, repo: &RepoRef, job: &JobId, from: u64) -> Result<LogChunk> {
        GitHub::job_log(self, repo, job, from).await
    }

    async fn pull_request_pulse(
        &self,
        repo: &RepoRef,
        number: PrNumber,
    ) -> Result<PullRequestPulse> {
        GitHub::pull_request_pulse(self, repo, number).await
    }
}

fn variables(repo: &RepoRef, number: PrNumber) -> Value {
    json!({"owner": repo.owner, "name": repo.name, "number": number.0})
}

fn decode<T: DeserializeOwned>(value: &Value, operation: &str) -> Result<T> {
    T::deserialize(value).map_err(|err| Error::Decode {
        request: operation.to_owned(),
        reason: err.to_string(),
    })
}

// ---- wire types -----------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    #[serde(default)]
    has_next_page: bool,
    end_cursor: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Connection<T> {
    // Explicit paths: a bare `default` would require `T: Default`.
    #[serde(default = "Vec::new")]
    nodes: Vec<Option<T>>,
    #[serde(default = "PageInfo::default")]
    page_info: PageInfo,
}

impl<T> Default for Connection<T> {
    fn default() -> Self {
        Self {
            nodes: Vec::new(),
            page_info: PageInfo::default(),
        }
    }
}

impl<T> Connection<T> {
    fn into_parts(self) -> (Vec<T>, PageInfo) {
        (self.nodes.into_iter().flatten().collect(), self.page_info)
    }

    fn items(&self) -> Vec<T>
    where
        T: Clone,
    {
        self.nodes.iter().flatten().cloned().collect()
    }
}

#[derive(Debug, Deserialize)]
struct ViewerEnvelope {
    viewer: Actor,
}

#[derive(Debug, Deserialize)]
struct SearchEnvelope {
    search: Connection<PullRequestNode>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepoEnvelope<N> {
    repository: Option<RepoNode<N>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepoNode<N> {
    pull_request: Option<N>,
    pull_requests: Option<Connection<N>>,
}

impl<N> RepoEnvelope<N> {
    fn pull_request(self) -> Option<N> {
        self.repository?.pull_request
    }

    fn into_pull_request(self, repo: &RepoRef, number: PrNumber) -> Result<N> {
        self.pull_request().ok_or_else(|| Error::NotFound {
            request: format!("{repo} pull request {number}"),
        })
    }
}

#[derive(Debug, Deserialize)]
struct NodeEnvelope {
    node: Option<NodeBody>,
}

/// What a `node(id:)` continuation can hold; only the asked-for field is set.
#[derive(Debug, Deserialize)]
struct NodeBody {
    comments: Option<Connection<ReviewCommentNode>>,
    steps: Option<Connection<StepNode>>,
    entries: Option<Connection<StackEntry>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Actor {
    #[serde(rename = "__typename", default)]
    typename: String,
    login: String,
    avatar_url: Option<String>,
    id: Option<String>,
    name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RequestedReviewer {
    #[serde(rename = "__typename")]
    typename: String,
    id: Option<String>,
    login: Option<String>,
    slug: Option<String>,
    name: Option<String>,
    avatar_url: Option<String>,
    organization: Option<Organization>,
}

#[derive(Debug, Clone, Deserialize)]
struct Organization {
    login: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepoName {
    name_with_owner: String,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum PrStateWire {
    Open,
    Closed,
    Merged,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum DecisionWire {
    Approved,
    ChangesRequested,
    ReviewRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum ReviewStateWire {
    Pending,
    Commented,
    Approved,
    ChangesRequested,
    Dismissed,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PullRequestNode {
    id: String,
    number: u64,
    title: String,
    #[serde(default)]
    body: String,
    author: Option<Actor>,
    state: PrStateWire,
    is_draft: bool,
    created_at: Timestamp,
    updated_at: Timestamp,
    base_ref_name: String,
    base_ref_oid: String,
    head_ref_name: String,
    head_ref_oid: String,
    head_repository: Option<RepoName>,
    base_repository: Option<RepoName>,
    review_decision: Option<DecisionWire>,
    review_requests: Connection<ReviewRequest>,
    latest_reviews: Connection<LatestReview>,
    labels: Connection<LabelNode>,
    #[serde(default)]
    assignees: Connection<Actor>,
    #[serde(default)]
    milestone: Option<MilestoneNode>,
    additions: u32,
    deletions: u32,
    changed_files: u32,
    url: String,
    #[serde(default)]
    viewer_can_apply_suggestion: bool,
    #[serde(default)]
    comment_count: Option<TotalCount>,
    #[serde(default)]
    thread_count: Option<TotalCount>,
    #[serde(default)]
    thread_counts: Option<Connection<ThreadCount>>,
    commits: Connection<HeadCommit>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TotalCount {
    total_count: u32,
}

/// One review thread's resolution and size; enough to count without paging.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadCount {
    is_resolved: bool,
    comments: TotalCount,
}

/// Conversation plus review comments, and unresolved threads. Threads past
/// the first page count one comment each, so the total is then a lower bound
/// and the unresolved count unknown. `None` when the query did not select
/// the counts (an older recording).
fn comment_counts(
    comments: Option<TotalCount>,
    threads: Option<TotalCount>,
    pages: Option<Connection<ThreadCount>>,
) -> (Option<u32>, Option<u32>) {
    let (Some(comments), Some(threads), Some(pages)) = (comments, threads, pages) else {
        return (None, None);
    };
    let nodes = pages.into_parts().0;
    let listed = u32::try_from(nodes.len()).unwrap_or(u32::MAX);
    let review: u32 = nodes.iter().map(|thread| thread.comments.total_count).sum();
    let unresolved = nodes.iter().filter(|thread| !thread.is_resolved).count();
    let missing = threads.total_count.saturating_sub(listed);
    let unresolved = (missing == 0).then(|| u32::try_from(unresolved).unwrap_or(u32::MAX));
    (Some(comments.total_count + review + missing), unresolved)
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewRequest {
    requested_reviewer: Option<RequestedReviewer>,
}

#[derive(Debug, Clone, Deserialize)]
struct LatestReview {
    state: ReviewStateWire,
    author: Option<Actor>,
    /// The head commit the review was written against.
    #[serde(default)]
    commit: Option<ReviewCommit>,
}

#[derive(Debug, Clone, Deserialize)]
struct ReviewCommit {
    oid: String,
}

#[derive(Debug, Clone, Deserialize)]
struct LabelNode {
    name: String,
    #[serde(default)]
    color: String,
    #[serde(default)]
    description: Option<String>,
}

impl From<LabelNode> for Label {
    fn from(label: LabelNode) -> Self {
        Self {
            name: label.name,
            color: Some(label.color).filter(|color| !color.is_empty()),
            description: label.description.filter(|text| !text.is_empty()),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct MilestoneNode {
    title: String,
}

#[derive(Debug, Clone, Deserialize)]
struct HeadCommit {
    commit: CommitNode,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommitNode {
    status_check_rollup: Option<Rollup>,
}

#[derive(Debug, Clone, Deserialize)]
struct Rollup {
    state: Option<RollupWire>,
    contexts: Option<Connection<Context>>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum RollupWire {
    Success,
    Failure,
    Error,
    Pending,
    Expected,
}

/// The parts of a pull request that are paged; a query selects some of them.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Sections {
    comments: Option<Connection<IssueCommentNode>>,
    reviews: Option<Connection<ReviewNode>>,
    review_threads: Option<Connection<ThreadNode>>,
    files: Option<Connection<FileNode>>,
    stack: Option<StackNode>,
    commits: Option<Connection<CheckCommit>>,
}

#[derive(Debug, Clone, Deserialize)]
struct CheckCommit {
    commit: CommitNode,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IssueCommentNode {
    id: String,
    author: Option<Actor>,
    body: String,
    created_at: Timestamp,
    last_edited_at: Option<Timestamp>,
    url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewNode {
    id: String,
    state: ReviewStateWire,
    #[serde(default)]
    body: String,
    author: Option<Actor>,
    created_at: Timestamp,
    last_edited_at: Option<Timestamp>,
    url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum DiffSide {
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum SubjectType {
    Line,
    File,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadNode {
    id: String,
    is_resolved: bool,
    is_outdated: bool,
    resolved_by: Option<Actor>,
    path: String,
    line: Option<u32>,
    start_line: Option<u32>,
    original_line: Option<u32>,
    original_start_line: Option<u32>,
    diff_side: Option<DiffSide>,
    start_diff_side: Option<DiffSide>,
    subject_type: Option<SubjectType>,
    /// The first page; replaced by every page in [`Self::comment_nodes`].
    #[serde(default)]
    comments: Connection<ReviewCommentNode>,
    #[serde(skip)]
    comment_nodes: Vec<ReviewCommentNode>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewCommentNode {
    id: String,
    author: Option<Actor>,
    body: String,
    created_at: Timestamp,
    last_edited_at: Option<Timestamp>,
    url: Option<String>,
    original_commit: Option<Oid>,
    commit: Option<Oid>,
    pull_request_review: Option<ReviewRef>,
}

#[derive(Debug, Clone, Deserialize)]
struct Oid {
    oid: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ReviewRef {
    id: String,
    state: ReviewStateWire,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "__typename")]
enum Context {
    CheckRun(Box<RunNode>),
    StatusContext(StatusNode),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunNode {
    id: String,
    database_id: Option<serde_json::Number>,
    name: String,
    status: Option<RunStatus>,
    conclusion: Option<Conclusion>,
    details_url: Option<String>,
    started_at: Option<Timestamp>,
    completed_at: Option<Timestamp>,
    #[serde(default)]
    steps: Option<Connection<StepNode>>,
    #[serde(skip)]
    step_nodes: Vec<StepNode>,
    check_suite: Option<CheckSuite>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CheckSuite {
    workflow_run: Option<WorkflowRun>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowRun {
    database_id: Option<serde_json::Number>,
    workflow: Option<Workflow>,
}

#[derive(Debug, Clone, Deserialize)]
struct Workflow {
    name: String,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum RunStatus {
    Queued,
    InProgress,
    Completed,
    Waiting,
    Pending,
    Requested,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum Conclusion {
    ActionRequired,
    Cancelled,
    Failure,
    Neutral,
    Skipped,
    Stale,
    StartupFailure,
    Success,
    TimedOut,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StepNode {
    number: u32,
    name: String,
    status: Option<RunStatus>,
    conclusion: Option<Conclusion>,
    started_at: Option<Timestamp>,
    completed_at: Option<Timestamp>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StatusNode {
    id: String,
    context: String,
    state: StatusState,
    target_url: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum StatusState {
    Expected,
    Error,
    Failure,
    Pending,
    Success,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum ViewedStateWire {
    Viewed,
    Unviewed,
    Dismissed,
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileNode {
    path: String,
    viewer_viewed_state: ViewedStateWire,
}

#[derive(Debug, Clone, Deserialize)]
struct StackNode {
    id: String,
    entries: Connection<StackEntry>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StackEntry {
    position: u32,
    pull_request: Option<StackPullRequest>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StackPullRequest {
    number: u64,
    title: String,
    state: PrStateWire,
    is_draft: bool,
    head_ref_name: String,
    base_ref_name: String,
}

// ---- mapping --------------------------------------------------------------

/// A deleted account comes back as `null`; GitHub shows it as `ghost`.
fn user(actor: Option<Actor>) -> User {
    let Some(actor) = actor else {
        return User::new("ghost");
    };
    User {
        id: actor.id.map(UserId::new),
        kind: if actor.typename == "Bot" {
            AccountKind::Bot
        } else {
            AccountKind::User
        },
        login: actor.login,
        name: actor.name.filter(|name| !name.is_empty()),
        avatar_url: actor.avatar_url,
    }
}

fn requested_user(reviewer: RequestedReviewer) -> Option<User> {
    if reviewer.typename == "Team" {
        let slug = reviewer.slug?;
        let login = match reviewer.organization {
            Some(org) => format!("{}/{slug}", org.login),
            None => slug,
        };
        return Some(User {
            id: reviewer.id.map(UserId::new),
            login,
            name: reviewer.name,
            avatar_url: reviewer.avatar_url,
            kind: AccountKind::Team,
        });
    }
    Some(User {
        id: reviewer.id.map(UserId::new),
        login: reviewer.login?,
        name: reviewer.name.filter(|name| !name.is_empty()),
        avatar_url: reviewer.avatar_url,
        kind: if reviewer.typename == "Bot" {
            AccountKind::Bot
        } else {
            AccountKind::User
        },
    })
}

fn pr_state(state: PrStateWire, is_draft: bool) -> PrState {
    match state {
        PrStateWire::Merged => PrState::Merged,
        PrStateWire::Closed => PrState::Closed,
        PrStateWire::Open if is_draft => PrState::Draft,
        PrStateWire::Open => PrState::Open,
    }
}

fn review_state(state: ReviewStateWire) -> ReviewState {
    match state {
        ReviewStateWire::Pending => ReviewState::Pending,
        ReviewStateWire::Commented => ReviewState::Commented,
        ReviewStateWire::Approved => ReviewState::Approved,
        ReviewStateWire::ChangesRequested => ReviewState::ChangesRequested,
        ReviewStateWire::Dismissed => ReviewState::Dismissed,
    }
}

/// GitHub's `reviewDecision` answers the repository's required-review
/// rule and stays "approved" after a push or a re-request, so an approval
/// that is stale or withdrawn is taken out of it here. It is null without
/// such a rule, and the reviewers decide alone.
fn review_decision(decision: Option<DecisionWire>, reviewers: &[Reviewer]) -> ReviewDecision {
    let by_reviewers = ReviewDecision::from_reviewers(reviewers);
    match decision {
        Some(DecisionWire::ChangesRequested) => ReviewDecision::ChangesRequested,
        Some(DecisionWire::ReviewRequired) => ReviewDecision::ReviewRequired,
        Some(DecisionWire::Approved) => match by_reviewers {
            ReviewDecision::StaleApproval => ReviewDecision::StaleApproval,
            ReviewDecision::ReviewRequired if reviewers.iter().any(|r| r.requested_again) => {
                ReviewDecision::ReviewRequired
            }
            _ => ReviewDecision::Approved,
        },
        None => by_reviewers,
    }
}

/// One row per person. A pending request wins over an older review (the
/// author asked again) and keeps the fact that there was one. An approval of
/// a commit other than `head` is stale.
fn reviewers(
    requests: Vec<ReviewRequest>,
    reviews: Vec<LatestReview>,
    head: &str,
) -> Vec<Reviewer> {
    let mut rows: Vec<Reviewer> = Vec::new();
    for review in reviews {
        if review.state == ReviewStateWire::Pending {
            continue;
        }
        let state = review_state(review.state);
        let stale = state == ReviewState::Approved
            && review
                .commit
                .as_ref()
                .is_some_and(|commit| commit.oid != head);
        let user = user(review.author);
        let row = Reviewer {
            stale,
            ..Reviewer::new(user.clone(), state)
        };
        match rows
            .iter_mut()
            .find(|existing| existing.user.login == user.login)
        {
            Some(existing) => *existing = row,
            None => rows.push(row),
        }
    }
    for request in requests {
        let Some(user) = request.requested_reviewer.and_then(requested_user) else {
            continue;
        };
        match rows.iter_mut().find(|row| row.user.login == user.login) {
            Some(row) => {
                row.requested_again = row.state != ReviewState::Requested;
                row.state = ReviewState::Requested;
                row.stale = false;
            }
            None => rows.push(Reviewer::new(user, ReviewState::Requested)),
        }
    }
    rows
}

fn rollup_state(state: RollupWire) -> CiState {
    match state {
        RollupWire::Success => CiState::Pass,
        RollupWire::Failure | RollupWire::Error => CiState::Fail,
        RollupWire::Pending | RollupWire::Expected => CiState::Running,
    }
}

fn side(side: Option<DiffSide>) -> Side {
    match side {
        Some(DiffSide::Left) => Side::Old,
        Some(DiffSide::Right) | None => Side::New,
    }
}

/// The first line of a multi-line range; `None` for a single line, which
/// GitHub reports with `startLine == line`.
fn range_start(
    start: Option<u32>,
    start_side: Side,
    line: u32,
    line_side: Side,
) -> Option<LineRef> {
    start
        .filter(|start| *start != line || start_side != line_side)
        .map(|start| LineRef {
            side: start_side,
            line: start,
        })
}

fn anchor(thread: &ThreadNode, first: &ReviewCommentNode) -> Anchor {
    let line_side = side(thread.diff_side);
    let start_side = thread.start_diff_side.map_or(line_side, |s| side(Some(s)));
    let position = thread.line.map(|line| Position {
        line: LineRef {
            side: line_side,
            line,
        },
        start: range_start(thread.start_line, start_side, line, line_side),
        commit: first.commit.as_ref().map(|c| Sha::new(&c.oid)),
    });
    let original = thread
        .original_line
        .map(|line| Position {
            line: LineRef {
                side: line_side,
                line,
            },
            start: range_start(thread.original_start_line, start_side, line, line_side),
            commit: first.original_commit.as_ref().map(|c| Sha::new(&c.oid)),
        })
        .filter(|original| {
            position
                .as_ref()
                .is_none_or(|p| p.line != original.line || p.start != original.start)
        });
    Anchor {
        path: thread.path.clone(),
        old_path: None,
        position,
        original,
    }
}

fn comment(node: ReviewCommentNode) -> Comment {
    Comment {
        id: CommentId::new(node.id),
        author: user(node.author),
        body: node.body,
        created_at: node.created_at,
        edited_at: node.last_edited_at,
        web_url: node.url,
        review: node.pull_request_review.map(|r| ReviewId::new(r.id)),
        verdict: None,
        suggestions: Vec::new(),
    }
}

/// Inline threads, issue comments and review summaries, oldest first. The
/// viewer's pending review is left out.
fn map_threads(
    comments: Vec<IssueCommentNode>,
    reviews: Vec<ReviewNode>,
    threads: Vec<ThreadNode>,
) -> Vec<Thread> {
    let mut out: Vec<Thread> = Vec::new();
    for thread in threads {
        let first = thread.comment_nodes.first().cloned();
        let Some(first) = first else { continue };
        let anchor = anchor(&thread, &first);
        let comments: Vec<Comment> = thread
            .comment_nodes
            .into_iter()
            .filter(|c| {
                c.pull_request_review
                    .as_ref()
                    .is_none_or(|r| r.state != ReviewStateWire::Pending)
            })
            .map(comment)
            .collect();
        if comments.is_empty() {
            continue;
        }
        out.push(Thread {
            id: ThreadId::new(thread.id),
            anchor: Some(anchor),
            resolution: if thread.is_resolved {
                Resolution::Resolved {
                    by: thread.resolved_by.map(|actor| user(Some(actor))),
                }
            } else {
                Resolution::Unresolved
            },
            outdated: thread.is_outdated,
            comments,
        });
    }
    for node in comments {
        out.push(conversation_thread(
            &node.id,
            Comment {
                id: CommentId::new(&node.id),
                author: user(node.author),
                body: node.body,
                created_at: node.created_at,
                edited_at: node.last_edited_at,
                web_url: node.url,
                review: None,
                verdict: None,
                suggestions: Vec::new(),
            },
        ));
    }
    for node in reviews {
        // A verdict is a timeline row even without text; an empty COMMENTED
        // review only wraps inline comments, which their threads already show.
        let keep = match node.state {
            ReviewStateWire::Pending => false,
            ReviewStateWire::Commented => !node.body.trim().is_empty(),
            ReviewStateWire::Approved
            | ReviewStateWire::ChangesRequested
            | ReviewStateWire::Dismissed => true,
        };
        if !keep {
            continue;
        }
        out.push(conversation_thread(
            &node.id,
            Comment {
                id: CommentId::new(&node.id),
                author: user(node.author),
                body: node.body,
                created_at: node.created_at,
                edited_at: node.last_edited_at,
                web_url: node.url,
                review: Some(ReviewId::new(&node.id)),
                verdict: Some(review_state(node.state)),
                suggestions: Vec::new(),
            },
        ));
    }
    out.sort_by_key(|thread| thread.comments[0].created_at);
    out
}

fn conversation_thread(id: &str, comment: Comment) -> Thread {
    Thread {
        id: ThreadId::new(id),
        anchor: None,
        resolution: Resolution::NotResolvable,
        outdated: false,
        comments: vec![comment],
    }
}

fn run_state(status: Option<RunStatus>, conclusion: Option<Conclusion>) -> CiState {
    match status {
        Some(RunStatus::InProgress) => CiState::Running,
        Some(RunStatus::Completed) => match conclusion {
            Some(Conclusion::Success | Conclusion::Neutral) => CiState::Pass,
            Some(
                Conclusion::Failure
                | Conclusion::TimedOut
                | Conclusion::ActionRequired
                | Conclusion::StartupFailure,
            ) => CiState::Fail,
            Some(Conclusion::Cancelled) => CiState::Canceled,
            Some(Conclusion::Skipped | Conclusion::Stale) => CiState::Skipped,
            None => CiState::None,
        },
        Some(
            RunStatus::Queued | RunStatus::Waiting | RunStatus::Pending | RunStatus::Requested,
        )
        | None => CiState::Queued,
    }
}

fn status_state(state: StatusState) -> CiState {
    match state {
        StatusState::Success => CiState::Pass,
        StatusState::Failure | StatusState::Error => CiState::Fail,
        StatusState::Pending | StatusState::Expected => CiState::Queued,
    }
}

/// Check runs and status contexts as rows. When the same check name shows up
/// twice in one workflow (a re-run), the newest attempt wins.
fn map_checks(contexts: Vec<Context>) -> Vec<CheckRun> {
    let mut rows: Vec<(u128, CheckRun)> = Vec::new();
    let mut index: HashMap<(Option<String>, String), usize> = HashMap::new();
    for context in contexts {
        let (rank, row) = match context {
            Context::CheckRun(run) => {
                let run = *run;
                let workflow = run.check_suite.and_then(|suite| suite.workflow_run);
                let rank = run
                    .database_id
                    .as_ref()
                    .and_then(serde_json::Number::as_u64)
                    .map_or(0, u128::from);
                let job = workflow
                    .as_ref()
                    .and(run.database_id.as_ref())
                    .map(|id| JobId::new(id.to_string()));
                let steps = run
                    .step_nodes
                    .into_iter()
                    .map(|step| CheckStep {
                        number: step.number,
                        name: step.name,
                        state: run_state(step.status, step.conclusion),
                        started_at: step.started_at,
                        completed_at: step.completed_at,
                    })
                    .collect();
                (
                    rank,
                    CheckRun {
                        id: CheckRunId::new(run.id),
                        name: run.name,
                        group: workflow
                            .as_ref()
                            .and_then(|w| w.workflow.as_ref())
                            .map(|w| w.name.clone()),
                        state: run_state(run.status, run.conclusion),
                        url: run.details_url,
                        job,
                        run: workflow
                            .and_then(|w| w.database_id)
                            .map(|id| RunId::new(id.to_string())),
                        started_at: run.started_at,
                        completed_at: run.completed_at,
                        steps,
                    },
                )
            }
            Context::StatusContext(status) => (
                0,
                CheckRun {
                    id: CheckRunId::new(status.id),
                    name: status.context,
                    group: None,
                    state: status_state(status.state),
                    url: status.target_url,
                    job: None,
                    run: None,
                    started_at: None,
                    completed_at: None,
                    steps: Vec::new(),
                },
            ),
        };
        let key = (row.group.clone(), row.name.clone());
        match index.get(&key) {
            Some(&at) => {
                if rank >= rows[at].0 {
                    rows[at] = (rank, row);
                }
            }
            None => {
                index.insert(key, rows.len());
                rows.push((rank, row));
            }
        }
    }
    rows.into_iter().map(|(_, row)| row).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inbox_queries() {
        let repo = RepoRef::new("github.com", "o", "r");
        let cases = [
            (
                InboxSection::ReviewRequested,
                "is:pr is:open review-requested:@me sort:updated-desc",
            ),
            (
                InboxSection::AuthoredByMe,
                "is:pr is:open author:@me sort:updated-desc",
            ),
            (
                InboxSection::OpenInRepo(repo),
                "is:pr is:open repo:o/r sort:updated-desc",
            ),
            (
                InboxSection::Query("label:bug sort:created-asc".to_owned()),
                "is:pr label:bug sort:created-asc",
            ),
            (
                InboxSection::Query("is:closed label:bug".to_owned()),
                "is:pr is:closed label:bug sort:updated-desc",
            ),
        ];
        for (section, query) in cases {
            assert_eq!(inbox_query(&section), query);
        }
    }

    #[test]
    fn a_missing_decision_falls_back_to_the_reviewers() {
        let row = |state| Reviewer::new(User::new("a"), state);
        assert_eq!(review_decision(None, &[]), ReviewDecision::None);
        assert_eq!(
            review_decision(None, &[row(ReviewState::Requested)]),
            ReviewDecision::ReviewRequired
        );
        assert_eq!(
            review_decision(None, &[row(ReviewState::Commented)]),
            ReviewDecision::Commented
        );
        assert_eq!(
            review_decision(
                None,
                &[
                    row(ReviewState::Approved),
                    row(ReviewState::ChangesRequested)
                ]
            ),
            ReviewDecision::ChangesRequested
        );
        assert_eq!(
            review_decision(Some(DecisionWire::Approved), &[]),
            ReviewDecision::Approved
        );
    }

    #[test]
    fn run_states_follow_the_model_table() {
        use CiState::{Canceled, Fail, Pass, Queued, Running, Skipped};
        let completed = |c| run_state(Some(RunStatus::Completed), Some(c));
        assert_eq!(completed(Conclusion::Success), Pass);
        assert_eq!(completed(Conclusion::Neutral), Pass);
        assert_eq!(completed(Conclusion::Failure), Fail);
        assert_eq!(completed(Conclusion::TimedOut), Fail);
        assert_eq!(completed(Conclusion::ActionRequired), Fail);
        assert_eq!(completed(Conclusion::StartupFailure), Fail);
        assert_eq!(completed(Conclusion::Cancelled), Canceled);
        assert_eq!(completed(Conclusion::Skipped), Skipped);
        assert_eq!(completed(Conclusion::Stale), Skipped);
        assert_eq!(run_state(Some(RunStatus::InProgress), None), Running);
        for status in [
            RunStatus::Queued,
            RunStatus::Waiting,
            RunStatus::Pending,
            RunStatus::Requested,
        ] {
            assert_eq!(run_state(Some(status), None), Queued);
        }
        assert_eq!(status_state(StatusState::Success), Pass);
        assert_eq!(status_state(StatusState::Error), Fail);
        assert_eq!(status_state(StatusState::Expected), Queued);
    }
}
