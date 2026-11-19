//! Grammars and their highlight configurations, built once per process.

use std::sync::OnceLock;

use tree_sitter::Language as TsLanguage;
use tree_sitter_highlight::HighlightConfiguration;

use super::{HighlightKind, Language};

/// Every grammar, including one that is only ever injected into another.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Grammar {
    Rust,
    TypeScript,
    Tsx,
    JavaScript,
    Python,
    Go,
    C,
    Cpp,
    CSharp,
    Java,
    Lua,
    Bash,
    Toml,
    Yaml,
    Json,
    Markdown,
    MarkdownInline,
    Html,
    Css,
    GdScript,
}

const GRAMMARS: usize = 20;

impl From<Language> for Grammar {
    fn from(language: Language) -> Self {
        match language {
            Language::Rust => Self::Rust,
            Language::TypeScript => Self::TypeScript,
            Language::Tsx => Self::Tsx,
            Language::JavaScript => Self::JavaScript,
            Language::Python => Self::Python,
            Language::Go => Self::Go,
            Language::C => Self::C,
            Language::Cpp => Self::Cpp,
            Language::CSharp => Self::CSharp,
            Language::Java => Self::Java,
            Language::Lua => Self::Lua,
            Language::Bash => Self::Bash,
            Language::Toml => Self::Toml,
            Language::Yaml => Self::Yaml,
            Language::Json => Self::Json,
            Language::Markdown => Self::Markdown,
            Language::Html => Self::Html,
            Language::Css => Self::Css,
            Language::GdScript => Self::GdScript,
        }
    }
}

impl Grammar {
    /// A grammar by the name an injection query or a code fence gives it.
    pub fn by_name(name: &str) -> Option<Self> {
        Some(match name.to_ascii_lowercase().as_str() {
            "rust" | "rs" => Self::Rust,
            "typescript" | "ts" => Self::TypeScript,
            "tsx" => Self::Tsx,
            "javascript" | "js" | "jsx" | "mjs" => Self::JavaScript,
            "python" | "py" => Self::Python,
            "go" | "golang" => Self::Go,
            "c" => Self::C,
            "cpp" | "c++" | "cxx" => Self::Cpp,
            "c_sharp" | "csharp" | "c#" | "cs" => Self::CSharp,
            "java" => Self::Java,
            "lua" => Self::Lua,
            "bash" | "sh" | "shell" | "zsh" => Self::Bash,
            "toml" => Self::Toml,
            "yaml" | "yml" => Self::Yaml,
            "json" | "jsonc" => Self::Json,
            "markdown" | "md" => Self::Markdown,
            "markdown_inline" => Self::MarkdownInline,
            "html" => Self::Html,
            "css" => Self::Css,
            "gdscript" | "gd" => Self::GdScript,
            _ => return None,
        })
    }

    /// The configuration, built on first use; `None` if its queries do not
    /// compile against the grammar, which leaves that language plain.
    pub fn config(self) -> Option<&'static HighlightConfiguration> {
        static CONFIGS: [OnceLock<Option<HighlightConfiguration>>; GRAMMARS] =
            [const { OnceLock::new() }; GRAMMARS];
        CONFIGS[self as usize].get_or_init(|| self.build()).as_ref()
    }

    fn build(self) -> Option<HighlightConfiguration> {
        let (language, name, highlights, injections): (TsLanguage, &str, String, &str) = match self
        {
            Self::Rust => (
                tree_sitter_rust::LANGUAGE.into(),
                "rust",
                tree_sitter_rust::HIGHLIGHTS_QUERY.into(),
                tree_sitter_rust::INJECTIONS_QUERY,
            ),
            Self::TypeScript => (
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
                "typescript",
                [
                    tree_sitter_javascript::HIGHLIGHT_QUERY,
                    tree_sitter_typescript::HIGHLIGHTS_QUERY,
                ]
                .join("\n"),
                "",
            ),
            Self::Tsx => (
                tree_sitter_typescript::LANGUAGE_TSX.into(),
                "tsx",
                [
                    tree_sitter_javascript::HIGHLIGHT_QUERY,
                    tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
                    tree_sitter_typescript::HIGHLIGHTS_QUERY,
                ]
                .join("\n"),
                "",
            ),
            Self::JavaScript => (
                tree_sitter_javascript::LANGUAGE.into(),
                "javascript",
                [
                    tree_sitter_javascript::HIGHLIGHT_QUERY,
                    tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
                ]
                .join("\n"),
                tree_sitter_javascript::INJECTIONS_QUERY,
            ),
            Self::Python => (
                tree_sitter_python::LANGUAGE.into(),
                "python",
                tree_sitter_python::HIGHLIGHTS_QUERY.into(),
                "",
            ),
            Self::Go => (
                tree_sitter_go::LANGUAGE.into(),
                "go",
                tree_sitter_go::HIGHLIGHTS_QUERY.into(),
                "",
            ),
            Self::C => (
                tree_sitter_c::LANGUAGE.into(),
                "c",
                tree_sitter_c::HIGHLIGHT_QUERY.into(),
                "",
            ),
            Self::Cpp => (
                tree_sitter_cpp::LANGUAGE.into(),
                "cpp",
                [
                    tree_sitter_c::HIGHLIGHT_QUERY,
                    tree_sitter_cpp::HIGHLIGHT_QUERY,
                ]
                .join("\n"),
                "",
            ),
            Self::CSharp => (
                tree_sitter_c_sharp::LANGUAGE.into(),
                "c_sharp",
                tree_sitter_c_sharp::HIGHLIGHTS_QUERY.into(),
                "",
            ),
            Self::Java => (
                tree_sitter_java::LANGUAGE.into(),
                "java",
                tree_sitter_java::HIGHLIGHTS_QUERY.into(),
                "",
            ),
            Self::Lua => (
                tree_sitter_lua::LANGUAGE.into(),
                "lua",
                tree_sitter_lua::HIGHLIGHTS_QUERY.into(),
                "",
            ),
            Self::Bash => (
                tree_sitter_bash::LANGUAGE.into(),
                "bash",
                tree_sitter_bash::HIGHLIGHT_QUERY.into(),
                "",
            ),
            Self::Toml => (
                tree_sitter_toml_ng::LANGUAGE.into(),
                "toml",
                tree_sitter_toml_ng::HIGHLIGHTS_QUERY.into(),
                "",
            ),
            Self::Yaml => (
                tree_sitter_yaml::LANGUAGE.into(),
                "yaml",
                tree_sitter_yaml::HIGHLIGHTS_QUERY.into(),
                "",
            ),
            Self::Json => (
                tree_sitter_json::LANGUAGE.into(),
                "json",
                tree_sitter_json::HIGHLIGHTS_QUERY.into(),
                "",
            ),
            Self::Markdown => (
                tree_sitter_md::LANGUAGE.into(),
                "markdown",
                tree_sitter_md::HIGHLIGHT_QUERY_BLOCK.into(),
                tree_sitter_md::INJECTION_QUERY_BLOCK,
            ),
            Self::MarkdownInline => (
                tree_sitter_md::INLINE_LANGUAGE.into(),
                "markdown_inline",
                tree_sitter_md::HIGHLIGHT_QUERY_INLINE.into(),
                tree_sitter_md::INJECTION_QUERY_INLINE,
            ),
            Self::Html => (
                tree_sitter_html::LANGUAGE.into(),
                "html",
                tree_sitter_html::HIGHLIGHTS_QUERY.into(),
                tree_sitter_html::INJECTIONS_QUERY,
            ),
            Self::Css => (
                tree_sitter_css::LANGUAGE.into(),
                "css",
                tree_sitter_css::HIGHLIGHTS_QUERY.into(),
                "",
            ),
            Self::GdScript => (
                tree_sitter_gdscript::LANGUAGE.into(),
                "gdscript",
                include_str!("gdscript.scm").into(),
                "",
            ),
        };
        let mut config =
            HighlightConfiguration::new(language, name, &highlights, injections, "").ok()?;
        let names: Vec<&str> = CAPTURES.iter().map(|&(name, _)| name).collect();
        config.configure(&names);
        Some(config)
    }
}

/// The capture names we color. The highlighter matches a capture to the
/// longest of these that is a dotted prefix of it.
const CAPTURES: &[(&str, HighlightKind)] = &[
    ("attribute", HighlightKind::Attribute),
    ("boolean", HighlightKind::Constant),
    ("comment", HighlightKind::Comment),
    ("constant", HighlightKind::Constant),
    ("constructor", HighlightKind::Type),
    ("embedded", HighlightKind::Plain),
    ("function", HighlightKind::Function),
    ("keyword", HighlightKind::Keyword),
    ("label", HighlightKind::Property),
    ("module", HighlightKind::Type),
    ("namespace", HighlightKind::Type),
    ("number", HighlightKind::Number),
    ("operator", HighlightKind::Operator),
    ("property", HighlightKind::Property),
    ("punctuation", HighlightKind::Punctuation),
    ("string", HighlightKind::String),
    ("string.escape", HighlightKind::Constant),
    ("tag", HighlightKind::Tag),
    ("type", HighlightKind::Type),
    ("variable", HighlightKind::Variable),
    ("variable.builtin", HighlightKind::Constant),
    ("variable.member", HighlightKind::Property),
    ("text.title", HighlightKind::Keyword),
    ("text.literal", HighlightKind::String),
    ("text.uri", HighlightKind::Constant),
    ("text.reference", HighlightKind::Function),
    ("text.emphasis", HighlightKind::Plain),
    ("text.strong", HighlightKind::Plain),
];

/// What a `Highlight(ix)` from the highlighter means.
pub(super) fn kind_of(ix: usize) -> HighlightKind {
    CAPTURES
        .get(ix)
        .map_or(HighlightKind::Plain, |&(_, kind)| kind)
}
