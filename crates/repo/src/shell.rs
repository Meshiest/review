//! Quoting for the POSIX shell `ssh` hands a remote command to.

use std::borrow::Cow;

/// `text` as one word of a POSIX shell command line (sh, bash, zsh, dash):
/// left as is when it holds only characters no shell treats specially,
/// otherwise in single quotes, with each `'` written as `'\''`.
#[must_use]
pub fn quote(text: &str) -> Cow<'_, str> {
    let plain = !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"%+,-./:=@_".contains(&b));
    // A leading `=` is an expansion in zsh, `~` in every shell.
    if plain && !text.starts_with('=') {
        return Cow::Borrowed(text);
    }
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('\'');
    for ch in text.chars() {
        if ch == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(ch);
        }
    }
    quoted.push('\'');
    Cow::Owned(quoted)
}

/// Words joined into one command line, each quoted.
#[must_use]
pub fn join<'a>(words: impl IntoIterator<Item = &'a str>) -> String {
    words.into_iter().map(quote).collect::<Vec<_>>().join(" ")
}

/// A remote path as `git -C` gets it: `~/x` becomes `x`, which a command
/// `ssh` starts resolves against the home directory, since a quoted `~` is
/// not expanded.
#[must_use]
pub fn home_relative(path: &str) -> &str {
    match path {
        "~" | "~/" => ".",
        _ => path.strip_prefix("~/").unwrap_or(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_words_stay_plain() {
        for word in ["git", "-C", "refs/review/pr/4", "a.b-c_d", "x=1", "host:22"] {
            assert_eq!(quote(word), word);
        }
    }

    #[test]
    fn everything_else_is_single_quoted() {
        assert_eq!(quote(""), "''");
        assert_eq!(quote("a b"), "'a b'");
        assert_eq!(quote("~/x"), "'~/x'");
        assert_eq!(quote("=cmd"), "'=cmd'");
        assert_eq!(quote("$(rm -rf /)"), "'$(rm -rf /)'");
        assert_eq!(quote("it's"), "'it'\\''s'");
        assert_eq!(quote("a;b|c&d`e`"), "'a;b|c&d`e`'");
        assert_eq!(quote("line\nbreak"), "'line\nbreak'");
        assert_eq!(join(["git", "-C", "my repo"]), "git -C 'my repo'");
    }

    #[test]
    fn home_paths_become_relative() {
        assert_eq!(home_relative("~/.cache/review/x"), ".cache/review/x");
        assert_eq!(home_relative("~"), ".");
        assert_eq!(home_relative("/srv/x"), "/srv/x");
        assert_eq!(home_relative("dev/x"), "dev/x");
    }

    /// The quoted word reaches the program as the text it came from, through
    /// a real POSIX shell, where there is one.
    #[test]
    fn a_shell_reads_back_what_was_quoted() {
        let Ok(output) = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                "printf '%s\\n' {}",
                join(["it's", "a b", "$HOME", "`x`", "~", "=y", "\"q\""])
            ))
            .output()
        else {
            return;
        };
        if !output.status.success() {
            return;
        }
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "it's\na b\n$HOME\n`x`\n~\n=y\n\"q\"\n"
        );
    }
}
