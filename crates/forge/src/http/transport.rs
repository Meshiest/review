use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{self, HeaderValue};

use super::redirect::same_origin;
use super::{HttpRequest, HttpResponse, Method, Transport, Url};
use crate::auth::Token;
use crate::error::{Error, Result};

/// Headers that must not follow a redirect to another origin.
const CREDENTIAL_HEADERS: [&str; 3] = ["authorization", "cookie", "proxy-authorization"];
/// Most of a body reserved before it is known to exist: a server can claim
/// any `Content-Length`.
const PREALLOCATE_MAX: usize = 1024 * 1024;

/// The real network: one `reqwest` client for every forge (rustls, HTTP/2
/// by ALPN, pooled keep-alive connections).
///
/// Redirects and body size are enforced here, per request: reqwest's own
/// redirect handling is off, each hop is checked against
/// [`HttpRequest::redirects`], and the body is read in chunks and abandoned
/// as soon as it passes [`HttpRequest::max_body`].
#[derive(Clone)]
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    /// # Errors
    /// [`Error::Network`] when TLS roots cannot be loaded.
    pub fn new() -> Result<Self> {
        Self::from_builder(Self::builder())
    }

    fn builder() -> reqwest::ClientBuilder {
        reqwest::Client::builder()
            .use_rustls_tls()
            .user_agent(concat!("review/", env!("CARGO_PKG_VERSION")))
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(60))
    }

    fn from_builder(builder: reqwest::ClientBuilder) -> Result<Self> {
        let client = builder.build().map_err(|err| Error::Network {
            request: "client setup".to_owned(),
            reason: err.to_string(),
        })?;
        Ok(Self { client })
    }
}

fn network_error(describe: &str, err: reqwest::Error) -> Error {
    Error::Network {
        request: describe.to_owned(),
        // Without the URL: it can carry a presigned query.
        reason: err.without_url().to_string(),
    }
}

fn reqwest_method(method: Method) -> reqwest::Method {
    match method {
        Method::Get => reqwest::Method::GET,
        Method::Post => reqwest::Method::POST,
        Method::Put => reqwest::Method::PUT,
        Method::Patch => reqwest::Method::PATCH,
        Method::Delete => reqwest::Method::DELETE,
    }
}

/// The method of the next hop. A `303` always becomes a GET and `301`/`302`
/// turn a POST into one; `307`/`308` resend as they were.
fn method_after_redirect(status: u16, method: Method) -> Method {
    match (status, method) {
        (303, _) | (301 | 302, Method::Post) => Method::Get,
        _ => method,
    }
}

/// One request as it is sent on the current hop.
struct Hop<'a> {
    url: Url,
    method: Method,
    headers: Vec<(String, String)>,
    authorization: Option<&'a Token>,
    body: Option<Vec<u8>>,
}

impl Hop<'_> {
    fn builder(&self, client: &reqwest::Client) -> Result<reqwest::RequestBuilder> {
        let mut builder = client.request(reqwest_method(self.method), self.url.clone());
        for (name, value) in &self.headers {
            builder = builder.header(name, value);
        }
        if let Some(token) = self.authorization {
            let mut value = HeaderValue::from_str(token.expose()).map_err(|_| Error::Auth {
                host: self.url.host_str().unwrap_or_default().to_owned(),
                reason: "token is not a valid header value".to_owned(),
            })?;
            value.set_sensitive(true);
            builder = builder.header(header::AUTHORIZATION, value);
        }
        if let Some(body) = &self.body {
            builder = builder
                .header(header::CONTENT_TYPE, "application/json")
                .body(body.clone());
        }
        Ok(builder)
    }

    /// Moves to `next` after a redirect with `status`.
    fn follow(&mut self, status: u16, next: Url) {
        if !same_origin(&self.url, &next) {
            self.authorization = None;
            self.headers
                .retain(|(name, _)| !CREDENTIAL_HEADERS.contains(&name.as_str()));
        }
        self.method = method_after_redirect(status, self.method);
        if self.method == Method::Get {
            self.body = None;
        }
        self.url = next;
    }
}

/// Reads the body of `response`, giving up once it passes `max` decoded
/// bytes. `Content-Length` (the encoded size) only refuses early; the count
/// of decoded chunks is the limit, so a small gzip body cannot expand past it.
async fn read_body(mut response: reqwest::Response, max: usize, describe: &str) -> Result<Vec<u8>> {
    let too_large = || Error::TooLarge {
        request: describe.to_owned(),
        limit: max,
    };
    let claimed = response.content_length();
    if claimed.is_some_and(|len| len > u64::try_from(max).unwrap_or(u64::MAX)) {
        return Err(too_large());
    }
    let reserve = claimed
        .and_then(|len| usize::try_from(len).ok())
        .map_or(0, |len| len.min(max).min(PREALLOCATE_MAX));
    let mut body = Vec::with_capacity(reserve);
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|err| network_error(describe, err))?
    {
        if chunk.len() > max - body.len() {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[async_trait]
impl Transport for ReqwestTransport {
    async fn send(&self, request: &HttpRequest) -> Result<HttpResponse> {
        let describe = request.describe();
        let body = request
            .body
            .as_ref()
            .map(serde_json::to_vec)
            .transpose()
            .map_err(|err| Error::Decode {
                request: describe.clone(),
                reason: err.to_string(),
            })?;
        let mut hop = Hop {
            url: request.url.clone(),
            method: request.method,
            headers: request.headers.clone(),
            authorization: request.authorization.as_ref(),
            body,
        };
        let mut hops = 0;
        loop {
            let response = hop
                .builder(&self.client)?
                .send()
                .await
                .map_err(|err| network_error(&describe, err))?;
            let status = response.status().as_u16();
            if matches!(status, 301 | 302 | 303 | 307 | 308)
                && let Some(location) = response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                && let Some(next) =
                    request
                        .redirects
                        .next(&hop.url, location, hops)
                        .map_err(|reason| Error::Redirect {
                            request: describe.clone(),
                            reason,
                        })?
            {
                hop.follow(status, next);
                hops += 1;
                continue;
            }
            let headers = response
                .headers()
                .iter()
                .filter_map(|(name, value)| {
                    Some((name.as_str().to_owned(), value.to_str().ok()?.to_owned()))
                })
                .collect();
            let body = read_body(response, request.max_body, &describe).await?;
            return Ok(HttpResponse {
                status,
                headers,
                body,
                cached: false,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use flate2::Compression;
    use flate2::write::GzEncoder;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    use super::*;
    use crate::http::{MAX_REDIRECTS, RedirectPolicy, RedirectRefusal};

    /// What a test server was asked.
    #[derive(Clone, Debug)]
    struct Seen {
        path: String,
        /// Lowercase names.
        headers: Vec<(String, String)>,
    }

    impl Seen {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
        }
    }

    enum Reply {
        /// A whole raw response; the connection then closes.
        Raw(Vec<u8>),
        /// A head with no length, then `total` bytes until the client leaves.
        Flood { total: usize },
        /// A head promising `claimed` bytes, one byte of them, then silence.
        Stall { claimed: usize },
    }

    fn raw(status: &str, headers: &[(&str, &str)], body: &[u8]) -> Reply {
        let mut out = format!(
            "HTTP/1.1 {status}\r\nconnection: close\r\ncontent-length: {}\r\n",
            body.len()
        );
        for (name, value) in headers {
            out.push_str(&format!("{name}: {value}\r\n"));
        }
        out.push_str("\r\n");
        let mut bytes = out.into_bytes();
        bytes.extend_from_slice(body);
        Reply::Raw(bytes)
    }

    fn redirect(status: &str, location: &str) -> Reply {
        raw(status, &[("location", location)], b"")
    }

    struct Server {
        addr: SocketAddr,
        seen: Arc<Mutex<Vec<Seen>>>,
        /// Bytes of a [`Reply::Flood`] written before the client went away.
        flooded: Arc<AtomicUsize>,
        flood_over: Arc<AtomicBool>,
    }

    impl Server {
        fn url(&self, path: &str) -> Url {
            Url::parse(&format!("http://{}{path}", self.addr)).unwrap()
        }

        fn seen(&self) -> Vec<Seen> {
            self.seen.lock().unwrap().clone()
        }
    }

    async fn serve(handler: impl Fn(&Seen) -> Reply + Send + Sync + 'static) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = Server {
            addr: listener.local_addr().unwrap(),
            seen: Arc::default(),
            flooded: Arc::default(),
            flood_over: Arc::default(),
        };
        let handler = Arc::new(handler);
        let (seen, flooded, flood_over) = (
            server.seen.clone(),
            server.flooded.clone(),
            server.flood_over.clone(),
        );
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let (handler, seen) = (handler.clone(), seen.clone());
                let (flooded, flood_over) = (flooded.clone(), flood_over.clone());
                tokio::spawn(async move {
                    let Some(request) = read_head(&mut stream).await else {
                        return;
                    };
                    let reply = handler(&request);
                    seen.lock().unwrap().push(request);
                    write_reply(&mut stream, reply, &flooded, &flood_over).await;
                });
            }
        });
        server
    }

    async fn read_head(stream: &mut TcpStream) -> Option<Seen> {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 2048];
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = stream.read(&mut chunk).await.ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        let text = String::from_utf8_lossy(&buf);
        let mut lines = text.split("\r\n");
        let path = lines.next()?.split(' ').nth(1)?.to_owned();
        let headers = lines
            .take_while(|line| !line.is_empty())
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
            .collect();
        Some(Seen { path, headers })
    }

    async fn write_reply(
        stream: &mut TcpStream,
        reply: Reply,
        flooded: &AtomicUsize,
        flood_over: &AtomicBool,
    ) {
        match reply {
            Reply::Raw(bytes) => {
                let _ = stream.write_all(&bytes).await;
                let _ = stream.shutdown().await;
            }
            Reply::Flood { total } => {
                let head = b"HTTP/1.1 200 OK\r\nconnection: close\r\n\r\n";
                if stream.write_all(head).await.is_ok() {
                    let block = [b'x'; 16 * 1024];
                    let mut written = 0;
                    while written < total && stream.write_all(&block).await.is_ok() {
                        written += block.len();
                        flooded.store(written, Ordering::SeqCst);
                    }
                }
                flood_over.store(true, Ordering::SeqCst);
            }
            Reply::Stall { claimed } => {
                let head = format!("HTTP/1.1 200 OK\r\ncontent-length: {claimed}\r\n\r\nx");
                let _ = stream.write_all(head.as_bytes()).await;
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        }
    }

    fn transport() -> ReqwestTransport {
        ReqwestTransport::from_builder(ReqwestTransport::builder().no_proxy()).unwrap()
    }

    async fn send(request: HttpRequest) -> Result<HttpResponse> {
        tokio::time::timeout(Duration::from_secs(10), transport().send(&request))
            .await
            .expect("the transport did not finish")
    }

    fn with_auth(url: Url) -> HttpRequest {
        HttpRequest {
            authorization: Some(Token::new("Bearer sekrit")),
            ..HttpRequest::get(url)
        }
    }

    fn redirect_refusal(result: Result<HttpResponse>) -> RedirectRefusal {
        match result {
            Err(Error::Redirect { reason, .. }) => reason,
            other => panic!("expected a refused redirect, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_redirect_to_a_disallowed_host_is_refused_without_contacting_it() {
        let other = serve(|_| raw("200 OK", &[], b"secret")).await;
        let target = other.url("/x").to_string();
        let first = serve(move |_| redirect("302 Found", &target)).await;
        let first_port = first.addr.port();
        let request =
            HttpRequest::get(first.url("/a")).redirects(RedirectPolicy::allow(move |to| {
                to.port() == Some(first_port)
            }));
        assert_eq!(
            redirect_refusal(send(request).await),
            RedirectRefusal::NotAllowed
        );
        assert!(other.seen().is_empty());
    }

    #[tokio::test]
    async fn an_allowed_redirect_is_followed_and_drops_authorization_across_origins() {
        let other = serve(|_| raw("200 OK", &[], b"landed")).await;
        let target = other.url("/x").to_string();
        let first = serve(move |_| redirect("302 Found", &target)).await;
        let request = with_auth(first.url("/a")).redirects(RedirectPolicy::allow(|_| true));
        let response = send(request).await.unwrap();
        assert_eq!(response.body, b"landed");
        assert_eq!(
            first.seen()[0].header("authorization"),
            Some("Bearer sekrit")
        );
        assert_eq!(other.seen()[0].path, "/x");
        assert_eq!(other.seen()[0].header("authorization"), None);
    }

    #[tokio::test]
    async fn api_requests_refuse_a_cross_origin_redirect() {
        let other = serve(|_| raw("200 OK", &[], b"secret")).await;
        let target = other.url("/x").to_string();
        let first = serve(move |_| redirect("307 Temporary Redirect", &target)).await;
        let result = send(with_auth(first.url("/a"))).await;
        assert_eq!(redirect_refusal(result), RedirectRefusal::CrossOrigin);
        assert!(other.seen().is_empty());
    }

    #[tokio::test]
    async fn a_same_origin_redirect_keeps_authorization() {
        let server = serve(|seen| match seen.path.as_str() {
            "/a" => redirect("301 Moved Permanently", "/b"),
            _ => raw("200 OK", &[], b"done"),
        })
        .await;
        let response = send(with_auth(server.url("/a"))).await.unwrap();
        assert_eq!(response.body, b"done");
        let seen = server.seen();
        assert_eq!(seen.len(), 2);
        assert!(seen.iter().all(|s| s.header("authorization").is_some()));
    }

    #[tokio::test]
    async fn the_none_policy_returns_the_redirect() {
        let server = serve(|_| redirect("302 Found", "/b")).await;
        let request = HttpRequest::get(server.url("/a")).redirects(RedirectPolicy::None);
        let response = send(request).await.unwrap();
        assert_eq!(response.status, 302);
        assert_eq!(response.header("location"), Some("/b"));
        assert_eq!(server.seen().len(), 1);
    }

    #[tokio::test]
    async fn redirects_stop_after_the_hop_limit() {
        let server = serve(|_| redirect("302 Found", "/loop")).await;
        let result = send(HttpRequest::get(server.url("/loop"))).await;
        assert_eq!(redirect_refusal(result), RedirectRefusal::TooMany);
        assert_eq!(server.seen().len(), MAX_REDIRECTS + 1);
    }

    #[tokio::test]
    async fn a_body_over_the_cap_aborts_mid_stream() {
        const TOTAL: usize = 256 * 1024 * 1024;
        let server = serve(|_| Reply::Flood { total: TOTAL }).await;
        let request = HttpRequest::get(server.url("/big")).max_body(64 * 1024);
        let result = send(request).await;
        assert!(
            matches!(result, Err(Error::TooLarge { limit: 65536, .. })),
            "{result:?}"
        );
        for _ in 0..100 {
            if server.flood_over.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            server.flood_over.load(Ordering::SeqCst),
            "client kept reading"
        );
        assert!(server.flooded.load(Ordering::SeqCst) < TOTAL);
    }

    #[tokio::test]
    async fn a_content_length_over_the_cap_is_refused_before_the_body() {
        let server = serve(|_| Reply::Stall {
            claimed: 10_000_000,
        })
        .await;
        let request = HttpRequest::get(server.url("/big")).max_body(1024);
        let result = send(request).await;
        assert!(matches!(result, Err(Error::TooLarge { .. })), "{result:?}");
    }

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::best());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    #[tokio::test]
    async fn the_cap_counts_decompressed_bytes() {
        const EXPANDED: usize = 1024 * 1024;
        let compressed = gzip(&vec![0u8; EXPANDED]);
        assert!(compressed.len() < 4096, "the fixture must be small");
        let body = compressed.clone();
        let server = serve(move |_| raw("200 OK", &[("content-encoding", "gzip")], &body)).await;

        let capped = HttpRequest::get(server.url("/zip")).max_body(64 * 1024);
        let result = send(capped).await;
        assert!(matches!(result, Err(Error::TooLarge { .. })), "{result:?}");

        let roomy = HttpRequest::get(server.url("/zip")).max_body(2 * EXPANDED);
        assert_eq!(send(roomy).await.unwrap().body.len(), EXPANDED);
    }

    #[test]
    fn redirects_change_the_method_like_browsers_do() {
        assert_eq!(method_after_redirect(303, Method::Delete), Method::Get);
        assert_eq!(method_after_redirect(302, Method::Post), Method::Get);
        assert_eq!(method_after_redirect(301, Method::Put), Method::Put);
        assert_eq!(method_after_redirect(307, Method::Post), Method::Post);
        assert_eq!(method_after_redirect(308, Method::Patch), Method::Patch);
    }
}
