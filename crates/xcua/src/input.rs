//! Input through XTEST on its own connection: motion, buttons, wheel
//! (buttons 4-7) and keys, with cuad's timings and cuad's atomicity.
//!
//! Every public method releases whatever it pressed before it returns,
//! including on error; `Backend` adds a `release_all` on top when a request
//! fails. A keysym the keyboard map does not hold is bound to a spare
//! keycode for the request and unbound before it answers (xdotool's trick),
//! so any Unicode character can be typed whatever the layout.
//!
//! There is no window manager in a sandbox cell, so nothing would give a
//! clicked window the keyboard. Every press (click, drag start,
//! `left_mouse_down`) therefore sets the input focus to the top-level
//! window under the pointer, the way a click-to-focus WM does, unless a
//! window manager is running, in which case focus is its business.

use std::collections::HashMap;
use std::thread::sleep;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use kmscua_proto::{Button, Rect};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xproto::{self, ConnectionExt as _, InputFocus, Window};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

use crate::keysym::{self, KeyMap, NO_SYMBOL};
use crate::x11;

pub struct Input {
    conn: RustConnection,
    root: Window,
    display: String,
    /// Pause between press and release, and between clicks.
    tap_ms: u64,
    /// Spare keycodes currently bound by us, to unbind on any exit path.
    borrowed: Vec<u8>,
}

/// Keysyms resolved for one request: `(keycode, shift)` each.
struct Bound {
    codes: HashMap<u32, (u8, bool)>,
}

impl Input {
    pub fn open(display: &str) -> Result<Self> {
        let (conn, root) = x11::connect(display)?;
        if conn.extension_information("XTEST")?.is_none() {
            bail!("the X server on {display} has no XTEST extension; input is impossible");
        }
        conn.xtest_get_version(2, 2)?.reply().context("XTEST GetVersion")?;
        x11::root_size(&conn, root, display)?;
        Ok(Self { conn, root, display: display.to_string(), tap_ms: 30, borrowed: Vec::new() })
    }

    pub fn conn(&self) -> &RustConnection {
        &self.conn
    }

    pub fn root(&self) -> Window {
        self.root
    }

    fn fake(&self, type_: u8, detail: u8, x: i16, y: i16) -> Result<()> {
        self.conn
            .xtest_fake_input(type_, detail, x11rb::CURRENT_TIME, self.root, x, y, 0)
            .context("XTEST FakeInput")?;
        self.conn.flush()?;
        Ok(())
    }

    /// Round trip: every event sent so far has been processed by the
    /// server. Also drains the errors and MappingNotify events the server
    /// sends unasked, which would otherwise pile up.
    pub fn sync(&self) -> Result<()> {
        self.conn.get_input_focus()?.reply().context("X round trip")?;
        while let Some(ev) = self.conn.poll_for_event()? {
            log::trace!("drained {ev:?}");
        }
        Ok(())
    }

    pub fn keymap(&self) -> Result<KeyMap> {
        let setup = self.conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let r = self
            .conn
            .get_keyboard_mapping(min, max - min + 1)?
            .reply()
            .context("GetKeyboardMapping")?;
        Ok(KeyMap { min, max, per: r.keysyms_per_keycode, syms: r.keysyms })
    }

    fn desktop(&self) -> Result<Rect> {
        let (w, h) = x11::root_size(&self.conn, self.root, &self.display)?;
        Ok(Rect { x: 0, y: 0, width: w, height: h })
    }

    pub fn move_to(&mut self, x: i32, y: i32) -> Result<()> {
        let (x, y) = clamp_point(x, y, self.desktop()?)?;
        self.fake(xproto::MOTION_NOTIFY_EVENT, 0, x as i16, y as i16)
    }

    /// Give the top-level window under the pointer the keyboard, when no
    /// window manager does it.
    fn focus_under_pointer(&self) {
        if x11::has_wm(&self.conn, self.root) {
            return;
        }
        let child = match self.conn.query_pointer(self.root).map(|c| c.reply()) {
            Ok(Ok(p)) => p.child,
            _ => return,
        };
        if child == x11rb::NONE {
            return;
        }
        let r = self
            .conn
            .set_input_focus(InputFocus::POINTER_ROOT, child, x11rb::CURRENT_TIME)
            .map_err(anyhow::Error::from)
            .and_then(|c| c.check().map_err(anyhow::Error::from));
        if let Err(e) = r {
            log::debug!("SetInputFocus {child:#x}: {e:#}");
        }
    }

    fn press(&self, button: u8, down: bool) -> Result<()> {
        let t = if down { xproto::BUTTON_PRESS_EVENT } else { xproto::BUTTON_RELEASE_EVENT };
        self.fake(t, button, 0, 0)
    }

    pub fn button(&mut self, button: Button, down: bool) -> Result<()> {
        if down {
            self.focus_under_pointer();
        }
        self.press(button_code(button), down)?;
        self.sync()
    }

    /// Resolve keysyms to keycodes, binding spare keycodes for the ones the
    /// map lacks. Unbind with [`Self::unbind`] on every path.
    fn bind(&mut self, syms: &[u32]) -> Result<Bound> {
        let map = self.keymap()?;
        let mut codes = HashMap::new();
        let mut missing = Vec::new();
        for &s in syms {
            if codes.contains_key(&s) || missing.contains(&s) {
                continue;
            }
            match map.lookup(s) {
                Some(kc) => {
                    codes.insert(s, kc);
                }
                None => missing.push(s),
            }
        }
        if missing.is_empty() {
            return Ok(Bound { codes });
        }
        let mut spare = map.spare();
        if spare.len() < missing.len() {
            bail!(
                "{} symbols are not on the keyboard map and only {} keycodes are free to bind them",
                missing.len(),
                spare.len()
            );
        }
        let per = map.per.max(2);
        for s in missing {
            let kc = spare.remove(0);
            // Same symbol unshifted and shifted, as xdotool binds it.
            let mut row = vec![NO_SYMBOL; per as usize];
            row[0] = s;
            row[1] = s;
            self.conn
                .change_keyboard_mapping(1, kc, per, &row)?
                .check()
                .with_context(|| format!("bind keysym {s:#x} to spare keycode {kc}"))?;
            self.borrowed.push(kc);
            codes.insert(s, (kc, false));
        }
        // Clients reread the map on MappingNotify, which reaches them before
        // the key events that follow; the pause covers toolkits that refresh
        // their XKB state lazily.
        sleep(Duration::from_millis(20));
        Ok(Bound { codes })
    }

    /// Give back every borrowed keycode (after a pause so the last key
    /// event is read with the binding it was sent under).
    fn unbind(&mut self) {
        if self.borrowed.is_empty() {
            return;
        }
        sleep(Duration::from_millis(40));
        let per = self.keymap().map(|m| m.per.max(2)).unwrap_or(2);
        for kc in std::mem::take(&mut self.borrowed) {
            let row = vec![NO_SYMBOL; per as usize];
            if let Err(e) = self
                .conn
                .change_keyboard_mapping(1, kc, per, &row)
                .map_err(anyhow::Error::from)
                .and_then(|c| c.check().map_err(anyhow::Error::from))
            {
                log::warn!("unbind spare keycode {kc}: {e:#}");
            }
        }
        let _ = self.sync();
    }

    fn key_code(&self, kc: u8, down: bool) -> Result<()> {
        let t = if down { xproto::KEY_PRESS_EVENT } else { xproto::KEY_RELEASE_EVENT };
        self.fake(t, kc, 0, 0)
    }

    /// Press the named keys, run `f`, release them in reverse. The release
    /// happens even when `f` fails.
    pub fn with_modifiers<T>(&mut self, names: &[String], f: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        if names.is_empty() {
            return f(self);
        }
        let syms = keysym::parse_combo(names).ok_or_else(|| anyhow!("unknown modifier in {names:?}"))?;
        let bound = match self.bind(&syms) {
            Ok(b) => b,
            Err(e) => {
                self.unbind();
                return Err(e);
            }
        };
        let codes: Vec<u8> = syms.iter().map(|s| bound.codes[s].0).collect();
        let mut held = Vec::new();
        let mut result = Ok(());
        for &c in &codes {
            if let Err(e) = self.key_code(c, true) {
                result = Err(e);
                break;
            }
            held.push(c);
            sleep(Duration::from_millis(12));
        }
        let out = match result {
            Ok(()) => f(self),
            Err(e) => Err(e),
        };
        for &c in held.iter().rev() {
            let _ = self.key_code(c, false);
            sleep(Duration::from_millis(12));
        }
        let _ = self.sync();
        self.unbind();
        out
    }

    /// Hold a chord for `seconds`, then release. Bounded to 100 s.
    pub fn hold(&mut self, keys: &[String], seconds: f32) -> Result<()> {
        let secs = seconds.clamp(0.0, 100.0);
        self.with_modifiers(keys, |_| {
            sleep(Duration::from_secs_f32(secs));
            Ok(())
        })
    }

    pub fn click(&mut self, x: i32, y: i32, button: Button, count: u32) -> Result<()> {
        self.move_to(x, y)?;
        sleep(Duration::from_millis(self.tap_ms));
        self.focus_under_pointer();
        let code = button_code(button);
        for i in 0..count.max(1) {
            if i > 0 {
                sleep(Duration::from_millis(60));
            }
            self.press(code, true)?;
            sleep(Duration::from_millis(self.tap_ms));
            self.press(code, false)?;
        }
        self.sync()
    }

    pub fn drag(&mut self, from: (i32, i32), to: (i32, i32), button: Button, steps: u32) -> Result<()> {
        let code = button_code(button);
        self.move_to(from.0, from.1)?;
        sleep(Duration::from_millis(self.tap_ms));
        self.focus_under_pointer();
        self.press(code, true)?;
        sleep(Duration::from_millis(60));
        let steps = steps.clamp(1, 200) as i32;
        let mut result = Ok(());
        for i in 1..=steps {
            let x = from.0 + (to.0 - from.0) * i / steps;
            let y = from.1 + (to.1 - from.1) * i / steps;
            if let Err(e) = self.move_to(x, y) {
                result = Err(e);
                break;
            }
            sleep(Duration::from_millis(8));
        }
        sleep(Duration::from_millis(60));
        let release = self.press(code, false);
        result.and(release).and_then(|_| self.sync())
    }

    /// Wheel detents; `dy > 0` scrolls down (button 5), `dx > 0` right (7).
    pub fn scroll(&mut self, x: i32, y: i32, dx: i32, dy: i32) -> Result<()> {
        self.move_to(x, y)?;
        sleep(Duration::from_millis(self.tap_ms));
        let hb = if dx > 0 { 7 } else { 6 };
        for _ in 0..dx.unsigned_abs().min(50) {
            self.press(hb, true)?;
            self.press(hb, false)?;
            sleep(Duration::from_millis(15));
        }
        let vb = if dy > 0 { 5 } else { 4 };
        for _ in 0..dy.unsigned_abs().min(50) {
            self.press(vb, true)?;
            self.press(vb, false)?;
            sleep(Duration::from_millis(15));
        }
        self.sync()
    }

    /// Raw evdev key code, as cuad takes it. X servers on the evdev XKB
    /// rules (Xvfb, Xorg with libinput) number keycodes evdev + 8; this is
    /// the one place that assumes it, because the request names a key, not
    /// a symbol.
    pub fn key(&mut self, code: u16, down: bool) -> Result<()> {
        let setup = self.conn.setup();
        let kc = code as u32 + 8;
        if kc < setup.min_keycode as u32 || kc > setup.max_keycode as u32 {
            bail!("evdev keycode {code} is outside the server's keycodes {}..={}", setup.min_keycode, setup.max_keycode);
        }
        self.key_code(kc as u8, down)?;
        self.sync()
    }

    /// Press the chord in order, release in reverse. Always releases.
    pub fn combo(&mut self, keys: &[String]) -> Result<()> {
        let tap = self.tap_ms;
        self.with_modifiers(keys, |_| {
            sleep(Duration::from_millis(tap));
            Ok(())
        })
    }

    /// Any text: characters on the map are typed with their keycode (and
    /// Shift for the shifted column), the rest through spare keycodes bound
    /// in chunks as large as the free keycodes allow.
    pub fn type_text(&mut self, text: &str, delay_ms: u32) -> Result<()> {
        let delay = Duration::from_millis(delay_ms.max(4) as u64);
        let mut syms = Vec::with_capacity(text.len());
        for (i, ch) in text.chars().enumerate() {
            syms.push(keysym::char_to_keysym(ch).ok_or_else(|| anyhow!("cannot type control character {ch:?} at index {i}"))?);
        }
        let map = self.keymap()?;
        let shift = map.lookup(keysym::SHIFT_L).map(|k| k.0);
        let room = map.spare().len().max(1);
        let mut start = 0;
        while start < syms.len() {
            // Grow the chunk while its unmapped symbols fit the spare keycodes.
            let mut unmapped: Vec<u32> = Vec::new();
            let mut end = start;
            while end < syms.len() {
                let s = syms[end];
                if map.lookup(s).is_none() && !unmapped.contains(&s) {
                    if unmapped.len() == room {
                        break;
                    }
                    unmapped.push(s);
                }
                end += 1;
            }
            let r = self.type_chunk(&syms[start..end], shift, delay);
            self.unbind();
            r?;
            start = end;
        }
        Ok(())
    }

    fn type_chunk(&mut self, syms: &[u32], shift: Option<u8>, delay: Duration) -> Result<()> {
        let bound = self.bind(syms)?;
        for &s in syms {
            let (kc, shifted) = bound.codes[&s];
            if shifted {
                let sc = shift.ok_or_else(|| anyhow!("no Shift_L on the keyboard map"))?;
                self.key_code(sc, true)?;
            }
            let r = self.key_code(kc, true).and_then(|_| {
                sleep(delay);
                self.key_code(kc, false)
            });
            if shifted {
                let _ = self.key_code(shift.unwrap(), false);
            }
            r?;
            sleep(delay);
        }
        self.sync()
    }

    /// One-pixel nudge and back, plus a screen saver reset: an Xvfb blanks
    /// after ten idle minutes unless started with `-s 0`.
    pub fn wake(&mut self) -> Result<()> {
        let _ = self.conn.force_screen_saver(xproto::ScreenSaver::RESET);
        let p = self.conn.query_pointer(self.root)?.reply()?;
        let (x, y) = (p.root_x as i32, p.root_y as i32);
        self.move_to(x + 1, y)?;
        sleep(Duration::from_millis(20));
        self.move_to(x, y)?;
        self.sync()
    }

    /// Release every key and button the server reports down and unbind any
    /// spare keycode still borrowed. Used after a failed request and on exit.
    pub fn release_all(&mut self) {
        if let Ok(Ok(km)) = self.conn.query_keymap().map(|c| c.reply()) {
            for (byte, bits) in km.keys.iter().enumerate() {
                for bit in 0..8 {
                    if bits & (1 << bit) != 0 {
                        let _ = self.key_code((byte * 8 + bit) as u8, false);
                    }
                }
            }
        }
        if let Ok(Ok(p)) = self.conn.query_pointer(self.root).map(|c| c.reply()) {
            let mask = u16::from(p.mask);
            for b in 1..=5u8 {
                if mask & (0x80 << b) != 0 {
                    let _ = self.press(b, false);
                }
            }
        }
        self.unbind();
        let _ = self.sync();
    }
}

impl Drop for Input {
    fn drop(&mut self) {
        self.release_all();
    }
}

/// `(x, y)` kept inside `d`, as cuad's ABS range does. An empty desktop
/// has no pixel to land on, which is an error rather than a panic.
fn clamp_point(x: i32, y: i32, d: Rect) -> Result<(i32, i32)> {
    if d.width == 0 || d.height == 0 {
        bail!("desktop is {}x{}: nowhere to move the pointer", d.width, d.height);
    }
    Ok((
        x.clamp(d.x, d.x + d.width as i32 - 1),
        y.clamp(d.y, d.y + d.height as i32 - 1),
    ))
}

fn button_code(b: Button) -> u8 {
    match b {
        Button::Left => 1,
        Button::Middle => 2,
        Button::Right => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_point_clips_and_refuses_an_empty_desktop() {
        let d = Rect { x: 0, y: 0, width: 640, height: 480 };
        assert_eq!(clamp_point(-5, 5000, d).unwrap(), (0, 479));
        assert_eq!(clamp_point(10, 20, d).unwrap(), (10, 20));
        assert!(clamp_point(0, 0, Rect { x: 0, y: 0, width: 0, height: 0 }).is_err());
        assert!(clamp_point(0, 0, Rect { x: 0, y: 0, width: 10, height: 0 }).is_err());
    }
}
