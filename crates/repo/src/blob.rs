use std::sync::Arc;

use crate::error::{Error, Result};

/// Git's own heuristic: a NUL byte in the first 8000 bytes means binary.
const BINARY_PROBE: usize = 8000;

/// Blob bytes shared by reference, with the byte offset of each line start.
///
/// Cloning is two reference-count bumps. Lines are slices of the buffer and
/// keep their line ending; the line table matches `diff::Text` (no empty
/// line after a final newline).
#[derive(Clone, Debug, Default)]
pub struct Blob {
    bytes: Arc<[u8]>,
    starts: Arc<[u32]>,
}

impl Blob {
    /// # Errors
    /// If the blob is 4 GiB or larger.
    pub fn new(bytes: impl Into<Arc<[u8]>>) -> Result<Self> {
        let bytes = bytes.into();
        if u32::try_from(bytes.len()).is_err() {
            return Err(Error::BlobTooLarge(bytes.len() as u64));
        }
        let starts = line_starts(&bytes);
        Ok(Self {
            bytes,
            starts: starts.into(),
        })
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    #[must_use]
    pub fn shared_bytes(&self) -> &Arc<[u8]> {
        &self.bytes
    }

    /// Byte offset of each line's first byte.
    #[must_use]
    pub fn line_starts(&self) -> &[u32] {
        &self.starts
    }

    #[must_use]
    pub fn shared_line_starts(&self) -> &Arc<[u32]> {
        &self.starts
    }

    #[must_use]
    pub fn line_count(&self) -> u32 {
        self.starts.len() as u32
    }

    /// Line `ix` including its `\n` or `\r\n`, if it has one.
    ///
    /// # Panics
    /// If `ix` is out of range.
    #[must_use]
    pub fn line_with_eol(&self, ix: u32) -> &[u8] {
        let ix = ix as usize;
        let start = self.starts[ix] as usize;
        let end = self
            .starts
            .get(ix + 1)
            .map_or(self.bytes.len(), |&s| s as usize);
        &self.bytes[start..end]
    }

    /// Line `ix` without its line ending.
    ///
    /// # Panics
    /// If `ix` is out of range.
    #[must_use]
    pub fn line(&self, ix: u32) -> &[u8] {
        let line = self.line_with_eol(ix);
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        line.strip_suffix(b"\r").unwrap_or(line)
    }

    #[must_use]
    pub fn ends_with_newline(&self) -> bool {
        self.bytes.last() == Some(&b'\n')
    }

    #[must_use]
    pub fn is_binary(&self) -> bool {
        is_binary(&self.bytes)
    }

    /// The blob as `diff::Text`, which holds a `str`. Copies the bytes once;
    /// `None` when they are not UTF-8.
    #[must_use]
    pub fn to_text(&self) -> Option<diff::Text> {
        std::str::from_utf8(&self.bytes).ok().map(diff::Text::new)
    }
}

pub(crate) fn is_binary(bytes: &[u8]) -> bool {
    memchr::memchr(0, &bytes[..bytes.len().min(BINARY_PROBE)]).is_some()
}

/// Offsets fit in `u32`: the caller checked the length.
fn line_starts(bytes: &[u8]) -> Vec<u32> {
    let mut starts = Vec::with_capacity(bytes.len() / 40 + 1);
    if !bytes.is_empty() {
        starts.push(0);
    }
    starts.extend(
        memchr::memchr_iter(b'\n', bytes)
            .map(|ix| ix + 1)
            .filter(|&start| start < bytes.len())
            .map(|start| start as u32),
    );
    starts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blob(bytes: &[u8]) -> Blob {
        Blob::new(bytes).unwrap()
    }

    #[test]
    fn lines_match_diff_text() {
        let b = blob(b"a\r\nbc\n\nd");
        assert_eq!(b.line_count(), 4);
        let lines: Vec<_> = (0..b.line_count()).map(|ix| b.line(ix)).collect();
        assert_eq!(lines, [&b"a"[..], b"bc", b"", b"d"]);
        assert_eq!(b.line_with_eol(0), b"a\r\n");
    }

    #[test]
    fn trailing_newline_adds_no_line() {
        assert_eq!(blob(b"a\nb\n").line_count(), 2);
        assert_eq!(blob(b"").line_count(), 0);
        assert_eq!(blob(b"\n").line_count(), 1);
        assert!(blob(b"x\n").ends_with_newline());
        assert!(!blob(b"x").ends_with_newline());
    }

    #[test]
    fn binary_probe() {
        assert!(blob(b"ab\0c").is_binary());
        assert!(!blob(b"text").is_binary());
        let mut late = vec![b'a'; BINARY_PROBE];
        late.push(0);
        assert!(!blob(&late).is_binary());
    }
}
