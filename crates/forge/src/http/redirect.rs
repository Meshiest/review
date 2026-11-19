//! Which redirects a request may follow. [`super::ReqwestTransport`] follows
//! them itself and asks [`RedirectPolicy::next`] before every hop.

use std::fmt;
use std::sync::Arc;

use reqwest::Url;

use crate::error::RedirectRefusal;

/// Redirects followed before the request fails.
pub const MAX_REDIRECTS: usize = 5;

/// A predicate over redirect targets, checked on every hop.
#[derive(Clone)]
pub struct UrlFilter(Arc<dyn Fn(&Url) -> bool + Send + Sync>);

impl UrlFilter {
    #[must_use]
    pub fn new(allow: impl Fn(&Url) -> bool + Send + Sync + 'static) -> Self {
        Self(Arc::new(allow))
    }

    fn allows(&self, url: &Url) -> bool {
        (self.0)(url)
    }
}

impl fmt::Debug for UrlFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("UrlFilter(..)")
    }
}

#[derive(Clone, Debug, Default)]
pub enum RedirectPolicy {
    /// A redirect is not followed: the 3xx response is returned as is.
    None,
    /// Only to the same scheme, host and port. The default, so a credential
    /// never travels to another origin.
    #[default]
    SameOrigin,
    /// To any target the filter accepts, on every hop. The `Authorization`
    /// header still never crosses origins.
    Allow(UrlFilter),
}

impl RedirectPolicy {
    #[must_use]
    pub fn allow(allow: impl Fn(&Url) -> bool + Send + Sync + 'static) -> Self {
        Self::Allow(UrlFilter::new(allow))
    }

    /// The URL of the next hop from `from` (where `hops` redirects were
    /// already followed), `Ok(None)` when redirects are off, or why the hop
    /// is refused.
    pub(crate) fn next(
        &self,
        from: &Url,
        location: &str,
        hops: usize,
    ) -> Result<Option<Url>, RedirectRefusal> {
        if matches!(self, Self::None) {
            return Ok(None);
        }
        if hops >= MAX_REDIRECTS {
            return Err(RedirectRefusal::TooMany);
        }
        let to = from
            .join(location)
            .ok()
            .filter(|to| matches!(to.scheme(), "http" | "https") && to.host().is_some())
            .ok_or(RedirectRefusal::BadLocation)?;
        if from.scheme() == "https" && to.scheme() == "http" {
            return Err(RedirectRefusal::Downgrade);
        }
        let refusal = match self {
            Self::None => return Ok(None),
            Self::SameOrigin => (!same_origin(from, &to)).then_some(RedirectRefusal::CrossOrigin),
            Self::Allow(filter) => (!filter.allows(&to)).then_some(RedirectRefusal::NotAllowed),
        };
        refusal.map_or(Ok(Some(to)), Err)
    }
}

/// Whether both URLs share scheme, host and port.
pub(crate) fn same_origin(a: &Url, b: &Url) -> bool {
    a.origin() == b.origin()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    #[test]
    fn same_origin_follows_paths_and_refuses_other_hosts_ports_and_schemes() {
        let policy = RedirectPolicy::SameOrigin;
        let from = url("https://api.example.org/a");
        let next = |location| policy.next(&from, location, 0);
        assert_eq!(
            next("/b?x=1").unwrap().unwrap().as_str(),
            "https://api.example.org/b?x=1"
        );
        assert_eq!(
            next("https://other.example.org/b"),
            Err(RedirectRefusal::CrossOrigin)
        );
        assert_eq!(
            next("https://api.example.org:8443/b"),
            Err(RedirectRefusal::CrossOrigin)
        );
        assert_eq!(
            next("http://api.example.org/b"),
            Err(RedirectRefusal::Downgrade)
        );
        assert_eq!(
            next("file:///etc/passwd"),
            Err(RedirectRefusal::BadLocation)
        );
    }

    #[test]
    fn none_returns_the_redirect_and_allow_asks_the_filter() {
        let from = url("https://a.example.org/");
        assert_eq!(
            RedirectPolicy::None.next(&from, "https://b.example.org/", 0),
            Ok(None)
        );
        let policy = RedirectPolicy::allow(|to| to.host_str() == Some("b.example.org"));
        assert!(policy.next(&from, "https://b.example.org/x", 0).is_ok());
        assert_eq!(
            policy.next(&from, "https://c.example.org/x", 0),
            Err(RedirectRefusal::NotAllowed)
        );
    }

    #[test]
    fn the_hop_limit_applies_to_every_policy_but_none() {
        let from = url("https://a.example.org/");
        let policy = RedirectPolicy::SameOrigin;
        assert!(policy.next(&from, "/x", MAX_REDIRECTS - 1).is_ok());
        assert_eq!(
            policy.next(&from, "/x", MAX_REDIRECTS),
            Err(RedirectRefusal::TooMany)
        );
    }
}
