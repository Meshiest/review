//! Keyboard: the search field, navigation and copy.

use gpui::{Context, KeyDownEvent, Window};

use super::LogView;

impl LogView {
    pub(super) fn key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let handled =
            (self.search.open && self.search_key(event, cx)) || self.view_key(event, window, cx);
        if handled {
            cx.stop_propagation();
        }
    }

    /// Keys that edit or drive the open search bar. False for the rest, which
    /// still scroll the view.
    fn search_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        let keystroke = &event.keystroke;
        let modifiers = keystroke.modifiers;
        let primary = modifiers.secondary(cx);
        match keystroke.key.as_str() {
            "escape" => self.close_search(cx),
            "enter" => self.step_match(!modifiers.shift, cx),
            "backspace" => {
                if primary {
                    self.search.input.clear();
                } else {
                    self.search.input.pop();
                }
                self.query_changed(cx);
            }
            "v" if primary => {
                if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    let first = text.lines().next().unwrap_or_default();
                    self.search.input.push_str(first);
                    self.query_changed(cx);
                }
            }
            "f" if primary => {}
            _ => {
                let typed = keystroke
                    .key_char
                    .as_deref()
                    .filter(|text| !text.is_empty() && text.chars().all(|c| !c.is_control()))
                    .filter(|_| {
                        event.prefer_character_input || !(modifiers.control || modifiers.platform)
                    });
                let Some(text) = typed else { return false };
                self.search.input.push_str(text);
                self.query_changed(cx);
            }
        }
        true
    }

    fn query_changed(&mut self, cx: &mut Context<Self>) {
        self.restart_search();
        cx.notify();
    }

    fn view_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let keystroke = &event.keystroke;
        let modifiers = keystroke.modifiers;
        let primary = modifiers.secondary(cx);
        let page = self.scroll.state.y.viewport - self.line_height();
        match keystroke.key.as_str() {
            "f" if primary => self.open_search(window, cx),
            "c" if primary => self.copy_selection(cx),
            "a" if primary => self.select_all(cx),
            "t" if modifiers.alt => self.set_show_time(!self.show_time, cx),
            "f3" => self.step_match(!modifiers.shift, cx),
            "escape" if self.selection.is_some() => self.clear_selection(cx),
            "end" => self.scroll_to_latest(cx),
            "home" => {
                self.set_scroll_y(0.0);
                cx.notify();
            }
            "pageup" => {
                self.scroll_by(-page);
                cx.notify();
            }
            "pagedown" => {
                self.scroll_by(page);
                cx.notify();
            }
            "up" => {
                self.scroll_by(-self.line_height());
                cx.notify();
            }
            "down" => {
                self.scroll_by(self.line_height());
                cx.notify();
            }
            _ => return false,
        }
        true
    }
}
