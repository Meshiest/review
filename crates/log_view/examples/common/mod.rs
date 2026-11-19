//! Shared setup for the log_view examples: a realistic CI log generator, the
//! style, and the window.

use gpui::{App, Bounds, WindowBounds, WindowOptions, px, size};
use log_view::LogStyle;
use theme::ThemeRegistry;

/// A deterministic CI log: groups of build and test output with ANSI colors,
/// timestamps, long lines, carriage-return progress and the odd error.
pub struct Generator {
    rng: u64,
    /// Seconds into the day, and the fraction in 100 ns units.
    second: u64,
    fraction: u64,
    in_group: bool,
    group_left: usize,
    group: usize,
}

const WORDS: [&str; 16] = [
    "resolving",
    "checking",
    "linking",
    "restoring",
    "cache",
    "artifact",
    "toolchain",
    "dependency",
    "registry",
    "manifest",
    "target",
    "workspace",
    "incremental",
    "fingerprint",
    "metadata",
    "profile",
];
const CRATES: [&str; 12] = [
    "serde",
    "tokio",
    "regex",
    "gpui",
    "memchr",
    "vte",
    "syn",
    "quote",
    "libc",
    "bytes",
    "once_cell",
    "async-channel",
];

impl Generator {
    pub fn new(seed: u64) -> Self {
        Self {
            rng: seed | 1,
            second: 12 * 3600,
            fraction: 0,
            in_group: false,
            group_left: 0,
            group: 0,
        }
    }

    fn next(&mut self) -> u64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        self.rng
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn timestamp(&mut self) -> String {
        self.fraction += 10_000 + self.next() % 400_000;
        self.second += self.fraction / 10_000_000;
        self.fraction %= 10_000_000;
        let s = self.second % 86_400;
        format!(
            "2024-05-01T{:02}:{:02}:{:02}.{:07}Z",
            s / 3600,
            s / 60 % 60,
            s % 60,
            self.fraction
        )
    }

    fn word(&mut self) -> &'static str {
        WORDS[self.below(WORDS.len())]
    }

    fn words(&mut self, count: usize) -> String {
        (0..count)
            .map(|_| self.word())
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The text of one line inside a group.
    fn body(&mut self) -> String {
        match self.below(100) {
            0..=29 => {
                let name = CRATES[self.below(CRATES.len())];
                let (a, b, c) = (self.below(3), self.below(20), self.below(30));
                format!("   \x1b[32;1mCompiling\x1b[0m {name} v{a}.{b}.{c}")
            }
            30..=44 => {
                let word = self.word();
                let n = self.below(500);
                format!("test {word}::case_{n} ... \x1b[32mok\x1b[0m")
            }
            45..=49 => {
                let (name, line) = (self.word(), self.below(900));
                format!(
                    "\x1b[1m\x1b[33mwarning\x1b[0m\x1b[1m: unused variable: `{name}`\x1b[0m\n  \
                     \x1b[1m\x1b[38;5;12m--> \x1b[0msrc/{name}.rs:{line}:9"
                )
            }
            50..=51 => "\x1b[1m\x1b[31merror[E0308]\x1b[0m\x1b[1m: mismatched types\x1b[0m".into(),
            52..=54 => {
                let count = 60 + self.below(200);
                let flags = self.words(count);
                format!("\x1b[36;1mrustc --crate-name app {flags}\x1b[0m")
            }
            55..=57 => {
                let step = 2 + self.below(5);
                (0..64)
                    .map(|i| {
                        let (r, g) = (i * 4, 255 - i * 4);
                        format!("\x1b[38;2;{r};{g};128m\u{2588}")
                    })
                    .step_by(step)
                    .collect::<String>()
                    + "\x1b[0m"
            }
            58..=62 => "Downloading 10%\rDownloading 55%\rDownloading 100%".to_owned(),
            63..=66 => {
                let (word, n) = (self.word(), self.below(9999));
                format!("\x1b[90m{word}\x1b[0m: \x1b[4m{n}\x1b[24m bytes \x1b[2mcached\x1b[0m")
            }
            67..=69 => {
                let n = 100 + self.below(1800);
                format!("{{\"data\": \"{}\"}}", "x".repeat(n))
            }
            _ => {
                let count = 3 + self.below(14);
                format!("  {}", self.words(count))
            }
        }
    }

    /// Complete lines until `viewable` of them will show in the viewer; the
    /// `##[endgroup]` lines it also writes are not counted.
    pub fn lines(&mut self, viewable: usize) -> Vec<u8> {
        let mut out = String::new();
        let mut shown = 0;
        while shown < viewable {
            let stamp = self.timestamp();
            if !self.in_group {
                if self.below(4) == 0 {
                    let body = self.body();
                    out += &format!("{stamp} {body}\n");
                    shown += body.matches('\n').count();
                } else {
                    self.group += 1;
                    self.in_group = true;
                    self.group_left = 5 + self.below(55);
                    let word = self.word();
                    let title = format!("Run step {} ({word})", self.group);
                    out += &format!("{stamp} ##[group]{title}\n");
                }
                shown += 1;
                continue;
            }
            if self.group_left == 0 {
                self.in_group = false;
                out += &format!("{stamp} ##[endgroup]\n");
                continue;
            }
            self.group_left -= 1;
            let body = self.body();
            for (n, part) in body.split('\n').enumerate() {
                let stamp = if n == 0 {
                    stamp.clone()
                } else {
                    self.timestamp()
                };
                out += &format!("{stamp} {part}\n");
                shown += 1;
            }
        }
        if self.below(50) == 0 {
            let stamp = self.timestamp();
            out += &format!("{stamp} ##[error]Process completed with exit code 1.\n");
        }
        out.into_bytes()
    }
}

/// Splits `bytes` into chunks of `size`, ignoring line and escape
/// boundaries as a network would.
#[allow(dead_code, reason = "only the bench feeds chunks")]
pub fn chunks(bytes: &[u8], size: usize) -> impl Iterator<Item = Vec<u8>> + '_ {
    bytes.chunks(size).map(<[u8]>::to_vec)
}

/// The first built-in theme's terminal palette and buffer font, with the
/// bundled fonts registered.
pub fn style(cx: &App) -> LogStyle {
    theme::fonts::register(cx);
    let registry = ThemeRegistry::builtin();
    LogStyle::from_theme(&registry.themes()[0])
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
