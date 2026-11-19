//! Links in headers and rows: text that follows a web URL (or opens a pull
//! request in the app, see [`crate::references::open_link`]) and shows the
//! URL it really goes to on hover.

use gpui::{
    AnyView, App, AppContext as _, Context, Div, ElementId, InteractiveElement as _, IntoElement,
    MouseButton, ParentElement as _, Render, SharedString, Stateful,
    StatefulInteractiveElement as _, Styled as _, Window, div, px,
};

use super::{TextExt as _, theme};
use crate::references;
use crate::untrusted::is_web_url;

/// The hover card of a link: where it goes, in full.
pub struct LinkTip {
    url: SharedString,
}

impl Render for LinkTip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        let c = &t.colors;
        let selector = format!("link-tooltip:{}", self.url);
        div()
            .debug_selector(move || selector)
            .max_w(px(520.0))
            .px(px(8.0))
            .py(px(4.0))
            .rounded(t.radius.md)
            .border_1()
            .border_color(c.border_strong)
            .bg(c.surface_menu)
            .text_color(c.text)
            .mono(&t)
            .tiny(&t)
            .child(self.url.clone())
    }
}

/// The tooltip view for a link to `url`.
pub fn tip(url: SharedString, cx: &mut App) -> AnyView {
    cx.new(|_| LinkTip { url }).into()
}

/// `label` as a link to `url`; nothing happens on a URL that is not
/// http(s). The row under it does not take the click.
pub fn web_link(
    id: impl Into<ElementId>,
    label: impl IntoElement,
    url: String,
    cx: &App,
) -> Stateful<Div> {
    let t = theme(cx);
    let link = t.colors.link;
    let shown: SharedString = url.clone().into();
    let selector = format!("web-link:{url}");
    div()
        .id(id)
        .debug_selector(move || selector)
        .min_w_0()
        .cursor_pointer()
        .hover(move |s| s.underline().text_color(link))
        .tooltip(move |_, cx| tip(shown.clone(), cx))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(move |_, _, cx| {
            cx.stop_propagation();
            if is_web_url(&url) {
                references::open_link(&url, cx);
            }
        })
        .child(label)
}
