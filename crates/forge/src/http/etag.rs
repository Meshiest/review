use std::collections::{HashMap, VecDeque};

use super::HttpResponse;

/// Last response per GET URL with its `ETag`, so polling sends
/// `If-None-Match` and a `304` (free against GitHub's quota) is answered from
/// here. Bounded: the oldest entry goes first.
#[derive(Debug)]
pub struct EtagCache {
    capacity: usize,
    entries: HashMap<String, (String, HttpResponse)>,
    order: VecDeque<String>,
}

impl EtagCache {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    #[must_use]
    pub fn etag(&self, url: &str) -> Option<&str> {
        self.entries.get(url).map(|(etag, _)| etag.as_str())
    }

    /// The cached response, marked as served from cache.
    #[must_use]
    pub fn cached(&self, url: &str) -> Option<HttpResponse> {
        self.entries.get(url).map(|(_, response)| HttpResponse {
            cached: true,
            ..response.clone()
        })
    }

    pub fn insert(&mut self, url: String, etag: String, response: HttpResponse) {
        if self.capacity == 0 {
            return;
        }
        if self.entries.insert(url.clone(), (etag, response)).is_some() {
            self.order.retain(|key| *key != url);
        }
        self.order.push_back(url);
        while self.order.len() > self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(body: &str) -> HttpResponse {
        HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: body.as_bytes().to_vec(),
            cached: false,
        }
    }

    #[test]
    fn evicts_the_oldest() {
        let mut cache = EtagCache::new(2);
        cache.insert("a".into(), "1".into(), response("a"));
        cache.insert("b".into(), "2".into(), response("b"));
        cache.insert("a".into(), "3".into(), response("a2"));
        cache.insert("c".into(), "4".into(), response("c"));
        assert!(cache.etag("b").is_none());
        assert_eq!(cache.etag("a"), Some("3"));
        let hit = cache.cached("a").unwrap();
        assert!(hit.cached);
        assert_eq!(hit.body, b"a2");
    }
}
