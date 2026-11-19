//! The comment composer: a multi-line markdown source box with a Write /
//! Preview toggle, a "Suggest change" helper, and a split button whose main
//! action adds the comment to the pending review and whose menu comments at
//! once.
//!
//! A composer only edits text and reports what the user asked for
//! ([`ComposerEvent`]); [`crate::writing::Writing`] owns the drafts and
//! decides what happens next. The text's line height is a whole number of
//! pixels (the theme's), so focusing the box never shifts its text by a
//! sub-pixel.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    InteractiveElement as _, IntoElement, ParentElement, Render, SharedString, Styled, Window,
    actions, canvas, div, prelude::FluentBuilder as _, px,
};
use gpui_kit::component::button::{Button as FlatButton, DropdownButton};
use gpui_kit::component::input::{Escape, InputEvent, Textarea, TextareaState};
use gpui_kit::component::menu::PopupMenuItem;
use gpui_kit::component::text::TextViewState;
use gpui_kit::component::{Disableable as _, Sizable as _, Size, h_flex, v_flex};
use store::DraftAnchor;

use crate::ui::{self, Button, ButtonKind, TextExt as _};
use crate::untrusted;

actions!(
    composer,
    [
        /// Ctrl+Enter: the composer's main action.
        SubmitPrimary,
        /// Ctrl+Shift+Enter: send as a single comment.
        SubmitNow
    ]
);

/// The key context of every composer; its bindings live in [`crate::bind_keys`].
pub const KEY_CONTEXT: &str = "Composer";

/// What the user asked for when they submitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmitAction {
    /// Add to the pending review.
    Queue,
    /// Post on its own.
    SendNow,
}

pub enum ComposerEvent {
    /// The text changed.
    Edited(String),
    Submit(SubmitAction),
    /// Esc: hide the composer, keep the draft.
    Close,
    /// Throw the draft away.
    Discard,
}

/// A button at the start of the action row, for what the box's place adds:
/// the Conversation's "Close pull request".
#[derive(Clone)]
pub struct Leading {
    pub id: &'static str,
    pub label: SharedString,
    pub on_click: crate::palette::Run,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Write,
    Preview,
}

/// What the composer's buttons say, by what it is writing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A new comment on code: Add to review, or Comment now.
    Code,
    Reply,
    /// New text for a queued comment.
    Edit,
    Conversation,
}

impl Kind {
    fn of(anchor: &DraftAnchor) -> Self {
        match anchor {
            DraftAnchor::Reply { .. } => Self::Reply,
            DraftAnchor::Edit { .. } => Self::Edit,
            DraftAnchor::Conversation | DraftAnchor::Review => Self::Conversation,
            DraftAnchor::Line { .. } | DraftAnchor::File { .. } => Self::Code,
        }
    }

    /// What the one submit button does, when there is no queue to add to.
    fn solo_label(self) -> &'static str {
        match self {
            Self::Reply => "Reply",
            Self::Edit => "Save",
            Self::Code | Self::Conversation => "Comment",
        }
    }
}

pub struct Composer {
    anchor: DraftAnchor,
    kind: Kind,
    state: Entity<TextareaState>,
    /// Focus of the whole composer, so Ctrl+Enter works in Preview too.
    focus: FocusHandle,
    mode: Mode,
    preview: Option<(SharedString, Entity<TextViewState>)>,
    /// The selected lines "Suggest change" starts from, for new-side lines.
    suggestion: Option<String>,
    /// A label for what it is attached to: `src/a.rs  line 31`.
    place: Option<SharedString>,
    busy: bool,
    error: Option<SharedString>,
    leading: Option<Leading>,
    focus_pending: bool,
    /// The text length the last paint saw, for the key-to-paint benchmark.
    painted: Rc<Cell<usize>>,
}

impl EventEmitter<ComposerEvent> for Composer {}

impl Focusable for Composer {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

/// What a composer starts from.
pub struct ComposerInit {
    pub anchor: DraftAnchor,
    pub text: String,
    pub suggestion: Option<String>,
    pub place: Option<SharedString>,
    pub focus: bool,
    /// Start on the Preview tab (text someone else wrote, such as Claude).
    pub preview: bool,
}

impl Composer {
    pub fn new(init: ComposerInit, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let kind = Kind::of(&init.anchor);
        let rows = match kind {
            Kind::Code => (3, 12),
            Kind::Reply | Kind::Edit => (2, 10),
            Kind::Conversation => (4, 12),
        };
        let state = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(rows.0, rows.1)
                .placeholder("Leave a comment (markdown)")
        });
        let preview = (init.preview && !init.text.is_empty()).then(|| {
            let source = untrusted::markdown_source(&init.text);
            let view = cx.new(|cx| TextViewState::markdown(&source, cx));
            (SharedString::from(init.text.clone()), view)
        });
        if !init.text.is_empty() {
            state.update(cx, |state, cx| state.set_value(init.text, window, cx));
        }
        cx.subscribe_in(
            &state,
            window,
            |this, state, event: &InputEvent, window, cx| match event {
                InputEvent::Change => {
                    let text = state.read(cx).value().to_string();
                    cx.emit(ComposerEvent::Edited(text));
                    cx.notify();
                }
                InputEvent::PressEnter {
                    secondary: true, ..
                } => {
                    this.submit(this.primary_action(), window, cx);
                }
                _ => {}
            },
        )
        .detach();
        Self {
            anchor: init.anchor,
            kind,
            state,
            focus: cx.focus_handle(),
            mode: if preview.is_some() {
                Mode::Preview
            } else {
                Mode::Write
            },
            preview,
            suggestion: init.suggestion,
            place: init.place,
            busy: false,
            error: None,
            leading: None,
            focus_pending: init.focus,
            painted: Rc::new(Cell::new(0)),
        }
    }

    pub fn anchor(&self) -> &DraftAnchor {
        &self.anchor
    }

    /// The text box's own focus handle.
    pub fn input_focus(&self, cx: &gpui::App) -> FocusHandle {
        self.state.read(cx).focus_handle(cx).clone()
    }

    pub fn text(&self, cx: &gpui::App) -> String {
        self.state.read(cx).value().to_string()
    }

    /// How many bytes of text the last paint drew.
    pub fn painted_len(&self) -> usize {
        self.painted.get()
    }

    pub fn focus_input(&self, window: &mut Window, cx: &mut Context<Self>) {
        let state = self.state.clone();
        state.update(cx, |state, cx| state.focus(window, cx));
    }

    /// Takes focus into the text box on the next frame.
    pub fn request_focus(&mut self, cx: &mut Context<Self>) {
        self.focus_pending = true;
        self.mode = Mode::Write;
        cx.notify();
    }

    /// Locks the box while a write is in flight; a failure keeps the text
    /// and says why.
    pub fn set_status(&mut self, busy: bool, error: Option<SharedString>, cx: &mut Context<Self>) {
        self.busy = busy;
        self.error = error;
        cx.notify();
    }

    /// Puts `leading` at the start of the action row, or takes it away.
    pub fn set_leading(&mut self, leading: Option<Leading>, cx: &mut Context<Self>) {
        let shown = |leading: &Option<Leading>| leading.as_ref().map(|l| (l.id, l.label.clone()));
        let changed = shown(&self.leading) != shown(&leading);
        self.leading = leading;
        if changed {
            cx.notify();
        }
    }

    fn primary_action(&self) -> SubmitAction {
        match self.kind {
            Kind::Code => SubmitAction::Queue,
            Kind::Reply | Kind::Edit | Kind::Conversation => SubmitAction::SendNow,
        }
    }

    fn submit(&mut self, action: SubmitAction, _: &mut Window, cx: &mut Context<Self>) {
        if self.busy || self.text(cx).trim().is_empty() {
            return;
        }
        cx.emit(ComposerEvent::Submit(action));
    }

    /// Switches to the Preview tab.
    pub fn show_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.set_mode(Mode::Preview, window, cx);
    }

    fn set_mode(&mut self, mode: Mode, window: &mut Window, cx: &mut Context<Self>) {
        if self.mode == mode {
            return;
        }
        self.mode = mode;
        match mode {
            Mode::Preview => {
                let text = self.text(cx);
                let fresh = self
                    .preview
                    .as_ref()
                    .is_none_or(|(parsed, _)| parsed.as_ref() != text);
                if fresh {
                    let source = untrusted::markdown_source(&text);
                    let state = cx.new(|cx| TextViewState::markdown(&source, cx));
                    self.preview = Some((SharedString::from(text), state));
                }
                window.focus(&self.focus, cx);
            }
            Mode::Write => self.focus_input(window, cx),
        }
        cx.notify();
    }

    /// Inserts a suggestion block with the selected lines at the cursor.
    fn suggest(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(lines) = self.suggestion.clone() else {
            return;
        };
        if self.mode == Mode::Preview {
            self.set_mode(Mode::Write, window, cx);
        }
        let current = self.text(cx);
        let lead = if current.is_empty() || current.ends_with('\n') {
            ""
        } else {
            "\n"
        };
        let block = format!("{lead}```suggestion\n{lines}\n```\n");
        self.state
            .update(cx, |state, cx| state.insert(block, window, cx));
        let text = self.text(cx);
        cx.emit(ComposerEvent::Edited(text));
        self.focus_input(window, cx);
        cx.notify();
    }

    fn header(&mut self, t: &theme::Theme, cx: &mut Context<Self>) -> gpui::Div {
        let c = &t.colors;
        let this = cx.entity();
        let modes = ui::segmented(
            cx,
            "composer-mode",
            vec![
                (Mode::Write, SharedString::new_static("Write")),
                (Mode::Preview, SharedString::new_static("Preview")),
            ],
            {
                let mode = self.mode;
                move |m| *m == mode
            },
            move |mode, window, cx| {
                let mode = *mode;
                this.update(cx, |this, cx| this.set_mode(mode, window, cx));
            },
        );
        h_flex()
            .gap(px(8.0))
            .items_center()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .small(t)
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(c.text_secondary)
                    .children(self.place.clone()),
            )
            .child(modes)
    }

    fn body(&mut self, window: &mut Window, t: &theme::Theme, cx: &mut Context<Self>) -> gpui::Div {
        let c = &t.colors;
        let line_height = t.fonts.buffer.line_height_px();
        match (self.mode, &self.preview) {
            (Mode::Preview, Some((text, state))) => {
                let empty = text.trim().is_empty();
                let body = if empty {
                    div()
                        .text_color(c.text_muted)
                        .child("Nothing to preview")
                        .into_any_element()
                } else {
                    untrusted::text_view(state)
                        .text_color(c.text)
                        .text_size(t.fonts.interface.size_px())
                        .into_any_element()
                };
                div()
                    .min_h(line_height * 3.0)
                    .p(px(8.0))
                    .rounded(t.radius.md)
                    .border_1()
                    .border_color(c.border)
                    .bg(c.surface)
                    .child(body)
            }
            _ => {
                let focused = self.state.read(cx).focus_handle(cx).is_focused(window);
                div()
                    .debug_selector(|| "composer-input".into())
                    .rounded(t.radius.md)
                    .border_1()
                    .border_color(if focused { c.focus } else { c.control_border })
                    .bg(c.field)
                    .overflow_hidden()
                    .child(
                        Textarea::new(&self.state)
                            .appearance(false)
                            .bordered(false)
                            .readonly(self.busy)
                            .mono(t)
                            .text_size(t.fonts.buffer.size_px())
                            .line_height(line_height),
                    )
            }
        }
    }

    fn actions(&mut self, t: &theme::Theme, cx: &mut Context<Self>) -> gpui::AnyElement {
        let c = &t.colors;
        let this = cx.entity();
        let mut row = h_flex().gap(px(8.0)).items_center();
        if let Some(leading) = self.leading.clone() {
            row = row.child(
                Button::new(leading.id, leading.label, ButtonKind::Secondary)
                    .on_click(move |_, window, cx| (leading.on_click)(window, cx)),
            );
        }
        if self.kind == Kind::Code && self.suggestion.is_some() {
            row = row.child(
                Button::new("composer-suggest", "Suggest change", ButtonKind::Ghost).on_click({
                    let this = this.clone();
                    move |_, window, cx| this.update(cx, |this, cx| this.suggest(window, cx))
                }),
            );
        }
        let hint = match (&self.error, self.kind) {
            (Some(error), _) => div().text_color(c.danger).child(error.clone()),
            (None, Kind::Code) => div()
                .text_color(c.text_muted)
                .child("Ctrl+Enter adds to the review. Esc keeps the draft."),
            (None, _) => div()
                .text_color(c.text_muted)
                .child("Ctrl+Enter sends. Esc keeps the draft."),
        };
        row = row
            .child(div().flex_1().min_w_0().truncate().small(t).child(hint))
            .child(
                Button::new("composer-discard", "Discard", ButtonKind::Ghost).on_click({
                    let this = this.clone();
                    move |_, _, cx| this.update(cx, |_, cx| cx.emit(ComposerEvent::Discard))
                }),
            );
        let disabled = self.busy;
        if self.kind == Kind::Code {
            let click = this.clone();
            let menu = this.clone();
            row.child(
                DropdownButton::new("composer-send")
                    .button(
                        FlatButton::new("composer-queue")
                            .label(if disabled { "Adding" } else { "Add to review" })
                            .with_size(Size::Small)
                            .disabled(disabled)
                            .when(!disabled, |button| button.cursor_pointer())
                            .on_click(move |_, window, cx| {
                                click.update(cx, |this, cx| {
                                    this.submit(SubmitAction::Queue, window, cx);
                                });
                            }),
                    )
                    .dropdown_menu(move |popup, _, _| {
                        let menu = menu.clone();
                        popup.item(PopupMenuItem::new("Comment now").on_click(
                            move |_, window, cx| {
                                menu.update(cx, |this, cx| {
                                    this.submit(SubmitAction::SendNow, window, cx);
                                });
                            },
                        ))
                    }),
            )
            .into_any_element()
        } else {
            row.child(
                FlatButton::new("composer-send-now")
                    .label(self.kind.solo_label())
                    .with_size(Size::Small)
                    .disabled(disabled)
                    .when(!disabled, |button| button.cursor_pointer())
                    .on_click(move |_, window, cx| {
                        this.update(cx, |this, cx| {
                            this.submit(SubmitAction::SendNow, window, cx);
                        });
                    }),
            )
            .into_any_element()
        }
    }
}

impl Render for Composer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = ui::theme(cx);
        let c = &t.colors;
        if std::mem::take(&mut self.focus_pending) {
            let state = self.state.clone();
            window.defer(cx, move |window, cx| {
                state.update(cx, |state, cx| state.focus(window, cx));
            });
        }
        let painted = self.painted.clone();
        let state = self.state.clone();
        let header = self.header(&t, cx);
        let body = self.body(window, &t, cx);
        let actions = self.actions(&t, cx);
        let framed = matches!(self.kind, Kind::Code);
        v_flex()
            .id(("composer", cx.entity_id()))
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &Escape, _, cx| {
                if this.kind != Kind::Conversation {
                    cx.emit(ComposerEvent::Close);
                }
            }))
            .on_action(cx.listener(|this, _: &SubmitPrimary, window, cx| {
                this.submit(this.primary_action(), window, cx);
            }))
            .on_action(cx.listener(|this, _: &SubmitNow, window, cx| {
                this.submit(SubmitAction::SendNow, window, cx);
            }))
            .gap(px(8.0))
            .p(px(10.0))
            .text_color(c.text)
            .when(framed, |el| {
                el.rounded(t.radius.lg)
                    .border_1()
                    .border_color(c.accent_fg)
                    .bg(c.surface_raised)
            })
            .child(header)
            .child(body)
            .child(actions)
            .child(canvas(
                |_, _, _| (),
                move |_, (), _, cx| painted.set(state.read(cx).text().len()),
            ))
    }
}
