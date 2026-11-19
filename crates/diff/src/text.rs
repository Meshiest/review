use std::borrow::Cow;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Identity of a blob, used to cache work done on it (highlights).
///
/// Callers that have a git object id pass it through [`BlobId::from_oid`], so
/// the same blob in two diffs shares its cached results. [`Text::new`] mints a
/// process-unique id instead.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlobId(u128);

impl BlobId {
    /// An id no other call has returned.
    #[must_use]
    pub fn unique() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(u128::from(NEXT.fetch_add(1, Ordering::Relaxed)) | UNIQUE_BIT)
    }

    /// The id of a git object id (the first 16 bytes of it).
    #[must_use]
    pub fn from_oid(oid: &[u8]) -> Self {
        let mut bytes = [0u8; 16];
        let n = oid.len().min(16);
        bytes[..n].copy_from_slice(&oid[..n]);
        Self(u128::from_be_bytes(bytes) & !UNIQUE_BIT)
    }
}

/// Keeps minted ids apart from object-id ones.
const UNIQUE_BIT: u128 = 1 << 127;

/// One blob's bytes with a table of line starts. Lines are slices of the
/// blob; nothing is copied per line, and cloning shares the blob.
///
/// A blob need not be UTF-8: lines that are not show replacement characters
/// (see [`Text::line`]), and highlighting skips the blob.
#[derive(Clone, Debug, Default)]
pub struct Text {
    id: BlobId,
    bytes: Arc<[u8]>,
    /// Byte offset of each line's first byte.
    starts: Arc<[u32]>,
    utf8: bool,
    crlf: bool,
}

impl Text {
    /// # Panics
    /// If the blob is 4 GiB or larger.
    #[must_use]
    pub fn new(blob: impl Into<Arc<str>>) -> Self {
        Self::with_id(BlobId::unique(), blob)
    }

    /// # Panics
    /// If the blob is 4 GiB or larger.
    #[must_use]
    pub fn with_id(id: BlobId, blob: impl Into<Arc<str>>) -> Self {
        let bytes = Arc::<[u8]>::from(blob.into());
        assert!(u32::try_from(bytes.len()).is_ok(), "blob over 4 GiB");
        let starts = line_starts(&bytes);
        Self {
            id,
            crlf: first_line_is_crlf(&bytes),
            bytes,
            starts: starts.into(),
            utf8: true,
        }
    }

    /// A blob from shared bytes and their line table, without copying either.
    /// The bytes may be anything; see [`Text::is_utf8`].
    ///
    /// `line_starts` is the byte offset of each line's first byte: strictly
    /// increasing, each inside the blob, with no entry for an empty line after
    /// a final newline (and none at all for an empty blob), as [`Text::new`]
    /// builds it.
    ///
    /// # Panics
    /// If `line_starts` does not have that shape.
    #[must_use]
    pub fn from_parts(bytes: Arc<[u8]>, line_starts: Arc<[u32]>) -> Self {
        Self::from_parts_with_id(BlobId::unique(), bytes, line_starts)
    }

    /// [`Text::from_parts`] with the id of the blob, so its highlights are
    /// shared by every diff that shows it.
    ///
    /// # Panics
    /// If `line_starts` does not have the shape [`Text::from_parts`] needs.
    #[must_use]
    pub fn from_parts_with_id(id: BlobId, bytes: Arc<[u8]>, line_starts: Arc<[u32]>) -> Self {
        let shaped = match (line_starts.first(), line_starts.last()) {
            (Some(&first), Some(&last)) => first == 0 && (last as usize) < bytes.len(),
            _ => bytes.is_empty(),
        };
        assert!(
            shaped && line_starts.windows(2).all(|w| w[0] < w[1]),
            "line starts must be increasing offsets inside the blob"
        );
        let utf8 = std::str::from_utf8(&bytes).is_ok();
        Self {
            id,
            crlf: first_line_is_crlf(&bytes),
            bytes,
            starts: line_starts,
            utf8,
        }
    }

    pub fn id(&self) -> BlobId {
        self.id
    }

    pub fn line_count(&self) -> u32 {
        self.starts.len() as u32
    }

    /// The line without its `\n` or `\r\n`, as text: bytes that are not UTF-8
    /// become U+FFFD. Borrowed unless that happens.
    ///
    /// # Panics
    /// If `ix` is out of range.
    pub fn line(&self, ix: u32) -> Cow<'_, str> {
        String::from_utf8_lossy(self.line_bytes(ix))
    }

    /// The line's bytes without its `\n` or `\r\n`.
    ///
    /// # Panics
    /// If `ix` is out of range.
    pub fn line_bytes(&self, ix: u32) -> &[u8] {
        let ix = ix as usize;
        let start = self.starts[ix] as usize;
        let end = self
            .starts
            .get(ix + 1)
            .map_or(self.bytes.len(), |&s| s as usize);
        let line = &self.bytes[start..end];
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        line.strip_suffix(b"\r").unwrap_or(line)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Whether every byte of the blob is valid UTF-8.
    pub fn is_utf8(&self) -> bool {
        self.utf8
    }

    /// Whether the blob's lines end in CR LF, judged by its first line
    /// break; what a copy of several lines joins them with.
    pub fn uses_crlf(&self) -> bool {
        self.crlf
    }

    /// Byte offset of each line's first byte.
    pub(crate) fn line_starts(&self) -> &[u32] {
        &self.starts
    }
}

fn first_line_is_crlf(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .position(|&b| b == b'\n')
        .is_some_and(|at| at > 0 && bytes[at - 1] == b'\r')
}

/// Offsets fit in `u32`: the caller checked the length.
fn line_starts(bytes: &[u8]) -> Vec<u32> {
    let mut starts = Vec::with_capacity(bytes.len() / 40 + 1);
    if !bytes.is_empty() {
        starts.push(0);
    }
    starts.extend(
        bytes
            .iter()
            .enumerate()
            .filter(|&(_, &b)| b == b'\n')
            .map(|(ix, _)| ix as u32 + 1)
            .filter(|&start| (start as usize) < bytes.len()),
    );
    starts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines() {
        let text = Text::new("a\r\nbc\n\nd");
        assert_eq!(text.line_count(), 4);
        let lines: Vec<_> = (0..text.line_count()).map(|ix| text.line(ix)).collect();
        assert_eq!(lines, ["a", "bc", "", "d"]);
    }

    #[test]
    fn trailing_newline_adds_no_line() {
        assert_eq!(Text::new("a\nb\n").line_count(), 2);
        assert_eq!(Text::new("").line_count(), 0);
        assert_eq!(Text::new("\n").line_count(), 1);
    }

    #[test]
    fn the_first_line_break_decides_the_line_ending() {
        assert!(Text::new("a\r\nb\n").uses_crlf());
        assert!(!Text::new("a\nb\r\n").uses_crlf());
        assert!(!Text::new("a").uses_crlf());
        assert!(!Text::new("").uses_crlf());
        assert!(Text::new("\r\n").uses_crlf());
        let made = Text::new("x\r\ny");
        let starts = Arc::from(made.line_starts().to_vec());
        assert!(Text::from_parts(Arc::from(&b"x\r\ny"[..]), starts).uses_crlf());
    }

    #[test]
    fn ids() {
        let a = Text::new("x");
        assert_eq!(a.id(), a.clone().id());
        assert_ne!(a.id(), Text::new("x").id());
        let oid = [7u8; 20];
        assert_eq!(BlobId::from_oid(&oid), BlobId::from_oid(&oid));
        assert_ne!(BlobId::from_oid(&oid), BlobId::unique());
    }

    #[test]
    fn from_parts_matches_new() {
        for source in ["a\r\nbc\n\nd", "a\nb\n", "", "\n", "x"] {
            let made = Text::new(source);
            let parts = Text::from_parts(
                Arc::from(source.as_bytes()),
                Arc::from(made.line_starts().to_vec()),
            );
            assert_eq!(parts.line_count(), made.line_count(), "{source:?}");
            assert!(parts.is_utf8());
            for ix in 0..made.line_count() {
                assert_eq!(parts.line(ix), made.line(ix));
            }
        }
    }

    #[test]
    fn non_utf8_lines_are_lossy_only_when_read() {
        let bytes: Arc<[u8]> = Arc::from(&b"ok\nbad \xff\xfe!\r\ncaf\xc3\xa9\n"[..]);
        let starts: Arc<[u32]> = Arc::from([0, 3, 12]);
        let text = Text::from_parts(bytes.clone(), starts);
        assert!(!text.is_utf8());
        assert_eq!(text.as_bytes(), &bytes[..]);
        assert_eq!(text.line(0), "ok");
        assert_eq!(text.line(1), "bad \u{fffd}\u{fffd}!");
        assert_eq!(text.line(2), "caf\u{e9}");
        assert_eq!(text.line_bytes(1), b"bad \xff\xfe!");
        assert!(matches!(text.line(0), Cow::Borrowed(_)));
    }

    #[test]
    #[should_panic(expected = "line starts")]
    fn from_parts_rejects_a_bad_table() {
        let _ = Text::from_parts(Arc::from(&b"a\nb"[..]), Arc::from([0, 9]));
    }
}
