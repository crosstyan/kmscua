//! Character and key-name to evdev keycode mapping.
//!
//! This assumes a US QWERTY layout on the virtual keyboard, which is what
//! every compositor assigns to an unknown evdev keyboard unless the user has
//! configured something else. Non-ASCII text is refused here; the client
//! pastes it through the clipboard instead.

use evdev::KeyCode as K;

/// (keycode, needs_shift) for a printable ASCII character.
pub fn char_to_key(ch: char) -> Option<(u16, bool)> {
    let (k, shift) = match ch {
        'a'..='z' => (letter(ch), false),
        'A'..='Z' => (letter(ch.to_ascii_lowercase()), true),
        '0' => (K::KEY_0, false),
        '1' => (K::KEY_1, false),
        '2' => (K::KEY_2, false),
        '3' => (K::KEY_3, false),
        '4' => (K::KEY_4, false),
        '5' => (K::KEY_5, false),
        '6' => (K::KEY_6, false),
        '7' => (K::KEY_7, false),
        '8' => (K::KEY_8, false),
        '9' => (K::KEY_9, false),
        ')' => (K::KEY_0, true),
        '!' => (K::KEY_1, true),
        '@' => (K::KEY_2, true),
        '#' => (K::KEY_3, true),
        '$' => (K::KEY_4, true),
        '%' => (K::KEY_5, true),
        '^' => (K::KEY_6, true),
        '&' => (K::KEY_7, true),
        '*' => (K::KEY_8, true),
        '(' => (K::KEY_9, true),
        ' ' => (K::KEY_SPACE, false),
        '\n' => (K::KEY_ENTER, false),
        '\t' => (K::KEY_TAB, false),
        '-' => (K::KEY_MINUS, false),
        '_' => (K::KEY_MINUS, true),
        '=' => (K::KEY_EQUAL, false),
        '+' => (K::KEY_EQUAL, true),
        '[' => (K::KEY_LEFTBRACE, false),
        '{' => (K::KEY_LEFTBRACE, true),
        ']' => (K::KEY_RIGHTBRACE, false),
        '}' => (K::KEY_RIGHTBRACE, true),
        ';' => (K::KEY_SEMICOLON, false),
        ':' => (K::KEY_SEMICOLON, true),
        '\'' => (K::KEY_APOSTROPHE, false),
        '"' => (K::KEY_APOSTROPHE, true),
        '`' => (K::KEY_GRAVE, false),
        '~' => (K::KEY_GRAVE, true),
        '\\' => (K::KEY_BACKSLASH, false),
        '|' => (K::KEY_BACKSLASH, true),
        ',' => (K::KEY_COMMA, false),
        '<' => (K::KEY_COMMA, true),
        '.' => (K::KEY_DOT, false),
        '>' => (K::KEY_DOT, true),
        '/' => (K::KEY_SLASH, false),
        '?' => (K::KEY_SLASH, true),
        _ => return None,
    };
    Some((k.0, shift))
}

fn letter(ch: char) -> K {
    match ch {
        'a' => K::KEY_A,
        'b' => K::KEY_B,
        'c' => K::KEY_C,
        'd' => K::KEY_D,
        'e' => K::KEY_E,
        'f' => K::KEY_F,
        'g' => K::KEY_G,
        'h' => K::KEY_H,
        'i' => K::KEY_I,
        'j' => K::KEY_J,
        'k' => K::KEY_K,
        'l' => K::KEY_L,
        'm' => K::KEY_M,
        'n' => K::KEY_N,
        'o' => K::KEY_O,
        'p' => K::KEY_P,
        'q' => K::KEY_Q,
        'r' => K::KEY_R,
        's' => K::KEY_S,
        't' => K::KEY_T,
        'u' => K::KEY_U,
        'v' => K::KEY_V,
        'w' => K::KEY_W,
        'x' => K::KEY_X,
        'y' => K::KEY_Y,
        'z' => K::KEY_Z,
        _ => unreachable!(),
    }
}

/// Key name (xdotool/Anthropic style, case-insensitive) to evdev keycode.
pub fn name_to_key(name: &str) -> Option<u16> {
    let n = name.trim().to_ascii_lowercase();
    let k = match n.as_str() {
        "ctrl" | "control" | "ctrl_l" | "lctrl" => K::KEY_LEFTCTRL,
        "ctrl_r" | "rctrl" => K::KEY_RIGHTCTRL,
        "shift" | "shift_l" | "lshift" => K::KEY_LEFTSHIFT,
        "shift_r" | "rshift" => K::KEY_RIGHTSHIFT,
        "alt" | "alt_l" | "lalt" | "meta" => K::KEY_LEFTALT,
        "alt_r" | "ralt" | "altgr" => K::KEY_RIGHTALT,
        "super" | "super_l" | "win" | "cmd" | "command" | "lsuper" => K::KEY_LEFTMETA,
        "super_r" | "rsuper" => K::KEY_RIGHTMETA,
        "enter" | "return" | "kp_enter" => K::KEY_ENTER,
        "esc" | "escape" => K::KEY_ESC,
        "tab" => K::KEY_TAB,
        "space" => K::KEY_SPACE,
        "backspace" => K::KEY_BACKSPACE,
        "delete" | "del" => K::KEY_DELETE,
        "insert" => K::KEY_INSERT,
        "home" => K::KEY_HOME,
        "end" => K::KEY_END,
        "pageup" | "page_up" | "prior" => K::KEY_PAGEUP,
        "pagedown" | "page_down" | "next" => K::KEY_PAGEDOWN,
        "up" => K::KEY_UP,
        "down" => K::KEY_DOWN,
        "left" => K::KEY_LEFT,
        "right" => K::KEY_RIGHT,
        "capslock" | "caps_lock" => K::KEY_CAPSLOCK,
        "numlock" | "num_lock" => K::KEY_NUMLOCK,
        "print" | "printscreen" | "sysrq" => K::KEY_SYSRQ,
        "scrolllock" | "scroll_lock" => K::KEY_SCROLLLOCK,
        "pause" => K::KEY_PAUSE,
        "menu" => K::KEY_MENU,
        "f1" => K::KEY_F1,
        "f2" => K::KEY_F2,
        "f3" => K::KEY_F3,
        "f4" => K::KEY_F4,
        "f5" => K::KEY_F5,
        "f6" => K::KEY_F6,
        "f7" => K::KEY_F7,
        "f8" => K::KEY_F8,
        "f9" => K::KEY_F9,
        "f10" => K::KEY_F10,
        "f11" => K::KEY_F11,
        "f12" => K::KEY_F12,
        "minus" => K::KEY_MINUS,
        "equal" | "equals" => K::KEY_EQUAL,
        "plus" => K::KEY_EQUAL,
        "comma" => K::KEY_COMMA,
        "period" | "dot" => K::KEY_DOT,
        "slash" => K::KEY_SLASH,
        "backslash" => K::KEY_BACKSLASH,
        "semicolon" => K::KEY_SEMICOLON,
        "apostrophe" | "quote" => K::KEY_APOSTROPHE,
        "grave" => K::KEY_GRAVE,
        "bracketleft" => K::KEY_LEFTBRACE,
        "bracketright" => K::KEY_RIGHTBRACE,
        "volumeup" => K::KEY_VOLUMEUP,
        "volumedown" => K::KEY_VOLUMEDOWN,
        "mute" => K::KEY_MUTE,
        _ => {
            let mut chars = n.chars();
            let ch = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            return char_to_key(ch).map(|(k, _)| k);
        }
    };
    Some(k.0)
}

/// Parse "ctrl+shift+t" or ["ctrl","shift","t"] style chords.
pub fn parse_combo(keys: &[String]) -> Option<Vec<u16>> {
    let mut out = Vec::new();
    for k in keys {
        for part in k.split('+') {
            out.push(name_to_key(part)?);
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}
