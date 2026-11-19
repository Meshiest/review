//! References in forge text: `owner/repo`, `@user`, `#1234`, `!1234`
//! (GitLab), `org/repo#12` and full web URLs, found in plain text and turned
//! into web links, and the click that follows one.
//!
//! Forge text is untrusted (see [`crate::untrusted`]): a reference only ever
//! becomes an `https` URL on the pull request's own host, and a click opens
//! a pull request in the app only when the app knows it or a configured
//! forge has it; everything else goes to the browser through the same
//! http(s)-only check as any other link.

use std::borrow::Cow;
use std::collections::HashSet;
use std::ops::Range;

use forge::model::{ForgeKind, PrNumber, RepoRef};
use gpui::App;
use markdown::mdast::Node;

use crate::model::PrKey;
use crate::panels::PanelKind;
use crate::session::{OpenMode, Session};
use crate::untrusted::{self, is_web_url};

/// What a reference may point at, from the pull request it is written on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefContext {
    pub forge: ForgeKind,
    /// The repository a bare `#12` belongs to.
    pub repo: RepoRef,
    /// `owner/name` of the repositories on the same host the app knows,
    /// lowercase: a bare `owner/name` is a link only when it is one of these.
    pub known_repos: HashSet<String>,
    /// Pull requests the app knows on the host, as (`owner/name` lowercase,
    /// number), so `#12` links to the pull request rather than the issue.
    pub known_prs: HashSet<(String, u64)>,
    /// Commit ids of the pull request (head, base, merge base), lowercase:
    /// a hex word that starts one of these is a commit from seven characters.
    pub known_commits: Vec<String>,
}

impl RefContext {
    /// A context that knows nothing beyond `repo`.
    #[must_use]
    pub fn new(forge: ForgeKind, repo: RepoRef) -> Self {
        Self {
            forge,
            repo,
            known_repos: HashSet::new(),
            known_prs: HashSet::new(),
            known_commits: Vec::new(),
        }
    }

    /// Whether `hex` (lowercase) abbreviates a commit of the pull request.
    fn knows_commit(&self, hex: &str) -> bool {
        self.known_commits.iter().any(|sha| sha.starts_with(hex))
    }
}

/// The references context of `key`'s text, with the commits of the pull
/// request it is on.
#[must_use]
pub fn context_for(session: &Session, key: &PrKey) -> Option<RefContext> {
    let mut ctx = session.ref_context(key)?;
    if let Some(pr) = session.pr(key).and_then(|open| open.forge_pr()) {
        ctx.known_commits = [
            Some(&pr.head.sha),
            Some(&pr.base.sha),
            pr.merge_base.as_ref(),
        ]
        .into_iter()
        .flatten()
        .map(|sha| sha.as_str().to_ascii_lowercase())
        .filter(|sha| !sha.is_empty())
        .collect();
    }
    Some(ctx)
}

/// The shortest unknown-commit abbreviation that links without help.
const COMMIT_MIN_BARE: usize = 12;
/// The shortest abbreviation git prints.
const COMMIT_MIN: usize = 7;
const COMMIT_MAX: usize = 40;

/// `#12` or, on GitLab, `!12`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sigil {
    Hash,
    Bang,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reference {
    /// `owner/name` of a repository the app knows.
    Repo(String),
    /// `@login`.
    User(String),
    /// `#12`, `!12`, `owner/name#12`.
    Number {
        repo: Option<String>,
        number: u64,
        sigil: Sigil,
    },
    /// A full `http(s)` URL.
    Url(String),
    /// A commit id, as written (7 to 40 hex characters).
    Commit(String),
}

/// One reference and where it is in the text, in bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub range: Range<usize>,
    pub reference: Reference,
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn is_path_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-')
}

fn prev_char(text: &str, at: usize) -> Option<char> {
    text[..at].chars().next_back()
}

fn next_char(text: &str, at: usize) -> Option<char> {
    text[at..].chars().next()
}

/// Where a URL starting at `start` ends: at whitespace or a character that
/// cannot be in one, less trailing punctuation and an unbalanced `)`.
fn url_end(text: &str, start: usize) -> usize {
    let rest = &text[start..];
    let mut end = rest
        .find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '`'))
        .unwrap_or(rest.len());
    loop {
        let trimmed = rest[..end].trim_end_matches(['.', ',', ';', ':', '!', '?', '\'', '*']);
        let mut cut = trimmed.len();
        if trimmed.ends_with(')') && trimmed.matches(')').count() > trimmed.matches('(').count() {
            cut -= 1;
        }
        if cut == end {
            break;
        }
        end = cut;
    }
    start + end
}

fn starts_url(text: &str, at: usize) -> bool {
    let rest = &text.as_bytes()[at..];
    let prefix =
        |p: &str| rest.len() >= p.len() && rest[..p.len()].eq_ignore_ascii_case(p.as_bytes());
    prefix("https://") || prefix("http://")
}

/// The digits at `at`, with the end, when a non-word character (or the end)
/// follows them.
fn number_at(text: &str, at: usize) -> Option<(u64, usize)> {
    let digits = text[at..].bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 || next_char(text, at + digits).is_some_and(is_word) {
        return None;
    }
    Some((text[at..at + digits].parse().ok()?, at + digits))
}

/// A `segment/segment[/segment]` token at `at`: where it ends, and whether
/// it reads as a repository path (two or more non-empty segments, the owner
/// without a dot, so `example.com/x` is not one).
fn path_token(text: &str, at: usize) -> (usize, bool) {
    let bytes = text.as_bytes();
    let mut end = at;
    loop {
        while bytes.get(end).copied().is_some_and(is_path_byte) {
            end += 1;
        }
        // A slash continues the token only when a segment follows it.
        if bytes.get(end) == Some(&b'/') && bytes.get(end + 1).copied().is_some_and(is_path_byte) {
            end += 1;
            continue;
        }
        break;
    }
    let token = &text[at..end];
    let owner = token.split('/').next().unwrap_or_default();
    let valid = token.contains('/')
        && !owner.contains('.')
        && !token.ends_with(['.', '-'])
        && token
            .split('/')
            .all(|s| s.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric));
    (end, valid)
}

/// Every reference in plain `text`. Code spans, links and the like are the
/// caller's to skip (see [`linkify_markdown`]).
#[must_use]
pub fn find(text: &str, ctx: &RefContext) -> Vec<Found> {
    let bytes = text.as_bytes();
    let says_commit = text.to_ascii_lowercase().contains("commit");
    let mut out = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let prev = prev_char(text, at);
        let b = bytes[at];
        let free = |prev: Option<char>, extra: &[char]| {
            prev.is_none_or(|c| !(is_word(c) || c == '\\' || extra.contains(&c)))
        };
        if (b == b'h' || b == b'H') && starts_url(text, at) && free(prev, &['/', '.', '@']) {
            let end = url_end(text, at);
            let url = &text[at..end];
            if is_web_url(url) {
                out.push(Found {
                    range: at..end,
                    reference: Reference::Url(url.to_owned()),
                });
            }
            at = end.max(at + 1);
            continue;
        }
        if b == b'@' && free(prev, &['.', '-', '/', '@', '`']) {
            let mut end = at + 1;
            while end < bytes.len() && is_path_byte(bytes[end]) {
                end += 1;
            }
            let login = text[at + 1..end].trim_end_matches(['.', '-']);
            let end = at + 1 + login.len();
            let next = next_char(text, end);
            if login
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
                && !matches!(next, Some('@' | '/'))
            {
                out.push(Found {
                    range: at..end,
                    reference: Reference::User(login.to_owned()),
                });
            }
            at = end.max(at + 1);
            continue;
        }
        if b.is_ascii_hexdigit()
            && free(prev, &['.', '-', '/', '@', '#', '!', '&'])
            && let Some(end) = commit_at(text, at, ctx, says_commit)
        {
            out.push(Found {
                range: at..end,
                reference: Reference::Commit(text[at..end].to_owned()),
            });
            at = end;
            continue;
        }
        if is_path_byte(b) && free(prev, &['.', '-', '/', '@', '#', '!', '&']) {
            let (end, repo) = path_token(text, at);
            let end = end.max(at + 1);
            if repo {
                let path = &text[at..end];
                let sigil = match bytes.get(end) {
                    Some(b'#') => Some(Sigil::Hash),
                    Some(b'!') if ctx.forge == ForgeKind::GitLab => Some(Sigil::Bang),
                    _ => None,
                };
                if let Some(sigil) = sigil
                    && let Some((number, after)) = number_at(text, end + 1)
                {
                    out.push(Found {
                        range: at..after,
                        reference: Reference::Number {
                            repo: Some(path.to_owned()),
                            number,
                            sigil,
                        },
                    });
                    at = after;
                    continue;
                }
                let next = next_char(text, end);
                let ends = next.is_none_or(|c| !(is_word(c) || matches!(c, '/' | '#' | '!' | '@')));
                if ends && ctx.known_repos.contains(&path.to_lowercase()) {
                    out.push(Found {
                        range: at..end,
                        reference: Reference::Repo(path.to_owned()),
                    });
                }
            }
            at = end;
            continue;
        }
        let sigil = match b {
            b'#' => Some(Sigil::Hash),
            b'!' if ctx.forge == ForgeKind::GitLab => Some(Sigil::Bang),
            _ => None,
        };
        if let Some(sigil) = sigil
            && free(prev, &['&', '/', '#', '!'])
            && let Some((number, end)) = number_at(text, at + 1)
        {
            out.push(Found {
                range: at..end,
                reference: Reference::Number {
                    repo: None,
                    number,
                    sigil,
                },
            });
            at = end;
            continue;
        }
        at += next_char(text, at).map_or(1, char::len_utf8);
    }
    out
}

/// The end of a commit id starting at `at`: a whole word of 7 to 40 hex
/// characters that abbreviates a commit of the pull request, or, when the
/// text speaks of a commit, any such word, or, otherwise, one of 12 or more.
/// Unknown words need a digit and a letter, so "defaced" is not a commit.
fn commit_at(text: &str, at: usize, ctx: &RefContext, says_commit: bool) -> Option<usize> {
    let hex = text[at..].bytes().take_while(u8::is_ascii_hexdigit).count();
    let end = at + hex;
    if !(COMMIT_MIN..=COMMIT_MAX).contains(&hex)
        || next_char(text, end).is_some_and(|c| is_word(c) || matches!(c, '-' | '/' | '#' | '@'))
    {
        return None;
    }
    let word = &text[at..end];
    let known = ctx.knows_commit(&word.to_ascii_lowercase());
    let mixed =
        word.bytes().any(|b| b.is_ascii_digit()) && word.bytes().any(|b| b.is_ascii_alphabetic());
    let long_enough = says_commit || hex >= COMMIT_MIN_BARE;
    (known || (mixed && long_enough)).then_some(end)
}

/// The web address of `reference` on the context's host; `None` when it
/// names nothing there (a `!12` off GitLab).
#[must_use]
pub fn web_url(reference: &Reference, ctx: &RefContext) -> Option<String> {
    let base = format!("https://{}", ctx.repo.host);
    let url = match reference {
        Reference::Url(url) => url.clone(),
        Reference::Repo(path) => format!("{base}/{path}"),
        Reference::User(login) => format!("{base}/{login}"),
        Reference::Commit(sha) => commit_url(ctx.forge, &ctx.repo, &sha.to_ascii_lowercase()),
        Reference::Number {
            repo,
            number,
            sigil,
        } => {
            let path = repo.clone().unwrap_or_else(|| ctx.repo.path());
            let known = ctx.known_prs.contains(&(path.to_lowercase(), *number));
            let tail = match (ctx.forge, sigil) {
                (ForgeKind::GitLab, Sigil::Bang) => format!("-/merge_requests/{number}"),
                (ForgeKind::GitLab, Sigil::Hash) => format!("-/issues/{number}"),
                (_, Sigil::Bang) => return None,
                (ForgeKind::GitHub, Sigil::Hash) if known => format!("pull/{number}"),
                (ForgeKind::Forgejo, Sigil::Hash) if known => format!("pulls/{number}"),
                (ForgeKind::GitHub | ForgeKind::Forgejo, Sigil::Hash) => {
                    format!("issues/{number}")
                }
            };
            format!("{base}/{path}/{tail}")
        }
    };
    is_web_url(&url).then_some(url)
}

/// The web page of a pull request.
#[must_use]
pub fn pr_url(forge: ForgeKind, repo: &RepoRef, number: PrNumber) -> String {
    let tail = match forge {
        ForgeKind::GitHub => "pull",
        ForgeKind::GitLab => "-/merge_requests",
        ForgeKind::Forgejo => "pulls",
    };
    format!("https://{}/{}/{tail}/{number}", repo.host, repo.path())
}

/// The web page of a commit of `repo`; `sha` is lowercase hex.
#[must_use]
pub fn commit_url(forge: ForgeKind, repo: &RepoRef, sha: &str) -> String {
    let tail = match forge {
        ForgeKind::GitHub | ForgeKind::Forgejo => "commit",
        ForgeKind::GitLab => "-/commit",
    };
    format!("https://{}/{}/{tail}/{sha}", repo.host, repo.path())
}

/// The web page of a repository.
#[must_use]
pub fn repo_url(repo: &RepoRef) -> String {
    format!("https://{}/{}", repo.host, repo.path())
}

/// The web page of an account on `host`.
#[must_use]
pub fn user_url(host: &str, login: &str) -> String {
    format!("https://{host}/{}", encode(login, false))
}

/// The web page of a branch of `repo`.
#[must_use]
pub fn branch_url(forge: ForgeKind, repo: &RepoRef, branch: &str) -> String {
    let tail = match forge {
        ForgeKind::GitHub => "tree",
        ForgeKind::GitLab => "-/tree",
        ForgeKind::Forgejo => "src/branch",
    };
    format!(
        "https://{}/{}/{tail}/{}",
        repo.host,
        repo.path(),
        encode(branch, true)
    )
}

/// Percent-encodes everything but unreserved characters (and `/` when
/// `slashes`), so a name from the forge cannot change the URL's shape.
fn encode(text: &str, slashes: bool) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        if b.is_ascii_alphanumeric()
            || matches!(b, b'-' | b'.' | b'_' | b'~')
            || (slashes && b == b'/')
        {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A pull request a web URL names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrTarget {
    pub key: PrKey,
    /// A pull request page; otherwise an issue page, which may redirect to
    /// a pull request of the same number.
    pub certain: bool,
}

/// The pull request `url` is the page of, by the URL shapes of the three
/// forges: `/o/r/pull/12`, `/group/r/-/merge_requests/12`, `/o/r/pulls/12`,
/// and `/o/r/issues/12` as an uncertain one.
#[must_use]
pub fn pr_target(url: &str) -> Option<PrTarget> {
    if !is_web_url(url) {
        return None;
    }
    let rest = url.split_once("://")?.1;
    let rest = rest.split(['?', '#']).next()?;
    let (host, path) = rest.split_once('/')?;
    if host.is_empty() || host.contains(['@', ':']) {
        return None;
    }
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let valid = |s: &str| !s.is_empty() && s.bytes().all(is_path_byte);
    let make = |owner: &[&str], name: &str, number: &str, certain: bool| {
        let number: u64 = number.parse().ok()?;
        (owner.iter().all(|s| valid(s)) && valid(name) && !owner.is_empty()).then(|| PrTarget {
            key: PrKey::new(
                RepoRef::new(host.to_lowercase(), owner.join("/"), name),
                PrNumber(number),
            ),
            certain,
        })
    };
    if let Some(dash) = segments.iter().position(|s| *s == "-")
        && segments.get(dash + 1) == Some(&"merge_requests")
        && dash >= 2
    {
        let number = segments.get(dash + 2)?;
        return make(&segments[..dash - 1], segments[dash - 1], number, true);
    }
    let [owner, name, kind, number, ..] = segments.as_slice() else {
        return None;
    };
    let certain = match *kind {
        "pull" | "pulls" => true,
        "issues" => false,
        _ => return None,
    };
    make(&[owner], name, number, certain)
}

/// Rewrites forge markdown so every reference outside code, links and HTML
/// is a link to its web URL. Display text is kept as written.
#[must_use]
pub fn linkify_markdown<'a>(text: &'a str, ctx: &RefContext) -> Cow<'a, str> {
    let Ok(root) = markdown::to_mdast(text, &untrusted::parse_options()) else {
        return Cow::Borrowed(text);
    };
    let mut spans = Vec::new();
    collect_text(&root, text, &mut spans);
    let mut edits: Vec<(Range<usize>, String)> = Vec::new();
    for span in spans {
        for found in find(&text[span.clone()], ctx) {
            let Some(url) = web_url(&found.reference, ctx) else {
                continue;
            };
            // A bare URL in a text node is not a link yet (GFM autolinks are
            // separate nodes); one inside is already handled by the policy.
            let range = span.start + found.range.start..span.start + found.range.end;
            let label = untrusted::escape_text(&text[range.clone()]);
            edits.push((
                range,
                format!("[{label}](<{}>)", untrusted::escape_destination(&url)),
            ));
        }
    }
    if edits.is_empty() {
        return Cow::Borrowed(text);
    }
    edits.sort_by_key(|(range, _)| range.start);
    let mut out = String::with_capacity(text.len() + edits.len() * 32);
    let mut at = 0;
    for (range, replacement) in edits {
        if range.start < at {
            continue;
        }
        out.push_str(&text[at..range.start]);
        out.push_str(&replacement);
        at = range.end;
    }
    out.push_str(&text[at..]);
    Cow::Owned(out)
}

/// The source spans of the text nodes that may hold references: not inside
/// a link, an image, code, math or HTML.
fn collect_text(node: &Node, text: &str, out: &mut Vec<Range<usize>>) {
    match node {
        Node::Text(_) => {
            if let Some(span) = node
                .position()
                .map(|p| p.start.offset..p.end.offset)
                .filter(|span| text.get(span.clone()).is_some())
            {
                out.push(span);
            }
        }
        Node::Link(_)
        | Node::LinkReference(_)
        | Node::Image(_)
        | Node::ImageReference(_)
        | Node::Code(_)
        | Node::InlineCode(_)
        | Node::Math(_)
        | Node::InlineMath(_)
        | Node::Html(_)
        | Node::Definition(_) => {}
        _ => {
            for child in node.children().into_iter().flatten() {
                collect_text(child, text, out);
            }
        }
    }
}

/// Follows a link from forge text or a header: a pull request the app knows,
/// or one a configured forge has, opens in the preview tab; anything else
/// opens in the browser if it is still an http(s) URL.
pub fn open_link(url: &str, cx: &mut App) {
    if !is_web_url(url) {
        return;
    }
    let session = Session::try_global(cx);
    let target = pr_target(url).zip(session);
    let Some((target, session)) = target else {
        cx.open_url(url);
        return;
    };
    let (key, configured) = {
        let s = session.read(cx);
        let known = s.known_key(&target.key);
        let configured = s
            .services()
            .is_some_and(|services| services.config.forge(&target.key.repo.host).is_some());
        (known, configured)
    };
    if let Some(key) = key {
        // The pull request on screen is already shown: its link goes to the web.
        if session
            .read(cx)
            .open_pr()
            .is_some_and(|open| open.key == key)
        {
            cx.open_url(url);
            return;
        }
        session.update(cx, |session, cx| {
            session.show(&key, PanelKind::Conversation, OpenMode::Preview, cx);
        });
        return;
    }
    if !configured {
        cx.open_url(url);
        return;
    }
    if target.certain {
        session.update(cx, |session, cx| {
            session.show(&target.key, PanelKind::Conversation, OpenMode::Preview, cx);
        });
        return;
    }
    // An issue page: open it here only if the forge says it is a pull
    // request.
    let Some(services) = session.read(cx).services().cloned() else {
        cx.open_url(url);
        return;
    };
    let (repo, number) = (target.key.repo.clone(), target.key.number);
    let call = services.forge(&repo.host.clone(), move |forge| async move {
        forge.pull_request(&repo, number).await
    });
    let url = url.to_owned();
    cx.spawn(async move |cx| {
        let found = call.await;
        cx.update(|cx| match found {
            Ok(pr) => session.update(cx, |session, cx| {
                session.show(
                    &PrKey::of(&pr),
                    PanelKind::Conversation,
                    OpenMode::Preview,
                    cx,
                );
            }),
            Err(_) if is_web_url(&url) => cx.open_url(&url),
            Err(_) => {}
        })
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(forge: ForgeKind) -> RefContext {
        let mut ctx = RefContext::new(forge, RepoRef::new("github.com", "acme", "api"));
        ctx.known_repos.insert("acme/web".to_owned());
        ctx.known_prs.insert(("acme/api".to_owned(), 7));
        ctx
    }

    fn refs(text: &str) -> Vec<(&str, Reference)> {
        find(text, &ctx(ForgeKind::GitHub))
            .into_iter()
            .map(|f| (&text[f.range], f.reference))
            .collect()
    }

    fn number(repo: Option<&str>, number: u64) -> Reference {
        Reference::Number {
            repo: repo.map(str::to_owned),
            number,
            sigil: Sigil::Hash,
        }
    }

    #[test]
    fn numbers_users_and_cross_repo_references() {
        assert_eq!(
            refs("Fixes #12, see org/repo#34 and ask @mira."),
            [
                ("#12", number(None, 12)),
                ("org/repo#34", number(Some("org/repo"), 34)),
                ("@mira", Reference::User("mira".into())),
            ]
        );
        assert_eq!(refs("(#5)"), [("#5", number(None, 5))]);
    }

    #[test]
    fn emails_are_not_mentions() {
        assert!(refs("write to me@example.com or a.b@c.io").is_empty());
        assert_eq!(
            refs("cc @wen-li, not @"),
            [("@wen-li", Reference::User("wen-li".into()))]
        );
    }

    #[test]
    fn a_hash_inside_a_url_or_a_word_is_not_a_reference() {
        let text = "see https://example.com/page#12 and abc#12 and &#35;3 and C#9";
        assert_eq!(
            refs(text),
            [(
                "https://example.com/page#12",
                Reference::Url("https://example.com/page#12".into())
            )]
        );
        assert!(refs("example.com/a#12 and #12abc and \\#4").is_empty());
    }

    #[test]
    fn bare_repositories_link_only_when_known() {
        assert_eq!(
            refs("moved to acme/web, not and/or or src/a.rs"),
            [("acme/web", Reference::Repo("acme/web".into()))]
        );
    }

    #[test]
    fn bang_is_a_merge_request_only_on_gitlab() {
        assert!(refs("wow!12").is_empty());
        assert!(refs("!12").is_empty());
        let gitlab = ctx(ForgeKind::GitLab);
        let found = find("see !12 and group/sub/proj!3", &gitlab);
        assert_eq!(found.len(), 2);
        assert_eq!(
            web_url(&found[0].reference, &gitlab).as_deref(),
            Some("https://github.com/acme/api/-/merge_requests/12")
        );
        assert_eq!(
            web_url(&found[1].reference, &gitlab).as_deref(),
            Some("https://github.com/group/sub/proj/-/merge_requests/3")
        );
    }

    #[test]
    fn known_pull_requests_link_to_their_page() {
        let ctx = ctx(ForgeKind::GitHub);
        assert_eq!(
            web_url(&number(None, 7), &ctx).as_deref(),
            Some("https://github.com/acme/api/pull/7")
        );
        assert_eq!(
            web_url(&number(None, 8), &ctx).as_deref(),
            Some("https://github.com/acme/api/issues/8")
        );
    }

    #[test]
    fn code_spans_blocks_and_links_are_left_alone() {
        let ctx = ctx(ForgeKind::GitHub);
        let text = "see #3 but `#4` and\n\n```\n#5 @x\n```\n\n[#6](https://e.x) @mira";
        let out = linkify_markdown(text, &ctx);
        assert!(
            out.contains("[\\#3](<https://github.com/acme/api/issues/3>)"),
            "{out}"
        );
        assert!(out.contains("`#4`") && out.contains("#5 @x") && out.contains("[#6](https://e.x)"));
        assert!(
            out.contains("[\\@mira](<https://github.com/mira>)"),
            "{out}"
        );
        assert!(matches!(linkify_markdown("`#1`", &ctx), Cow::Borrowed(_)));
    }

    fn commits(text: &str, known: &[&str]) -> Vec<String> {
        let mut ctx = ctx(ForgeKind::GitHub);
        ctx.known_commits = known.iter().map(|s| (*s).to_owned()).collect();
        find(text, &ctx)
            .into_iter()
            .filter(|f| matches!(f.reference, Reference::Commit(_)))
            .map(|f| text[f.range].to_owned())
            .collect()
    }

    #[test]
    fn short_commit_ids_link_when_known_or_named_as_commits() {
        let known = ["1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d"];
        assert_eq!(commits("fixed in 1a2b3c4", &known), ["1a2b3c4"]);
        assert_eq!(commits("fixed in 1A2B3C4.", &known), ["1A2B3C4"]);
        assert!(commits("fixed in 1a2b3c5", &known).is_empty());
        assert_eq!(commits("see commit 9f8e7d6c", &[]), ["9f8e7d6c"]);
        assert!(commits("see 9f8e7d6c", &[]).is_empty());
    }

    #[test]
    fn long_hex_words_link_but_hex_looking_words_do_not() {
        assert_eq!(commits("at 0123456789ab done", &[]), ["0123456789ab"]);
        assert!(commits("a defaced facade, an effaced decade", &[]).is_empty());
        assert!(commits("commit defaced and deadbeef", &[]).is_empty());
        assert!(commits("1234567 and 123456789012345678", &[]).is_empty());
        assert!(commits("commit 0123456789abcdef0123456789abcdef0123456789", &[]).is_empty());
        assert!(commits("commit abc123", &[]).is_empty());
        assert!(commits("x0123456789ab and 0123456789ab_ and 0123456789ab-1", &[]).is_empty());
    }

    #[test]
    fn commit_links_follow_each_forges_shape() {
        let repo = RepoRef::new("h.example", "acme", "api");
        let url = |forge| commit_url(forge, &repo, "abc1234");
        assert_eq!(
            url(ForgeKind::GitHub),
            "https://h.example/acme/api/commit/abc1234"
        );
        assert_eq!(
            url(ForgeKind::GitLab),
            "https://h.example/acme/api/-/commit/abc1234"
        );
        assert_eq!(
            url(ForgeKind::Forgejo),
            "https://h.example/acme/api/commit/abc1234"
        );
    }

    #[test]
    fn commits_in_code_are_not_linked() {
        let mut ctx = ctx(ForgeKind::GitHub);
        ctx.known_commits = vec!["1a2b3c4d5e6f".to_owned()];
        let out = linkify_markdown("`1a2b3c4` and 1a2b3c4\n\n```\n1a2b3c4\n```", &ctx);
        assert_eq!(out.matches("/commit/").count(), 1, "{out}");
    }

    #[test]
    fn pull_request_urls_name_their_pull_request() {
        let target =
            |url: &str| pr_target(url).map(|t| (t.key.repo.path(), t.key.number.0, t.certain));
        assert_eq!(
            target("https://github.com/acme/api/pull/12/files#diff"),
            Some(("acme/api".into(), 12, true))
        );
        assert_eq!(
            target("https://gitlab.com/group/sub/proj/-/merge_requests/3"),
            Some(("group/sub/proj".into(), 3, true))
        );
        assert_eq!(
            target("https://git.example/cora/review/pulls/38"),
            Some(("cora/review".into(), 38, true))
        );
        assert_eq!(
            target("https://github.com/acme/api/issues/9"),
            Some(("acme/api".into(), 9, false))
        );
        assert_eq!(target("https://github.com/acme/api/blob/main/x.rs"), None);
        assert_eq!(target("javascript:alert(1)//github.com/a/b/pull/1"), None);
        assert_eq!(target("https://user@evil/a/b/pull/1"), None);
    }

    #[test]
    fn names_from_the_forge_cannot_reshape_a_url() {
        let repo = RepoRef::new("github.com", "acme", "api");
        assert_eq!(
            branch_url(ForgeKind::GitHub, &repo, "feat/a b?c#d"),
            "https://github.com/acme/api/tree/feat/a%20b%3Fc%23d"
        );
        assert_eq!(user_url("h", "a/../b"), "https://h/a%2F..%2Fb");
    }
}
