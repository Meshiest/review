//! Account avatars: which URLs may be fetched, the fetch, and the decode.
//!
//! Avatar URLs come from forge data, so they are untrusted. A URL is fetched
//! only when it is `https` and its host is the forge's own, that forge's
//! avatar CDN, or (when the app allows it) Gravatar. Every other URL is
//! refused before any request is made. The request carries no credential:
//! it goes through the shared [`Transport`], not an authorizing client.

use std::io::Cursor;
use std::sync::Arc;

use image::{DynamicImage, ImageFormat, ImageReader, Limits, imageops::FilterType};

use crate::error::{Error, Result};
use crate::http::{HttpRequest, RedirectPolicy, RetryMode, Transport, Url};
use crate::model::ForgeKind;

/// Largest response taken; the transport stops reading at this many bytes.
pub const MAX_BYTES: usize = 512 * 1024;
/// Side of the decoded image, in px. Larger images are scaled down to it.
pub const SIZE: u32 = 64;
/// Widest source image decoded, so a small file cannot expand into a huge buffer.
const MAX_SOURCE_SIDE: u32 = 4096;
const MAX_DECODE_ALLOC: u64 = 64 * 1024 * 1024;

const GITHUB_CDN: &str = "avatars.githubusercontent.com";
const GITLAB_COM: &str = "gitlab.com";
const GITLAB_COM_CDN: &str = "assets.gitlab-static.net";
const GRAVATAR_HOSTS: [&str; 2] = ["secure.gravatar.com", "www.gravatar.com"];

/// A decoded avatar: straight-alpha RGBA, at most [`SIZE`] on a side.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Avatar {
    pub width: u32,
    pub height: u32,
    pub rgba: Arc<[u8]>,
}

impl Avatar {
    /// Decodes PNG, JPEG, GIF (first frame) or WebP, whatever the claimed
    /// content type, and scales anything past [`SIZE`] down to a square.
    ///
    /// # Errors
    /// [`Error::Decode`] when the bytes are too large, another format, or
    /// not a valid image.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let bad = |reason: String| Error::Decode {
            request: "avatar".to_owned(),
            reason,
        };
        if bytes.len() > MAX_BYTES {
            return Err(bad(format!(
                "{} bytes is over the {MAX_BYTES} limit",
                bytes.len()
            )));
        }
        let format = image::guess_format(bytes).map_err(|err| bad(err.to_string()))?;
        if !matches!(
            format,
            ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Gif | ImageFormat::WebP
        ) {
            return Err(bad(format!("{format:?} is not an accepted image type")));
        }
        let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
        let mut limits = Limits::default();
        limits.max_image_width = Some(MAX_SOURCE_SIDE);
        limits.max_image_height = Some(MAX_SOURCE_SIDE);
        limits.max_alloc = Some(MAX_DECODE_ALLOC);
        reader.limits(limits);
        let mut image = reader.decode().map_err(|err| bad(err.to_string()))?;
        if image.width() > SIZE || image.height() > SIZE {
            image = image.resize_to_fill(SIZE, SIZE, FilterType::Triangle);
        }
        let rgba = image.into_rgba8();
        Ok(Self {
            width: rgba.width(),
            height: rgba.height(),
            rgba: rgba.into_raw().into(),
        })
    }

    /// The image as a PNG, for the app's disk cache.
    ///
    /// # Errors
    /// [`Error::Decode`] when the encoder fails.
    pub fn to_png(&self) -> Result<Vec<u8>> {
        let mut out = Cursor::new(Vec::new());
        let buffer = image::RgbaImage::from_raw(self.width, self.height, self.rgba.to_vec())
            .ok_or_else(|| Error::Decode {
                request: "avatar".to_owned(),
                reason: "pixel buffer does not match its size".to_owned(),
            })?;
        DynamicImage::ImageRgba8(buffer)
            .write_to(&mut out, ImageFormat::Png)
            .map_err(|err| Error::Decode {
                request: "avatar".to_owned(),
                reason: err.to_string(),
            })?;
        Ok(out.into_inner())
    }
}

/// Why a URL was not fetched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    NotUrl,
    NotHttps,
    Credentials,
    Host,
    Gravatar,
}

impl Refusal {
    fn error(self, kind: ForgeKind) -> Error {
        Error::Unsupported {
            kind,
            feature: match self {
                Self::NotUrl => "avatar URLs that are not URLs",
                Self::NotHttps => "avatar URLs that are not https",
                Self::Credentials => "avatar URLs with a user name or password",
                Self::Host => "avatars from a host outside the forge and its avatar CDN",
                Self::Gravatar => "Gravatar avatars while they are turned off",
            },
        }
    }
}

/// How an allowed origin takes a size hint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SizeParam {
    /// GitHub's CDN and Gravatar.
    S,
    /// GitLab uploads.
    Width,
    /// Forgejo.
    Size,
}

impl SizeParam {
    fn name(self) -> &'static str {
        match self {
            Self::S => "s",
            Self::Width => "width",
            Self::Size => "size",
        }
    }
}

/// The URL to request for `raw`, with a size hint, or why it must not be.
/// `forge_host` is the configured web host, with its port if it has one.
pub(crate) fn resolve(
    kind: ForgeKind,
    forge_host: &str,
    raw: &str,
    gravatar: bool,
) -> std::result::Result<Url, Refusal> {
    let mut url = Url::parse(raw).map_err(|_| Refusal::NotUrl)?;
    if url.scheme() != "https" {
        return Err(Refusal::NotHttps);
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(Refusal::Credentials);
    }
    let host = url.host_str().ok_or(Refusal::NotUrl)?.to_ascii_lowercase();
    let own = match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.clone(),
    };
    let param = if own.eq_ignore_ascii_case(forge_host) {
        match kind {
            ForgeKind::GitHub => SizeParam::S,
            ForgeKind::GitLab => SizeParam::Width,
            ForgeKind::Forgejo => SizeParam::Size,
        }
    } else if url.port().is_some() {
        return Err(Refusal::Host);
    } else if GRAVATAR_HOSTS.contains(&host.as_str()) {
        if !gravatar {
            return Err(Refusal::Gravatar);
        }
        SizeParam::S
    } else if kind == ForgeKind::GitHub && host == GITHUB_CDN {
        SizeParam::S
    } else if kind == ForgeKind::GitLab
        && forge_host.eq_ignore_ascii_case(GITLAB_COM)
        && host == GITLAB_COM_CDN
    {
        SizeParam::Width
    } else {
        return Err(Refusal::Host);
    };
    set_query(&mut url, param.name(), &SIZE.to_string());
    Ok(url)
}

/// Sets `key` to `value` in the query, replacing any earlier value.
fn set_query(url: &mut Url, key: &str, value: &str) {
    let kept: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(name, _)| name != key)
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect();
    url.query_pairs_mut()
        .clear()
        .extend_pairs(kept)
        .append_pair(key, value);
}

/// Whether `raw` may be fetched.
///
/// # Errors
/// [`Error::Unsupported`] when it is refused.
pub(crate) fn check(kind: ForgeKind, forge_host: &str, raw: &str, gravatar: bool) -> Result<()> {
    resolve(kind, forge_host, raw, gravatar)
        .map(drop)
        .map_err(|refusal| refusal.error(kind))
}

/// Fetches and decodes one avatar. A refused URL makes no request.
pub(crate) async fn fetch(
    transport: &dyn Transport,
    kind: ForgeKind,
    forge_host: &str,
    raw: &str,
    gravatar: bool,
) -> Result<Avatar> {
    let url = resolve(kind, forge_host, raw, gravatar).map_err(|refusal| refusal.error(kind))?;
    // Every redirect hop must pass the same allowlist as the first URL, and
    // the transport stops reading at the cap, so a redirect cannot reach
    // another host or stream an unbounded body into memory.
    let forge_host = forge_host.to_owned();
    let redirects =
        RedirectPolicy::allow(move |to| resolve(kind, &forge_host, to.as_str(), gravatar).is_ok());
    let request = HttpRequest::get(url)
        .header("accept", "image/png,image/jpeg,image/gif,image/webp")
        .retry(RetryMode::Never)
        .max_body(MAX_BYTES)
        .redirects(redirects);
    let describe = request.describe();
    let response = transport.send(&request).await?;
    if !response.is_success() {
        return Err(Error::Status {
            request: describe,
            status: response.status,
            message: "avatar".to_owned(),
        });
    }
    Avatar::decode(&response.body)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;

    use super::*;
    use crate::http::HttpResponse;

    const GH: ForgeKind = ForgeKind::GitHub;
    const GL: ForgeKind = ForgeKind::GitLab;
    const FJ: ForgeKind = ForgeKind::Forgejo;

    fn allowed(kind: ForgeKind, host: &str, url: &str, gravatar: bool) -> Option<String> {
        resolve(kind, host, url, gravatar).ok().map(String::from)
    }

    #[test]
    fn github_allows_its_cdn_and_host_with_a_size_hint() {
        assert_eq!(
            allowed(
                GH,
                "github.com",
                "https://avatars.githubusercontent.com/u/9?v=4",
                false
            )
            .as_deref(),
            Some("https://avatars.githubusercontent.com/u/9?v=4&s=64")
        );
        assert_eq!(
            allowed(
                GH,
                "github.com",
                "https://avatars.githubusercontent.com/u/9?s=400&v=4",
                false
            )
            .as_deref(),
            Some("https://avatars.githubusercontent.com/u/9?v=4&s=64")
        );
        assert!(allowed(GH, "github.com", "https://github.com/octocat.png", false).is_some());
        assert!(
            allowed(
                GH,
                "ghe.example.org",
                "https://ghe.example.org/avatars/u/1",
                false
            )
            .is_some()
        );
    }

    #[test]
    fn gitlab_allows_its_upload_hosts_and_hints_with_width() {
        assert_eq!(
            allowed(
                GL,
                "gitlab.com",
                "https://gitlab.com/uploads/-/system/user/avatar/1/a.png",
                false
            )
            .as_deref(),
            Some("https://gitlab.com/uploads/-/system/user/avatar/1/a.png?width=64")
        );
        assert!(
            allowed(
                GL,
                "gitlab.com",
                "https://assets.gitlab-static.net/uploads/-/a.png",
                false
            )
            .is_some()
        );
        // The CDN belongs to gitlab.com only; a self-managed server has its own host.
        assert!(
            allowed(
                GL,
                "gl.example.org",
                "https://assets.gitlab-static.net/a.png",
                false
            )
            .is_none()
        );
        assert!(
            allowed(
                GL,
                "gl.example.org",
                "https://gl.example.org/uploads/a.png",
                false
            )
            .is_some()
        );
    }

    #[test]
    fn forgejo_allows_only_its_own_host() {
        assert_eq!(
            allowed(
                FJ,
                "git.example.org",
                "https://git.example.org/avatars/abc",
                false
            )
            .as_deref(),
            Some("https://git.example.org/avatars/abc?size=64")
        );
        assert!(
            allowed(
                FJ,
                "git.example.org",
                "https://avatars.githubusercontent.com/u/1",
                false
            )
            .is_none()
        );
        assert!(
            allowed(
                FJ,
                "git.example.org",
                "https://other.example.org/avatars/abc",
                false
            )
            .is_none()
        );
    }

    #[test]
    fn a_cdn_is_not_shared_between_forge_kinds() {
        assert!(
            allowed(
                GL,
                "gitlab.com",
                "https://avatars.githubusercontent.com/u/1",
                false
            )
            .is_none()
        );
        assert!(
            allowed(
                GH,
                "github.com",
                "https://assets.gitlab-static.net/a.png",
                false
            )
            .is_none()
        );
    }

    #[test]
    fn gravatar_needs_the_flag() {
        for host in GRAVATAR_HOSTS {
            let url = format!("https://{host}/avatar/abc?s=80&d=identicon");
            assert_eq!(
                resolve(GL, "gitlab.com", &url, false),
                Err(Refusal::Gravatar)
            );
            let on = allowed(GL, "gitlab.com", &url, true).unwrap();
            assert!(on.contains("d=identicon") && on.ends_with("s=64"), "{on}");
        }
    }

    #[test]
    fn lookalike_and_unsafe_urls_are_refused() {
        let refused = [
            "http://avatars.githubusercontent.com/u/1",
            "https://avatars.githubusercontent.com.evil.example/u/1",
            "https://evilavatars.githubusercontent.com/u/1",
            "https://avatars.githubusercontent.com@evil.example/u/1",
            "https://user:pw@github.com/a.png",
            "https://github.com:8443/a.png",
            "https://evil.example/avatars.githubusercontent.com/u/1",
            "file:///C:/Windows/win.ini",
            "data:image/png;base64,AAAA",
            "//avatars.githubusercontent.com/u/1",
            "/uploads/a.png",
            "",
        ];
        for url in refused {
            assert!(allowed(GH, "github.com", url, true).is_none(), "{url}");
        }
    }

    #[test]
    fn a_port_must_be_the_configured_one() {
        assert!(
            allowed(
                FJ,
                "localhost:3000",
                "https://localhost:3000/avatars/a",
                false
            )
            .is_some()
        );
        assert!(
            allowed(
                FJ,
                "localhost:3000",
                "https://localhost:4000/avatars/a",
                false
            )
            .is_none()
        );
        assert!(allowed(FJ, "localhost:3000", "https://localhost/avatars/a", false).is_none());
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(width, height, image::Rgba([200, 30, 30, 255]));
        let mut out = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(image)
            .write_to(&mut out, ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    struct Fake {
        body: Vec<u8>,
        status: u16,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl Transport for Fake {
        async fn send(&self, _: &HttpRequest) -> Result<HttpResponse> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok(HttpResponse {
                status: self.status,
                headers: Vec::new(),
                body: self.body.clone(),
                cached: false,
            })
        }
    }

    fn fake(body: Vec<u8>, status: u16) -> Fake {
        Fake {
            body,
            status,
            calls: AtomicUsize::new(0),
        }
    }

    const CDN_URL: &str = "https://avatars.githubusercontent.com/u/1?v=4";

    #[tokio::test]
    async fn a_refused_url_sends_no_request() {
        let transport = fake(png(8, 8), 200);
        let result = fetch(
            &transport,
            GH,
            "github.com",
            "https://evil.example/a.png",
            true,
        )
        .await;
        assert!(matches!(result, Err(Error::Unsupported { .. })));
        assert_eq!(transport.calls.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn an_allowed_url_is_fetched_and_decoded() {
        let transport = fake(png(8, 8), 200);
        let avatar = fetch(&transport, GH, "github.com", CDN_URL, false)
            .await
            .unwrap();
        assert_eq!((avatar.width, avatar.height), (8, 8));
        assert_eq!(avatar.rgba.len(), 8 * 8 * 4);
        assert_eq!(transport.calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn a_response_over_the_cap_is_dropped() {
        let mut body = png(8, 8);
        body.resize(MAX_BYTES + 1, 0);
        let transport = fake(body, 200);
        let result = fetch(&transport, GH, "github.com", CDN_URL, false).await;
        assert!(matches!(result, Err(Error::Decode { .. })), "{result:?}");
        let mut at_cap = png(8, 8);
        at_cap.resize(MAX_BYTES, 0);
        // Trailing bytes after a PNG are ignored, so exactly the cap decodes.
        assert!(Avatar::decode(&at_cap).is_ok());
    }

    struct Capture(std::sync::Mutex<Option<HttpRequest>>);

    #[async_trait]
    impl Transport for Capture {
        async fn send(&self, request: &HttpRequest) -> Result<HttpResponse> {
            *self.0.lock().unwrap() = Some(request.clone());
            Ok(HttpResponse {
                status: 200,
                headers: Vec::new(),
                body: png(4, 4),
                cached: false,
            })
        }
    }

    #[tokio::test]
    async fn the_request_carries_the_cap_and_the_allowlist_for_every_hop() {
        let transport = Capture(std::sync::Mutex::default());
        fetch(&transport, GH, "github.com", CDN_URL, false)
            .await
            .unwrap();
        let request = transport.0.lock().unwrap().take().unwrap();
        assert_eq!(request.max_body, MAX_BYTES);
        let hop = |to: &str, from: &Url| match request.redirects.next(from, to, 0) {
            Ok(next) => next.is_some(),
            Err(_) => false,
        };
        assert!(hop("https://github.com/octocat.png", &request.url));
        assert!(!hop("https://evil.example/a.png", &request.url));
        assert!(!hop(
            "http://avatars.githubusercontent.com/u/1",
            &request.url
        ));
    }

    #[tokio::test]
    async fn an_error_status_is_an_error() {
        let transport = fake(Vec::new(), 404);
        let result = fetch(&transport, GH, "github.com", CDN_URL, false).await;
        assert!(matches!(result, Err(Error::Status { status: 404, .. })));
    }

    #[test]
    fn only_png_jpeg_gif_and_webp_decode() {
        assert!(Avatar::decode(&png(4, 4)).is_ok());
        assert!(Avatar::decode(b"GIF89a\x01\x00\x01\x00").is_err());
        assert!(
            Avatar::decode(
                b"BM\x1e\0\0\0\0\0\0\0\x1a\0\0\0\x0c\0\0\0\x01\0\x01\0\x01\0\x18\0\0\0\xff\0"
            )
            .is_err()
        );
        assert!(Avatar::decode(b"<svg xmlns='http://www.w3.org/2000/svg'/>").is_err());
        assert!(Avatar::decode(b"").is_err());
    }

    #[test]
    fn large_images_are_scaled_to_a_square() {
        let avatar = Avatar::decode(&png(400, 200)).unwrap();
        assert_eq!((avatar.width, avatar.height), (SIZE, SIZE));
        let small = Avatar::decode(&png(20, 30)).unwrap();
        assert_eq!((small.width, small.height), (20, 30));
    }

    #[test]
    fn png_round_trip_keeps_pixels() {
        let avatar = Avatar::decode(&png(10, 10)).unwrap();
        let again = Avatar::decode(&avatar.to_png().unwrap()).unwrap();
        assert_eq!(avatar, again);
    }
}
