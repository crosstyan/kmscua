//! Key names and characters to X keysyms, and keysyms to the server's
//! keycodes.
//!
//! The names are the ones cuad's `keymap.rs` accepts (xdotool/Anthropic
//! style, case-insensitive), so a chord means the same thing on both
//! backends. Where cuad maps a name to a key rather than a symbol ("plus" is
//! the `=` key, "meta" is left Alt, "kp_enter" is Return) this does too.
//!
//! Keycodes come from the server's own `GetKeyboardMapping`, never from
//! "evdev + 8". A keysym the map does not hold is not an error: the input
//! layer binds it to a spare keycode for the duration of a request, the way
//! xdotool types characters outside the layout.

/// `NoSymbol`.
pub const NO_SYMBOL: u32 = 0;
pub const SHIFT_L: u32 = 0xffe1;
pub const RETURN: u32 = 0xff0d;
pub const TAB: u32 = 0xff09;

/// The keysym a character types: ASCII and Latin-1 are their own code
/// point, Return and Tab for `\n` and `\t`, everything else the Unicode
/// keysym `0x0100_0000 + code point`. Control characters have none.
pub fn char_to_keysym(ch: char) -> Option<u32> {
    let cp = ch as u32;
    match ch {
        '\n' => Some(RETURN),
        '\t' => Some(TAB),
        ' '..='~' => Some(cp),
        _ if ch.is_control() => None,
        '\u{a0}'..='\u{ff}' => Some(cp),
        _ => Some(0x0100_0000 + cp),
    }
}

/// Key name to keysym. Single characters are accepted whatever they are;
/// the rest of the list is cuad's.
pub fn name_to_keysym(name: &str) -> Option<u32> {
    let n = name.trim().to_lowercase();
    let k = match n.as_str() {
        "ctrl" | "control" | "ctrl_l" | "lctrl" => 0xffe3,
        "ctrl_r" | "rctrl" => 0xffe4,
        "shift" | "shift_l" | "lshift" => SHIFT_L,
        "shift_r" | "rshift" => 0xffe2,
        "alt" | "alt_l" | "lalt" | "meta" => 0xffe9,
        "alt_r" | "ralt" | "altgr" => 0xffea,
        "super" | "super_l" | "win" | "cmd" | "command" | "lsuper" => 0xffeb,
        "super_r" | "rsuper" => 0xffec,
        "enter" | "return" | "kp_enter" => RETURN,
        "esc" | "escape" => 0xff1b,
        "tab" => TAB,
        "space" => 0x20,
        "backspace" => 0xff08,
        "delete" | "del" => 0xffff,
        "insert" => 0xff63,
        "home" => 0xff50,
        "end" => 0xff57,
        "pageup" | "page_up" | "prior" => 0xff55,
        "pagedown" | "page_down" | "next" => 0xff56,
        "up" => 0xff52,
        "down" => 0xff54,
        "left" => 0xff51,
        "right" => 0xff53,
        "capslock" | "caps_lock" => 0xffe5,
        "numlock" | "num_lock" => 0xff7f,
        "print" | "printscreen" | "sysrq" => 0xff61,
        "scrolllock" | "scroll_lock" => 0xff14,
        "pause" => 0xff13,
        "menu" => 0xff67,
        "minus" => 0x2d,
        "equal" | "equals" | "plus" => 0x3d,
        "comma" => 0x2c,
        "period" | "dot" => 0x2e,
        "slash" => 0x2f,
        "backslash" => 0x5c,
        "semicolon" => 0x3b,
        "apostrophe" | "quote" => 0x27,
        "grave" => 0x60,
        "bracketleft" => 0x5b,
        "bracketright" => 0x5d,
        "volumeup" => 0x1008_ff13,
        "volumedown" => 0x1008_ff11,
        "mute" => 0x1008_ff12,
        f if f.len() >= 2 && f.starts_with('f') && f[1..].chars().all(|c| c.is_ascii_digit()) => {
            let i: u32 = f[1..].parse().ok()?;
            if !(1..=24).contains(&i) {
                return None;
            }
            0xffbe + i - 1
        }
        _ => {
            let mut chars = n.chars();
            let ch = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            // `name` lowercased: "A" is the a key, as on cuad.
            return char_to_keysym(ch);
        }
    };
    Some(k)
}

/// `parse_combo`, saying which name it did not know.
pub fn parse_combo_named(keys: &[String]) -> Result<Vec<u32>, String> {
    if let Some(v) = parse_combo(keys) {
        return Ok(v);
    }
    let bad = keys
        .iter()
        .flat_map(|k| if k.trim() == "+" { vec![] } else { k.split('+').map(str::to_string).collect() })
        .find(|part| name_to_keysym(part).is_none());
    Err(match bad {
        Some(b) if b.trim().is_empty() => format!("empty key name in {keys:?}"),
        Some(b) => format!("unknown key name: {b} (xdotool names: ctrl, shift, alt, super, Return, Escape, Tab, BackSpace, F1, a, …)"),
        None => format!("no keys in {keys:?}"),
    })
}

/// Parse "ctrl+shift+t" or ["ctrl","shift","t"] style chords into keysyms.
/// A lone "+" is the plus key.
pub fn parse_combo(keys: &[String]) -> Option<Vec<u32>> {
    let mut out = Vec::new();
    for k in keys {
        if k.trim() == "+" {
            out.push(name_to_keysym("plus")?);
            continue;
        }
        for part in k.split('+') {
            out.push(name_to_keysym(part)?);
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// The server's core keyboard mapping: `per` keysyms for each keycode from
/// `min` up. Column 0 is the unshifted symbol, column 1 the shifted one.
#[derive(Clone, Debug)]
pub struct KeyMap {
    pub min: u8,
    pub max: u8,
    pub per: u8,
    pub syms: Vec<u32>,
}

impl KeyMap {
    pub fn syms_of(&self, keycode: u8) -> &[u32] {
        if keycode < self.min || keycode > self.max || self.per == 0 {
            return &[];
        }
        let i = (keycode - self.min) as usize * self.per as usize;
        self.syms.get(i..i + self.per as usize).unwrap_or(&[])
    }

    /// `(keycode, shift)` that types `keysym` on group 1, unshifted
    /// preferred. A lowercase-only entry also answers for its uppercase
    /// letter with shift, which is how Xlib reads a one-symbol keycode.
    pub fn lookup(&self, keysym: u32) -> Option<(u8, bool)> {
        if keysym == NO_SYMBOL {
            return None;
        }
        for col in 0..self.per.min(2) as usize {
            for kc in self.min..=self.max {
                if self.syms_of(kc).get(col) == Some(&keysym) {
                    return Some((kc, col == 1));
                }
            }
        }
        let upper = (0x41..=0x5a).contains(&keysym);
        if upper {
            let lower = keysym + 0x20;
            for kc in self.min..=self.max {
                let s = self.syms_of(kc);
                if s.first() == Some(&lower) && s.get(1).is_none_or(|&u| u == NO_SYMBOL) {
                    return Some((kc, true));
                }
            }
        }
        None
    }

    /// Keycodes with no symbol at all: free to bind for a moment. Highest
    /// first, away from the keys a layout is likely to fill in later.
    pub fn spare(&self) -> Vec<u8> {
        (self.min..=self.max)
            .rev()
            .filter(|&kc| {
                let s = self.syms_of(kc);
                !s.is_empty() && s.iter().all(|&k| k == NO_SYMBOL)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chars() {
        assert_eq!(char_to_keysym('a'), Some(0x61));
        assert_eq!(char_to_keysym('\n'), Some(RETURN));
        assert_eq!(char_to_keysym('é'), Some(0xe9));
        assert_eq!(char_to_keysym('铃'), Some(0x0100_94c3));
        assert_eq!(char_to_keysym('\u{7}'), None);
    }

    #[test]
    fn names_match_cuad() {
        assert_eq!(parse_combo(&["ctrl+shift+t".into()]), Some(vec![0xffe3, 0xffe1, 0x74]));
        assert_eq!(name_to_keysym("Escape"), Some(0xff1b));
        assert_eq!(name_to_keysym("F12"), Some(0xffc9));
        assert_eq!(name_to_keysym("A"), Some(0x61));
        assert_eq!(parse_combo(&["ctrl".into(), "+".into()]), Some(vec![0xffe3, 0x3d]));
        assert_eq!(name_to_keysym("nosuchkey"), None);
        assert_eq!(parse_combo_named(&["ctrl+question".into()]).unwrap_err().split(' ').take(4).collect::<Vec<_>>().join(" "), "unknown key name: question");
    }

    #[test]
    fn lookup_and_spare() {
        // keycodes 10..=13: [a A] [1 exclam] [b] [empty]
        let m = KeyMap { min: 10, max: 13, per: 2, syms: vec![0x61, 0x41, 0x31, 0x21, 0x62, 0, 0, 0] };
        assert_eq!(m.lookup(0x41), Some((10, true)));
        assert_eq!(m.lookup(0x21), Some((11, true)));
        assert_eq!(m.lookup(0x42), Some((12, true)));
        assert_eq!(m.lookup(0xe9), None);
        assert_eq!(m.spare(), vec![13]);
    }
}
