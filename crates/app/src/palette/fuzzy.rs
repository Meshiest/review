//! Fuzzy matching for the palette: every character of the query, in order,
//! ignoring case and spaces. A match scores higher where its characters
//! start words and follow one another, and lower for every character it
//! skips, so "nf" ranks "Next file" over "Unified".

/// Every matched character.
const MATCHED: i32 = 1;
/// A character right after the one matched before it.
const RUN: i32 = 5;
/// A character that starts a word: the first, one after a separator, or an
/// upper-case letter after a lower-case one.
const WORD_START: i32 = 8;
/// Taken off for each character skipped between two matched ones.
const GAP: i32 = 1;
/// The most a match loses for where its first character is.
const MAX_LEAD: i32 = 8;

/// How well a query matched a text, and where.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Match {
    /// Higher is better.
    pub score: i32,
    /// The byte offset in the text of each matched character.
    pub positions: Vec<usize>,
}

fn is_separator(ch: char) -> bool {
    ch.is_whitespace()
        || matches!(
            ch,
            '/' | '\\' | '-' | '_' | '.' | '#' | '!' | ':' | '(' | ','
        )
}

fn fold(ch: char) -> char {
    ch.to_lowercase().next().unwrap_or(ch)
}

/// The best match of `query` in `text`, or `None` when some character of
/// the query is missing. An empty query matches everything with score 0.
#[must_use]
pub fn fuzzy(query: &str, text: &str) -> Option<Match> {
    let query: Vec<char> = query
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .map(fold)
        .collect();
    if query.is_empty() {
        return Some(Match {
            score: 0,
            positions: Vec::new(),
        });
    }
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let folded: Vec<char> = chars.iter().map(|&(_, ch)| fold(ch)).collect();
    // Most texts fail here, before the scoring table is built.
    let mut next = 0;
    for &wanted in &query {
        next = next + folded[next..].iter().position(|&ch| ch == wanted)? + 1;
    }
    let bonus: Vec<i32> = (0..chars.len())
        .map(|at| {
            let start = at == 0 || {
                let (before, now) = (chars[at - 1].1, chars[at].1);
                is_separator(before) || (before.is_lowercase() && now.is_uppercase())
            };
            MATCHED + if start { WORD_START } else { 0 }
        })
        .collect();
    let width = chars.len();
    // best[i * width + j]: the best score with query[i] on text[j], and
    // where query[i - 1] went for it.
    let mut best: Vec<Option<(i32, usize)>> = vec![None; query.len() * width];
    for (i, &wanted) in query.iter().enumerate() {
        // The best `score + GAP * k` of the previous row over k < j - 1,
        // so a gap costs GAP per skipped character.
        let mut far: Option<(i32, usize)> = None;
        for j in 0..width {
            if i > 0 && j >= 2 {
                let k = j - 2;
                if let Some((score, _)) = best[(i - 1) * width + k] {
                    let value = score + GAP * k as i32;
                    if far.is_none_or(|(held, _)| value > held) {
                        far = Some((value, k));
                    }
                }
            }
            if folded[j] != wanted {
                continue;
            }
            let here = if i == 0 {
                Some((bonus[j] - (j as i32).min(MAX_LEAD), usize::MAX))
            } else {
                let adjacent = j
                    .checked_sub(1)
                    .and_then(|k| best[(i - 1) * width + k].map(|(s, _)| (s + RUN, k)));
                let gapped = far.map(|(value, k)| (value - GAP * (j as i32 - 1), k));
                match (adjacent, gapped) {
                    (Some(a), Some(g)) => Some(if a.0 >= g.0 { a } else { g }),
                    (a, g) => a.or(g),
                }
                .map(|(score, k)| (score + bonus[j], k))
            };
            best[i * width + j] = here;
        }
    }
    let last = query.len() - 1;
    let (mut at, (score, _)) = (0..width)
        .filter_map(|j| best[last * width + j].map(|cell| (j, cell)))
        .max_by_key(|&(j, (score, _))| (score, std::cmp::Reverse(j)))?;
    let mut positions = vec![0; query.len()];
    for i in (0..query.len()).rev() {
        positions[i] = chars[at].0;
        if i > 0 {
            at = best[i * width + at].map(|(_, from)| from)?;
        }
    }
    Some(Match { score, positions })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matched<'a>(query: &str, text: &'a str) -> Vec<&'a str> {
        let found = fuzzy(query, text).expect("a match");
        found
            .positions
            .iter()
            .map(|&at| {
                let len = text[at..].chars().next().map_or(0, char::len_utf8);
                &text[at..at + len]
            })
            .collect()
    }

    fn score(query: &str, text: &str) -> i32 {
        fuzzy(query, text).expect("a match").score
    }

    #[test]
    fn every_query_character_must_appear_in_order() {
        assert!(fuzzy("xyz", "Next file").is_none());
        assert!(fuzzy("fn", "Next file").is_none());
        assert!(fuzzy("nf", "Next file").is_some());
        assert_eq!(fuzzy("", "anything").map(|m| m.score), Some(0));
    }

    #[test]
    fn case_and_spaces_are_ignored() {
        assert_eq!(matched("next FILE", "Next file").len(), 8);
        assert!(fuzzy("SPLIT", "Unified or split diff").is_some());
    }

    #[test]
    fn the_match_prefers_word_starts_and_runs() {
        let text = "Unified or split diff";
        let found = fuzzy("split", text).expect("a match");
        assert_eq!(found.positions, (11..16).collect::<Vec<_>>());
        // "sd": the d of "diff", not the one in "sidebar".
        let found = fuzzy("sd", "sidebar diff").expect("a match");
        assert_eq!(found.positions, vec![0, 8]);
    }

    #[test]
    fn word_start_matches_rank_first() {
        assert!(score("nf", "Next file") > score("nf", "Unified"));
        assert!(score("pr", "pull request") > score("pr", "the preview"));
        assert!(score("tab", "tab group") > score("tab", "the tab"));
    }

    #[test]
    fn positions_are_byte_offsets() {
        let found = fuzzy("é", "café au lait").expect("a match");
        assert_eq!(found.positions, vec![3]);
        let found = fuzzy("ab", "src/aé/b.rs").expect("a match");
        assert_eq!(found.positions, vec![4, 8]);
    }
}
