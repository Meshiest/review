//! What forge text (comment bodies, descriptions) may do when it is drawn.
//! Other people write it, so a link's display text can lie and an image URL
//! can name a local file.
//!
//! - Only `http` and `https` links are links; any other scheme or a relative
//!   path is drawn as plain text. A click opens the URL only after checking
//!   it again (a pull request the app can show opens in the app, see
//!   [`crate::references`]), and hovering a link shows where it really goes.
//! - Images are never loaded: each becomes a link to its URL labeled with its
//!   alt text, under the same rule, and anything left that still asks for an
//!   image (raw HTML) gets a source that loads nothing.

use std::borrow::Cow;
use std::ops::Range;
use std::sync::Arc;

use gpui::{Entity, ImageCacheError, ImageSource};
use gpui_kit::base::text::Link;
use gpui_kit::component::text::{TextView, TextViewState};
use markdown::mdast::Node;

/// The label of an image without alt text.
const IMAGE_LABEL: &str = "image";

/// Whether `url` is an absolute `http` or `https` URL with nothing that could
/// make a handler read it differently (whitespace or control characters).
pub fn is_web_url(url: &str) -> bool {
    if url.chars().any(|ch| ch.is_whitespace() || ch.is_control()) {
        return false;
    }
    let Some((scheme, rest)) = url.split_once(':') else {
        return false;
    };
    (scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https"))
        && rest.starts_with("//")
        && rest.len() > 2
}

/// The [`TextView::link`] hook: web links stay, everything else is text.
pub fn allowed_link(link: &Link) -> Option<Link> {
    is_web_url(&link.url).then(|| link.clone())
}

/// The image source for forge text: never a file, never the network.
pub fn blocked_image() -> ImageSource {
    ImageSource::Custom(Arc::new(|_, _| {
        Some(Err(ImageCacheError::Asset(
            "images in forge text are not loaded".into(),
        )))
    }))
}

/// A text view over forge markdown with the link and image rules applied.
/// Parse the text with [`markdown_source`] first.
pub fn text_view(state: &Entity<TextViewState>) -> TextView {
    TextView::new(state)
        .link(|link, _| allowed_link(link))
        .on_link_click(|url, _, _, cx| {
            if is_web_url(url) {
                crate::references::open_link(url, cx);
            }
        })
        .link_tooltip(|url, _, cx| crate::ui::link::tip(url.clone(), cx))
        .image_source(|_| blocked_image())
}

/// `text` with every markdown image turned into a link to its URL, labeled
/// with its alt text. An image inside a link becomes its alt text, so the
/// outer link stays one link.
pub fn markdown_source(text: &str) -> Cow<'_, str> {
    let Ok(root) = markdown::to_mdast(text, &parse_options()) else {
        return Cow::Borrowed(text);
    };
    let mut edits = Vec::new();
    collect_images(&root, text, false, &mut edits);
    if edits.is_empty() {
        return Cow::Borrowed(text);
    }
    edits.sort_by_key(|(range, _)| range.start);
    let mut out = String::with_capacity(text.len() + edits.len() * 8);
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

/// The constructs the text view parses with, so the images found here are
/// the ones it would draw.
pub(crate) fn parse_options() -> markdown::ParseOptions {
    let mut options = markdown::ParseOptions::gfm();
    options.constructs.math_text = true;
    options.constructs.math_flow = true;
    options
}

fn collect_images(node: &Node, text: &str, in_link: bool, edits: &mut Vec<(Range<usize>, String)>) {
    let span = node
        .position()
        .map(|p| p.start.offset..p.end.offset)
        .filter(|span| text.get(span.clone()).is_some());
    match (node, span) {
        (Node::Image(image), Some(span)) => {
            let label = escape_text(label(&image.alt));
            let replacement = if in_link {
                label
            } else {
                format!("[{label}](<{}>)", escape_destination(&image.url))
            };
            edits.push((span, replacement));
            return;
        }
        (Node::ImageReference(image), Some(span)) => {
            // `![alt][ref]` without its `!` is the same reference as a link.
            let replacement = if in_link {
                escape_text(label(&image.alt))
            } else if text[span.clone()].starts_with("![]") {
                format!("[{IMAGE_LABEL}]")
            } else {
                edits.push((span.start..span.start + 1, String::new()));
                return;
            };
            let end = if in_link {
                span.end
            } else {
                span.start + "![]".len()
            };
            edits.push((span.start..end, replacement));
            return;
        }
        _ => {}
    }
    let in_link = in_link || matches!(node, Node::Link(_) | Node::LinkReference(_));
    for child in node.children().into_iter().flatten() {
        collect_images(child, text, in_link, edits);
    }
}

fn label(alt: &str) -> &str {
    if alt.trim().is_empty() {
        IMAGE_LABEL
    } else {
        alt
    }
}

/// `text` without control characters other than line breaks and tabs, and
/// without the bidirectional controls that make text read in another order
/// than it is stored: what someone else's text keeps before it is shown.
pub fn strip_controls(text: &str) -> Cow<'_, str> {
    let unwanted = |ch: char| {
        (ch.is_control() && !matches!(ch, '\n' | '\t'))
            || matches!(ch, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
    };
    if text.contains(unwanted) {
        Cow::Owned(text.chars().filter(|&ch| !unwanted(ch)).collect())
    } else {
        Cow::Borrowed(text)
    }
}

/// Backslash-escapes markdown punctuation so `text` reads as plain text.
pub(crate) fn escape_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_ascii_punctuation() {
            out.push('\\');
        }
        if ch == '\n' || ch == '\r' {
            out.push(' ');
            continue;
        }
        out.push(ch);
    }
    out
}

/// A destination for the `<...>` form, which ends at the first `>` or line end.
pub(crate) fn escape_destination(url: &str) -> String {
    let mut out = String::with_capacity(url.len());
    for ch in url.chars() {
        match ch {
            '<' | '>' | '\\' => {
                out.push('\\');
                out.push(ch);
            }
            '\n' | '\r' => {}
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every link the text view would draw from `source` after the policy:
    /// (display text, target), reference links resolved, blocked ones left
    /// out.
    fn links(source: &str) -> Vec<(String, String)> {
        let rewritten = markdown_source(source);
        let root = markdown::to_mdast(&rewritten, &parse_options()).expect("parses");
        let mut definitions = Vec::new();
        collect_definitions(&root, &mut definitions);
        let mut found = Vec::new();
        walk(&root, &definitions, &mut found);
        found
    }

    fn collect_definitions(node: &Node, out: &mut Vec<(String, String)>) {
        if let Node::Definition(definition) = node {
            out.push((definition.identifier.clone(), definition.url.clone()));
        }
        for child in node.children().into_iter().flatten() {
            collect_definitions(child, out);
        }
    }

    fn walk(node: &Node, definitions: &[(String, String)], found: &mut Vec<(String, String)>) {
        let target = match node {
            Node::Link(link) => Some(link.url.clone()),
            Node::LinkReference(link) => definitions
                .iter()
                .find(|(id, _)| *id == link.identifier)
                .map(|(_, url)| url.clone()),
            Node::Image(_) | Node::ImageReference(_) => panic!("an image survived: {node:?}"),
            _ => None,
        };
        if let Some(target) = target {
            if allowed_link(&Link::new(target.clone())).is_some() {
                found.push((node.to_string(), target));
            }
            return;
        }
        for child in node.children().into_iter().flatten() {
            walk(child, definitions, found);
        }
    }

    #[test]
    fn only_web_schemes_are_links() {
        for url in [
            "http://example.com",
            "https://example.com/a?b#c",
            "HTTPS://EXAMPLE.COM",
        ] {
            assert!(is_web_url(url), "{url} is allowed");
        }
        for url in [
            "file:///C:/Windows/win.ini",
            "C:/Users/me/secret.png",
            "javascript:alert(1)",
            "ms-settings:privacy",
            "vscode://file/c:/x",
            "mailto:a@b.c",
            "data:text/html,<b>x</b>",
            "../relative/path",
            "/absolute/path",
            "//host/share",
            "https:",
            "https://",
            "https:example.com",
            "https://exa mple.com",
            " https://example.com",
            "https://example.com\u{0}",
            "",
        ] {
            assert!(!is_web_url(url), "{url:?} is blocked");
        }
    }

    #[test]
    fn the_hook_keeps_web_links_and_drops_the_rest() {
        let web = Link::new("https://example.com").title("t");
        assert_eq!(allowed_link(&web), Some(web.clone()));
        assert_eq!(allowed_link(&Link::new("file:///etc/passwd")), None);
        assert_eq!(allowed_link(&Link::new("javascript:alert(1)")), None);
        assert_eq!(allowed_link(&Link::new("./notes.md")), None);
    }

    #[test]
    fn a_link_that_lies_about_its_target_shows_the_real_one() {
        let source = "[https://github.com/safe](https://evil.example/phish)";
        assert_eq!(
            links(source),
            [(
                "https://github.com/safe".to_owned(),
                "https://evil.example/phish".to_owned()
            )]
        );
        // A display text that looks like a web link does not make a local
        // target clickable.
        assert!(links("[https://github.com](file:///C:/x.exe)").is_empty());
    }

    #[test]
    fn a_local_image_is_never_loaded_and_is_not_a_link() {
        let source = "see ![secret](C:/Users/me/secret.png) and ![](file:///etc/passwd)";
        let rewritten = markdown_source(source);
        assert_eq!(
            rewritten,
            "see [secret](<C:/Users/me/secret.png>) and [image](<file:///etc/passwd>)"
        );
        assert!(links(source).is_empty());
        assert!(matches!(blocked_image(), ImageSource::Custom(_)));
    }

    #[test]
    fn a_web_image_becomes_a_link_with_its_alt_text() {
        assert_eq!(
            links("![build *status*](https://ci.example/badge.svg \"title\")"),
            [(
                "build status".to_owned(),
                "https://ci.example/badge.svg".to_owned()
            )]
        );
    }

    #[test]
    fn a_linked_image_keeps_the_outer_link() {
        assert_eq!(
            links("[![badge](https://ci.example/b.svg)](https://ci.example/run)"),
            [("badge".to_owned(), "https://ci.example/run".to_owned())]
        );
    }

    #[test]
    fn reference_images_become_reference_links() {
        let source = "![logo][l] and ![][l]\n\n[l]: https://example.com/logo.png\n";
        assert_eq!(
            links(source),
            [
                ("logo".to_owned(), "https://example.com/logo.png".to_owned()),
                (
                    "image".to_owned(),
                    "https://example.com/logo.png".to_owned()
                )
            ]
        );
    }

    #[test]
    fn controls_are_stripped_but_lines_stay() {
        assert_eq!(strip_controls("a\tb\nc"), "a\tb\nc");
        assert_eq!(
            strip_controls("a\u{1b}[31mred\r\u{7}\u{202e}cba\u{2066}"),
            "a[31mredcba"
        );
        assert!(matches!(strip_controls("plain"), Cow::Borrowed(_)));
    }

    #[test]
    fn text_without_images_is_untouched() {
        let source = "plain `![not](an image)` text\n\n```\n![x](y)\n```\n";
        assert!(matches!(markdown_source(source), Cow::Borrowed(_)));
    }
}
