//! Mouse events as terminal input, for programs that ask to receive them
//! (xterm's 1000, 1002, 1003, 1005 and 1006 modes).

use term::alacritty_terminal::term::TermMode;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Button {
    Left,
    Middle,
    Right,
    /// Moved with no button held.
    None,
    WheelUp,
    WheelDown,
}

impl Button {
    fn code(self) -> u32 {
        match self {
            Self::Left => 0,
            Self::Middle => 1,
            Self::Right => 2,
            Self::None => 3,
            Self::WheelUp => 64,
            Self::WheelDown => 65,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Press,
    Release,
    Motion,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Modifiers {
    pub shift: bool,
    pub alt: bool,
    pub control: bool,
}

/// One mouse event at a cell (zero-based).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Report {
    pub button: Button,
    pub action: Action,
    pub modifiers: Modifiers,
    pub col: usize,
    pub row: usize,
}

/// True when the program wants mouse events.
pub(crate) fn reporting(mode: TermMode) -> bool {
    mode.intersects(TermMode::MOUSE_MODE)
}

/// The bytes that tell the program about `report`, or `None` when its mode
/// does not include this kind of event or the cell cannot be encoded.
pub(crate) fn encode(report: Report, mode: TermMode) -> Option<Vec<u8>> {
    let Report {
        button,
        action,
        modifiers,
        col,
        row,
    } = report;
    let wanted = match action {
        Action::Press | Action::Release => reporting(mode),
        Action::Motion if button == Button::None => mode.contains(TermMode::MOUSE_MOTION),
        Action::Motion => mode.intersects(TermMode::MOUSE_MOTION | TermMode::MOUSE_DRAG),
    };
    if !wanted {
        return None;
    }
    let sgr = mode.contains(TermMode::SGR_MOUSE);
    let mut code = match action {
        // Without SGR a release does not say which button it was.
        Action::Release if !sgr => Button::None.code(),
        _ => button.code(),
    };
    code += 4 * u32::from(modifiers.shift)
        + 8 * u32::from(modifiers.alt)
        + 16 * u32::from(modifiers.control);
    if action == Action::Motion {
        code += 32;
    }

    if sgr {
        let end = if action == Action::Release { 'm' } else { 'M' };
        return Some(format!("\x1b[<{code};{};{}{end}", col + 1, row + 1).into_bytes());
    }
    let mut out = b"\x1b[M".to_vec();
    out.push(u8::try_from(32 + code).ok()?);
    if mode.contains(TermMode::UTF8_MOUSE) {
        for position in [col, row] {
            let c = char::from_u32(32 + u32::try_from(position + 1).ok()?)?;
            let mut buf = [0; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        }
    } else {
        for position in [col, row] {
            out.push(u8::try_from(32 + position + 1).ok()?);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(button: Button, action: Action) -> Report {
        Report {
            button,
            action,
            modifiers: Modifiers::default(),
            col: 4,
            row: 9,
        }
    }

    const SGR: TermMode = TermMode::SGR_MOUSE.union(TermMode::MOUSE_REPORT_CLICK);

    #[test]
    fn sgr_press_and_release_name_the_button() {
        let press = encode(report(Button::Left, Action::Press), SGR);
        assert_eq!(press.as_deref(), Some(&b"\x1b[<0;5;10M"[..]));
        let release = encode(report(Button::Right, Action::Release), SGR);
        assert_eq!(release.as_deref(), Some(&b"\x1b[<2;5;10m"[..]));
    }

    #[test]
    fn modifiers_and_motion_add_to_the_code() {
        let mut r = report(Button::Left, Action::Motion);
        r.modifiers = Modifiers {
            shift: true,
            alt: false,
            control: true,
        };
        let mode = SGR | TermMode::MOUSE_DRAG;
        assert_eq!(
            encode(r, mode).as_deref(),
            Some(&b"\x1b[<52;5;10M"[..]),
            "0 + shift 4 + control 16 + motion 32"
        );
    }

    #[test]
    fn the_wheel_is_buttons_64_and_65() {
        let up = encode(report(Button::WheelUp, Action::Press), SGR);
        assert_eq!(up.as_deref(), Some(&b"\x1b[<64;5;10M"[..]));
        let down = encode(report(Button::WheelDown, Action::Press), SGR);
        assert_eq!(down.as_deref(), Some(&b"\x1b[<65;5;10M"[..]));
    }

    #[test]
    fn the_legacy_encoding_offsets_by_32_and_has_no_release_button() {
        let mode = TermMode::MOUSE_REPORT_CLICK;
        let press = encode(report(Button::Middle, Action::Press), mode);
        assert_eq!(press.as_deref(), Some(&[0x1b, b'[', b'M', 33, 37, 42][..]));
        let release = encode(report(Button::Middle, Action::Release), mode);
        assert_eq!(
            release.as_deref(),
            Some(&[0x1b, b'[', b'M', 35, 37, 42][..])
        );
    }

    #[test]
    fn the_legacy_encoding_gives_up_past_column_222() {
        let mut r = report(Button::Left, Action::Press);
        r.col = 300;
        assert_eq!(encode(r, TermMode::MOUSE_REPORT_CLICK), None);
        let utf8 = TermMode::MOUSE_REPORT_CLICK | TermMode::UTF8_MOUSE;
        assert!(encode(r, utf8).is_some());
    }

    #[test]
    fn motion_needs_a_mode_that_asks_for_it() {
        let drag = report(Button::Left, Action::Motion);
        assert_eq!(encode(drag, SGR), None);
        assert!(encode(drag, SGR | TermMode::MOUSE_DRAG).is_some());
        let hover = report(Button::None, Action::Motion);
        assert_eq!(encode(hover, SGR | TermMode::MOUSE_DRAG), None);
        assert!(encode(hover, SGR | TermMode::MOUSE_MOTION).is_some());
        assert_eq!(encode(drag, TermMode::empty()), None);
    }
}
