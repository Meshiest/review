//! The app's drawing helpers. Views call these and read colors, radii and
//! fonts from the active theme; they never name a color, a corner radius or a
//! font family themselves.

pub mod avatar;
mod chrome;
mod controls;
pub mod fold;
pub mod label;
pub mod link;
pub mod list;
mod menu_button;
pub mod spinner;

use std::sync::Arc;

use gpui::{AnyElement, App, Div, Hsla, IntoElement as _, ParentElement as _, Styled, div, px};
use gpui_kit::assets::IconName;
use gpui_kit::component::Icon;
use scroll::{AutoscrollStyle, ScrollStyle, ScrollbarStyle};
use theme::{ActiveTheme, Theme};

pub use avatar::{Who, avatar};
pub use chrome::{
    Button, ButtonKind, CONTROL_HEIGHT, CONTROL_ICON, bar, card, checkbox, chip, divider,
    empty_note, empty_state, panel, secondary_style, status_bar, title_bar, vdivider, wordmark,
};
pub use controls::{TOGGLE_HEIGHT, icon_button, search_input, stepper, toggle_group};
pub use label::label_chip;
pub use link::web_link;
pub use menu_button::{MenuTrigger, SplitButton};

use crate::model::{CiState, FileStatus, ReviewState};
use crate::ui::spinner::SpinnerSlots;

/// Spacing steps, in pixels, for views to take padding and gaps from.
pub mod space {
    /// Between a pane's edge and content that would otherwise run to it:
    /// the terminal grid.
    pub const PANE_INSET: f32 = 8.0;
}

/// The active theme. Cheap to hold across a render.
pub fn theme(cx: &App) -> Arc<Theme> {
    cx.global::<ActiveTheme>().0.clone()
}

/// Fonts and text sizes from the theme, as methods on any styled element.
pub trait TextExt: Styled + Sized {
    /// Code, branch names, SHAs and `repo#number` references.
    fn mono(self, t: &Theme) -> Self {
        self.font_family(t.fonts.buffer.family())
    }

    /// Titles and headings.
    fn display(self, t: &Theme) -> Self {
        self.font_family(t.display_family())
    }

    /// One step below the interface size: secondary labels.
    fn small(self, t: &Theme) -> Self {
        self.text_size(t.fonts.interface.size_px() - px(1.0))
    }

    /// Two steps below the interface size: counts and mono references.
    fn tiny(self, t: &Theme) -> Self {
        self.text_size(t.fonts.interface.size_px() - px(2.0))
    }
}

impl<E: Styled> TextExt for E {}

/// Scrollbar and autoscroll marker colors for every scrolling view.
pub fn scroll_style(t: &Theme) -> ScrollStyle {
    let c = &t.colors;
    ScrollStyle {
        scrollbar: ScrollbarStyle {
            track: Hsla { a: 0.0, ..c.hover },
            thumb: c.border_strong,
            thumb_hover: c.text_faint,
            thumb_active: c.text_muted,
            ..ScrollbarStyle::default()
        },
        autoscroll: AutoscrollStyle {
            fill: c.surface_raised,
            border: c.border_strong,
            arrow: c.text,
            ..AutoscrollStyle::default()
        },
    }
}

/// The base text of a window or a pane: the interface font at its size.
pub fn interface_text(el: Div, t: &Theme) -> Div {
    el.font_family(t.fonts.interface.family())
        .text_size(t.fonts.interface.size_px())
        .text_color(t.colors.text)
}

/// The icon and color for a CI state; `None` draws nothing.
pub fn ci_style(t: &Theme, state: CiState) -> Option<(IconName, Hsla)> {
    let c = &t.colors;
    match state {
        CiState::Pass => Some((IconName::CircleCheck, c.ok)),
        CiState::Fail => Some((IconName::CircleX, c.danger)),
        CiState::Running => Some((IconName::LoaderCircle, c.info)),
        CiState::Queued => Some((IconName::CircleDashed, c.warn)),
        CiState::Skipped => Some((IconName::CircleMinus, c.text_muted)),
        CiState::Canceled => Some((IconName::Ban, c.text_muted)),
        CiState::None => None,
    }
}

pub fn ci_color(t: &Theme, state: CiState) -> Hsla {
    ci_style(t, state).map_or(t.colors.text_muted, |(_, color)| color)
}

pub fn ci_label(state: CiState) -> &'static str {
    match state {
        CiState::Pass => "passing",
        CiState::Fail => "failing",
        CiState::Running => "running",
        CiState::Queued => "queued",
        CiState::Skipped => "skipped",
        CiState::Canceled => "canceled",
        CiState::None => "no checks",
    }
}

/// Icon size in list rows.
pub const CI_ROW: f32 = 14.0;
/// Icon size in panel headers.
pub const CI_HEADER: f32 = 16.0;

/// A CI state icon. A state without an icon still takes its space so titles
/// stay aligned. The running icon, the only thing in the app that moves, is
/// a slot in `spinners` that the shell's spinner layer turns, so the view
/// drawing it is not rendered again every frame.
pub fn ci_icon(t: &Theme, state: CiState, size: f32, spinners: &SpinnerSlots) -> AnyElement {
    let slot = div().size(px(size)).flex_none();
    let Some((name, color)) = ci_style(t, state) else {
        return slot.into_any_element();
    };
    let icon = if state == CiState::Running {
        spinners.slot(size, color).into_any_element()
    } else {
        Icon::new(name)
            .size(px(size))
            .text_color(color)
            .into_any_element()
    };
    slot.child(icon).into_any_element()
}

pub fn review_color(t: &Theme, state: ReviewState) -> Hsla {
    match state {
        ReviewState::Approved => t.colors.ok,
        ReviewState::ChangesRequested => t.colors.danger,
        ReviewState::Pending | ReviewState::Commented | ReviewState::Draft => t.colors.text_muted,
    }
}

pub fn status_color(t: &Theme, status: FileStatus) -> Hsla {
    match status {
        FileStatus::Added => t.colors.ok,
        FileStatus::Deleted => t.colors.danger,
        FileStatus::Modified => t.colors.info,
        FileStatus::Renamed => t.colors.warn,
    }
}

pub fn status_letter(status: FileStatus) -> &'static str {
    match status {
        FileStatus::Added => "A",
        FileStatus::Deleted => "D",
        FileStatus::Modified => "M",
        FileStatus::Renamed => "R",
    }
}
