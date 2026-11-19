//! Suggestion blocks in review comments and applying them.
//!
//! - GitLab applies its own suggestions ([`Apply::Forge`]): one request for
//!   one, `batch_apply` for several.
//! - GitHub and Forgejo have no API for it, so a suggestion becomes a
//!   [`LineEdit`] on the thread's lines and the `repo` crate commits it on the
//!   pull request's head from git objects and pushes that as a fast-forward
//!   ([`apply_locally`]). Only where the forge says the viewer may push to
//!   the head branch (never someone else's fork otherwise), and only for a
//!   thread that is not outdated, on the new side, with one suggestion.
//!
//! Comment bodies are other people's text: a block only ever replaces the
//! lines of the thread it was written on, in the file the thread names.
//! Blocks are read from the same markdown tree the card is drawn from (see
//! [`scan`]), and a comment with raw HTML is never applied from here. What
//! the user confirms ([`Confirmation`]) is the object that is applied.
//!
//! What the user sees must be what is applied. A suggestion is refused
//! ([`Refusal`]) when it has a character outside a deny-by-default set
//! ([`is_plain`]), has a line too
//! long to read, or, on GitLab, is not what GitLab itself would apply. The
//! lines it removes are shown only from a diff built at the head being
//! committed, and on GitHub and Forgejo only when they equal the file's
//! lines at that head in the object store ([`read_current_lines`]).

use forge::model::{
    Comment, CommentId, PullRequest, Side, Suggestion, SuggestionId, Thread, ThreadId,
};
use forge::{Capabilities, Suggestions};
use markdown::mdast::Node;
use repo::{FileListOptions, FileMode, LineEdit, ObjectId, PushTarget, Store};
use std::time::Duration;

use crate::services::Services;

const PUSH_TIMEOUT: Duration = Duration::from_secs(120);

/// Longest suggestion line that is offered, in characters.
pub const MAX_LINE_CHARS: usize = 1000;

/// Characters per row of a displayed line before it continues on the next.
pub const ROW_CHARS: usize = 80;

/// One suggestion, by the comment that holds it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SuggestionKey {
    pub thread: ThreadId,
    pub comment: CommentId,
}

/// How a suggestion would be applied. Each carries the change as the card
/// shows it, and it is what gets applied, never a second parse of the body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Apply {
    /// By the forge (GitLab): its suggestion ids, each with the change the
    /// body shows for it.
    Forge(Vec<(SuggestionId, LineEdit)>),
    /// As a commit on the head.
    Local(LineEdit),
}

impl Apply {
    /// The changes this would make.
    pub fn edits(&self) -> impl Iterator<Item = &LineEdit> {
        let (forge, local) = match self {
            Self::Forge(pairs) => (Some(pairs), None),
            Self::Local(edit) => (None, Some(edit)),
        };
        forge
            .into_iter()
            .flatten()
            .map(|(_, edit)| edit)
            .chain(local)
    }
}

/// What a comment offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Offer {
    Apply(Apply),
    /// The body has raw HTML, which the card does not draw the way the forge
    /// does, so applying is left to the forge.
    Html,
    /// There, but not safe to apply from here; the card says why.
    Refused(Refusal),
}

/// Why a suggestion that is there is not offered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// A zero-width or bidirectional control character: the lines read
    /// differently from what they contain.
    HiddenCharacters,
    /// A line over [`MAX_LINE_CHARS`].
    LineTooLong,
    /// GitLab's own range or text is not what the card shows.
    DiffersFromGitLab,
}

impl Refusal {
    /// The sentence the card shows.
    #[must_use]
    pub fn reason(self) -> &'static str {
        match self {
            Self::HiddenCharacters => {
                "not offered: the suggestion has invisible or formatting characters"
            }
            Self::LineTooLong => "not offered: the suggestion has a line over 1,000 characters",
            Self::DiffersFromGitLab => "this suggestion differs from what GitLab would apply",
        }
    }
}

/// Whether `c` may appear in an applied suggestion. Deny by default: only
/// tab, the ASCII space, and letters, marks, numbers, punctuation and symbols
/// pass, minus the default-ignorable code points that draw as nothing
/// (variation selectors, the combining grapheme joiner, Hangul fillers and
/// the like). Format characters (zero-width, bidirectional, tags), other
/// spaces and separators, controls and private-use characters are refused.
#[must_use]
pub fn is_plain(c: char) -> bool {
    use unicode_general_category::{GeneralCategory as G, get_general_category};
    if matches!(c, '\t' | ' ') {
        return true;
    }
    if is_default_ignorable(c) {
        return false;
    }
    matches!(
        get_general_category(c),
        G::UppercaseLetter
            | G::LowercaseLetter
            | G::TitlecaseLetter
            | G::ModifierLetter
            | G::OtherLetter
            | G::NonspacingMark
            | G::SpacingMark
            | G::EnclosingMark
            | G::DecimalNumber
            | G::LetterNumber
            | G::OtherNumber
            | G::ConnectorPunctuation
            | G::DashPunctuation
            | G::OpenPunctuation
            | G::ClosePunctuation
            | G::InitialPunctuation
            | G::FinalPunctuation
            | G::OtherPunctuation
            | G::MathSymbol
            | G::CurrencySymbol
            | G::ModifierSymbol
            | G::OtherSymbol
    )
}

/// Default-ignorable code points outside the format category, which render
/// as nothing although their category looks harmless.
fn is_default_ignorable(c: char) -> bool {
    matches!(
        c,
        '\u{034F}'
            | '\u{115F}'
            | '\u{1160}'
            | '\u{17B4}'
            | '\u{17B5}'
            | '\u{180B}'..='\u{180F}'
            | '\u{3164}'
            | '\u{FE00}'..='\u{FE0F}'
            | '\u{FFA0}'
            | '\u{E0000}'..='\u{E0FFF}'
    )
}

/// Whether `c` hides itself or changes how the text around it reads: any
/// character other than tab that [`is_plain`] refuses.
#[must_use]
pub fn is_hidden(c: char) -> bool {
    c != '\t' && !is_plain(c)
}

/// The first reason to refuse `blocks`, if any.
fn refusal(blocks: &[Vec<String>]) -> Option<Refusal> {
    let lines = || blocks.iter().flatten();
    if lines().any(|line| line.chars().any(is_hidden)) {
        return Some(Refusal::HiddenCharacters);
    }
    lines()
        .any(|line| line.len() > MAX_LINE_CHARS && line.chars().count() > MAX_LINE_CHARS)
        .then_some(Refusal::LineTooLong)
}

/// What [`scan`] found in a comment body.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Scan {
    /// The lines of each visible suggestion block.
    pub blocks: Vec<Vec<String>>,
    /// Whether the body holds raw HTML or an HTML comment anywhere.
    pub has_html: bool,
}

/// Reads `body` with the markdown parser the card is drawn from. A
/// suggestion is a top-level fenced code node whose info string is exactly
/// `suggestion`, closed by its own fence; nothing inside a quote, a list or
/// HTML counts.
#[must_use]
pub fn scan(body: &str) -> Scan {
    let Ok(root) = markdown::to_mdast(body, &crate::untrusted::parse_options()) else {
        return Scan::default();
    };
    let blocks = root
        .children()
        .into_iter()
        .flatten()
        .filter_map(|node| suggestion_lines(node, body))
        .collect();
    Scan {
        blocks,
        has_html: has_html(&root),
    }
}

/// The lines of each suggestion block in `body`; see [`scan`].
#[must_use]
pub fn blocks(body: &str) -> Vec<Vec<String>> {
    scan(body).blocks
}

/// A piece of a comment body: markdown, or a suggestion block's lines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BodyPart<'a> {
    Markdown(&'a str),
    Suggestion(Vec<String>),
}

/// `body` split at its suggestion blocks (the ones [`scan`] finds), in
/// order, so each block can be drawn as a suggestion and the rest as
/// markdown. Blank markdown between blocks is left out.
#[must_use]
pub fn split_body(body: &str) -> Vec<BodyPart<'_>> {
    let Ok(root) = markdown::to_mdast(body, &crate::untrusted::parse_options()) else {
        return vec![BodyPart::Markdown(body)];
    };
    let mut parts = Vec::new();
    let mut at = 0;
    for node in root.children().into_iter().flatten() {
        let Some(lines) = suggestion_lines(node, body) else {
            continue;
        };
        let Some(span) = node.position() else {
            continue;
        };
        let before = &body[at..span.start.offset];
        if !before.trim().is_empty() {
            parts.push(BodyPart::Markdown(before));
        }
        parts.push(BodyPart::Suggestion(lines));
        at = span.end.offset;
    }
    let rest = &body[at..];
    if !rest.trim().is_empty() || parts.is_empty() {
        parts.push(BodyPart::Markdown(rest));
    }
    parts
}

fn has_html(node: &Node) -> bool {
    matches!(node, Node::Html(_)) || node.children().into_iter().flatten().any(has_html)
}

/// The content lines when `node` is a closed, exactly-`suggestion` fence.
fn suggestion_lines(node: &Node, body: &str) -> Option<Vec<String>> {
    let Node::Code(code) = node else {
        return None;
    };
    if code.lang.as_deref() != Some("suggestion") || code.meta.is_some() {
        return None;
    }
    let span = node.position()?;
    let raw = body
        .get(span.start.offset..span.end.offset)?
        .trim_end_matches(['\n', '\r']);
    let lines: Vec<&str> = raw
        .split('\n')
        .map(|line| line.trim_end_matches('\r'))
        .collect();
    let [open, inner @ .., close] = lines.as_slice() else {
        return None;
    };
    let (fence, _) = opening_fence(open)?;
    if !closes(close, fence) {
        return None;
    }
    // The parsed value has the fence's indent removed but drops blank lines
    // at the end of the block, so those come from the source.
    let value: Vec<&str> = code.value.split('\n').collect();
    let indent = open.len() - open.trim_start_matches(' ').len();
    Some(
        inner
            .iter()
            .enumerate()
            .map(|(i, line)| match value.get(i) {
                Some(parsed) if !code.value.is_empty() => parsed.trim_end_matches('\r').to_owned(),
                _ => strip_indent(line, indent),
            })
            .collect(),
    )
}

fn strip_indent(line: &str, indent: usize) -> String {
    let spaces = line.len() - line.trim_start_matches(' ').len();
    line[spaces.min(indent)..].to_owned()
}

/// `(fence, info)` for an opening code fence: up to three spaces of indent,
/// then three or more of one fence character.
fn opening_fence(line: &str) -> Option<(&str, &str)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let ch = rest.chars().next().filter(|&c| c == '`' || c == '~')?;
    let len = rest.len() - rest.trim_start_matches(ch).len();
    if len < 3 {
        return None;
    }
    let info = rest[len..].trim();
    // A backtick fence's info string may not hold a backtick.
    if ch == '`' && info.contains('`') {
        return None;
    }
    Some((&rest[..len], info))
}

fn closes(line: &str, fence: &str) -> bool {
    let ch = fence.as_bytes()[0] as char;
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 {
        return false;
    }
    let len = trimmed.len() - trimmed.trim_start_matches(ch).len();
    len >= fence.len() && trimmed[len..].trim().is_empty()
}

/// Whether `body` could hold a suggestion, as a cheap test before [`scan`].
#[must_use]
pub fn mentions(body: &str) -> bool {
    body.contains("suggestion")
}

/// What the comment offers on `pr`, or `None` where nothing is (and the
/// buttons are hidden).
#[must_use]
pub fn offer(
    thread: &Thread,
    comment: &Comment,
    pr: &PullRequest,
    caps: &Capabilities,
) -> Option<Offer> {
    if !pr.state.is_open() {
        return None;
    }
    let forge = caps.suggestions == Suggestions::RenderedAndApplicable;
    // Pushing needs the head branch to exist and the forge to say the
    // viewer may write to it.
    if !forge && (!pr.head_writable || pr.head.repo.is_none() || thread.outdated) {
        return None;
    }
    let scan = scan(&comment.body);
    if scan.has_html {
        let mentions = !comment.suggestions.is_empty() || mentions(&comment.body);
        return mentions.then_some(Offer::Html);
    }
    if let Some(why) = refusal(&scan.blocks) {
        return Some(Offer::Refused(why));
    }
    if forge {
        return forge_offer(thread, comment, &scan.blocks);
    }
    let [lines] = <[Vec<String>; 1]>::try_from(scan.blocks).ok()?;
    Some(Offer::Apply(Apply::Local(thread_edit(thread, lines)?)))
}

/// GitLab applies its own record of each suggestion, so the card must show
/// that record: the ids pair with the visible blocks in order, and each
/// must have the range of the thread and the text of its block.
fn forge_offer(thread: &Thread, comment: &Comment, blocks: &[Vec<String>]) -> Option<Offer> {
    if comment.suggestions.is_empty() {
        return None;
    }
    let differs = Some(Offer::Refused(Refusal::DiffersFromGitLab));
    if comment.suggestions.len() != blocks.len() {
        return differs;
    }
    let mut pairs = Vec::new();
    for (suggestion, lines) in comment.suggestions.iter().zip(blocks) {
        if !suggestion.appliable || suggestion.applied {
            continue;
        }
        let shown = thread_edit(thread, lines.clone())?;
        let Some(edit) = gitlab_edit(&shown, suggestion) else {
            return differs;
        };
        pairs.push((suggestion.id.clone(), edit));
    }
    (!pairs.is_empty()).then_some(Offer::Apply(Apply::Forge(pairs)))
}

/// `shown` as GitLab's own record has it, or `None` when the range or the
/// text differs. Only a trailing newline of GitLab's text is forgiven, and
/// offset suggestions are not supported.
fn gitlab_edit(shown: &LineEdit, suggestion: &Suggestion) -> Option<LineEdit> {
    if suggestion.lines_above != 0 || suggestion.lines_below != 0 {
        return None;
    }
    if (suggestion.from_line, suggestion.to_line) != (Some(shown.first), Some(shown.last)) {
        return None;
    }
    let text = suggestion.to_content.as_deref()?;
    let lines = if text.is_empty() {
        Vec::new()
    } else {
        text.strip_suffix('\n')
            .unwrap_or(text)
            .split('\n')
            .map(str::to_owned)
            .collect()
    };
    (lines == shown.lines).then(|| LineEdit {
        lines,
        ..shown.clone()
    })
}

/// How the suggestion in `comment` of `thread` would be applied on `pr`,
/// or `None` where it cannot be.
#[must_use]
pub fn applicable(
    thread: &Thread,
    comment: &Comment,
    pr: &PullRequest,
    caps: &Capabilities,
) -> Option<Apply> {
    match offer(thread, comment, pr, caps)? {
        Offer::Apply(apply) => Some(apply),
        Offer::Html | Offer::Refused(_) => None,
    }
}

/// `lines` in place of the thread's lines on the new side.
fn thread_edit(thread: &Thread, lines: Vec<String>) -> Option<LineEdit> {
    let anchor = thread.anchor.as_ref()?;
    let position = anchor.position.as_ref()?;
    if position.line.side != Side::New {
        return None;
    }
    let last = position.line.line;
    let first = position
        .start
        .filter(|start| start.side == Side::New && start.line < last)
        .map_or(last, |start| start.line);
    Some(LineEdit {
        path: anchor.path.clone(),
        first,
        last,
        lines,
    })
}

/// The lines a change removes, as far as they can be trusted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Removed {
    /// The lines at the head being committed.
    Lines(Vec<String>),
    /// The object store is still being read.
    Loading,
    /// The diff on screen is not of the head being committed, lacks the
    /// file, or disagrees with the object store.
    NotLoaded,
}

/// One change as the confirmation lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub path: String,
    pub first: u32,
    pub last: u32,
    /// The lines `first..=last` are now.
    pub removed: Removed,
    pub added: Vec<String>,
}

impl Change {
    /// Whether the removed lines are known, so applying can be allowed.
    #[must_use]
    pub fn is_verified(&self) -> bool {
        matches!(self.removed, Removed::Lines(_))
    }
}

/// Whether every change in `changes` shows lines it can stand behind; an
/// empty list does not.
#[must_use]
pub fn all_verified(changes: &[Change]) -> bool {
    !changes.is_empty() && changes.iter().all(Change::is_verified)
}

/// What the object store says the replaced lines are, for GitHub and
/// Forgejo, where the commit is built from those objects.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Stored {
    /// Nothing to compare against: GitLab applies it, or there is no store.
    NotNeeded,
    /// Being read in the background.
    Pending,
    /// Per local edit, the lines at the head; `None` where they could not
    /// be read.
    Read(Vec<Option<Vec<String>>>),
}

/// What the user is asked to confirm: the suggestions parsed once, held as
/// the objects that get applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Confirmation {
    plans: Vec<Apply>,
    /// The head commit the changes were shown against.
    head: String,
    stored: Stored,
}

impl Confirmation {
    #[must_use]
    pub fn new(plans: Vec<Apply>, head: impl Into<String>) -> Self {
        Self {
            plans,
            head: head.into(),
            stored: Stored::NotNeeded,
        }
    }

    /// Holds the changes back until the lines they replace have been read
    /// from the object store ([`Self::set_read`]); a no-op where the forge
    /// applies them.
    #[must_use]
    pub fn awaiting_store(mut self) -> Self {
        if !self.local_edits().is_empty() {
            self.stored = Stored::Pending;
        }
        self
    }

    /// The lines read from the object store for [`Self::local_edits`], in
    /// that order; ignored when `edits` are not the ones this holds.
    pub fn set_read(&mut self, edits: &[LineEdit], read: Vec<Option<Vec<String>>>) {
        if self.local_edits() == edits && read.len() == edits.len() {
            self.stored = Stored::Read(read);
        }
    }

    /// The head commit the changes were shown against.
    #[must_use]
    pub fn head(&self) -> &str {
        &self.head
    }

    /// Every change to list. `diff_head` is the commit the open diff was
    /// built from and `diff_lines` reads (path, first, last) from it; its
    /// lines count only when that is the head being committed, and, where
    /// the object store was read, only when they equal what it holds.
    pub fn changes(
        &self,
        diff_head: &str,
        diff_lines: impl Fn(&str, u32, u32) -> Option<Vec<String>>,
    ) -> Vec<Change> {
        let current = diff_head == self.head;
        let mut local = 0;
        let mut out = Vec::new();
        for plan in &self.plans {
            let is_local = matches!(plan, Apply::Local(_));
            for edit in plan.edits() {
                let shown = current
                    .then(|| diff_lines(&edit.path, edit.first, edit.last))
                    .flatten();
                let removed = match (shown, &self.stored) {
                    (None, _) => Removed::NotLoaded,
                    (Some(_), Stored::Pending) if is_local => Removed::Loading,
                    (Some(lines), Stored::Read(read)) if is_local => {
                        match read.get(local).and_then(Option::as_ref) {
                            Some(stored) if *stored == lines => Removed::Lines(lines),
                            _ => Removed::NotLoaded,
                        }
                    }
                    (Some(lines), _) => Removed::Lines(lines),
                };
                local += usize::from(is_local);
                out.push(Change {
                    path: edit.path.clone(),
                    first: edit.first,
                    last: edit.last,
                    removed,
                    added: edit.lines.clone(),
                });
            }
        }
        out
    }

    /// The edits `commit_edits` receives; none when the forge applies them.
    #[must_use]
    pub fn local_edits(&self) -> Vec<LineEdit> {
        self.plans
            .iter()
            .filter_map(|plan| match plan {
                Apply::Local(edit) => Some(edit.clone()),
                Apply::Forge(_) => None,
            })
            .collect()
    }

    /// The forge's suggestion ids to apply.
    #[must_use]
    pub fn forge_ids(&self) -> Vec<SuggestionId> {
        self.plans
            .iter()
            .filter_map(|plan| match plan {
                Apply::Forge(pairs) => Some(pairs.iter().map(|(id, _)| id.clone())),
                Apply::Local(_) => None,
            })
            .flatten()
            .collect()
    }

    /// How many suggestions it applies.
    #[must_use]
    pub fn len(&self) -> usize {
        self.plans.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.plans.is_empty()
    }
}

/// For each of `edits`, the lines `first..=last` of its file at `head` in
/// the object store, or `None` where they cannot be read: the head is not
/// fetched, the file is not one the pull request changed, it is not UTF-8,
/// or the range is outside it. Blocks on git.
#[must_use]
pub fn read_current_lines(
    store: &Store,
    number: u64,
    head: &str,
    edits: &[LineEdit],
) -> Vec<Option<Vec<String>>> {
    let list = (|| {
        let head = store.resolve_commit(head).ok()?;
        let (base, _) = store.pr_range(number).ok()?;
        let options = FileListOptions {
            renames: false,
            detect_binary: false,
        };
        store.file_list(Some(base), head, options).ok()
    })();
    edits
        .iter()
        .map(|edit| {
            let list = list.as_ref()?;
            let entry = list
                .files
                .iter()
                .find(|file| file.path == edit.path)?
                .new
                .filter(|entry| entry.mode != FileMode::Submodule)?;
            let blob = store.blob(entry.id).ok()?;
            let (first, last) = (edit.first.checked_sub(1)?, edit.last);
            if first >= last || last > blob.line_count() {
                return None;
            }
            (first..last)
                .map(|ix| Some(str::from_utf8(blob.line(ix)).ok()?.to_owned()))
                .collect()
        })
        .collect()
}

/// A piece of a displayed line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Piece {
    Text(String),
    /// A character that would not show as itself, as its code point
    /// (`U+0020`, with `x3` after it for a run of three).
    Mark(String),
}

/// Whether `c` is drawn as a code point wherever it is, rather than as
/// itself: tabs and other control characters, hidden characters, and the
/// spaces and separators that look like nothing.
fn is_escaped(c: char) -> bool {
    c == '\t' || is_hidden(c)
}

/// `line` for display: ordinary text as it is, and trailing whitespace and
/// every escaped character as marks, a run of one character as one mark.
#[must_use]
pub fn pieces(line: &str) -> Vec<Piece> {
    fn flush(run: &mut Option<(char, usize)>, out: &mut Vec<Piece>) {
        if let Some((c, count)) = run.take() {
            let mut mark = format!("U+{:04X}", u32::from(c));
            if count > 1 {
                mark.push_str(&format!(" x{count}"));
            }
            out.push(Piece::Mark(mark));
        }
    }
    let trailing = line.trim_end_matches(char::is_whitespace).len();
    let mut out: Vec<Piece> = Vec::new();
    let mut run: Option<(char, usize)> = None;
    for (at, c) in line.char_indices() {
        if is_escaped(c) || (at >= trailing && c.is_whitespace()) {
            match &mut run {
                Some((last, count)) if *last == c => *count += 1,
                _ => {
                    flush(&mut run, &mut out);
                    run = Some((c, 1));
                }
            }
            continue;
        }
        flush(&mut run, &mut out);
        match out.last_mut() {
            Some(Piece::Text(text)) => text.push(c),
            _ => out.push(Piece::Text(c.to_string())),
        }
    }
    flush(&mut run, &mut out);
    out
}

/// [`pieces`] cut into rows of about `width` characters, so a long line
/// continues on the next row instead of running out of the card. Always at
/// least one row.
#[must_use]
pub fn rows(line: &str, width: usize) -> Vec<Vec<Piece>> {
    let width = width.max(1);
    let mut rows: Vec<Vec<Piece>> = vec![Vec::new()];
    let mut used = 0;
    for piece in pieces(line) {
        match piece {
            Piece::Text(text) => {
                for c in text.chars() {
                    if used == width {
                        rows.push(Vec::new());
                        used = 0;
                    }
                    let row = rows.last_mut().expect("rows is never empty");
                    match row.last_mut() {
                        Some(Piece::Text(open)) => open.push(c),
                        _ => row.push(Piece::Text(c.to_string())),
                    }
                    used += 1;
                }
            }
            Piece::Mark(mark) => {
                // A mark is drawn with a little padding.
                let size = mark.chars().count() + 2;
                if used > 0 && used + size > width {
                    rows.push(Vec::new());
                    used = 0;
                }
                rows.last_mut()
                    .expect("rows is never empty")
                    .push(Piece::Mark(mark));
                used += size;
            }
        }
    }
    rows
}

/// The commit message for `count` suggestions.
#[must_use]
pub fn message(count: usize) -> String {
    match count {
        1 => "Apply suggestion from code review".to_owned(),
        n => format!("Apply {n} suggestions from code review"),
    }
}

/// Commits `edits` on `pr`'s head (as the forge last reported it) in the
/// local object store and pushes the commit to the head branch, refusing if
/// the branch moved. Returns the new head. Runs git off the calling thread.
///
/// # Errors
/// Text for the user: the head is not fetched yet, an edit does not fit the
/// file, the branch moved, or git failed.
pub async fn apply_locally(
    services: &Services,
    pr: &PullRequest,
    edits: Vec<LineEdit>,
) -> Result<ObjectId, String> {
    let head_repo = pr
        .head
        .repo
        .clone()
        .ok_or("the head branch's repository is gone")?;
    if !pr.head_writable {
        return Err("the forge says you cannot push to this pull request's branch".to_owned());
    }
    let target = PushTarget {
        remote_url: services.config.remote_url(&head_repo),
        branch: pr.head.name.clone(),
        timeout: PUSH_TIMEOUT,
    };
    let (objects, repo) = (services.objects.clone(), pr.repo.clone());
    let head = pr.head.sha.as_str().to_owned();
    let number = pr.number.0;
    let fetches = services.fetches.clone();
    let message = message(edits.len());
    services
        .run_blocking(move || {
            let store = objects.resolve(&repo)?;
            let head = store.resolve_commit(&head).map_err(|_| {
                format!("the head {head} is not fetched yet; open the pull request again")
            })?;
            let commit = store
                .commit_edits(head, &edits, &message)
                .map_err(|error| error.to_string())?;
            store
                .push_fast_forward(&target, head, commit)
                .map_err(|error| error.to_string())?;
            fetches.forget(&store, number);
            Ok(commit)
        })
        .await
        .unwrap_or_else(|| Err("applying the suggestions stopped".to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge::model::{
        Anchor, BranchRef, LineRef, Position, PrNumber, PrState, PullRequestId, RepoRef,
        Resolution, ReviewDecision, Sha, Suggestion, User,
    };

    #[test]
    fn suggestion_fences_are_found_with_their_lines() {
        let body = "Try:\n\n```suggestion\nlet a = 1;\n\nlet b = 2;\n```\nthanks";
        assert_eq!(blocks(body), [vec!["let a = 1;", "", "let b = 2;"]]);
        assert_eq!(
            blocks("```suggestion\n```"),
            [Vec::<String>::new()],
            "an empty block deletes"
        );
        assert_eq!(
            blocks("```suggestion\n\n\n```"),
            [vec!["", ""]],
            "blank lines are lines"
        );
        assert_eq!(
            blocks("```suggestion\na\n\n```"),
            [vec!["a", ""]],
            "a trailing blank line is kept"
        );
        assert_eq!(blocks("~~~~suggestion\r\nx\r\n~~~~\r\n"), [vec!["x"]]);
        assert_eq!(
            blocks("  ```suggestion\n  x\n    y\n  ```"),
            [vec!["x", "  y"]]
        );
        // A longer fence holds a shorter one.
        assert_eq!(
            blocks("````suggestion\n```\ncode\n```\n````"),
            [vec!["```", "code", "```"]]
        );
        for no in [
            "```rust\nx\n```",
            "```suggestion\nnever closed",
            "    ```suggestion\nindented code\n```",
            "```suggestion:-1+0\nGitLab offsets\n```",
            "```suggestion extra\nx\n```",
            "`` suggestion\nx\n``",
        ] {
            assert!(blocks(no).is_empty(), "{no:?}");
        }
        assert_eq!(
            blocks("```suggestion\na\n```\n```suggestion\nb\n```").len(),
            2
        );
    }

    #[test]
    fn only_top_level_fences_are_suggestions() {
        for html in [
            "<!--
```suggestion
evil();
```
-->",
            "<details>
<summary>hi</summary>
```suggestion
evil();
```
</details>",
            "<div>

```suggestion
evil();
```",
            "text <!-- c --> more

```suggestion
evil();
```",
        ] {
            assert!(scan(html).has_html, "{html:?}");
        }
        for nested in [
            "> ```suggestion
> evil();
> ```",
            "- item

  ```suggestion
  evil();
  ```",
            "1. ```suggestion
   evil();
   ```",
        ] {
            assert_eq!(scan(nested), Scan::default(), "{nested:?}");
        }
        assert!(!scan("plain\n\n```suggestion\nx\n```").has_html);
        assert!(!scan("```\n<!-- in code -->\n```").has_html);
    }

    fn pr(writable: bool) -> PullRequest {
        let repo = RepoRef::new("github.com", "Lovelace", "review-sandbox");
        let branch = |name: &str| BranchRef {
            repo: Some(repo.clone()),
            name: name.to_owned(),
            sha: Sha::new("h"),
        };
        PullRequest {
            id: PullRequestId::new("P"),
            repo: repo.clone(),
            number: PrNumber(4),
            title: String::new(),
            body: String::new(),
            author: User::new("cora"),
            state: PrState::Open,
            base: branch("main"),
            head: branch("topic"),
            merge_base: None,
            created_at: chrono::DateTime::UNIX_EPOCH,
            updated_at: chrono::DateTime::UNIX_EPOCH,
            review_decision: ReviewDecision::None,
            reviewers: Vec::new(),
            ci: None,
            comment_count: None,
            unresolved_threads: None,
            labels: Vec::new(),
            diff_stat: None,
            assignees: Vec::new(),
            milestone: None,
            web_url: String::new(),
            head_writable: writable,
        }
    }

    fn thread(side: Side, start: Option<u32>, line: u32, body: &str) -> Thread {
        Thread {
            id: ThreadId::new("T"),
            anchor: Some(Anchor {
                path: "docs/intro.md".into(),
                old_path: None,
                position: Some(Position {
                    line: LineRef { side, line },
                    start: start.map(|line| LineRef { side, line }),
                    commit: None,
                }),
                original: None,
            }),
            resolution: Resolution::Unresolved,
            outdated: false,
            comments: vec![Comment {
                id: CommentId::new("C"),
                author: User::new("mira"),
                body: body.to_owned(),
                created_at: chrono::DateTime::UNIX_EPOCH,
                edited_at: None,
                web_url: None,
                review: None,
                verdict: None,
                suggestions: Vec::new(),
            }],
        }
    }

    const ONE: &str = "```suggestion\nnew line\n```";

    #[test]
    fn a_local_suggestion_replaces_the_thread_s_lines() {
        let github = Capabilities::GITHUB;
        let t = thread(Side::New, Some(3), 5, ONE);
        assert_eq!(
            applicable(&t, &t.comments[0], &pr(true), &github),
            Some(Apply::Local(LineEdit {
                path: "docs/intro.md".into(),
                first: 3,
                last: 5,
                lines: vec!["new line".into()],
            }))
        );
        let single = thread(Side::New, Some(5), 5, ONE);
        let Some(Apply::Local(edit)) = applicable(&single, &single.comments[0], &pr(true), &github)
        else {
            panic!("applicable");
        };
        assert_eq!((edit.first, edit.last), (5, 5));
    }

    #[test]
    fn the_button_is_hidden_where_applying_is_not_safe() {
        let github = Capabilities::GITHUB;
        let t = thread(Side::New, None, 5, ONE);
        // Not allowed to push (someone else's fork).
        assert_eq!(applicable(&t, &t.comments[0], &pr(false), &github), None);
        // A deleted fork.
        let mut gone = pr(true);
        gone.head.repo = None;
        assert_eq!(applicable(&t, &t.comments[0], &gone, &github), None);
        // The removed side, an outdated thread, two blocks, none, a closed PR.
        let old = thread(Side::Old, None, 5, ONE);
        assert_eq!(applicable(&old, &old.comments[0], &pr(true), &github), None);
        let mut outdated = thread(Side::New, None, 5, ONE);
        outdated.outdated = true;
        assert_eq!(
            applicable(&outdated, &outdated.comments[0], &pr(true), &github),
            None
        );
        let two = thread(Side::New, None, 5, &format!("{ONE}\n\n{ONE}"));
        assert_eq!(applicable(&two, &two.comments[0], &pr(true), &github), None);
        let none = thread(Side::New, None, 5, "looks good");
        assert_eq!(
            applicable(&none, &none.comments[0], &pr(true), &github),
            None
        );
        let mut closed = pr(true);
        closed.state = PrState::Merged;
        assert_eq!(applicable(&t, &t.comments[0], &closed, &github), None);
    }

    fn offered(body: &str) -> Option<Offer> {
        let t = thread(Side::New, None, 5, body);
        offer(&t, &t.comments[0], &pr(true), &Capabilities::GITHUB)
    }

    #[test]
    fn a_visible_suggestion_is_offered() {
        assert_eq!(
            offered("Maybe:\n\n```suggestion\nnew line\n```\n"),
            Some(Offer::Apply(Apply::Local(LineEdit {
                path: "docs/intro.md".into(),
                first: 5,
                last: 5,
                lines: vec!["new line".into()],
            })))
        );
    }

    #[test]
    fn a_hidden_suggestion_is_not_offered() {
        for body in [
            "<!-- hidden -->\n\n```suggestion\nevil();\n```",
            "<!--\n```suggestion\nevil();\n```\n-->",
            "<details><summary>x</summary>\n\n```suggestion\nevil();\n```\n\n</details>",
            "<div>\n\n```suggestion\nevil();\n```",
            "text <b>bold</b>\n\n```suggestion\nevil();\n```",
        ] {
            assert_eq!(offered(body), Some(Offer::Html), "{body:?}");
        }
        for body in [
            "> ```suggestion\n> evil();\n> ```",
            "- item\n\n  ```suggestion\n  evil();\n  ```",
            "1. ```suggestion\n   evil();\n   ```",
        ] {
            assert_eq!(offered(body), None, "{body:?}");
        }
        // HTML with no suggestion in it says nothing.
        assert_eq!(offered("a <b>bold</b> remark"), None);
    }

    #[test]
    fn two_visible_blocks_are_not_applied_locally() {
        assert_eq!(
            offered("```suggestion\na\n```\n\n```suggestion\nb\n```"),
            None
        );
    }

    #[test]
    fn gitlab_applies_the_forge_s_own_suggestions() {
        let gitlab = Capabilities::GITLAB;
        let body = format!("{ONE}\n\n{ONE}\n\n{ONE}");
        let mut t = thread(Side::New, None, 5, &body);
        assert_eq!(applicable(&t, &t.comments[0], &pr(false), &gitlab), None);
        let suggestion =
            |id: &str, appliable, applied| gitlab_suggestion(id, appliable, applied, "new line\n");
        t.comments[0].suggestions = vec![
            suggestion("1", true, false),
            suggestion("2", false, false),
            suggestion("3", true, true),
        ];
        let edit = LineEdit {
            path: "docs/intro.md".into(),
            first: 5,
            last: 5,
            lines: vec!["new line".into()],
        };
        assert_eq!(
            applicable(&t, &t.comments[0], &pr(false), &gitlab),
            Some(Apply::Forge(vec![(SuggestionId::new("1"), edit)]))
        );
        // Ids that do not pair with the visible blocks are not applied.
        t.comments[0].suggestions.pop();
        assert_eq!(
            offer(&t, &t.comments[0], &pr(false), &gitlab),
            Some(Offer::Refused(Refusal::DiffersFromGitLab))
        );
        assert_eq!(applicable(&t, &t.comments[0], &pr(false), &gitlab), None);
        // Nor are those of a comment with HTML in it.
        let mut hidden = thread(Side::New, None, 5, &format!("<!-- x -->\n\n{ONE}"));
        hidden.comments[0].suggestions = vec![suggestion("1", true, false)];
        assert_eq!(
            offer(&hidden, &hidden.comments[0], &pr(false), &gitlab),
            Some(Offer::Html)
        );
    }

    #[test]
    fn the_confirmation_lists_what_commit_edits_receives() {
        let t = thread(
            Side::New,
            Some(3),
            5,
            "```suggestion\nfirst\n  second\n\n```",
        );
        let Some(Offer::Apply(plan)) = offer(&t, &t.comments[0], &pr(true), &Capabilities::GITHUB)
        else {
            panic!("offered");
        };
        let confirmation = Confirmation::new(vec![plan], "h");
        let shown = confirmation.changes("h", |path, first, last| {
            assert_eq!((path, first, last), ("docs/intro.md", 3, 5));
            Some(vec!["old".into()])
        });
        let committed = confirmation.local_edits();
        assert_eq!(shown.len(), committed.len());
        for (change, edit) in shown.iter().zip(&committed) {
            assert_eq!(change.path, edit.path);
            assert_eq!((change.first, change.last), (edit.first, edit.last));
            assert_eq!(change.added, edit.lines);
        }
        assert_eq!(committed[0].lines, ["first", "  second", ""]);
        assert_eq!(shown[0].removed, Removed::Lines(vec!["old".to_owned()]));
        assert!(confirmation.forge_ids().is_empty());
    }

    fn gitlab_suggestion(id: &str, appliable: bool, applied: bool, to: &str) -> Suggestion {
        Suggestion {
            id: SuggestionId::new(id),
            applied,
            appliable,
            from_line: Some(5),
            to_line: Some(5),
            from_content: Some("old\n".to_owned()),
            to_content: Some(to.to_owned()),
            lines_above: 0,
            lines_below: 0,
        }
    }

    fn gitlab_offer(change: impl FnOnce(&mut Suggestion)) -> Option<Offer> {
        let mut t = thread(Side::New, None, 5, "```suggestion\nnew line\n```");
        let mut suggestion = gitlab_suggestion("1", true, false, "new line\n");
        change(&mut suggestion);
        t.comments[0].suggestions = vec![suggestion];
        offer(&t, &t.comments[0], &pr(false), &Capabilities::GITLAB)
    }

    #[test]
    fn a_suggestion_with_hidden_characters_is_refused() {
        for c in [
            '\u{200B}',
            '\u{200C}',
            '\u{200D}',
            '\u{2060}',
            '\u{FEFF}',
            '\u{202A}',
            '\u{202E}',
            '\u{2066}',
            '\u{2069}',
            '\u{200E}',
            '\u{200F}',
            '\u{061C}',
            // Caught by the deny-by-default check rather than a list: soft
            // hyphen, variation selector, combining grapheme joiner, Hangul
            // filler, tag character, no-break and ideographic spaces, line
            // separator, private use.
            '\u{00AD}',
            '\u{FE0F}',
            '\u{034F}',
            '\u{3164}',
            '\u{E0041}',
            '\u{00A0}',
            '\u{3000}',
            '\u{2028}',
            '\u{E000}',
        ] {
            let body = format!("```suggestion\nlet a{c} = 1;\n```");
            assert_eq!(
                offered(&body),
                Some(Offer::Refused(Refusal::HiddenCharacters)),
                "U+{:04X}",
                u32::from(c)
            );
        }
        // Also where the forge applies it.
        let mut t = thread(Side::New, None, 5, "```suggestion\nx\u{202E}y\n```");
        t.comments[0].suggestions = vec![gitlab_suggestion("1", true, false, "x\u{202E}y\n")];
        assert_eq!(
            offer(&t, &t.comments[0], &pr(false), &Capabilities::GITLAB),
            Some(Offer::Refused(Refusal::HiddenCharacters))
        );
        assert_eq!(
            applicable(&t, &t.comments[0], &pr(false), &Capabilities::GITLAB),
            None
        );
        // Tabs and ordinary Unicode are fine.
        assert!(matches!(
            offered("```suggestion\n\tlet caf\u{e9} = 1;\n```"),
            Some(Offer::Apply(_))
        ));
    }

    #[test]
    fn a_long_line_is_not_offered() {
        let line = |n: usize| format!("```suggestion\n{}\n```", "a".repeat(n));
        assert!(matches!(
            offered(&line(MAX_LINE_CHARS)),
            Some(Offer::Apply(_))
        ));
        assert_eq!(
            offered(&line(MAX_LINE_CHARS + 1)),
            Some(Offer::Refused(Refusal::LineTooLong))
        );
        // Characters, not bytes.
        let wide = format!("```suggestion\n{}\n```", "\u{e9}".repeat(MAX_LINE_CHARS));
        assert!(matches!(offered(&wide), Some(Offer::Apply(_))));
    }

    #[test]
    fn trailing_whitespace_and_tabs_render_as_marks() {
        let mark = |s: &str| Piece::Mark(s.to_owned());
        let text = |s: &str| Piece::Text(s.to_owned());
        assert_eq!(pieces("let a;"), [text("let a;")]);
        assert_eq!(pieces("let a;  "), [text("let a;"), mark("U+0020 x2")]);
        assert_eq!(pieces("x "), [text("x"), mark("U+0020")]);
        assert_eq!(pieces("a b"), [text("a b")], "inner spaces stay spaces");
        assert_eq!(
            pieces("\tx\u{200B}y\u{202E}"),
            [
                mark("U+0009"),
                text("x"),
                mark("U+200B"),
                text("y"),
                mark("U+202E")
            ]
        );
        assert_eq!(pieces("  "), [mark("U+0020 x2")], "a blank line is marks");
        assert_eq!(pieces("a \t"), [text("a"), mark("U+0020"), mark("U+0009")]);
        assert_eq!(pieces(""), []);
    }

    #[test]
    fn long_lines_continue_on_further_rows_without_losing_text() {
        let line = format!("{}  ", "ab".repeat(100));
        let wrapped = rows(&line, 80);
        assert_eq!(wrapped.len(), 3);
        let text: String = wrapped
            .iter()
            .flatten()
            .filter_map(|piece| match piece {
                Piece::Text(text) => Some(text.as_str()),
                Piece::Mark(_) => None,
            })
            .collect();
        assert_eq!(text, "ab".repeat(100));
        assert_eq!(
            wrapped[2].last(),
            Some(&Piece::Mark("U+0020 x2".to_owned()))
        );
        assert_eq!(rows("", 80), [Vec::<Piece>::new()]);
        assert_eq!(rows(&"x".repeat(80), 80).len(), 1);
    }

    #[test]
    fn a_gitlab_suggestion_that_matches_is_offered_with_gitlab_s_text() {
        let edit = LineEdit {
            path: "docs/intro.md".into(),
            first: 5,
            last: 5,
            lines: vec!["new line".into()],
        };
        for to in ["new line\n", "new line"] {
            assert_eq!(
                gitlab_offer(|s| s.to_content = Some(to.to_owned())),
                Some(Offer::Apply(Apply::Forge(vec![(
                    SuggestionId::new("1"),
                    edit.clone()
                )]))),
                "{to:?}"
            );
        }
        // The confirmation lists GitLab's text.
        let Some(Offer::Apply(plan)) = gitlab_offer(|_| {}) else {
            panic!("offered");
        };
        let shown =
            Confirmation::new(vec![plan], "h").changes("h", |_, _, _| Some(vec!["old".into()]));
        assert_eq!(shown[0].added, ["new line"]);
    }

    #[test]
    fn a_gitlab_suggestion_that_differs_is_refused() {
        let refused = Some(Offer::Refused(Refusal::DiffersFromGitLab));
        assert_eq!(gitlab_offer(|s| s.from_line = Some(4)), refused, "first");
        assert_eq!(gitlab_offer(|s| s.to_line = Some(6)), refused, "last");
        assert_eq!(gitlab_offer(|s| s.to_line = None), refused, "no range");
        for to in ["other\n", "new line \n", "new line\n\n", "New line\n", ""] {
            assert_eq!(
                gitlab_offer(|s| s.to_content = Some(to.to_owned())),
                refused,
                "{to:?}"
            );
        }
        assert_eq!(gitlab_offer(|s| s.to_content = None), refused, "no text");
        assert_eq!(gitlab_offer(|s| s.lines_above = 1), refused, "offset");
        assert_eq!(gitlab_offer(|s| s.lines_below = 2), refused, "offset");
        // A block GitLab does not have, and one it has twice.
        let mut t = thread(Side::New, None, 5, ONE);
        t.comments[0].suggestions = vec![
            gitlab_suggestion("1", true, false, "new line\n"),
            gitlab_suggestion("2", true, false, "new line\n"),
        ];
        assert_eq!(
            offer(&t, &t.comments[0], &pr(false), &Capabilities::GITLAB),
            refused
        );
    }

    #[test]
    fn a_confirmation_against_another_head_has_no_lines_to_apply_from() {
        let t = thread(Side::New, None, 5, ONE);
        let Some(Offer::Apply(plan)) = offer(&t, &t.comments[0], &pr(true), &Capabilities::GITHUB)
        else {
            panic!("offered");
        };
        let confirmation = Confirmation::new(vec![plan], "head-new");
        let lines = |_: &str, _: u32, _: u32| Some(vec!["old".to_owned()]);
        let changes = confirmation.changes("head-old", lines);
        assert_eq!(changes[0].removed, Removed::NotLoaded);
        assert!(!all_verified(&changes), "apply stays disabled");
        assert!(!all_verified(&confirmation.changes("", lines)), "no diff");
        assert!(all_verified(&confirmation.changes("head-new", lines)));
        assert!(!all_verified(&[]), "nothing to apply is not verified");
        // The diff has no such file.
        let none = confirmation.changes("head-new", |_, _, _| None);
        assert_eq!(none[0].removed, Removed::NotLoaded);
    }

    #[test]
    fn local_changes_wait_for_and_match_the_object_store() {
        let t = thread(Side::New, None, 5, ONE);
        let Some(Offer::Apply(plan)) = offer(&t, &t.comments[0], &pr(true), &Capabilities::GITHUB)
        else {
            panic!("offered");
        };
        let mut confirmation = Confirmation::new(vec![plan], "h").awaiting_store();
        let lines = |_: &str, _: u32, _: u32| Some(vec!["old".to_owned()]);
        let removed = |c: &Confirmation| c.changes("h", lines)[0].removed.clone();
        assert_eq!(removed(&confirmation), Removed::Loading);

        let edits = confirmation.local_edits();
        // Edits that are not this confirmation's are ignored.
        confirmation.set_read(&[], Vec::new());
        assert_eq!(removed(&confirmation), Removed::Loading);

        confirmation.set_read(&edits, vec![Some(vec!["old".to_owned()])]);
        assert_eq!(removed(&confirmation), Removed::Lines(vec!["old".into()]));
        // The file at the head holds other lines than the diff on screen.
        confirmation.set_read(&edits, vec![Some(vec!["changed".to_owned()])]);
        assert_eq!(removed(&confirmation), Removed::NotLoaded);
        // The store could not be read.
        confirmation.set_read(&edits, vec![None]);
        assert_eq!(removed(&confirmation), Removed::NotLoaded);
    }

    #[test]
    fn commit_messages_count_the_suggestions() {
        assert_eq!(message(1), "Apply suggestion from code review");
        assert_eq!(message(3), "Apply 3 suggestions from code review");
    }

    #[test]
    fn bodies_split_at_their_suggestion_blocks() {
        let body = "Use this:

```suggestion
let x = 1;
```

then

> ```suggestion
quoted
> ```
";
        let parts = split_body(body);
        assert_eq!(parts.len(), 3, "{parts:?}");
        assert_eq!(
            parts[0],
            BodyPart::Markdown(
                "Use this:

"
            )
        );
        assert_eq!(
            parts[1],
            BodyPart::Suggestion(vec!["let x = 1;".to_owned()])
        );
        assert!(
            matches!(parts[2], BodyPart::Markdown(text) if text.contains("then") && text.contains("quoted"))
        );
        assert_eq!(split_body("plain"), [BodyPart::Markdown("plain")]);
        assert_eq!(
            split_body(
                "```suggestion
```
"
            ),
            [BodyPart::Suggestion(Vec::new())],
            "an empty suggestion deletes the lines"
        );
    }
}
