//! Syntax highlighting of whole blobs with tree-sitter.
//!
//! [`highlight`] parses one blob and returns per-line [`Span`]s, so a string
//! or comment that spans lines is colored on every line. [`Highlighter`] runs
//! that on a thread pool and caches the results by blob id; nothing here
//! touches a UI.

mod grammar;
mod language;
mod service;

use std::cell::RefCell;

use tree_sitter_highlight::{HighlightEvent, Highlighter as TsHighlighter};

use crate::Text;
use grammar::{Grammar, kind_of};
pub use language::Language;
pub use service::{Highlighter, Wake};

/// Blobs over this many bytes stay plain.
pub const MAX_BYTES: usize = 2 * 1024 * 1024;
/// Blobs over this many lines stay plain.
pub const MAX_LINES: u32 = 50_000;

/// What a run of code is, coarse enough for a theme to give each its color.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum HighlightKind {
    Keyword,
    Function,
    Type,
    String,
    Number,
    Comment,
    Constant,
    Operator,
    Punctuation,
    Property,
    Variable,
    Attribute,
    Tag,
    #[default]
    Plain,
}

impl HighlightKind {
    pub const ALL: [Self; 14] = [
        Self::Keyword,
        Self::Function,
        Self::Type,
        Self::String,
        Self::Number,
        Self::Comment,
        Self::Constant,
        Self::Operator,
        Self::Punctuation,
        Self::Property,
        Self::Variable,
        Self::Attribute,
        Self::Tag,
        Self::Plain,
    ];
}

/// A colored byte range inside one line (without its line ending).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub start: u32,
    pub end: u32,
    pub kind: HighlightKind,
}

/// Per-line spans for one blob. Only non-plain runs are stored; the gaps
/// between spans are plain. Spans of a line are sorted, do not overlap and
/// never reach past the end of the line.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Highlights {
    /// `lines[i]..lines[i + 1]` indexes `spans` for line `i`; empty when plain.
    lines: Vec<u32>,
    spans: Vec<Span>,
}

impl Highlights {
    #[must_use]
    pub fn plain() -> Self {
        Self::default()
    }

    /// True when nothing is colored (an oversize blob, an unknown grammar).
    pub fn is_plain(&self) -> bool {
        self.spans.is_empty()
    }

    /// The spans of line `ix`, empty if it has none.
    pub fn line(&self, ix: u32) -> &[Span] {
        let ix = ix as usize;
        match (self.lines.get(ix), self.lines.get(ix + 1)) {
            (Some(&from), Some(&to)) => &self.spans[from as usize..to as usize],
            _ => &[],
        }
    }

    pub fn span_count(&self) -> usize {
        self.spans.len()
    }

    /// Rough heap size, for cache budgets.
    pub fn byte_size(&self) -> usize {
        self.spans.len() * size_of::<Span>() + self.lines.len() * size_of::<u32>()
    }
}

/// Whether a blob is too big to color.
pub fn is_oversize(text: &Text) -> bool {
    text.as_bytes().len() > MAX_BYTES || text.line_count() > MAX_LINES
}

/// Whether a blob is left plain: too big, or not UTF-8 (its lines are shown
/// lossily, so span offsets could not match what is painted).
pub fn stays_plain(text: &Text) -> bool {
    !text.is_utf8() || is_oversize(text)
}

thread_local! {
    /// Parser state is reused across blobs on each thread.
    static PARSER: RefCell<TsHighlighter> = RefCell::new(TsHighlighter::new());
}

/// Highlights one blob. Plain when the blob is over [`MAX_BYTES`] or
/// [`MAX_LINES`], is not UTF-8, or when the language's queries failed to load. Takes
/// milliseconds for a big blob: call it off the UI thread.
#[must_use]
pub fn highlight(text: &Text, language: Language) -> Highlights {
    if stays_plain(text) {
        return Highlights::plain();
    }
    run(text, language).unwrap_or_default()
}

fn run(text: &Text, language: Language) -> Option<Highlights> {
    let config = Grammar::from(language).config()?;
    PARSER.with(|parser| {
        let mut parser = parser.borrow_mut();
        let events = parser
            .highlight(config, text.as_bytes(), None, None, |name| {
                Grammar::by_name(name).and_then(Grammar::config)
            })
            .ok()?;
        let mut builder = Builder::new(text);
        let mut stack: Vec<HighlightKind> = Vec::new();
        for event in events {
            match event.ok()? {
                HighlightEvent::HighlightStart(h) => stack.push(kind_of(h.0)),
                HighlightEvent::HighlightEnd => {
                    stack.pop();
                }
                HighlightEvent::Source { start, end } => {
                    let kind = stack.last().copied().unwrap_or_default();
                    builder.push(start, end, kind);
                }
            }
        }
        Some(builder.finish())
    })
}

/// Turns byte ranges over a blob (which arrive in order) into per-line spans.
struct Builder<'a> {
    text: &'a Text,
    starts: &'a [u32],
    lines: Vec<u32>,
    spans: Vec<Span>,
    /// The line the last range ended in.
    line: usize,
}

impl<'a> Builder<'a> {
    fn new(text: &'a Text) -> Self {
        Self {
            text,
            starts: text.line_starts(),
            lines: Vec::new(),
            spans: Vec::new(),
            line: 0,
        }
    }

    fn push(&mut self, start: usize, end: usize, kind: HighlightKind) {
        if kind == HighlightKind::Plain || self.starts.is_empty() {
            return;
        }
        let end = end.min(self.text.as_bytes().len());
        let mut at = start;
        while at < end {
            while self
                .starts
                .get(self.line + 1)
                .is_some_and(|&next| next as usize <= at)
            {
                self.line += 1;
            }
            let line_start = self.starts[self.line] as usize;
            let content_end = line_start + self.text.line_bytes(self.line as u32).len();
            if at < content_end {
                let stop = end.min(content_end);
                self.emit(line_start, at, stop, kind);
                at = stop;
            }
            // Skip the line ending, or whatever else is left past the content.
            match self.starts.get(self.line + 1) {
                Some(&next) if at < end => at = at.max(next as usize),
                _ => break,
            }
        }
    }

    fn emit(&mut self, line_start: usize, from: usize, to: usize, kind: HighlightKind) {
        while self.lines.len() <= self.line {
            self.lines.push(self.spans.len() as u32);
        }
        let (from, to) = ((from - line_start) as u32, (to - line_start) as u32);
        let first = self.lines[self.line] as usize;
        if let Some(last) = self.spans[first..].last_mut()
            && last.kind == kind
            && last.end == from
        {
            last.end = to;
            return;
        }
        self.spans.push(Span {
            start: from,
            end: to,
            kind,
        });
    }

    fn finish(mut self) -> Highlights {
        while self.lines.len() <= self.starts.len() {
            self.lines.push(self.spans.len() as u32);
        }
        self.spans.shrink_to_fit();
        Highlights {
            lines: self.lines,
            spans: self.spans,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(h: &Highlights, text: &Text, line: u32) -> Vec<(String, HighlightKind)> {
        let src = text.line(line);
        h.line(line)
            .iter()
            .map(|s| (src[s.start as usize..s.end as usize].to_owned(), s.kind))
            .collect()
    }

    #[test]
    fn multi_line_string_is_colored_on_every_line() {
        let text = Text::new("let s = \"one\ntwo\nthree\";\nlet n = 1;\n");
        let h = highlight(&text, Language::Rust);
        let string = |line: u32| {
            h.line(line)
                .iter()
                .filter(|s| s.kind == HighlightKind::String)
                .map(|s| (s.start, s.end))
                .collect::<Vec<_>>()
        };
        assert_eq!(string(0), [(8, 12)], "{:?}", kinds(&h, &text, 0));
        assert_eq!(string(1), [(0, 3)], "{:?}", kinds(&h, &text, 1));
        assert_eq!(string(2), [(0, 6)], "{:?}", kinds(&h, &text, 2));
        assert!(string(3).is_empty());
    }

    #[test]
    fn block_comment_is_colored_on_every_line() {
        let text = Text::new("fn f() {\n    /* a\n       b\n    */\n    1\n}\n");
        let h = highlight(&text, Language::Rust);
        for (line, from, to) in [(1, 4, 8), (2, 0, 8), (3, 0, 6)] {
            let comment: Vec<_> = h
                .line(line)
                .iter()
                .filter(|s| s.kind == HighlightKind::Comment)
                .collect();
            assert!(
                comment.iter().any(|s| s.start == from && s.end == to),
                "line {line}: {:?}",
                kinds(&h, &text, line)
            );
        }
        assert!(
            !h.line(4).iter().any(|s| s.kind == HighlightKind::Comment),
            "code after the comment is not a comment"
        );
    }

    #[test]
    fn spans_never_cross_line_ends() {
        let sources = [
            (
                "fn main() {\r\n    let s = \"a\r\nb\";\r\n    // c\r\n}\r\n",
                Language::Rust,
            ),
            (
                "x = '''\nmulti\nline\n'''\nprint(x)\n# done",
                Language::Python,
            ),
            (
                "# T\n\n```rust\nfn x() {}\n```\n\n*em* `code`\n",
                Language::Markdown,
            ),
            (
                "<script>\nlet a = 1;\n</script>\n<style>\na { color: red }\n</style>",
                Language::Html,
            ),
        ];
        for (source, language) in sources {
            let text = Text::new(source);
            let h = highlight(&text, language);
            assert!(!h.is_plain(), "{language:?}");
            for line in 0..text.line_count() {
                let len = text.line(line).len() as u32;
                let mut at = 0;
                for span in h.line(line) {
                    assert!(
                        span.start < span.end,
                        "{language:?} line {line}: empty span"
                    );
                    assert!(span.start >= at, "{language:?} line {line}: overlap");
                    assert!(span.end <= len, "{language:?} line {line}: past line end");
                    assert!(text.line(line).is_char_boundary(span.start as usize));
                    assert!(text.line(line).is_char_boundary(span.end as usize));
                    at = span.end;
                }
            }
        }
    }

    #[test]
    fn oversize_blobs_stay_plain() {
        let long_lines = Text::new("let a = 1;\n".repeat(MAX_LINES as usize + 1));
        assert!(highlight(&long_lines, Language::Rust).is_plain());

        let big = Text::new(format!("// {}\n", "x".repeat(MAX_BYTES)));
        assert!(highlight(&big, Language::Rust).is_plain());

        let fine = Text::new("let a = 1;\n".repeat(100));
        assert!(!highlight(&fine, Language::Rust).is_plain());
    }

    #[test]
    fn every_language_highlights_a_sample() {
        let samples = [
            (Language::Rust, "fn main() { let x: u32 = 1; } // c"),
            (
                Language::TypeScript,
                "const x: number = 1; function f(a: string) {}",
            ),
            (Language::Tsx, "const a = <div className=\"x\">{b}</div>;"),
            (
                Language::JavaScript,
                "const x = 1; function f(a) { return \"s\"; }",
            ),
            (Language::Python, "def f(a):\n    return \"s\" # c\n"),
            (Language::Go, "package main\nfunc main() { x := 1 }\n"),
            (Language::C, "int main(void) { return 0; } // c"),
            (Language::Cpp, "class A { public: int f() const; };\n// c"),
            (Language::CSharp, "class A { int F() { return 1; } } // c"),
            (Language::Java, "class A { int f() { return 1; } } // c"),
            (
                Language::Lua,
                "local x = 1 -- c\nfunction f() return \"s\" end",
            ),
            (Language::Bash, "if [ -f x ]; then echo \"hi\"; fi # c"),
            (Language::Toml, "[package]\nname = \"x\"\nn = 1 # c"),
            (Language::Yaml, "key: value\nlist:\n  - 1 # c\n"),
            (Language::Json, "{\"a\": [1, true, null, \"s\"]}"),
            (Language::Markdown, "# Title\n\nSome `code` and *em*.\n"),
            (Language::Html, "<div class=\"a\">hi</div>"),
            (Language::Css, "a { color: red; } /* c */"),
            (
                Language::GdScript,
                "extends Node\nfunc _ready():\n\tvar x = 1 # c\n",
            ),
        ];
        assert_eq!(samples.len(), Language::ALL.len());
        for (language, source) in samples {
            let text = Text::new(source);
            let h = highlight(&text, language);
            assert!(!h.is_plain(), "{language:?} produced no spans");
        }
    }

    #[test]
    fn non_utf8_blobs_stay_plain() {
        let bytes: std::sync::Arc<[u8]> = std::sync::Arc::from(&b"let a = 1; // \xff\n"[..]);
        let text = Text::from_parts(bytes, std::sync::Arc::from([0]));
        assert!(highlight(&text, Language::Rust).is_plain());
        assert!(stays_plain(&text));
    }

    #[test]
    fn empty_and_blank_blobs_are_fine() {
        for source in ["", "\n", "\n\n\n", "   "] {
            let text = Text::new(source);
            let h = highlight(&text, Language::Rust);
            for line in 0..text.line_count() + 2 {
                assert!(h.line(line).is_empty());
            }
        }
    }
}
