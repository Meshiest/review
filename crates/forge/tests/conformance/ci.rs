//! CI reads, logs and retries against one recorded `ci_live` run per forge
//! (`tests/fixtures/<forge>/ci.json`).
//!
//! Record: `REVIEW_RECORD=1 cargo run -p forge --example ci_live -- --forge
//! <forge>` (a few minutes on `review-sandbox`; it opens and closes a
//! scratch pull request). The replay runs the same steps as the live check
//! with no pauses: the replay serves each request's answers in recorded
//! order, so every wait loop ends where the live one did, and every
//! recorded exchange must be used.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use forge::Forge;
use forge::auth::{Credential, StaticCredential, Token};
use forge::forgejo::Forgejo;
use forge::github::GitHub;
use forge::gitlab::GitLab;
use forge::http::{
    CacheMode, HttpClient, HttpOptions, HttpRequest, HttpResponse, ManualClock, Method, RetryMode,
    Transport,
};
use forge::model::{CiState, ForgeKind, PrNumber, PrState, RepoRef, Sha};
use forge::record::{
    Cassette, Exchange, RecordedBody, RecordedRequest, RecordedResponse, ReplayTransport,
    fixture_path,
};
use serde_json::{Value, json};

use super::ci_steps::{self, BUILD_STEPS, JOBS, Pace, Seen, Status};

/// One request as the backend sent it.
#[derive(Clone, Debug)]
struct Sent {
    method: Method,
    path: String,
    range: Option<String>,
    retry: RetryMode,
    cache: CacheMode,
}

struct Watching {
    replay: Arc<ReplayTransport>,
    sent: Mutex<Vec<Sent>>,
}

#[async_trait]
impl Transport for Watching {
    async fn send(&self, request: &HttpRequest) -> forge::Result<HttpResponse> {
        self.sent.lock().unwrap().push(Sent {
            method: request.method,
            path: request.url.path().to_owned(),
            range: request.header_value("range").map(str::to_owned),
            retry: request.retry,
            cache: request.cache,
        });
        self.replay.send(request).await
    }
}

struct Sandbox {
    kind: ForgeKind,
    host: &'static str,
    owner: &'static str,
    /// The query parameter a runs list filters the commit by.
    sha_param: &'static str,
    /// The path a log is read from ends so.
    log_suffix: &'static str,
    /// The path the `test` retry is posted to ends so.
    retry_suffix: &'static str,
}

const GITHUB: Sandbox = Sandbox {
    kind: ForgeKind::GitHub,
    host: "github.com",
    owner: "Lovelace",
    sha_param: "head_sha",
    log_suffix: "/logs",
    retry_suffix: "/rerun",
};

const GITLAB: Sandbox = Sandbox {
    kind: ForgeKind::GitLab,
    host: "gitlab.com",
    owner: "Lovelace",
    sha_param: "sha",
    log_suffix: "/trace",
    retry_suffix: "/retry",
};

const FORGEJO: Sandbox = Sandbox {
    kind: ForgeKind::Forgejo,
    host: "forgejo.example.com",
    owner: "cora",
    sha_param: "head_sha",
    log_suffix: "/logs",
    retry_suffix: "/rerun",
};

const REPLAY: Pace = Pace {
    poll: Duration::ZERO,
    tail: Duration::ZERO,
    timeout: Duration::from_secs(15 * 60),
};

fn cassette(kind: ForgeKind) -> Cassette {
    let text = std::fs::read_to_string(fixture_path(kind, "ci")).unwrap();
    serde_json::from_str(&text).unwrap()
}

/// The commits the recording listed runs for: the failing head first, the
/// fix last.
fn recorded_heads(sandbox: &Sandbox, cassette: &Cassette) -> (Sha, Sha) {
    let shas: Vec<&str> = cassette
        .exchanges
        .iter()
        .filter_map(|e| {
            e.request
                .query
                .iter()
                .find(|(name, _)| name == sandbox.sha_param)
                .map(|(_, value)| value.as_str())
        })
        .collect();
    let (first, last) = (shas.first().unwrap(), shas.last().unwrap());
    assert_ne!(first, last, "the recording lists runs of two commits");
    (Sha::new(*first), Sha::new(*last))
}

struct Replayed {
    seen: Seen,
    statuses: Vec<(&'static str, Status)>,
    sent: Vec<Sent>,
    unused: Vec<String>,
}

async fn replay(sandbox: &Sandbox) -> Replayed {
    let path = fixture_path(sandbox.kind, "ci");
    let cassette = cassette(sandbox.kind);
    let (head, fix) = recorded_heads(sandbox, &cassette);
    let watching = Arc::new(Watching {
        replay: Arc::new(ReplayTransport::new(path.display().to_string(), cassette)),
        sent: Mutex::default(),
    });
    let options = HttpOptions {
        clock: Arc::new(ManualClock::new(Duration::from_secs(1_700_000_000))),
        ..HttpOptions::for_kind(sandbox.kind)
    };
    let credentials = Arc::new(StaticCredential(Credential::bearer(Token::new("replay"))));
    let http = HttpClient::new(
        sandbox.kind,
        sandbox.host,
        watching.clone(),
        credentials,
        options,
    );
    let forge: Box<dyn Forge> = match sandbox.kind {
        ForgeKind::GitHub => Box::new(GitHub::connect(http).await.unwrap()),
        ForgeKind::GitLab => Box::new(GitLab::connect(http).await.unwrap()),
        ForgeKind::Forgejo => Box::new(Forgejo::connect(http).await.unwrap()),
    };
    let repo = RepoRef::new(sandbox.host, sandbox.owner, "review-sandbox");
    let (outcomes, seen) = ci_steps::run(
        forge.as_ref(),
        &repo,
        &head,
        REPLAY,
        move || Ok(fix),
        |_| {},
    )
    .await;
    Replayed {
        seen,
        statuses: outcomes.into_iter().map(|o| (o.step, o.status)).collect(),
        sent: watching.sent.lock().unwrap().clone(),
        unused: watching.replay.unused(),
    }
}

fn check(sandbox: &Sandbox, replayed: &Replayed, steps: bool) {
    let failed: Vec<_> = replayed
        .statuses
        .iter()
        .filter(|(_, status)| matches!(status, Status::Fail(_)))
        .collect();
    assert!(failed.is_empty(), "{failed:?}");
    let names: Vec<&str> = replayed.statuses.iter().map(|(step, _)| *step).collect();
    assert_eq!(names, ["run", "tail", "states", "steps", "retry", "fix"]);
    assert_eq!(
        matches!(replayed.statuses[3].1, Status::Skip(_)),
        !steps,
        "steps are skipped exactly where the forge has none"
    );
    assert_eq!(
        replayed.unused,
        Vec::<String>::new(),
        "every recorded request is replayed"
    );

    let seen = &replayed.seen;
    let run = seen.run.as_ref().unwrap();
    for name in JOBS {
        assert!(run.job(name).is_some(), "{name} in {:?}", run.jobs);
    }
    let finished = seen.finished.as_ref().unwrap();
    assert!(finished.completed_at.is_some() && finished.started_at.is_some());
    assert_eq!(finished.state, CiState::Fail);
    assert!(finished.url.is_some() && finished.number.is_some());
    let build = finished.job("build").unwrap();
    if steps {
        let names: Vec<&str> = build.steps.iter().map(|s| s.name.as_str()).collect();
        for step in BUILD_STEPS {
            assert!(names.contains(&step), "{names:?}");
        }
        assert!(build.steps.windows(2).all(|w| w[0].number < w[1].number));
    } else {
        assert!(build.steps.is_empty());
    }

    // The tail: each read asks where the last ended, with that range.
    let tail = &seen.tail;
    assert!(tail.len() > 1, "{tail:?}");
    assert_eq!(tail[0].asked, 0);
    for pair in tail.windows(2) {
        assert_eq!(pair[1].asked, pair[0].next_offset, "{tail:?}");
    }
    assert!(tail.iter().all(|read| read.offset == read.asked));
    assert!(tail.last().unwrap().complete);
    assert!(tail[..tail.len() - 1].iter().all(|read| !read.complete));
    assert_eq!(
        u64::try_from(seen.log.len()).unwrap(),
        tail.last().unwrap().next_offset
    );
    ci_steps::check_slow_log(&seen.log).unwrap();
    let ranges: Vec<String> = replayed
        .sent
        .iter()
        .filter(|sent| sent.path.ends_with(sandbox.log_suffix))
        .map(|sent| {
            assert_eq!(sent.cache, CacheMode::Bypass, "logs skip the ETag cache");
            sent.range.clone().unwrap_or_default()
        })
        .collect();
    let asked: Vec<String> = tail
        .iter()
        .map(|read| {
            format!(
                "bytes={}-{}",
                read.asked,
                read.asked + forge::ci_log::LOG_FETCH_MAX - 1
            )
        })
        .collect();
    assert_eq!(ranges, asked);

    // One retry of `test`, never resent, and it failed again.
    let writes: Vec<&Sent> = replayed
        .sent
        .iter()
        .filter(|sent| sent.method != Method::Get)
        .collect();
    assert_eq!(writes.len(), 1, "{writes:?}");
    let test = finished.job("test").unwrap();
    assert!(
        writes[0]
            .path
            .ends_with(&format!("/jobs/{}{}", test.id, sandbox.retry_suffix)),
        "{}",
        writes[0].path
    );
    assert_eq!(
        (writes[0].method, writes[0].retry),
        (Method::Post, RetryMode::Never)
    );
    let retried = seen.retried.as_ref().unwrap();
    assert_eq!(retried.state, CiState::Fail);
    assert_ne!((&retried.id, retried.attempt), (&test.id, test.attempt));

    let fixed = seen.fixed.as_ref().unwrap();
    assert_eq!(fixed.state, CiState::Pass);
    assert!(fixed.jobs.iter().all(|job| job.state == CiState::Pass));
}

#[tokio::test]
async fn github_ci_replays_runs_jobs_steps_logs_and_a_retry() {
    let replayed = replay(&GITHUB).await;
    check(&GITHUB, &replayed, true);
}

#[tokio::test]
async fn gitlab_ci_replays_pipelines_jobs_logs_and_a_retry() {
    let replayed = replay(&GITLAB).await;
    check(&GITLAB, &replayed, false);
    let test = replayed
        .seen
        .finished
        .as_ref()
        .unwrap()
        .job("test")
        .unwrap();
    assert_eq!(test.stage.as_deref(), Some("test"));
    assert_ne!(
        replayed.seen.retried.as_ref().unwrap().id,
        test.id,
        "a GitLab retry is a new job"
    );
}

#[tokio::test]
async fn forgejo_ci_replays_runs_jobs_steps_logs_and_a_rerun() {
    let replayed = replay(&FORGEJO).await;
    check(&FORGEJO, &replayed, true);
    let test = replayed
        .seen
        .finished
        .as_ref()
        .unwrap()
        .job("test")
        .unwrap();
    let retried = replayed.seen.retried.as_ref().unwrap();
    assert_eq!(retried.id, test.id, "a Forgejo re-run keeps the job id");
    assert_eq!(retried.attempt, test.attempt.map(|n| n + 1));
}

fn get(
    path: &str,
    query: &[(&str, &str)],
    status: u16,
    etag: Option<&str>,
    body: Value,
) -> Exchange {
    Exchange {
        request: RecordedRequest {
            method: Method::Get,
            path: path.to_owned(),
            query: query
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            operation: None,
            variables: None,
            body: None,
        },
        response: RecordedResponse {
            status,
            headers: etag
                .map(|etag| ("etag".to_owned(), etag.to_owned()))
                .into_iter()
                .collect(),
            body: if body.is_null() {
                RecordedBody::Empty
            } else {
                RecordedBody::Json(body)
            },
        },
    }
}

async fn inline(sandbox: &Sandbox, exchanges: Vec<Exchange>) -> (Box<dyn Forge>, Arc<Watching>) {
    let watching = Arc::new(Watching {
        replay: Arc::new(ReplayTransport::new("inline", Cassette { exchanges })),
        sent: Mutex::default(),
    });
    let options = HttpOptions {
        clock: Arc::new(ManualClock::new(Duration::from_secs(1_700_000_000))),
        ..HttpOptions::for_kind(sandbox.kind)
    };
    let credentials = Arc::new(StaticCredential(Credential::bearer(Token::new("replay"))));
    let http = HttpClient::new(
        sandbox.kind,
        sandbox.host,
        watching.clone(),
        credentials,
        options,
    );
    let forge: Box<dyn Forge> = match sandbox.kind {
        ForgeKind::GitHub => Box::new(GitHub::connect(http).await.unwrap()),
        ForgeKind::GitLab => Box::new(GitLab::connect(http).await.unwrap()),
        ForgeKind::Forgejo => Box::new(Forgejo::connect(http).await.unwrap()),
    };
    (forge, watching)
}

const SHA: &str = "1111111111111111111111111111111111111111";

#[tokio::test]
async fn ci_pulse_on_github_is_three_conditional_reads() {
    let repo = "/repos/Lovelace/review-sandbox";
    let pull = json!({"state": "open", "draft": false, "merged_at": null,
        "updated_at": "2026-11-17T12:00:00Z", "head": {"sha": SHA}});
    let checks = json!({"check_runs": [
        {"status": "completed", "conclusion": "success"},
        {"status": "in_progress", "conclusion": null},
    ]});
    let statuses = json!({"statuses": [{"state": "success"}]});
    let page = [("per_page", "100")];
    let (forge, watching) = inline(
        &GITHUB,
        vec![
            get(&format!("{repo}/pulls/4"), &[], 200, Some("\"p\""), pull),
            get(
                &format!("{repo}/commits/{SHA}/check-runs"),
                &page,
                200,
                Some("\"c\""),
                checks,
            ),
            get(
                &format!("{repo}/commits/{SHA}/status"),
                &page,
                200,
                Some("\"s\""),
                statuses,
            ),
            get(&format!("{repo}/pulls/4"), &[], 304, None, Value::Null),
            get(
                &format!("{repo}/commits/{SHA}/check-runs"),
                &page,
                304,
                None,
                Value::Null,
            ),
            get(
                &format!("{repo}/commits/{SHA}/status"),
                &page,
                304,
                None,
                Value::Null,
            ),
        ],
    )
    .await;
    let sandbox = RepoRef::new("github.com", "Lovelace", "review-sandbox");
    let first = forge
        .pull_request_pulse(&sandbox, PrNumber(4))
        .await
        .unwrap();
    assert_eq!(
        (first.state, first.ci, first.head.as_str()),
        (PrState::Open, CiState::Running, SHA)
    );
    let second = forge
        .pull_request_pulse(&sandbox, PrNumber(4))
        .await
        .unwrap();
    assert_eq!(first, second, "304s answer from the cache");
    assert_eq!(watching.sent.lock().unwrap().len(), 6);
    assert_eq!(watching.replay.unused(), Vec::<String>::new());
}

#[tokio::test]
async fn ci_pulse_on_gitlab_is_one_conditional_read() {
    let mr = json!({"state": "merged", "draft": false, "sha": SHA,
        "updated_at": "2026-11-17T12:00:00Z", "head_pipeline": {"status": "failed"}});
    let path = "/api/v4/projects/Lovelace%2Freview-sandbox/merge_requests/4";
    let (forge, watching) = inline(&GITLAB, vec![get(path, &[], 200, Some("W/\"m\""), mr)]).await;
    let sandbox = RepoRef::new("gitlab.com", "Lovelace", "review-sandbox");
    let pulse = forge
        .pull_request_pulse(&sandbox, PrNumber(4))
        .await
        .unwrap();
    assert_eq!((pulse.state, pulse.ci), (PrState::Merged, CiState::Fail));
    assert_eq!(watching.sent.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn ci_pulse_on_forgejo_reads_the_pull_and_the_combined_status() {
    let repo = "/api/v1/repos/cora/review-sandbox";
    let pull = json!({"id": 1, "number": 4, "state": "open", "draft": false, "merged": false,
        "updated_at": "2026-11-17T12:00:00Z",
        "base": {"ref": "main", "sha": SHA}, "head": {"ref": "x", "sha": SHA}});
    let (forge, watching) = inline(
        &FORGEJO,
        vec![
            get(
                "/api/v1/version",
                &[],
                200,
                None,
                json!({"version": "17.0.1+gitea-1.22.0"}),
            ),
            get(&format!("{repo}/pulls/4"), &[], 200, None, pull),
            get(
                &format!("{repo}/commits/{SHA}/status"),
                &[],
                200,
                None,
                json!({"state": "pending", "total_count": 3}),
            ),
        ],
    )
    .await;
    let sandbox = RepoRef::new("forgejo.example.com", "cora", "review-sandbox");
    let pulse = forge
        .pull_request_pulse(&sandbox, PrNumber(4))
        .await
        .unwrap();
    assert_eq!((pulse.state, pulse.ci), (PrState::Open, CiState::Queued));
    assert_eq!(watching.sent.lock().unwrap().len(), 3);
}
