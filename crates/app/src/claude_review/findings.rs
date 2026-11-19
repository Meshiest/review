//! Where a finding the agent reports goes in the diff: only a line the diff
//! shows takes a comment, so a finding on another line or on a file the pull
//! request does not change is refused with the reason, which the agent reads.

use forge::model::{LineRef, Side};
use store::DraftAnchor;

/// One comment Claude suggests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub path: String,
    /// 1-based, on `side`.
    pub line: u32,
    /// The first line of a range ending at `line`.
    pub start: Option<u32>,
    pub side: Side,
    pub body: String,
}

/// Why a finding did not become a draft.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dropped {
    /// The pull request does not change that file.
    NotInDiff,
    /// The diff does not show that line on that side.
    LineNotShown,
    /// The user already has a draft on that line.
    DraftThere,
    /// The body looks like it carries a credential: a pull request can ask
    /// Claude to copy one into a comment, so such a finding is never drafted.
    LooksSecret,
}

impl Dropped {
    /// What the agent is told, for `finding`.
    #[must_use]
    pub fn explain(self, finding: &Finding) -> String {
        let at = format!("{}:{}", finding.path, finding.line);
        match self {
            Self::NotInDiff => format!(
                "Not added: the pull request does not change {}. Comment only on files in the diff.",
                finding.path
            ),
            Self::LineNotShown => format!(
                "Not added: the diff does not show {at} on the {} side. Use a line of a hunk:                  an added or unchanged line on the new side, a removed line on the old side.",
                side_word(finding.side)
            ),
            Self::DraftThere => format!(
                "Not added: there is already a draft on {at}. Put everything about that line                  in one comment, or pick another line."
            ),
            Self::LooksSecret => {
                "Not added: the body looks like it contains a credential.".to_owned()
            }
        }
    }
}

fn side_word(side: Side) -> &'static str {
    match side {
        Side::New => "new",
        Side::Old => "old",
    }
}

/// Where `finding` goes in `diff`: a line (or range) the diff shows on a
/// file it changes, with the body; or why it cannot go anywhere.
///
/// # Errors
/// The reason it takes no draft.
pub fn anchor(finding: &Finding, diff: &diff::Diff) -> Result<DraftAnchor, Dropped> {
    if looks_secret(&finding.body) {
        return Err(Dropped::LooksSecret);
    }
    let file = diff
        .files
        .iter()
        .find(|file| file.path == finding.path)
        .ok_or(Dropped::NotInDiff)?;
    let side = match finding.side {
        Side::Old => diff::Side::Old,
        Side::New => diff::Side::New,
    };
    let shown = |line: u32| {
        line.checked_sub(1)
            .is_some_and(|index| file.shows(side, index))
    };
    if !shown(finding.line) || finding.start.is_some_and(|start| !shown(start)) {
        return Err(Dropped::LineNotShown);
    }
    let line = |line| LineRef {
        side: finding.side,
        line,
    };
    Ok(DraftAnchor::Line {
        path: finding.path.clone(),
        line: line(finding.line),
        start: finding
            .start
            .filter(|start| *start < finding.line)
            .map(line),
    })
}

/// Shortest run of token characters after a known prefix that counts as a
/// token rather than a mention of the prefix.
const TOKEN_TAIL: usize = 16;

/// Whether `text` holds something shaped like a credential: a forge token
/// (a known prefix and a token-length tail), a PEM private key or an AWS
/// access key id.
#[must_use]
pub fn looks_secret(text: &str) -> bool {
    let token_char = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    let has_tail = |rest: &str| rest.chars().take_while(|&c| token_char(c)).count() >= TOKEN_TAIL;
    let prefixed = forge::record::TOKEN_PREFIXES.iter().any(|prefix| {
        text.match_indices(prefix)
            .any(|(at, _)| has_tail(&text[at + prefix.len()..]))
    });
    let aws = text.match_indices("AKIA").any(|(at, _)| {
        let id = &text[at + 4..];
        id.chars()
            .take_while(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
            .count()
            >= 16
    });
    prefixed || aws || (text.contains("-----BEGIN") && text.contains("PRIVATE KEY"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use diff::{FileDiff, Hunk, Segment, Text};

    fn finding(path: &str, line: u32, side: Side, body: &str) -> Finding {
        Finding {
            path: path.into(),
            line,
            start: None,
            side,
            body: body.into(),
        }
    }

    /// `src/a.rs`: old lines 1-10 and new lines 1-11; the hunk shows old
    /// lines 3-6 and new lines 3-7.
    fn diff() -> diff::Diff {
        let text = |n: u32| Text::new((1..=n).map(|i| format!("{i}\n")).collect::<String>());
        diff::Diff {
            files: vec![FileDiff {
                path: "src/a.rs".into(),
                old: text(10),
                new: text(11),
                hunks: vec![Hunk {
                    old_start: 2,
                    new_start: 2,
                    segments: vec![
                        Segment::Context(1),
                        Segment::Change {
                            removed: 2,
                            added: 3,
                        },
                        Segment::Context(1),
                    ],
                }],
            }],
            blocks: Vec::new(),
        }
    }

    #[test]
    fn only_lines_the_diff_shows_take_a_draft() {
        let line = |side, line, start: Option<u32>| DraftAnchor::Line {
            path: "src/a.rs".into(),
            line: LineRef { side, line },
            start: start.map(|line| LineRef { side, line }),
        };
        let diff = diff();
        let at = |finding: Finding| anchor(&finding, &diff);
        assert_eq!(
            at(finding("src/a.rs", 4, Side::New, "x")),
            Ok(line(Side::New, 4, None))
        );
        assert_eq!(
            at(finding("src/a.rs", 6, Side::Old, "x")),
            Ok(line(Side::Old, 6, None))
        );
        let range = Finding {
            start: Some(3),
            ..finding("src/a.rs", 5, Side::New, "x")
        };
        assert_eq!(at(range), Ok(line(Side::New, 5, Some(3))));
        let outside = Finding {
            start: Some(1),
            ..finding("src/a.rs", 5, Side::New, "x")
        };
        assert_eq!(at(outside), Err(Dropped::LineNotShown));
        for (finding, why) in [
            (
                finding("src/a.rs", 9, Side::New, "x"),
                Dropped::LineNotShown,
            ),
            (
                finding("src/a.rs", 0, Side::New, "x"),
                Dropped::LineNotShown,
            ),
            (
                finding("src/other.rs", 1, Side::New, "x"),
                Dropped::NotInDiff,
            ),
        ] {
            assert_eq!(at(finding.clone()), Err(why));
            assert!(why.explain(&finding).starts_with("Not added"));
        }
    }

    #[test]
    fn findings_that_carry_a_credential_are_never_drafted() {
        let token = format!("Leaked: {}{}", "ghp_", "a1B2c3D4e5F6g7H8i9J0");
        assert!(looks_secret(&token));
        assert!(looks_secret("key AKIAIOSFODNN7EXAMPLE here"));
        assert!(looks_secret(
            "-----BEGIN OPENSSH PRIVATE KEY-----
abc"
        ));
        assert!(!looks_secret("Do not hardcode ghp_ tokens in tests."));
        assert!(!looks_secret("Use AKIA-style ids only in docs."));

        let diff = diff::Diff::default();
        assert_eq!(
            anchor(&finding("src/a.rs", 1, Side::New, &token), &diff),
            Err(Dropped::LooksSecret)
        );
    }
}
