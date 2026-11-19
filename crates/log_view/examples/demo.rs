//! A CI log streaming into the viewer, for screenshots and hand testing.
//!
//! cargo run -p log_view --release --example demo -- [--lines-per-second N] [--initial N] [--time]
//!     [--fold-every N] [--search TEXT]
//!
//! Finished groups are folded like a CI page does: every N-th one (default
//! 2, 0 for none). `--search` starts with the search bar open.
//!
//! The view follows the end while lines arrive. Scroll up to leave follow
//! mode, then use the pill to return. Ctrl+F searches, Enter and Shift+Enter
//! step through matches, a click on a group header folds it, a drag selects
//! lines, Ctrl+C copies them, Alt+T toggles the time column, and the wheel,
//! scrollbar and middle-click autoscroll scroll.

mod common;

use std::time::Duration;

use gpui::{App, AppContext, AsyncApp, Entity, Focusable};
use log_view::{LogFeed, LogView};

const TICK: Duration = Duration::from_millis(50);

struct Args {
    lines_per_second: usize,
    initial: usize,
    time: bool,
    /// Fold every this many finished groups; 0 folds none.
    fold_every: usize,
    search: Option<String>,
}

impl Args {
    fn parse() -> Self {
        let mut args = Self {
            lines_per_second: 120,
            initial: 400,
            time: false,
            fold_every: 2,
            search: None,
        };
        let mut it = std::env::args().skip(1);
        while let Some(arg) = it.next() {
            let mut number = |name: &str| -> usize {
                it.next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or_else(|| panic!("{name} takes a number"))
            };
            match arg.as_str() {
                "--lines-per-second" => args.lines_per_second = number("--lines-per-second"),
                "--initial" => args.initial = number("--initial"),
                "--fold-every" => args.fold_every = number("--fold-every"),
                "--search" => args.search = it.next(),
                "--time" => args.time = true,
                other => panic!(
                    "unknown argument {other}; expected --lines-per-second N, --initial N, \
                     --fold-every N, --search TEXT, --time"
                ),
            }
        }
        args
    }
}

/// Appends generated lines forever and folds each group that finishes, if
/// it is one of the every `fold_every`-th.
async fn stream(
    feed: LogFeed,
    view: Entity<LogView>,
    args: Args,
    mut generator: common::Generator,
    cx: &mut AsyncApp,
) {
    let per_tick = (args.lines_per_second * TICK.as_millis() as usize / 1000).max(1);
    let mut checked = 0;
    loop {
        cx.background_executor().timer(TICK).await;
        feed.push(generator.lines(per_tick));
        if args.fold_every == 0 {
            continue;
        }
        let finished = feed.read(|b| {
            b.groups()[checked..]
                .iter()
                .take_while(|g| g.end.is_some())
                .map(|g| g.start)
                .collect::<Vec<_>>()
        });
        for header in finished {
            checked += 1;
            if checked % args.fold_every == 0 {
                view.update(cx, |view, cx| view.set_group_collapsed(header, true, cx));
            }
        }
    }
}

fn main() {
    let args = Args::parse();
    gpui_platform::application().run(move |cx: &mut App| {
        let style = common::style(cx);
        let mut generator = common::Generator::new(0x5eed_d3a0);
        let feed = LogFeed::new();
        feed.push(generator.lines(args.initial));
        let options = common::window_options(cx, "log view demo");
        let show_time = args.time;
        let search = args.search.clone();
        let mut shown = None;
        cx.open_window(options, |window, cx| {
            let view = cx.new(|cx| {
                let mut view = LogView::new(&feed, style, cx);
                view.set_show_time(show_time, cx);
                view
            });
            window.focus(&view.focus_handle(cx), cx);
            if let Some(text) = &search {
                view.update(cx, |view, cx| view.set_search(text, cx));
            }
            shown = Some(view.clone());
            view
        })
        .expect("open window");
        let view = shown.expect("the window builds its view");
        cx.spawn(async move |cx| stream(feed, view, args, generator, cx).await)
            .detach();
        cx.activate(true);
    });
}
