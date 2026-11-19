//! Requests an idle watched pull request costs: the
//! app's watch wiring ([`app::watching::Watching`] choosing and following
//! subscriptions) over the forge's watch engine, each real backend talking
//! to a counting mock server, on a fake clock run through an hour. Nothing
//! changes on the forge, so every request is the polling's own.
//!
//! The mock answers the pulls (`pulls/N`, `check-runs` and `status` on
//! GitHub, `merge_requests/N` on GitLab) with ETags and `304`s like the
//! forges; the snapshot read once at the start, and all of Forgejo, are
//! served from the recorded conformance fixtures.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use app::settings::NotificationSettings;
use app::watching::{Effect, Engine, PrInfo, Watching};
use async_trait::async_trait;
use forge::Forge;
use forge::auth::{Credential, StaticCredential, Token};
use forge::forgejo::Forgejo;
use forge::github::GitHub;
use forge::gitlab::GitLab;
use forge::http::{
    Clock, HttpClient, HttpOptions, HttpRequest, HttpResponse, ManualClock, Method, Transport,
};
use forge::model::{ForgeKind, PrNumber, RepoRef};
use forge::record::{Cassette, Exchange, ReplayTransport, fixture_path};
use forge::watch::{SubscriptionId, WatchTarget, Watcher};
use store::PrKey;

const HOUR: Duration = Duration::from_secs(3600);
const BUDGET: usize = 60;
/// The sandbox pull request every recorded snapshot fixture is of.
const NUMBER: u64 = 4;
const HEAD: &str = "4f1c3c9d6f0a2b7e8d5c4b3a29180716f5e4d3c2";
const UPDATED: &str = "2026-11-16T12:05:00Z";

/// Fixtures that hold the reads of pull request 4, per forge.
fn fixtures(kind: ForgeKind) -> &'static [&'static str] {
    match kind {
        ForgeKind::GitHub | ForgeKind::GitLab => &["snapshot"],
        ForgeKind::Forgejo => &["whoami", "pulls", "threads", "checks"],
    }
}

/// Every recorded exchange of `names`, the last answer of each request
/// kept, so a repeated read gets the same answer each time.
fn cassette(kind: ForgeKind, names: &[&str]) -> Cassette {
    let mut exchanges: Vec<Exchange> = Vec::new();
    for name in names {
        let text = std::fs::read_to_string(fixture_path(kind, name)).expect("fixture");
        let cassette: Cassette = serde_json::from_str(&text).expect("cassette");
        for exchange in cassette.exchanges {
            exchanges.retain(|kept| kept.request != exchange.request);
            exchanges.push(exchange);
        }
    }
    Cassette { exchanges }
}

/// A forge server that counts what it is asked.
struct MockForge {
    kind: ForgeKind,
    replay: ReplayTransport,
    sent: Mutex<Vec<String>>,
}

fn json(status: u16, body: &str, etag: Option<&str>) -> HttpResponse {
    HttpResponse {
        status,
        headers: etag
            .map(|etag| vec![("etag".to_owned(), etag.to_owned())])
            .into_iter()
            .flatten()
            .collect(),
        body: body.as_bytes().to_vec(),
        cached: false,
    }
}

impl MockForge {
    /// The pulse reads, answered the way the forge would for a pull
    /// request where nothing happens.
    fn pulse(&self, request: &HttpRequest) -> Option<HttpResponse> {
        let path = request.url.path();
        let (body, etag) = match self.kind {
            ForgeKind::GitHub if path.ends_with(&format!("/pulls/{NUMBER}")) => (
                format!(
                    r#"{{"state":"open","draft":false,"merged_at":null,"updated_at":"{UPDATED}","head":{{"sha":"{HEAD}"}}}}"#
                ),
                "\"pull\"",
            ),
            ForgeKind::GitHub if path.ends_with("/check-runs") => (
                r#"{"total_count":3,"check_runs":[{"status":"completed","conclusion":"success"},{"status":"completed","conclusion":"failure"},{"status":"completed","conclusion":"success"}]}"#.to_owned(),
                "\"checks\"",
            ),
            ForgeKind::GitHub if path.ends_with("/status") => (
                r#"{"state":"success","statuses":[]}"#.to_owned(),
                "\"status\"",
            ),
            ForgeKind::GitLab if path.ends_with(&format!("/merge_requests/{NUMBER}")) => (
                format!(
                    r#"{{"state":"opened","draft":false,"sha":"{HEAD}","updated_at":"{UPDATED}","head_pipeline":{{"status":"failed"}}}}"#
                ),
                "\"mr\"",
            ),
            _ => return None,
        };
        if request.header_value("if-none-match") == Some(etag) {
            return Some(json(304, "", Some(etag)));
        }
        Some(json(200, &body, Some(etag)))
    }
}

#[async_trait]
impl Transport for MockForge {
    async fn send(&self, request: &HttpRequest) -> forge::Result<HttpResponse> {
        self.sent
            .lock()
            .unwrap()
            .push(format!("{} {}", request.method, request.url.path()));
        if request.method == Method::Get
            && let Some(response) = self.pulse(request)
        {
            return Ok(response);
        }
        self.replay.send(request).await
    }
}

/// The forge's watcher as the app's engine.
struct Local {
    watcher: Watcher,
    forge: Arc<dyn Forge>,
}

impl Engine for Local {
    fn subscribe(&mut self, target: WatchTarget) -> SubscriptionId {
        self.watcher.subscribe(self.forge.clone(), target)
    }

    fn unsubscribe(&mut self, id: SubscriptionId) {
        self.watcher.unsubscribe(id);
    }
}

struct Sandbox {
    kind: ForgeKind,
    host: &'static str,
    owner: &'static str,
}

const SANDBOXES: [Sandbox; 3] = [
    Sandbox {
        kind: ForgeKind::GitHub,
        host: "github.com",
        owner: "Lovelace",
    },
    Sandbox {
        kind: ForgeKind::GitLab,
        host: "gitlab.com",
        owner: "Lovelace",
    },
    Sandbox {
        kind: ForgeKind::Forgejo,
        host: "forgejo.example.com",
        owner: "cora",
    },
];

struct Cost {
    requests: Vec<String>,
    effects: Vec<Effect>,
    subscriptions: usize,
}

async fn idle_hour(sandbox: &Sandbox) -> Cost {
    let kind = sandbox.kind;
    let clock = Arc::new(ManualClock::new(Duration::from_secs(1_700_000_000)));
    let mock = Arc::new(MockForge {
        kind,
        replay: ReplayTransport::new(kind.as_str(), cassette(kind, fixtures(kind))),
        sent: Mutex::default(),
    });
    let options = HttpOptions {
        clock: clock.clone(),
        ..HttpOptions::for_kind(kind)
    };
    let credentials = Arc::new(StaticCredential(Credential::bearer(Token::new("test"))));
    let http = HttpClient::new(kind, sandbox.host, mock.clone(), credentials, options);
    let forge: Arc<dyn Forge> = match kind {
        ForgeKind::GitHub => Arc::new(GitHub::connect(http).await.unwrap()),
        ForgeKind::GitLab => Arc::new(GitLab::connect(http).await.unwrap()),
        ForgeKind::Forgejo => Arc::new(Forgejo::connect(http).await.unwrap()),
    };
    let connect = mock.sent.lock().unwrap().len();

    // The app's side: the viewer's own pull request in the inbox is
    // watched by default, nothing else is open.
    let key = PrKey::new(
        RepoRef::new(sandbox.host, sandbox.owner, "review-sandbox"),
        PrNumber(NUMBER),
    );
    let mut watching = Watching::new(NotificationSettings::default());
    watching.set_me(sandbox.host, "cora");
    let wanted = watching.plan(
        [PrInfo {
            key: key.clone(),
            label: format!("review-sandbox#{NUMBER}"),
            title: "Idle".to_owned(),
            authored: true,
            mine: true,
        }],
        None,
    );
    let mut engine = Local {
        watcher: Watcher::new(clock.clone() as Arc<dyn Clock>),
        forge: forge.clone(),
    };
    watching.sync(wanted, &mut engine);
    let subscriptions = watching.subscription_count();

    let mut effects = Vec::new();
    engine
        .watcher
        .run_until(clock.now() + HOUR, |change| {
            effects.extend(watching.handle(change, Instant::now()));
        })
        .await;
    // What the app would read on an effect, read here too.
    for effect in &effects {
        if let Effect::RefreshCi(_) = effect {
            let _ = forge
                .ci_runs(&key.repo, &forge::model::Sha::new(HEAD))
                .await;
        }
    }
    let requests = mock.sent.lock().unwrap()[connect..].to_vec();
    Cost {
        requests,
        effects,
        subscriptions,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn an_idle_watched_pull_request_costs_at_most_60_requests_an_hour() {
    let mut failures = Vec::new();
    for sandbox in &SANDBOXES {
        let cost = idle_hour(sandbox).await;
        let mut by_path: BTreeMap<&str, usize> = BTreeMap::new();
        for request in &cost.requests {
            *by_path.entry(request.as_str()).or_default() += 1;
        }
        println!(
            "poll_cost {}: {} requests in a simulated hour (budget {BUDGET}), {} subscription(s)",
            sandbox.kind,
            cost.requests.len(),
            cost.subscriptions
        );
        for (request, count) in &by_path {
            println!("  {count:>3}  {request}");
        }
        assert_eq!(
            cost.subscriptions, 1,
            "one subscription for one pull request"
        );
        let failed: Vec<&Effect> = cost
            .effects
            .iter()
            .filter(|effect| matches!(effect, Effect::PollFailed { .. }))
            .collect();
        assert!(failed.is_empty(), "{}: {failed:?}", sandbox.kind);
        assert!(
            cost.effects.is_empty(),
            "{}: idle, yet {:?}",
            sandbox.kind,
            cost.effects
        );
        if cost.requests.len() > BUDGET {
            failures.push(format!("{}: {}", sandbox.kind, cost.requests.len()));
        }
    }
    assert!(failures.is_empty(), "over {BUDGET} an hour: {failures:?}");
}
