//! A diff in a window to scroll by hand: real source files unless told
//! otherwise.
//!
//! cargo run -p diff_view --release --example demo -- [--split] [--rows N] [--real] [--synthetic] [--source DIR]
//!     [--theme FILE] [--font FAMILY] [--font-size PX] [--demo-select]
//!
//! Wheel, scrollbar (drag the thumb, click the track) and middle-click
//! autoscroll all work, and so does selecting text with the mouse
//! (Ctrl+C copies, Escape clears). `--demo-select` makes a selection through
//! the view's own press and drag methods, without any input from outside.

mod common;

use diff::{Row, Side};
use diff_view::DiffView;
use gpui::{App, AppContext, Context, point, px};

/// Selects from the middle of one code line to the middle of another a few
/// rows below it, the way a drag would. Split view takes the new side.
fn select_demo(view: &mut DiffView) {
    let (Some(left), Some(ch)) = (view.text_left(Side::New), view.char_width()) else {
        return;
    };
    let list = view.list();
    let on_new_side = |row: usize| match list.rows()[row] {
        Row::Line { .. } => true,
        Row::Pair { right, .. } => right.is_some(),
        _ => false,
    };
    let rows: Vec<usize> = view
        .rows_in_view()
        .filter(|&row| on_new_side(row))
        .collect();
    let (Some(&first), Some(&last)) = (rows.get(3), rows.get(9)) else {
        return;
    };
    let middle = |row: usize, columns: f32| {
        let top = list.top_of(row) - view.scroll_y() + list.row_height(row) / 2.0;
        point(left + ch * columns, px(top as f32))
    };
    let (from, to) = (middle(first, 6.0), middle(last, 22.0));
    view.press_text(from, 1, false);
    view.drag_text(to);
    view.release_text();
    eprintln!("demo-select: copy gives {:?}", view.copy_selection());
}

/// Marks the file at the top viewed and scrolls to its tail, so its header is
/// pinned (checked, dimmed) over its code with the next file's header below.
fn sticky_demo(view: &mut DiffView, cx: &mut Context<DiffView>) {
    let Some(&next) = view.list().file_rows().get(1) else {
        return;
    };
    let next_top = view.list().top_of(next as usize);
    view.set_scroll_y((next_top - 140.0).max(0.0));
    if let Some(file) = view.current_file() {
        view.set_viewed(file, true, cx);
    }
    eprintln!(
        "demo-sticky: current file {:?}, scroll {}",
        view.current_file(),
        view.scroll_y()
    );
}

fn main() {
    let args = common::Args::parse();
    gpui_platform::application().run(move |cx: &mut App| {
        let styles = common::styles(&args, cx);
        let view = common::view(&args, styles.first);
        let title = format!("diff demo ({})", args.content());
        let options = common::window_options(cx, &title);
        let demo_select = args.demo_select;
        let demo_sticky = args.demo_sticky;
        let split_ratio = args.split_ratio;
        cx.open_window(options, |window, cx| {
            let view = cx.new(|cx| {
                let mut view = view;
                if let Some(ratio) = split_ratio {
                    view.set_split_ratio(ratio, cx);
                }
                view
            });
            if demo_sticky {
                let view = view.clone();
                window.on_next_frame(move |_, cx| {
                    view.update(cx, |view: &mut DiffView, cx: &mut Context<DiffView>| {
                        sticky_demo(view, cx);
                        cx.notify();
                    });
                });
            }
            if demo_select {
                // Columns are measured by the first frame.
                let view = view.clone();
                window.on_next_frame(move |_, cx| {
                    view.update(cx, |view: &mut DiffView, cx: &mut Context<DiffView>| {
                        select_demo(view);
                        cx.notify();
                    });
                });
            }
            view
        })
        .expect("open window");
        cx.activate(true);
    });
}
