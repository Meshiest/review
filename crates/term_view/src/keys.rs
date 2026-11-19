//! Keystroke to terminal bytes.

use gpui::Keystroke;
use term::alacritty_terminal::grid::Scroll;
use term::alacritty_terminal::term::TermMode;

const ESC: u8 = 0x1b;

/// What a key does for the terminal itself instead of reaching the program.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Local {
    Copy,
    Paste,
    Scroll(Scroll),
}

/// The terminal's own shortcuts: Ctrl+Shift+C and V, and Shift with PageUp,
/// PageDown, Home or End to scroll the history. A full-screen program on the
/// alternate screen has no history, so it gets the scroll keys.
pub(crate) fn local(keystroke: &Keystroke, mode: TermMode) -> Option<Local> {
    let m = keystroke.modifiers;
    if m.platform || m.alt {
        return None;
    }
    if m.control && m.shift {
        return match keystroke.key.as_str() {
            "c" => Some(Local::Copy),
            "v" => Some(Local::Paste),
            _ => None,
        };
    }
    if !m.shift || m.control || mode.contains(TermMode::ALT_SCREEN) {
        return None;
    }
    match keystroke.key.as_str() {
        "pageup" => Some(Local::Scroll(Scroll::PageUp)),
        "pagedown" => Some(Local::Scroll(Scroll::PageDown)),
        "home" => Some(Local::Scroll(Scroll::Top)),
        "end" => Some(Local::Scroll(Scroll::Bottom)),
        _ => None,
    }
}

/// The bytes a terminal program expects for this key, or `None` when the key
/// produces nothing (a bare modifier, a shortcut for the platform).
///
/// `prefer_character_input` is set when the typed character depends on
/// modifiers (AltGr), so the text is sent as is.
pub(crate) fn encode(
    keystroke: &Keystroke,
    prefer_character_input: bool,
    mode: TermMode,
) -> Option<Vec<u8>> {
    let m = keystroke.modifiers;
    if m.platform {
        return None;
    }
    if prefer_character_input && let Some(text) = &keystroke.key_char {
        return Some(text.clone().into_bytes());
    }

    let key = keystroke.key.as_str();
    let mods = 1 + u8::from(m.shift) + 2 * u8::from(m.alt) + 4 * u8::from(m.control);
    let app_cursor = mode.contains(TermMode::APP_CURSOR);

    let named = match key {
        "enter" => Some(alt_prefix(m.alt, b"\r")),
        "escape" => Some(alt_prefix(m.alt, &[ESC])),
        "tab" if m.shift => Some(b"\x1b[Z".to_vec()),
        "tab" => Some(alt_prefix(m.alt, b"\t")),
        "backspace" => Some(alt_prefix(m.alt, if m.control { b"\x08" } else { b"\x7f" })),
        "space" if m.control => Some(alt_prefix(m.alt, b"\0")),
        "up" => Some(csi_or_ss3(b'A', mods, app_cursor)),
        "down" => Some(csi_or_ss3(b'B', mods, app_cursor)),
        "right" => Some(csi_or_ss3(b'C', mods, app_cursor)),
        "left" => Some(csi_or_ss3(b'D', mods, app_cursor)),
        "home" => Some(csi_or_ss3(b'H', mods, app_cursor)),
        "end" => Some(csi_or_ss3(b'F', mods, app_cursor)),
        "insert" => Some(tilde(2, mods)),
        "delete" => Some(tilde(3, mods)),
        "pageup" => Some(tilde(5, mods)),
        "pagedown" => Some(tilde(6, mods)),
        "f1" => Some(csi_or_ss3(b'P', mods, true)),
        "f2" => Some(csi_or_ss3(b'Q', mods, true)),
        "f3" => Some(csi_or_ss3(b'R', mods, true)),
        "f4" => Some(csi_or_ss3(b'S', mods, true)),
        "f5" => Some(tilde(15, mods)),
        "f6" => Some(tilde(17, mods)),
        "f7" => Some(tilde(18, mods)),
        "f8" => Some(tilde(19, mods)),
        "f9" => Some(tilde(20, mods)),
        "f10" => Some(tilde(21, mods)),
        "f11" => Some(tilde(23, mods)),
        "f12" => Some(tilde(24, mods)),
        _ => None,
    };
    if named.is_some() {
        return named;
    }

    if m.control {
        return control_byte(key).map(|byte| alt_prefix(m.alt, &[byte]));
    }

    let text = keystroke
        .key_char
        .as_deref()
        .or_else(|| (key.chars().count() == 1).then_some(key))?;
    Some(alt_prefix(m.alt, text.as_bytes()))
}

fn alt_prefix(alt: bool, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 1);
    if alt {
        out.push(ESC);
    }
    out.extend_from_slice(bytes);
    out
}

/// `ESC [ <final>`, `ESC O <final>` in application mode, or `ESC [ 1 ; mods <final>`.
fn csi_or_ss3(final_byte: u8, mods: u8, application: bool) -> Vec<u8> {
    if mods > 1 {
        format!("\x1b[1;{mods}{}", final_byte as char).into_bytes()
    } else if application {
        vec![ESC, b'O', final_byte]
    } else {
        vec![ESC, b'[', final_byte]
    }
}

fn tilde(code: u8, mods: u8) -> Vec<u8> {
    if mods > 1 {
        format!("\x1b[{code};{mods}~").into_bytes()
    } else {
        format!("\x1b[{code}~").into_bytes()
    }
}

fn control_byte(key: &str) -> Option<u8> {
    let mut chars = key.chars();
    let (ch, None) = (chars.next()?, chars.next()) else {
        return None;
    };
    match ch {
        'a'..='z' => Some(ch as u8 - b'a' + 1),
        '@' | '2' => Some(0),
        '[' | '3' => Some(0x1b),
        '\\' | '4' => Some(0x1c),
        ']' | '5' => Some(0x1d),
        '^' | '6' => Some(0x1e),
        '_' | '-' | '/' | '7' => Some(0x1f),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use gpui::Modifiers;

    use super::*;

    fn press(key: &str, key_char: Option<&str>, modifiers: Modifiers) -> Keystroke {
        Keystroke {
            modifiers,
            key: key.into(),
            key_char: key_char.map(Into::into),
        }
    }

    fn mods(control: bool, alt: bool, shift: bool) -> Modifiers {
        Modifiers {
            control,
            alt,
            shift,
            ..Modifiers::default()
        }
    }

    fn bytes(keystroke: &Keystroke) -> Vec<u8> {
        encode(keystroke, false, TermMode::empty()).expect("key produces bytes")
    }

    #[test]
    fn printable_text_and_alt_prefix() {
        assert_eq!(bytes(&press("a", Some("a"), Modifiers::default())), b"a");
        assert_eq!(
            bytes(&press("a", Some("A"), mods(false, false, true))),
            b"A"
        );
        assert_eq!(
            bytes(&press("a", Some("a"), mods(false, true, false))),
            b"\x1ba"
        );
        assert_eq!(
            bytes(&press("space", Some(" "), Modifiers::default())),
            b" "
        );
    }

    #[test]
    fn named_keys() {
        let none = Modifiers::default();
        assert_eq!(bytes(&press("enter", None, none)), b"\r");
        assert_eq!(bytes(&press("backspace", None, none)), b"\x7f");
        assert_eq!(bytes(&press("tab", None, none)), b"\t");
        assert_eq!(
            bytes(&press("tab", None, mods(false, false, true))),
            b"\x1b[Z"
        );
        assert_eq!(bytes(&press("escape", None, none)), b"\x1b");
        assert_eq!(bytes(&press("up", None, none)), b"\x1b[A");
        assert_eq!(bytes(&press("home", None, none)), b"\x1b[H");
        assert_eq!(bytes(&press("end", None, none)), b"\x1b[F");
        assert_eq!(bytes(&press("pageup", None, none)), b"\x1b[5~");
        assert_eq!(bytes(&press("pagedown", None, none)), b"\x1b[6~");
        assert_eq!(bytes(&press("delete", None, none)), b"\x1b[3~");
        assert_eq!(bytes(&press("f5", None, none)), b"\x1b[15~");
    }

    #[test]
    fn modified_and_application_cursor_keys() {
        let ctrl = mods(true, false, false);
        assert_eq!(bytes(&press("left", None, ctrl)), b"\x1b[1;5D");
        assert_eq!(
            bytes(&press("delete", None, mods(false, true, false))),
            b"\x1b[3;3~"
        );
        let app = encode(
            &press("up", None, Modifiers::default()),
            false,
            TermMode::APP_CURSOR,
        );
        assert_eq!(app.as_deref(), Some(&b"\x1bOA"[..]));
    }

    #[test]
    fn control_letters() {
        let ctrl = mods(true, false, false);
        assert_eq!(bytes(&press("c", None, ctrl)), [3]);
        assert_eq!(bytes(&press("a", None, ctrl)), [1]);
        assert_eq!(bytes(&press("z", None, ctrl)), [26]);
        assert_eq!(bytes(&press("[", None, ctrl)), [0x1b]);
        assert_eq!(bytes(&press("space", None, ctrl)), [0]);
        assert_eq!(bytes(&press("c", None, mods(true, true, false))), [0x1b, 3]);
    }

    #[test]
    fn altgr_text_is_sent_as_typed() {
        let altgr = mods(true, true, false);
        let key = press("q", Some("@"), altgr);
        assert_eq!(
            encode(&key, true, TermMode::empty()).as_deref(),
            Some(&b"@"[..])
        );
    }

    #[test]
    fn platform_keys_and_bare_modifiers_send_nothing() {
        let win = Modifiers {
            platform: true,
            ..Modifiers::default()
        };
        assert_eq!(
            encode(&press("a", Some("a"), win), false, TermMode::empty()),
            None
        );
        assert_eq!(
            encode(
                &press("shift", None, Modifiers::default()),
                false,
                TermMode::empty()
            ),
            None
        );
    }

    #[test]
    fn terminal_shortcuts() {
        let none = TermMode::empty();
        let cs = mods(true, false, true);
        assert!(matches!(
            local(&press("v", Some("V"), cs), none),
            Some(Local::Paste)
        ));
        assert!(matches!(
            local(&press("c", Some("C"), cs), none),
            Some(Local::Copy)
        ));
        assert!(local(&press("c", None, mods(true, false, false)), none).is_none());
        let shift = mods(false, false, true);
        assert!(matches!(
            local(&press("pageup", None, shift), none),
            Some(Local::Scroll(Scroll::PageUp))
        ));
        assert!(matches!(
            local(&press("end", None, shift), none),
            Some(Local::Scroll(Scroll::Bottom))
        ));
        assert!(local(&press("pageup", None, Modifiers::default()), none).is_none());
        assert!(local(&press("pageup", None, shift), TermMode::ALT_SCREEN).is_none());
    }
}
