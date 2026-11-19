//! Every token of a built-in theme as a labeled swatch, sample text in its
//! interface and buffer fonts, and syntax-colored code on the surface and both
//! diff tints.
//!
//!     cargo run -p theme --example swatches -- <family> <mode> [--stay]
//!
//! Quits after 15 seconds unless `--stay` is given.

use std::time::Duration;

use gpui::{
    AnyElement, App, AppContext as _, Bounds, Context, Div, FontWeight, HighlightStyle, Hsla,
    IntoElement, ParentElement, Render, SharedString, Styled, StyledText, Window, WindowBounds,
    WindowOptions, div, px, size,
};
use theme::{BUILTIN, Color, Theme, ThemeData};

struct Swatches {
    data: ThemeData,
}

const SAMPLE: &[&[(&str, &str)]] = &[
    &[("comment", "/// Parses one hunk header.")],
    &[("attribute", "#[must_use]")],
    &[
        ("keyword", "pub fn "),
        ("function", "parse_hunk"),
        ("punctuation", "("),
        ("variable", "line"),
        ("punctuation", ": &"),
        ("type", "str"),
        ("punctuation", ") -> "),
        ("type", "Option"),
        ("punctuation", "<"),
        ("type", "Hunk"),
        ("punctuation", "> {"),
    ],
    &[
        ("plain", "    "),
        ("keyword", "let "),
        ("variable", "rest"),
        ("operator", " = "),
        ("variable", "line"),
        ("punctuation", "."),
        ("function", "strip_prefix"),
        ("punctuation", "("),
        ("string", "\"@@ -\""),
        ("punctuation", ")?;"),
    ],
    &[
        ("plain", "    "),
        ("keyword", "const "),
        ("constant", "MAX"),
        ("punctuation", ": "),
        ("type", "u32"),
        ("operator", " = "),
        ("number", "4_096"),
        ("punctuation", "; "),
        ("tag", "<b>"),
        ("plain", "x"),
        ("punctuation", "."),
        ("property", "len"),
    ],
];

fn syntax_color(theme: &Theme, kind: &str) -> Hsla {
    theme
        .syntax
        .entries()
        .find(|(k, _)| *k == kind)
        .map_or(theme.syntax.plain, |(_, c)| *c)
}

fn code_line(theme: &Theme, spans: &[(&str, &str)]) -> StyledText {
    let mut text = String::new();
    let mut runs = Vec::new();
    for (kind, part) in spans {
        let start = text.len();
        text.push_str(part);
        let style = HighlightStyle {
            color: Some(syntax_color(theme, kind)),
            ..HighlightStyle::default()
        };
        runs.push((start..text.len(), style));
    }
    StyledText::new(SharedString::from(text)).with_highlights(runs)
}

fn diff_block(theme: &Theme, label: &str, row: Hsla, gutter: Hsla, sign: &str) -> Div {
    let c = &theme.colors;
    let buffer = &theme.fonts.buffer;
    let rows = SAMPLE.iter().enumerate().map(|(i, spans)| {
        div()
            .flex()
            .h(buffer.line_height_px())
            .bg(row)
            .child(
                div()
                    .w(px(56.))
                    .px(px(6.))
                    .bg(gutter)
                    .text_color(c.gutter_text)
                    .child(format!("{:>3} {sign}", i + 1)),
            )
            .child(div().pl(px(8.)).child(code_line(theme, spans)))
    });
    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w(px(0.))
        .border_1()
        .border_color(c.border)
        .rounded(theme.radius.lg)
        .overflow_hidden()
        .bg(c.surface)
        .font_family(buffer.family())
        .text_size(buffer.size_px())
        .line_height(buffer.line_height_px())
        .child(
            div()
                .px(px(8.))
                .bg(c.diff_hunk_bg)
                .text_color(c.diff_hunk_text)
                .child(format!("@@ -1,5 +1,5 @@ {label}")),
        )
        .children(rows)
}

fn swatch(theme: &Theme, key: &str, color: Hsla, hex: Color) -> Div {
    let c = &theme.colors;
    div()
        .flex()
        .items_center()
        .gap(px(6.))
        .w(px(206.))
        .h(px(22.))
        .child(
            div()
                .w(px(30.))
                .h(px(18.))
                .flex_none()
                .rounded(theme.radius.sm)
                .border_1()
                .border_color(c.border_strong)
                .bg(color),
        )
        .child(div().flex_1().text_color(c.text).child(key.to_owned()))
        .child(
            div()
                .font_family(theme.fonts.buffer.family())
                .text_size(px(10.))
                .text_color(c.text_muted)
                .child(hex.to_string()),
        )
}

fn section<'a>(
    theme: &Theme,
    title: &str,
    entries: impl Iterator<Item = (&'a str, Hsla, Color)>,
) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(4.))
        .child(
            div()
                .text_color(theme.colors.text_muted)
                .font_weight(FontWeight::SEMIBOLD)
                .child(title.to_owned()),
        )
        .child(
            div()
                .flex()
                .flex_wrap()
                .gap_x(px(10.))
                .children(entries.map(|(k, c, hex)| swatch(theme, k, c, hex))),
        )
}

fn chips(theme: &Theme) -> Div {
    let c = &theme.colors;
    let states = [
        ("ok", c.ok),
        ("warn", c.warn),
        ("danger", c.danger),
        ("info", c.info),
        ("accent", c.accent_fg),
    ];
    div()
        .flex()
        .gap(px(8.))
        .children(states.into_iter().map(|(name, state)| {
            let chip = theme.chip_colors(state);
            div()
                .h(px(24.))
                .px(px(8.))
                .flex()
                .items_center()
                .rounded(theme.radius.md)
                .border_1()
                .border_color(chip.border)
                .bg(chip.fill)
                .text_color(chip.text)
                .text_size(px(12.))
                .child(format!("{name} chip"))
        }))
        .child(
            div()
                .h(px(24.))
                .px(px(10.))
                .flex()
                .items_center()
                .rounded(theme.radius.md)
                .bg(c.accent)
                .text_color(c.accent_text)
                .text_size(px(12.))
                .child("Primary"),
        )
        .child(
            div()
                .h(px(24.))
                .px(px(10.))
                .flex()
                .items_center()
                .rounded(theme.radius.md)
                .bg(c.danger)
                .text_color(c.state_text)
                .text_size(px(12.))
                .child("Danger"),
        )
        .child(
            div()
                .h(px(24.))
                .px(px(10.))
                .flex()
                .items_center()
                .rounded(theme.radius.md)
                .border_1()
                .border_color(c.control_border)
                .bg(c.field)
                .text_color(c.text_faint)
                .text_size(px(12.))
                .child("Field placeholder"),
        )
}

fn fonts(theme: &Theme) -> Div {
    let c = &theme.colors;
    let ui = &theme.fonts.interface;
    let buf = &theme.fonts.buffer;
    let line = |family: SharedString, size, lh, weight, color, text: String| -> AnyElement {
        div()
            .font_family(family)
            .text_size(size)
            .line_height(lh)
            .font_weight(weight)
            .text_color(color)
            .child(text)
            .into_any_element()
    };
    div()
        .flex()
        .flex_col()
        .gap(px(2.))
        .p(px(12.))
        .bg(c.surface_raised)
        .border_1()
        .border_color(c.border)
        .rounded(theme.radius.lg)
        .child(line(
            ui.family(),
            px(16.),
            px(22.),
            FontWeight::SEMIBOLD,
            c.text,
            format!(
                "{} {}px / {}px: Review pull requests at the speed of thought",
                ui.family, ui.size, ui.line_height
            ),
        ))
        .child(line(
            ui.family(),
            ui.size_px(),
            ui.line_height_px(),
            FontWeight::MEDIUM,
            c.text,
            "Medium: 3 files changed, 2 unresolved threads".into(),
        ))
        .child(line(
            ui.family(),
            ui.size_px(),
            ui.line_height_px(),
            FontWeight::NORMAL,
            c.text_secondary,
            "Secondary: the diff, conversation and checks open here.".into(),
        ))
        .child(line(
            ui.family(),
            ui.size_px(),
            ui.line_height_px(),
            FontWeight::NORMAL,
            c.text_muted,
            "Muted: opened 2 hours ago by cora".into(),
        ))
        .child(line(
            ui.family(),
            ui.size_px(),
            ui.line_height_px(),
            FontWeight::NORMAL,
            c.text_faint,
            "Faint: resolved".into(),
        ))
        .child(line(
            ui.family(),
            ui.size_px(),
            ui.line_height_px(),
            FontWeight::NORMAL,
            c.link,
            "Link: view on forge".into(),
        ))
        .child(line(
            buf.family(),
            buf.size_px(),
            buf.line_height_px(),
            FontWeight::NORMAL,
            c.text,
            format!(
                "{} {}px / {}px: fn main() {{ 0O1lI|{{}}[] -> => != }}",
                buf.family, buf.size, buf.line_height
            ),
        ))
        .child(line(
            buf.family(),
            buf.size_px(),
            buf.line_height_px(),
            FontWeight::MEDIUM,
            c.text,
            "Medium: git rebase --onto main feature~3".into(),
        ))
}

impl Render for Swatches {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme::current(cx);
        let c = &theme.colors;
        let data = &self.data;
        let colors = theme
            .colors
            .entries()
            .zip(data.colors.entries())
            .map(|((k, c), (_, hex))| (k, *c, *hex));
        let syntax = theme
            .syntax
            .entries()
            .zip(data.syntax.entries())
            .map(|((k, c), (_, hex))| (k, *c, *hex));
        let terminal = theme
            .terminal
            .entries()
            .zip(data.terminal.entries())
            .map(|((k, c), (_, hex))| (k, *c, *hex));
        div()
            .size_full()
            .flex()
            .flex_col()
            .gap(px(12.))
            .p(px(16.))
            .bg(c.background)
            .text_color(c.text)
            .font_family(theme.fonts.interface.family())
            .text_size(px(12.))
            .child(
                div()
                    .text_size(px(18.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(format!(
                        "{} {}  ·  radius {} / {} / {}  ·  chips {} / {}",
                        theme.name,
                        theme.mode,
                        data.radius.sm,
                        data.radius.md,
                        data.radius.lg,
                        data.chips.fill_alpha,
                        data.chips.border_alpha
                    )),
            )
            .child(section(theme, "colors", colors))
            .child(chips(theme))
            .child(
                div()
                    .flex()
                    .gap(px(12.))
                    .child(div().flex_1().child(section(theme, "syntax", syntax)))
                    .child(div().w(px(450.)).child(fonts(theme))),
            )
            .child(section(theme, "terminal", terminal))
            .child(
                div()
                    .flex()
                    .gap(px(12.))
                    .child(diff_block(theme, "context", c.surface, c.gutter_bg, " "))
                    .child(diff_block(
                        theme,
                        "added",
                        c.diff_add_bg,
                        c.diff_add_gutter,
                        "+",
                    ))
                    .child(diff_block(
                        theme,
                        "removed",
                        c.diff_remove_bg,
                        c.diff_remove_gutter,
                        "-",
                    )),
            )
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let family = args.first().map_or("vercel", String::as_str).to_lowercase();
    let mode = args.get(1).map_or("dark", String::as_str).to_lowercase();
    let stay = args.iter().any(|a| a == "--stay");
    let id = format!("{family}-{mode}");
    let Some(builtin) = BUILTIN.iter().find(|b| b.id == id) else {
        let ids: Vec<_> = BUILTIN.iter().map(|b| b.id).collect();
        eprintln!("swatches: no built-in theme {id}; expected one of {ids:?}");
        std::process::exit(2);
    };
    let data = ThemeData::parse(builtin.id, builtin.id, builtin.source).unwrap_or_else(|e| {
        eprintln!("swatches: {e}");
        std::process::exit(1);
    });

    gpui_kit::application().run(move |cx: &mut App| {
        gpui_kit::init(cx);
        theme::fonts::register(cx);
        theme::apply(cx, Theme::from_data(data.clone()));
        let bounds = Bounds::centered(None, size(px(1480.), px(1040.)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            ..WindowOptions::default()
        };
        gpui_kit::open_window(options, cx, move |_, cx| cx.new(|_| Swatches { data }))
            .expect("open the swatches window");
        cx.activate(true);
        if !stay {
            cx.spawn(async move |cx| {
                cx.background_executor()
                    .timer(Duration::from_secs(15))
                    .await;
                cx.update(|cx| cx.quit());
            })
            .detach();
        }
    });
}
