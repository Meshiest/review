//! `ForgeHub::avatar` end to end over a fake transport: allowed URLs are
//! fetched and decoded, refused ones never reach the transport.

mod common;

use std::io::Cursor;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use forge::http::{HttpRequest, HttpResponse, Transport};
use forge::model::{
    CheckRun, ForgeKind, InboxSection, PrNumber, PullRequest, RepoRef, Stack, Thread, User,
};
use forge::{Capabilities, Error, Forge, ForgeHub, Result};

struct Github;

#[async_trait]
impl Forge for Github {
    fn kind(&self) -> ForgeKind {
        ForgeKind::GitHub
    }

    fn host(&self) -> &str {
        "github.com"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::GITHUB
    }

    async fn whoami(&self) -> Result<User> {
        Ok(User::new("cora"))
    }

    async fn inbox(&self, _: &InboxSection) -> Result<Vec<PullRequest>> {
        Ok(Vec::new())
    }

    async fn pull_request(&self, _: &RepoRef, number: PrNumber) -> Result<PullRequest> {
        Ok(common::pr(number.0, "a", "main"))
    }

    async fn threads(&self, _: &RepoRef, _: PrNumber) -> Result<Vec<Thread>> {
        Ok(Vec::new())
    }

    async fn checks(&self, _: &RepoRef, _: PrNumber) -> Result<Vec<CheckRun>> {
        Ok(Vec::new())
    }

    async fn stack(&self, _: &RepoRef, number: PrNumber) -> Result<Stack> {
        Ok(forge::stack_by_base_branch(
            &common::pr(number.0, "a", "main"),
            &[],
        ))
    }
}

struct Counting {
    body: Vec<u8>,
    urls: std::sync::Mutex<Vec<String>>,
    calls: AtomicUsize,
}

#[async_trait]
impl Transport for Counting {
    async fn send(&self, request: &HttpRequest) -> Result<HttpResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.urls.lock().unwrap().push(request.url.to_string());
        assert!(
            request.authorization.is_none(),
            "an avatar request must carry no credential"
        );
        Ok(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: self.body.clone(),
            cached: false,
        })
    }
}

fn png() -> Vec<u8> {
    let image = image::RgbaImage::from_pixel(90, 90, image::Rgba([10, 120, 200, 255]));
    let mut out = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut out, image::ImageFormat::Png)
        .unwrap();
    out.into_inner()
}

fn setup() -> (ForgeHub, Arc<Counting>) {
    let transport = Arc::new(Counting {
        body: png(),
        urls: std::sync::Mutex::default(),
        calls: AtomicUsize::new(0),
    });
    let hub = ForgeHub::with_backends(vec![Arc::new(Github)])
        .unwrap()
        .with_avatar_transport(transport.clone());
    (hub, transport)
}

fn wait<T>(future: impl Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

#[test]
fn an_allowed_avatar_is_fetched_with_a_size_hint_and_decoded() {
    let (hub, transport) = setup();
    let avatar = wait(hub.avatar(
        "github.com",
        "https://avatars.githubusercontent.com/u/1?v=4",
    ))
    .unwrap();
    assert_eq!((avatar.width, avatar.height), (64, 64));
    assert_eq!(
        transport.urls.lock().unwrap().as_slice(),
        ["https://avatars.githubusercontent.com/u/1?v=4&s=64"]
    );
}

#[test]
fn refused_and_unknown_requests_never_reach_the_transport() {
    let (hub, transport) = setup();
    for url in [
        "https://evil.example/a.png",
        "http://avatars.githubusercontent.com/u/1",
        "https://secure.gravatar.com/avatar/abc",
    ] {
        let result = wait(hub.avatar("github.com", url));
        assert!(matches!(result, Err(Error::Unsupported { .. })), "{url}");
    }
    let unknown = wait(hub.avatar("nowhere.test", "https://avatars.githubusercontent.com/u/1"));
    assert!(matches!(unknown, Err(Error::UnknownForge(_))));
    assert_eq!(transport.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn gravatar_loads_only_once_allowed() {
    let (hub, transport) = setup();
    let url = "https://secure.gravatar.com/avatar/abc?s=80&d=identicon";
    assert!(wait(hub.avatar("github.com", url)).is_err());
    hub.set_allow_gravatar(true);
    assert!(wait(hub.avatar("github.com", url)).is_ok());
    assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
    hub.set_allow_gravatar(false);
    assert!(wait(hub.avatar("github.com", url)).is_err());
}
