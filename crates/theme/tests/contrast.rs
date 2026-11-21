//! WCAG 2.x contrast of every built-in theme: text, muted text, accent text,
//! state colors on their chips and every syntax color on the surface and both
//! diff tints reach 4.5:1; focus rings, control outlines, faint text and line
//! numbers reach 3:1. Translucent tokens are composited over the surface they
//! sit on first.

use theme::{BUILTIN, Color, ThemeData};

const TEXT: f64 = 4.5;
const NON_TEXT: f64 = 3.0;

struct Pair {
    fg: String,
    bg: String,
    ratio: f64,
    need: f64,
}

fn pairs(t: &ThemeData) -> Vec<Pair> {
    let c = &t.colors;
    let on_surface = |tint: Color| tint.over(c.surface);
    let mut out = Vec::new();
    let mut check = |fg_name: &str, fg: Color, bg_name: &str, bg: Color, need: f64| {
        out.push(Pair {
            fg: fg_name.to_owned(),
            bg: bg_name.to_owned(),
            ratio: fg.contrast_on(bg),
            need,
        });
    };

    for (bg_name, bg) in [
        ("background", c.background),
        ("surface", c.surface),
        ("surface_raised", c.surface_raised),
        ("surface_menu", c.surface_menu),
    ] {
        check("text", c.text, bg_name, bg, TEXT);
        check("text_secondary", c.text_secondary, bg_name, bg, TEXT);
        check("text_muted", c.text_muted, bg_name, bg, TEXT);
        check("text_faint", c.text_faint, bg_name, bg, NON_TEXT);
    }
    check("text", c.text, "field", c.field, TEXT);
    check("accent_text", c.accent_text, "accent", c.accent, TEXT);
    check("accent_fg", c.accent_fg, "surface", c.surface, TEXT);
    check(
        "accent_fg",
        c.accent_fg,
        "selected",
        on_surface(c.selected),
        TEXT,
    );
    check("link", c.link, "surface", c.surface, TEXT);
    for (name, state) in [
        ("ok", c.ok),
        ("warn", c.warn),
        ("danger", c.danger),
        ("info", c.info),
    ] {
        let chip = on_surface(state.with_alpha(t.chips.fill_alpha));
        check(name, state, &format!("{name} chip"), chip, TEXT);
        check(name, state, "surface_raised", c.surface_raised, TEXT);
        check("state_text", c.state_text, name, state, TEXT);
    }
    for (bg_name, bg) in [
        ("hover", c.hover),
        ("selected", c.selected),
        ("active", c.active),
        ("find_match", c.find_match),
        ("find_current", c.find_current),
    ] {
        check("text", c.text, bg_name, on_surface(bg), TEXT);
    }
    for (bg_name, bg) in [
        ("surface", c.surface),
        ("diff_add_bg", c.diff_add_bg),
        ("diff_remove_bg", c.diff_remove_bg),
    ] {
        for (kind, color) in t.syntax.entries() {
            check(
                &format!("syntax.{kind}"),
                *color,
                bg_name,
                on_surface(bg),
                TEXT,
            );
        }
    }
    for (bg_name, bg) in [
        ("diff_add_emphasis", c.diff_add_emphasis),
        ("diff_remove_emphasis", c.diff_remove_emphasis),
    ] {
        check(
            "syntax.plain",
            t.syntax.plain,
            bg_name,
            on_surface(bg),
            TEXT,
        );
    }
    check(
        "diff_hunk_text",
        c.diff_hunk_text,
        "diff_hunk_bg",
        on_surface(c.diff_hunk_bg),
        TEXT,
    );
    // Line numbers are faint text.
    for (bg_name, bg) in [
        ("gutter_bg", c.gutter_bg),
        ("diff_add_gutter", c.diff_add_gutter),
        ("diff_remove_gutter", c.diff_remove_gutter),
    ] {
        check(
            "gutter_text",
            c.gutter_text,
            bg_name,
            on_surface(bg),
            NON_TEXT,
        );
    }
    check("focus", c.focus, "surface", c.surface, NON_TEXT);
    check("focus", c.focus, "background", c.background, NON_TEXT);
    check(
        "control_border",
        c.control_border,
        "surface",
        c.surface,
        NON_TEXT,
    );
    check(
        "control_border",
        c.control_border,
        "field",
        c.field,
        NON_TEXT,
    );

    let term = &t.terminal;
    let ground = term.background;
    check(
        "terminal.foreground",
        term.foreground,
        "terminal.background",
        ground,
        TEXT,
    );
    let ansi = term.ansi();
    let names = ["red", "green", "yellow", "blue", "magenta", "cyan"];
    for (i, name) in names.iter().enumerate() {
        check(
            &format!("terminal.{name}"),
            ansi[i + 1],
            "terminal.background",
            ground,
            TEXT,
        );
        check(
            &format!("terminal.bright_{name}"),
            ansi[i + 9],
            "terminal.background",
            ground,
            NON_TEXT,
        );
    }
    check(
        "terminal.bright_black",
        term.bright_black,
        "terminal.background",
        ground,
        NON_TEXT,
    );
    out
}

#[test]
fn every_builtin_theme_meets_wcag_aa() {
    let mut failures = 0;
    for builtin in BUILTIN {
        let data = ThemeData::parse(builtin.id, builtin.id, builtin.source)
            .unwrap_or_else(|e| panic!("{e}"));
        let mut pairs = pairs(&data);
        pairs.sort_by(|a, b| (a.ratio / a.need).total_cmp(&(b.ratio / b.need)));
        let below = pairs.iter().filter(|p| p.ratio < p.need).count();
        failures += below;
        println!(
            "{} ({} pairs, {below} below threshold), tightest:",
            builtin.id,
            pairs.len()
        );
        for p in pairs.iter().take(6) {
            let mark = if p.ratio < p.need { "FAIL" } else { "    " };
            println!(
                "  {mark} {:5.2} / {:.1}  {} on {}",
                p.ratio, p.need, p.fg, p.bg
            );
        }
    }
    assert_eq!(
        failures, 0,
        "pairs below their WCAG threshold (see the table above)"
    );
}
