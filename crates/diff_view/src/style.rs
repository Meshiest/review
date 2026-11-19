use diff::HighlightKind;
use gpui::{Hsla, Pixels, SharedString};
use scroll::{AutoscrollStyle, ScrollStyle, ScrollbarStyle};

/// Colors and metrics for the diff list. Plain data with no defaults: the
/// caller maps its own theme onto it.
#[derive(Clone, Debug, PartialEq)]
pub struct DiffStyle {
    /// A monospace family.
    pub font_family: SharedString,
    pub font_size: Pixels,
    /// Height of code rows and hunk headers.
    pub line_height: Pixels,
    pub file_header_height: Pixels,
    /// Columns a tab advances to.
    pub tab_width: u32,

    pub background: Hsla,
    pub text: Hsla,
    pub gutter_background: Hsla,
    pub gutter_text: Hsla,
    pub added_background: Hsla,
    pub added_gutter_background: Hsla,
    pub removed_background: Hsla,
    pub removed_gutter_background: Hsla,
    /// Behind the changed words of an added line, over `added_background`.
    pub add_emphasis: Hsla,
    /// Behind the changed words of a removed line, over `removed_background`.
    pub remove_emphasis: Hsla,
    /// Split view's filler for the side a line does not exist on.
    pub empty_background: Hsla,
    pub file_header_background: Hsla,
    pub file_header_text: Hsla,
    /// The path of a file marked viewed.
    pub file_header_dim: Hsla,
    /// Outline of the unchecked viewed checkbox.
    pub checkbox_border: Hsla,
    /// Fill and outline of the checked viewed checkbox.
    pub checkbox_checked: Hsla,
    /// The tick on the checked viewed checkbox.
    pub checkbox_mark: Hsla,
    /// A file header's count of added lines and its change bar's blocks for
    /// them.
    pub stat_added: Hsla,
    /// Likewise for removed lines.
    pub stat_removed: Hsla,
    /// The change bar's blocks for neither.
    pub stat_empty: Hsla,
    pub hunk_header_background: Hsla,
    pub hunk_header_text: Hsla,
    pub block_background: Hsla,
    pub block_border: Hsla,
    pub divider: Hsla,
    /// The split divider while it is being dragged.
    pub divider_active: Hsla,
    /// Behind selected text.
    pub selection: Hsla,
    pub scrollbar: ScrollbarStyle,
    /// The anchor marker of middle-click autoscroll.
    pub autoscroll: AutoscrollStyle,
    /// Syntax colors, one per [`HighlightKind`].
    pub palette: SyntaxPalette,
}

/// One color per [`HighlightKind`]. `plain` is the color of unhighlighted code,
/// so it normally equals [`DiffStyle::text`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SyntaxPalette {
    pub keyword: Hsla,
    pub function: Hsla,
    pub type_: Hsla,
    pub string: Hsla,
    pub number: Hsla,
    pub comment: Hsla,
    pub constant: Hsla,
    pub operator: Hsla,
    pub punctuation: Hsla,
    pub property: Hsla,
    pub variable: Hsla,
    pub attribute: Hsla,
    pub tag: Hsla,
    pub plain: Hsla,
}

impl SyntaxPalette {
    /// Every kind in one color: code that is highlighted but not colored.
    #[must_use]
    pub fn uniform(color: Hsla) -> Self {
        Self {
            keyword: color,
            function: color,
            type_: color,
            string: color,
            number: color,
            comment: color,
            constant: color,
            operator: color,
            punctuation: color,
            property: color,
            variable: color,
            attribute: color,
            tag: color,
            plain: color,
        }
    }

    #[must_use]
    pub fn color(&self, kind: HighlightKind) -> Hsla {
        match kind {
            HighlightKind::Keyword => self.keyword,
            HighlightKind::Function => self.function,
            HighlightKind::Type => self.type_,
            HighlightKind::String => self.string,
            HighlightKind::Number => self.number,
            HighlightKind::Comment => self.comment,
            HighlightKind::Constant => self.constant,
            HighlightKind::Operator => self.operator,
            HighlightKind::Punctuation => self.punctuation,
            HighlightKind::Property => self.property,
            HighlightKind::Variable => self.variable,
            HighlightKind::Attribute => self.attribute,
            HighlightKind::Tag => self.tag,
            HighlightKind::Plain => self.plain,
        }
    }
}

impl DiffStyle {
    /// The scroll component's share of the style.
    #[must_use]
    pub fn scroll_style(&self) -> ScrollStyle {
        ScrollStyle {
            scrollbar: self.scrollbar,
            autoscroll: self.autoscroll,
        }
    }
}
