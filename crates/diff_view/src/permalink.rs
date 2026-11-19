//! Permalinks to lines of a diff on the forge's web UI.

use std::fmt::Write as _;

use diff::Side;
use forge::model::ForgeKind;

/// A line or a range of lines of one side of a file, pinned to a commit so
/// the link keeps pointing at the same text.
///
/// Lines are 0-based like everywhere in the diff model, `end` inclusive; the
/// URL's 1-based numbers are made in [`LineAnchor::forge_url`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineAnchor {
    /// The file's path in the repository, `/`-separated, not encoded.
    pub path: String,
    pub side: Side,
    pub start: u32,
    pub end: u32,
    /// The commit the lines are at: the head for the new side, the base
    /// (merge-base) for the old side. See [`LineAnchor::at_side`].
    pub commit: String,
}

impl LineAnchor {
    /// An anchor on `side`, pinned to `head` for the new side and to `base`
    /// for the old one. `start` and `end` may come in either order.
    #[must_use]
    pub fn at_side(
        path: impl Into<String>,
        side: Side,
        start: u32,
        end: u32,
        head: &str,
        base: &str,
    ) -> Self {
        let commit = match side {
            Side::New => head,
            Side::Old => base,
        };
        Self {
            path: path.into(),
            side,
            start: start.min(end),
            end: start.max(end),
            commit: commit.to_owned(),
        }
    }

    /// The link to the lines in the forge's file view at the commit.
    ///
    /// `web_base` is the forge's site root (`https://github.com`, no
    /// trailing slash needed); `owner` may hold `/`-separated groups
    /// (GitLab subgroups). One line gives `#L{n}`, several the forge's range
    /// form.
    #[must_use]
    pub fn forge_url(&self, kind: ForgeKind, web_base: &str, owner: &str, repo: &str) -> String {
        let (first, last) = (self.start.min(self.end) + 1, self.start.max(self.end) + 1);
        let (view, range_end) = match kind {
            ForgeKind::GitHub => ("blob", format!("L{last}")),
            ForgeKind::GitLab => ("-/blob", last.to_string()),
            ForgeKind::Forgejo => ("src/commit", format!("L{last}")),
        };
        let mut url = String::from(web_base.trim_end_matches('/'));
        url.push('/');
        push_encoded_path(&mut url, owner);
        url.push('/');
        push_encoded_path(&mut url, repo);
        let _ = write!(url, "/{view}/");
        push_encoded_path(&mut url, &self.commit);
        url.push('/');
        push_encoded_path(&mut url, &self.path);
        let _ = write!(url, "#L{first}");
        if last != first {
            let _ = write!(url, "-{range_end}");
        }
        url
    }
}

/// Appends `path` with each `/`-separated segment percent-encoded, so spaces,
/// `#`, `?` and `%` in a name cannot end the path or start the fragment.
fn push_encoded_path(out: &mut String, path: &str) {
    for (ix, segment) in path.split('/').enumerate() {
        if ix > 0 {
            out.push('/');
        }
        for &byte in segment.as_bytes() {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
                out.push(char::from(byte));
            } else {
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn anchor(path: &str, start: u32, end: u32) -> LineAnchor {
        LineAnchor {
            path: path.into(),
            side: Side::New,
            start,
            end,
            commit: SHA.into(),
        }
    }

    #[test]
    fn each_forge_has_its_own_blob_path_and_range_fragment() {
        let a = anchor("src/lib.rs", 9, 19);
        let url = |kind, base| a.forge_url(kind, base, "octo", "repo");
        assert_eq!(
            url(ForgeKind::GitHub, "https://github.com"),
            format!("https://github.com/octo/repo/blob/{SHA}/src/lib.rs#L10-L20")
        );
        assert_eq!(
            url(ForgeKind::GitLab, "https://gitlab.com"),
            format!("https://gitlab.com/octo/repo/-/blob/{SHA}/src/lib.rs#L10-20")
        );
        assert_eq!(
            url(ForgeKind::Forgejo, "https://forge.example/"),
            format!("https://forge.example/octo/repo/src/commit/{SHA}/src/lib.rs#L10-L20")
        );
    }

    #[test]
    fn one_line_has_no_range() {
        let a = anchor("a.rs", 4, 4);
        for kind in ForgeKind::ALL {
            let url = a.forge_url(kind, "https://h", "o", "r");
            assert!(url.ends_with("/a.rs#L5"), "{url}");
        }
    }

    #[test]
    fn paths_are_percent_encoded_by_segment() {
        let a = anchor("my dir/a#b?c%d/é.rs", 0, 1);
        let url = a.forge_url(ForgeKind::GitHub, "https://github.com", "o", "r");
        assert_eq!(
            url,
            format!("https://github.com/o/r/blob/{SHA}/my%20dir/a%23b%3Fc%25d/%C3%A9.rs#L1-L2")
        );
        // Subgroups keep their slashes.
        let url = anchor("x", 0, 0).forge_url(ForgeKind::GitLab, "https://g", "grp/sub grp", "r");
        assert_eq!(url, format!("https://g/grp/sub%20grp/r/-/blob/{SHA}/x#L1"));
    }

    #[test]
    fn the_commit_is_the_head_for_new_lines_and_the_base_for_old_ones() {
        let new = LineAnchor::at_side("a", Side::New, 5, 2, "head", "base");
        assert_eq!((new.commit.as_str(), new.start, new.end), ("head", 2, 5));
        let old = LineAnchor::at_side("a", Side::Old, 2, 5, "head", "base");
        assert_eq!(old.commit, "base");
        assert!(
            old.forge_url(ForgeKind::GitHub, "https://h", "o", "r")
                .contains("/blob/base/a#L3-L6")
        );
    }

    #[test]
    fn reversed_lines_are_put_in_order() {
        let a = anchor("a", 7, 3);
        let url = a.forge_url(ForgeKind::GitLab, "https://h", "o", "r");
        assert!(url.ends_with("#L4-8"), "{url}");
    }
}
