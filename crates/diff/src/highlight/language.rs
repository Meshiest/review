/// A language the highlighter has a grammar for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Language {
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
    Html,
    Css,
    GdScript,
}

impl Language {
    /// Every language, for tests and tools.
    pub const ALL: [Self; 19] = [
        Self::Rust,
        Self::TypeScript,
        Self::Tsx,
        Self::JavaScript,
        Self::Python,
        Self::Go,
        Self::C,
        Self::Cpp,
        Self::CSharp,
        Self::Java,
        Self::Lua,
        Self::Bash,
        Self::Toml,
        Self::Yaml,
        Self::Json,
        Self::Markdown,
        Self::Html,
        Self::Css,
        Self::GdScript,
    ];

    /// The language of a file by its name and extension; `None` stays plain
    /// (`Dockerfile`, `Makefile` and anything unknown).
    #[must_use]
    pub fn detect(path: &str) -> Option<Self> {
        let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
        if let Some(language) = by_file_name(name) {
            return Some(language);
        }
        let (stem, ext) = name.rsplit_once('.')?;
        if stem.is_empty() {
            return None;
        }
        by_extension(&ext.to_ascii_lowercase())
    }
}

fn by_file_name(name: &str) -> Option<Language> {
    Some(match name {
        "Cargo.lock" | "Pipfile" | "poetry.lock" | "uv.lock" => Language::Toml,
        ".bashrc" | ".bash_profile" | ".bash_logout" | ".profile" | ".zshrc" | ".zshenv"
        | ".zprofile" | "PKGBUILD" => Language::Bash,
        ".babelrc" | ".eslintrc" | ".prettierrc" | ".swcrc" => Language::Json,
        "SConstruct" | "SConscript" | "BUILD.bazel" | "BUILD" | "WORKSPACE" | "Snakefile" => {
            Language::Python
        }
        _ => return None,
    })
}

fn by_extension(ext: &str) -> Option<Language> {
    Some(match ext {
        "rs" => Language::Rust,
        "ts" | "mts" | "cts" => Language::TypeScript,
        "tsx" => Language::Tsx,
        "js" | "mjs" | "cjs" | "jsx" => Language::JavaScript,
        "py" | "pyi" | "pyw" | "bzl" => Language::Python,
        "go" => Language::Go,
        "c" | "h" => Language::C,
        "cc" | "cpp" | "cxx" | "c++" | "hh" | "hpp" | "hxx" | "h++" | "ipp" | "inl" => {
            Language::Cpp
        }
        "cs" => Language::CSharp,
        "java" => Language::Java,
        "lua" => Language::Lua,
        "sh" | "bash" | "zsh" | "ksh" => Language::Bash,
        "toml" => Language::Toml,
        "yml" | "yaml" => Language::Yaml,
        "json" | "jsonc" | "json5" | "geojson" | "webmanifest" => Language::Json,
        "md" | "markdown" | "mdx" => Language::Markdown,
        "html" | "htm" | "xhtml" => Language::Html,
        "css" => Language::Css,
        "gd" => Language::GdScript,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_by_extension_and_name() {
        let cases = [
            ("src/main.rs", Some(Language::Rust)),
            ("a/b/App.TSX", Some(Language::Tsx)),
            ("web/index.ts", Some(Language::TypeScript)),
            ("web/index.mjs", Some(Language::JavaScript)),
            ("tool.py", Some(Language::Python)),
            ("main.go", Some(Language::Go)),
            ("x.h", Some(Language::C)),
            ("x.hpp", Some(Language::Cpp)),
            ("Program.cs", Some(Language::CSharp)),
            ("A.java", Some(Language::Java)),
            ("init.lua", Some(Language::Lua)),
            ("run.sh", Some(Language::Bash)),
            ("Cargo.toml", Some(Language::Toml)),
            ("Cargo.lock", Some(Language::Toml)),
            ("ci.yml", Some(Language::Yaml)),
            ("package.json", Some(Language::Json)),
            ("README.md", Some(Language::Markdown)),
            ("index.html", Some(Language::Html)),
            ("style.css", Some(Language::Css)),
            ("player.gd", Some(Language::GdScript)),
            (".bashrc", Some(Language::Bash)),
            ("C:\\repo\\src\\lib.rs", Some(Language::Rust)),
            ("Dockerfile", None),
            ("Makefile", None),
            ("notes.txt", None),
            (".gitignore", None),
            ("rs", None),
        ];
        for (path, want) in cases {
            assert_eq!(Language::detect(path), want, "{path}");
        }
    }
}
