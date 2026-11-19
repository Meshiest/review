use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use forge::Error;
use forge::auth::Token;
use forge::http::{HttpRequest, HttpResponse, Method, RetryMode, Transport, Url};
use forge::record::{
    Cassette, Exchange, RecordedBody, RecordedRequest, RecordedResponse, RecordingTransport,
    ReplayTransport,
};
use serde_json::{Value, json};

fn url(path: &str) -> Url {
    Url::parse("https://api.example.test")
        .unwrap()
        .join(path)
        .unwrap()
}

fn exchange(request: RecordedRequest, body: Value) -> Exchange {
    Exchange {
        request,
        response: RecordedResponse {
            status: 200,
            headers: [("etag".to_owned(), "\"e1\"".to_owned())].into(),
            body: RecordedBody::Json(body),
        },
    }
}

fn get_key(path: &str, query: &[(&str, &str)]) -> RecordedRequest {
    RecordedRequest {
        method: Method::Get,
        path: path.to_owned(),
        query: query
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect(),
        operation: None,
        variables: None,
        body: None,
    }
}

fn graphql_key(operation: &str, variables: Value) -> RecordedRequest {
    RecordedRequest {
        method: Method::Post,
        path: "/graphql".to_owned(),
        query: Vec::new(),
        operation: Some(operation.to_owned()),
        variables: Some(variables),
        body: None,
    }
}

fn graphql(operation: &str, query: &str, variables: Value) -> HttpRequest {
    HttpRequest::post_json(
        url("/graphql"),
        json!({"operationName": operation, "query": query, "variables": variables}),
    )
    .retry(RetryMode::Safe)
}

fn body(response: &HttpResponse) -> Value {
    serde_json::from_slice(&response.body).unwrap()
}

#[tokio::test]
async fn query_order_does_not_matter() {
    let replay = ReplayTransport::new(
        "inline",
        Cassette {
            exchanges: vec![exchange(
                get_key("/repos/o/r/pulls", &[("page", "2"), ("state", "open")]),
                json!([{"number": 1}]),
            )],
        },
    );
    let response = replay
        .send(&HttpRequest::get(url("/repos/o/r/pulls?state=open&page=2")))
        .await
        .unwrap();
    assert_eq!(body(&response), json!([{"number": 1}]));
    assert_eq!(response.header("etag"), Some("\"e1\""));
    assert!(replay.unused().is_empty());
}

#[tokio::test]
async fn a_miss_names_the_request() {
    let replay = ReplayTransport::new("pulls.json", Cassette::default());
    let err = replay
        .send(&HttpRequest::get(url("/repos/o/r/pulls?state=closed")))
        .await
        .unwrap_err();
    assert_eq!(
        err,
        Error::ReplayMiss {
            cassette: "pulls.json".to_owned(),
            request: "GET /repos/o/r/pulls?state=closed".to_owned(),
        }
    );
    assert!(
        err.to_string()
            .contains("GET /repos/o/r/pulls?state=closed")
    );
}

#[tokio::test]
async fn graphql_matches_operation_and_variables_not_query_text() {
    let replay = ReplayTransport::new(
        "inline",
        Cassette {
            exchanges: vec![
                exchange(
                    graphql_key("Pr", json!({"number": 1, "owner": "o"})),
                    json!({"n": 1}),
                ),
                exchange(
                    graphql_key("Pr", json!({"number": 2, "owner": "o"})),
                    json!({"n": 2}),
                ),
            ],
        },
    );
    // Same variables in another key order, and reformatted query text.
    let two = graphql(
        "Pr",
        "query Pr($owner: String!) {\n  x\n}",
        json!({"owner": "o", "number": 2}),
    );
    assert_eq!(body(&replay.send(&two).await.unwrap()), json!({"n": 2}));
    // The name may come from the query text alone.
    let one = HttpRequest::post_json(
        url("/graphql"),
        json!({"query": "query Pr { x }", "variables": {"owner": "o", "number": 1}}),
    );
    assert_eq!(body(&replay.send(&one).await.unwrap()), json!({"n": 1}));

    let three = graphql("Pr", "query Pr { x }", json!({"owner": "o", "number": 3}));
    let err = replay.send(&three).await.unwrap_err();
    let Error::ReplayMiss { request, .. } = err else {
        panic!("expected a miss, got {err:?}");
    };
    assert!(
        request.contains("operation=Pr") && request.contains("\"number\":3"),
        "{request}"
    );
    let other = graphql(
        "Viewer",
        "query Viewer { x }",
        json!({"owner": "o", "number": 1}),
    );
    assert!(matches!(
        replay.send(&other).await,
        Err(Error::ReplayMiss { .. })
    ));
}

#[tokio::test]
async fn repeated_requests_serve_in_order_then_repeat_the_last() {
    let key = get_key("/user", &[]);
    let replay = ReplayTransport::new(
        "inline",
        Cassette {
            exchanges: vec![exchange(key.clone(), json!(1)), exchange(key, json!(2))],
        },
    );
    let request = HttpRequest::get(url("/user"));
    let mut seen = Vec::new();
    for _ in 0..3 {
        seen.push(body(&replay.send(&request).await.unwrap()));
    }
    assert_eq!(seen, [json!(1), json!(2), json!(2)]);
}

/// Answers every request with a body that echoes secrets back.
struct Leaky;

#[async_trait]
impl Transport for Leaky {
    async fn send(&self, _request: &HttpRequest) -> forge::Result<HttpResponse> {
        let body = json!({
            "login": "cora",
            "email": "cora@example.org",
            "bio": "reach me at cora@example.org",
            "token": "whatever",
            "note": "glpat-abcdefghijklmnopqrstu and gho_secret-token-1234",
        });
        Ok(HttpResponse {
            status: 200,
            headers: vec![
                ("etag".to_owned(), "\"v1\"".to_owned()),
                ("set-cookie".to_owned(), "session=abc".to_owned()),
                ("x-ratelimit-remaining".to_owned(), "4999".to_owned()),
            ],
            body: serde_json::to_vec(&body).unwrap(),
            cached: false,
        })
    }
}

fn scratch(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("forge-record")
        .join(name)
}

#[tokio::test]
async fn recording_scrubs_and_replays() {
    let path = scratch("scrub.json");
    let recorder = RecordingTransport::new(Arc::new(Leaky), path.clone());
    let mut request = HttpRequest::get(url("/user?per_page=100"));
    request.authorization = Some(Token::new("Bearer gho_secret-token-1234"));
    request = request.header("if-none-match", "\"v0\"");
    let live = recorder.send(&request).await.unwrap();
    assert_eq!(
        body(&live)["email"],
        "cora@example.org",
        "the caller sees the real response"
    );

    let text = std::fs::read_to_string(&path).unwrap();
    for secret in [
        "gho_secret",
        "glpat-",
        "cora@example.org",
        "session=abc",
        "Bearer",
        "whatever",
    ] {
        assert!(!text.contains(secret), "{secret} leaked into {text}");
    }
    let cassette: Cassette = serde_json::from_str(&text).unwrap();
    let recorded = &cassette.exchanges[0];
    assert_eq!(recorded.request, get_key("/user", &[("per_page", "100")]));
    assert_eq!(
        recorded.response.headers.keys().collect::<Vec<_>>(),
        ["etag", "x-ratelimit-remaining"]
    );

    let replay = ReplayTransport::load(&path).unwrap();
    let replayed = replay
        .send(&HttpRequest::get(url("/user?per_page=100")))
        .await
        .unwrap();
    assert_eq!(body(&replayed)["login"], "cora");
    assert_eq!(body(&replayed)["email"], "user@example.invalid");
    assert_eq!(replayed.header("etag"), Some("\"v1\""));
}
