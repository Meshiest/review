//! Job log fetching that every backend shares: a ranged, uncached GET whose
//! answer becomes a [`LogChunk`].
//!
//! The body is read once, by the transport, under [`LOG_FETCH_MAX`]; a forge
//! that ignores `Range` and sends the whole log has the part before `from`
//! drained in place, so a log is never held twice.

use crate::error::{Error, Result};
use crate::http::{CacheMode, HttpClient, HttpRequest, HttpResponse, RetryMode, Url};
use crate::model::LogChunk;

/// Most bytes one [`crate::Forge::job_log`] call returns; a longer log comes
/// in several calls.
pub const LOG_FETCH_MAX: u64 = 64 * 1024 * 1024;

/// `GET url` for the log from `from` on: `Range`, no ETag cache, the log
/// cap, and a `416` (range past the end) returned instead of failing.
pub(crate) fn request(url: Url, from: u64) -> HttpRequest {
    let last = from.saturating_add(LOG_FETCH_MAX - 1);
    HttpRequest::get(url)
        .header("range", format!("bytes={from}-{last}"))
        .cache(CacheMode::Bypass)
        .max_body(usize::try_from(LOG_FETCH_MAX).unwrap_or(usize::MAX))
        .accept(&[416])
}

/// Sends `build(from)` and maps the answer. `finished` is the job's state,
/// read before the log, so a log fetched after it is the final one.
///
/// While the job is not finished, a missing log is empty so far: Forgejo
/// answers `500` ("file does not exist") until the runner's first lines
/// land, and a forge may `404` before then. Such a read is not retried
/// here, since the tail asks again anyway. A log shorter than `from` was
/// restarted by a re-run and is read again from 0.
pub(crate) async fn fetch(
    http: &HttpClient,
    from: u64,
    finished: bool,
    build: impl Fn(u64) -> Result<HttpRequest>,
) -> Result<LogChunk> {
    let send = |from| -> Result<HttpRequest> {
        let retry = if finished {
            RetryMode::Safe
        } else {
            RetryMode::Never
        };
        Ok(build(from)?.retry(retry))
    };
    let response = match http.send(send(from)?).await {
        Ok(response) => response,
        Err(Error::NotFound { .. }) if !finished => return Ok(empty(from, false)),
        Err(Error::Status { status, .. }) if !finished && status >= 500 => {
            return Ok(empty(from, false));
        }
        Err(err) => return Err(err),
    };
    if response.status == 416 {
        return match content_range(&response).and_then(|range| range.total) {
            Some(total) if total < from => {
                let response = http.send(send(0)?).await?;
                Ok(chunk(response, 0, finished))
            }
            _ => Ok(empty(from, finished)),
        };
    }
    Ok(chunk(response, from, finished))
}

fn empty(at: u64, complete: bool) -> LogChunk {
    LogChunk {
        offset: at,
        bytes: Vec::new(),
        next_offset: at,
        complete,
    }
}

fn chunk(response: HttpResponse, from: u64, finished: bool) -> LogChunk {
    let len = |bytes: &[u8]| u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    let range = (response.status == 206)
        .then(|| content_range(&response))
        .flatten();
    let mut bytes = response.body;
    let (offset, reached_end) = match (response.status, range) {
        (206, Some(range)) => {
            let next = range.start.saturating_add(len(&bytes));
            (
                range.start,
                range
                    .total
                    .map_or(len(&bytes) < LOG_FETCH_MAX, |total| next >= total),
            )
        }
        (206, None) => (from, len(&bytes) < LOG_FETCH_MAX),
        (200, _) if from > len(&bytes) => (0, true),
        (200, _) => {
            bytes.drain(..usize::try_from(from).unwrap_or(usize::MAX));
            (from, true)
        }
        _ => {
            bytes.clear();
            (from, true)
        }
    };
    LogChunk {
        offset,
        next_offset: offset.saturating_add(len(&bytes)),
        bytes,
        complete: finished && reached_end,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ContentRange {
    start: u64,
    /// `None` for `/*` (length not known yet).
    total: Option<u64>,
}

/// `bytes 100-199/1000`, `bytes 100-199/*` or `bytes */1000` (start 0).
fn content_range(response: &HttpResponse) -> Option<ContentRange> {
    let value = response.header("content-range")?.trim();
    let (span, total) = value.strip_prefix("bytes")?.trim().split_once('/')?;
    let total = match total.trim() {
        "*" => None,
        total => Some(total.parse().ok()?),
    };
    let start = match span.trim() {
        "*" => 0,
        span => span.split_once('-')?.0.trim().parse().ok()?,
    };
    Some(ContentRange { start, total })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(status: u16, range: Option<&str>, body: &[u8]) -> HttpResponse {
        HttpResponse {
            status,
            headers: range
                .map(|r| ("content-range".to_owned(), r.to_owned()))
                .into_iter()
                .collect(),
            body: body.to_vec(),
            cached: false,
        }
    }

    #[test]
    fn parses_content_ranges() {
        let parse = |value| content_range(&response(206, Some(value), b""));
        assert_eq!(
            parse("bytes 100-199/1000"),
            Some(ContentRange {
                start: 100,
                total: Some(1000)
            })
        );
        assert_eq!(
            parse("bytes 5-9/*"),
            Some(ContentRange {
                start: 5,
                total: None
            })
        );
        assert_eq!(
            parse("bytes */8747"),
            Some(ContentRange {
                start: 0,
                total: Some(8747)
            })
        );
        assert_eq!(parse("items 1-2/3"), None);
    }

    #[test]
    fn a_partial_answer_continues_where_it_ends() {
        let tail = chunk(response(206, Some("bytes 4-9/10"), b"efghij"), 4, true);
        assert_eq!(
            (tail.offset, tail.next_offset, tail.complete),
            (4, 10, true)
        );
        let middle = chunk(response(206, Some("bytes 4-5/10"), b"ef"), 4, true);
        assert!(!middle.complete, "more of the log is there");
        let running = chunk(response(206, Some("bytes 0-3/4"), b"abcd"), 0, false);
        assert!(!running.complete, "the job is still going");
    }

    #[test]
    fn a_whole_log_is_cut_to_the_offset_in_place() {
        let cut = chunk(response(200, None, b"abcdef"), 2, true);
        assert_eq!(cut.bytes, b"cdef");
        assert_eq!((cut.offset, cut.next_offset), (2, 6));
        let restarted = chunk(response(200, None, b"ab"), 5, false);
        assert_eq!(
            (restarted.offset, restarted.bytes.as_slice()),
            (0, &b"ab"[..])
        );
    }
}
