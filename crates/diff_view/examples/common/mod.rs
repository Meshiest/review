//! Shared setup for the diff_view examples.

use std::cell::Cell;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use diff::synth::{SynthConfig, synthesize};
use diff::{
    Anchor, Block, BlockId, Diff, DiffList, FileDiff, FileIx, Hunk, Language, Layout, RowHeights,
    Segment, Side, Text,
};
use diff_view::{DiffStyle, DiffView, Rail, SyntaxPalette};
use gpui::{
    AnyElement, App, Bounds, Hsla, InteractiveElement as _, IntoElement, ParentElement as _,
    Pixels, Styled as _, WindowBounds, WindowOptions, div, prelude::FluentBuilder as _, px, rgb,
    size,
};
use scroll::{AutoscrollStyle, ScrollbarStyle};
use theme::{FontSettings, Mode, Theme, ThemeData, ThemeRegistry};

pub const SEED: u64 = 0x5eed_d1ff;

pub struct Args {
    pub rows: usize,
    pub layout: Layout,
    pub frames: usize,
    /// Frames of the autoscroll run after the scroll run; 0 skips it.
    pub autoscroll_frames: usize,
    /// Skip highlighting, to compare against plain text.
    pub plain: bool,
    /// Four real files, colored by file type: a small diff for the demo.
    pub real: bool,
    /// Synthetic text instead of real source, colored as Rust.
    pub synthetic: bool,
    /// Directories searched for `.rs` files; empty for this repository's
    /// `crates` and `.scratch/oracle/ripgrep` when present.
    pub source: Vec<PathBuf>,
    /// A theme file to take the colors (and, unless overridden, the buffer
    /// font) from; without one the examples use a plain built-in style.
    pub theme: Option<PathBuf>,
    /// Buffer font family, overriding the theme's.
    pub font: Option<String>,
    /// Buffer font size in px, overriding the theme's; the line height
    /// scales with it.
    pub font_size: Option<f32>,
    /// Switch to the other mode of the theme (and back) every this many
    /// recorded frames, to time `set_style`.
    pub switch_style_every: Option<usize>,
    /// A thread card under the last line of every this many hunks.
    pub blocks: Option<usize>,
    /// Make a text selection across a few rows once the first frame is
    /// up, and print what a copy would give. For the demo's screenshot.
    pub demo_select: bool,
    /// Mark the file at the top of the view viewed and scroll to where its
    /// header is pinned over its code, the next file's header coming up.
    pub demo_sticky: bool,
    /// The left pane's share of the width in split layout.
    pub split_ratio: Option<f32>,
}

impl Args {
    /// `--rows N`, `--split`, `--frames N`, `--autoscroll-frames N`,
    /// `--plain`, `--real`, `--synthetic`, `--source DIR` (repeatable),
    /// `--theme FILE`, `--font FAMILY`, `--font-size PX`,
    /// `--switch-style-every N`.
    pub fn parse() -> Self {
        let mut args = Self {
            rows: 100_000,
            layout: Layout::Unified,
            frames: 1200,
            autoscroll_frames: 600,
            plain: false,
            real: false,
            synthetic: false,
            source: Vec::new(),
            theme: None,
            font: None,
            font_size: None,
            switch_style_every: None,
            blocks: None,
            demo_select: false,
            demo_sticky: false,
            split_ratio: None,
        };
        let mut it = std::env::args().skip(1);
        while let Some(arg) = it.next() {
            let mut number = |name: &str| -> usize {
                it.next()
                    .and_then(|v| v.replace('_', "").parse().ok())
                    .unwrap_or_else(|| panic!("{name} takes a number"))
            };
            match arg.as_str() {
                "--rows" => args.rows = number("--rows"),
                "--frames" => args.frames = number("--frames"),
                "--autoscroll-frames" => args.autoscroll_frames = number("--autoscroll-frames"),
                "--split" => args.layout = Layout::Split,
                "--plain" => args.plain = true,
                "--demo-select" => args.demo_select = true,
                "--demo-sticky" => args.demo_sticky = true,
                "--split-ratio" => {
                    args.split_ratio = Some(
                        it.next()
                            .and_then(|v| v.parse().ok())
                            .unwrap_or_else(|| panic!("--split-ratio takes a number")),
                    );
                }
                "--real" => args.real = true,
                "--synthetic" => args.synthetic = true,
                "--switch-style-every" => {
                    args.switch_style_every = Some(number("--switch-style-every").max(1));
                }
                "--blocks" => args.blocks = Some(number("--blocks").max(1)),
                "--source" => args.source.push(
                    it.next()
                        .unwrap_or_else(|| panic!("--source takes a directory"))
                        .into(),
                ),
                "--theme" => {
                    args.theme = Some(
                        it.next()
                            .unwrap_or_else(|| panic!("--theme takes a theme file"))
                            .into(),
                    );
                }
                "--font" => {
                    args.font = Some(
                        it.next()
                            .unwrap_or_else(|| panic!("--font takes a family name")),
                    );
                }
                "--font-size" => {
                    args.font_size = Some(
                        it.next()
                            .and_then(|v| v.parse().ok())
                            .unwrap_or_else(|| panic!("--font-size takes a number of px")),
                    );
                }
                other => panic!(
                    "unknown argument {other}; expected --rows N, --split, --frames N, \
                     --autoscroll-frames N, --plain, --real, --synthetic, --source DIR, \
                     --theme FILE, --font FAMILY, --font-size PX, --switch-style-every N, \
                     --blocks N, --demo-select, --demo-sticky, --split-ratio R"
                ),
            }
        }
        args
    }

    /// What the diff is made of, for reports.
    pub fn content(&self) -> &'static str {
        match (self.real, self.synthetic, self.plain) {
            (true, ..) => "real",
            (_, true, true) => "synthetic_plain",
            (_, true, false) => "synthetic",
            (_, false, true) => "source_plain",
            (_, false, false) => "source",
        }
    }
}

/// The styles an example runs with, and what to call its metrics.
#[allow(dead_code, reason = "each example reads only the fields it needs")]
pub struct Styles {
    pub first: DiffStyle,
    /// The other mode of the theme, for `--switch-style-every`.
    pub alternate: Option<DiffStyle>,
    /// Appended to metric names (`@vercel-dark@Geist Mono`); empty unless
    /// `--theme` or `--font` is given.
    pub suffix: String,
    pub font_family: String,
    pub font_size: f32,
}

/// Builds the styles from the flags and registers the bundled fonts.
pub fn styles(args: &Args, cx: &App) -> Styles {
    theme::fonts::register(cx);
    let theme = args.theme.as_ref().map(|path| load_theme(path));
    let mut font = theme.as_ref().map_or_else(
        || FontSettings {
            family: if cfg!(windows) {
                "Consolas"
            } else {
                "DejaVu Sans Mono"
            }
            .to_owned(),
            size: 13.0,
            line_height: 20,
            fallbacks: Vec::new(),
        },
        |(_, theme)| theme.fonts.buffer.clone(),
    );
    if let Some(family) = &args.font {
        font.family.clone_from(family);
    }
    if let Some(size) = args.font_size {
        let scaled = (size * font.line_height as f32 / font.size)
            .round()
            .max(1.0);
        font.line_height = scaled as u32;
        font.size = size;
    }
    let first = theme
        .as_ref()
        .map_or_else(|| fallback_style(&font), |(_, t)| theme_style(t, &font));
    let alternate = args.switch_style_every.map(|_| {
        let (_, theme) = theme
            .as_ref()
            .expect("--switch-style-every needs --theme, to find the other mode");
        let other = if theme.is_dark() {
            Mode::Light
        } else {
            Mode::Dark
        };
        let registry = ThemeRegistry::builtin();
        let other = registry
            .find(&theme.name, other)
            .unwrap_or_else(|| panic!("no built-in {other} mode of the {} theme", theme.name));
        theme_style(other, &font)
    });
    let suffix = if args.theme.is_some() || args.font.is_some() {
        let id = theme.as_ref().map_or("default", |(id, _)| id.as_str());
        format!("@{id}@{}", font.family)
    } else {
        String::new()
    };
    Styles {
        first,
        alternate,
        suffix,
        font_family: font.family,
        font_size: font.size,
    }
}

/// The theme in `path` and its id (the file stem).
fn load_theme(path: &Path) -> (String, Theme) {
    let source = fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let id = path
        .file_stem()
        .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
    let data = ThemeData::parse(&id, &path.display().to_string(), &source)
        .unwrap_or_else(|e| panic!("{e}"));
    (id, Theme::from_data(data))
}

/// The same mapping the app uses, from a theme to a diff style.
fn theme_style(theme: &Theme, font: &FontSettings) -> DiffStyle {
    let c = &theme.colors;
    let s = &theme.syntax;
    DiffStyle {
        font_family: font.family(),
        font_size: font.size_px(),
        line_height: font.line_height_px(),
        file_header_height: px(32.0),
        tab_width: 4,
        background: c.surface,
        text: s.plain,
        gutter_background: c.gutter_bg,
        gutter_text: c.gutter_text,
        added_background: c.diff_add_bg,
        added_gutter_background: c.diff_add_gutter,
        removed_background: c.diff_remove_bg,
        removed_gutter_background: c.diff_remove_gutter,
        add_emphasis: c.diff_add_emphasis,
        remove_emphasis: c.diff_remove_emphasis,
        empty_background: c.diff_empty_bg,
        file_header_background: c.surface_raised,
        file_header_text: c.text,
        file_header_dim: c.text_muted,
        checkbox_border: c.control_border,
        checkbox_checked: c.accent,
        checkbox_mark: c.accent_text,
        stat_added: c.ok,
        stat_removed: c.danger,
        stat_empty: c.border_strong,
        hunk_header_background: c.diff_hunk_bg,
        hunk_header_text: c.diff_hunk_text,
        block_background: c.surface_raised,
        block_border: c.border_strong,
        divider: c.border,
        divider_active: c.accent,
        selection: c.selection,
        scrollbar: ScrollbarStyle {
            thumb: c.border_strong,
            ..ScrollbarStyle::default()
        },
        autoscroll: AutoscrollStyle::default(),
        palette: SyntaxPalette {
            keyword: s.keyword,
            function: s.function,
            type_: s.type_,
            string: s.string,
            number: s.number,
            comment: s.comment,
            constant: s.constant,
            operator: s.operator,
            punctuation: s.punctuation,
            property: s.property,
            variable: s.variable,
            attribute: s.attribute,
            tag: s.tag,
            plain: s.plain,
        },
    }
}

/// A plain dark style for runs without `--theme`; the app takes its colors
/// from its theme.
fn fallback_style(font: &FontSettings) -> DiffStyle {
    let c = |hex: u32| Hsla::from(rgb(hex));
    DiffStyle {
        font_family: font.family(),
        font_size: font.size_px(),
        line_height: font.line_height_px(),
        file_header_height: px(34.0),
        tab_width: 4,
        background: c(0x14161a),
        text: c(0xd4d2cf),
        gutter_background: c(0x191c21),
        gutter_text: c(0x6b7079),
        added_background: c(0x15301f),
        added_gutter_background: c(0x1b3a26),
        removed_background: c(0x3a1a1d),
        removed_gutter_background: c(0x472024),
        add_emphasis: c(0x236b3a),
        remove_emphasis: c(0x7a2a31),
        empty_background: c(0x101215),
        file_header_background: c(0x22262d),
        file_header_text: c(0xe8e6e3),
        file_header_dim: c(0x7d818a),
        checkbox_border: c(0x6b7079),
        checkbox_checked: c(0x4a7bd0),
        checkbox_mark: c(0xffffff),
        stat_added: c(0x4fb36b),
        stat_removed: c(0xd2555f),
        stat_empty: c(0x353a44),
        hunk_header_background: c(0x1a2230),
        hunk_header_text: c(0x7d93b3),
        block_background: c(0x1e2127),
        block_border: c(0x353a44),
        divider: c(0x2a2e35),
        divider_active: c(0x4a7bd0),
        selection: c(0x2c4a73),
        scrollbar: ScrollbarStyle::default(),
        autoscroll: AutoscrollStyle::default(),
        palette: SyntaxPalette {
            keyword: c(0xc792ea),
            function: c(0x82aaff),
            type_: c(0xffcb6b),
            string: c(0xc3e88d),
            number: c(0xf78c6c),
            comment: c(0x697098),
            constant: c(0xf78c6c),
            operator: c(0x89ddff),
            punctuation: c(0x9aa5b8),
            property: c(0x80cbc4),
            variable: c(0xd4d2cf),
            attribute: c(0xffcb6b),
            tag: c(0xf07178),
            plain: c(0xd4d2cf),
        },
    }
}

/// The diff with at least `rows` rows in `layout`: real source files unless
/// `--synthetic` or `--real`, highlighted by file type (as Rust for synthetic
/// text) unless `plain`.
pub fn view(args: &Args, style: DiffStyle) -> DiffView {
    let (layout, plain) = (args.layout, args.plain);
    let mut diff = if args.real {
        real_diff()
    } else if args.synthetic {
        synthesize(&SynthConfig::rows(SEED, layout, args.rows))
    } else {
        source_diff(args, &style)
    };
    let mut rails = Vec::new();
    if let Some(every) = args.blocks {
        let (blocks, block_rails) = thread_blocks(&diff, every, &style);
        diff.blocks = blocks;
        rails = block_rails;
    }
    let card_style = style.clone();
    let mut view = DiffView::new(diff, layout, style);
    if args.blocks.is_some() {
        view.set_rails(rails);
        view.set_block_renderer(Rc::new(move |id, width, _, _| {
            thread_card(id, width, &card_style)
        }));
    }
    if !plain && args.synthetic && !args.real {
        view.set_language(Some(Language::Rust));
    }
    view
}

/// A thread under the last new line of every `every`-th hunk, alternating
/// single lines and three-line ranges, with a rail each; every fifth is
/// drawn dashed, as an outdated one.
fn thread_blocks(diff: &Diff, every: usize, style: &DiffStyle) -> (Vec<Block>, Vec<Option<Rail>>) {
    let mut blocks = Vec::new();
    let mut rails = Vec::new();
    let mut hunk_count = 0usize;
    for (file_ix, file) in diff.files.iter().enumerate() {
        for hunk in &file.hunks {
            hunk_count += 1;
            if !hunk_count.is_multiple_of(every) || hunk.new_len() == 0 {
                continue;
            }
            let line = hunk.new_start + hunk.new_len() - 1;
            let n = blocks.len();
            let mut block = Block::new(
                Anchor::Line {
                    file: FileIx(file_ix as u32),
                    side: Side::New,
                    line,
                },
                120.0,
            );
            if n % 2 == 1 {
                block.first_line = Some(line.saturating_sub(2).max(hunk.new_start));
            }
            blocks.push(block);
            rails.push(Some(Rail {
                color: if n % 5 == 4 {
                    style.gutter_text
                } else {
                    style.hunk_header_text
                },
                dashed: n % 5 == 4,
                elbow: px(CARD_TOP + CARD_HEADER / 2.0),
            }));
        }
    }
    (blocks, rails)
}

const CARD_TOP: f32 = 6.0;
const CARD_HEADER: f32 = 30.0;
const AUTHORS: [&str; 5] = ["mira", "tobias", "wen", "priya", "ansel"];
const BODIES: [&str; 6] = [
    "This loop can overflow for large exponents. Consider checked_mul, or saturating_mul if a clamp is what callers expect here.",
    "Nice simplification. Can you add a test for the empty case? The old code returned early and this path now walks the whole list.",
    "Will do, tracking it in a follow-up so this one stays small.",
    "Why drop the plain subtraction? It read fine and the wrapping version hides the case where the counter goes backwards.",
    "Prefer `% 2` here, it reads better and the compiler emits the same instruction either way.",
    "Is the lock still needed now that the cache is per thread? If not, removing it would also drop the poisoning branch below.",
];
const CODE: [&str; 3] = [
    "let height = (line_height * rows).ceil();",
    "if b == 0 { return None; }",
    "acc = acc.checked_mul(base)?;",
];

/// A card shaped like the app's thread card: one to three comments with an
/// avatar, author, time and a wrapped body, some with a code line; every
/// fourth is a collapsed one-line row.
fn thread_card(id: BlockId, width: Pixels, style: &DiffStyle) -> AnyElement {
    let n = id.0 as usize;
    let collapsed = n % 4 == 3;
    let comments = 1 + n % 3;
    let hover = style.hunk_header_background;
    let mut card = div()
        .id(("card", n))
        .flex()
        .flex_col()
        .border_1()
        .border_color(style.block_border)
        .rounded_md()
        .bg(style.block_background)
        .text_color(style.text)
        .text_size(px(13.0))
        .overflow_hidden();
    if collapsed {
        card = card.child(
            div()
                .id(("collapsed", n))
                .h(px(CARD_HEADER))
                .px(px(10.0))
                .flex()
                .gap(px(8.0))
                .items_center()
                .hover(move |s| s.bg(hover))
                .child(avatar(AUTHORS[n % AUTHORS.len()], style))
                .child(div().flex_none().child(AUTHORS[n % AUTHORS.len()]))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(style.gutter_text)
                        .child(BODIES[n % BODIES.len()]),
                )
                .child(
                    div()
                        .flex_none()
                        .text_color(style.gutter_text)
                        .child(format!("{comments} comments, resolved by mira")),
                ),
        );
    } else {
        for k in 0..comments {
            let author = AUTHORS[(n + k) % AUTHORS.len()];
            let body = BODIES[(n + k) % BODIES.len()];
            let entry = div()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .px(px(10.0))
                .py(px(6.0))
                .when(k > 0, |el| el.border_t_1().border_color(style.divider))
                .child(
                    div()
                        .h(px(CARD_HEADER - 12.0))
                        .flex()
                        .gap(px(8.0))
                        .items_center()
                        .child(avatar(author, style))
                        .child(div().child(author))
                        .child(
                            div()
                                .text_color(style.gutter_text)
                                .child(format!("{}h", k + 1)),
                        )
                        .when(k == 0, |el| {
                            el.child(
                                div()
                                    .text_color(style.gutter_text)
                                    .child(format!("line {}", n + 10)),
                            )
                        }),
                )
                .child(div().pl(px(28.0)).child(body))
                .when((n + k).is_multiple_of(3), |el| {
                    el.child(
                        div()
                            .ml(px(28.0))
                            .px(px(6.0))
                            .py(px(2.0))
                            .bg(style.hunk_header_background)
                            .font_family(style.font_family.clone())
                            .child(CODE[(n + k) % CODE.len()]),
                    )
                });
            card = card.child(entry);
        }
    }
    div()
        .w(width)
        .pt(px(CARD_TOP))
        .pb(px(8.0))
        .pr(px(12.0))
        .child(card)
        .into_any_element()
}

fn avatar(author: &str, style: &DiffStyle) -> impl IntoElement {
    div()
        .size(px(20.0))
        .flex_none()
        .rounded_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(style.hunk_header_background)
        .text_color(style.hunk_header_text)
        .text_size(px(11.0))
        .child(author[..1].to_uppercase())
}

/// The `.rs` files under the source directories as `(path, text)`, sorted by
/// path so a run always sees the same files.
fn collect_sources(args: &Args) -> Vec<(String, String)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let roots = if args.source.is_empty() {
        vec![root.join("crates"), root.join(".scratch/oracle/ripgrep")]
    } else {
        args.source.clone()
    };
    let mut files = Vec::new();
    let mut pending: Vec<PathBuf> = roots.into_iter().filter(|dir| dir.is_dir()).collect();
    while let Some(dir) = pending.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let name = entry.file_name();
                if !matches!(name.to_str(), Some("target" | ".git" | "node_modules")) {
                    pending.push(path);
                }
            } else if path.extension().is_some_and(|ext| ext == "rs")
                && let Ok(text) = fs::read_to_string(&path)
            {
                files.push((path.to_string_lossy().replace('\\', "/"), text));
            }
        }
    }
    files.sort();
    files
}

/// Real source repeated until the projection has at least `args.rows` rows.
/// Each repeat appends a line, so its blobs are new to the highlighter and
/// cached highlights do not make later copies free.
fn source_diff(args: &Args, style: &DiffStyle) -> Diff {
    let sources = collect_sources(args);
    assert!(!sources.is_empty(), "no .rs files found; pass --source DIR");
    let pass = |copy: usize| -> Vec<FileDiff> {
        sources
            .iter()
            .map(|(path, text)| {
                let name = if args.plain {
                    format!("copy{copy}/{path}.txt")
                } else {
                    format!("copy{copy}/{path}")
                };
                if copy == 0 {
                    source_file(name, text)
                } else {
                    source_file(name, &format!("{text}\n// copy {copy}\n"))
                }
            })
            .collect()
    };
    let heights = RowHeights {
        code: style.line_height.into(),
        file_header: style.file_header_height.into(),
    };
    let first = pass(0);
    let rows_per_pass = DiffList::new(
        Diff {
            files: first.clone(),
            blocks: Vec::new(),
        },
        args.layout,
        heights,
    )
    .len()
    .max(1);
    let copies = args.rows.div_ceil(rows_per_pass).max(1);
    let lines: usize = sources.iter().map(|(_, text)| text.lines().count()).sum();
    eprintln!(
        "source: {} files, {lines} lines, {rows_per_pass} rows per pass, {copies} copies",
        sources.len()
    );
    let mut files = first;
    for copy in 1..copies {
        files.extend(pass(copy));
    }
    Diff {
        files,
        blocks: Vec::new(),
    }
}

/// Appends `lines` to both sides, counting them.
struct Sides {
    old: String,
    new: String,
    old_count: u32,
    new_count: u32,
}

impl Sides {
    fn both(&mut self, lines: &[&str]) {
        for line in lines {
            self.old.push_str(line);
            self.old.push('\n');
            self.new.push_str(line);
            self.new.push('\n');
        }
        self.old_count += lines.len() as u32;
        self.new_count += lines.len() as u32;
    }
}

/// One file as a diff with a change every few lines: context, one line
/// replaced by two, context, in hunks of several changes with unchanged
/// stretches between them, so nearly every source line is a row.
fn source_file(path: String, source: &str) -> FileDiff {
    const LEAD: usize = 6;
    const TRAIL: usize = 3;
    const CHANGES_PER_HUNK: usize = 8;
    const GAP: usize = 20;
    let lines: Vec<&str> = source.lines().collect();
    let fits = |at: usize| at + LEAD + 2 + TRAIL <= lines.len();
    let mut sides = Sides {
        old: String::new(),
        new: String::new(),
        old_count: 0,
        new_count: 0,
    };
    let mut hunks = Vec::new();
    let mut at = 0;
    while fits(at) {
        let (old_start, new_start) = (sides.old_count, sides.new_count);
        let mut segments = Vec::new();
        let mut context = 0u32;
        for _ in 0..CHANGES_PER_HUNK {
            if !fits(at) {
                break;
            }
            sides.both(&lines[at..at + LEAD]);
            segments.push(Segment::Context(context + LEAD as u32));
            at += LEAD;
            sides.old.push_str(lines[at]);
            sides.old.push('\n');
            sides.old_count += 1;
            sides.new.push_str(lines[at]);
            sides.new.push_str(" //\n");
            sides.new.push_str(lines[at + 1]);
            sides.new.push('\n');
            sides.new_count += 2;
            segments.push(Segment::Change {
                removed: 1,
                added: 2,
            });
            at += 2;
            sides.both(&lines[at..at + TRAIL]);
            at += TRAIL;
            context = TRAIL as u32;
        }
        segments.push(Segment::Context(context));
        hunks.push(Hunk {
            old_start,
            new_start,
            segments,
        });
        let gap = GAP.min(lines.len() - at);
        sides.both(&lines[at..at + gap]);
        at += gap;
    }
    sides.both(&lines[at..]);
    FileDiff {
        path,
        old: Text::new(sides.old),
        new: Text::new(sides.new),
        hunks,
    }
}

/// Real source files shown as a diff: every 30 lines two lines are added and
/// one is replaced, so the colors can be judged on code that parses.
fn real_diff() -> Diff {
    let sources = [
        (
            "crates/diff/src/rows.rs",
            include_str!("../../../diff/src/rows.rs"),
        ),
        ("Cargo.toml", include_str!("../../../../Cargo.toml")),
        (
            "scripts/checks.py",
            include_str!("../../../../scripts/checks.py"),
        ),
        ("docs/design.md", include_str!("../../../../docs/design.md")),
    ];
    Diff {
        files: sources
            .into_iter()
            .map(|(path, source)| real_file(path, source))
            .collect(),
        blocks: Vec::new(),
    }
}

/// An earlier version of `line`: its last word of three letters or more
/// swapped for another, so the pair shares most of its words. Lines without
/// such a word get a stand-in comment.
fn edited(line: &str) -> String {
    let mut words = line
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|w| w.len() >= 3);
    match words.next_back() {
        Some(word) => {
            let at = line.rfind(word).unwrap_or(0);
            format!("{}previous{}", &line[..at], &line[at + word.len()..])
        }
        None => "    // replaced line, old version".to_owned(),
    }
}

fn real_file(path: &str, source: &str) -> FileDiff {
    const SHARED: usize = 30;
    const CONTEXT: usize = 3;
    let lines: Vec<&str> = source.lines().collect();
    let (mut old, mut new) = (String::new(), String::new());
    let (old_lines, new_lines) = (Cell::new(0u32), Cell::new(0u32));
    let mut hunks = Vec::new();
    let mut at = 0;
    let mut push = |text: &str, old_side: bool, new_side: bool| {
        if old_side {
            old.push_str(text);
            old.push('\n');
            old_lines.set(old_lines.get() + 1);
        }
        if new_side {
            new.push_str(text);
            new.push('\n');
            new_lines.set(new_lines.get() + 1);
        }
    };
    while at < lines.len() {
        let shared = SHARED.min(lines.len() - at);
        for line in &lines[at..at + shared] {
            push(line, true, true);
        }
        at += shared;
        let lead = CONTEXT.min(shared) as u32;
        let (old_start, new_start) = (old_lines.get() - lead, new_lines.get() - lead);
        let added = 2.min(lines.len() - at);
        let replaced = lines.get(at).map_or_else(String::new, |l| edited(l));
        push(&replaced, true, false);
        for line in &lines[at..at + added] {
            push(line, false, true);
        }
        at += added;
        let trail = CONTEXT.min(lines.len() - at);
        for line in &lines[at..at + trail] {
            push(line, true, true);
        }
        at += trail;
        hunks.push(Hunk {
            old_start,
            new_start,
            segments: vec![
                Segment::Context(lead),
                Segment::Change {
                    removed: 1,
                    added: added as u32,
                },
                Segment::Context(trail as u32),
            ],
        });
    }
    FileDiff {
        path: path.to_owned(),
        old: Text::new(old),
        new: Text::new(new),
        hunks,
    }
}

/// A centered window that gpui never throttles, so frame pacing is the
/// display's even when another window has focus.
pub fn window_options(cx: &App, title: &str) -> WindowOptions {
    let bounds = Bounds::centered(None, size(px(1400.0), px(1000.0)), cx);
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(gpui::TitlebarOptions {
            title: Some(title.to_owned().into()),
            ..Default::default()
        }),
        focus: true,
        inactive_frame_interval: None,
        low_power_frame_interval: None,
        thermal_frame_interval: None,
        ..Default::default()
    }
}
