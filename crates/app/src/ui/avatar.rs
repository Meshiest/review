//! Account avatars: the picture clipped to a circle, or the letter circle
//! while it loads, when it fails, and for accounts without one.

use gpui::{
    AnyElement, App, FontWeight, IntoElement as _, ObjectFit, ParentElement as _, Styled as _,
    StyledImage as _, div, img, px,
};
use theme::Theme;

use super::theme;
use crate::avatars::Avatars;

/// Avatar size in list rows: the inbox, the stack, thread lists, reviewers
/// and the status bar.
pub const ROW: f32 = 16.0;
/// Avatar size on thread cards and in the conversation timeline.
pub const CARD: f32 = 20.0;

/// An account as the avatar needs it.
#[derive(Clone, Copy, Debug)]
pub struct Who<'a> {
    pub login: &'a str,
    pub avatar_url: Option<&'a str>,
}

impl<'a> Who<'a> {
    pub fn new(login: &'a str, avatar_url: Option<&'a str>) -> Self {
        Self { login, avatar_url }
    }
}

impl<'a> From<&'a forge::model::User> for Who<'a> {
    fn from(user: &'a forge::model::User) -> Self {
        Self::new(&user.login, user.avatar_url.as_deref())
    }
}

/// The account's avatar, `size` px square, for an account on the forge at
/// `host`. Drawn from memory; a miss queues the fetch and draws the letter
/// circle now (see [`Avatars::image`]).
pub fn avatar<'a>(cx: &App, host: &str, who: impl Into<Who<'a>>, size: f32) -> AnyElement {
    let who = who.into();
    let image = who
        .avatar_url
        .filter(|url| !url.is_empty())
        .and_then(|url| cx.try_global::<Avatars>()?.image(host, url));
    match image {
        Some(image) => img(image)
            .size(px(size))
            .flex_none()
            .rounded_full()
            .object_fit(ObjectFit::Cover)
            .into_any_element(),
        None => letter(&theme(cx), who.login, size).into_any_element(),
    }
}

/// A circle with the account's initial, its color picked by the login from
/// the theme's state colors.
pub fn letter(t: &Theme, login: &str, size: f32) -> gpui::Div {
    let c = &t.colors;
    let hues = [c.info, c.ok, c.warn, c.accent_fg, c.accent_2, c.danger];
    let hash = login
        .bytes()
        .fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(u32::from(b)));
    let colors = t.chip_colors(hues[hash as usize % hues.len()]);
    let initial = login
        .chars()
        .next()
        .map(|ch| ch.to_uppercase().to_string())
        .unwrap_or_default();
    div()
        .size(px(size))
        .flex_none()
        .rounded_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(colors.fill)
        .text_color(colors.text)
        .text_size(px((size * 0.55).round()))
        .line_height(px(size))
        .font_weight(FontWeight::SEMIBOLD)
        .child(initial)
}
